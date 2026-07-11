//! Headless screenshot renderer (debug / self-review;  follow-up).
//!
//! Renders a deterministic scene to an offscreen texture and writes a PNG — no
//! window, no surface, so it runs in an agent's shell or CI (Metal/Vulkan render
//! headless). It reuses the *same* draw helpers as the on-screen [`Renderer`]
//! (`shape_grid` / `grid_quads` / `build_tabs`), so the PNG is what ships,
//! pixel-for-pixel. Pass a `scale` to reproduce a HiDPI (Retina) target.

use std::path::Path;

use ember_core::{Rect, SettingsRowView};
use glyphon::{
    Buffer, Cache, Color, CustomGlyph, Family, FontSystem, Metrics, Resolution, SwashCache,
    TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};
use wgpu::{
    DeviceDescriptor, Instance, InstanceDescriptor, MultisampleState, RequestAdapterOptions,
    TextureFormat,
};

use crate::background::{ImageRenderer, SparkRenderer};
use crate::grid_model::GridModel;
use crate::paint::{
    AboutLayout, bell_wash, build_about, build_confirm, build_fps, build_help,
    build_ime_preedit, build_search_bar, build_settings,
    build_tabs, grid_quads, hold_ring_quads, link_quads, measure_cell_width, morph_quads,
    push_backdrop, scrollbar, selection_quads, shape_grid, spark_quads, split_preview,
};
use crate::quads::{QuadRenderer, srgb_to_linear};
use crate::renderer::{
    ABOUT_TITLE_LINE, ABOUT_TITLE_SIZE, AMBER, AboutInfo, BG, BackdropParams, FG, FONT_SIZE,
    HELP_PAD, ImageFit, LINE_HEIGHT, PAD, TabLabel,
};
use crate::selection::Selection;

/// One pane in a screenshot scene: a grid and the **logical** inner rect it fills.
pub struct PaneShot<'a> {
    pub grid: &'a GridModel,
    pub rect: Rect,
    pub focused: bool,
    /// Text selection to highlight in this pane, if any.
    pub selection: Option<Selection>,
    /// Split preview `(horizontal, ratio, before)` for this pane, if any.
    pub split_preview: Option<(bool, f32, bool)>,
}

/// A full scene to capture: logical window size, HiDPI scale, the panes, and the
/// tab strip (drawn only when more than one tab is present).
pub struct Shot<'a> {
    pub logical_w: f32,
    pub logical_h: f32,
    pub scale: f32,
    pub panes: Vec<PaneShot<'a>>,
    pub tabs: Vec<TabLabel>,
    /// In-progress tab drag `(dragged slot, cursor x logical)`, for the lifted tab.
    pub tab_drag: Option<(usize, f32)>,
    /// Tab the cursor is over (hover highlight + "✕" close affordance), or `None`.
    pub hovered_tab: Option<usize>,
    /// When set, the cheat-sheet overlay is drawn instead of the panes.
    pub help: Option<Vec<(String, String)>>,
    /// Overrides the help panel's `(title, hint)`; `None` → shortcuts default.
    pub help_title: Option<(String, String)>,
    /// When set, the About overlay is drawn, with `(info, glow, elapsed_seconds)`.
    pub about: Option<(AboutInfo, f32, f32)>,
    /// When set, the Settings overlay is drawn: `(resolved rows, selected)`.
    pub settings: Option<(Vec<SettingsRowView>, usize)>,
    /// Campfire backdrop + ember sparks (drawn behind the panes when active).
    pub backdrop: BackdropParams,
    /// A backdrop image as `(rgba8, width, height)`; drawn behind the cells in
    /// place of the gradient when set.
    pub image: Option<(Vec<u8>, u32, u32)>,
    /// How the backdrop image fills the window.
    pub image_fit: ImageFit,
    /// FPS/frame-time debug readout text (bottom-right), or `None`.
    pub fps_overlay: Option<String>,
    /// Scrollback-search bar text (top-right), or `None` when search is closed.
    pub search_bar: Option<String>,
    /// IME composition (preedit) text, drawn at the focused pane's cursor.
    pub ime_preedit: Option<String>,
    /// Visual-bell flash intensity (`0..1`) — a warm amber wash over the panes.
    pub bell_flash: f32,
    /// Terminal font point size (matches the live renderer's current zoom).
    pub font_size: f32,
    /// Font family name (`None` → monospace default).
    pub font_family: Option<String>,
    /// A blocking confirm modal drawn over everything, if shown.
    pub confirm: Option<crate::renderer::ConfirmView>,
    /// Hold-to-wisp ring (v1.1): `(logical x, logical y, progress 0..1)` —
    /// mirrors [`crate::Renderer`]'s live `hold_ring` state so a mid-gesture
    /// `ctl screenshot` shows the sweep for visual verification.
    pub hold_ring: Option<(f32, f32, f32)>,
    /// Incoming-drag ghost tab (v0.4.0): `(label, elapsed seconds)` — mirrors
    /// [`crate::Renderer`]'s live `ghost_tab` state so a mid-hover `ctl
    /// screenshot` on the TARGET window shows it for visual verification.
    pub ghost_tab: Option<(String, f32)>,
    /// Suck-in/pour-out morph (v0.4.0): `(rect, grab point, t01, inward)` —
    /// mirrors [`crate::Renderer`]'s live `morph` state so a mid-gesture
    /// `ctl screenshot` shows the collapsing/expanding rect.
    pub morph: Option<crate::renderer::MorphState>,
}

/// The measured `(cell_width, cell_height)` in logical px — lets a caller derive
/// pane grid dimensions to match what `capture` will draw. CPU-only (no GPU).
pub fn cell_metrics() -> (f32, f32) {
    cell_metrics_for(FONT_SIZE, None)
}

/// Cell `(width, height)` px for a given font size + family — lets the headless
/// caller derive grid dims that match a non-default zoom/font.
pub fn cell_metrics_for(size: f32, family: Option<&str>) -> (f32, f32) {
    let mut font_system = crate::paint::new_font_system();
    let cw = measure_cell_width(&mut font_system, size, crate::paint::family_of(family));
    (cw, crate::paint::line_height_for(size))
}

/// Why a headless [`capture`] (or [`crate::Renderer::capture_to_png`]) failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum CaptureError {
    /// No suitable GPU adapter.
    Adapter(String),
    /// Failed to acquire the GPU device/queue.
    Device(String),
    /// glyphon text-prepare failed.
    TextPrepare(String),
    /// GPU poll / buffer-map failed.
    Map(String),
    /// PNG encoding failed.
    Png(png::EncodingError),
    /// Filesystem IO failed (creating/writing the PNG).
    Io(std::io::Error),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Adapter(e) => write!(f, "request GPU adapter: {e}"),
            Self::Device(e) => write!(f, "request GPU device: {e}"),
            Self::TextPrepare(e) => write!(f, "prepare text: {e}"),
            Self::Map(e) => write!(f, "read back GPU buffer: {e}"),
            Self::Png(e) => write!(f, "encode PNG: {e}"),
            Self::Io(e) => write!(f, "write PNG file: {e}"),
        }
    }
}

impl std::error::Error for CaptureError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Png(e) => Some(e),
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for CaptureError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<png::EncodingError> for CaptureError {
    fn from(e: png::EncodingError) -> Self {
        Self::Png(e)
    }
}

/// Render `shot` to a PNG, building a throwaway GPU device and font system.
/// Used by the standalone `--screenshot` CLI, where the one-time cost is fine.
/// The live control/MCP screenshot path calls [`capture_reusing`] instead, so a
/// repeated screenshot doesn't rebuild the GPU stack or re-scan system fonts
/// (~100ms) every time.
pub fn capture(shot: &Shot, path: &Path) -> Result<(), CaptureError> {
    let (device, queue) = pollster::block_on(async {
        let instance = Instance::new(InstanceDescriptor::new_without_display_handle());
        let adapter = instance
            .request_adapter(&RequestAdapterOptions {
                compatible_surface: None,
                ..Default::default()
            })
            .await
            .map_err(|e| CaptureError::Adapter(format!("{e:?}")))?;
        adapter
            .request_device(&DeviceDescriptor::default())
            .await
            .map_err(|e| CaptureError::Device(format!("{e:?}")))
    })?;
    let mut font_system = crate::paint::new_font_system();
    capture_reusing(&device, &queue, &mut font_system, shot, path)
}

/// Render `shot` reusing an existing device/queue/font system (the live
/// renderer's). Only a per-call offscreen texture + format-specific pipelines
/// are created; the expensive device creation and system-font enumeration are
/// skipped. Synchronous — the only GPU wait is `device.poll(Wait)` on read-back.
pub fn capture_reusing(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    font_system: &mut FontSystem,
    shot: &Shot,
    path: &Path,
) -> Result<(), CaptureError> {
    let sf = shot.scale.max(0.1);
    let phys_w = ((shot.logical_w * sf).ceil() as u32).max(1);
    let phys_h = ((shot.logical_h * sf).ceil() as u32).max(1);

    // sRGB target so the read-back bytes are already gamma-encoded for PNG.
    let format = TextureFormat::Rgba8UnormSrgb;
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("ember-headless"),
        size: wgpu::Extent3d {
            width: phys_w,
            height: phys_h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    let mut swash_cache = SwashCache::new();
    let cache = Cache::new(device);
    let mut viewport = Viewport::new(device, &cache);
    let mut atlas = TextAtlas::new(device, queue, &cache, format);
    let mut text_renderer =
        TextRenderer::new(&mut atlas, device, MultisampleState::default(), None);
    // Second pass for the confirm dialog's text (drawn after its opaque panel),
    // mirroring the windowed renderer so screenshots match on-screen.
    let mut overlay_text = TextRenderer::new(&mut atlas, device, MultisampleState::default(), None);
    let mut quads = QuadRenderer::new(device, format);
    let mut sparks = SparkRenderer::new(device, format);
    let mut image = ImageRenderer::new(device, format);
    let mut draw_image = false;
    let font_size = shot.font_size.clamp(6.0, 48.0);
    let line_height = crate::paint::line_height_for(font_size);
    let font_family = crate::paint::family_of(shot.font_family.as_deref());
    let cw = measure_cell_width(font_system, font_size, font_family);
    // The Settings panel's own text always shapes at the fixed FONT_SIZE/
    // Monospace, never the shot's (terminal) font — reusing `cw` here made
    // the panel's column alignment wildly wrong whenever the terminal font
    // size differed from FONT_SIZE (see the matching fix in renderer.rs).
    let settings_cw = measure_cell_width(font_system, FONT_SIZE, Family::Monospace);

    let full_bounds = TextBounds {
        left: 0,
        top: 0,
        right: phys_w as i32,
        bottom: phys_h as i32,
    };
    let mut buffers: Vec<Buffer> = Vec::new();
    let mut help_buf = Buffer::new(font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
    let mut chrome = Buffer::new(font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
    let mut close_buf = Buffer::new(font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
    let mut about_title = Buffer::new(
        font_system,
        Metrics::new(ABOUT_TITLE_SIZE, ABOUT_TITLE_LINE),
    );
    let mut about_body = Buffer::new(font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
    let mut settings_buf = Buffer::new(font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
    let mut fps_buf = Buffer::new(font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
    let mut cf_title = Buffer::new(font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
    let mut cf_msg = Buffer::new(font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
    let mut cf_cancel = Buffer::new(font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
    let mut cf_ok = Buffer::new(font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
    let mut confirm_layout: Option<crate::paint::ConfirmLayout> = None;
    let mut rects: Vec<([f32; 4], [f32; 4])> = Vec::new();
    let mut rounded: Vec<([f32; 4], [f32; 4], f32)> = Vec::new();
    let mut spark_rects: Vec<([f32; 4], [f32; 4])> = Vec::new();
    // Where the additive spark pass is interleaved (see the live renderer):
    // backdrop before, cells + chrome after, so content covers the embers.
    let mut spark_layer: usize = 0;
    let mut help_panel: Option<Rect> = None;
    let mut about_layout: Option<AboutLayout> = None;
    let mut settings_origin: Option<(f32, f32)> = None;
    let mut fps_origin: Option<(f32, f32)> = None;
    let mut search_buf = Buffer::new(font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
    let mut search_origin: Option<(f32, f32)> = None;
    let mut preedit_buf = Buffer::new(font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
    let mut preedit_origin: Option<(f32, f32)> = None;
    // Center-x of the hovered tab's "✕" (pill left cap), when a tab is hovered.
    let mut close_cx: Option<f32> = None;

    if let Some((rows, sel)) = &shot.settings {
        settings_origin = Some(build_settings(
            font_system,
            &mut settings_buf,
            rows,
            *sel,
            settings_cw,
            shot.logical_w,
            shot.logical_h,
            sf,
            true, // one-shot capture: always shape
            &mut rects,
        ));
    } else if let Some((info, glow, t)) = &shot.about {
        // About overlay replaces the panes (same helper as on-screen).
        about_layout = Some(build_about(
            font_system,
            &mut about_title,
            &mut about_body,
            info,
            *glow,
            *t,
            cw,
            shot.logical_w,
            shot.logical_h,
            sf,
            &mut rects,
        ));
    } else if let Some(lines) = &shot.help {
        // Cheat-sheet overlay replaces the panes (same helper as on-screen).
        let (htitle, hhint) = shot
            .help_title
            .clone()
            .unwrap_or_else(|| ("Keyboard Shortcuts".into(), "any key to close".into()));
        help_panel = Some(build_help(
            font_system,
            &mut help_buf,
            &htitle,
            &hhint,
            lines,
            shot.logical_w,
            shot.logical_h,
            sf,
            &mut rects,
        ));
    } else {
        // Campfire backdrop (image or gradient, + scrim) behind the cells, then
        // sparks. A backdrop image is the base layer drawn in the render pass; it
        // replaces the gradient (scrim still applies).
        if let Some((rgba, w, h)) = &shot.image {
            image.set_image(device, queue, rgba, *w, *h);
            image.prepare(
                device,
                queue,
                (phys_w as f32, phys_h as f32),
                shot.image_fit,
            );
            draw_image = true;
        }
        let mut bp = shot.backdrop;
        if draw_image {
            bp.gradient = false;
        }
        push_backdrop(&mut rects, &bp, shot.logical_w, shot.logical_h, sf);
        spark_layer = rects.len();
        if shot.backdrop.sparks {
            spark_rects = spark_quads(
                shot.backdrop.density,
                shot.backdrop.time,
                shot.logical_w,
                shot.logical_h,
                sf,
                shot.backdrop.frame_dt,
            );
        }
        // Shape each pane into its own logical-sized buffer, then build quads.
        for pane in &shot.panes {
            let mut buffer = Buffer::new(font_system, Metrics::new(font_size, line_height));
            buffer.set_size(
                font_system,
                Some(pane.rect.width as f32),
                Some(pane.rect.height as f32),
            );
            shape_grid(
                font_system,
                &mut buffer,
                pane.grid,
                font_size,
                line_height,
                cw,
                font_family,
            );
            buffers.push(buffer);
        }
        let split = shot.panes.len() > 1;
        for pane in &shot.panes {
            grid_quads(
                pane.grid,
                pane.rect,
                cw,
                line_height,
                sf,
                pane.focused,
                split,
                &mut rects,
            );
            // Link underlines: headless capture has no hover state (no live cursor
            // to hit-test), so always pass `None` — mirrors the live renderer's
            // `link_quads` call (renderer.rs) but without a hovered link.
            let link_spans = pane.grid.link_spans();
            link_quads(
                &link_spans,
                None,
                (pane.rect.x as f32, pane.rect.y as f32),
                cw,
                line_height,
                sf,
                &mut rects,
            );
            if let Some(sel) = &pane.selection {
                selection_quads(pane.grid, sel, pane.rect, cw, line_height, sf, &mut rects);
            }
            if let Some((horizontal, ratio, before)) = pane.split_preview {
                split_preview(pane.rect, horizontal, before, ratio, sf, &mut rects);
            }
            if !pane.grid.alt_screen {
                scrollbar(
                    pane.grid.display_offset,
                    pane.grid.history_len,
                    pane.grid.dims.screen_lines,
                    pane.rect,
                    sf,
                    &mut rects,
                );
            }
        }
        // Hold-to-wisp ring: window-space, not tied to any one pane, so it's
        // drawn once here (mirrors the live renderer's placement — after
        // every pane's own content, before the tab strip).
        if let Some((rx, ry, progress)) = shot.hold_ring {
            hold_ring_quads(rx, ry, progress, sf)
                .into_iter()
                .for_each(|q| rects.push(q));
        }
        // Suck-in/pour-out morph (v0.4.0): same window-space placement as the
        // hold ring, for the same reason (not tied to any one pane).
        if let Some((rect, grab, t01, inward)) = shot.morph {
            morph_quads(rect, grab, t01, inward, sf)
                .into_iter()
                .for_each(|q| rects.push(q));
        }
        // One-shot capture: a fresh cache (always shapes once) keeps the shared
        // signature without threading state through the headless path.
        let mut tabs_cache = crate::paint::TabsCache::default();
        let ghost = shot.ghost_tab.as_ref().map(|(l, t)| (l.as_str(), *t));
        close_cx = build_tabs(
            font_system,
            &mut chrome,
            &mut close_buf,
            &mut tabs_cache,
            &shot.tabs,
            shot.tab_drag,
            ghost,
            shot.hovered_tab,
            cw,
            shot.logical_w,
            sf,
            &mut rects,
            &mut rounded,
        );
        if let Some(text) = &shot.fps_overlay {
            fps_origin = Some(build_fps(
                font_system,
                &mut fps_buf,
                text,
                cw,
                shot.logical_w,
                shot.logical_h,
                sf,
                &mut rects,
            ));
        }
        if let Some(text) = &shot.ime_preedit {
            if let Some(pane) = shot.panes.iter().find(|p| p.focused) {
                let cur = pane.grid.cursor;
                let px = pane.rect.x as f32 + cur.col as f32 * cw;
                let py = pane.rect.y as f32 + cur.row as f32 * line_height;
                preedit_origin = Some(build_ime_preedit(
                    font_system,
                    &mut preedit_buf,
                    text,
                    px,
                    py,
                    cw,
                    line_height,
                    shot.logical_w,
                    sf,
                    &mut rects,
                ));
            }
        }
        if let Some(text) = &shot.search_bar {
            search_origin = Some(build_search_bar(
                font_system,
                &mut search_buf,
                text,
                cw,
                shot.logical_w,
                sf,
                &mut rects,
            ));
        }
        bell_wash(
            &mut rects,
            shot.bell_flash,
            shot.logical_w,
            shot.logical_h,
            sf,
        );
    }
    // Boundary between base rounded quads (tab pills) and the confirm modal's
    // scrim+panel+buttons, appended next — so the panel draws after pane text.
    let rounded_pre_confirm = rounded.len() as u32;
    if let Some(view) = &shot.confirm {
        confirm_layout = Some(build_confirm(
            font_system,
            &mut cf_title,
            &mut cf_msg,
            &mut cf_cancel,
            &mut cf_ok,
            view,
            cw,
            shot.logical_w,
            shot.logical_h,
            sf,
            &mut rounded,
        ));
    }
    quads.prepare(
        device,
        queue,
        (phys_w as f32, phys_h as f32),
        &rects,
        &rounded,
    );
    sparks.prepare(device, queue, (phys_w as f32, phys_h as f32), &spark_rects);

    viewport.update(
        queue,
        Resolution {
            width: phys_w,
            height: phys_h,
        },
    );

    let mut areas: Vec<TextArea> = Vec::new();
    // Sprite-path `CustomGlyph`s per pane, in `shot.panes` order —
    // declared out here so it outlives the `areas` that borrow from it.
    let pane_customs: Vec<Vec<CustomGlyph>>;
    if let Some((left, top)) = settings_origin {
        areas.push(TextArea {
            buffer: &settings_buf,
            left: left * sf,
            top: top * sf,
            scale: sf,
            bounds: full_bounds,
            default_color: Color::rgb(FG.r, FG.g, FG.b),
            custom_glyphs: &[],
        });
    } else if let Some(layout) = &about_layout {
        areas.push(TextArea {
            buffer: &about_title,
            left: layout.title_left * sf,
            top: layout.title_top * sf,
            scale: sf,
            bounds: full_bounds,
            default_color: Color::rgb(AMBER.r, AMBER.g, AMBER.b),
            custom_glyphs: &[],
        });
        areas.push(TextArea {
            buffer: &about_body,
            left: 0.0,
            top: layout.body_top * sf,
            scale: sf,
            bounds: full_bounds,
            default_color: Color::rgb(FG.r, FG.g, FG.b),
            custom_glyphs: &[],
        });
    } else if let Some(panel) = help_panel {
        areas.push(TextArea {
            buffer: &help_buf,
            left: (panel.x as f32 + HELP_PAD) * sf,
            top: (panel.y as f32 + HELP_PAD) * sf,
            scale: sf,
            bounds: full_bounds,
            default_color: Color::rgb(FG.r, FG.g, FG.b),
            custom_glyphs: &[],
        });
    } else {
        // Sprite-path glyphs ride alongside the shaped text as
        // `CustomGlyph`s — mirrors the windowed renderer's Pass 3 exactly, so
        // the PNG matches on-screen pixel-for-pixel.
        pane_customs = shot
            .panes
            .iter()
            .map(|pane| crate::sprite::pane_custom_glyphs(pane.grid, cw, line_height, sf))
            .collect();
        for ((pane, buffer), customs) in shot
            .panes
            .iter()
            .zip(buffers.iter())
            .zip(pane_customs.iter())
        {
            areas.push(TextArea {
                buffer,
                left: pane.rect.x as f32 * sf,
                top: pane.rect.y as f32 * sf,
                scale: sf,
                bounds: TextBounds {
                    left: (pane.rect.x as f32 * sf) as i32,
                    top: (pane.rect.y as f32 * sf) as i32,
                    right: ((pane.rect.x + pane.rect.width) as f32 * sf) as i32,
                    bottom: ((pane.rect.y + pane.rect.height) as f32 * sf) as i32,
                },
                default_color: Color::rgb(FG.r, FG.g, FG.b),
                custom_glyphs: customs,
            });
        }
        // The strip (with +/?/⚙ controls) is always drawn, so always show its text.
        areas.push(TextArea {
            buffer: &chrome,
            left: 0.0,
            top: PAD * sf,
            scale: sf,
            bounds: full_bounds,
            default_color: Color::rgb(FG.r, FG.g, FG.b),
            custom_glyphs: &[],
        });
        // The hovered tab's "✕", pixel-centered in the pill's left cap (matches
        // the windowed renderer).
        if let Some(cx) = close_cx {
            areas.push(TextArea {
                buffer: &close_buf,
                left: (cx - cw * 0.5) * sf,
                top: PAD * sf,
                scale: sf,
                bounds: full_bounds,
                default_color: Color::rgb(0xcc, 0xcc, 0xcc),
                custom_glyphs: &[],
            });
        }
        if let Some((left, top)) = fps_origin {
            areas.push(TextArea {
                buffer: &fps_buf,
                left: left * sf,
                top: top * sf,
                scale: sf,
                bounds: full_bounds,
                default_color: Color::rgb(AMBER.r, AMBER.g, AMBER.b),
                custom_glyphs: &[],
            });
        }
        if let Some((left, top)) = preedit_origin {
            areas.push(TextArea {
                buffer: &preedit_buf,
                left: left * sf,
                top: top * sf,
                scale: sf,
                bounds: full_bounds,
                default_color: Color::rgb(0xf5, 0xf5, 0xdc),
                custom_glyphs: &[],
            });
        }
        if let Some((left, top)) = search_origin {
            areas.push(TextArea {
                buffer: &search_buf,
                left: left * sf,
                top: top * sf,
                scale: sf,
                bounds: full_bounds,
                default_color: Color::rgb(0xf5, 0xf5, 0xdc),
                custom_glyphs: &[],
            });
        }
    }
    // Confirm dialog text goes to the overlay pass (drawn after its opaque panel).
    let mut overlay_areas: Vec<TextArea> = Vec::new();
    if let Some(cl) = &confirm_layout {
        for (buf, (ox, oy)) in [
            (&cf_title, cl.title_origin),
            (&cf_msg, cl.msg_origin),
            (&cf_cancel, cl.cancel_origin),
            (&cf_ok, cl.ok_origin),
        ] {
            overlay_areas.push(TextArea {
                buffer: buf,
                left: ox * sf,
                top: oy * sf,
                scale: sf,
                bounds: full_bounds,
                default_color: Color::rgb(FG.r, FG.g, FG.b),
                custom_glyphs: &[],
            });
        }
    }
    text_renderer
        .prepare_with_custom(
            device,
            queue,
            font_system,
            &mut atlas,
            &viewport,
            areas,
            &mut swash_cache,
            crate::sprite::rasterize,
        )
        .map_err(|e| CaptureError::TextPrepare(format!("{e:?}")))?;
    overlay_text
        .prepare(
            device,
            queue,
            font_system,
            &mut atlas,
            &viewport,
            overlay_areas,
            &mut swash_cache,
        )
        .map_err(|e| CaptureError::TextPrepare(format!("{e:?}")))?;

    // Read-back buffer with 256-byte-aligned rows (wgpu copy requirement).
    let bpp = 4u32;
    let unpadded = phys_w * bpp;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = unpadded.div_ceil(align) * align;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("ember-readback"),
        size: (padded * phys_h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder =
        device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("ember-headless"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: srgb_to_linear(BG.r) as f64,
                        g: srgb_to_linear(BG.g) as f64,
                        b: srgb_to_linear(BG.b) as f64,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        if draw_image {
            image.draw(&mut pass);
        }
        let split = spark_layer as u32;
        let sharp = quads.sharp_count();
        // Base rounded quads (tab pills) before pane text; the confirm modal's
        // scrim+panel+buttons after it, so the opaque panel covers pane glyphs.
        let base_rounded_end = sharp + rounded_pre_confirm;
        quads.draw_range(&mut pass, 0..split);
        sparks.draw(&mut pass);
        quads.draw_range(&mut pass, split..sharp); // cells + chrome
        quads.draw_range(&mut pass, sharp..base_rounded_end); // tab pills
        let _ = text_renderer.render(&atlas, &viewport, &mut pass); // panes + chrome
        quads.draw_range(&mut pass, base_rounded_end..u32::MAX); // confirm panel
        let _ = overlay_text.render(&atlas, &viewport, &mut pass); // dialog text
    }
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(phys_h),
            },
        },
        wgpu::Extent3d {
            width: phys_w,
            height: phys_h,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(Some(encoder.finish()));

    let slice = readback.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .map_err(|e| CaptureError::Map(format!("{e:?}")))?;
    rx.recv()
        .map_err(|e| CaptureError::Map(format!("{e}")))?
        .map_err(|e| CaptureError::Map(format!("{e:?}")))?;

    let data = slice.get_mapped_range();
    let mut pixels = Vec::with_capacity((unpadded * phys_h) as usize);
    for row in 0..phys_h {
        let start = (row * padded) as usize;
        pixels.extend_from_slice(&data[start..start + unpadded as usize]);
    }
    drop(data);
    readback.unmap();

    write_png(path, phys_w, phys_h, &pixels)
}

/// Render one frame of a wisp style's particle cluster to a PNG, on an
/// OPAQUE dark backdrop rather than the wisp window's real transparent
/// canvas — the wisp itself only ever draws over a fully transparent clear
/// (see [`crate::wisp::WispRenderer::render`]), which isn't viewable as a
/// standalone PNG. This swaps in [`crate::renderer::BG`] (the app's own
/// dark background) as an opaque base purely so the additive quads show up
/// against something, exactly the way they'd read floating over a dark
/// desktop/terminal behind the real wisp window.
///
/// Debug/comparison tooling only (`ember-term --screenshot <path>
/// --wisp-preview <style>`, v0.4.1's 6-style wisp) — builds its own
/// throwaway GPU device, same cost tradeoff as [`capture`].
pub fn capture_wisp_preview(
    style: ember_core::WispStyle,
    t: f32,
    intensity: f32,
    velocity: (f32, f32),
    logical_size: f32,
    scale: f32,
    path: &Path,
) -> Result<(), CaptureError> {
    let (device, queue) = pollster::block_on(async {
        let instance = Instance::new(InstanceDescriptor::new_without_display_handle());
        let adapter = instance
            .request_adapter(&RequestAdapterOptions {
                compatible_surface: None,
                ..Default::default()
            })
            .await
            .map_err(|e| CaptureError::Adapter(format!("{e:?}")))?;
        adapter
            .request_device(&DeviceDescriptor::default())
            .await
            .map_err(|e| CaptureError::Device(format!("{e:?}")))
    })?;

    let sf = scale.max(0.1);
    let phys = ((logical_size * sf).ceil() as u32).max(1);
    let format = TextureFormat::Rgba8UnormSrgb;
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("ember-wisp-preview"),
        size: wgpu::Extent3d {
            width: phys,
            height: phys,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    let mut sparks = SparkRenderer::new(&device, format);
    // Same generator the live wisp window calls every frame — the preview
    // is pixel-for-pixel what that style actually draws, just against an
    // opaque canvas instead of a transparent one.
    let quads = crate::wisp::wisp_quads(style, t, intensity, velocity, phys as f32, phys as f32);
    sparks.prepare(&device, &queue, (phys as f32, phys as f32), &quads);
    // The `coal` style draws a solid procedural burning-rock body UNDER the
    // additive spark shower — the additive pass alone can't do solid.
    let coal = if matches!(style, ember_core::WispStyle::Coal) {
        let c = crate::coal::CoalRenderer::new(&device, format);
        c.prepare(&queue, (phys as f32, phys as f32), t, intensity);
        Some(c)
    } else {
        None
    };

    let bpp = 4u32;
    let unpadded = phys * bpp;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = unpadded.div_ceil(align) * align;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("ember-wisp-preview-readback"),
        size: (padded * phys) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder =
        device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("ember-wisp-preview"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: srgb_to_linear(BG.r) as f64,
                        g: srgb_to_linear(BG.g) as f64,
                        b: srgb_to_linear(BG.b) as f64,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        if let Some(coal) = &coal {
            coal.draw(&mut pass);
        }
        sparks.draw(&mut pass);
    }
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(phys),
            },
        },
        wgpu::Extent3d {
            width: phys,
            height: phys,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(Some(encoder.finish()));

    let slice = readback.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .map_err(|e| CaptureError::Map(format!("{e:?}")))?;
    rx.recv()
        .map_err(|e| CaptureError::Map(format!("{e}")))?
        .map_err(|e| CaptureError::Map(format!("{e:?}")))?;

    let data = slice.get_mapped_range();
    let mut pixels = Vec::with_capacity((unpadded * phys) as usize);
    for row in 0..phys {
        let start = (row * padded) as usize;
        pixels.extend_from_slice(&data[start..start + unpadded as usize]);
    }
    drop(data);
    readback.unmap();

    write_png(path, phys, phys, &pixels)
}

fn write_png(path: &Path, w: u32, h: u32, rgba: &[u8]) -> Result<(), CaptureError> {
    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(rgba)?;
    Ok(())
}
