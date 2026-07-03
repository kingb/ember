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
    Buffer, Cache, Color, FontSystem, Metrics, Resolution, SwashCache, TextArea, TextAtlas,
    TextBounds, TextRenderer, Viewport,
};

use crate::quads::{QuadRenderer, srgb_to_linear};
use wgpu::{
    CompositeAlphaMode, DeviceDescriptor, Instance, InstanceDescriptor, MultisampleState,
    PresentMode, RequestAdapterOptions, SurfaceConfiguration, TextureFormat, TextureUsages,
};
use winit::window::Window;

use crate::background::{ImageRenderer, SparkRenderer};
use crate::grid_model::GridModel;
use crate::paint::{
    BTN_COLS, bell_wash, build_about, build_fps, build_help, build_settings, build_tabs,
    debug_emit, grid_quads, measure_cell_width, push_backdrop, scrollbar, scrollbar_geometry,
    selection_quads, shape_grid, spark_quads, split_preview,
};
use crate::selection::Selection;

pub(crate) const FONT_SIZE: f32 = 12.0;
pub(crate) const LINE_HEIGHT: f32 = 15.0;
/// Live-zoom bounds for the terminal font (Cmd +/-). Chrome stays fixed-size.
pub(crate) const MIN_FONT_SIZE: f32 = 6.0;
pub(crate) const MAX_FONT_SIZE: f32 = 48.0;
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
/// Amber the ember glow brightens toward at peak intensity.
pub(crate) const AMBER: Rgb = Rgb::new(0xff, 0x9d, 0x3c);
/// About-page wordmark font size (logical px).
pub(crate) const ABOUT_TITLE_SIZE: f32 = 46.0;
pub(crate) const ABOUT_TITLE_LINE: f32 = 54.0;

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
    /// True while this tab's title is being edited inline (draw a caret + accent,
    /// omit the `⌘N` hint). The `title` then carries the live edit buffer.
    pub editing: bool,
    /// True when this tab has an unseen bell (a background tab belled) — draws a
    /// small amber indicator so the user can see which tab wants attention.
    pub bell: bool,
}

/// Static content for the About overlay (the animated glow is separate).
#[derive(Clone, Debug)]
pub struct AboutInfo {
    /// Large wordmark (e.g. "ember").
    pub title: String,
    /// Centered lines below the wordmark (tagline, version, commit, license, authors).
    pub lines: Vec<String>,
    /// Clickable `(label, url)` buttons below the lines (Docs, GitHub). Opened via
    /// the platform seam when clicked; hit-tested by [`Renderer::about_link_at`].
    pub links: Vec<(String, String)>,
}

/// Campfire backdrop + ember-spark parameters. All off by default, so
/// the terminal looks unchanged until enabled. The app updates `time` each frame
/// (and should only animate — drive redraws — while `sparks` is on).
#[derive(Clone, Copy, Debug)]
pub struct BackdropParams {
    /// Draw the warm vertical gradient behind the cells.
    pub gradient: bool,
    /// Darkening scrim over the backdrop for text legibility (`0.0`–`1.0`).
    pub scrim: f32,
    /// Draw the drifting, glowing ember sparks (additive).
    pub sparks: bool,
    /// Spark density multiplier (`1.0` ≈ 50 sparks).
    pub density: f32,
    /// Elapsed seconds, driving the spark animation.
    pub time: f32,
}

impl Default for BackdropParams {
    fn default() -> Self {
        Self {
            gradient: false,
            scrim: 0.0,
            sparks: false,
            density: 1.0,
            time: 0.0,
        }
    }
}

/// How a backdrop image fills the window. Cover/contain/stretch use a
/// clamped sampler with computed UVs; tile repeats the image at its native size.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ImageFit {
    /// Scale to fill the window, cropping the overflowing axis (default).
    #[default]
    Cover,
    /// Scale to fit entirely inside the window, letterboxing the short axis.
    Contain,
    /// Stretch to the window, ignoring aspect ratio.
    Stretch,
    /// Repeat the image at its native pixel size.
    Tile,
}

impl ImageFit {
    /// Parse a config string (`cover`|`contain`|`stretch`|`tile`); unknown → `Cover`.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "contain" => Self::Contain,
            "stretch" | "fill" => Self::Stretch,
            "tile" | "repeat" => Self::Tile,
            _ => Self::Cover,
        }
    }
}

impl From<&str> for ImageFit {
    fn from(s: &str) -> Self {
        Self::parse(s)
    }
}

/// What the tab strip was clicked on (from [`Renderer::tab_hit`]). Exhaustive on
/// purpose: it's matched only in the app, so the compiler flags an unhandled
/// button when a variant is added (more useful than downstream compat here).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabHit {
    /// The tab button at this index.
    Tab(usize),
    /// The trailing "+" button (open a new tab).
    NewTab,
    /// The trailing "?" button (toggle the shortcuts overlay).
    Help,
    /// The trailing "⚙" button (toggle the Settings overlay).
    Settings,
}

/// A pane's terminal modes (from the latest delta), driving mouse-wheel handling.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PaneModes {
    /// The alternate screen is active (vim/less/htop) — no scrollback there.
    pub alt_screen: bool,
    /// The app enabled mouse reporting — forward the wheel as mouse events.
    pub mouse_reporting: bool,
    /// Application cursor keys (DECCKM) — arrows encode as `ESC O A`….
    pub app_cursor: bool,
    /// Which mouse-reporting protocols the app enabled.
    pub mouse: ember_core::MouseProto,
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
    /// The shaped `buffer` is stale and must be re-shaped before the next draw.
    /// Set on any applied delta or buffer resize; cleared after reshaping. Lets the
    /// per-frame render skip glyphon reshaping for panes that didn't change — the
    /// big CPU win when sparks drive 60fps redraws over an idle grid.
    dirty: bool,
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
    /// In-progress tab drag: `(dragged slot, cursor x in logical px)` for the lifted,
    /// cursor-following tab; `None` when not dragging.
    tab_drag: Option<(usize, f32)>,
    /// Glyph buffer for the tab strip.
    chrome: Buffer,
    /// When `Some`, the cheat-sheet overlay is shown with these `(key, desc)` rows.
    help: Option<Vec<(String, String)>>,
    /// Overrides the help panel's `(title, hint)`; `None` → the shortcuts
    /// default. Lets the same panel serve confirmations.
    help_title: Option<(String, String)>,
    /// Glyph buffer for the help overlay.
    help_buffer: Buffer,
    /// When `Some`, the About overlay is shown.
    about: Option<AboutInfo>,
    /// Animated ember-glow intensity for the About overlay, in `[0, 1]`.
    about_glow: f32,
    /// Elapsed seconds the About overlay has been open (drives the ember sparks).
    about_time: f32,
    /// Large wordmark buffer + body-lines buffer for the About overlay.
    about_title: Buffer,
    about_body: Buffer,
    /// Logical click rects + target URLs for the About overlay's link buttons
    /// (Docs, GitHub), rebuilt each About frame. Empty when About is hidden.
    about_links: Vec<([f32; 4], String)>,
    /// When `Some`, the Settings overlay is shown: `(rows of (label, value), selected)`.
    settings: Option<(Vec<(String, String)>, usize)>,
    /// Glyph buffer for the Settings overlay.
    settings_buffer: Buffer,
    /// Measured monospace advance (px) — keeps bg quads aligned with glyphs.
    cell_w: f32,
    /// Current terminal font point size (mutated by live zoom).
    font_size: f32,
    /// Current cell/line height (px), derived from `font_size`.
    line_height: f32,
    /// Configured font family name (`None` → monospace default).
    family_name: Option<String>,
    /// Campfire backdrop + ember-spark settings (off by default).
    backdrop: BackdropParams,
    /// Additive pipeline for the glowing ember sparks.
    sparks: SparkRenderer,
    /// Full-surface textured-quad pass for a user-supplied backdrop image.
    image: ImageRenderer,
    /// How the backdrop image fills the window.
    image_fit: ImageFit,
    /// The decoded backdrop image kept in RAM so [`Self::capture_to_png`] can
    /// replay it through the headless path (the GPU texture isn't readable here).
    image_rgba: Option<(Vec<u8>, u32, u32)>,
    /// The active text selection and the session whose pane it belongs to.
    selection: Option<(SessionId, Selection)>,
    /// Ctrl+Opt split drop-zone preview: `(hovered session, horizontal, ratio)`.
    split_preview: Option<(SessionId, bool, f32)>,
    /// FPS/frame-time debug readout text (bottom-right), or `None` when hidden.
    fps_overlay: Option<String>,
    /// Glyph buffer for the FPS overlay.
    fps_buffer: Buffer,
    /// Visual-bell flash intensity (`0..1`); a warm amber wash over the panes that
    /// the app decays to 0 after a BEL. `0.0` = no flash.
    bell_flash: f32,
    // Keep the window LAST so it drops after the surface (winit/wgpu requirement).
    window: Arc<Window>,
}

impl Renderer {
    /// Build the renderer for an existing window. Blocks on async GPU init.
    pub fn new(window: Arc<Window>, font: &ember_core::Font) -> Self {
        pollster::block_on(Self::new_async(window, font))
    }

    async fn new_async(window: Arc<Window>, font: &ember_core::Font) -> Self {
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
        // The whole pipeline emits linear color and relies on an sRGB target to
        // re-encode (quads/sparks convert manually; glyphon runs in Accurate
        // mode). caps.formats[0] is Bgra8Unorm on Metal, which would display
        // those linear values raw — gamma-darkening every color on screen —
        // while the headless path (always Rgba8UnormSrgb) captures them right.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(TextureFormat::is_srgb)
            .unwrap_or(caps.formats[0]);
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

        let mut font_system = crate::paint::new_font_system();
        let swash_cache = SwashCache::new();
        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, format);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, MultisampleState::default(), None);

        let mut chrome = Buffer::new(&mut font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
        chrome.set_size(&mut font_system, Some(width as f32), Some(LINE_HEIGHT));
        let help_buffer = Buffer::new(&mut font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
        let about_title = Buffer::new(
            &mut font_system,
            Metrics::new(ABOUT_TITLE_SIZE, ABOUT_TITLE_LINE),
        );
        let about_body = Buffer::new(&mut font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
        let settings_buffer = Buffer::new(&mut font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
        let fps_buffer = Buffer::new(&mut font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));

        // Runtime font state (Cmd +/-/0 mutate size at runtime; family from cfg).
        let family_name = font.family.clone();
        let font_size = font.size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
        let line_height = crate::paint::line_height_for(font_size);
        let cell_w = measure_cell_width(
            &mut font_system,
            font_size,
            crate::paint::family_of(family_name.as_deref()),
        );
        let quads = QuadRenderer::new(&device, format);
        let sparks = SparkRenderer::new(&device, format);
        let image = ImageRenderer::new(&device, format);

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
            tab_drag: None,
            chrome,
            help: None,
            help_title: None,
            help_buffer,
            about: None,
            about_glow: 0.0,
            about_time: 0.0,
            about_title,
            about_body,
            about_links: Vec::new(),
            settings: None,
            settings_buffer,
            cell_w,
            font_size,
            line_height,
            family_name,
            backdrop: BackdropParams::default(),
            sparks,
            image,
            image_fit: ImageFit::default(),
            image_rgba: None,
            selection: None,
            split_preview: None,
            fps_overlay: None,
            fps_buffer,
            bell_flash: 0.0,
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
        (self.cell_w, self.line_height)
    }

    /// The current terminal font point size (for the app's zoom step math).
    pub fn font_size(&self) -> f32 {
        self.font_size
    }

    /// Set the terminal font size (live zoom), clamped to `[MIN, MAX]`. Re-measures
    /// the cell advance and re-metrics every pane buffer (re-shaped next frame).
    /// Returns whether it changed — the caller must re-layout (the cell size, and
    /// thus every pane's grid dimensions, changed). Chrome/overlays stay fixed.
    pub fn set_font_size(&mut self, size: f32) -> bool {
        let size = size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
        if (size - self.font_size).abs() < f32::EPSILON {
            return false;
        }
        self.font_size = size;
        self.line_height = crate::paint::line_height_for(size);
        self.cell_w = measure_cell_width(
            &mut self.font_system,
            size,
            crate::paint::family_of(self.family_name.as_deref()),
        );
        let metrics = Metrics::new(self.font_size, self.line_height);
        for p in self.panes.values_mut() {
            p.buffer.set_metrics(&mut self.font_system, metrics);
            p.dirty = true;
        }
        self.window.request_redraw();
        true
    }

    /// Height in px reserved for the tab strip. The strip is **always** drawn (it
    /// carries the +/?/⚙ controls even with one tab — design §1 discoverability),
    /// so this is constant. The app subtracts it from the layout viewport.
    pub fn chrome_height() -> f32 {
        CELL_HEIGHT + 2.0 * PAD
    }

    /// Register a session's grid so deltas can be routed to it. Idempotent.
    pub fn ensure_pane(&mut self, session: &SessionId, dims: GridDims) {
        if self.panes.contains_key(session) {
            return;
        }
        let mut buffer = Buffer::new(
            &mut self.font_system,
            Metrics::new(self.font_size, self.line_height),
        );
        buffer.set_size(&mut self.font_system, Some(1.0), Some(1.0));
        self.panes.insert(
            session.clone(),
            PaneRender {
                grid: GridModel::new(dims),
                buffer,
                dirty: true,
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
    pub fn capture_to_png(&mut self, path: &Path) -> Result<(), crate::headless::CaptureError> {
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
                        selection: self
                            .selection
                            .as_ref()
                            .filter(|(sid, _)| *sid == vp.session)
                            .map(|(_, sel)| *sel),
                        split_preview: self
                            .split_preview
                            .as_ref()
                            .filter(|(sid, _, _)| *sid == vp.session)
                            .map(|(_, h, r)| (*h, *r)),
                    })
            })
            .collect();
        let shot = crate::headless::Shot {
            logical_w: self.config.width as f32 / sf,
            logical_h: self.config.height as f32 / sf,
            scale: sf,
            panes,
            tabs: self.tabs.clone(),
            tab_drag: self.tab_drag,
            help: self.help.clone(),
            help_title: self.help_title.clone(),
            about: self
                .about
                .clone()
                .map(|i| (i, self.about_glow, self.about_time)),
            settings: self.settings.clone(),
            backdrop: self.backdrop,
            image: self.image_rgba.clone(),
            image_fit: self.image_fit,
            fps_overlay: self.fps_overlay.clone(),
            bell_flash: self.bell_flash,
            font_size: self.font_size,
            font_family: self.family_name.clone(),
        };
        crate::headless::capture_reusing(
            &self.device,
            &self.queue,
            &mut self.font_system,
            &shot,
            path,
        )
    }

    /// `(alt_screen, mouse_reporting)` for a session's pane (from the latest delta),
    /// defaulting to `(false, false)`. Drives how the app treats the mouse wheel:
    /// history-scroll on the primary screen, wheel→arrows in a full-screen app.
    pub fn pane_modes(&self, session: &SessionId) -> PaneModes {
        self.panes
            .get(session)
            .map(|p| PaneModes {
                alt_screen: p.grid.alt_screen,
                mouse_reporting: p.grid.mouse_reporting,
                app_cursor: p.grid.app_cursor,
                mouse: p.grid.mouse,
            })
            .unwrap_or_default()
    }

    /// Which visible pane's scrollbar track contains logical `(x, y)`, if any — so
    /// the app can grab it (with priority over text selection).
    pub fn scrollbar_hit(&self, x: f32, y: f32) -> Option<SessionId> {
        for vp in &self.visible {
            let Some(p) = self.panes.get(&vp.session) else {
                continue;
            };
            if p.grid.alt_screen {
                continue;
            }
            if let Some((track, _)) = scrollbar_geometry(
                p.grid.display_offset,
                p.grid.history_len,
                p.grid.dims.screen_lines,
                vp.rect,
            ) {
                if x >= track[0]
                    && x <= track[0] + track[2]
                    && y >= track[1]
                    && y <= track[1] + track[3]
                {
                    return Some(vp.session.clone());
                }
            }
        }
        None
    }

    /// Map a mouse `y` (logical px) to a target display offset for `session`'s
    /// scrollbar — used for the thumb drag + track click. Clamped to the history.
    pub fn scroll_offset_at(&self, session: &SessionId, y: f32) -> Option<u16> {
        let vp = self.visible.iter().find(|v| &v.session == session)?;
        let p = self.panes.get(session)?;
        let (_, thumb) = scrollbar_geometry(
            p.grid.display_offset,
            p.grid.history_len,
            p.grid.dims.screen_lines,
            vp.rect,
        )?;
        let py = vp.rect.y as f32;
        let ph = vp.rect.height as f32;
        let thumb_h = thumb[3];
        let travel = (ph - thumb_h).max(1.0);
        let top_frac = ((y - py - thumb_h / 2.0) / travel).clamp(0.0, 1.0);
        Some(((1.0 - top_frac) * p.grid.history_len as f32).round() as u16)
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
            p.dirty = true;
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
                // Resizing the buffer invalidates its shaped layout.
                p.dirty = true;
            }
        }
        self.visible = visible;
        self.focused = Some(focused);
        self.tabs = tabs;
    }

    /// Set/clear the in-progress tab drag: `(dragged slot, cursor x in logical px)`.
    /// Drives the lifted, cursor-following tab in the strip.
    pub fn set_tab_drag(&mut self, drag: Option<(usize, f32)>) {
        self.tab_drag = drag;
        self.window.request_redraw();
    }

    /// Set/clear the Ctrl+Opt split drop-zone preview: `(hovered session,
    /// horizontal = side-by-side, ratio = existing pane fraction)`.
    pub fn set_split_preview(&mut self, preview: Option<(SessionId, bool, f32)>) {
        self.split_preview = preview;
        self.window.request_redraw();
    }

    /// Hit-test a click at logical `(x, y)` against the tab strip: a tab button, the
    /// trailing "+" (new tab) or "?" (help), or `None` (no strip / click below it).
    /// Must mirror the column math in [`build_tabs`].
    pub fn tab_hit(&self, x: f32, y: f32) -> Option<TabHit> {
        let strip_h = CELL_HEIGHT + 2.0 * PAD;
        if !(0.0..=strip_h).contains(&y) || x < 0.0 {
            return None;
        }
        let sf = self.window.scale_factor() as f32;
        let logical_w = self.config.width as f32 / sf;
        let cw = self.cell_w;
        let total_cols = (logical_w / cw).floor() as usize;
        let plus_cols = BTN_COLS.min(total_cols);
        let help_cols = BTN_COLS.min(total_cols.saturating_sub(plus_cols));
        let gear_cols = BTN_COLS.min(total_cols.saturating_sub(plus_cols + help_cols));
        let tab_cols = total_cols.saturating_sub(plus_cols + help_cols + gear_cols);
        let col = (x / cw).floor() as usize;
        if col >= total_cols {
            return None;
        }
        // Trailing controls (always present): … "+" "?" "⚙".
        if col >= tab_cols + plus_cols + help_cols {
            return Some(TabHit::Settings);
        }
        if col >= tab_cols + plus_cols {
            return Some(TabHit::Help);
        }
        if col >= tab_cols {
            return Some(TabHit::NewTab);
        }
        // Tab buttons only exist when there's more than one tab.
        let n = self.tabs.len();
        if n > 1 {
            let seg = tab_cols / n;
            let mut acc = 0;
            for i in 0..n {
                let width = if i == n - 1 { tab_cols - acc } else { seg };
                if col >= acc && col < acc + width {
                    return Some(TabHit::Tab(i));
                }
                acc += width;
            }
        }
        None
    }

    /// Which tab slot logical-x falls over, clamped to a valid tab index — used
    /// during a drag to pick the drop position. `None` when there are no tab
    /// buttons (≤1 tab). Mirrors the tab-area column math in [`build_tabs`].
    pub fn tab_slot_at(&self, x: f32) -> Option<usize> {
        let n = self.tabs.len();
        if n <= 1 {
            return None;
        }
        let sf = self.window.scale_factor() as f32;
        let logical_w = self.config.width as f32 / sf;
        let cw = self.cell_w;
        let total_cols = (logical_w / cw).floor() as usize;
        let plus_cols = BTN_COLS.min(total_cols);
        let help_cols = BTN_COLS.min(total_cols.saturating_sub(plus_cols));
        let gear_cols = BTN_COLS.min(total_cols.saturating_sub(plus_cols + help_cols));
        let tab_cols = total_cols.saturating_sub(plus_cols + help_cols + gear_cols);
        if tab_cols == 0 {
            return Some(0);
        }
        let col = ((x / cw).floor().max(0.0) as usize).min(tab_cols - 1);
        let seg = (tab_cols / n).max(1);
        Some((col / seg).min(n - 1))
    }

    /// Show the cheat-sheet overlay with these `(key, description)` rows, or hide
    /// it with `None`. The next `render` draws (or stops drawing) the modal.
    /// Override the help panel's `(title, hint)`, or reset to the shortcuts
    /// default with `None`. Set before `set_help` when reusing the panel (e.g.
    /// a close confirmation).
    pub fn set_help_title(&mut self, title: Option<(String, String)>) {
        self.help_title = title;
    }

    pub fn set_help(&mut self, lines: Option<Vec<(String, String)>>) {
        self.help = lines;
        self.window.request_redraw();
    }

    /// Whether the help overlay is currently shown.
    pub fn help_visible(&self) -> bool {
        self.help.is_some()
    }

    /// Show the About overlay with this content, or hide it with `None`.
    pub fn set_about(&mut self, info: Option<AboutInfo>) {
        if info.is_none() {
            self.about_links.clear();
        }
        self.about = info;
        self.window.request_redraw();
    }

    /// Whether the About overlay is shown.
    pub fn about_visible(&self) -> bool {
        self.about.is_some()
    }

    /// If the About overlay is open and logical point `(x, y)` is over a link button,
    /// the target URL. Lets the app open Docs/GitHub instead of dismissing.
    pub fn about_link_at(&self, x: f32, y: f32) -> Option<&str> {
        self.about.as_ref()?;
        self.about_links.iter().find_map(|([rx, ry, rw, rh], url)| {
            (x >= *rx && x < rx + rw && y >= *ry && y < ry + rh).then_some(url.as_str())
        })
    }

    /// Update the About overlay's animation inputs each frame: glow intensity
    /// (`[0,1]`) and elapsed seconds since it opened (drives the ember sparks).
    pub fn set_about_anim(&mut self, glow: f32, t: f32) {
        self.about_glow = glow.clamp(0.0, 1.0);
        self.about_time = t;
    }

    /// Show the Settings overlay with these `(label, value)` rows and the selected
    /// row index, or hide it with `None`.
    pub fn set_settings(&mut self, view: Option<(Vec<(String, String)>, usize)>) {
        self.settings = view;
        self.window.request_redraw();
    }

    /// Whether the Settings overlay is shown.
    pub fn settings_visible(&self) -> bool {
        self.settings.is_some()
    }

    /// Set the campfire backdrop + ember-spark parameters. All off by
    /// default; the app updates this each frame (and should only drive continuous
    /// redraws while `sparks` is on). Requests a redraw.
    pub fn set_backdrop(&mut self, params: BackdropParams) {
        self.backdrop = params;
        self.window.request_redraw();
    }

    /// Whether an animated backdrop effect (sparks) is active — the app uses this
    /// to decide whether to drive continuous redraws.
    pub fn backdrop_animating(&self) -> bool {
        self.backdrop.sparks
    }

    /// Set or clear the active text selection (and the session it belongs to).
    /// Requests a redraw so the highlight updates.
    pub fn set_selection(&mut self, selection: Option<(SessionId, Selection)>) {
        self.selection = selection;
        self.window.request_redraw();
    }

    /// Set or clear the FPS/frame-time debug readout text (bottom-right).
    pub fn set_fps_overlay(&mut self, text: Option<String>) {
        self.fps_overlay = text;
    }

    /// Set the visual-bell flash intensity (`0..1`) — a warm amber wash over the
    /// panes. The app drives this each frame, decaying it to 0 after a BEL.
    pub fn set_bell_flash(&mut self, intensity: f32) {
        self.bell_flash = intensity.clamp(0.0, 1.0);
        self.window.request_redraw();
    }

    /// The currently selected text, if any (read from the owning pane's grid).
    pub fn selected_text(&self) -> Option<String> {
        let (sid, sel) = self.selection.as_ref()?;
        let p = self.panes.get(sid)?;
        let text = sel.text(&p.grid);
        (!text.is_empty()).then_some(text)
    }

    /// Set (or clear with `None`) the backdrop image and its fit mode.
    /// When an image is set it draws behind the cells *in place of* the gradient;
    /// the scrim still applies for legibility. The decoded RGBA is kept in RAM so
    /// on-screen captures can reproduce it headlessly. Requests a redraw.
    pub fn set_backdrop_image(&mut self, img: Option<(Vec<u8>, u32, u32)>, fit: ImageFit) {
        match &img {
            Some((rgba, w, h)) => self
                .image
                .set_image(&self.device, &self.queue, rgba, *w, *h),
            None => self.image.clear(),
        }
        self.image_rgba = img;
        self.image_fit = fit;
        self.window.request_redraw();
    }

    /// Build the help overlay using this renderer's buffer (wrapper over the shared
    /// [`build_help`] so the windowed + headless paths render it identically).
    fn build_help_quads(&mut self, sf: f32, rects: &mut Vec<([f32; 4], [f32; 4])>) -> Rect {
        let logical_w = self.config.width as f32 / sf;
        let logical_h = self.config.height as f32 / sf;
        let rows = self.help.clone().unwrap_or_default();
        let (title, hint) = self
            .help_title
            .clone()
            .unwrap_or_else(|| ("Keyboard Shortcuts".into(), "any key to close".into()));
        build_help(
            &mut self.font_system,
            &mut self.help_buffer,
            &title,
            &hint,
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
        let mut spark_rects: Vec<([f32; 4], [f32; 4])> = Vec::new();
        // Index into `rects` where the additive spark pass is interleaved:
        // everything before is backdrop (gradient/scrim), everything after is
        // cells + chrome, so opaque content covers the embers. 0 = no backdrop
        // (overlays): all quads draw after the (empty) spark pass.
        let mut spark_layer: usize = 0;
        let mut areas: Vec<TextArea> = Vec::new();
        // Whether to draw the backdrop image this frame (pane view only — never
        // over the Settings/About/help overlays).
        let mut draw_image = false;

        if let Some((rows, selected)) = self.settings.clone() {
            // Modal Settings overlay.
            let logical_w = self.config.width as f32 / sf;
            let logical_h = self.config.height as f32 / sf;
            let (left, top) = build_settings(
                &mut self.font_system,
                &mut self.settings_buffer,
                &rows,
                selected,
                self.cell_w,
                logical_w,
                logical_h,
                sf,
                &mut rects,
            );
            areas.push(TextArea {
                buffer: &self.settings_buffer,
                left: left * sf,
                top: top * sf,
                scale: sf,
                bounds: full_bounds,
                default_color: Color::rgb(FG.r, FG.g, FG.b),
                custom_glyphs: &[],
            });
        } else if let Some(info) = self.about.clone() {
            // Modal About page: scrim + animated ember glow + wordmark + info.
            let logical_w = self.config.width as f32 / sf;
            let logical_h = self.config.height as f32 / sf;
            let layout = build_about(
                &mut self.font_system,
                &mut self.about_title,
                &mut self.about_body,
                &info,
                self.about_glow,
                self.about_time,
                self.cell_w,
                logical_w,
                logical_h,
                sf,
                &mut rects,
            );
            // Pair each link's click rect with its URL for hit-testing.
            self.about_links = layout
                .link_rects
                .iter()
                .zip(info.links.iter())
                .map(|(r, (_label, url))| (*r, url.clone()))
                .collect();
            areas.push(TextArea {
                buffer: &self.about_title,
                left: layout.title_left * sf,
                top: layout.title_top * sf,
                scale: sf,
                bounds: full_bounds,
                default_color: Color::rgb(AMBER.r, AMBER.g, AMBER.b),
                custom_glyphs: &[],
            });
            areas.push(TextArea {
                buffer: &self.about_body,
                left: 0.0,
                top: layout.body_top * sf,
                scale: sf,
                bounds: full_bounds,
                default_color: Color::rgb(FG.r, FG.g, FG.b),
                custom_glyphs: &[],
            });
        } else if self.help.is_some() {
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
            // Campfire backdrop (image or gradient, + scrim) behind the cells —
            // drawn first so empty cells (no bg quad) let it show through. A
            // backdrop image is the base layer (drawn in the render pass below), so
            // suppress the gradient bands when one is set; the scrim still applies.
            let logical_w = self.config.width as f32 / sf;
            let logical_h = self.config.height as f32 / sf;
            draw_image = self.image.has_image();
            let mut bp = self.backdrop;
            if draw_image {
                bp.gradient = false;
            }
            push_backdrop(&mut rects, &bp, logical_w, logical_h, sf);
            spark_layer = rects.len();
            if draw_image {
                self.image.prepare(
                    &self.device,
                    &self.queue,
                    (self.config.width as f32, self.config.height as f32),
                    self.image_fit,
                );
            }
            if self.backdrop.sparks {
                spark_rects = spark_quads(
                    self.backdrop.density,
                    self.backdrop.time,
                    logical_w,
                    logical_h,
                    sf,
                );
            }
            // Pass 1: (re)shape only panes whose grid/size changed since last frame.
            // Buffers persist in `PaneRender`, so unchanged panes reuse their shaping
            // — the TextArea below just references the existing buffer.
            let (size, lh) = (self.font_size, self.line_height);
            let family = crate::paint::family_of(self.family_name.as_deref());
            for vp in &self.visible {
                if let Some(p) = self.panes.get_mut(&vp.session) {
                    if p.dirty {
                        shape_grid(
                            &mut self.font_system,
                            &mut p.buffer,
                            &p.grid,
                            size,
                            lh,
                            family,
                        );
                        p.dirty = false;
                    }
                }
            }
            // Pass 2: bg fills, cursor, focus border, tab strip (logical px * sf).
            let cw = self.cell_w;
            let ch = self.line_height;
            let split = self.visible.len() > 1;
            for vp in &self.visible {
                if let Some(p) = self.panes.get(&vp.session) {
                    let focused = self.focused.as_ref() == Some(&vp.session);
                    grid_quads(&p.grid, vp.rect, cw, ch, sf, focused, split, &mut rects);
                    if let Some((sid, sel)) = &self.selection {
                        if *sid == vp.session {
                            selection_quads(&p.grid, sel, vp.rect, cw, ch, sf, &mut rects);
                        }
                    }
                    // Ctrl+Opt split drop-zone preview over the hovered pane.
                    if let Some((psid, horizontal, ratio)) = &self.split_preview {
                        if *psid == vp.session {
                            split_preview(vp.rect, *horizontal, *ratio, sf, &mut rects);
                        }
                    }
                    // Scrollbar (right edge): shown whenever the pane has history and
                    // isn't on the alt screen (no scrollback there).
                    if !p.grid.alt_screen {
                        scrollbar(
                            p.grid.display_offset,
                            p.grid.history_len,
                            p.grid.dims.screen_lines,
                            vp.rect,
                            sf,
                            &mut rects,
                        );
                    }
                }
            }
            let logical_w = self.config.width as f32 / sf;
            build_tabs(
                &mut self.font_system,
                &mut self.chrome,
                &self.tabs,
                self.tab_drag,
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
            // The strip (with +/?/⚙ controls) is always drawn, so always show its text.
            areas.push(TextArea {
                buffer: &self.chrome,
                left: 0.0,
                top: PAD * sf,
                scale: sf,
                bounds: full_bounds,
                default_color: Color::rgb(FG.r, FG.g, FG.b),
                custom_glyphs: &[],
            });
            // FPS/frame-time debug readout, on top of the panes (bottom-right).
            if let Some(text) = self.fps_overlay.clone() {
                let (left, top) = build_fps(
                    &mut self.font_system,
                    &mut self.fps_buffer,
                    &text,
                    cw,
                    logical_w,
                    logical_h,
                    sf,
                    &mut rects,
                );
                areas.push(TextArea {
                    buffer: &self.fps_buffer,
                    left: left * sf,
                    top: top * sf,
                    scale: sf,
                    bounds: full_bounds,
                    default_color: Color::rgb(AMBER.r, AMBER.g, AMBER.b),
                    custom_glyphs: &[],
                });
            }
            // Visual-bell flash: a warm amber wash over everything (under the text).
            bell_wash(&mut rects, self.bell_flash, logical_w, logical_h, sf);
        }

        self.quads.prepare(
            &self.device,
            &self.queue,
            (self.config.width as f32, self.config.height as f32),
            &rects,
        );
        self.sparks.prepare(
            &self.device,
            &self.queue,
            (self.config.width as f32, self.config.height as f32),
            &spark_rects,
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
            // Backdrop image is the base layer: drawn before the gradient/scrim
            // quads (which alpha-darken it) and the cells.
            if draw_image {
                self.image.draw(&mut pass);
            }
            // Backdrop quads → sparks (additive) → cells + chrome → text. The
            // embers glow over the gradient but sit behind opaque cell bgs, the
            // selection, and the tab strip.
            let split = spark_layer as u32;
            self.quads.draw_range(&mut pass, 0..split);
            self.sparks.draw(&mut pass);
            self.quads.draw_range(&mut pass, split..u32::MAX);
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
