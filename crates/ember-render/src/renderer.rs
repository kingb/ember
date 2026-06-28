//! The GPU cell renderer (design §6; , ). A pure consumer: it owns
//! one neutral grid *per session*, applies owned `GridDelta`s off the pixel lane,
//! and tiles the visible panes by the layout rects the app hands it.
//!
//! v1 scope: monospace text with per-cell fg/bg color and a block cursor, drawn
//! per pane within its rect; a minimal tab strip when more than one tab is open;
//! a focus border on the active pane when the window is split. The multiplexer
//! *logic* (layout tree, splits, focus) lives in `ember-core`; this layer only
//! draws what it is told and routes deltas to the right pane.

use std::collections::HashMap;
use std::sync::Arc;

use ember_core::{GridDelta, GridDims, Rect, Rgb, SessionId};
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

const FONT_SIZE: f32 = 12.0;
const LINE_HEIGHT: f32 = 15.0;
/// Approximate monospace advance as a fraction of font size — used only to pick a
/// sensible default window size; glyphon does the real per-glyph advance.
pub const CELL_WIDTH: f32 = FONT_SIZE * 0.6;
pub const CELL_HEIGHT: f32 = LINE_HEIGHT;
const PAD: f32 = 4.0;

/// Dark background fill (matches the surface clear).
const BG: Rgb = Rgb::new(0x10, 0x10, 0x10);
/// Default foreground for blanks / separators.
const FG: Rgb = Rgb::new(0xcc, 0xcc, 0xcc);
/// Accent used for the focused-pane border and the active tab background.
const ACCENT: Rgb = Rgb::new(0xe2, 0x5a, 0x1c);

/// One visible pane: which session fills it and the inner pixel rect it occupies
/// (already inset for padding by the app).
#[derive(Clone, Debug)]
pub struct VisiblePane {
    pub session: SessionId,
    pub rect: Rect,
}

/// One entry in the tab strip.
#[derive(Clone, Debug)]
pub struct TabLabel {
    pub title: String,
    pub active: bool,
}

/// Per-session render state: the neutral grid plus the glyph buffer it flows into.
struct PaneRender {
    grid: GridModel,
    buffer: Buffer,
}

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
    quads: QuadRenderer,
    /// One grid + glyph buffer per live session (visible or backgrounded).
    panes: HashMap<SessionId, PaneRender>,
    /// The panes to draw this frame, in layout order, with their inner rects.
    visible: Vec<VisiblePane>,
    /// The session that owns the cursor / focus border.
    focused: Option<SessionId>,
    /// Tab strip entries (drawn only when more than one tab exists).
    tabs: Vec<TabLabel>,
    /// Glyph buffer for the tab strip.
    chrome: Buffer,
    /// Measured monospace advance (px) — keeps bg quads aligned with glyphs.
    cell_w: f32,
    // Keep the window LAST so it drops after the surface (winit/wgpu requirement).
    window: Arc<Window>,
}

impl Renderer {
    /// Build the renderer for an existing window. Blocks on async GPU init.
    pub fn new(window: Arc<Window>) -> Self {
        pollster::block_on(Self::new_async(window))
    }

    async fn new_async(window: Arc<Window>) -> Self {
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

        let mut chrome = Buffer::new(&mut font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
        chrome.set_size(&mut font_system, Some(width as f32), Some(LINE_HEIGHT));

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
            quads,
            panes: HashMap::new(),
            visible: Vec::new(),
            focused: None,
            tabs: Vec::new(),
            chrome,
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

    /// Measured `(cell_width, cell_height)` in px — the app derives pane grid
    /// dimensions from these.
    pub fn cell_size(&self) -> (f32, f32) {
        (self.cell_w, CELL_HEIGHT)
    }

    /// Height in px reserved for the tab strip given a tab count (0 for a single
    /// tab — no strip is drawn). The app subtracts this from the layout viewport.
    pub fn chrome_height(tab_count: usize) -> f32 {
        if tab_count > 1 {
            CELL_HEIGHT + 2.0 * PAD
        } else {
            0.0
        }
    }

    /// Register a session's grid so deltas can be routed to it. Idempotent.
    pub fn ensure_pane(&mut self, session: &SessionId, dims: GridDims) {
        if self.panes.contains_key(session) {
            return;
        }
        let mut buffer = Buffer::new(&mut self.font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
        buffer.set_size(&mut self.font_system, Some(1.0), Some(1.0));
        self.panes.insert(
            session.clone(),
            PaneRender {
                grid: GridModel::new(dims),
                buffer,
            },
        );
    }

    /// Drop a session's grid (its shell exited or its pane was closed).
    pub fn remove_pane(&mut self, session: &SessionId) {
        self.panes.remove(session);
    }

    /// Apply an owned delta to the pane backing `session`, off the pixel lane.
    pub fn apply_delta(&mut self, session: &SessionId, delta: GridDelta) {
        if let Some(p) = self.panes.get_mut(session) {
            p.grid.apply(delta);
        }
    }

    /// Set which panes are drawn this frame (and their inner rects), the focused
    /// session, and the tab strip. Resizes each visible pane's glyph buffer.
    pub fn set_visible(
        &mut self,
        visible: Vec<VisiblePane>,
        focused: SessionId,
        tabs: Vec<TabLabel>,
    ) {
        for vp in &visible {
            if let Some(p) = self.panes.get_mut(&vp.session) {
                p.buffer.set_size(
                    &mut self.font_system,
                    Some(vp.rect.width as f32),
                    Some(vp.rect.height as f32),
                );
            }
        }
        self.visible = visible;
        self.focused = Some(focused);
        self.tabs = tabs;
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&self.device, &self.config);
        self.chrome.set_size(
            &mut self.font_system,
            Some(self.config.width as f32),
            Some(LINE_HEIGHT),
        );
        self.window.request_redraw();
    }

    /// Draw all visible panes (each in its rect) plus the tab strip. Returns
    /// `false` if the surface needs reconfiguring (request another redraw).
    pub fn render(&mut self) -> bool {
        // Pass 1: shape each visible pane's text into its own buffer.
        for vp in &self.visible {
            if let Some(p) = self.panes.get_mut(&vp.session) {
                let lines = p.grid.dims.screen_lines;
                let mut spans: Vec<(String, Color)> = Vec::new();
                for row in 0..lines {
                    for (text, fg) in p.grid.row_runs(row) {
                        spans.push((text, Color::rgb(fg.r, fg.g, fg.b)));
                    }
                    if row + 1 < lines {
                        spans.push(("\n".to_string(), Color::rgb(FG.r, FG.g, FG.b)));
                    }
                }
                let base = Attrs::new().family(Family::Monospace);
                p.buffer.set_rich_text(
                    &mut self.font_system,
                    spans.iter().map(|(t, c)| {
                        (t.as_str(), Attrs::new().family(Family::Monospace).color(*c))
                    }),
                    &base,
                    Shaping::Advanced,
                    None,
                );
                p.buffer.shape_until_scroll(&mut self.font_system, false);
            }
        }

        // Pass 2: collect background fills, cursor, focus border, and tab strip.
        let cw = self.cell_w;
        let ch = CELL_HEIGHT;
        let split = self.visible.len() > 1;
        let mut rects: Vec<([f32; 4], [f32; 4])> = Vec::new();
        for vp in &self.visible {
            let Some(p) = self.panes.get(&vp.session) else {
                continue;
            };
            let ox = vp.rect.x as f32;
            let oy = vp.rect.y as f32;
            let cols = p.grid.dims.columns;
            let lines = p.grid.dims.screen_lines;
            for row in 0..lines {
                for col in 0..cols {
                    if let Some(cell) = p.grid.cell(row, col) {
                        let bg = p.grid.style_of(cell.style).bg;
                        if bg != BG {
                            rects.push((
                                [ox + col as f32 * cw, oy + row as f32 * ch, cw, ch],
                                lin_rgba(bg, 1.0),
                            ));
                        }
                    }
                }
            }
            let focused = self.focused.as_ref() == Some(&vp.session);
            if focused {
                let cursor = p.grid.cursor;
                if cursor.visible {
                    rects.push((
                        [
                            ox + cursor.col as f32 * cw,
                            oy + cursor.row as f32 * ch,
                            cw,
                            ch,
                        ],
                        lin_rgba(FG, 0.5),
                    ));
                }
            }
            // Border the focused pane only when the window is actually split.
            if focused && split {
                push_border(&mut rects, vp.rect, ACCENT);
            }
        }
        self.build_tab_strip(&mut rects);

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

        // Pass 3: one TextArea per visible pane (clipped to its rect) + the strip.
        let full_bounds = TextBounds {
            left: 0,
            top: 0,
            right: self.config.width as i32,
            bottom: self.config.height as i32,
        };
        let mut areas: Vec<TextArea> = Vec::new();
        for vp in &self.visible {
            if let Some(p) = self.panes.get(&vp.session) {
                areas.push(TextArea {
                    buffer: &p.buffer,
                    left: vp.rect.x as f32,
                    top: vp.rect.y as f32,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: vp.rect.x as i32,
                        top: vp.rect.y as i32,
                        right: (vp.rect.x + vp.rect.width) as i32,
                        bottom: (vp.rect.y + vp.rect.height) as i32,
                    },
                    default_color: Color::rgb(FG.r, FG.g, FG.b),
                    custom_glyphs: &[],
                });
            }
        }
        if self.tabs.len() > 1 {
            areas.push(TextArea {
                buffer: &self.chrome,
                left: 0.0,
                top: PAD,
                scale: 1.0,
                bounds: full_bounds,
                default_color: Color::rgb(FG.r, FG.g, FG.b),
                custom_glyphs: &[],
            });
        }

        let prepared = self.text_renderer.prepare(
            &self.device,
            &self.queue,
            &mut self.font_system,
            &mut self.atlas,
            &self.viewport,
            areas,
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

    /// Build the tab strip: a background quad behind the active tab plus the
    /// concatenated labels shaped into the chrome buffer. No-op for a single tab.
    fn build_tab_strip(&mut self, rects: &mut Vec<([f32; 4], [f32; 4])>) {
        if self.tabs.len() <= 1 {
            return;
        }
        let cw = self.cell_w;
        let strip_h = CELL_HEIGHT + 2.0 * PAD;
        let mut x = 0.0f32;
        let mut spans: Vec<(String, Color)> = Vec::new();
        for tab in &self.tabs {
            let label = format!(" {} ", tab.title);
            let w = label.chars().count() as f32 * cw;
            if tab.active {
                rects.push(([x, 0.0, w, strip_h], lin_rgba(ACCENT, 0.85)));
            }
            let fg = if tab.active {
                Color::rgb(0xff, 0xff, 0xff)
            } else {
                Color::rgb(0x88, 0x88, 0x88)
            };
            spans.push((label, fg));
            x += w;
        }
        let base = Attrs::new().family(Family::Monospace);
        self.chrome.set_rich_text(
            &mut self.font_system,
            spans
                .iter()
                .map(|(t, c)| (t.as_str(), Attrs::new().family(Family::Monospace).color(*c))),
            &base,
            Shaping::Advanced,
            None,
        );
        self.chrome.shape_until_scroll(&mut self.font_system, false);
    }
}

/// Push four thin quads outlining `rect` in `color` (a 1px focus border).
fn push_border(rects: &mut Vec<([f32; 4], [f32; 4])>, rect: Rect, color: Rgb) {
    let t = 1.0f32;
    let (x, y, w, h) = (
        rect.x as f32,
        rect.y as f32,
        rect.width as f32,
        rect.height as f32,
    );
    let c = lin_rgba(color, 1.0);
    rects.push(([x, y, w, t], c)); // top
    rects.push(([x, y + h - t, w, t], c)); // bottom
    rects.push(([x, y, t, h], c)); // left
    rects.push(([x + w - t, y, t, h], c)); // right
}
