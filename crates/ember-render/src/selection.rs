//! Text selection over the neutral [`GridModel`] (design §6; copy/paste basics).
//!
//! Selection lives in the neutral layer — never the engine. A [`Selection`] holds
//! an anchor + active point (cell coords) and a [`SelectionMode`]; the effective
//! range is expanded against the grid at query time (Alacritty-style simple / word
//! / line semantics), so the same selection drives both the highlight quads and
//! the copied text. Pure + headlessly testable (no GPU).

use ember_core::CellContent;

use crate::grid_model::GridModel;

/// How a drag selects: by cell, by word, or by whole line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SelectionMode {
    /// Cell-precise (single click + drag).
    Simple,
    /// Snap each end to word boundaries (double click).
    Word,
    /// Whole rows (triple click).
    Line,
}

/// A cell coordinate within a pane's grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Point {
    pub row: u16,
    pub col: u16,
}

impl Point {
    pub fn new(row: u16, col: u16) -> Self {
        Self { row, col }
    }
}

/// An in-progress or finalized selection, in CURRENT-VIEWPORT coordinates —
/// the projection [`AnchoredSelection::project`] produces each frame, and the
/// space all the query fns below ([`Selection::row_span`], [`Selection::text`])
/// operate in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Point,
    pub active: Point,
    pub mode: SelectionMode,
}

/// A cell position anchored to a scrollback-ABSOLUTE line (the
/// [`crate::grid_model::GridModel::abs_of_row`] space), so it stays glued to
/// its text as output scrolls by or the user pages through history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct AbsPoint {
    pub line: u32,
    pub col: u16,
}

/// The selection the app actually stores: endpoints anchored to absolute
/// lines. Scrolling (new output rotating lines into history, or the user
/// moving the display offset) leaves these untouched — the per-frame
/// [`Self::project`] maps them into whatever the viewport currently shows,
/// so the highlight travels WITH its text (iTerm2 behavior) instead of
/// sitting at fixed screen rows while different text slides through it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnchoredSelection {
    pub anchor: AbsPoint,
    pub active: AbsPoint,
    pub mode: SelectionMode,
}

impl AnchoredSelection {
    /// Start a selection at viewport cell `p` (anchor = active), capturing
    /// the absolute line under it right now.
    pub fn new(grid: &GridModel, p: Point, mode: SelectionMode) -> Self {
        let a = AbsPoint {
            line: grid.abs_of_row(p.row),
            col: p.col,
        };
        Self {
            anchor: a,
            active: a,
            mode,
        }
    }

    /// Move the active end to viewport cell `p` (during a drag).
    pub fn update(&mut self, grid: &GridModel, p: Point) {
        self.active = AbsPoint {
            line: grid.abs_of_row(p.row),
            col: p.col,
        };
    }

    /// Whether this is an un-dragged single Simple click (the "clear the
    /// previous selection" gesture, not a selection of its own).
    pub fn is_empty_click(&self) -> bool {
        self.mode == SelectionMode::Simple && self.anchor == self.active
    }

    /// Project into the current viewport as a [`Selection`], clamped to the
    /// visible intersection: an endpoint scrolled above the view clamps to
    /// row 0 col 0 (the visible part is a continuation), one below clamps to
    /// the last row's last column. `None` when the whole selection is out of
    /// view — scrolled away (and it comes back when scrolled back to).
    pub fn project(&self, grid: &GridModel) -> Option<Selection> {
        let (s, e) = if self.anchor <= self.active {
            (self.anchor, self.active)
        } else {
            (self.active, self.anchor)
        };
        let rows = grid.dims.screen_lines;
        if rows == 0 {
            return None;
        }
        let base = grid.abs_of_row0();
        let top = base;
        let bottom = base + (rows as u32 - 1);
        if e.line < top || s.line > bottom {
            return None; // fully scrolled out (above or below)
        }
        let last_col = grid.dims.columns.saturating_sub(1);
        let start = if s.line < top {
            Point::new(0, 0)
        } else {
            Point::new((s.line - base) as u16, s.col)
        };
        let end = if e.line > bottom {
            Point::new(rows - 1, last_col)
        } else {
            Point::new((e.line - base) as u16, e.col)
        };
        Some(Selection {
            anchor: start,
            active: end,
            mode: self.mode,
        })
    }
}

/// Character class for word-boundary detection (Alacritty-style): word chars vs
/// whitespace vs everything-else (punctuation), so a double-click grabs a word.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Word,
    Space,
    Other,
}

fn class_of(ch: char) -> Class {
    if ch == ' ' || ch == '\0' {
        Class::Space
    } else if ch.is_alphanumeric() || ch == '_' {
        Class::Word
    } else {
        Class::Other
    }
}

/// The char a cell renders as (Empty/spacer → space), for selection text + classes.
fn cell_char(grid: &GridModel, row: u16, col: u16) -> char {
    match grid.cell(row, col).map(|c| &c.content) {
        Some(CellContent::Char(c)) => *c,
        Some(CellContent::Cluster(s)) => s.chars().next().unwrap_or(' '),
        _ => ' ',
    }
}

impl Selection {
    /// Start a selection at `p` in `mode` (anchor = active).
    pub fn new(p: Point, mode: SelectionMode) -> Self {
        Self {
            anchor: p,
            active: p,
            mode,
        }
    }

    /// Move the active end (during a drag).
    pub fn update(&mut self, p: Point) {
        self.active = p;
    }

    /// `(start, end)` ordered top-left → bottom-right (inclusive of both cells).
    fn ordered(&self) -> (Point, Point) {
        if self.anchor <= self.active {
            (self.anchor, self.active)
        } else {
            (self.active, self.anchor)
        }
    }

    /// The effective inclusive `(start, end)` after mode expansion against `grid`.
    pub fn effective_range(&self, grid: &GridModel) -> (Point, Point) {
        let (mut s, mut e) = self.ordered();
        let cols = grid.dims.columns;
        let last_col = cols.saturating_sub(1);
        match self.mode {
            SelectionMode::Simple => {}
            SelectionMode::Word => {
                s.col = word_start(grid, s.row, s.col);
                e.col = word_end(grid, e.row, e.col, last_col);
            }
            SelectionMode::Line => {
                s.col = 0;
                e.col = last_col;
            }
        }
        (s, e)
    }

    /// The selected columns `[c0, c1]` (inclusive) for `row`, if any — drives the
    /// highlight quads. `None` for rows outside the selection.
    ///
    /// `self.anchor`/`self.active` are absolute cell coordinates captured at
    /// whatever pane size was current when the selection was made or last
    /// extended; nothing shrinks them when the pane is resized narrower
    /// afterward (a split, or a keyboard-driven resize) — the `Selection`
    /// just sits there, stale, until the next click clears it or a new drag
    /// starts. `c1` was already clamped to the CURRENT `last_col` below, but
    /// `c0` wasn't: a selection whose start column also falls past the new
    /// (narrower) width produced `c0 > c1`, and every caller — this crate's
    /// `selection_quads` computing `(c1 - c0 + 1) as f32 * cw` foremost —
    /// assumes `c0 <= c1`, so that underflowed and panicked. Rather than
    /// mutate/clear the `Selection` at resize time (which would need a hook
    /// on every resize path — divider drag, split, keyboard chord — to find
    /// and special-case, and would throw away a still-meaningful selection
    /// on a one-column trim), clamp here, at the single query point every
    /// consumer (this fn's callers: the highlight quads AND `text()`'s copy
    /// path) already goes through: a row whose selection starts past the
    /// visible width has nothing left to show for THAT row, so it's
    /// dropped; a row where only the end overruns keeps showing its now-
    /// truncated visible portion (this is also how Alacritty et al. treat a
    /// live selection surviving a resize — clamped to what's visible, not
    /// discarded wholesale).
    pub fn row_span(&self, grid: &GridModel, row: u16) -> Option<(u16, u16)> {
        let (s, e) = self.effective_range(grid);
        if row < s.row || row > e.row {
            return None;
        }
        let last_col = grid.dims.columns.saturating_sub(1);
        let c0 = if row == s.row { s.col } else { 0 };
        if c0 > last_col {
            return None;
        }
        let c1 = if row == e.row { e.col } else { last_col };
        Some((c0, c1.min(last_col)))
    }

    /// The selected text. A soft-wrapped row is joined to the next **without** a
    /// newline (so a wrapped logical line copies as one line); a hard line-break is
    /// a `\n`. Hard lines are trailing-trimmed; a wrapped row keeps its full width
    /// (its content continues into the next row).
    pub fn text(&self, grid: &GridModel) -> String {
        let (s, e) = self.effective_range(grid);
        let last_col = grid.dims.columns.saturating_sub(1);
        let end_row = e.row.min(grid.dims.screen_lines.saturating_sub(1));
        let mut out = String::new();
        for row in s.row..=end_row {
            let c0 = if row == s.row { s.col } else { 0 };
            let c1 = if row == e.row { e.col } else { last_col };
            let mut line = String::new();
            let mut col = c0;
            while col <= c1.min(last_col) {
                match grid.cell(row, col).map(|c| &c.content) {
                    Some(CellContent::Char(c)) => line.push(*c),
                    Some(CellContent::Cluster(cl)) => line.push_str(cl),
                    // The wide leader already contributed both columns' text.
                    Some(CellContent::WideSpacer) => {}
                    _ => line.push(' '),
                }
                col += 1;
            }
            // Does this row continue into the next selected row (soft wrap)?
            if row < end_row && grid.row_wrapped(row) {
                out.push_str(&line); // continues — no trim, no newline
            } else {
                out.push_str(line.trim_end());
                if row < end_row {
                    out.push('\n');
                }
            }
        }
        out
    }
}

/// Leftmost column of the word containing `col` on `row`.
fn word_start(grid: &GridModel, row: u16, col: u16) -> u16 {
    let class = class_of(cell_char(grid, row, col));
    let mut c = col;
    while c > 0 && class_of(cell_char(grid, row, c - 1)) == class {
        c -= 1;
    }
    c
}

/// Rightmost column of the word containing `col` on `row`.
fn word_end(grid: &GridModel, row: u16, col: u16, last_col: u16) -> u16 {
    let class = class_of(cell_char(grid, row, col));
    let mut c = col;
    while c < last_col && class_of(cell_char(grid, row, c + 1)) == class {
        c += 1;
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use ember_core::{CellPatch, GridDelta, GridDims, NeutralCell, StyleId};

    fn grid_from(rows: &[&str]) -> GridModel {
        let cols = rows.iter().map(|r| r.chars().count()).max().unwrap_or(1) as u16;
        let dims = GridDims::new(cols, rows.len() as u16);
        let mut g = GridModel::new(dims);
        let mut cells = Vec::new();
        for (r, line) in rows.iter().enumerate() {
            for (c, ch) in line.chars().enumerate() {
                cells.push(CellPatch {
                    row: r as u16,
                    col: c as u16,
                    cell: NeutralCell::new(CellContent::Char(ch), StyleId(0)),
                });
            }
        }
        g.apply(GridDelta {
            epoch: 1,
            dims,
            reset: true,
            cells,
            ..Default::default()
        });
        g
    }

    fn sel(a: (u16, u16), b: (u16, u16), mode: SelectionMode) -> Selection {
        let mut s = Selection::new(Point::new(a.0, a.1), mode);
        s.update(Point::new(b.0, b.1));
        s
    }

    #[test]
    fn simple_single_row() {
        let g = grid_from(&["hello world"]);
        let s = sel((0, 0), (0, 4), SelectionMode::Simple);
        assert_eq!(s.text(&g), "hello");
    }

    #[test]
    fn simple_is_order_independent() {
        let g = grid_from(&["hello world"]);
        let s = sel((0, 4), (0, 0), SelectionMode::Simple); // dragged right-to-left
        assert_eq!(s.text(&g), "hello");
    }

    #[test]
    fn simple_multi_row_trims_and_newlines() {
        let g = grid_from(&["abc   ", "defgh "]);
        let s = sel((0, 0), (1, 5), SelectionMode::Simple);
        // First row trailing blanks trimmed; rows joined with newline.
        assert_eq!(s.text(&g), "abc\ndefgh");
    }

    #[test]
    fn word_double_click_grabs_word() {
        let g = grid_from(&["hello world"]);
        // Click anywhere in "world" (cols 6..10) in Word mode.
        let s = sel((0, 8), (0, 8), SelectionMode::Word);
        assert_eq!(s.text(&g), "world");
    }

    #[test]
    fn word_stops_at_punctuation() {
        let g = grid_from(&["foo.bar baz"]);
        let s = sel((0, 1), (0, 1), SelectionMode::Word); // inside "foo"
        assert_eq!(s.text(&g), "foo");
    }

    #[test]
    fn line_mode_selects_whole_row() {
        let g = grid_from(&["hi there"]);
        let s = sel((0, 2), (0, 2), SelectionMode::Line);
        assert_eq!(s.text(&g), "hi there");
    }

    #[test]
    fn row_span_for_highlight() {
        let g = grid_from(&["hello world"]);
        let s = sel((0, 2), (0, 6), SelectionMode::Simple);
        assert_eq!(s.row_span(&g, 0), Some((2, 6)));
        assert_eq!(s.row_span(&g, 1), None);
    }

    /// Reproduces the P1 crash (surface-drag task 4's report, task 5's
    /// fix): a selection made on a wide pane, entirely past the column
    /// range of the SAME pane after a resize (e.g. a split) shrinks it
    /// narrower — `paint::selection_quads` used to compute `c1 - c0 + 1` on
    /// the raw `(c0, c1)` from `row_span` and underflow-panic the instant
    /// `c0` (still the wide-pane column) exceeded the new, narrower
    /// `last_col`. `row_span` must now drop that row (`None`) rather than
    /// hand back a `c0 > c1` pair.
    #[test]
    fn row_span_drops_a_row_whose_start_column_outlives_a_shrink() {
        let mut g = grid_from(&["0123456789abcdefghij"]); // 20 cols wide.
        // Selected columns 12..=18 while the pane was 20 cols wide.
        let s = sel((0, 12), (0, 18), SelectionMode::Simple);
        assert_eq!(s.row_span(&g, 0), Some((12, 18)));

        // The pane (e.g. a vertical split) shrinks to 5 cols — narrower
        // than the selection's start column — WITHOUT the selection itself
        // being cleared or re-clamped (this is the actual failure mode: a
        // stale `Selection` surviving a resize with no hook to touch it).
        g.dims = GridDims::new(5, 1);
        assert_eq!(
            s.row_span(&g, 0),
            None,
            "a selection start column past the new width must be dropped, not \
             handed back with c0 > c1 (that's the underflow the crash was)"
        );
    }

    /// The companion case: only the END column outlives the shrink, not the
    /// start — the row still has SOMETHING visible to highlight/copy, just
    /// truncated to the new width, matching how a live selection surviving
    /// a resize reads elsewhere (Alacritty et al.): clamped, not discarded.
    #[test]
    fn row_span_clamps_end_column_that_outlives_a_shrink() {
        let mut g = grid_from(&["0123456789abcdefghij"]); // 20 cols wide.
        let s = sel((0, 2), (0, 18), SelectionMode::Simple);
        g.dims = GridDims::new(5, 1); // last_col = 4 now.
        assert_eq!(s.row_span(&g, 0), Some((2, 4)));
    }

    #[test]
    fn wrapped_row_joins_without_newline() {
        // row0 soft-wraps into row1 (its last cell carries WRAPLINE) → one logical
        // line, copied with no newline between.
        let dims = GridDims::new(4, 2);
        let mut g = GridModel::new(dims);
        let mut cells = Vec::new();
        for (c, ch) in "abcd".chars().enumerate() {
            let mut cell = NeutralCell::new(CellContent::Char(ch), StyleId(0));
            if c == 3 {
                cell.wrapped = true; // last cell of the wrapped row
            }
            cells.push(CellPatch {
                row: 0,
                col: c as u16,
                cell,
            });
        }
        for (c, ch) in "ef".chars().enumerate() {
            cells.push(CellPatch {
                row: 1,
                col: c as u16,
                cell: NeutralCell::new(CellContent::Char(ch), StyleId(0)),
            });
        }
        g.apply(GridDelta {
            epoch: 1,
            dims,
            reset: true,
            cells,
            ..Default::default()
        });
        let s = sel((0, 0), (1, 3), SelectionMode::Simple);
        assert_eq!(s.text(&g), "abcdef");
    }

    #[test]
    fn hard_line_keeps_newline() {
        // Same shape but NOT wrapped → newline preserved between the two lines.
        let g = grid_from(&["abcd", "ef  "]);
        let s = sel((0, 0), (1, 3), SelectionMode::Simple);
        assert_eq!(s.text(&g), "abcd\nef");
    }

    #[test]
    fn multi_row_span_fills_middle() {
        let g = grid_from(&["aaaa", "bbbb", "cccc"]);
        let s = sel((0, 2), (2, 1), SelectionMode::Simple);
        assert_eq!(s.row_span(&g, 0), Some((2, 3))); // from col2 to end
        assert_eq!(s.row_span(&g, 1), Some((0, 3))); // full middle row
        assert_eq!(s.row_span(&g, 2), Some((0, 1))); // to col1
    }

    // --- Anchored selections: glued to text, not to viewport rows -----------

    #[test]
    fn anchored_selection_moves_up_as_output_scrolls() {
        let mut g = grid_from(&["aaaa", "bbbb", "cccc", "dddd"]);
        // Select viewport rows 2..3 at the live bottom (history 0).
        let a = AnchoredSelection::new(&g, Point::new(2, 1), SelectionMode::Simple);
        let mut a = a;
        a.update(&g, Point::new(3, 2));
        // One new output line rotates into history: same text is now one row up.
        g.history_len = 1;
        let v = a.project(&g).expect("still visible");
        assert_eq!((v.anchor.row, v.active.row), (1, 2), "rows shifted up by 1");
        assert_eq!((v.anchor.col, v.active.col), (1, 2), "cols untouched");
    }

    #[test]
    fn anchored_selection_scrolls_out_and_comes_back() {
        let mut g = grid_from(&["aaaa", "bbbb", "cccc", "dddd"]);
        let mut a = AnchoredSelection::new(&g, Point::new(0, 0), SelectionMode::Simple);
        a.update(&g, Point::new(1, 3));
        // Enough output that the selected text is fully above the viewport.
        g.history_len = 6;
        assert_eq!(a.project(&g), None, "scrolled away, not stuck on screen");
        // The user scrolls back up to it: it reappears at the same text.
        g.display_offset = 6;
        let v = a.project(&g).expect("scrolled back into view");
        assert_eq!((v.anchor.row, v.active.row), (0, 1));
    }

    #[test]
    fn anchored_selection_clamps_partially_visible_ends() {
        let mut g = grid_from(&["aaaa", "bbbb", "cccc", "dddd"]);
        let mut a = AnchoredSelection::new(&g, Point::new(0, 2), SelectionMode::Simple);
        a.update(&g, Point::new(3, 1));
        // Two lines rotate away: the start is above the view now.
        g.history_len = 2;
        let v = a.project(&g).expect("tail still visible");
        // Continuation semantics: visible part starts at row 0 col 0.
        assert_eq!((v.anchor.row, v.anchor.col), (0, 0));
        assert_eq!((v.active.row, v.active.col), (1, 1));
    }

    #[test]
    fn anchored_empty_click_detection_matches_simple_unmoved() {
        let g = grid_from(&["aaaa"]);
        let a = AnchoredSelection::new(&g, Point::new(0, 1), SelectionMode::Simple);
        assert!(a.is_empty_click());
        let w = AnchoredSelection::new(&g, Point::new(0, 1), SelectionMode::Word);
        assert!(!w.is_empty_click(), "word click is a real selection");
        let mut d = a;
        d.update(&g, Point::new(0, 2));
        assert!(!d.is_empty_click(), "a drag is a real selection");
    }
}
