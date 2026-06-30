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
use std::path::Path;
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

pub(crate) const FONT_SIZE: f32 = 12.0;
pub(crate) const LINE_HEIGHT: f32 = 15.0;
/// Approximate monospace advance as a fraction of font size — used only to pick a
/// sensible default window size; glyphon does the real per-glyph advance.
pub const CELL_WIDTH: f32 = FONT_SIZE * 0.6;
pub const CELL_HEIGHT: f32 = LINE_HEIGHT;
pub(crate) const PAD: f32 = 4.0;

/// Dark background fill (matches the surface clear).
pub(crate) const BG: Rgb = Rgb::new(0x10, 0x10, 0x10);
/// Default foreground for blanks / separators.
pub(crate) const FG: Rgb = Rgb::new(0xcc, 0xcc, 0xcc);
/// Accent used for the focused-pane border and the active tab background.
pub(crate) const ACCENT: Rgb = Rgb::new(0xe2, 0x5a, 0x1c);
/// Inner padding of the help (cheat-sheet) overlay panel, in logical px.
pub(crate) const HELP_PAD: f32 = 16.0;

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

/// What the tab strip was clicked on (from [`Renderer::tab_hit`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabHit {
    /// The tab button at this index.
    Tab(usize),
    /// The trailing "+" button (open a new tab).
    NewTab,
}

/// A read-only snapshot of a pane's grid for the debug control surface.
#[derive(Clone, Debug)]
pub struct PaneSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub cursor_visible: bool,
    pub styles_known: usize,
    pub text: String,
}

/// Per-session render state: the neutral grid plus the glyph buffer it flows into.
struct PaneRender {
    grid: GridModel,
    buffer: Buffer,
}

/// Measure the monospace advance for the current font/size, so background quads
/// line up with the glyphs glyphon flows.
pub(crate) fn measure_cell_width(font_system: &mut FontSystem) -> f32 {
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
pub(crate) fn lin_rgba(c: Rgb, a: f32) -> [f32; 4] {
    [
        srgb_to_linear(c.r),
        srgb_to_linear(c.g),
        srgb_to_linear(c.b),
        a,
    ]
}

// --- Shared draw logic (windowed Renderer + headless screenshot) -------------
// These free fns are the single source of truth for how a grid becomes glyphs +
// quads, so the headless PNG matches what ships on screen pixel-for-pixel.

/// Shape one grid's rows into `buffer` as per-cell fg-colored runs (one logical
/// line per grid row).
pub(crate) fn shape_grid(font_system: &mut FontSystem, buffer: &mut Buffer, grid: &GridModel) {
    let lines = grid.dims.screen_lines;
    let mut spans: Vec<(String, Color)> = Vec::new();
    for row in 0..lines {
        for (text, fg) in grid.row_runs(row) {
            spans.push((text, Color::rgb(fg.r, fg.g, fg.b)));
        }
        if row + 1 < lines {
            spans.push(("\n".to_string(), Color::rgb(FG.r, FG.g, FG.b)));
        }
    }
    let base = Attrs::new().family(Family::Monospace);
    buffer.set_rich_text(
        font_system,
        spans
            .iter()
            .map(|(t, c)| (t.as_str(), Attrs::new().family(Family::Monospace).color(*c))),
        &base,
        Shaping::Advanced,
        None,
    );
    buffer.shape_until_scroll(font_system, false);
}

/// Append a grid's bg fills + (when focused) cursor + (when focused && split)
/// focus border, for a pane at logical `rect`, scaled to physical px by `sf`.
pub(crate) fn grid_quads(
    grid: &GridModel,
    rect: Rect,
    cw: f32,
    sf: f32,
    focused: bool,
    split: bool,
    out: &mut Vec<([f32; 4], [f32; 4])>,
) {
    let ox = rect.x as f32;
    let oy = rect.y as f32;
    let ch = CELL_HEIGHT;
    for row in 0..grid.dims.screen_lines {
        for col in 0..grid.dims.columns {
            if let Some(cell) = grid.cell(row, col) {
                let bg = grid.style_of(cell.style).bg;
                if bg != BG {
                    out.push((
                        scaled(ox + col as f32 * cw, oy + row as f32 * ch, cw, ch, sf),
                        lin_rgba(bg, 1.0),
                    ));
                }
            }
        }
    }
    if focused {
        let cur = grid.cursor;
        if cur.visible {
            out.push((
                scaled(
                    ox + cur.col as f32 * cw,
                    oy + cur.row as f32 * ch,
                    cw,
                    ch,
                    sf,
                ),
                lin_rgba(FG, 0.5),
            ));
        }
        if split {
            push_border(out, rect, ACCENT, sf);
        }
    }
}

/// Background of the tab strip (a touch lighter than the terminal, iTerm-style).
const STRIP_BG: Rgb = Rgb::new(0x1b, 0x1b, 0x1b);
/// Fill of the active tab button.
const TAB_ACTIVE: Rgb = Rgb::new(0x2e, 0x2e, 0x2e);

/// Center `s` in a field `width` columns wide (truncating with `…` if too long).
fn center(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if width == 0 {
        return String::new();
    }
    if n >= width {
        if width == 1 {
            return "…".to_string();
        }
        let keep: String = s.chars().take(width - 1).collect();
        return format!("{keep}…");
    }
    let total = width - n;
    let left = total / 2;
    let right = total - left;
    format!("{}{}{}", " ".repeat(left), s, " ".repeat(right))
}

/// Build the tab strip (iTerm-style): a full-width bar, equal-width tab buttons
/// (active one lighter with an Ember-orange underline), `⌘N` hints, and a `+`
/// button. Quads go to `out`; the single concatenated label line is shaped into
/// `chrome`. No-op for a single tab. All geometry is logical px, scaled by `sf`.
pub(crate) fn build_tabs(
    font_system: &mut FontSystem,
    chrome: &mut Buffer,
    tabs: &[TabLabel],
    cw: f32,
    logical_w: f32,
    sf: f32,
    out: &mut Vec<([f32; 4], [f32; 4])>,
) {
    if tabs.len() <= 1 {
        return;
    }
    chrome.set_size(font_system, Some(logical_w), Some(LINE_HEIGHT));
    let strip_h = CELL_HEIGHT + 2.0 * PAD;
    // Full-width strip background.
    out.push((
        scaled(0.0, 0.0, logical_w, strip_h, sf),
        lin_rgba(STRIP_BG, 1.0),
    ));

    // Work in integer columns so quads and (monospace) text stay aligned.
    let total_cols = (logical_w / cw).floor() as usize;
    let plus_cols = 3usize.min(total_cols);
    let tab_cols = total_cols.saturating_sub(plus_cols);
    let n = tabs.len(); // >= 2 (single-tab strips return early above)
    let seg = tab_cols / n;

    let base = Attrs::new().family(Family::Monospace);
    let mut spans: Vec<(String, Color)> = Vec::new();
    let mut col = 0usize;
    for (i, tab) in tabs.iter().enumerate() {
        // Last tab absorbs any leftover columns so the row fills exactly.
        let width = if i == n - 1 { tab_cols - col } else { seg };
        let x = col as f32 * cw;
        let w = width as f32 * cw;
        if tab.active {
            out.push((scaled(x, 0.0, w, strip_h, sf), lin_rgba(TAB_ACTIVE, 1.0)));
            // Ember-orange underline accent on the active tab.
            out.push((scaled(x, strip_h - 2.0, w, 2.0, sf), lin_rgba(ACCENT, 1.0)));
        }
        let label = format!("{}  ⌘{}", tab.title, i + 1);
        let fg = if tab.active {
            Color::rgb(0xff, 0xff, 0xff)
        } else {
            Color::rgb(0x8a, 0x8a, 0x8a)
        };
        spans.push((center(&label, width), fg));
        col += width;
    }
    // The "+" button.
    spans.push((center("+", plus_cols), Color::rgb(0x8a, 0x8a, 0x8a)));

    chrome.set_rich_text(
        font_system,
        spans
            .iter()
            .map(|(t, c)| (t.as_str(), Attrs::new().family(Family::Monospace).color(*c))),
        &base,
        Shaping::Advanced,
        None,
    );
    chrome.shape_until_scroll(font_system, false);
}

/// Build the cheat-sheet overlay: a full scrim + a centered panel (accent border)
/// sized to `lines`, with the `(key, desc)` rows shaped into `buffer`. Pushes quads
/// to `out` and returns the panel rect (logical px) for text placement. Shared by
/// the windowed renderer and the headless capture so they render identically.
pub(crate) fn build_help(
    font_system: &mut FontSystem,
    buffer: &mut Buffer,
    lines: &[(String, String)],
    logical_w: f32,
    logical_h: f32,
    sf: f32,
    out: &mut Vec<([f32; 4], [f32; 4])>,
) -> Rect {
    // Panel sized to content: title + dismiss hint + blank + one row per line.
    let w = (logical_w * 0.7).clamp(280.0, 460.0);
    let h = (lines.len() as f32 + 3.0) * LINE_HEIGHT + 2.0 * HELP_PAD;
    let x = ((logical_w - w) * 0.5).max(0.0);
    let y = ((logical_h - h) * 0.5).max(0.0);
    let panel = Rect::new(x as f64, y as f64, w as f64, h as f64);

    // Scrim over everything, then the panel fill + Ember-orange border.
    out.push((
        scaled(0.0, 0.0, logical_w, logical_h, sf),
        lin_rgba(Rgb::new(0, 0, 0), 0.66),
    ));
    out.push((
        scaled(x, y, w, h, sf),
        lin_rgba(Rgb::new(0x20, 0x22, 0x28), 0.98),
    ));
    push_border(out, panel, ACCENT, sf);

    // Shape the cheat-sheet text (keys in accent, descriptions in fg).
    buffer.set_size(
        font_system,
        Some(w - 2.0 * HELP_PAD),
        Some(h - 2.0 * HELP_PAD),
    );
    let mut spans: Vec<(String, Color)> = Vec::new();
    spans.push((
        "Keyboard Shortcuts\n".to_string(),
        Color::rgb(0xff, 0xff, 0xff),
    ));
    spans.push((
        "press any key to dismiss\n\n".to_string(),
        Color::rgb(0x88, 0x88, 0x88),
    ));
    for (key, desc) in lines {
        spans.push((
            format!("{key:<18}"),
            Color::rgb(ACCENT.r, ACCENT.g, ACCENT.b),
        ));
        spans.push((format!("{desc}\n"), Color::rgb(FG.r, FG.g, FG.b)));
    }
    let base = Attrs::new().family(Family::Monospace);
    buffer.set_rich_text(
        font_system,
        spans
            .iter()
            .map(|(t, c)| (t.as_str(), Attrs::new().family(Family::Monospace).color(*c))),
        &base,
        Shaping::Advanced,
        None,
    );
    buffer.shape_until_scroll(font_system, false);
    panel
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
    /// When `Some`, the cheat-sheet overlay is shown with these `(key, desc)` rows.
    help: Option<Vec<(String, String)>>,
    /// Glyph buffer for the help overlay.
    help_buffer: Buffer,
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
        let help_buffer = Buffer::new(&mut font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));

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
            help: None,
            help_buffer,
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

    /// Capture the **current on-screen scene** to a PNG (debug control surface).
    /// Renders the live grids through the same headless path used by `--screenshot`,
    /// so the PNG is pixel-identical to the window. Builds a throwaway offscreen GPU
    /// context (no surface read-back needed).
    pub fn capture_to_png(&self, path: &Path) -> Result<(), String> {
        let sf = self.window.scale_factor() as f32;
        let panes: Vec<crate::headless::PaneShot> = self
            .visible
            .iter()
            .filter_map(|vp| {
                self.panes
                    .get(&vp.session)
                    .map(|p| crate::headless::PaneShot {
                        grid: &p.grid,
                        rect: vp.rect,
                        focused: self.focused.as_ref() == Some(&vp.session),
                    })
            })
            .collect();
        let shot = crate::headless::Shot {
            logical_w: self.config.width as f32 / sf,
            logical_h: self.config.height as f32 / sf,
            scale: sf,
            panes,
            tabs: self.tabs.clone(),
            help: self.help.clone(),
        };
        crate::headless::capture(&shot, path)
    }

    /// A read-only snapshot of a pane's grid — for the debug control surface. The
    /// `text` is the whole screen as text (trailing blanks trimmed per row).
    pub fn pane_snapshot(&self, session: &SessionId) -> Option<PaneSnapshot> {
        self.panes.get(session).map(|p| PaneSnapshot {
            cols: p.grid.dims.columns,
            rows: p.grid.dims.screen_lines,
            cursor_row: p.grid.cursor.row,
            cursor_col: p.grid.cursor.col,
            cursor_visible: p.grid.cursor.visible,
            styles_known: p.grid.styles_len(),
            text: p.grid.screen_text(),
        })
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

    /// Hit-test a click at logical `(x, y)` against the tab strip. Returns the tab
    /// button or the "+" button hit, or `None` (no strip, or click below it). Must
    /// mirror the column math in [`build_tabs`].
    pub fn tab_hit(&self, x: f32, y: f32) -> Option<TabHit> {
        let n = self.tabs.len();
        if n <= 1 {
            return None;
        }
        let strip_h = CELL_HEIGHT + 2.0 * PAD;
        if !(0.0..=strip_h).contains(&y) || x < 0.0 {
            return None;
        }
        let sf = self.window.scale_factor() as f32;
        let logical_w = self.config.width as f32 / sf;
        let cw = self.cell_w;
        let total_cols = (logical_w / cw).floor() as usize;
        let plus_cols = 3usize.min(total_cols);
        let tab_cols = total_cols.saturating_sub(plus_cols);
        let seg = tab_cols / n;
        let col = (x / cw).floor() as usize;
        if col >= total_cols {
            return None;
        }
        if col >= tab_cols {
            return Some(TabHit::NewTab);
        }
        let mut acc = 0;
        for i in 0..n {
            let width = if i == n - 1 { tab_cols - acc } else { seg };
            if col >= acc && col < acc + width {
                return Some(TabHit::Tab(i));
            }
            acc += width;
        }
        None
    }

    /// Show the cheat-sheet overlay with these `(key, description)` rows, or hide
    /// it with `None`. The next `render` draws (or stops drawing) the modal.
    pub fn set_help(&mut self, lines: Option<Vec<(String, String)>>) {
        self.help = lines;
        self.window.request_redraw();
    }

    /// Whether the help overlay is currently shown.
    pub fn help_visible(&self) -> bool {
        self.help.is_some()
    }

    /// Build the help overlay using this renderer's buffer (wrapper over the shared
    /// [`build_help`] so the windowed + headless paths render it identically).
    fn build_help_quads(&mut self, sf: f32, rects: &mut Vec<([f32; 4], [f32; 4])>) -> Rect {
        let logical_w = self.config.width as f32 / sf;
        let logical_h = self.config.height as f32 / sf;
        let rows = self.help.clone().unwrap_or_default();
        build_help(
            &mut self.font_system,
            &mut self.help_buffer,
            &rows,
            logical_w,
            logical_h,
            sf,
            rects,
        )
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
        // The app works in logical px; the surface is physical. Scale every draw
        // coordinate by the live HiDPI factor (handles Retina + display moves).
        let sf = self.window.scale_factor() as f32;
        let full_bounds = TextBounds {
            left: 0,
            top: 0,
            right: self.config.width as i32,
            bottom: self.config.height as i32,
        };
        let mut rects: Vec<([f32; 4], [f32; 4])> = Vec::new();
        let mut areas: Vec<TextArea> = Vec::new();

        if self.help.is_some() {
            // Modal cheat-sheet: a scrim + centered panel + the key list, drawn
            // instead of the panes (fully obscured so the text stays legible).
            let panel = self.build_help_quads(sf, &mut rects);
            areas.push(TextArea {
                buffer: &self.help_buffer,
                left: (panel.x as f32 + HELP_PAD) * sf,
                top: (panel.y as f32 + HELP_PAD) * sf,
                scale: sf,
                bounds: full_bounds,
                default_color: Color::rgb(FG.r, FG.g, FG.b),
                custom_glyphs: &[],
            });
        } else {
            // Pass 1: shape each visible pane's text into its own buffer.
            for vp in &self.visible {
                if let Some(p) = self.panes.get_mut(&vp.session) {
                    shape_grid(&mut self.font_system, &mut p.buffer, &p.grid);
                }
            }
            // Pass 2: bg fills, cursor, focus border, tab strip (logical px * sf).
            let cw = self.cell_w;
            let split = self.visible.len() > 1;
            for vp in &self.visible {
                if let Some(p) = self.panes.get(&vp.session) {
                    let focused = self.focused.as_ref() == Some(&vp.session);
                    grid_quads(&p.grid, vp.rect, cw, sf, focused, split, &mut rects);
                }
            }
            let logical_w = self.config.width as f32 / sf;
            build_tabs(
                &mut self.font_system,
                &mut self.chrome,
                &self.tabs,
                cw,
                logical_w,
                sf,
                &mut rects,
            );

            // Pass 3: one TextArea per visible pane (clipped to its rect) + the strip.
            for vp in &self.visible {
                if let Some(p) = self.panes.get(&vp.session) {
                    areas.push(TextArea {
                        buffer: &p.buffer,
                        left: vp.rect.x as f32 * sf,
                        top: vp.rect.y as f32 * sf,
                        scale: sf,
                        bounds: TextBounds {
                            left: (vp.rect.x as f32 * sf) as i32,
                            top: (vp.rect.y as f32 * sf) as i32,
                            right: ((vp.rect.x + vp.rect.width) as f32 * sf) as i32,
                            bottom: ((vp.rect.y + vp.rect.height) as f32 * sf) as i32,
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
                    top: PAD * sf,
                    scale: sf,
                    bounds: full_bounds,
                    default_color: Color::rgb(FG.r, FG.g, FG.b),
                    custom_glyphs: &[],
                });
            }
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

        // Optional diagnostics: `EMBER_DEBUG=/tmp/e.log ember-term` (file sink) or
        // `EMBER_DEBUG=1` (stderr). Logs scale, surface size, and each visible
        // pane's rect/dims/cursor + cursor-row text, so a display-less reviewer can
        // tell whether the grid has content and whether geometry is sane. Captures
        // the first few frames (startup) plus a periodic heartbeat.
        if std::env::var_os("EMBER_DEBUG").is_some() {
            use std::sync::atomic::{AtomicU64, Ordering};
            static FRAME: AtomicU64 = AtomicU64::new(0);
            let f = FRAME.fetch_add(1, Ordering::Relaxed);
            if f < 8 || f % 60 == 0 {
                debug_emit(&format!(
                    "[ember-debug] frame={f} sf={sf} surface={}x{} visible={} areas={}",
                    self.config.width,
                    self.config.height,
                    self.visible.len(),
                    areas.len()
                ));
                for vp in &self.visible {
                    if let Some(p) = self.panes.get(&vp.session) {
                        let c = p.grid.cursor;
                        let row = c.row.min(p.grid.dims.screen_lines.saturating_sub(1));
                        debug_emit(&format!(
                            "  {:?} rect=({:.0},{:.0},{:.0},{:.0}) dims={}x{} cur=({},{},vis={}) styles_known={} row[{}]={:?}",
                            vp.session,
                            vp.rect.x,
                            vp.rect.y,
                            vp.rect.width,
                            vp.rect.height,
                            p.grid.dims.columns,
                            p.grid.dims.screen_lines,
                            c.row,
                            c.col,
                            c.visible,
                            p.grid.styles_len(),
                            row,
                            p.grid.row_text(row).trim_end()
                        ));
                    }
                }
            }
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
        if let Err(e) = prepared {
            // Don't freeze on a transient atlas/prepare error: log it (always, since
            // it means glyphs won't paint this frame) and ask for another redraw.
            debug_emit(&format!("[ember] text prepare failed this frame: {e:?}"));
            eprintln!("[ember] text prepare failed, skipping glyphs this frame: {e:?}");
            self.window.request_redraw();
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
}

/// A `(rect_px, …)`-ready physical-pixel quad from logical `x,y,w,h` and the
/// HiDPI scale factor.
pub(crate) fn scaled(x: f32, y: f32, w: f32, h: f32, sf: f32) -> [f32; 4] {
    [x * sf, y * sf, w * sf, h * sf]
}

/// Emit a debug line to the `EMBER_DEBUG` sink: a file path if the value contains
/// `/` (so a reviewer can `cat` it on the same machine), else stderr. No-op when
/// the var is unset.
pub(crate) fn debug_emit(line: &str) {
    match std::env::var("EMBER_DEBUG") {
        Ok(v) if v.contains('/') => {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&v)
            {
                let _ = writeln!(f, "{line}");
            }
        }
        Ok(_) => eprintln!("{line}"),
        Err(_) => {}
    }
}

/// Push four thin quads outlining `rect` (logical px) in `color` — a ~1.5px
/// focus border, scaled to physical px by `sf`.
pub(crate) fn push_border(rects: &mut Vec<([f32; 4], [f32; 4])>, rect: Rect, color: Rgb, sf: f32) {
    let t = 1.5f32;
    let (x, y, w, h) = (
        rect.x as f32,
        rect.y as f32,
        rect.width as f32,
        rect.height as f32,
    );
    let c = lin_rgba(color, 1.0);
    rects.push((scaled(x, y, w, t, sf), c)); // top
    rects.push((scaled(x, y + h - t, w, t, sf), c)); // bottom
    rects.push((scaled(x, y, t, h, sf), c)); // left
    rects.push((scaled(x + w - t, y, t, h, sf), c)); // right
}
