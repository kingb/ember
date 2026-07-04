//! Wires the box-drawing "sprite font" into glyphon's custom-glyph path
//! ( — the foundation /2.5 build on). Per the 
//! ADR, we use glyphon's `prepare_with_custom`/`CustomGlyph` API rather than a
//! bespoke atlas: glyphon owns the GPU atlas and caches rasterized glyphs by
//! `(id, physical size, subpixel bin)`, so zoom-triggered re-rasterization and
//! cache invalidation come for free.
//!
//! Exactly ONE codepoint is wired end-to-end here — U+2500 `─` — to prove the
//! whole path (grid cell -> `CustomGlyph` emission -> font suppression ->
//! [`rasterize`] -> glyphon atlas -> composite at the cell) before
//!  extends the rasterizer to the rest of the Box Drawing block via
//! [`crate::boxdraw::box_glyph`].
//!
//! Verified headless (see `examples/sprite_smoke.rs`): a row of the
//! placeholder glyph composites as a horizontal bar at the right row, in the
//! cell's fg color, alongside ordinary shaped text on other rows. One known
//! seam: each cell is a separate `CustomGlyph` snapped to physical pixels
//! independently, so a run of adjacent glyphs can show a hairline gap between
//! cells when `cell_w * scale` isn't a whole number of physical px. Real
//! arm-to-arm tiling (and the junction/hollow-center logic for double lines)
//! is /2.5's job, not this foundation's.

use glyphon::{
    Color, ContentType, CustomGlyph, CustomGlyphId, RasterizeCustomGlyphRequest,
    RasterizedCustomGlyph,
};

use crate::canvas::Canvas;
use crate::grid_model::GridModel;

/// The one codepoint proven end-to-end by this foundation bead.
pub const PLACEHOLDER_GLYPH: char = '\u{2500}'; // ─ light horizontal

/// `CustomGlyphId` is `u16`; the whole Box Drawing block (U+2500..=U+257F)
/// fits, so the id IS the codepoint — no separate registry to keep in sync.
fn glyph_id(c: char) -> CustomGlyphId {
    c as u32 as CustomGlyphId
}

/// Whether `c` is drawn via the sprite path rather than the font. Used both
/// to suppress it in shaped text ([`GridModel::row_runs`]) and to decide what
/// [`row_custom_glyphs`] emits. Only [`PLACEHOLDER_GLYPH`] today; 
/// grows this to `boxdraw::box_glyph(c).is_some()`.
pub fn is_sprite_glyph(c: char) -> bool {
    c == PLACEHOLDER_GLYPH
}

/// Rasterize a sprite-path codepoint into an alpha-coverage mask, for
/// glyphon's `rasterize_custom_glyph` callback. Returns `None` for anything
/// but [`PLACEHOLDER_GLYPH`] — unreachable today since nothing else emits a
/// `CustomGlyph` yet.
pub fn rasterize(request: RasterizeCustomGlyphRequest) -> Option<RasterizedCustomGlyph> {
    if request.id != glyph_id(PLACEHOLDER_GLYPH) {
        return None;
    }
    let (w, h) = (request.width, request.height);
    if w == 0 || h == 0 {
        return None;
    }
    let mut canvas = Canvas::new(w, h);
    // Light horizontal arm: full cell width, centered vertically. The
    // thickness ratio is a placeholder — /2.5 refine per-weight
    // metrics (see boxdraw.rs's module notes on heavy-stroke width).
    let thickness = (h as f32 / 8.0).max(1.0);
    canvas.stroke_orthogonal(true, h as f32 / 2.0, 0.0, w as f32, thickness);
    Some(RasterizedCustomGlyph {
        data: canvas.into_data(),
        content_type: ContentType::Mask,
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
        .map(|(col, fg)| CustomGlyph {
            id: glyph_id(PLACEHOLDER_GLYPH),
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

    #[test]
    fn rasterize_only_handles_the_placeholder_id() {
        let req = RasterizeCustomGlyphRequest {
            id: glyph_id(PLACEHOLDER_GLYPH),
            width: 16,
            height: 24,
            x_bin: glyphon::cosmic_text::SubpixelBin::Zero,
            y_bin: glyphon::cosmic_text::SubpixelBin::Zero,
            scale: 1.0,
        };
        let out = rasterize(req).expect("placeholder glyph rasterizes");
        assert_eq!(out.data.len(), 16 * 24);
        assert_eq!(out.content_type, ContentType::Mask);

        let mut other = req;
        other.id = glyph_id('a');
        assert!(rasterize(other).is_none());
    }

    #[test]
    fn pane_custom_glyphs_positions_one_per_placeholder_cell() {
        let dims = GridDims::new(5, 2);
        let g = grid_with(
            dims,
            vec![CellPatch {
                row: 1,
                col: 2,
                cell: NeutralCell::new(CellContent::Char(PLACEHOLDER_GLYPH), StyleId(0)),
            }],
        );
        let glyphs = pane_custom_glyphs(&g, 10.0, 20.0);
        assert_eq!(glyphs.len(), 1);
        let glyph = glyphs[0];
        assert_eq!(glyph.id, glyph_id(PLACEHOLDER_GLYPH));
        assert_eq!((glyph.left, glyph.top), (20.0, 20.0));
        assert_eq!((glyph.width, glyph.height), (10.0, 20.0));
        assert!(glyph.snap_to_physical_pixel);
    }

    #[test]
    fn non_placeholder_glyphs_are_not_emitted() {
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
