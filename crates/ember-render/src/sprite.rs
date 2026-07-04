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
//! shell (`ember-term --screenshot --run 'printf ...'`). One known seam: each
//! cell is a separate `CustomGlyph` snapped to physical pixels independently,
//! so a run of adjacent glyphs can show a hairline gap when `cell_w * scale`
//! isn't a whole number of physical px — a compositing rounding artifact, not
//! a rasterizer gap (see `boxpaint::paint`'s SEAM property doc).

use glyphon::{
    Color, CustomGlyph, CustomGlyphId, RasterizeCustomGlyphRequest, RasterizedCustomGlyph,
};

use crate::boxdraw::box_glyph;
use crate::grid_model::GridModel;

/// `CustomGlyphId` is `u16`; the whole Box Drawing block (U+2500..=U+257F)
/// fits, so the id IS the codepoint — no separate registry to keep in sync.
fn glyph_id(c: char) -> CustomGlyphId {
    c as u32 as CustomGlyphId
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
    let c = char::from_u32(request.id as u32)?;
    let glyph = box_glyph(c)?;
    let (w, h) = (request.width, request.height);
    if w == 0 || h == 0 {
        return None;
    }
    let canvas = crate::boxpaint::paint(&glyph, w, h);
    Some(RasterizedCustomGlyph {
        data: canvas.into_data(),
        content_type: glyphon::ContentType::Mask,
    })
}

/// `CustomGlyph`s for one row's sprite-path cells, positioned pane-relative
/// in logical px (`left`/`top` are added to the pane `TextArea`'s own
/// `left`/`top`, then both scaled by `scale` — see glyphon's
/// `text_render.rs`). Monospace layout means a cell's position is just
/// `(col * cell_w, row * cell_h)`; no shaping lookup needed.
pub fn row_custom_glyphs(grid: &GridModel, row: u16, cell_w: f32, cell_h: f32) -> Vec<CustomGlyph> {
    grid.sprite_glyphs(row)
        .into_iter()
        .map(|(col, c, fg)| CustomGlyph {
            id: glyph_id(c),
            left: col as f32 * cell_w,
            top: row as f32 * cell_h,
            width: cell_w,
            height: cell_h,
            color: Some(Color::rgb(fg.r, fg.g, fg.b)),
            snap_to_physical_pixel: true,
            metadata: 0,
        })
        .collect()
}

/// All sprite-path `CustomGlyph`s for a pane's visible rows.
pub fn pane_custom_glyphs(grid: &GridModel, cell_w: f32, cell_h: f32) -> Vec<CustomGlyph> {
    (0..grid.dims.screen_lines)
        .flat_map(|row| row_custom_glyphs(grid, row, cell_w, cell_h))
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
            let out = rasterize(req(glyph_id(c), 16, 24))
                .unwrap_or_else(|| panic!("U+{:04X} should rasterize", c as u32));
            assert_eq!(out.data.len(), 16 * 24);
            assert_eq!(out.content_type, glyphon::ContentType::Mask);
        }
    }

    #[test]
    fn rasterize_declines_non_box_ids() {
        assert!(rasterize(req(glyph_id('a'), 16, 24)).is_none());
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
        let glyphs = pane_custom_glyphs(&g, 10.0, 20.0);
        assert_eq!(glyphs.len(), 1);
        let glyph = glyphs[0];
        assert_eq!(glyph.id, glyph_id('┏'));
        assert_eq!((glyph.left, glyph.top), (20.0, 20.0));
        assert_eq!((glyph.width, glyph.height), (10.0, 20.0));
        assert!(glyph.snap_to_physical_pixel);
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
            let glyphs = pane_custom_glyphs(&g, 10.0, 20.0);
            assert_eq!(
                glyphs.len(),
                1,
                "U+{:04X} should emit a CustomGlyph",
                c as u32
            );
            assert_eq!(glyphs[0].id, glyph_id(c));
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
        assert!(pane_custom_glyphs(&g, 10.0, 20.0).is_empty());
    }
}
