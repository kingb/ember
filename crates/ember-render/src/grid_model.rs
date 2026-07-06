//! The render-owned neutral grid (design §6). Render owns this; it applies owned
//! [`GridDelta`]s off the pixel lane and never touches engine memory. Pure and
//! headlessly testable — no GPU involved.

use std::collections::HashMap;

use ember_core::{CellContent, CursorState, GridDelta, GridDims, NeutralCell, Style, StyleId};

/// The current screen state, reconstructed from coalesced deltas.
#[derive(Debug)]
pub struct GridModel {
    pub dims: GridDims,
    cells: Vec<NeutralCell>,
    styles: HashMap<StyleId, Style>,
    pub cursor: CursorState,
    pub epoch: u64,
    /// Scrollback viewport state, carried from the latest delta.
    pub display_offset: u16,
    pub history_len: u16,
    pub alt_screen: bool,
    pub mouse_reporting: bool,
    /// Application cursor keys (DECCKM) — arrows encode as `ESC O A`….
    pub app_cursor: bool,
    /// Which mouse-reporting protocols the app enabled.
    pub mouse: ember_core::MouseProto,
    /// OSC 133 command marks visible this frame, as `(row, status)`.
    pub marks: Vec<(u16, ember_core::MarkStatus)>,
}

/// Display-only glyph substitution for symbols the terminal engine widths as a
/// single cell but whose default (no variation-selector) font glyph is a color
/// emoji the shaper draws ~2 cells wide — which would shove the rest of the row
/// one column right (the ⏺-toggle "flicker"). cosmic-text ignores the U+FE0E
/// text-presentation selector, so we swap in a monochrome look-alike that shapes
/// to one cell. This runs only on the glyph pass ([`GridModel::row_runs`]); the
/// text path ([`GridModel::row_text`]/[`GridModel::screen_text`]) keeps the real
/// scalar, so copy/paste and selection are unaffected.
fn monochrome_glyph(c: char) -> char {
    match c {
        // ⏺ RECORD (Claude Code's tool-activity bullet) → ● BLACK CIRCLE.
        '\u{23FA}' => '\u{25CF}',
        _ => c,
    }
}

/// Apply [`monochrome_glyph`] across a grapheme cluster, returning `Some` only
/// when a scalar actually changed (so the common path keeps borrowing the
/// original). Covers clusters like `⏺\u{FE0F}` where the emoji carries a
/// presentation selector.
fn remap_cluster(s: &str) -> Option<String> {
    s.chars()
        .any(|c| monochrome_glyph(c) != c)
        .then(|| s.chars().map(monochrome_glyph).collect())
}

impl GridModel {
    pub fn new(dims: GridDims) -> Self {
        Self {
            dims,
            cells: vec![NeutralCell::default(); dims.cells()],
            styles: HashMap::new(),
            cursor: CursorState::default(),
            epoch: 0,
            display_offset: 0,
            history_len: 0,
            alt_screen: false,
            mouse_reporting: false,
            app_cursor: false,
            mouse: ember_core::MouseProto::default(),
            marks: Vec::new(),
        }
    }

    /// Apply an owned delta (design §6 hot path): learn new styles, rebuild on a
    /// reset/resize, then patch the damaged cells.
    pub fn apply(&mut self, delta: GridDelta) {
        for (id, style) in &delta.new_styles {
            self.styles.insert(*id, *style);
        }
        if delta.reset || delta.dims != self.dims {
            self.dims = delta.dims;
            self.cells = vec![NeutralCell::default(); self.dims.cells()];
        }
        let cols = self.dims.columns as usize;
        for patch in &delta.cells {
            let idx = patch.row as usize * cols + patch.col as usize;
            if idx < self.cells.len() {
                self.cells[idx] = patch.cell.clone();
            }
        }
        self.cursor = delta.cursor;
        self.epoch = delta.epoch;
        self.display_offset = delta.display_offset;
        self.history_len = delta.history_len;
        self.alt_screen = delta.alt_screen;
        self.mouse_reporting = delta.mouse_reporting;
        self.app_cursor = delta.app_cursor;
        self.mouse = delta.mouse;
        self.marks = delta.marks;
    }

    /// Whether the view is scrolled up into history (not at the live bottom).
    pub fn scrolled(&self) -> bool {
        self.display_offset > 0
    }

    pub fn style_of(&self, id: StyleId) -> Style {
        self.styles.get(&id).copied().unwrap_or_default()
    }

    /// How many styles have been learned — `0` means no delta has arrived yet, so
    /// every cell falls back to the default style (a diagnostic signal).
    pub fn styles_len(&self) -> usize {
        self.styles.len()
    }

    /// The cell at `(row, col)`, if in bounds.
    pub fn cell(&self, row: u16, col: u16) -> Option<&NeutralCell> {
        let idx = row as usize * self.dims.columns as usize + col as usize;
        self.cells.get(idx)
    }

    /// Whether `row` soft-wraps into the next row (its last cell carries WRAPLINE).
    /// Lets copy join a wrapped logical line without a spurious newline.
    pub fn row_wrapped(&self, row: u16) -> bool {
        let cols = self.dims.columns;
        cols > 0 && self.cell(row, cols - 1).is_some_and(|c| c.wrapped)
    }

    /// The plain text of one row (blanks rendered as spaces).
    pub fn row_text(&self, row: u16) -> String {
        let cols = self.dims.columns as usize;
        let start = row as usize * cols;
        let end = (start + cols).min(self.cells.len());
        self.cells[start..end]
            .iter()
            .filter_map(|c| match &c.content {
                CellContent::Char(ch) => Some(ch.to_string()),
                CellContent::Cluster(s) => Some(s.to_string()),
                CellContent::Empty => Some(" ".to_string()),
                // The leader at col-1 owns the glyph; emitting a space here
                // would put a phantom gap after every wide char in copies.
                CellContent::WideSpacer => None,
                _ => Some(" ".to_string()),
            })
            .collect()
    }

    /// One row as runs of `(text, fg, attrs)`, merging consecutive same-styled
    /// cells and trimming the trailing blank run. Drives per-cell foreground
    /// color + text attributes. SGR 8 (conceal) cells shape as blanks here so
    /// hidden text never reaches the glyph pass.
    pub fn row_runs(&self, row: u16) -> Vec<(String, ember_core::Rgb, ember_core::Attrs)> {
        let cols = self.dims.columns as usize;
        let start = row as usize * cols;
        let end = (start + cols).min(self.cells.len());
        let mut runs: Vec<(String, ember_core::Rgb, ember_core::Attrs)> = Vec::new();
        for cell in &self.cells[start..end] {
            // The wide glyph's own advance covers the spacer's column — shaping
            // a space there would shift the rest of the row right by a cell.
            if matches!(cell.content, CellContent::WideSpacer) {
                continue;
            }
            let style = self.style_of(cell.style);
            let mut push = |txt: &str| {
                let fg = style.fg;
                match runs.last_mut() {
                    Some((text, run_fg, run_attrs))
                        if *run_fg == fg && *run_attrs == style.attrs =>
                    {
                        text.push_str(txt)
                    }
                    _ => runs.push((txt.to_string(), fg, style.attrs)),
                }
            };
            if style.attrs.contains(ember_core::Attrs::HIDDEN) {
                push(" ");
                continue;
            }
            match &cell.content {
                // Sprite-path glyphs are drawn as a `CustomGlyph`
                // (see `sprite::pane_custom_glyphs`), not shaped text — suppress
                // here so the font never double-draws it, same pattern as the
                // U+23FA monochrome remap above.
                CellContent::Char(c) if crate::sprite::is_sprite_glyph(*c) => push(" "),
                CellContent::Char(c) => push(&monochrome_glyph(*c).to_string()),
                // Full cluster: combining accents / ZWJ emoji shape as one glyph.
                CellContent::Cluster(s) => match remap_cluster(s) {
                    Some(remapped) => push(&remapped),
                    None => push(s),
                },
                _ => push(" "),
            }
        }
        // Trailing blanks have no glyph; drop them to keep the buffer small.
        while runs.last().is_some_and(|(t, _, _)| t.trim().is_empty()) {
            runs.pop();
        }
        if let Some((text, _, _)) = runs.last_mut() {
            let trimmed = text.trim_end().to_string();
            *text = trimmed;
        }
        runs
    }

    /// Columns in `row` carrying a sprite-path glyph, with the codepoint,
    /// each cell's foreground color, and its attrs (bold/dim) —
    /// `sprite::row_custom_glyphs` turns this into `CustomGlyph`s. Mirrors
    /// `row_runs`'s per-cell style lookup, including its SGR 8 (conceal)
    /// handling: a hidden cell must not reach the sprite pass either, or a
    /// concealed box-drawing char would render its sprite instead of
    /// staying blank.
    pub fn sprite_glyphs(&self, row: u16) -> Vec<(u16, char, ember_core::Rgb, ember_core::Attrs)> {
        let cols = self.dims.columns as usize;
        let start = row as usize * cols;
        let end = (start + cols).min(self.cells.len());
        self.cells[start..end]
            .iter()
            .enumerate()
            .filter_map(|(i, cell)| {
                let style = self.style_of(cell.style);
                if style.attrs.contains(ember_core::Attrs::HIDDEN) {
                    return None;
                }
                match &cell.content {
                    CellContent::Char(c) if crate::sprite::is_sprite_glyph(*c) => {
                        Some((i as u16, *c, style.fg, style.attrs))
                    }
                    _ => None,
                }
            })
            .collect()
    }

    /// The whole screen as text, rows separated by `\n` (trailing blanks trimmed
    /// per row to keep the buffer small).
    pub fn screen_text(&self) -> String {
        (0..self.dims.screen_lines)
            .map(|r| {
                let row = self.row_text(r);
                row.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Where a link came from. `Explicit` (OSC 8) is added in the follow-up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LinkSource {
    /// Matched by the plain-text URL scanner.
    Detected,
}

/// One row's segment of a clickable link. Multi-row (soft-wrapped) URLs
/// produce one span per touched row, sharing a `link_id`.
#[derive(Clone, Debug, PartialEq)]
pub struct LinkSpan {
    pub link_id: u32,
    pub row: u16,
    pub cols: std::ops::Range<u16>,
    pub url: String,
    pub source: LinkSource,
}

impl GridModel {
    /// Detect clickable URLs in the visible grid. Soft-wrapped rows (the
    /// `wrapped` flag on a row's last cell) are joined into one logical line
    /// before scanning, so long URLs that wrap still match; a char→(row,col)
    /// map converts match ranges back to grid columns (wide cells make
    /// string index ≠ column). Concealed (SGR 8) cells contribute blanks —
    /// hidden text is not a link.
    pub fn link_spans(&self) -> Vec<LinkSpan> {
        let cols = self.dims.columns as usize;
        let lines = self.dims.screen_lines;
        let mut out = Vec::new();
        let mut link_id = 0u32;
        let mut row = 0u16;
        while row < lines {
            // Join this row with following rows while the wrapped flag is set.
            let mut text = String::new();
            let mut map: Vec<(u16, u16)> = Vec::new(); // char index -> (row, col)
            let mut r = row;
            loop {
                let start = r as usize * cols;
                let end = (start + cols).min(self.cells.len());
                let mut wrapped = false;
                for (i, cell) in self.cells[start..end].iter().enumerate() {
                    let col = i as u16;
                    let hidden = self
                        .styles
                        .get(&cell.style)
                        .is_some_and(|s| s.attrs.contains(ember_core::Attrs::HIDDEN));
                    match (&cell.content, hidden) {
                        (CellContent::WideSpacer, _) => {} // leader owns the glyph
                        (CellContent::Char(ch), false) => {
                            text.push(*ch);
                            map.push((r, col));
                        }
                        (CellContent::Cluster(s), false) => {
                            // A cluster is one grid cell; its first char stands
                            // in for URL scanning (URLs are ASCII anyway).
                            text.push(s.chars().next().unwrap_or(' '));
                            map.push((r, col));
                        }
                        _ => {
                            text.push(' ');
                            map.push((r, col));
                        }
                    }
                    if i + 1 == end - start {
                        wrapped = cell.wrapped;
                    }
                }
                if wrapped && r + 1 < lines {
                    r += 1;
                } else {
                    break;
                }
            }

            for m in ember_core::links::find_urls(&text) {
                let url = text[m.bytes.clone()].to_string();
                // Group consecutive chars by row into per-row col ranges.
                let mut seg: Option<(u16, u16, u16)> = None; // (row, first, last)
                for ci in m.chars.clone() {
                    let (cr, cc) = map[ci];
                    match seg {
                        Some((sr, first, _)) if sr == cr => seg = Some((sr, first, cc)),
                        Some((sr, first, last)) => {
                            out.push(LinkSpan {
                                link_id,
                                row: sr,
                                cols: first..last + 1,
                                url: url.clone(),
                                source: LinkSource::Detected,
                            });
                            seg = Some((cr, cc, cc));
                        }
                        None => seg = Some((cr, cc, cc)),
                    }
                }
                if let Some((sr, first, last)) = seg {
                    out.push(LinkSpan {
                        link_id,
                        row: sr,
                        cols: first..last + 1,
                        url,
                        source: LinkSource::Detected,
                    });
                }
                link_id += 1;
            }
            row = r + 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ember_core::{CellPatch, NeutralCell};

    fn delta_with(epoch: u64, dims: GridDims, cells: Vec<CellPatch>, reset: bool) -> GridDelta {
        GridDelta {
            epoch,
            dims,
            reset,
            cells,
            ..Default::default()
        }
    }

    fn patch(row: u16, col: u16, ch: char) -> CellPatch {
        CellPatch {
            row,
            col,
            cell: NeutralCell::new(CellContent::Char(ch), StyleId(0)),
        }
    }

    #[test]
    fn applies_patches_into_rows() {
        let dims = GridDims::new(10, 3);
        let mut g = GridModel::new(dims);
        g.apply(delta_with(
            1,
            dims,
            vec![patch(0, 0, 'h'), patch(0, 1, 'i'), patch(1, 0, 'y')],
            true,
        ));
        assert_eq!(g.row_text(0).trim_end(), "hi");
        assert_eq!(g.row_text(1).trim_end(), "y");
        assert_eq!(g.screen_text(), "hi\ny\n");
    }

    #[test]
    fn row_runs_merge_by_color_and_trim() {
        use ember_core::{Rgb, Style};
        let dims = GridDims::new(10, 1);
        let mut g = GridModel::new(dims);
        let red = Style {
            fg: Rgb::new(255, 0, 0),
            ..Default::default()
        };
        let mut d = GridDelta {
            epoch: 1,
            dims,
            reset: true,
            cells: vec![
                CellPatch {
                    row: 0,
                    col: 0,
                    cell: NeutralCell::new(CellContent::Char('a'), StyleId(1)),
                },
                CellPatch {
                    row: 0,
                    col: 1,
                    cell: NeutralCell::new(CellContent::Char('b'), StyleId(1)),
                },
                CellPatch {
                    row: 0,
                    col: 2,
                    cell: NeutralCell::new(CellContent::Char('c'), StyleId(0)),
                },
            ],
            ..Default::default()
        };
        d.new_styles = vec![(StyleId(1), red), (StyleId(0), Style::default())];
        g.apply(d);
        let runs = g.row_runs(0);
        // "ab" (red) merges into one run; "c" (default) is its own; blanks trimmed.
        assert_eq!(runs.len(), 2);
        assert_eq!(
            runs[0],
            (
                "ab".to_string(),
                Rgb::new(255, 0, 0),
                ember_core::Attrs::empty()
            )
        );
        assert_eq!(runs[1].0, "c");
    }

    #[test]
    fn runs_split_on_attr_change_and_conceal_blanks() {
        let dims = GridDims::new(10, 1);
        let mut g = GridModel::new(dims);
        let bold = Style {
            attrs: ember_core::Attrs::BOLD,
            ..Style::default()
        };
        let hidden = Style {
            attrs: ember_core::Attrs::HIDDEN,
            ..Style::default()
        };
        let mut d = delta_with(1, dims, Vec::new(), false);
        d.cells = vec![
            CellPatch {
                row: 0,
                col: 0,
                cell: NeutralCell::new(CellContent::Char('a'), StyleId(0)),
            },
            CellPatch {
                row: 0,
                col: 1,
                cell: NeutralCell::new(CellContent::Char('b'), StyleId(1)),
            },
            CellPatch {
                row: 0,
                col: 2,
                cell: NeutralCell::new(CellContent::Char('s'), StyleId(2)),
            },
            CellPatch {
                row: 0,
                col: 3,
                cell: NeutralCell::new(CellContent::Char('z'), StyleId(0)),
            },
        ];
        d.new_styles = vec![
            (StyleId(0), Style::default()),
            (StyleId(1), bold),
            (StyleId(2), hidden),
        ];
        g.apply(d);
        let runs = g.row_runs(0);
        // Same fg throughout, but attrs split the runs; the concealed 's'
        // shapes as a blank.
        assert_eq!(runs.len(), 4);
        assert_eq!(runs[0].0, "a");
        assert_eq!(
            (runs[1].0.as_str(), runs[1].2),
            ("b", ember_core::Attrs::BOLD)
        );
        assert_eq!(
            (runs[2].0.as_str(), runs[2].2),
            (" ", ember_core::Attrs::HIDDEN)
        );
        assert_eq!(runs[3].0, "z");
    }

    #[test]
    fn concealed_sprite_glyph_does_not_reach_the_sprite_pass() {
        // A box-drawing char under SGR 8 (conceal) must stay blank, same as
        // it does on the text path (`row_runs` above) — otherwise a hidden
        // cell would render a visible sprite instead of nothing.
        let dims = GridDims::new(5, 1);
        let mut g = GridModel::new(dims);
        let hidden = Style {
            attrs: ember_core::Attrs::HIDDEN,
            ..Style::default()
        };
        let mut d = delta_with(1, dims, Vec::new(), true);
        d.cells = vec![CellPatch {
            row: 0,
            col: 0,
            cell: NeutralCell::new(CellContent::Char('\u{2500}'), StyleId(1)),
        }];
        d.new_styles = vec![(StyleId(0), Style::default()), (StyleId(1), hidden)];
        g.apply(d);
        assert!(g.sprite_glyphs(0).is_empty());
    }

    #[test]
    fn wide_color_emoji_remapped_to_monochrome_for_display_only() {
        // U+23FA (⏺, Claude Code's tool-activity bullet) is engine-width 1 but
        // its bare font glyph is a color emoji the shaper draws ~2 cells wide,
        // shoving the rest of the row. The glyph pass swaps in the monochrome
        // U+25CF (●) look-alike, which shapes to one cell.
        let dims = GridDims::new(10, 1);
        let mut g = GridModel::new(dims);
        g.apply(delta_with(1, dims, vec![patch(0, 0, '\u{23FA}')], true));
        // Display path (row_runs → glyph pass) gets the monochrome circle...
        assert_eq!(g.row_runs(0)[0].0, "\u{25CF}");
        // ...but the text path (copy/paste, selection) keeps the real char.
        assert_eq!(g.row_text(0).trim_end(), "\u{23FA}");
    }

    #[test]
    fn reset_rebuilds_grid() {
        let dims = GridDims::new(10, 3);
        let mut g = GridModel::new(dims);
        g.apply(delta_with(1, dims, vec![patch(0, 0, 'x')], false));
        assert_eq!(g.row_text(0).trim_end(), "x");
        // A reset clears prior content.
        g.apply(delta_with(2, dims, vec![patch(0, 0, 'z')], true));
        assert_eq!(g.row_text(0).trim_end(), "z");
    }

    /// Write `text` into `g` starting at (row, col), one Char cell per char.
    fn put(g: &mut GridModel, epoch: u64, dims: GridDims, row: u16, col: u16, text: &str) {
        let cells = text
            .chars()
            .enumerate()
            .map(|(i, ch)| patch(row, col + i as u16, ch))
            .collect();
        g.apply(delta_with(epoch, dims, cells, false));
    }

    #[test]
    fn link_spans_finds_a_url_on_one_row() {
        let dims = GridDims::new(40, 3);
        let mut g = GridModel::new(dims);
        g.apply(delta_with(1, dims, vec![], true));
        put(&mut g, 2, dims, 1, 3, "see https://a.io now");
        let spans = g.link_spans();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].row, 1);
        assert_eq!(spans[0].cols, 7..19); // "https://a.io" starts at col 3+4
        assert_eq!(spans[0].url, "https://a.io");
    }

    #[test]
    fn link_spans_joins_soft_wrapped_rows() {
        let dims = GridDims::new(10, 3);
        let mut g = GridModel::new(dims);
        g.apply(delta_with(1, dims, vec![], true));
        // Row 0: "https://ex" wrapped into row 1: "ample.com/" wrapped into row 2: "path x"
        put(&mut g, 2, dims, 0, 0, "https://ex");
        put(&mut g, 3, dims, 1, 0, "ample.com/");
        put(&mut g, 4, dims, 2, 0, "path x");
        // Set the wrapped flag on the LAST cell of rows 0 and 1.
        let mut wrap0 = patch(0, 9, 'x');
        wrap0.cell = NeutralCell {
            wrapped: true,
            ..NeutralCell::new(CellContent::Char('x'), StyleId(0))
        };
        let mut wrap1 = patch(1, 9, '/');
        wrap1.cell = NeutralCell {
            wrapped: true,
            ..NeutralCell::new(CellContent::Char('/'), StyleId(0))
        };
        g.apply(delta_with(5, dims, vec![wrap0, wrap1], false));
        let spans = g.link_spans();
        assert_eq!(spans.len(), 3, "one segment per touched row");
        let id = spans[0].link_id;
        assert!(spans.iter().all(|s| s.link_id == id));
        assert!(spans.iter().all(|s| s.url == "https://example.com/path"));
        assert_eq!((spans[0].row, spans[0].cols.clone()), (0, 0..10));
        assert_eq!((spans[1].row, spans[1].cols.clone()), (1, 0..10));
        assert_eq!((spans[2].row, spans[2].cols.clone()), (2, 0..4));
    }

    #[test]
    fn link_spans_maps_columns_past_wide_cells() {
        let dims = GridDims::new(30, 1);
        let mut g = GridModel::new(dims);
        g.apply(delta_with(1, dims, vec![], true));
        // Col 0: wide 你 (leader) + col 1: spacer; URL starts at col 3.
        let mut wide = patch(0, 0, '你');
        wide.cell = NeutralCell {
            wide: true,
            ..NeutralCell::new(CellContent::Char('你'), StyleId(0))
        };
        let spacer = CellPatch {
            row: 0,
            col: 1,
            cell: NeutralCell::new(CellContent::WideSpacer, StyleId(0)),
        };
        g.apply(delta_with(2, dims, vec![wide, spacer], false));
        put(&mut g, 3, dims, 0, 3, "https://a.io");
        let spans = g.link_spans();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].cols, 3..15, "columns, not char indices");
    }

    #[test]
    fn link_spans_skips_concealed_text() {
        use ember_core::{Attrs as CellAttrs, Style};
        let dims = GridDims::new(30, 1);
        let mut g = GridModel::new(dims);
        let hidden = Style {
            attrs: CellAttrs::HIDDEN,
            ..Style::default()
        };
        let mut d = delta_with(1, dims, vec![], true);
        d.new_styles = vec![(StyleId(7), hidden)];
        g.apply(d);
        let cells = "https://a.io"
            .chars()
            .enumerate()
            .map(|(i, ch)| CellPatch {
                row: 0,
                col: i as u16,
                cell: NeutralCell::new(CellContent::Char(ch), StyleId(7)),
            })
            .collect();
        g.apply(delta_with(2, dims, cells, false));
        assert!(g.link_spans().is_empty(), "hidden text is not a link");
    }
}
