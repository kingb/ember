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
use std::time::{Duration, Instant};

use ember_core::{GridDelta, GridDims, Rect, Rgb, SessionId, SettingsRowView};
use glyphon::{
    Buffer, Cache, Color, CustomGlyph, Family, FontSystem, Metrics, Resolution, SwashCache,
    TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
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
    BTN_COLS, CLOSE_COLS, bell_wash, build_about, build_confirm, build_fps, build_help,
    build_settings, build_tabs, debug_emit, grid_quads, hold_ring_quads, measure_cell_width,
    morph_quads, push_backdrop, scrollbar, scrollbar_geometry, selection_quads, shape_grid,
    spark_quads, split_preview,
};
use crate::selection::AnchoredSelection;

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

/// Suck-in/pour-out morph state (v0.4.0): `(rect, grab point, t01, inward)`,
/// all logical px/`0..1`, this window's own space — see
/// [`Renderer::set_morph`]'s doc for what each field means.
pub type MorphState = ([f32; 4], (f32, f32), f32, bool);

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

/// A blocking confirm modal: a title, a one-line message, and two buttons.
/// `focused` is the highlighted/default button (0 = cancel, 1 = confirm).
#[derive(Clone, Debug)]
pub struct ConfirmView {
    pub title: String,
    pub message: String,
    pub cancel_label: String,
    pub confirm_label: String,
    pub focused: usize,
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
    /// Seconds between animation ticks (`1 / ember_fps`). Sizes the sparks'
    /// velocity-stretched trail segments so consecutive frames' streaks
    /// connect end-to-end at any configured frame rate.
    pub frame_dt: f32,
}

impl Default for BackdropParams {
    fn default() -> Self {
        Self {
            gradient: false,
            scrim: 0.0,
            sparks: false,
            density: 1.0,
            time: 0.0,
            frame_dt: 1.0 / 15.0,
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
    /// The "✕" close zone of the hovered tab at this index (left edge).
    CloseTab(usize),
    /// The trailing "+" button (open a new tab).
    NewTab,
    /// The trailing "?" button (toggle the shortcuts overlay).
    Help,
    /// The trailing "⚙" button (toggle the Settings overlay).
    Settings,
}

/// Which strip slot a live-drag hover falls over (finding #2/#3's
/// spring-loaded tab-select and ghost coexistence): an existing tab's own
/// chip, or the trailing ghost/append segment when this window is currently
/// showing one ([`Renderer::ghost_active`]). See [`Renderer::
/// tab_slot_or_ghost_at`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StripSlot {
    /// Hovering real tab `usize`'s own chip.
    Tab(usize),
    /// Hovering the reserved ghost/append segment (only possible while a
    /// ghost is showing — see [`Renderer::ghost_active`]).
    Ghost,
}

/// Resolve a tab-area column to a hit, mirroring the equal-width slot math in
/// [`build_tabs`]. `hovered` gates the left `CLOSE_COLS` "✕" close zone (only the
/// hovered tab exposes it). Pure so the geometry is unit-testable without a GPU.
fn tab_col_hit(n: usize, tab_cols: usize, hovered: Option<usize>, col: usize) -> Option<TabHit> {
    if n == 0 {
        return None;
    }
    let seg = tab_cols / n;
    let mut acc = 0;
    for i in 0..n {
        let width = if i == n - 1 { tab_cols - acc } else { seg };
        if col >= acc && col < acc + width {
            if hovered == Some(i) && width > CLOSE_COLS && col - acc < CLOSE_COLS {
                return Some(TabHit::CloseTab(i));
            }
            return Some(TabHit::Tab(i));
        }
        acc += width;
    }
    None
}

/// Resolve a tab-area column to a [`StripSlot`], mirroring [`build_tabs`]'s
/// `ghost_n` segmenting (real tabs compress into `n + 1` equal segments,
/// ghost last, while `with_ghost` — see that fn's doc) — unlike
/// [`tab_col_hit`]/[`Renderer::tab_slot_at`] (drop-position math, always
/// resolves to a REAL tab, ghost-oblivious by design), this is what tells a
/// live drag hover whether the cursor is over a chip it should
/// spring-load-select, or the trailing ghost region it shouldn't (finding
/// #2). Pure, unit-testable independent of a live `Renderer`.
fn tab_or_ghost_col_hit(
    n: usize,
    with_ghost: bool,
    tab_cols: usize,
    col: usize,
) -> Option<StripSlot> {
    if n == 0 {
        return None;
    }
    let ghost_n = n + with_ghost as usize;
    let seg = (tab_cols / ghost_n).max(1);
    let mut acc = 0usize;
    for i in 0..n {
        let width = if i == n - 1 && !with_ghost {
            tab_cols - acc
        } else {
            seg
        };
        if col < acc + width {
            return Some(StripSlot::Tab(i));
        }
        acc += width;
    }
    if with_ghost {
        Some(StripSlot::Ghost)
    } else {
        // Ghost-oblivious callers can overshoot the last real segment by
        // integer-division slack; clamp onto the last tab rather than
        // reporting nothing (mirrors `tab_slot_at`'s own clamp).
        Some(StripSlot::Tab(n - 1))
    }
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
/// See [`Renderer::retained`]: the encoder inputs produced by a full scene
/// build, reused verbatim by animation-only frames.
#[derive(Default, Clone, Copy)]
struct RetainedScene {
    draw_image: bool,
    spark_layer: usize,
    rounded_pre_confirm: u32,
}

struct PaneRender {
    grid: GridModel,
    buffer: Buffer,
    /// The shaped `buffer` is stale and must be re-shaped before the next draw.
    /// Set on any applied delta or buffer resize; cleared after reshaping. Lets the
    /// per-frame render skip glyphon reshaping for panes that didn't change — the
    /// big CPU win when sparks drive 60fps redraws over an idle grid.
    dirty: bool,
    /// Clickable-link spans for the current grid, rebuilt when `dirty`.
    links: Vec<crate::grid_model::LinkSpan>,
}

/// What [`Renderer::render`] did with the frame — tells the app whether (and
/// whether NOT) to schedule another redraw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderOutcome {
    /// Frame presented normally.
    Presented,
    /// The surface was stale (resize / device loss) and has been reconfigured;
    /// this frame never presented. Redraw now, or the window shows stale
    /// pixels until the next input.
    Retry,
    /// No drawable: the window is occluded or the display is asleep. Drawing
    /// is pointless and *retrying in a loop is the OOM spin*: each
    /// attempt burns full frame prep + staging allocations at PTY-event rate.
    /// Do NOT auto-retry — the repaint arrives with `Occluded(false)` or the
    /// next content change once visible.
    Starved,
}

/// Rate-limits render attempts while the surface is starved of drawables
/// (occluded window / asleep display). The app already gates redraw requests
/// on winit's `Occluded` events; this is the renderer-side backstop for
/// occlusions winit never reports (e.g. display sleep → `Timeout`). Without
/// it, content-driven redraws re-enter full frame prep at PTY rate — the
///  spin that leaked ~3,500 GPU allocations/s and OOM'd the machine.
/// A successful present clears the gate, so a revealed window never waits.
///
/// The holdoff backs off exponentially the longer starvation persists (sleep
/// balloon postmortem: a fixed 4/s retry forever is "bounded" per
/// attempt but not per NIGHT — display sleep can starve a window for eight-plus
/// hours, during which winit's real `Occluded`/`Focused` events still clear
/// this gate INSTANTLY via `surface_revealed` — they never go through
/// `should_attempt` at all — so backing off the blind poll path costs nothing
/// on a real reveal and only trims the total attempt count across a long,
/// otherwise-invisible stretch.
struct StarveGate {
    starved_at: Option<Instant>,
    /// Consecutive starved attempts since the last `clear()` — drives the
    /// exponential backoff. Reset to 0 on every `clear()`.
    consecutive: u32,
}

impl StarveGate {
    /// Starting attempt rate while starved: one per 250ms (4/s) — unchanged
    /// from the original fix, so a single transient starve (e.g. a startup
    /// burst) still self-heals just as fast as before.
    const HOLDOFF: Duration = Duration::from_millis(250);
    /// Ceiling the backoff ramps to (250ms * 2^5 = 8s) and holds there —
    /// reached on the 6th consecutive starve, roughly 8 seconds into an
    /// unbroken stretch.
    const MAX_BACKOFF_SHIFT: u32 = 5;

    fn new() -> Self {
        Self {
            starved_at: None,
            consecutive: 0,
        }
    }

    /// The current holdoff: `HOLDOFF` for the first starve, doubling each
    /// consecutive one after that, up to the cap.
    fn current_holdoff(&self) -> Duration {
        let shift = self
            .consecutive
            .saturating_sub(1)
            .min(Self::MAX_BACKOFF_SHIFT);
        Self::HOLDOFF * (1u32 << shift)
    }

    /// Whether a render attempt is allowed at `now`.
    fn should_attempt(&self, now: Instant) -> bool {
        self.starved_at
            .is_none_or(|t| now.duration_since(t) >= self.current_holdoff())
    }

    /// Record a starved attempt at `now`; holds off retries for the current
    /// (growing) backoff and widens it for next time. Logs to `EMBER_DEBUG`
    /// on the first starve and every 60th thereafter (a no-op when the var is
    /// unset), plus an ALWAYS-ON stderr tripwire every 1,000th — a streak
    /// that long is hours of starvation, and the sleep-balloon incident log
    /// was silent precisely because everything here was opt-in. With the
    /// backoff at its 8s cap, an 8-hour sleep is ~3,600 attempts total
    /// (instead of the old ~115,000), so the tripwire fires at most a few
    /// times per night.
    fn starve(&mut self, now: Instant) {
        self.starved_at = Some(now);
        self.consecutive = self.consecutive.saturating_add(1);
        if self.consecutive == 1 || self.consecutive % 60 == 0 {
            debug_emit(&format!(
                "[ember] starve-gate: {} consecutive starved attempts, holdoff now {:?}",
                self.consecutive,
                self.current_holdoff()
            ));
        }
        if self.consecutive % 1000 == 0 {
            eprintln!(
                "[ember] starve-gate: {} consecutive starved render attempts (holdoff {:?}) — \
                 surface has produced no drawable for a long stretch",
                self.consecutive,
                self.current_holdoff()
            );
        }
    }

    /// A drawable came through — stop gating and reset the backoff.
    fn clear(&mut self) {
        self.starved_at = None;
        self.consecutive = 0;
    }
}

/// Bounds the reconfigure-per-attempt loop on a persistently Lost/Outdated
/// surface (sleep-balloon postmortem). A Lost/Outdated acquire reconfigures
/// the surface — which allocates a fresh swapchain (~tens of MB at Retina
/// sizes) — and returns `Retry`, which the app answers with an immediate
/// `request_redraw`. On a healthy surface that settles in one round trip
/// (resize race), but while the GPU is asleep (display sleep / dark wake) the
/// surface can stay Lost for hours, turning that round trip into an unbounded
/// event-loop-speed spin: one swapchain allocation per iteration, faster than
/// Metal reclaims the abandoned ones. This counter lets the first couple of
/// losses keep the fast immediate-retry path, then hands persistent loss to
/// the [`StarveGate`] cadence, where reclamation trivially keeps up.
struct LostSurfaceBound {
    /// Consecutive Lost/Outdated/Validation acquires since the last
    /// successful acquire (or reveal/resize).
    consecutive: u32,
}

impl LostSurfaceBound {
    /// How many consecutive losses still earn an immediate retry. One covers
    /// the benign resize race; the second is margin for a reveal that lands
    /// mid-reconfigure. The third consecutive loss means the surface is not
    /// coming back on its own — stop spinning.
    const MAX_IMMEDIATE: u32 = 2;

    fn new() -> Self {
        Self { consecutive: 0 }
    }

    /// Record one more lost acquire. `true` = an immediate retry is still
    /// allowed; `false` = fall back to the starve cadence.
    fn record(&mut self) -> bool {
        self.consecutive = self.consecutive.saturating_add(1);
        self.consecutive <= Self::MAX_IMMEDIATE
    }

    /// The surface produced a drawable (or was rebuilt for a reveal/resize):
    /// the next loss is a fresh incident, not a continuation.
    fn clear(&mut self) {
        self.consecutive = 0;
    }
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
    /// Second text pass for the confirm modal's dialog text, drawn *after* the
    /// opaque panel quad so pane glyphs (htop etc.) can't bleed through it.
    overlay_text: TextRenderer,
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
    /// Tab the cursor is currently over, or `None`. Drives the hover highlight +
    /// the "✕" close affordance; also gates the close zone in [`Self::tab_hit`].
    hovered_tab: Option<usize>,
    /// The link under the pointer, for the brighter underline: `(pane, link)`.
    hovered_link: Option<(SessionId, u32)>,
    /// Whether anything that affects the STATIC scene (everything except the
    /// spark animation clock) changed since the last full build. When false
    /// and sparks are animating, `render` takes the animation-only fast path:
    /// it skips the whole scene rebuild + quad/text uploads and re-encodes
    /// with fresh spark quads over the retained buffers. Every scene mutator
    /// sets this; `set_backdrop` only when a non-`time` field changed.
    scene_dirty: bool,
    /// The per-frame values the encoder needs from the last full build, kept
    /// so animation-only frames can encode without rebuilding.
    retained: RetainedScene,
    /// Glyph buffer for the tab strip.
    chrome: Buffer,
    /// Last-shaped tab-strip inputs; skips per-frame re-shaping.
    tabs_cache: crate::paint::TabsCache,
    /// Glyph buffer for the hovered tab's "✕", positioned in the pill's left cap.
    close_buffer: Buffer,
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
    /// When `Some`, the Settings overlay is shown: `(resolved rows, selected)`.
    settings: Option<(Vec<SettingsRowView>, usize)>,
    /// Glyph buffer for the Settings overlay.
    settings_buffer: Buffer,
    /// Cell width for the Settings panel's OWN text, measured once at its
    /// fixed `FONT_SIZE`/`Family::Monospace` — never the live terminal
    /// `cell_w`. The panel's label/value column alignment used to reuse
    /// `self.cell_w` (the *terminal's* cell width, which now changes live via
    /// the Font size row), so zooming to an extreme size made `inner_cols`
    /// wildly wrong for text actually shaped at the panel's own fixed size,
    /// producing a huge padding-space run that wrapped the value onto its own
    /// line. Fixed by giving the panel a cell width of its own.
    settings_cw: f32,
    /// What the settings buffer was last shaped for: `(rows, selected,
    /// surface w, surface h, scale-factor bits)`. Text shaping is the
    /// expensive part of the overlay (hundreds of runs through cosmic-text's
    /// fallback machinery) and the modal redraws on every frame
    /// while open — so shape only when the content or geometry actually
    /// changed, like `TabsCache` does for the tab strip. Quads
    /// (scrim/panel/highlight) still rebuild every frame; they're cheap.
    settings_shaped: Option<(Vec<SettingsRowView>, usize, u32, u32, u32)>,
    /// When `Some`, a blocking confirm modal is shown.
    confirm: Option<ConfirmView>,
    /// Buffers for the confirm modal's message + two button labels.
    confirm_title: Buffer,
    confirm_msg: Buffer,
    confirm_cancel: Buffer,
    confirm_ok: Buffer,
    /// The confirm modal's `[cancel, confirm]` button rects (logical px), for
    /// hit-testing clicks. Empty when the modal is hidden.
    confirm_buttons: Vec<([f32; 4], usize)>,
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
    selection: Option<(SessionId, AnchoredSelection)>,
    /// Split drop-zone preview: `(hovered session, horizontal, ratio, before)`.
    /// `before` is `true` when the NEW pane lands on the left/top sibling
    /// (surface-drag `DropZone::Edge`'s `before` bit — release 2); the
    /// Ctrl+Opt manual split preview always passes `before: false` (that
    /// gesture only ever appends the new pane on the far side).
    split_preview: Option<(SessionId, bool, f32, bool)>,
    /// Hold-to-wisp ring (v1.1): `(logical x, logical y, progress 0..1)` of
    /// the cursor a hold is armed/sweeping at, this window's own space.
    /// `None` while no hold is live. See [`crate::paint::hold_ring_quads`].
    hold_ring: Option<(f32, f32, f32)>,
    /// The incoming-drag ghost tab (v0.4.0): `(label, since)` — `label` is
    /// the carried surface's title, or the "＋" fallback (see
    /// `Self::set_ghost_tab`'s doc); `since` anchors the procedural flicker
    /// clock, kept across repeated `set_ghost_tab(Some(_))` calls with the
    /// SAME label (a live hover re-sets this every motion tick) so the
    /// shimmer doesn't restart every frame. `None` while no cross-window
    /// drag is hovering this window's strip.
    ghost_tab: Option<(String, Instant)>,
    /// The suck-in/pour-out morph (v0.4.0): `(rect, grab point, t01, inward)`
    /// — see [`Self::set_morph`]'s doc. `None` while no morph is playing (the
    /// overwhelmingly common case — a self-terminating ~150-200ms animation,
    /// not a persistent drag cue).
    morph: Option<MorphState>,
    /// FPS/frame-time debug readout text (bottom-right), or `None` when hidden.
    fps_overlay: Option<String>,
    search_bar: Option<String>,
    /// Glyph buffer for the FPS overlay.
    fps_buffer: Buffer,
    /// Visual-bell flash intensity (`0..1`); a warm amber wash over the panes that
    /// the app decays to 0 after a BEL. `0.0` = no flash.
    bell_flash: f32,
    /// Throttles render attempts while the surface has no drawable.
    starve_gate: StarveGate,
    /// Bounds immediate retries while the surface is persistently lost.
    lost_bound: LostSurfaceBound,
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
        // On Metal this branch never wins — macOS caps are [Fifo, Immediate]
        // (verified via the startup debug log) — so macOS always runs Fifo and
        // relies on drawable backpressure for pacing. The Mailbox preference is
        // kept for the Linux/Vulkan build, where it exists and gives lower
        // latency than Fifo without Immediate's tearing. Do NOT "fix" this by
        // preferring Immediate on Metal: unpaced presentation is how the
        // class of runaway feeds itself.
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
        // Which mode we actually got matters when debugging pacing/leak issues
        // (Metal offers Fifo+Immediate only — the Mailbox branch never wins there).
        debug_emit(&format!(
            "[ember] surface: present_mode={present_mode:?} caps={:?} latency=2",
            caps.present_modes
        ));

        let mut font_system = crate::paint::new_font_system();
        let swash_cache = SwashCache::new();
        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, format);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, MultisampleState::default(), None);
        let overlay_text =
            TextRenderer::new(&mut atlas, &device, MultisampleState::default(), None);

        let mut chrome = Buffer::new(&mut font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
        chrome.set_size(&mut font_system, Some(width as f32), Some(LINE_HEIGHT));
        let close_buffer = Buffer::new(&mut font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
        let help_buffer = Buffer::new(&mut font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
        let about_title = Buffer::new(
            &mut font_system,
            Metrics::new(ABOUT_TITLE_SIZE, ABOUT_TITLE_LINE),
        );
        let about_body = Buffer::new(&mut font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
        let settings_buffer = Buffer::new(&mut font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
        // The panel's own text always shapes at FONT_SIZE/Monospace, never the
        // live terminal font — so its cell width is measured once here and
        // never revisited, unlike `cell_w` below.
        let settings_cw = measure_cell_width(&mut font_system, FONT_SIZE, Family::Monospace);
        let confirm_title = Buffer::new(&mut font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
        let confirm_msg = Buffer::new(&mut font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
        let confirm_cancel = Buffer::new(&mut font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
        let confirm_ok = Buffer::new(&mut font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
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
            overlay_text,
            quads,
            panes: HashMap::new(),
            visible: Vec::new(),
            focused: None,
            tabs: Vec::new(),
            tab_drag: None,
            hovered_tab: None,
            hovered_link: None,
            scene_dirty: true,
            retained: RetainedScene::default(),
            chrome,
            tabs_cache: crate::paint::TabsCache::default(),
            close_buffer,
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
            settings_cw,
            settings_shaped: None,
            confirm: None,
            confirm_title,
            confirm_msg,
            confirm_cancel,
            confirm_ok,
            confirm_buttons: Vec::new(),
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
            hold_ring: None,
            ghost_tab: None,
            morph: None,
            fps_overlay: None,
            search_bar: None,
            fps_buffer,
            bell_flash: 0.0,
            starve_gate: StarveGate::new(),
            lost_bound: LostSurfaceBound::new(),
            window,
        }
    }

    pub fn present_mode(&self) -> PresentMode {
        self.config.present_mode
    }

    /// The window just became visible again (winit `Occluded(false)`): lift the
    /// starve throttle so the reveal repaint isn't delayed by the holdoff.
    pub fn surface_revealed(&mut self) {
        self.scene_dirty = true;
        self.starve_gate.clear();
        self.lost_bound.clear();
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
        self.scene_dirty = true;
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

    /// Set the terminal font family (live, from the Settings Cycle row).
    /// `None` means the platform monospace default. Mirrors `set_font_size`:
    /// re-measures the cell advance (a different family can have a different
    /// average glyph width at the same point size) and marks every pane
    /// dirty so it re-shapes with the new family next frame. Returns whether
    /// it changed — the caller must re-layout, since the cell width may have.
    pub fn set_family(&mut self, family: Option<String>) -> bool {
        self.scene_dirty = true;
        if family == self.family_name {
            return false;
        }
        self.family_name = family;
        self.cell_w = measure_cell_width(
            &mut self.font_system,
            self.font_size,
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
        self.scene_dirty = true;
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
                links: Vec::new(),
            },
        );
    }

    /// Drop a session's grid (its shell exited or its pane was closed).
    pub fn remove_pane(&mut self, session: &SessionId) {
        self.scene_dirty = true;
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
                            .and_then(|(_, sel)| sel.project(&p.grid)),
                        split_preview: self
                            .split_preview
                            .as_ref()
                            .filter(|(sid, _, _, _)| *sid == vp.session)
                            .map(|(_, h, r, before)| (*h, *r, *before)),
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
            hovered_tab: self.hovered_tab,
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
            search_bar: self.search_bar.clone(),
            bell_flash: self.bell_flash,
            font_size: self.font_size,
            font_family: self.family_name.clone(),
            confirm: self.confirm.clone(),
            hold_ring: self.hold_ring,
            ghost_tab: self
                .ghost_tab
                .as_ref()
                .map(|(label, since)| (label.clone(), since.elapsed().as_secs_f32())),
            morph: self.morph,
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

    /// The live `GridModel` behind a session's pane, if registered — the read
    /// half of `apply_delta`. Multi-window replay (opening/moving a pane into a
    /// new window) sources a `GridModel::snapshot_delta()` from here rather than
    /// re-deriving one from `pane_snapshot`'s flattened text, so styles/cursor/
    /// scrollback-view/marks all carry over exactly, not just the glyphs.
    pub fn grid(&self, session: &SessionId) -> Option<&GridModel> {
        self.panes.get(session).map(|p| &p.grid)
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
        self.scene_dirty = true;
        if let Some(p) = self.panes.get_mut(session) {
            // Reshape (the expensive part) only when glyph content actually
            // changed. Cursor-only, mode-only, marks-only, and scroll-offset
            // deltas — which the projection ships on any state change — leave
            // the glyphs untouched; the cursor/overlay quads rebuild every frame
            // regardless, so those deltas need a redraw but not a reshape.
            let content_changed = delta.reset || !delta.cells.is_empty();
            p.grid.apply(delta);
            if content_changed {
                p.dirty = true;
            }
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
        self.scene_dirty = true;
        self.tab_drag = drag;
        self.window.request_redraw();
    }

    /// Set/clear the hovered tab. Redraws only on a real change so per-pixel
    /// cursor motion inside one tab doesn't churn frames.
    pub fn set_hovered_tab(&mut self, hovered: Option<usize>) {
        self.scene_dirty = true;
        if self.hovered_tab != hovered {
            self.hovered_tab = hovered;
            self.window.request_redraw();
        }
    }

    /// Highlight (or clear) the hovered link; redraws on change.
    pub fn set_hovered_link(&mut self, hovered: Option<(SessionId, u32)>) {
        if self.hovered_link != hovered {
            self.scene_dirty = true;
            self.hovered_link = hovered;
            self.window.request_redraw();
        }
    }

    /// The link at a pane cell, if any: `(link id, url)`.
    pub fn link_at(&self, session: &SessionId, row: u16, col: u16) -> Option<(u32, &str)> {
        self.panes
            .get(session)?
            .links
            .iter()
            .find(|s| s.row == row && s.cols.contains(&col))
            .map(|s| (s.link_id, s.url.as_str()))
    }

    /// Set/clear the split drop-zone preview: `(hovered session, horizontal =
    /// side-by-side, ratio = existing pane fraction, before = new pane on
    /// the left/top sibling)`.
    pub fn set_split_preview(&mut self, preview: Option<(SessionId, bool, f32, bool)>) {
        self.scene_dirty = true;
        self.split_preview = preview;
        self.window.request_redraw();
    }

    /// Set/clear the hold-to-wisp ring (v1.1): logical `(x, y)` cursor
    /// position + sweep progress (0..1). `None` while no hold is
    /// armed/sweeping — see [`crate::paint::hold_ring_quads`] for the quad
    /// geometry this drives at scene-build time.
    pub fn set_hold_ring(&mut self, ring: Option<(f32, f32, f32)>) {
        self.scene_dirty = true;
        self.hold_ring = ring;
        self.window.request_redraw();
    }

    /// Set/clear the incoming-drag ghost tab (v0.4.0): the carried surface's
    /// title, or `None` to clear. Always marks the scene dirty (unlike most
    /// setters here, which no-op on an unchanged value) — a live hover
    /// re-sets this every motion tick AND every `about_to_wait` animation
    /// tick (`WindowState::advance_animations`) purely to keep the flicker
    /// live, which needs a full rebuild each time; see [`Self::ghost_active`].
    /// The start-of-shimmer clock is preserved across repeated `Some(_)`
    /// calls with the SAME label so the flicker phase doesn't jump every
    /// motion tick — only a label CHANGE (or a `None` → `Some` transition)
    /// resets it.
    pub fn set_ghost_tab(&mut self, label: Option<String>) {
        self.scene_dirty = true;
        match (&mut self.ghost_tab, label) {
            (Some((cur, _)), Some(new)) if *cur == new => {}
            (slot, Some(new)) => *slot = Some((new, Instant::now())),
            (slot, None) => *slot = None,
        }
        self.window.request_redraw();
    }

    /// Whether a ghost tab is currently showing — `about_to_wait`'s
    /// per-window animation gate ORs this in so the flicker keeps ticking
    /// (via [`Self::touch_ghost`], called every animation tick) purely
    /// while a cross-window drag is hovering this window's strip, and costs
    /// nothing the rest of the time.
    pub fn ghost_active(&self) -> bool {
        self.ghost_tab.is_some()
    }

    /// Re-mark the scene dirty for a live ghost tab's shimmer, without
    /// changing its label — a no-op when no ghost is showing. Called every
    /// animation tick (`WindowState::advance_animations`) so the flicker
    /// keeps advancing even when the pointer itself isn't moving (a static
    /// hover, no new `set_ghost_tab` call otherwise).
    pub fn touch_ghost(&mut self) {
        if self.ghost_tab.is_some() {
            self.scene_dirty = true;
            self.window.request_redraw();
        }
    }

    /// Set/clear the suck-in/pour-out morph (v0.4.0): `(rect, grab point,
    /// t01, inward)`, all logical px/`0..1`, this window's own space —
    /// `inward: true` for the tear-off suck-in, `false` for the drop/cancel
    /// pour-out. See [`crate::paint::morph_quads`] for the quad geometry
    /// this drives at scene-build time. Driven by `WindowState::tick_morph`
    /// every `about_to_wait` tick, exactly like [`Self::set_hold_ring`] —
    /// and, like it, NOT cleared by `WindowState::clear_drag_visuals`: a
    /// self-terminating ~150-200ms animation, not a persistent drag cue.
    pub fn set_morph(&mut self, morph: Option<MorphState>) {
        self.scene_dirty = true;
        self.morph = morph;
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
        tab_col_hit(self.tabs.len(), tab_cols, self.hovered_tab, col)
    }

    /// Which tab slot logical-x falls over, clamped to a valid tab index — used
    /// during a drag to pick the drop position. `None` only when the strip has
    /// no tabs at all. Mirrors the tab-area column math in [`build_tabs`].
    pub fn tab_slot_at(&self, x: f32) -> Option<usize> {
        let n = self.tabs.len();
        if n == 0 {
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

    /// Which strip slot logical-x falls over during a live drag hover: a
    /// real tab's own chip (spring-loads a `select_tab`, finding #2) or the
    /// reserved ghost/append segment (finding #3 — this window is showing a
    /// ghost tab for an incoming cross-window drag). `None` only when the
    /// strip has no tabs. Ghost-AWARE unlike [`Self::tab_slot_at`] (drop
    /// position math): segments `tab_cols` exactly like [`build_tabs`] does,
    /// `n + 1`-wide only while THIS window's [`Self::ghost_active`].
    pub fn tab_slot_or_ghost_at(&self, x: f32) -> Option<StripSlot> {
        let n = self.tabs.len();
        if n == 0 {
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
            return Some(StripSlot::Tab(0));
        }
        let col = ((x / cw).floor().max(0.0) as usize).min(tab_cols - 1);
        tab_or_ghost_col_hit(n, self.ghost_active(), tab_cols, col)
    }

    /// Show the cheat-sheet overlay with these `(key, description)` rows, or hide
    /// it with `None`. The next `render` draws (or stops drawing) the modal.
    /// Override the help panel's `(title, hint)`, or reset to the shortcuts
    /// default with `None`. Set before `set_help` when reusing the panel (e.g.
    /// a close confirmation).
    pub fn set_help_title(&mut self, title: Option<(String, String)>) {
        self.scene_dirty = true;
        self.help_title = title;
    }

    pub fn set_help(&mut self, lines: Option<Vec<(String, String)>>) {
        self.scene_dirty = true;
        self.help = lines;
        self.window.request_redraw();
    }

    /// Whether the help overlay is currently shown.
    pub fn help_visible(&self) -> bool {
        self.help.is_some()
    }

    /// Show the About overlay with this content, or hide it with `None`.
    /// Show/hide the blocking confirm modal.
    pub fn set_confirm(&mut self, view: Option<ConfirmView>) {
        self.scene_dirty = true;
        self.confirm = view;
    }

    pub fn confirm_shown(&self) -> bool {
        self.confirm.is_some()
    }

    /// Which confirm button (`0` cancel, `1` confirm) is at logical `(x, y)`.
    pub fn confirm_button_at(&self, x: f32, y: f32) -> Option<usize> {
        self.confirm_buttons.iter().find_map(|(r, idx)| {
            (x >= r[0] && x < r[0] + r[2] && y >= r[1] && y < r[1] + r[3]).then_some(*idx)
        })
    }

    pub fn set_about(&mut self, info: Option<AboutInfo>) {
        self.scene_dirty = true;
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
        self.scene_dirty = true;
        self.about_glow = glow.clamp(0.0, 1.0);
        self.about_time = t;
    }

    /// Show the Settings overlay with these resolved rows and the selected
    /// row index, or hide it with `None`.
    pub fn set_settings(&mut self, view: Option<(Vec<SettingsRowView>, usize)>) {
        self.scene_dirty = true;
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
        // `time` advances every animation tick; only the OTHER fields are
        // scene changes. Marking dirty on time would defeat the
        // animation-only fast path entirely.
        let scene_changed = {
            let a = &self.backdrop;
            let b = &params;
            (a.gradient, a.sparks, a.density, a.scrim, a.frame_dt)
                != (b.gradient, b.sparks, b.density, b.scrim, b.frame_dt)
        };
        if scene_changed {
            self.scene_dirty = true;
        }
        self.backdrop = params;
        self.window.request_redraw();
    }

    /// Whether an animated backdrop effect (sparks) is active — the app uses this
    /// to decide whether to drive continuous redraws.
    pub fn backdrop_animating(&self) -> bool {
        self.backdrop.sparks
    }

    /// Set or clear the active text selection (and the session it belongs to).
    /// Anchored to absolute scrollback lines; projected into the viewport at
    /// paint time so the highlight travels with its text as output scrolls.
    /// Requests a redraw so the highlight updates.
    pub fn set_selection(&mut self, selection: Option<(SessionId, AnchoredSelection)>) {
        self.scene_dirty = true;
        self.selection = selection;
        self.window.request_redraw();
    }

    /// Set or clear the scrollback-search bar text (top-right). `None` = closed.
    pub fn set_search_bar(&mut self, text: Option<String>) {
        self.scene_dirty = true;
        self.search_bar = text;
        self.window.request_redraw();
    }

    /// Set or clear the FPS/frame-time debug readout text (bottom-right).
    pub fn set_fps_overlay(&mut self, text: Option<String>) {
        self.scene_dirty = true;
        self.fps_overlay = text;
    }

    /// Set the visual-bell flash intensity (`0..1`) — a warm amber wash over the
    /// panes. The app drives this each frame, decaying it to 0 after a BEL.
    pub fn set_bell_flash(&mut self, intensity: f32) {
        self.scene_dirty = true;
        self.bell_flash = intensity.clamp(0.0, 1.0);
        self.window.request_redraw();
    }

    /// The currently selected text, if any (read from the owning pane's grid,
    /// after projecting the anchored selection into the current viewport —
    /// only the visible portion is readable here; the copy path prefers the
    /// snapshot captured at selection time).
    pub fn selected_text(&self) -> Option<String> {
        let (sid, sel) = self.selection.as_ref()?;
        let p = self.panes.get(sid)?;
        let view = sel.project(&p.grid)?;
        let text = view.text(&p.grid);
        (!text.is_empty()).then_some(text)
    }

    /// Set (or clear with `None`) the backdrop image and its fit mode.
    /// When an image is set it draws behind the cells *in place of* the gradient;
    /// the scrim still applies for legibility. The decoded RGBA is kept in RAM so
    /// on-screen captures can reproduce it headlessly. Requests a redraw.
    pub fn set_backdrop_image(&mut self, img: Option<(Vec<u8>, u32, u32)>, fit: ImageFit) {
        self.scene_dirty = true;
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
        self.scene_dirty = true;
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&self.device, &self.config);
        // A real resize is a fresh situation for the swapchain — don't let a
        // stale lost-streak from before the resize starve the repaint.
        self.lost_bound.clear();
        // Chrome sizing is owned by `build_tabs` (keyed on logical width in its
        // shaping cache) — sizing it here to the *physical* width was both
        // redundant and wrong, masked only by the old per-frame re-shape.
        self.window.request_redraw();
    }

    /// Draw all visible panes (each in its rect) plus the tab strip. Returns
    /// `false` if the surface needs reconfiguring (request another redraw).
    pub fn render(&mut self) -> RenderOutcome {
        // Starved of drawables (occluded / display asleep)? Skip the whole
        // frame — prep below allocates GPU staging buffers, and doing that at
        // PTY-event rate with no present is the  OOM spin. Checked FIRST
        // so a starved frame costs nothing.
        let frame_start = Instant::now();
        if !self.starve_gate.should_attempt(frame_start) {
            return RenderOutcome::Starved;
        }
        // The app works in logical px; the surface is physical. Scale every draw
        // coordinate by the live HiDPI factor (handles Retina + display moves).
        let sf = self.window.scale_factor() as f32;
        let full_bounds = TextBounds {
            left: 0,
            top: 0,
            right: self.config.width as i32,
            bottom: self.config.height as i32,
        };
        // Animation-only fast path: nothing in the static scene changed and
        // the only reason we're rendering is the spark clock. Skip the whole
        // scene rebuild and the quad/text uploads; re-encode over the
        // retained GPU buffers with fresh spark quads. This is what keeps
        // idle-with-sparks cheap (the full rebuild costs ~4x more CPU per
        // frame than the sparks themselves).
        let animation_only =
            !self.scene_dirty && self.backdrop.sparks && self.panes.values().all(|p| !p.dirty);
        if animation_only {
            let lw = self.config.width as f32 / sf;
            let lh = self.config.height as f32 / sf;
            let spark_rects = spark_quads(
                self.backdrop.density,
                self.backdrop.time,
                lw,
                lh,
                sf,
                self.backdrop.frame_dt,
            );
            self.sparks.prepare(
                &self.device,
                &self.queue,
                (self.config.width as f32, self.config.height as f32),
                &spark_rects,
            );
        } else {
            let mut rects: Vec<([f32; 4], [f32; 4])> = Vec::new();
            // Rounded quads (tab pills) — drawn after the sharp chrome, before text.
            let mut rounded: Vec<([f32; 4], [f32; 4], f32)> = Vec::new();
            let mut spark_rects: Vec<([f32; 4], [f32; 4])> = Vec::new();
            // Index into `rects` where the additive spark pass is interleaved:
            // everything before is backdrop (gradient/scrim), everything after is
            // cells + chrome, so opaque content covers the embers. 0 = no backdrop
            // (overlays): all quads draw after the (empty) spark pass.
            let mut spark_layer: usize = 0;
            let mut areas: Vec<TextArea> = Vec::new();
            // Confirm-modal dialog text: a second pass, drawn after the panel quad.
            let mut overlay_areas: Vec<TextArea> = Vec::new();
            // Sprite-path `CustomGlyph`s per visible pane, in `self.visible`
            // order — declared out here (not in the `else` block below) so it outlives
            // the `areas` that borrow from it, through the `prepare_with_custom` call.
            let pane_customs: Vec<Vec<CustomGlyph>>;
            // Whether to draw the backdrop image this frame (pane view only — never
            // over the Settings/About/help overlays).
            let mut draw_image = false;

            if let Some((rows, selected)) = self.settings.clone() {
                // Modal Settings overlay. Re-shape the text only when the rows,
                // selection, or surface geometry changed — the modal repaints
                // every frame, and shaping is the expensive part.
                let logical_w = self.config.width as f32 / sf;
                let logical_h = self.config.height as f32 / sf;
                let shape_key = (
                    rows.clone(),
                    selected,
                    self.config.width,
                    self.config.height,
                    sf.to_bits(),
                );
                let reshape = self.settings_shaped.as_ref() != Some(&shape_key);
                let (left, top) = build_settings(
                    &mut self.font_system,
                    &mut self.settings_buffer,
                    &rows,
                    selected,
                    self.settings_cw,
                    logical_w,
                    logical_h,
                    sf,
                    reshape,
                    &mut rects,
                );
                if reshape {
                    self.settings_shaped = Some(shape_key);
                }
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
                        self.backdrop.frame_dt,
                    );
                }
                // Pass 1: (re)shape only panes whose grid/size changed since last frame.
                // Buffers persist in `PaneRender`, so unchanged panes reuse their shaping
                // — the TextArea below just references the existing buffer.
                let (size, lh, cw) = (self.font_size, self.line_height, self.cell_w);
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
                                cw,
                                family,
                            );
                            p.links = p.grid.link_spans();
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
                        let hovered_link = self
                            .hovered_link
                            .as_ref()
                            .filter(|(sid, _)| sid == &vp.session)
                            .map(|(_, id)| *id);
                        crate::paint::link_quads(
                            &p.links,
                            hovered_link,
                            (vp.rect.x as f32, vp.rect.y as f32),
                            cw,
                            ch,
                            sf,
                            &mut rects,
                        );
                        if let Some((sid, sel)) = &self.selection {
                            if *sid == vp.session {
                                // Absolute -> current-viewport projection: the
                                // highlight follows its text (or is off-screen).
                                if let Some(view) = sel.project(&p.grid) {
                                    selection_quads(&p.grid, &view, vp.rect, cw, ch, sf, &mut rects);
                                }
                            }
                        }
                        // Split drop-zone preview over the hovered pane (Ctrl+Opt
                        // manual split, or a surface-drag Edge hover).
                        if let Some((psid, horizontal, ratio, before)) = &self.split_preview {
                            if *psid == vp.session {
                                split_preview(
                                    vp.rect,
                                    *horizontal,
                                    *before,
                                    *ratio,
                                    sf,
                                    &mut rects,
                                );
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
                // Hold-to-wisp ring (v1.1): window-space, not tied to any
                // one pane, so it's drawn once here — after every pane's
                // own content (including that pane's split preview), before
                // the tab strip — so it reads as sitting above pane content.
                if let Some((rx, ry, progress)) = self.hold_ring {
                    hold_ring_quads(rx, ry, progress, sf)
                        .into_iter()
                        .for_each(|q| rects.push(q));
                }
                // Suck-in/pour-out morph (v0.4.0): same window-space
                // placement as the hold ring, for the same reason.
                if let Some((rect, grab, t01, inward)) = self.morph {
                    morph_quads(rect, grab, t01, inward, sf)
                        .into_iter()
                        .for_each(|q| rects.push(q));
                }
                let logical_w = self.config.width as f32 / sf;
                let ghost = self
                    .ghost_tab
                    .as_ref()
                    .map(|(label, since)| (label.as_str(), since.elapsed().as_secs_f32()));
                let close_cx = build_tabs(
                    &mut self.font_system,
                    &mut self.chrome,
                    &mut self.close_buffer,
                    &mut self.tabs_cache,
                    &self.tabs,
                    self.tab_drag,
                    ghost,
                    self.hovered_tab,
                    cw,
                    logical_w,
                    sf,
                    &mut rects,
                    &mut rounded,
                );

                // Pass 3: one TextArea per visible pane (clipped to its rect) + the strip.
                // Sprite-path glyphs ride alongside the shaped text as
                // `CustomGlyph`s — computed here (not cached with the buffer) so a
                // glyph's cell position always matches this frame's `cw`/`ch`.
                pane_customs = self
                    .visible
                    .iter()
                    .map(|vp| {
                        self.panes
                            .get(&vp.session)
                            .map(|p| crate::sprite::pane_custom_glyphs(&p.grid, cw, ch, sf))
                            .unwrap_or_default()
                    })
                    .collect();
                for (vp, customs) in self.visible.iter().zip(pane_customs.iter()) {
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
                            custom_glyphs: customs,
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
                // The hovered tab's "✕", pixel-centered in the pill's left cap. `cw/2`
                // recenters the 1-cell glyph on the cap center; `top: PAD` matches the
                // chrome baseline (already vertically centered in the strip).
                if let Some(cx) = close_cx {
                    areas.push(TextArea {
                        buffer: &self.close_buffer,
                        left: (cx - cw * 0.5) * sf,
                        top: PAD * sf,
                        scale: sf,
                        bounds: full_bounds,
                        default_color: Color::rgb(0xcc, 0xcc, 0xcc),
                        custom_glyphs: &[],
                    });
                }
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

            // Blocking confirm modal — drawn OVER everything (panes, tabs, overlays).
            // Its scrim + panel + button quads are appended to `rounded` *after* this
            // point, so this boundary splits base rounded quads (tab pills, drawn
            // before the pane text) from the confirm quads (drawn after it).
            let rounded_pre_confirm = rounded.len() as u32;
            self.confirm_buttons.clear();
            if let Some(view) = self.confirm.clone() {
                let lw = self.config.width as f32 / sf;
                let lh = self.config.height as f32 / sf;
                let cw = self.cell_w;
                let cl = build_confirm(
                    &mut self.font_system,
                    &mut self.confirm_title,
                    &mut self.confirm_msg,
                    &mut self.confirm_cancel,
                    &mut self.confirm_ok,
                    &view,
                    cw,
                    lw,
                    lh,
                    sf,
                    &mut rounded,
                );
                self.confirm_buttons = cl.buttons;
                for (buf, (ox, oy)) in [
                    (&self.confirm_title, cl.title_origin),
                    (&self.confirm_msg, cl.msg_origin),
                    (&self.confirm_cancel, cl.cancel_origin),
                    (&self.confirm_ok, cl.ok_origin),
                ] {
                    // Overlay pass: drawn after the opaque panel, so it can't be
                    // overpainted by pane text underneath.
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

            self.quads.prepare(
                &self.device,
                &self.queue,
                (self.config.width as f32, self.config.height as f32),
                &rects,
                &rounded,
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

            let prepared = self.text_renderer.prepare_with_custom(
                &self.device,
                &self.queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                areas,
                &mut self.swash_cache,
                crate::sprite::rasterize,
            );
            if let Err(e) = prepared {
                // Don't freeze on a transient atlas/prepare error: log it (always, since
                // it means glyphs won't paint this frame) and ask for another redraw.
                // Poll first so the staging buffers this frame already wrote get
                // reclaimed — early returns must never skip reclamation.
                debug_emit(&format!("[ember] text prepare failed this frame: {e:?}"));
                eprintln!("[ember] text prepare failed, skipping glyphs this frame: {e:?}");
                let _ = self.device.poll(wgpu::PollType::Poll);
                return RenderOutcome::Retry;
            }
            // Confirm-dialog text (empty unless the modal is up), prepared into the
            // shared atlas for its own post-panel render pass.
            let prepared_overlay = self.overlay_text.prepare(
                &self.device,
                &self.queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                overlay_areas,
                &mut self.swash_cache,
            );
            if let Err(e) = prepared_overlay {
                debug_emit(&format!(
                    "[ember] overlay text prepare failed this frame: {e:?}"
                ));
                let _ = self.device.poll(wgpu::PollType::Poll);
                return RenderOutcome::Retry;
            }
            // Everything the animation-only path will reuse is now uploaded;
            // remember the encoder inputs and mark the scene clean. (Kept after
            // the prepare error-returns above: a failed upload must NOT leave a
            // half-built scene marked clean.)
            self.retained = RetainedScene {
                draw_image,
                spark_layer,
                rounded_pre_confirm,
            };
            self.scene_dirty = false;
        }

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) => f,
            wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            // No drawable because nobody can see us (occluded window, asleep
            // display). The surface is NOT broken — reconfiguring here would
            // allocate a fresh swapchain per attempt, which at retry rate is
            // exactly the  leak (~3,500 IOAccelerator regions/s → OOM).
            // Poll so in-flight work still gets reclaimed, arm the starve
            // gate, and tell the app not to retry.
            wgpu::CurrentSurfaceTexture::Occluded | wgpu::CurrentSurfaceTexture::Timeout => {
                debug_emit("[ember] no drawable (occluded/timeout): starving redraws");
                self.starve_gate.starve(frame_start);
                let _ = self.device.poll(wgpu::PollType::Poll);
                return RenderOutcome::Starved;
            }
            // The surface genuinely needs rebuilding (resize race, device
            // loss, validation): reconfigure and ask for one immediate retry.
            // But only a couple of times in a row — each reconfigure here
            // allocates a fresh swapchain, and while the GPU is asleep the
            // surface can stay Lost for hours: an unbounded Retry loop at
            // event-loop speed is the sleep balloon (GBs of abandoned
            // swapchains, reclaimed only slowly after wake). Persistent loss
            // falls back to the starve cadence, which still reconfigures on
            // each (backed-off) attempt so the surface heals the moment the
            // GPU can actually produce a drawable again.
            wgpu::CurrentSurfaceTexture::Outdated
            | wgpu::CurrentSurfaceTexture::Lost
            | wgpu::CurrentSurfaceTexture::Validation => {
                self.surface.configure(&self.device, &self.config);
                let _ = self.device.poll(wgpu::PollType::Poll);
                if self.lost_bound.record() {
                    return RenderOutcome::Retry;
                }
                debug_emit(&format!(
                    "[ember] surface lost {} times consecutively: starving redraws",
                    self.lost_bound.consecutive
                ));
                self.starve_gate.starve(frame_start);
                return RenderOutcome::Starved;
            }
        };
        self.starve_gate.clear();
        self.lost_bound.clear();
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
            if self.retained.draw_image {
                self.image.draw(&mut pass);
            }
            // Backdrop quads → sparks (additive) → cells + chrome → text. The
            // embers glow over the gradient but sit behind opaque cell bgs, the
            // selection, and the tab strip.
            let split = self.retained.spark_layer as u32;
            let sharp = self.quads.sharp_count();
            // Rounded quads split into base (tab pills, before pane text) and the
            // confirm modal's scrim+panel+buttons (after it, so the opaque panel
            // covers pane glyphs instead of them bleeding through).
            let base_rounded_end = sharp + self.retained.rounded_pre_confirm;
            self.quads.draw_range(&mut pass, 0..split);
            self.sparks.draw(&mut pass);
            self.quads.draw_range(&mut pass, split..sharp); // cells + chrome
            self.quads.draw_range(&mut pass, sharp..base_rounded_end); // tab pills
            let _ = self
                .text_renderer
                .render(&self.atlas, &self.viewport, &mut pass); // panes + chrome
            // Confirm modal on top: scrim dims the (painted) panes, the opaque
            // panel covers its box, then the dialog text renders over it.
            self.quads.draw_range(&mut pass, base_rounded_end..u32::MAX);
            let _ = self
                .overlay_text
                .render(&self.atlas, &self.viewport, &mut pass);
        }
        self.queue.submit(Some(encoder.finish()));
        frame.present();
        // Advance wgpu's resource lifecycle so completed per-frame GPU resources
        // (command buffers, temporary/destroyed staging buffers) are actually
        // reclaimed. The windowed loop runs on ControlFlow::Wait and never
        // otherwise polls, so without this every redraw leaks GPU allocations —
        // they pile up as thousands of IOAccelerator regions and balloon the
        // process footprint (purgeable, but it thrashes swap). Non-blocking.
        let _ = self.device.poll(wgpu::PollType::Poll);
        self.atlas.trim();
        RenderOutcome::Presented
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Duration, Instant, LostSurfaceBound, StarveGate, StripSlot, TabHit, tab_col_hit,
        tab_or_ghost_col_hit,
    };

    // 3 equal tabs over 30 columns → 10 cols each (slots 0-9, 10-19, 20-29).
    const N: usize = 3;
    const COLS: usize = 30;

    #[test]
    fn a_lone_tab_is_still_pressable() {
        // A single-tab strip used to render (and hit-test) nothing, which
        // made "grab this window's tab" impossible once tabs became
        // draggable. The lone tab now owns the whole tab area.
        assert_eq!(tab_col_hit(1, COLS, None, 5), Some(TabHit::Tab(0)));
        assert_eq!(tab_col_hit(0, COLS, None, 5), None);
    }

    #[test]
    fn plain_column_selects_its_tab() {
        assert_eq!(tab_col_hit(N, COLS, None, 5), Some(TabHit::Tab(0)));
        assert_eq!(tab_col_hit(N, COLS, None, 15), Some(TabHit::Tab(1)));
        assert_eq!(tab_col_hit(N, COLS, None, 25), Some(TabHit::Tab(2)));
    }

    #[test]
    fn hovered_tab_left_zone_is_a_close() {
        // Hovering tab 1 (cols 10-19): its first CLOSE_COLS (10,11) close it...
        assert_eq!(tab_col_hit(N, COLS, Some(1), 10), Some(TabHit::CloseTab(1)));
        assert_eq!(tab_col_hit(N, COLS, Some(1), 11), Some(TabHit::CloseTab(1)));
        // ...but the rest of the tab still selects.
        assert_eq!(tab_col_hit(N, COLS, Some(1), 12), Some(TabHit::Tab(1)));
    }

    #[test]
    fn close_zone_only_on_the_hovered_tab() {
        // Tab 0's left columns are NOT a close zone when tab 1 is the hovered one.
        assert_eq!(tab_col_hit(N, COLS, Some(1), 0), Some(TabHit::Tab(0)));
        // No hover at all → never a close.
        assert_eq!(tab_col_hit(N, COLS, None, 10), Some(TabHit::Tab(1)));
    }

    // --- tab_or_ghost_col_hit: strip slots for a live drag hover ---

    #[test]
    fn without_a_ghost_the_slots_match_the_plain_hit_math() {
        // Same 3x10 segmentation as `tab_col_hit`, plus the trailing-slack
        // clamp onto the last tab.
        assert_eq!(
            tab_or_ghost_col_hit(N, false, COLS, 5),
            Some(StripSlot::Tab(0))
        );
        assert_eq!(
            tab_or_ghost_col_hit(N, false, COLS, 15),
            Some(StripSlot::Tab(1))
        );
        assert_eq!(
            tab_or_ghost_col_hit(N, false, COLS, 29),
            Some(StripSlot::Tab(2))
        );
        assert_eq!(tab_or_ghost_col_hit(0, false, COLS, 5), None);
    }

    #[test]
    fn a_ghost_claims_the_last_segment_and_compresses_the_real_tabs() {
        // 3 real tabs + ghost over 30 cols → 4 segments of 7 (with 2 cols
        // of slack landing in the ghost's trailing segment, exactly like
        // `build_tabs`' "the true last segment absorbs the leftover").
        assert_eq!(
            tab_or_ghost_col_hit(N, true, COLS, 6),
            Some(StripSlot::Tab(0))
        );
        assert_eq!(
            tab_or_ghost_col_hit(N, true, COLS, 7),
            Some(StripSlot::Tab(1))
        );
        assert_eq!(
            tab_or_ghost_col_hit(N, true, COLS, 20),
            Some(StripSlot::Tab(2))
        );
        assert_eq!(
            tab_or_ghost_col_hit(N, true, COLS, 21),
            Some(StripSlot::Ghost)
        );
        assert_eq!(
            tab_or_ghost_col_hit(N, true, COLS, 29),
            Some(StripSlot::Ghost)
        );
    }

    #[test]
    fn a_lone_tab_with_a_ghost_splits_the_strip_in_two() {
        assert_eq!(
            tab_or_ghost_col_hit(1, true, COLS, 14),
            Some(StripSlot::Tab(0))
        );
        assert_eq!(
            tab_or_ghost_col_hit(1, true, COLS, 15),
            Some(StripSlot::Ghost)
        );
    }

    // --- StarveGate: occluded-surface render throttle -------------

    #[test]
    fn fresh_gate_always_attempts() {
        let gate = StarveGate::new();
        assert!(gate.should_attempt(Instant::now()));
    }

    #[test]
    fn starved_gate_holds_off_then_reopens() {
        let mut gate = StarveGate::new();
        let t0 = Instant::now();
        gate.starve(t0);
        // Immediately after starving: blocked (this is the anti-spin property —
        // a 700Hz retry storm collapses to ≤4 attempts/s).
        assert!(!gate.should_attempt(t0));
        assert!(!gate.should_attempt(t0 + Duration::from_millis(249)));
        // After the holdoff: one attempt is allowed again (self-healing even if
        // winit never delivers an Occluded(false) event).
        assert!(gate.should_attempt(t0 + StarveGate::HOLDOFF));
    }

    #[test]
    fn clear_reopens_immediately() {
        // A successful present (or an explicit un-occlude) must never leave a
        // revealed window waiting out the holdoff.
        let mut gate = StarveGate::new();
        let t0 = Instant::now();
        gate.starve(t0);
        gate.clear();
        assert!(gate.should_attempt(t0));
    }

    // --- StarveGate backoff: sleep-balloon regression --------------

    #[test]
    fn holdoff_doubles_each_consecutive_starve_up_to_the_cap() {
        // A display-sleep stretch winit never reports as `Occluded` must not
        // poll forever at the original 4/s: each consecutive starve (no
        // `clear()` in between, i.e. never actually revealed) should widen
        // the holdoff, not repeat it.
        let mut gate = StarveGate::new();
        let t0 = Instant::now();
        assert_eq!(gate.current_holdoff(), StarveGate::HOLDOFF); // fresh gate
        gate.starve(t0);
        assert_eq!(gate.current_holdoff(), Duration::from_millis(250));
        gate.starve(t0);
        assert_eq!(gate.current_holdoff(), Duration::from_millis(500));
        gate.starve(t0);
        assert_eq!(gate.current_holdoff(), Duration::from_secs(1));
        gate.starve(t0);
        assert_eq!(gate.current_holdoff(), Duration::from_secs(2));
        gate.starve(t0);
        assert_eq!(gate.current_holdoff(), Duration::from_secs(4));
        gate.starve(t0);
        assert_eq!(gate.current_holdoff(), Duration::from_secs(8));
        // Further consecutive starves hold at the cap rather than growing
        // unbounded — an all-night sleep must settle, not keep widening.
        gate.starve(t0);
        assert_eq!(gate.current_holdoff(), Duration::from_secs(8));
    }

    #[test]
    fn an_eight_hour_sleep_attempts_hundreds_not_hundreds_of_thousands_of_times() {
        // The regression this backoff exists for: at the old flat 4/s, an
        // 8-hour display sleep would attempt ~115,200 acquires. With the
        // backoff ramped to its 8s cap, the same stretch should attempt on
        // the order of a few thousand at most.
        let mut gate = StarveGate::new();
        let mut now = Instant::now();
        let deadline = now + Duration::from_secs(8 * 3600);
        let mut attempts = 0u32;
        while now < deadline {
            if gate.should_attempt(now) {
                attempts += 1;
                gate.starve(now);
            }
            now += Duration::from_millis(50); // finer than any holdoff in play
        }
        assert!(
            attempts < 5_000,
            "expected the backoff to bound an 8h sleep to a few thousand attempts, got {attempts}"
        );
    }

    #[test]
    fn clearing_resets_the_backoff_for_the_next_stretch() {
        // One long starved stretch must not permanently slow down the NEXT
        // one — a real reveal (`clear()`) has to reset the ramp back to the
        // fast 250ms starting holdoff.
        let mut gate = StarveGate::new();
        let t0 = Instant::now();
        for _ in 0..10 {
            gate.starve(t0);
        }
        assert_eq!(gate.current_holdoff(), Duration::from_secs(8));
        gate.clear();
        assert_eq!(gate.current_holdoff(), StarveGate::HOLDOFF);
    }

    // --- LostSurfaceBound: persistent-loss reconfigure spin (sleep balloon) --

    #[test]
    fn persistent_surface_loss_stops_earning_immediate_retries() {
        // The sleep-balloon mechanism: Lost → reconfigure (fresh swapchain)
        // → Retry → immediate request_redraw → Lost → ... unbounded while
        // the GPU sleeps. The first couple of losses keep the fast path (the
        // benign resize race must still settle in one round trip); from the
        // third on, the caller must be told to starve, not retry.
        let mut bound = LostSurfaceBound::new();
        assert!(bound.record(), "1st loss: immediate retry (resize race)");
        assert!(bound.record(), "2nd loss: one more for margin");
        assert!(!bound.record(), "3rd loss: fall back to starve cadence");
        // ...and it must STAY on the starve cadence, not oscillate back.
        for _ in 0..100 {
            assert!(!bound.record());
        }
    }

    #[test]
    fn a_successful_acquire_resets_the_lost_streak() {
        // Distinct incidents get the fast path each time: a loss streak that
        // ended in a real drawable (or reveal/resize) must not leave the next
        // resize race starved.
        let mut bound = LostSurfaceBound::new();
        for _ in 0..10 {
            bound.record();
        }
        bound.clear();
        assert!(bound.record(), "fresh incident earns the fast path again");
    }
}
