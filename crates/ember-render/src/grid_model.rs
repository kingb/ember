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
}

impl GridModel {
    pub fn new(dims: GridDims) -> Self {
        Self {
            dims,
            cells: vec![NeutralCell::default(); dims.cells()],
            styles: HashMap::new(),
            cursor: CursorState::default(),
            epoch: 0,
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
    }

    pub fn style_of(&self, id: StyleId) -> Style {
        self.styles.get(&id).copied().unwrap_or_default()
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
