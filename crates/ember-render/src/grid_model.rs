//! The render-owned neutral grid (design §6). Render owns this; it applies owned
//! [`GridDelta`]s off the pixel lane and never touches engine memory. Pure and
//! headlessly testable — no GPU involved.

use std::collections::HashMap;

use ember_core::{CellContent, CursorState, GridDelta, GridDims, NeutralCell, Style, StyleId};

/// The current screen state, reconstructed from coalesced deltas.
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
    /// OSC 133 command marks visible this frame, as `(row, status)`.
    pub marks: Vec<(u16, ember_core::MarkStatus)>,
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
            .map(|c| match &c.content {
                CellContent::Char(ch) => *ch,
                CellContent::Cluster(s) => s.chars().next().unwrap_or(' '),
                CellContent::Empty => ' ',
            })
            .collect()
    }

    /// One row as runs of `(text, fg)`, merging consecutive same-fg cells and
    /// trimming the trailing blank run. Drives per-cell foreground color.
    pub fn row_runs(&self, row: u16) -> Vec<(String, ember_core::Rgb)> {
        let cols = self.dims.columns as usize;
        let start = row as usize * cols;
        let end = (start + cols).min(self.cells.len());
        let mut runs: Vec<(String, ember_core::Rgb)> = Vec::new();
        for cell in &self.cells[start..end] {
            let ch = match &cell.content {
                CellContent::Char(c) => *c,
                CellContent::Cluster(s) => s.chars().next().unwrap_or(' '),
                CellContent::Empty => ' ',
            };
            let fg = self.style_of(cell.style).fg;
            match runs.last_mut() {
                Some((text, run_fg)) if *run_fg == fg => text.push(ch),
                _ => runs.push((ch.to_string(), fg)),
            }
        }
        // Trailing blanks have no glyph; drop them to keep the buffer small.
        while runs.last().is_some_and(|(t, _)| t.trim().is_empty()) {
            runs.pop();
        }
        if let Some((text, _)) = runs.last_mut() {
            let trimmed = text.trim_end().to_string();
            *text = trimmed;
        }
        runs
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
        assert_eq!(runs[0], ("ab".to_string(), Rgb::new(255, 0, 0)));
        assert_eq!(runs[1].0, "c");
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
}
