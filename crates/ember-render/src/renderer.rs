//! The GPU cell renderer (design §6; ). A pure consumer: it owns the
//! neutral grid and the glyph pipeline, applies owned `GridDelta`s, and draws.
//!
//! v1 scope: monospace text in the default foreground over a dark background,
//! present-mode chosen as Mailbox-with-Fifo-fallback (the §6 latency lever).
//! Per-cell fg/bg color and the cursor quad are the next refinement (see the
//! morning brief) — this establishes the full window→GPU→glyph path.

use std::sync::Arc;

use ember_core::{GridDelta, GridDims, Rgb};
use glyphon::{
    Attrs, Buffer, Cache, Color, Family, FontSystem, Metrics, Resolution, Shaping, SwashCache,
    TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};

use crate::quads::{QuadRenderer, srgb_to_linear};
use wgpu::{
    CompositeAlphaMode, DeviceDescriptor, Instance, InstanceDescriptor, MultisampleState,
    PresentMode, RequestAdapterOptions, SurfaceConfiguration, TextureUsages,
};
use winit::window::Window;

use crate::grid_model::GridModel;

const FONT_SIZE: f32 = 16.0;
const LINE_HEIGHT: f32 = 20.0;
/// Approximate monospace advance as a fraction of font size — used only to pick a
/// sensible default window size; glyphon does the real per-glyph advance.
pub const CELL_WIDTH: f32 = FONT_SIZE * 0.6;
pub const CELL_HEIGHT: f32 = LINE_HEIGHT;
const PAD: f32 = 4.0;

/// Measure the monospace advance for the current font/size, so background quads
/// line up with the glyphs glyphon flows.
fn measure_cell_width(font_system: &mut FontSystem) -> f32 {
    let mut probe = Buffer::new(font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
    probe.set_text(
        font_system,
        "MMMMMMMMMM",
        &Attrs::new().family(Family::Monospace),
        Shaping::Advanced,
        None,
    );
    probe.shape_until_scroll(font_system, false);
    if let Some(run) = probe.layout_runs().next() {
        let glyphs = run.glyphs;
        if glyphs.len() >= 2 {
            let span = glyphs[glyphs.len() - 1].x - glyphs[0].x;
            return span / (glyphs.len() - 1) as f32;
        }
    }
    FONT_SIZE * 0.6
}

/// sRGB `Rgb` + alpha → linear RGBA for the sRGB surface target.
fn lin_rgba(c: Rgb, a: f32) -> [f32; 4] {
    [
        srgb_to_linear(c.r),
        srgb_to_linear(c.g),
        srgb_to_linear(c.b),
        a,
    ]
}

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: SurfaceConfiguration,
    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    buffer: Buffer,
    quads: QuadRenderer,
    grid: GridModel,
    /// Measured monospace advance (px) — keeps bg quads aligned with glyphs.
    cell_w: f32,
    // Keep the window LAST so it drops after the surface (winit/wgpu requirement).
    window: Arc<Window>,
}

impl Renderer {
    /// Build the renderer for an existing window. Blocks on async GPU init.
    pub fn new(window: Arc<Window>, dims: GridDims) -> Self {
        pollster::block_on(Self::new_async(window, dims))
    }

    async fn new_async(window: Arc<Window>, dims: GridDims) -> Self {
        let size = window.inner_size();
        let (width, height) = (size.width.max(1), size.height.max(1));

        let instance = Instance::new(InstanceDescriptor::new_with_display_handle(Box::new(
            Arc::clone(&window),
        )));
        let surface = instance
            .create_surface(Arc::clone(&window))
            .expect("create surface");
        let adapter = instance
            .request_adapter(&RequestAdapterOptions {
                compatible_surface: Some(&surface),
                ..Default::default()
            })
            .await
            .expect("request adapter");
        let (device, queue) = adapter
            .request_device(&DeviceDescriptor::default())
            .await
            .expect("request device");

        let caps = surface.get_capabilities(&adapter);
        let format = caps.formats[0];
        // Present mode is the latency lever (§6): Mailbox where honored, else Fifo.
        let present_mode = if caps.present_modes.contains(&PresentMode::Mailbox) {
            PresentMode::Mailbox
        } else {
            PresentMode::Fifo
        };
        let config = SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode,
            alpha_mode: CompositeAlphaMode::Opaque,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let mut font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, format);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, MultisampleState::default(), None);

        let mut buffer = Buffer::new(&mut font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
        buffer.set_size(&mut font_system, Some(width as f32), Some(height as f32));

        let cell_w = measure_cell_width(&mut font_system);
        let quads = QuadRenderer::new(&device, format);

        Self {
            device,
            queue,
            surface,
            config,
            font_system,
            swash_cache,
            viewport,
            atlas,
            text_renderer,
            buffer,
            quads,
            grid: GridModel::new(dims),
            cell_w,
            window,
        }
    }

    pub fn present_mode(&self) -> PresentMode {
        self.config.present_mode
    }

    pub fn window(&self) -> &Arc<Window> {
        &self.window
    }

    /// Apply an owned delta off the pixel lane.
    pub fn apply_delta(&mut self, delta: GridDelta) {
        self.grid.apply(delta);
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&self.device, &self.config);
        self.buffer.set_size(
            &mut self.font_system,
            Some(self.config.width as f32),
            Some(self.config.height as f32),
        );
        self.window.request_redraw();
    }

    /// Draw the current grid. Returns `false` if the surface needs reconfiguring
    /// (the caller should request another redraw).
    pub fn render(&mut self) -> bool {
        // Build per-cell foreground-colored spans (runs of same-fg text per row,
        // rows joined by newlines).
        let lines = self.grid.dims.screen_lines;
        let default_fg = Color::rgb(0xcc, 0xcc, 0xcc);
        let mut spans: Vec<(String, Color)> = Vec::new();
        for row in 0..lines {
            for (text, fg) in self.grid.row_runs(row) {
                spans.push((text, Color::rgb(fg.r, fg.g, fg.b)));
            }
            if row + 1 < lines {
                spans.push(("\n".to_string(), default_fg));
            }
        }
        let base = Attrs::new().family(Family::Monospace);
        self.buffer.set_rich_text(
            &mut self.font_system,
            spans
                .iter()
                .map(|(t, c)| (t.as_str(), Attrs::new().family(Family::Monospace).color(*c))),
            &base,
            Shaping::Advanced,
            None,
        );
        self.buffer.shape_until_scroll(&mut self.font_system, false);

        // Build per-cell background fills + the block cursor (drawn under text).
        let default_bg = Rgb::new(0x10, 0x10, 0x10);
        let cw = self.cell_w;
        let ch = CELL_HEIGHT;
        let cols = self.grid.dims.columns;
        let mut rects: Vec<([f32; 4], [f32; 4])> = Vec::new();
        for row in 0..lines {
            for col in 0..cols {
                if let Some(cell) = self.grid.cell(row, col) {
                    let bg = self.grid.style_of(cell.style).bg;
                    if bg != default_bg {
                        let x = PAD + col as f32 * cw;
                        let y = PAD + row as f32 * ch;
                        rects.push(([x, y, cw, ch], lin_rgba(bg, 1.0)));
                    }
                }
            }
        }
        let cursor = self.grid.cursor;
        if cursor.visible {
            let x = PAD + cursor.col as f32 * cw;
            let y = PAD + cursor.row as f32 * ch;
            rects.push(([x, y, cw, ch], lin_rgba(Rgb::new(0xcc, 0xcc, 0xcc), 0.5)));
        }
        self.quads.prepare(
            &self.device,
            &self.queue,
            (self.config.width as f32, self.config.height as f32),
            &rects,
        );

        self.viewport.update(
            &self.queue,
            Resolution {
                width: self.config.width,
                height: self.config.height,
            },
        );

        let prepared = self.text_renderer.prepare(
            &self.device,
            &self.queue,
            &mut self.font_system,
            &mut self.atlas,
            &self.viewport,
            [TextArea {
                buffer: &self.buffer,
                left: PAD,
                top: PAD,
                scale: 1.0,
                bounds: TextBounds {
                    left: 0,
                    top: 0,
                    right: self.config.width as i32,
                    bottom: self.config.height as i32,
                },
                default_color: Color::rgb(0xcc, 0xcc, 0xcc),
                custom_glyphs: &[],
            }],
            &mut self.swash_cache,
        );
        if prepared.is_err() {
            return true;
        }

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) => f,
            wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated
            | wgpu::CurrentSurfaceTexture::Lost
            | wgpu::CurrentSurfaceTexture::Occluded
            | wgpu::CurrentSurfaceTexture::Timeout
            | wgpu::CurrentSurfaceTexture::Validation => {
                self.surface.configure(&self.device, &self.config);
                return false;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ember-cells"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: srgb_to_linear(0x10) as f64,
                            g: srgb_to_linear(0x10) as f64,
                            b: srgb_to_linear(0x10) as f64,
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
            self.quads.draw(&mut pass);
            let _ = self
                .text_renderer
                .render(&self.atlas, &self.viewport, &mut pass);
        }
        self.queue.submit(Some(encoder.finish()));
        frame.present();
        self.atlas.trim();
        true
    }
}
