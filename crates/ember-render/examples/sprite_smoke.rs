//! Manual smoke check for : renders a pane whose first row is all
//! U+2500 (the wired placeholder sprite glyph) to a PNG via the headless path,
//! so the sprite-rasterizer -> glyphon-atlas -> composite path can be
//! eyeballed. Useful again once /2.5 wire more of the table in.
//!
//!   cargo run -q --example sprite_smoke -- /tmp/sprite_smoke.png

use ember_core::{CellContent, CellPatch, GridDelta, GridDims, NeutralCell, Rgb, Style, StyleId};
use ember_render::headless::{PaneShot, Shot, capture};
use ember_render::{BackdropParams, GridModel, ImageFit};

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/sprite_smoke.png".to_string());

    let dims = GridDims::new(10, 4);
    let mut grid = GridModel::new(dims);
    let mut cells = Vec::new();
    // Row 0: a full row of the placeholder glyph (should render as a solid
    // horizontal line across the row, like a `─` divider).
    for col in 0..dims.columns {
        cells.push(CellPatch {
            row: 0,
            col,
            cell: NeutralCell::new(CellContent::Char('\u{2500}'), StyleId(1)),
        });
    }
    // Row 2: plain text, to confirm the font path still shapes normally
    // alongside the sprite path.
    for (col, ch) in "hello!".chars().enumerate() {
        cells.push(CellPatch {
            row: 2,
            col: col as u16,
            cell: NeutralCell::new(CellContent::Char(ch), StyleId(1)),
        });
    }
    grid.apply(GridDelta {
        epoch: 1,
        dims,
        reset: true,
        cells,
        new_styles: vec![(
            StyleId(1),
            Style {
                fg: Rgb::new(0xff, 0x9d, 0x3c),
                ..Default::default()
            },
        )],
        ..Default::default()
    });

    let (cw, ch) = ember_render::headless::cell_metrics();
    let logical_w = cw * dims.columns as f32 + 20.0;
    let logical_h = ch * dims.screen_lines as f32 + 40.0;

    let shot = Shot {
        logical_w,
        logical_h,
        scale: 2.0,
        panes: vec![PaneShot {
            grid: &grid,
            rect: ember_core::Rect {
                x: 0.0,
                y: 30.0,
                width: logical_w as f64,
                height: (logical_h - 30.0) as f64,
            },
            focused: true,
            selection: None,
            split_preview: None,
        }],
        tabs: vec![],
        tab_drag: None,
        hovered_tab: None,
        help: None,
        help_title: None,
        about: None,
        settings: None,
        backdrop: BackdropParams::default(),
        image: None,
        image_fit: ImageFit::Cover,
        fps_overlay: None,
        bell_flash: 0.0,
        font_size: 12.0,
        font_family: None,
        confirm: None,
        hold_ring: None,
    };

    capture(&shot, std::path::Path::new(&path)).expect("capture");
    println!("wrote {path}");
}
