//! Headless screenshot renderer (debug / self-review;  follow-up).
//!
//! Renders a deterministic scene to an offscreen texture and writes a PNG — no
//! window, no surface, so it runs in an agent's shell or CI (Metal/Vulkan render
//! headless). It reuses the *same* draw helpers as the on-screen [`Renderer`]
//! (`shape_grid` / `grid_quads` / `build_tabs`), so the PNG is what ships,
//! pixel-for-pixel. Pass a `scale` to reproduce a HiDPI (Retina) target.

use std::path::Path;

use ember_core::Rect;
use glyphon::{
    Buffer, Cache, Color, Metrics, Resolution, SwashCache, TextArea, TextAtlas, TextBounds,
    TextRenderer, Viewport,
};
use wgpu::{
    DeviceDescriptor, Instance, InstanceDescriptor, MultisampleState, RequestAdapterOptions,
    TextureFormat,
};

use crate::background::{ImageRenderer, SparkRenderer};
use crate::grid_model::GridModel;
use crate::paint::{
    AboutLayout, bell_wash, build_about, build_fps, build_help, build_settings, build_tabs,
    grid_quads, measure_cell_width, push_backdrop, scrollbar, selection_quads, shape_grid,
    spark_quads, split_preview,
};
use crate::quads::{QuadRenderer, srgb_to_linear};
use crate::renderer::{
    ABOUT_TITLE_LINE, ABOUT_TITLE_SIZE, AMBER, AboutInfo, BG, BackdropParams, CELL_HEIGHT, FG,
    FONT_SIZE, HELP_PAD, ImageFit, LINE_HEIGHT, PAD, TabLabel,
};
use crate::selection::Selection;

/// One pane in a screenshot scene: a grid and the **logical** inner rect it fills.
pub struct PaneShot<'a> {
    pub grid: &'a GridModel,
    pub rect: Rect,
    pub focused: bool,
    /// Text selection to highlight in this pane, if any.
    pub selection: Option<Selection>,
    /// Ctrl+Opt split preview `(horizontal, ratio)` for this pane, if any.
    pub split_preview: Option<(bool, f32)>,
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
    /// When set, the cheat-sheet overlay is drawn instead of the panes.
    pub help: Option<Vec<(String, String)>>,
    /// Overrides the help panel's `(title, hint)`; `None` → shortcuts default.
    pub help_title: Option<(String, String)>,
    /// When set, the About overlay is drawn, with `(info, glow, elapsed_seconds)`.
    pub about: Option<(AboutInfo, f32, f32)>,
    /// When set, the Settings overlay is drawn: `(rows of (label, value), selected)`.
    pub settings: Option<(Vec<(String, String)>, usize)>,
    /// Campfire backdrop + ember sparks (drawn behind the panes when active).
    pub backdrop: BackdropParams,
    /// A backdrop image as `(rgba8, width, height)`; drawn behind the cells in
    /// place of the gradient when set.
    pub image: Option<(Vec<u8>, u32, u32)>,
    /// How the backdrop image fills the window.
    pub image_fit: ImageFit,
    /// FPS/frame-time debug readout text (bottom-right), or `None`.
    pub fps_overlay: Option<String>,
    /// Visual-bell flash intensity (`0..1`) — a warm amber wash over the panes.
    pub bell_flash: f32,
}

/// The measured `(cell_width, cell_height)` in logical px — lets a caller derive
/// pane grid dimensions to match what `capture` will draw. CPU-only (no GPU).
pub fn cell_metrics() -> (f32, f32) {
    let mut font_system = crate::paint::new_font_system();
    (measure_cell_width(&mut font_system), CELL_HEIGHT)
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

/// Render `shot` and write it to `path` as a PNG. Blocks on GPU work.
pub fn capture(shot: &Shot, path: &Path) -> Result<(), CaptureError> {
    pollster::block_on(capture_async(shot, path))
}

async fn capture_async(shot: &Shot<'_>, path: &Path) -> Result<(), CaptureError> {
    let sf = shot.scale.max(0.1);
    let phys_w = ((shot.logical_w * sf).ceil() as u32).max(1);
    let phys_h = ((shot.logical_h * sf).ceil() as u32).max(1);

    let instance = Instance::new(InstanceDescriptor::new_without_display_handle());
    let adapter = instance
        .request_adapter(&RequestAdapterOptions {
            compatible_surface: None,
            ..Default::default()
        })
        .await
        .map_err(|e| CaptureError::Adapter(format!("{e:?}")))?;
    let (device, queue) = adapter
        .request_device(&DeviceDescriptor::default())
        .await
        .map_err(|e| CaptureError::Device(format!("{e:?}")))?;

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

    let mut font_system = crate::paint::new_font_system();
    let mut swash_cache = SwashCache::new();
    let cache = Cache::new(&device);
    let mut viewport = Viewport::new(&device, &cache);
    let mut atlas = TextAtlas::new(&device, &queue, &cache, format);
    let mut text_renderer =
        TextRenderer::new(&mut atlas, &device, MultisampleState::default(), None);
    let mut quads = QuadRenderer::new(&device, format);
    let mut sparks = SparkRenderer::new(&device, format);
    let mut image = ImageRenderer::new(&device, format);
    let mut draw_image = false;
    let cw = measure_cell_width(&mut font_system);

    let full_bounds = TextBounds {
        left: 0,
        top: 0,
        right: phys_w as i32,
        bottom: phys_h as i32,
    };
    let mut buffers: Vec<Buffer> = Vec::new();
    let mut help_buf = Buffer::new(&mut font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
    let mut chrome = Buffer::new(&mut font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
    let mut about_title = Buffer::new(
        &mut font_system,
        Metrics::new(ABOUT_TITLE_SIZE, ABOUT_TITLE_LINE),
    );
    let mut about_body = Buffer::new(&mut font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
    let mut settings_buf = Buffer::new(&mut font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
    let mut fps_buf = Buffer::new(&mut font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
    let mut rects: Vec<([f32; 4], [f32; 4])> = Vec::new();
    let mut spark_rects: Vec<([f32; 4], [f32; 4])> = Vec::new();
    let mut help_panel: Option<Rect> = None;
    let mut about_layout: Option<AboutLayout> = None;
    let mut settings_origin: Option<(f32, f32)> = None;
    let mut fps_origin: Option<(f32, f32)> = None;

    if let Some((rows, sel)) = &shot.settings {
        settings_origin = Some(build_settings(
            &mut font_system,
            &mut settings_buf,
            rows,
            *sel,
            cw,
            shot.logical_w,
            shot.logical_h,
            sf,
            &mut rects,
        ));
    } else if let Some((info, glow, t)) = &shot.about {
        // About overlay replaces the panes (same helper as on-screen).
        about_layout = Some(build_about(
            &mut font_system,
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
            &mut font_system,
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
            image.set_image(&device, &queue, rgba, *w, *h);
            image.prepare(
                &device,
                &queue,
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
        if shot.backdrop.sparks {
            spark_rects = spark_quads(
                shot.backdrop.density,
                shot.backdrop.time,
                shot.logical_w,
                shot.logical_h,
                sf,
            );
        }
        // Shape each pane into its own logical-sized buffer, then build quads.
        for pane in &shot.panes {
            let mut buffer = Buffer::new(&mut font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
            buffer.set_size(
                &mut font_system,
                Some(pane.rect.width as f32),
                Some(pane.rect.height as f32),
            );
            shape_grid(&mut font_system, &mut buffer, pane.grid);
            buffers.push(buffer);
        }
        let split = shot.panes.len() > 1;
        for pane in &shot.panes {
            grid_quads(
                pane.grid,
                pane.rect,
                cw,
                sf,
                pane.focused,
                split,
                &mut rects,
            );
            if let Some(sel) = &pane.selection {
                selection_quads(pane.grid, sel, pane.rect, cw, sf, &mut rects);
            }
            if let Some((horizontal, ratio)) = pane.split_preview {
                split_preview(pane.rect, horizontal, ratio, sf, &mut rects);
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
        build_tabs(
            &mut font_system,
            &mut chrome,
            &shot.tabs,
            shot.tab_drag,
            cw,
            shot.logical_w,
            sf,
            &mut rects,
        );
        if let Some(text) = &shot.fps_overlay {
            fps_origin = Some(build_fps(
                &mut font_system,
                &mut fps_buf,
                text,
                cw,
                shot.logical_w,
                shot.logical_h,
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
    quads.prepare(&device, &queue, (phys_w as f32, phys_h as f32), &rects);
    sparks.prepare(
        &device,
        &queue,
        (phys_w as f32, phys_h as f32),
        &spark_rects,
    );

    viewport.update(
        &queue,
        Resolution {
            width: phys_w,
            height: phys_h,
        },
    );

    let mut areas: Vec<TextArea> = Vec::new();
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
        for (pane, buffer) in shot.panes.iter().zip(buffers.iter()) {
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
                custom_glyphs: &[],
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
    }
    text_renderer
        .prepare(
            &device,
            &queue,
            &mut font_system,
            &mut atlas,
            &viewport,
            areas,
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
        quads.draw(&mut pass);
        sparks.draw(&mut pass);
        let _ = text_renderer.render(&atlas, &viewport, &mut pass);
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

fn write_png(path: &Path, w: u32, h: u32, rgba: &[u8]) -> Result<(), CaptureError> {
    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(rgba)?;
    Ok(())
}
