//! The alacritty → neutral-grid projection (design §4; ). Owns the
//! `alacritty_terminal::Term`, feeds it PTY bytes, and drains its native damage
//! into an owned [`GridDelta`] of resolved [`NeutralCell`]s. This is the v1 arm
//! of the swappable-engine contract; a `libghostty` projection would be the
//! phase-2 arm, differing only here.

use std::collections::HashMap;

use alacritty_terminal::event::EventListener;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::Processor;
use ember_core::{
    Attrs, CellContent, CellPatch, CursorShape, CursorState, GridDelta, GridDims, NeutralCell,
    Style, StyleId, VtProjection,
};

use crate::palette::Palette;

/// Assigns small dense [`StyleId`]s to distinct [`Style`]s and tracks which were
/// first seen in the current delta (design §4: render keys its raster cache on
/// `(glyph, StyleId)`).
struct StyleInterner {
    map: HashMap<Style, StyleId>,
    next: u32,
}

impl StyleInterner {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            next: 0,
        }
    }

    fn intern(&mut self, style: Style, first_seen: &mut Vec<(StyleId, Style)>) -> StyleId {
        if let Some(id) = self.map.get(&style) {
            *id
        } else {
            let id = StyleId(self.next);
            self.next += 1;
            self.map.insert(style, id);
            first_seen.push((id, style));
            id
        }
    }
}

/// A projection driving an `alacritty_terminal::Term` from bytes into the neutral
/// grid. Generic over the engine's `EventListener` (the LocalPty backend supplies
/// one that forwards semantic events; tests use a no-op).
pub struct AlacrittyProjection<L: EventListener> {
    term: Term<L>,
    parser: Processor,
    interner: StyleInterner,
    palette: Palette,
    dims: GridDims,
    epoch: u64,
}

impl<L: EventListener> AlacrittyProjection<L> {
    pub fn new(dims: GridDims, listener: L) -> Self {
        let size = TermSize::new(dims.columns as usize, dims.screen_lines as usize);
        let term = Term::new(Config::default(), &size, listener);
        Self {
            term,
            parser: Processor::new(),
            interner: StyleInterner::new(),
            palette: Palette::dark(),
            dims,
            epoch: 0,
        }
    }

    /// Feed raw PTY bytes through the VT parser into the engine.
    pub fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// Resize the engine grid.
    pub fn resize(&mut self, dims: GridDims) {
        self.dims = dims;
        self.term.resize(TermSize::new(
            dims.columns as usize,
            dims.screen_lines as usize,
        ));
    }

    pub fn dims(&self) -> GridDims {
        self.dims
    }

    fn cursor_state(&self) -> CursorState {
        let point = self.term.grid().cursor.point;
        let visible = self.term.mode().contains(TermMode::SHOW_CURSOR);
        CursorState {
            row: point.line.0.max(0) as u16,
            col: point.column.0 as u16,
            shape: CursorShape::Block,
            visible,
        }
    }
}

impl<L: EventListener> VtProjection for AlacrittyProjection<L> {
    fn drain_damage_into(&mut self, out: &mut GridDelta) {
        self.epoch += 1;
        out.epoch = self.epoch;
        out.dims = self.dims;

        let cols = self.dims.columns as usize;
        let lines = self.dims.screen_lines as usize;

        // Collect damaged ranges first — the damage iterator borrows the term,
        // and we need to read the grid afterwards. The arm consumes the iterator
        // into an owned Vec, ending the borrow before we read cells.
        let (full, ranges): (bool, Vec<(usize, usize, usize)>) = match self.term.damage() {
            TermDamage::Full => (true, Vec::new()),
            TermDamage::Partial(it) => (false, it.map(|ld| (ld.line, ld.left, ld.right)).collect()),
        };
        self.term.reset_damage();

        let mut first_seen = Vec::new();
        if full {
            out.reset = true;
            out.cells.clear();
            for line in 0..lines {
                for col in 0..cols {
                    out.cells.push(self.patch(line, col, &mut first_seen));
                }
            }
        } else {
            for (line, left, right) in ranges {
                for col in left..=right {
                    out.cells.push(self.patch(line, col, &mut first_seen));
                }
            }
        }
        out.new_styles = first_seen;
        out.cursor = self.cursor_state();
        out.bracketed_paste = self.term.mode().contains(TermMode::BRACKETED_PASTE);
    }
}

impl<L: EventListener> AlacrittyProjection<L> {
    fn patch(
        &mut self,
        line: usize,
        col: usize,
        first_seen: &mut Vec<(StyleId, Style)>,
    ) -> CellPatch {
        let (content, style, wrapped) = {
            let cell = &self.term.grid()[Point::new(Line(line as i32), Column(col))];
            neutral_of(cell, &self.palette)
        };
        let id = self.interner.intern(style, first_seen);
        CellPatch {
            row: line as u16,
            col: col as u16,
            cell: NeutralCell {
                content,
                style: id,
                wrapped,
            },
        }
    }
}

/// Resolve one alacritty cell into neutral content + style + soft-wrap flag.
fn neutral_of(cell: &Cell, palette: &Palette) -> (CellContent, Style, bool) {
    let flags = cell.flags;
    let mut fg = palette.resolve(cell.fg);
    let mut bg = palette.resolve(cell.bg);
    if flags.contains(Flags::INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }

    let mut attrs = Attrs::empty();
    if flags.contains(Flags::BOLD) {
        attrs |= Attrs::BOLD;
    }
    if flags.contains(Flags::ITALIC) {
        attrs |= Attrs::ITALIC;
    }
    if flags.contains(Flags::UNDERLINE) {
        attrs |= Attrs::UNDERLINE;
    }
    if flags.contains(Flags::STRIKEOUT) {
        attrs |= Attrs::STRIKEOUT;
    }
    if flags.contains(Flags::DIM) {
        attrs |= Attrs::DIM;
    }
    if flags.contains(Flags::HIDDEN) {
        attrs |= Attrs::HIDDEN;
    }

    let style = Style { fg, bg, attrs };
    // Blanks (incl. the spacer after a wide char) carry no glyph; render fills bg.
    let content = match cell.c {
        ' ' | '\0' => CellContent::Empty,
        c => CellContent::Char(c),
    };
    // Last cell of a soft-wrapped row carries WRAPLINE — the logical line continues.
    let wrapped = flags.contains(Flags::WRAPLINE);
    (content, style, wrapped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;

    fn proj() -> AlacrittyProjection<VoidListener> {
        AlacrittyProjection::new(GridDims::new(80, 24), VoidListener)
    }

    fn find(d: &GridDelta, row: u16, col: u16) -> &CellPatch {
        d.cells
            .iter()
            .find(|p| p.row == row && p.col == col)
            .expect("cell present")
    }

    #[test]
    fn projects_typed_bytes_into_cells() {
        let mut p = proj();
        p.advance(b"hi");
        let mut delta = GridDelta::default();
        p.drain_damage_into(&mut delta);
        assert!(delta.reset, "first drain is a full reset");
        assert_eq!(find(&delta, 0, 0).cell.content, CellContent::Char('h'));
        assert_eq!(find(&delta, 0, 1).cell.content, CellContent::Char('i'));
        assert_eq!(delta.dims, GridDims::new(80, 24));
        assert_eq!(delta.epoch, 1);
    }

    #[test]
    fn newline_moves_to_next_row() {
        let mut p = proj();
        p.advance(b"a\r\nb");
        let mut delta = GridDelta::default();
        p.drain_damage_into(&mut delta);
        assert_eq!(find(&delta, 0, 0).cell.content, CellContent::Char('a'));
        assert_eq!(find(&delta, 1, 0).cell.content, CellContent::Char('b'));
    }

    #[test]
    fn background_color_resolves_to_palette_not_black() {
        //  guard: a cell with an ANSI background (SGR 44 = blue bg) must
        // resolve to the palette color, never the default/pure-black that a
        // missing style would produce. (The black-block symptom was actually the
        // styles-dropped-on-reset coalescing bug, fixed in ember-core::merge.)
        use ember_core::Rgb;
        let mut p = proj();
        p.advance(b"\x1b[44mX");
        let mut d = GridDelta::default();
        p.drain_damage_into(&mut d);
        let cell = find(&d, 0, 0);
        let style = d
            .new_styles
            .iter()
            .find(|(id, _)| *id == cell.cell.style)
            .map(|(_, s)| *s)
            .expect("style shipped in new_styles");
        assert_eq!(cell.cell.content, CellContent::Char('X'));
        assert_eq!(
            style.bg,
            Rgb::new(0x3b, 0x8e, 0xea),
            "blue bg from the palette"
        );
        assert_ne!(style.bg, Rgb::new(0, 0, 0), "never pure black");
    }

    #[test]
    fn styles_are_interned_and_shipped() {
        let mut p = proj();
        p.advance(b"x");
        let mut delta = GridDelta::default();
        p.drain_damage_into(&mut delta);
        // The 'x' cell references a style id that appears in new_styles.
        let x = find(&delta, 0, 0);
        assert!(delta.new_styles.iter().any(|(id, _)| *id == x.cell.style));
    }

    #[test]
    fn second_drain_after_reset_is_partial() {
        let mut p = proj();
        p.advance(b"a");
        let mut first = GridDelta::default();
        p.drain_damage_into(&mut first);
        assert!(first.reset);
        // Type more; the next drain should be partial (not a full reset).
        p.advance(b"b");
        let mut second = GridDelta::default();
        p.drain_damage_into(&mut second);
        assert!(!second.reset, "incremental drain is not a full reset");
        assert_eq!(find(&second, 0, 1).cell.content, CellContent::Char('b'));
    }
}
