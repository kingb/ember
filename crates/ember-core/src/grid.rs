//! The neutral grid contract (design §4; the B1 swappable-engine contract signatures).
//!
//! Engine-agnostic, owned, `Send`. Render owns the neutral grid; the engine's
//! native grid never leaks past this seam. A [`GridDelta`] is the inter-thread
//! message — owned and **mergeable**, so under backpressure successive frames
//! coalesce (never an unbounded queue, never a dropped frame).
//!
//! These types are deliberately *resolved* (concrete RGB, a single style key per
//! cell): both `alacritty_terminal` and `libghostty-vt` map onto them via their
//! own projection function — neither is assumed to memcpy into a `NeutralCell`.

use std::collections::BTreeMap;

use bitflags::bitflags;
use serde::{Deserialize, Serialize};

/// 8-bit RGB. The projection resolves engine colors — including the indexed
/// palette and the fg/bg/cursor defaults the engine does not supply — to
/// concrete RGB before they cross the seam.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

impl From<(u8, u8, u8)> for Rgb {
    fn from((r, g, b): (u8, u8, u8)) -> Self {
        Self { r, g, b }
    }
}

bitflags! {
    /// Per-cell rendering attributes — a superset both engines map onto
    /// (libghostty's `Style` POD carries exactly these as bools; alacritty's
    /// `Flags` map by name).
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct Attrs: u16 {
        const BOLD      = 1 << 0;
        const ITALIC    = 1 << 1;
        const UNDERLINE = 1 << 2;
        const INVERSE   = 1 << 3;
        const DIM       = 1 << 4;
        const STRIKEOUT = 1 << 5;
        const BLINK     = 1 << 6;
        const HIDDEN    = 1 << 7;
        const OVERLINE  = 1 << 8;
    }
}

/// Interned style key. The projection assigns ids and ships first-seen styles in
/// a delta's [`GridDelta::new_styles`]; render caches glyph rasters on
/// `(glyph, StyleId)` so unchanged glyphs skip rasterization (design §4, §6).
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct StyleId(pub u32);

/// The resolved style behind a [`StyleId`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Style {
    pub fg: Rgb,
    pub bg: Rgb,
    pub attrs: Attrs,
}

/// A cell's printable content: a single char (the common case), a multi-codepoint
/// grapheme cluster, or empty (blank — render fills the cell's bg, no glyph).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum CellContent {
    #[default]
    Empty,
    Char(char),
    Cluster(Box<str>),
}

/// One neutral cell: resolved content + an interned style key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NeutralCell {
    pub content: CellContent,
    pub style: StyleId,
    /// Set on the **last cell of a row** when that row soft-wraps into the next
    /// (alacritty's `WRAPLINE`). Lets copy join a wrapped logical line without a
    /// spurious newline. Meaningless on non-last cells.
    pub wrapped: bool,
}

impl NeutralCell {
    pub fn new(content: CellContent, style: StyleId) -> Self {
        Self {
            content,
            style,
            wrapped: false,
        }
    }
}

/// Grid dimensions in cells. Carries resize across the seam.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GridDims {
    pub columns: u16,
    pub screen_lines: u16,
}

impl GridDims {
    pub const fn new(columns: u16, screen_lines: u16) -> Self {
        Self {
            columns,
            screen_lines,
        }
    }

    pub fn cells(&self) -> usize {
        self.columns as usize * self.screen_lines as usize
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Beam,
    Hidden,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorState {
    pub row: u16,
    pub col: u16,
    pub shape: CursorShape,
    pub visible: bool,
}

/// A single damaged cell at `(row, col)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellPatch {
    pub row: u16,
    pub col: u16,
    pub cell: NeutralCell,
}

/// Shell-integration (OSC 133) command status, shown as a colored mark in the
/// pane's left gutter at the command's prompt line. `Running` = command in flight
/// (no exit yet); `Ok`/`Fail` from the command's exit code.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum MarkStatus {
    #[default]
    Running,
    Ok,
    Fail,
}

/// Owned, `Send`, **mergeable** render-bound delta (the B1 contract signature 1).
///
/// Under backpressure the producer merges successive drains into one delta (the
/// union of patches) — frames coalesce, never drop, so render never needs a
/// resync. Bounded by viewport size, so there is no unbounded queue.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridDelta {
    /// Monotonic; render asserts "this is newer".
    pub epoch: u64,
    /// Current dims (carries resize).
    pub dims: GridDims,
    /// `Damage::Full` → render rebuilds from scratch.
    pub reset: bool,
    /// Only the damaged cells.
    pub cells: Vec<CellPatch>,
    /// Styles first referenced by this delta.
    pub new_styles: Vec<(StyleId, Style)>,
    pub cursor: CursorState,
    /// Snapshot of the engine's bracketed-paste mode (DEC 2004) as of this drain —
    /// terminal state, like `cursor`, not damage. Lets the app wrap pastes in
    /// `ESC[200~`…`ESC[201~` only when the app asked for it. Latest-wins on merge.
    pub bracketed_paste: bool,
    /// Scrollback viewport state (terminal state, latest-wins on merge): how many
    /// lines the display is scrolled **up** from the live bottom (`0` = at bottom),
    /// and how many lines of history exist above.
    pub display_offset: u16,
    pub history_len: u16,
    /// Alternate screen active (vim/less/htop/…): there is NO scrollback here, so
    /// the app must suppress history scrolling and translate the wheel to arrows.
    pub alt_screen: bool,
    /// The app has enabled mouse reporting — the wheel should go to it as mouse
    /// events, not be translated to arrow keys.
    pub mouse_reporting: bool,
    /// OSC 133 command marks currently **visible** in the viewport, as
    /// `(visible_row, status)` — recomputed each drain from the marks' absolute
    /// history lines + `display_offset`, so they scroll with the content. Latest-
    /// wins on merge (terminal state, not damage).
    pub marks: Vec<(u16, MarkStatus)>,
}

impl GridDelta {
    /// An empty (no-damage) delta at `epoch`/`dims`.
    pub fn new(epoch: u64, dims: GridDims) -> Self {
        Self {
            epoch,
            dims,
            ..Default::default()
        }
    }

    /// A full-reset delta (render rebuilds from scratch).
    pub fn full(epoch: u64, dims: GridDims) -> Self {
        Self {
            epoch,
            dims,
            reset: true,
            ..Default::default()
        }
    }

    /// Merge `newer` into `self` (coalescing under backpressure). Newer per-cell
    /// patches win; newer cursor/dims/epoch take precedence; first-seen styles
    /// accumulate (newer wins per id); a `reset` supersedes all pending state.
    pub fn merge(&mut self, mut newer: GridDelta) {
        if newer.reset {
            // A reset rebuilds cells from scratch, but `StyleId`s are stable and
            // cumulative: the engine's interner ships each style as `new_styles`
            // exactly once, and `newer`'s cells may reference ids first seen in the
            // delta being superseded. Carry the learned styles forward (newer wins
            // per id) so a coalesced reset never strands a cell with no known style
            // — otherwise the consumer falls back to the default style and renders
            // black-on-black.
            let mut styles: BTreeMap<StyleId, Style> =
                std::mem::take(&mut self.new_styles).into_iter().collect();
            for (id, style) in std::mem::take(&mut newer.new_styles) {
                styles.insert(id, style);
            }
            newer.new_styles = styles.into_iter().collect();
            *self = newer;
            return;
        }
        // Union cell patches — newer wins per (row, col); BTreeMap keeps the
        // result deterministically ordered (row, then col).
        let mut cells: BTreeMap<(u16, u16), NeutralCell> = std::mem::take(&mut self.cells)
            .into_iter()
            .map(|p| ((p.row, p.col), p.cell))
            .collect();
        for p in newer.cells {
            cells.insert((p.row, p.col), p.cell);
        }
        self.cells = cells
            .into_iter()
            .map(|((row, col), cell)| CellPatch { row, col, cell })
            .collect();

        // Union first-seen styles — newer wins per id.
        let mut styles: BTreeMap<StyleId, Style> =
            std::mem::take(&mut self.new_styles).into_iter().collect();
        for (id, style) in newer.new_styles {
            styles.insert(id, style);
        }
        self.new_styles = styles.into_iter().collect();

        self.epoch = newer.epoch;
        self.dims = newer.dims;
        self.cursor = newer.cursor;
        self.bracketed_paste = newer.bracketed_paste;
        self.display_offset = newer.display_offset;
        self.history_len = newer.history_len;
        self.alt_screen = newer.alt_screen;
        self.mouse_reporting = newer.mouse_reporting;
        self.marks = newer.marks;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch(row: u16, col: u16, ch: char, style: u32) -> CellPatch {
        CellPatch {
            row,
            col,
            cell: NeutralCell::new(CellContent::Char(ch), StyleId(style)),
        }
    }

    #[test]
    fn merge_unions_distinct_cells() {
        let dims = GridDims::new(80, 24);
        let mut a = GridDelta::new(1, dims);
        a.cells = vec![patch(0, 0, 'a', 0)];
        let mut b = GridDelta::new(2, dims);
        b.cells = vec![patch(0, 1, 'b', 0)];
        a.merge(b);
        assert_eq!(a.epoch, 2);
        assert_eq!(a.cells, vec![patch(0, 0, 'a', 0), patch(0, 1, 'b', 0)]);
    }

    #[test]
    fn merge_newer_cell_wins_at_same_position() {
        let dims = GridDims::new(80, 24);
        let mut a = GridDelta::new(1, dims);
        a.cells = vec![patch(2, 3, 'o', 0)];
        let mut b = GridDelta::new(2, dims);
        b.cells = vec![patch(2, 3, 'n', 1)];
        a.merge(b);
        assert_eq!(a.cells, vec![patch(2, 3, 'n', 1)]);
    }

    #[test]
    fn merge_reset_supersedes_pending() {
        let dims = GridDims::new(80, 24);
        let mut a = GridDelta::new(1, dims);
        a.cells = vec![patch(0, 0, 'a', 0)];
        let b = GridDelta::full(5, dims);
        a.merge(b);
        assert!(a.reset);
        assert!(a.cells.is_empty());
        assert_eq!(a.epoch, 5);
    }

    #[test]
    fn merge_reset_carries_styles_forward() {
        // Regression: a styles-bearing delta coalescing with a later reset (e.g.
        // init styles + a resize reset, both pending before the first drain) must
        // not strand cells with no known style — else the consumer renders the
        // default style (black-on-black). The interner only ships each style once,
        // so the reset must keep the superseded delta's styles.
        let dims = GridDims::new(80, 24);
        let red = Style {
            fg: Rgb::new(255, 0, 0),
            ..Default::default()
        };
        let mut a = GridDelta::new(1, dims);
        a.new_styles = vec![(StyleId(0), red)];

        // A full reset whose cells reference StyleId(0) but ships no new styles.
        let mut b = GridDelta::full(2, dims);
        b.cells = vec![patch(0, 0, 'x', 0)];
        a.merge(b);

        assert!(a.reset);
        assert_eq!(
            a.new_styles,
            vec![(StyleId(0), red)],
            "reset must carry forward the style its cells reference"
        );
    }

    #[test]
    fn merge_accumulates_new_styles_newer_wins() {
        let dims = GridDims::new(80, 24);
        let mut a = GridDelta::new(1, dims);
        a.new_styles = vec![(StyleId(0), Style::default())];
        let mut b = GridDelta::new(2, dims);
        let red = Style {
            fg: Rgb::new(255, 0, 0),
            ..Default::default()
        };
        b.new_styles = vec![(StyleId(0), red), (StyleId(1), Style::default())];
        a.merge(b);
        assert_eq!(
            a.new_styles,
            vec![(StyleId(0), red), (StyleId(1), Style::default())]
        );
    }
}
