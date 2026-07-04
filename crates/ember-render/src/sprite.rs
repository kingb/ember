//! Wires the box-drawing "sprite font" into glyphon's custom-glyph path.
//! Per the  ADR, we use glyphon's `prepare_with_custom`/`CustomGlyph`
//! API rather than a bespoke atlas: glyphon owns the GPU atlas and caches
//! rasterized glyphs by `(id, physical size, subpixel bin)`, so zoom-triggered
//! re-rasterization and cache invalidation come for free.
//!
//!  proved the whole path end-to-end (grid cell -> `CustomGlyph`
//! emission -> font suppression -> [`rasterize`] -> glyphon atlas -> composite
//! at the cell) with one placeholder codepoint;  grew [`rasterize`]
//! to the orthogonal majority;  added rounded corners + diagonals
//! via [`crate::boxpaint::paint`] — [`is_sprite_glyph`] now covers the whole
//! Box Drawing block (U+2500..=U+257F). Anything else (Block Elements and
//! beyond) still isn't a box-drawing codepoint at all, so it shapes normally
//! — never a regression.
//!
//! Verified headless (see `examples/sprite_smoke.rs`) and via a real `LocalPty`
//! shell (`ember-term --screenshot --run 'printf ...'`).
//!
//!  seam-suite finding: glyphon positions each `CustomGlyph`
//! independently (rounds to a whole physical pixel *per glyph*), so a naive
//! `col * cell_w` position with a fixed `cell_w` width let adjacent cells'
//! rounded edges drift apart by up to 1px — see [`snap_to_physical`]'s doc
//! for the fix (independently-snapped edges, width derived as their
//! difference, not a rasterizer gap — `boxpaint::paint`'s SEAM property
//! still holds per-glyph).

use ember_core::Attrs;
use glyphon::{
    Color, CustomGlyph, CustomGlyphId, RasterizeCustomGlyphRequest, RasterizedCustomGlyph,
};

use crate::boxdraw::box_glyph;
use crate::grid_model::GridModel;
use crate::paint::dim_rgb;

/// `CustomGlyphId` is `u16`; the Box Drawing block (U+2500..=U+257F) only
/// needs its low 9 bits, so bit 15 is free to fold in SGR 1 (bold) — a bold
/// box-drawing cell must rasterize *differently* (thicker strokes), so it
/// needs its own glyphon cache entry, not just a different composite color.
const BOLD_BIT: u32 = 0x8000;

/// The id glyphon caches by, encoding both the codepoint and whether the
/// cell is bold (see [`BOLD_BIT`]).
fn glyph_id(c: char, bold: bool) -> CustomGlyphId {
    let id = c as u32 | if bold { BOLD_BIT } else { 0 };
    id as CustomGlyphId
}

/// Whether `c` is drawn via the sprite path rather than the font. Used both
/// to suppress it in shaped text ([`GridModel::row_runs`]) and to decide what
/// [`row_custom_glyphs`] emits: every codepoint [`crate::boxdraw::box_glyph`]
/// maps (never a regression — anything it doesn't map simply shapes as text).
pub fn is_sprite_glyph(c: char) -> bool {
    box_glyph(c).is_some()
}

/// Rasterize a sprite-path codepoint into an alpha-coverage mask, for
/// glyphon's `rasterize_custom_glyph` callback. Returns `None` for anything
/// [`is_sprite_glyph`] doesn't claim — unreachable in practice, since nothing
/// else emits a `CustomGlyph`.
pub fn rasterize(request: RasterizeCustomGlyphRequest) -> Option<RasterizedCustomGlyph> {
    let id = request.id as u32;
    let bold = id & BOLD_BIT != 0;
    let c = char::from_u32(id & !BOLD_BIT)?;
    let glyph = box_glyph(c)?;
    let (w, h) = (request.width, request.height);
    if w == 0 || h == 0 {
        return None;
    }
    let canvas = crate::boxpaint::paint(&glyph, w, h, bold);
    Some(RasterizedCustomGlyph {
        data: canvas.into_data(),
        content_type: glyphon::ContentType::Mask,
    })
}

/// Snap a cell-grid coordinate (`unit * cell_size`) to the nearest physical
/// pixel at the given `scale`, in logical px — i.e. `round(unit * cell_size *
/// scale) / scale`.
///
///  seam-suite finding: glyphon positions each `CustomGlyph`
/// independently (`snap_to_physical_pixel` rounds to a whole physical pixel
/// *per glyph*), so a naive `col as f32 * cell_w` for `left` and a fixed
/// `cell_w` for `width` lets adjacent cells' rounded edges drift apart by up
/// to 1px — visible at the default font size on a 2x display (12pt cell
/// width lands ~0.45px off whole-pixel, right at the worst case). The fix:
/// snap *both* edges of a cell to the true cumulative position (`col` and
/// `col + 1`) and derive the width as their difference, so the rasterized
/// glyph's size always exactly matches the gap to its snapped neighbor —
/// zero drift (each edge tracks the true `col * cell_w`, not an accumulated
/// per-cell delta) and zero seam gap/overlap (width is defined as the
/// distance to the next cell's own snapped edge, not a fixed constant).
fn snap_to_physical(unit: f32, cell_size: f32, scale: f32) -> f32 {
    (unit * cell_size * scale).round() / scale
}

/// `CustomGlyph`s for one row's sprite-path cells, positioned pane-relative
/// in logical px (`left`/`top` are added to the pane `TextArea`'s own
/// `left`/`top`, then both scaled by `scale` — see glyphon's
/// `text_render.rs`); `scale` here must be that same `TextArea::scale` for
/// [`snap_to_physical`]'s rounding to land on the same physical pixels.
/// Respects SGR 1 (bold, via [`BOLD_BIT`] on the glyph id) and SGR 2 (dim,
/// via [`dim_rgb`] on the color) — the same two attrs the text path
/// (`paint::shape_grid`) respects.
pub fn row_custom_glyphs(
    grid: &GridModel,
    row: u16,
    cell_w: f32,
    cell_h: f32,
    scale: f32,
) -> Vec<CustomGlyph> {
    let top = snap_to_physical(row as f32, cell_h, scale);
    let height = snap_to_physical(row as f32 + 1.0, cell_h, scale) - top;
    grid.sprite_glyphs(row)
        .into_iter()
        .map(|(col, c, fg, attrs)| {
            let fg = if attrs.contains(Attrs::DIM) {
                dim_rgb(fg)
            } else {
                fg
            };
            let left = snap_to_physical(col as f32, cell_w, scale);
            let width = snap_to_physical(col as f32 + 1.0, cell_w, scale) - left;
            CustomGlyph {
                id: glyph_id(c, attrs.contains(Attrs::BOLD)),
                left,
                top,
                width,
                height,
                color: Some(Color::rgb(fg.r, fg.g, fg.b)),
                snap_to_physical_pixel: true,
                metadata: 0,
            }
        })
        .collect()
}

/// All sprite-path `CustomGlyph`s for a pane's visible rows.
pub fn pane_custom_glyphs(
    grid: &GridModel,
    cell_w: f32,
    cell_h: f32,
    scale: f32,
) -> Vec<CustomGlyph> {
    (0..grid.dims.screen_lines)
        .flat_map(|row| row_custom_glyphs(grid, row, cell_w, cell_h, scale))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ember_core::{
        CellContent, CellPatch, GridDelta, GridDims, NeutralCell, Rgb, Style, StyleId,
    };

    fn grid_with(dims: GridDims, cells: Vec<CellPatch>) -> GridModel {
        let mut g = GridModel::new(dims);
        g.apply(GridDelta {
            epoch: 1,
            dims,
            reset: true,
            cells,
            new_styles: vec![(
                StyleId(0),
                Style {
                    fg: Rgb::new(200, 200, 200),
                    ..Default::default()
                },
            )],
            ..Default::default()
        });
        g
    }

    fn req(id: CustomGlyphId, width: u16, height: u16) -> RasterizeCustomGlyphRequest {
        RasterizeCustomGlyphRequest {
            id,
            width,
            height,
            x_bin: glyphon::cosmic_text::SubpixelBin::Zero,
            y_bin: glyphon::cosmic_text::SubpixelBin::Zero,
            scale: 1.0,
        }
    }

    #[test]
    fn rasterize_handles_every_box_drawing_shape() {
        for c in ['─', '┏', '┄', '═', '╋', '╭', '╱', '╳'] {
            let out = rasterize(req(glyph_id(c, false), 16, 24))
                .unwrap_or_else(|| panic!("U+{:04X} should rasterize", c as u32));
            assert_eq!(out.data.len(), 16 * 24);
            assert_eq!(out.content_type, glyphon::ContentType::Mask);
        }
    }

    #[test]
    fn rasterize_declines_non_box_ids() {
        assert!(rasterize(req(glyph_id('a', false), 16, 24)).is_none());
    }

    #[test]
    fn bold_bit_rasterizes_a_thicker_glyph_than_plain() {
        let plain = rasterize(req(glyph_id('─', false), 16, 24)).unwrap();
        let bold = rasterize(req(glyph_id('─', true), 16, 24)).unwrap();
        // Total ink (sum of coverage, including AA fringes) rather than a
        // count of fully-covered pixels — at some cell sizes a modest
        // thickness bump doesn't cross another whole-pixel boundary, but the
        // AA edges still carry strictly more coverage.
        let total_ink = |data: &[u8]| data.iter().map(|&b| b as u64).sum::<u64>();
        assert!(total_ink(&bold.data) > total_ink(&plain.data));
    }

    #[test]
    fn pane_custom_glyphs_positions_one_per_sprite_cell() {
        let dims = GridDims::new(5, 2);
        let g = grid_with(
            dims,
            vec![CellPatch {
                row: 1,
                col: 2,
                cell: NeutralCell::new(CellContent::Char('┏'), StyleId(0)),
            }],
        );
        let glyphs = pane_custom_glyphs(&g, 10.0, 20.0, 1.0);
        assert_eq!(glyphs.len(), 1);
        let glyph = glyphs[0];
        assert_eq!(glyph.id, glyph_id('┏', false));
        assert_eq!((glyph.left, glyph.top), (20.0, 20.0));
        assert_eq!((glyph.width, glyph.height), (10.0, 20.0));
        assert!(glyph.snap_to_physical_pixel);
    }

    ///  seam suite: at a cell size/scale combo picked to land near
    /// the worst-case 0.5px rounding fraction (12pt-cell-like `cw=7.2246`,
    /// `scale=2.0` — the actual default-font-size-on-Retina numbers that
    /// exposed this), a long run of adjacent sprite cells must tile with no
    /// gap or overlap: each cell's `left` must equal the previous cell's
    /// `left + width` exactly (in physical px, post-scale), all the way
    /// across a wide row — not just the first pair.
    #[test]
    fn adjacent_sprite_cells_tile_with_no_gap_across_a_wide_row() {
        let cols = 80u16;
        let dims = GridDims::new(cols, 1);
        let cells = (0..cols)
            .map(|col| CellPatch {
                row: 0,
                col,
                cell: NeutralCell::new(CellContent::Char('─'), StyleId(0)),
            })
            .collect();
        let g = grid_with(dims, cells);
        let cell_w = 7.224_6;
        let scale = 2.0;
        let glyphs = pane_custom_glyphs(&g, cell_w, 15.0, scale);
        assert_eq!(glyphs.len(), cols as usize);
        for pair in glyphs.windows(2) {
            let (prev, next) = (pair[0], pair[1]);
            let prev_right_physical = (prev.left + prev.width) * scale;
            let next_left_physical = next.left * scale;
            assert!(
                (prev_right_physical - next_left_physical).abs() < 1e-3,
                "gap/overlap between columns: prev right={prev_right_physical} next left={next_left_physical}"
            );
        }
        // No drift from the true grid over the whole row: the last cell's
        // right edge should be within half a physical pixel of `cols * cw`.
        let last = glyphs.last().unwrap();
        let true_right_physical = cols as f32 * cell_w * scale;
        let actual_right_physical = (last.left + last.width) * scale;
        assert!((actual_right_physical - true_right_physical).abs() <= 0.5 + 1e-3);
    }

    #[test]
    fn bold_and_dim_attrs_are_respected_on_custom_glyphs() {
        let dims = GridDims::new(5, 1);
        let mut g = GridModel::new(dims);
        g.apply(GridDelta {
            epoch: 1,
            dims,
            reset: true,
            cells: vec![
                CellPatch {
                    row: 0,
                    col: 0,
                    cell: NeutralCell::new(CellContent::Char('─'), StyleId(1)),
                },
                CellPatch {
                    row: 0,
                    col: 1,
                    cell: NeutralCell::new(CellContent::Char('─'), StyleId(2)),
                },
            ],
            new_styles: vec![
                (
                    StyleId(1),
                    Style {
                        fg: Rgb::new(90, 90, 90),
                        attrs: ember_core::Attrs::BOLD,
                        ..Default::default()
                    },
                ),
                (
                    StyleId(2),
                    Style {
                        fg: Rgb::new(90, 90, 90),
                        attrs: ember_core::Attrs::DIM,
                        ..Default::default()
                    },
                ),
            ],
            ..Default::default()
        });
        let glyphs = pane_custom_glyphs(&g, 10.0, 20.0, 1.0);
        assert_eq!(glyphs.len(), 2);
        // Bold: distinct (bit-set) id, color unchanged.
        assert_eq!(glyphs[0].id, glyph_id('─', true));
        assert_eq!(glyphs[0].color, Some(Color::rgb(90, 90, 90)));
        // Dim: plain id, color scaled toward the background.
        assert_eq!(glyphs[1].id, glyph_id('─', false));
        assert_eq!(glyphs[1].color, Some(Color::rgb(60, 60, 60)));
    }

    #[test]
    fn rounded_and_diagonal_glyphs_are_emitted_too() {
        let dims = GridDims::new(5, 1);
        for c in ['╭', '╱'] {
            let g = grid_with(
                dims,
                vec![CellPatch {
                    row: 0,
                    col: 0,
                    cell: NeutralCell::new(CellContent::Char(c), StyleId(0)),
                }],
            );
            let glyphs = pane_custom_glyphs(&g, 10.0, 20.0, 1.0);
            assert_eq!(
                glyphs.len(),
                1,
                "U+{:04X} should emit a CustomGlyph",
                c as u32
            );
            assert_eq!(glyphs[0].id, glyph_id(c, false));
        }
    }

    #[test]
    fn plain_text_glyphs_are_not_emitted() {
        let dims = GridDims::new(5, 1);
        let g = grid_with(
            dims,
            vec![CellPatch {
                row: 0,
                col: 0,
                cell: NeutralCell::new(CellContent::Char('a'), StyleId(0)),
            }],
        );
        assert!(pane_custom_glyphs(&g, 10.0, 20.0, 1.0).is_empty());
    }
}
