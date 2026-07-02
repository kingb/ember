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

/// An in-progress or finalized selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Point,
    pub active: Point,
    pub mode: SelectionMode,
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
    pub fn row_span(&self, grid: &GridModel, row: u16) -> Option<(u16, u16)> {
        let (s, e) = self.effective_range(grid);
        if row < s.row || row > e.row {
            return None;
        }
        let last_col = grid.dims.columns.saturating_sub(1);
        let c0 = if row == s.row { s.col } else { 0 };
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
}
