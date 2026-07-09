//! `ember-term` — the Ember terminal binary (design §2; , ).
//!
//! Owns the event loop (winit) and wires the window (`ember-platform`) to the GPU
//! renderer (`ember-render`) and N `LocalPty` sessions (`ember-session`). The
//! `ember-core` multiplexer drives everything: the layout tree says which panes
//! exist and where, one `LocalPty` session backs each pane leaf, and keystrokes
//! either drive a `LayoutCommand` (split/close/focus/new-tab) or flow to the
//! focused pane's PTY as `BackendControl::Input`. PTY output flows back over each
//! session's pixel lane into that pane's grid. This is the splits + tabs
//! milestone — live tiled shells, on Linux and macOS.

mod config;
mod control;
#[cfg(unix)]
mod mcp;
mod screenshot;
mod window_state;

use window_state::{DragEnded, HoldTick, MorphTick, WindowState};

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use control::{ControlMsg, MoveTabTarget, PromotePaneTarget};

use ember_core::{
    Axis, BackendControl, BackendEvent, BackendHandle, ClipboardOp, Config, GridDims, LayoutNode,
    MoveEffect, MoveError, OscEvent, PaneId, Rect, RowKind, ScrollAmount, SessionId,
    SettingsRowView, SparksMode, SurfaceDest, SurfaceRef, Tab, TabId, resolve_rows,
};
use ember_platform::{MenuAction, PlatformBackend};
use ember_render::{
    BackdropParams, CELL_HEIGHT, CELL_WIDTH, RenderOutcome, Renderer, Selection, SelectionMode,
    TabHit, WispRenderer, WispUnsupported,
};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey, SmolStr};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::window::WindowId;

/// The Ember app icon (embedded). Set on the window + the macOS dock at startup.
const ICON_PNG: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icon.png"));

pub(crate) const PAD: f32 = 4.0;
pub(crate) const DEFAULT_COLS: u16 = 80;
pub(crate) const DEFAULT_ROWS: u16 = 24;

/// The winit user-event type: a wake nudge from the PTY frame lane or the
/// control socket, so the loop can idle on `ControlFlow::Wait` instead of
/// polling and only run when there is genuinely something to do.
#[derive(Debug, Clone, Copy)]
enum EmberEvent {
    Wake,
}
/// Redraw cadence (~60fps) while an animation (e.g. the About glow) is active.
const ANIM_FRAME: Duration = Duration::from_millis(16);
/// Sparks guardrails (v0.3.1): how long a cached Low Power Mode/Reduce Motion
/// read (`Shared::power_state`) stays valid before the next check re-queries
/// the OS. Neither setting changes on a timescale that matters for a sparks
/// animation — a few seconds of staleness is invisible — so this trades a
/// bounded staleness window for keeping the OS query off the animation-gate
/// hot path.
const POWER_STATE_TTL: Duration = Duration::from_secs(5);
/// Max gap between clicks at the same cell to count as a double/triple click.
pub(crate) const MULTI_CLICK: Duration = Duration::from_millis(400);
/// Scrollback lines per mouse-wheel notch (Alacritty/Ghostty default).
const WHEEL_LINES: i32 = 3;

// `EMBER_FONT_DEBUG=1`: a stderr logger that surfaces cosmic-text's internal
// font diagnostics ("font matches for … in …", "failed to find family …",
// "failed to load font …") with since-launch millisecond timestamps. Font
// resolution problems are invisible from the outside (wrong glyph, slow
// frame, or nothing at all) and these logs are the only window into which
// fonts cosmic-text actually tried — this is what root-caused the
// font-family-switch hang. Off (and free) unless the env var is
// set; the sibling of `EMBER_DEBUG`'s frame diagnostics.
struct FontDebugLogger(std::time::Instant);
impl log::Log for FontDebugLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Debug
    }
    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            eprintln!(
                "[{:>10.3}ms {} {}] {}",
                self.0.elapsed().as_secs_f64() * 1000.0,
                record.level(),
                record.target(),
                record.args()
            );
        }
    }
    fn flush(&self) {}
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if std::env::var_os("EMBER_FONT_DEBUG").is_some() {
        let logger = Box::leak(Box::new(FontDebugLogger(std::time::Instant::now())));
        if log::set_logger(logger).is_ok() {
            log::set_max_level(log::LevelFilter::Debug);
        }
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        print_banner();
        return;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return;
    }
    // A typo'd flag must not fall through to a full GUI launch (a window that
    // steals focus and feeds keystrokes to a live shell). Bare `ember-term`
    // launches the app; anything unrecognized errors out.
    if args.len() > 1 && !args.iter().any(|a| a == "--screenshot") {
        let cmd = args[1].as_str();
        if cmd != "ctl" && cmd != "mcp" {
            eprintln!("ember-term: unrecognized argument `{cmd}` (see --help)");
            std::process::exit(2);
        }
    }
    // Debug control client: `ember-term ctl [--pid N|--sock P] <list|type|key|chord|state>`
    // talks to a running instance's EMBER_CONTROL socket. See `control`.
    if args.get(1).map(String::as_str) == Some("ctl") {
        if let Err(e) = control::client(&args[1..]) {
            eprintln!("ctl: {e}");
            std::process::exit(1);
        }
        return;
    }
    // MCP stdio server: `ember-term mcp` exposes the control surface as tools.
    if args.get(1).map(String::as_str) == Some("mcp") {
        #[cfg(unix)]
        {
            if let Err(e) = mcp::serve() {
                eprintln!("mcp: {e}");
                std::process::exit(1);
            }
            return;
        }
        #[cfg(not(unix))]
        {
            eprintln!("mcp is unix-only");
            std::process::exit(1);
        }
    }
    // Headless self-review: render a deterministic scene to a PNG and exit. Needs
    // a GPU but no display (Metal/Vulkan render offscreen), so it works in CI /
    // an agent shell. See `screenshot::parse` for flags.
    if args.iter().any(|a| a == "--screenshot") {
        match screenshot::parse(&args).and_then(screenshot::run) {
            Ok(path) => println!("wrote {path}"),
            Err(e) => {
                eprintln!("screenshot failed: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    // Set the macOS app name BEFORE winit builds NSApplication (see below), and
    // build the event loop early so its proxy can wake the loop from the PTY
    // frame lane and the control socket (the loop idles on ControlFlow::Wait).
    ember_platform::set_app_name("Ember");
    let event_loop = EventLoop::<EmberEvent>::with_user_event()
        .build()
        .expect("create event loop");
    let proxy = event_loop.create_proxy();
    let wake: std::sync::Arc<dyn Fn() + Send + Sync> = {
        let proxy = proxy.clone();
        std::sync::Arc::new(move || {
            let _ = proxy.send_event(EmberEvent::Wake);
        })
    };
    // Optional debug control surface. `EMBER_CONTROL=1` binds a per-PID socket
    // under $TMPDIR/ember-ctl/ (so multiple instances coexist); an explicit path
    // is used verbatim. `ember-term ctl`/`mcp` then drive + introspect this window.
    // EMBER_CONTROL still force-binds at startup (dev/testing). The normal path
    // is the Developer Mode config toggle, bound by the app once config loads.
    let (control_rx, control_server) = match std::env::var("EMBER_CONTROL") {
        Ok(val) if !val.is_empty() => {
            match control::spawn_listener(&control::server_bind_path(&val), wake.clone()) {
                Ok((rx, server)) => {
                    eprintln!(
                        "[ember] control socket listening at {}",
                        server.path().display()
                    );
                    (Some(rx), Some(server))
                }
                Err(e) => {
                    eprintln!("[ember] control socket failed: {e}");
                    (None, None)
                }
            }
        }
        _ => (None, None),
    };
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App {
        shared: None,
        windows: HashMap::new(),
        focused_window: None,
        focus_history: Vec::new(),
        control_rx,
        control_server,
        wake,
    };
    event_loop.run_app(&mut app).expect("run event loop");
}

fn print_banner() {
    println!(
        "ember-term {} (core {}, session {}, render {}, platform {})",
        env!("CARGO_PKG_VERSION"),
        ember_core::version(),
        ember_session::core_version(),
        ember_render::core_version(),
        ember_platform::core_version(),
    );
}

fn print_usage() {
    println!(
        "usage: ember-term [--version|-V] [--help|-h]\n\
         \n\
         \x20 ember-term                     launch the terminal\n\
         \x20 ember-term ctl <cmd> …         drive a running instance (EMBER_CONTROL)\n\
         \x20 ember-term mcp                 MCP stdio server over the control surface\n\
         \x20 ember-term --screenshot <png>  headless render to a PNG (see screenshot flags)\n\
         \n\
         env: EMBER_CONTROL=1|<path>  bind the debug control socket"
    );
}

struct App {
    /// Process-wide state shared across every window (one window today).
    shared: Option<Shared>,
    /// Per-window state, keyed by winit `WindowId` (exactly one entry today).
    windows: HashMap<WindowId, WindowState>,
    /// The window that currently has focus (the sole window today).
    focused_window: Option<WindowId>,
    /// Front-to-back focus order (index 0 = most recently focused), updated
    /// on `WindowEvent::Focused(true)` — dedup by moving the id to the
    /// front rather than pushing a duplicate. Drives `window_frames()`'s
    /// z-order guess for cross-window drag hit-testing (release 2): we have
    /// no real window-manager stacking order, so "most recently focused
    /// first" is the best approximation available. A window that's never
    /// been focused (just opened, still behind another) isn't in here yet —
    /// `window_frames()` appends any missing ids from `window_order` after.
    focus_history: Vec<WindowId>,
    /// Receiver for debug-control commands (Some while the control socket is bound).
    control_rx: Option<Receiver<ControlMsg>>,
    /// The bound control listener (from EMBER_CONTROL); moved into Shared.
    control_server: Option<control::ControlServer>,
    /// Wakes the event loop from the PTY frame lane; handed to each session.
    wake: std::sync::Arc<dyn Fn() + Send + Sync>,
}

/// Process-wide state that is not tied to any one window: the running sessions,
/// user config, OS effect seam, control socket, and per-session bookkeeping.
pub(crate) struct Shared {
    /// One running session per pane leaf, keyed by its `SessionId`.
    pub(crate) sessions: HashMap<SessionId, BackendHandle>,
    /// Which window owns each session, so PTY pixel-lane deltas (drained in
    /// `about_to_wait`) land in the renderer that actually registered that
    /// pane, not "whichever window happens to be focused." Populated by
    /// `WindowState::spawn_session` and cleared by `kill_session`/window
    /// close; Task 4's `move_surface` updates it when a session changes
    /// windows.
    pub(crate) session_window: HashMap<SessionId, WindowId>,
    /// Stable window order (creation order): index = the 0-based window
    /// number `ember_core::Windows`/`SurfaceRef`/`SurfaceDest` and the ctl
    /// surface's 1-based `move-tab <N>` operate on. Maintained by
    /// `open_window` (push on create) and `close_window`/
    /// `close_window_shell_only` (remove on close) — never reordered
    /// otherwise, so an index computed before a move stays valid for every
    /// site that reads it during the same tick.
    pub(crate) window_order: Vec<WindowId>,
    // window-scoped later? id counters are per-app today (one window).
    pub(crate) next_pane: u64,
    pub(crate) next_session: u64,
    pub(crate) next_tab: u64,
    /// Debug-control command receiver (drained each poll tick).
    pub(crate) control_rx: Option<Receiver<ControlMsg>>,
    /// The bound control listener, if the socket is currently open (Settings
    /// toggle / EMBER_CONTROL). Dropping/stopping it closes the socket.
    pub(crate) control_server: Option<control::ControlServer>,
    /// User config (the Settings overlay reads + mutates it).
    pub(crate) config: Config,
    /// Backdrop animation clock.
    pub(crate) backdrop_since: Instant,
    /// Native menu bar (macOS); inert elsewhere. Kept alive for the app's life.
    pub(crate) menu: ember_platform::AppMenu,
    /// The OS effect seam (design §7, ): clipboard + open-path,
    /// `MacBackend`/`LinuxBackend` for the host OS.
    pub(crate) platform: ember_platform::HostBackend,
    /// Sparks guardrails (v0.3.1): cached `(checked_at, low_power, reduce_motion)`
    /// read from `platform`. Each field is a cheap objc message send, but the
    /// sparks animation gate (`WindowState::backdrop_animating`) is evaluated
    /// once per window on every `about_to_wait` tick while anything is
    /// animating — up to `ember_fps` times a second — so polling the OS that
    /// often would itself be the kind of "new ticking" cost this feature
    /// exists to avoid. Refreshed at most every `POWER_STATE_TTL` via
    /// `Shared::low_power_mode`/`Shared::reduce_motion`. `Cell`, not a plain
    /// field: those two gate methods only ever see `&Shared` (mirroring
    /// `backdrop_animating`'s own `&self` signature), so this is the one
    /// piece of interior mutability on the render-loop hot path — bounded
    /// (worst case a `POWER_STATE_TTL`-stale power-state read), never a
    /// wrong architecture.
    pub(crate) power_state: Cell<Option<(Instant, bool, bool)>>,
    /// Per-session bracketed-paste (DEC 2004) mode, updated from each frame delta —
    /// so paste can wrap in `ESC[200~`…`ESC[201~` only when the app asked for it.
    pub(crate) bracketed: HashMap<SessionId, bool>,
    // window-scoped later? focus reporting is per-window.
    /// The session last told it has focus (DEC 1004 focus reporting) — the
    /// backend only writes `CSI I`/`CSI O` when the app enabled mode 1004.
    pub(crate) focus_notified: Option<SessionId>,
    // window-scoped later? mouse forwarding follows a window's pointer.
    /// A mouse press being forwarded to an app (session + button code) — its
    /// drag/release go to the same session even if the pointer leaves the pane.
    pub(crate) mouse_press: Option<(SessionId, u8)>,
    // window-scoped later? per-window pointer dedup.
    /// Last (col, row) a motion report was sent for (dedup per cell).
    pub(crate) last_mouse_cell: Option<(u16, u16)>,
    // window-scoped later? titles are per-session, surfaced per-window.
    /// Latest OSC title per session, so the window title can be re-asserted on
    /// tab/pane switch (not just when a fresh Title event happens to arrive).
    pub(crate) titles: std::collections::HashMap<SessionId, String>,
    /// Latest OSC 1337 `CurrentDir` per session — a new split spawned FROM a
    /// pane inherits its cwd (design §8.1). Not removed on exit; only read
    /// while spawning, and a dead `SessionId` is never reused.
    pub(crate) cwd_by_session: std::collections::HashMap<SessionId, String>,
    /// Wakes the event loop when a session publishes a frame; registered on
    /// every session's pixel lane so the loop can idle on `ControlFlow::Wait`.
    pub(crate) wake: std::sync::Arc<dyn Fn() + Send + Sync>,
    /// A surface drag in progress (tab tear-off, and later panes/cross-window
    /// carries) — spans windows, hence living here rather than on
    /// `WindowState`. `None` outside a drag; a real mouse press+motion or
    /// `ctl drag`'s synthesized one both drive it via `WindowState::
    /// update_drag`/`update_drag_hover`/`resolve_drag_drop`.
    pub(crate) drag: Option<DragState>,
    /// One-shot position hint for the next `open_window` call, set by
    /// `WindowState::resolve_drag_drop`'s `Desktop` arm right before staging
    /// a `SurfaceDest::NewWindow` `pending_move`, and consumed (taken) by
    /// `apply_move`'s `MoveEffect::WindowOpened` handler a few lines later in
    /// the same tick. `None` for every other window-opening path (Cmd+N,
    /// `ctl new-window`, move-tab-to-new-window) — those place the OS window
    /// at its default position, unchanged.
    pub(crate) new_window_position_hint: Option<winit::dpi::PhysicalPosition<i32>>,
    /// THE WISP (release 2 task 5): the glowing drag-token window, lazily
    /// created on the first carried transition of the FIRST drag in the
    /// process and reused (shown/hidden) for every drag after that. Lives on
    /// `Shared` (not `App`) so the free functions that already thread
    /// `&mut Shared` everywhere a drag is created/advanced/ended
    /// (`update_cross_window_drag`, `WindowState::left_release`,
    /// `cancel_drag_everywhere`, `clear_drag_on_window_close`) can drive it
    /// without a second parameter — those same call sites already have an
    /// `&ActiveEventLoop` in scope for the one-time lazy creation.
    pub(crate) wisp: WispSlot,
    /// A `ctl drag --paced <ms>` gesture whose waypoints are being advanced
    /// one per `about_to_wait` tick rather than all at once (Task 6's test
    /// machinery). `None` outside a paced drag; at most one at a time — see
    /// `run_ctl_drag`'s "already running" guard. Lives on `Shared` for the
    /// same reason `drag` does: it must survive across many `about_to_wait`
    /// calls, not just the one that received the `ControlMsg::Drag`.
    pub(crate) paced_drag: Option<PacedDrag>,
    /// Windows playing their farewell suck-in (finding #4): a source window
    /// whose last tab just moved away by a drop is REMOVED from
    /// `App::windows`/`window_order` immediately (so index math, focus, and
    /// input routing all see it gone — no half-alive window can corrupt a
    /// follow-up move), but its `WindowState` parks here so the OS window
    /// stays open just long enough (`SUCK_IN_MS`) to play "the window's
    /// content vanishes into the wisp". Ticked+rendered directly by
    /// `about_to_wait` (it's not in `App::windows`, so `RedrawRequested`
    /// can't reach it); dropped — which is what actually closes the OS
    /// window — the tick its morph self-terminates.
    pub(crate) dying_windows: Vec<WindowState>,
}

/// State for an in-flight paced `ctl drag`. Never advanced by a blocking
/// sleep — that would freeze the whole event loop (the wisp's own redraws
/// included), which is exactly the problem `--paced` exists to avoid.
/// Advanced instead from `about_to_wait`, at most one waypoint per tick,
/// once `interval` has elapsed since `last_step`.
pub(crate) struct PacedDrag {
    /// Motion waypoints not yet visited, in order; the LAST one is `(x2,
    /// y2)` from the original request. Never includes the press point
    /// `(x1, y1)` — that already ran synchronously in `run_ctl_drag` before
    /// this was stashed. Popping the final waypoint (leaving this empty)
    /// is what triggers the release-or-cancel tail.
    pub(crate) waypoints: VecDeque<(f64, f64)>,
    /// Wall-clock spacing between waypoints (`--paced <ms>`).
    pub(crate) interval: Duration,
    /// When the last waypoint (or the press, for the first tick) ran.
    pub(crate) last_step: Instant,
    /// The window the whole gesture (press through release) runs on.
    pub(crate) window: WindowId,
    /// Whether the tail, once waypoints are exhausted, is a release or an
    /// Escape-cancel — same meaning as `ControlMsg::Drag`'s `cancel`.
    pub(crate) cancel: bool,
    /// The window's modifiers from before the drag overwrote them with
    /// `--mods`, restored by the tail — same bookkeeping `run_ctl_drag`
    /// does locally for the unpaced case, just kept alive across ticks.
    pub(crate) saved_mods: ModifiersState,
    /// The `ctl drag` client's reply channel — held, unfired, until the
    /// tail runs (the whole point of pacing: the client's `recv` blocks for
    /// the full gesture, same reply shape as the unpaced case).
    pub(crate) reply: Sender<String>,
}

/// The wisp's lazy-creation/degradation state (Task 5). Not `Option<WispWindow>`
/// alone: a GPU/surface that can't do alpha compositing must fail creation
/// EXACTLY ONCE, not be retried every single drag — `Unsupported` remembers
/// that so every later drag just skips the wisp for free, forever (cost
/// discipline).
pub(crate) enum WispSlot {
    /// Never attempted — no drag has been carried yet this process.
    Uninit,
    /// Created and usable. Boxed: `WispWindow` (GPU device/surface/window)
    /// is far larger than `Uninit`/`Unsupported`'s zero bytes, and every
    /// `Shared` — even one that never sees a drag — pays this enum's size.
    Ready(Box<WispWindow>),
    /// Attempted once and failed (see [`WispUnsupported`]) — never retried.
    Unsupported,
}

/// The wisp's own always-on-top, click-through, borderless window + renderer,
/// plus the fade-ramp/velocity bookkeeping `ember-app` drives it with.
/// NEVER inserted into `App::windows`/`Shared::window_order`/`focus_history`
/// — see `window_frames`/`window_event`'s early `self.windows.get_mut` miss,
/// which already no-ops any stray `WindowEvent` (CloseRequested, Focused,
/// spontaneous RedrawRequested) delivered for its `WindowId`. The wisp is
/// never driven by `RedrawRequested` at all: `about_to_wait`'s pacing loop
/// calls `WispWindow::tick` directly, which calls `WispRenderer::render`
/// synchronously (present-per-call, same as `Renderer::render`, just not
/// gated behind a redraw round-trip).
pub(crate) struct WispWindow {
    window: Arc<winit::window::Window>,
    renderer: WispRenderer,
    fade: WispFade,
    /// Hover target for `render`'s `intensity` arg: `1.0` over a drop
    /// target, `0.6` otherwise (`Shared.drag.hover`'s `Some`/`None`, per the
    /// brief). Updated every carried tick by `wisp_tick`; read by `tick`.
    intensity_target: f32,
    /// Previous SCREEN-space (physical px) position + when it was set, for
    /// velocity — derived from successive `DragState::last_screen` deltas
    /// (there's no other velocity signal available).
    prev: Option<((f64, f64), Instant)>,
    /// Smoothed screen-space px/s, fed to `WispRenderer::render`'s trail bias.
    velocity: (f32, f32),
    /// Free-running clock for the particle animation (never reset — same
    /// "procedural from t alone" shape as the backdrop sparks).
    since: Instant,
    /// Last time `tick` actually advanced+rendered a frame (paces at
    /// `WISP_FRAME`, independent of how often `about_to_wait` itself runs).
    last_anim: Instant,
}

/// The wisp's fade-ramp state machine (Task 5 §2: "fade-in ~150ms at
/// tear-off, fade-out ~200ms at drop/cancel").
enum WispFade {
    In {
        start: Instant,
    },
    Steady,
    Out {
        start: Instant,
    },
    /// Not showing; not ticking. The only state `WispWindow::tick` isn't
    /// called in (cost discipline: zero cost while no drag is carried).
    Hidden,
}

/// Wisp redraw cadence (matches `ANIM_FRAME`; the sparks/fade don't need a
/// tighter budget than the rest of the app's animations).
const WISP_FRAME: Duration = Duration::from_millis(16);
/// Fade-in duration at tear-off (Task 5 §2).
const WISP_FADE_IN: Duration = Duration::from_millis(150);
/// Fade-out duration at drop/cancel (Task 5 §2).
const WISP_FADE_OUT: Duration = Duration::from_millis(200);
/// Logical px side length of the wisp window. Started at ~140 (Task 5); bumped
/// to 230 after the first live look read as "barely visible" — the cluster
/// geometry in `wisp_quads` is a fraction of this, so it scales as one knob.
const WISP_SIZE: f32 = 230.0;

impl WispWindow {
    /// Build (and show) a brand-new wisp window + renderer. `Err` on any
    /// failure — OS window creation or `WispRenderer::new`'s own alpha-mode
    /// feature-detection — collapses to the same `WispUnsupported` the
    /// caller degrades on; the wisp is entirely best-effort.
    fn new(event_loop: &ActiveEventLoop) -> Result<Self, WispUnsupported> {
        use winit::dpi::LogicalSize;
        use winit::window::{Window, WindowLevel};

        let attrs = Window::default_attributes()
            .with_title("")
            .with_inner_size(LogicalSize::new(WISP_SIZE, WISP_SIZE))
            .with_decorations(false)
            .with_transparent(true)
            .with_resizable(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_active(false)
            .with_visible(false);
        let window = event_loop
            .create_window(attrs)
            .map_err(|_| WispUnsupported)?;
        // Click-through: the wisp sits directly under the cursor by
        // definition, and must never win the drop hit-test against the real
        // window underneath it. Best-effort (`Result` ignored) — platforms
        // that don't support this (see winit's own doc) just leave hittest
        // on, which only matters if the OS ever routes input to a
        // non-active, `AlwaysOnTop`, decorationless window at all.
        let _ = window.set_cursor_hittest(false);
        let window = Arc::new(window);
        let renderer = WispRenderer::new(Arc::clone(&window))?;
        let now = Instant::now();
        Ok(Self {
            window,
            renderer,
            fade: WispFade::Hidden,
            intensity_target: 0.6,
            prev: None,
            velocity: (0.0, 0.0),
            since: now,
            last_anim: now,
        })
    }

    /// Show the window and start its fade-in ramp. Called once per drag, on
    /// the first carried transition.
    fn show(&mut self) {
        self.window.set_visible(true);
        self.fade = WispFade::In {
            start: Instant::now(),
        };
    }

    /// Start the fade-out ramp (drop/cancel). No-op if already hidden or
    /// already fading out — idempotent, since every drag-end site calls this
    /// unconditionally (most drags never carry far enough to have shown a
    /// wisp at all).
    fn begin_fade_out(&mut self) {
        if matches!(self.fade, WispFade::In { .. } | WispFade::Steady) {
            self.fade = WispFade::Out {
                start: Instant::now(),
            };
        }
    }

    /// Whether `tick` still needs calling (fading in/out, or steady while
    /// carried) — `about_to_wait`'s pacing loop uses this to fold the wisp
    /// into its `next_wake` deadline exactly like a per-window animation.
    fn is_active(&self) -> bool {
        !matches!(self.fade, WispFade::Hidden)
    }

    /// Move the window so it's centered on the given SCREEN-space (physical
    /// px) point, and update the velocity estimate from the delta since the
    /// last call. Called every carried motion tick.
    fn move_to(&mut self, screen_x: f64, screen_y: f64) {
        let sf = self.window.scale_factor();
        let half = (WISP_SIZE as f64 / 2.0) * sf;
        self.window
            .set_outer_position(winit::dpi::PhysicalPosition::new(
                (screen_x - half).round() as i32,
                (screen_y - half).round() as i32,
            ));
        let now = Instant::now();
        if let Some((prev_pos, prev_t)) = self.prev {
            let dt = now.duration_since(prev_t).as_secs_f32();
            if dt > 0.0005 {
                let vx = (screen_x - prev_pos.0) as f32 / dt;
                let vy = (screen_y - prev_pos.1) as f32 / dt;
                // Light smoothing so one noisy tick doesn't whip the trail.
                self.velocity = (
                    self.velocity.0 * 0.5 + vx * 0.5,
                    self.velocity.1 * 0.5 + vy * 0.5,
                );
            }
        }
        self.prev = Some(((screen_x, screen_y), now));
    }

    fn set_target_intensity(&mut self, v: f32) {
        self.intensity_target = v;
    }

    /// Advance the fade ramp and, at the `WISP_FRAME` cadence, render one
    /// frame. Returns `true` if still active afterward (another tick is
    /// needed — fold into `next_wake`), `false` once a fade-out just
    /// finished and the window is hidden again (no more ticking until the
    /// next carried drag: cost discipline).
    fn tick(&mut self, now: Instant) -> bool {
        if now.duration_since(self.last_anim) < WISP_FRAME {
            return self.is_active();
        }
        self.last_anim = now;
        let alpha = match self.fade {
            WispFade::Hidden => return false,
            WispFade::In { start } => {
                let e = now.duration_since(start);
                if e >= WISP_FADE_IN {
                    self.fade = WispFade::Steady;
                    1.0
                } else {
                    e.as_secs_f32() / WISP_FADE_IN.as_secs_f32()
                }
            }
            WispFade::Steady => 1.0,
            WispFade::Out { start } => {
                let e = now.duration_since(start);
                if e >= WISP_FADE_OUT {
                    self.window.set_visible(false);
                    self.fade = WispFade::Hidden;
                    self.prev = None;
                    self.velocity = (0.0, 0.0);
                    return false;
                }
                1.0 - e.as_secs_f32() / WISP_FADE_OUT.as_secs_f32()
            }
        };
        let t = self.since.elapsed().as_secs_f32();
        self.renderer
            .render(t, alpha * self.intensity_target, self.velocity);
        true
    }
}

/// A drag in progress, spanning windows (hence on [`Shared`], not
/// `WindowState`).
pub(crate) struct DragState {
    /// The surface being carried, captured as indices into `window_order` at
    /// the moment the drag tore off the strip (release 1's `SurfaceRef`
    /// indices, not raw ids — a drop re-resolves the WINDOW by `WindowId`
    /// identity via `source_window`/a `DropHover`'s own `window`, never by
    /// trusting these indices to still be valid at drop time).
    pub(crate) surface: SurfaceRef,
    /// The window the drag started in, by identity (not index — an index can
    /// go stale if some other action reorders `window_order` mid-drag).
    pub(crate) source_window: WindowId,
    /// Pointer offset inside the lifted visual, logical (SOURCE window)
    /// px, captured at tear-off. Read by `resolve_drag_drop`'s `Desktop`
    /// arm to place a brand-new window at (roughly) the drop point, minus
    /// this offset — a real offset-from-the-grabbed-corner sprite is Task
    /// 5's wisp; this is the closest existing artifact and reuse is
    /// documented as the deliberate (not accidental) choice here.
    pub(crate) grab: (f64, f64),
    /// Whether the pointer has left the source window's surface (a
    /// cross-window/desktop carry). Monotonic once a drag is torn off: flips
    /// `true` on first exit and never flips back for this drag, even if the
    /// pointer re-enters the source window's own bounds later. Not read by
    /// this task's mechanics (hover/drop resolution don't need it — `hover`
    /// alone is authoritative); kept for Task 5's wisp (visible only while
    /// carried) and as a `ctl`-visible breadcrumb.
    pub(crate) carried: bool,
    /// The current drop target, if any, driving the live preview. `None`
    /// means "release here cancels."
    pub(crate) hover: Option<DropHover>,
    /// Last known SCREEN-space (physical px, across every window) cursor
    /// position, updated every motion tick once torn off by
    /// `update_cross_window_drag`. Read by `resolve_drag_drop`'s `Desktop`
    /// arm for the new window's position hint.
    pub(crate) last_screen: (f64, f64),
    /// The window most recently raised because the carry hovered it. Tracked
    /// EXPLICITLY (not derived from `hover`, which flips `None` over a
    /// window's body outside any pane/strip) so the raise fires exactly once
    /// per target change. Deriving it from `hover` re-raised on every motion
    /// tick over body regions — an activation storm that ballooned the event
    /// queue to gigabytes and froze the app in the first live session.
    pub(crate) last_raised: Option<WindowId>,
    /// The carried surface's tab title, captured ONCE at tear-off (v0.4.0) —
    /// threaded into a target window's ghost tab (`WindowState::
    /// set_incoming_drop`) when cheaply available. Empty for the rare cases
    /// with no title at hand yet; the ghost falls back to a static glyph
    /// then (see `set_incoming_drop`'s doc).
    pub(crate) title: String,
}

/// Where a carried surface would land, driving the live drag preview.
#[derive(Clone, Copy, Debug)]
pub(crate) enum DropHover {
    /// Hovering a window's tab strip: dropping here reorders/re-inserts the
    /// carried tab at `insert_at`. `chip` is `Some(i)` while the pointer sits
    /// directly over an EXISTING tab `i`'s own chip (as opposed to the
    /// trailing ghost/append segment, when one is showing) — spring-loaded
    /// tab-select (`WindowState::spring_load_hover`, finding #2) only fires
    /// on a `chip` hover, so skating past the ghost's own segment never
    /// flips the displayed tab. `insert_at` keeps its pre-existing meaning
    /// unchanged (a real tab index either way — see
    /// `WindowState::hover_at`'s doc).
    Strip {
        window: WindowId,
        insert_at: usize,
        chip: Option<usize>,
    },
    /// Hovering a pane: dropping here either splits `pane` (an `Edge` zone)
    /// or appends as a new tab of `window` (`Center`) — see
    /// `ember_core::DropZone`.
    Pane {
        window: WindowId,
        tab: usize,
        pane: PaneId,
        zone: ember_core::DropZone,
    },
    /// Hovering neither a tracked window's strip nor one of its panes: a
    /// cross-window "drop on the desktop" target — release 2 task 3's
    /// addition. Dropping here opens a brand-new window (`SurfaceDest::
    /// NewWindow`) at (roughly) the drop point.
    Desktop,
}

/// A close action deferred behind a running-process confirmation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PendingClose {
    /// Close the focused pane (Cmd+W).
    Pane,
    /// Close a whole tab by id (middle-click).
    Tab(TabId),
    /// Quit the whole app (Cmd+Q / the OS close button on the LAST window).
    Quit,
    /// Close just this one window (the OS close button on a non-last window).
    /// Other windows' sessions are untouched.
    CloseWindow,
}

/// What the caller must do after a debug-control command resolved a pending
/// close confirmation (`WindowState::handle_control` can't decide this itself —
/// it has no view of how many windows exist).
pub(crate) enum ControlClose {
    /// Quit the whole app (all windows, all sessions) — the confirmed kind was
    /// [`PendingClose::Quit`].
    ExitApp,
    /// Close just this window, unless it turns out to be the last one (then
    /// quit) — see [`finish_close`].
    CloseWindow,
}

/// Inset a rect by `p` on every side (clamped to stay positive).
pub(crate) fn inset(r: Rect, p: f64) -> Rect {
    Rect::new(
        r.x + p,
        r.y + p,
        (r.width - 2.0 * p).max(1.0),
        (r.height - 2.0 * p).max(1.0),
    )
}

/// Load + decode a backdrop image (PNG) from `path` into `(rgba8, w, h)`.
/// Forgiving: a missing/unreadable/non-PNG path yields `None` (no image).
pub(crate) fn load_backdrop_image(path: &str) -> Option<(Vec<u8>, u32, u32)> {
    let bytes = std::fs::read(path).ok()?;
    ember_platform::decode_png_rgba(&bytes)
}

/// Cell grid that fits an inner pixel rect.
pub(crate) fn dims_for_rect(r: Rect, cw: f32, ch: f32) -> GridDims {
    let cols = ((r.width as f32 / cw).floor() as i64).clamp(1, u16::MAX as i64);
    let rows = ((r.height as f32 / ch).floor() as i64).clamp(1, u16::MAX as i64);
    GridDims::new(cols as u16, rows as u16)
}

impl ApplicationHandler<EmberEvent> for App {
    /// A wake nudge (frame ready / control command): the real work happens in
    /// `about_to_wait`, which drains the lanes and requests a redraw — this just
    /// ensures the loop ran a cycle.
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: EmberEvent) {}

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.shared.is_some() {
            return;
        }
        let w = DEFAULT_COLS as f32 * CELL_WIDTH + 2.0 * PAD;
        let h = DEFAULT_ROWS as f32 * CELL_HEIGHT + 2.0 * PAD;
        let attrs = ember_platform::window_attributes("Ember", w, h);
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        ember_platform::set_app_icon(&window, ICON_PNG);
        let window_id = window.id();

        let size = window.inner_size();
        let px = (size.width.max(1), size.height.max(1));
        let config = config::load();
        let renderer = Renderer::new(Arc::clone(&window), &config.font);

        // The seed tab: one pane backed by one shell.
        let pane = PaneId(1);
        let session = SessionId::new("s1");
        let tree = ember_core::WindowTree {
            tabs: vec![Tab {
                id: TabId(1),
                title: String::new(),
                root: LayoutNode::pane(pane, session.clone()),
                focus: pane,
            }],
            active: 0,
        };

        let mut shared = Shared {
            sessions: HashMap::new(),
            session_window: HashMap::new(),
            window_order: vec![window_id],
            next_pane: 2,
            next_session: 2,
            next_tab: 2,
            control_rx: self.control_rx.take(),
            control_server: self.control_server.take(),
            config,
            backdrop_since: Instant::now(),
            menu: ember_platform::build_menu(),
            platform: ember_platform::HostBackend::default(),
            power_state: Cell::new(None),
            bracketed: HashMap::new(),
            focus_notified: None,
            mouse_press: None,
            last_mouse_cell: None,
            titles: std::collections::HashMap::new(),
            cwd_by_session: std::collections::HashMap::new(),
            wake: self.wake.clone(),
            drag: None,
            new_window_position_hint: None,
            wisp: WispSlot::Uninit,
            paced_drag: None,
            dying_windows: Vec::new(),
        };
        let mut win = WindowState::new(renderer, tree);
        win.px = px;
        if !win.spawn_session(
            &mut shared,
            session,
            GridDims::new(DEFAULT_COLS, DEFAULT_ROWS),
            None,
        ) {
            // No shell at startup means nothing to show; exit with the message
            // spawn_session already printed instead of presenting a dead window.
            std::process::exit(1);
        }
        win.sync_layout(&shared);
        win.apply_appearance(&shared);
        if shared.config.developer_mode && shared.control_server.is_none() {
            shared.set_developer_mode(true);
        }
        // Paint once now: with ControlFlow::Wait the loop won't run again until
        // an event or a frame-lane wake, and the very first frame may have been
        // published before the waker was registered.
        win.renderer.window().request_redraw();
        self.windows.insert(window_id, win);
        self.focused_window = Some(window_id);
        self.focus_history.push(window_id);
        self.shared = Some(shared);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        // Read before `win` borrows `self.windows` mutably below — used only
        // by `CloseRequested` to decide "quit the app" vs. "close just this
        // window".
        let is_last_window = self.windows.len() <= 1;
        let Some(shared) = self.shared.as_mut() else {
            return;
        };
        let Some(win) = self.windows.get_mut(&id) else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => {
                // The OS close button on the LAST window keeps today's quit
                // behavior (checks every session app-wide); on any other
                // window it closes just that one (checks only ITS sessions).
                let kind = if is_last_window {
                    PendingClose::Quit
                } else {
                    PendingClose::CloseWindow
                };
                if win.request_close(shared, kind) {
                    finish_close(
                        &mut self.windows,
                        shared,
                        &mut self.focused_window,
                        event_loop,
                        id,
                    );
                }
            }
            WindowEvent::Resized(size) => {
                win.px = (size.width.max(1), size.height.max(1));
                win.renderer.resize(win.px.0, win.px.1);
                win.sync_layout(shared);
            }
            WindowEvent::Focused(focused) => {
                win.window_focused = focused;
                if focused {
                    // The window the OS gave keyboard focus to becomes the
                    // target for keystrokes, ctl commands, and (per-window)
                    // shortcuts.
                    self.focused_window = Some(id);
                    self.focus_history.retain(|w| *w != id);
                    self.focus_history.insert(0, id);
                    // A focused window is never occluded. Focus events are the
                    // reliable reveal signal when an Occluded(false) got lost
                    // (e.g. around display sleep/unlock), so also clear the
                    // renderer's starve throttle before the repaint.
                    win.occluded = false;
                    win.renderer.surface_revealed();
                    win.renderer.window().request_redraw();
                }
            }
            WindowEvent::Occluded(occluded) => {
                win.occluded = occluded;
                if !occluded {
                    // Lift the renderer's starve throttle BEFORE requesting the
                    // reveal repaint, so it isn't swallowed by the holdoff.
                    win.renderer.surface_revealed();
                    win.renderer.window().request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(mods) => {
                win.modifiers = mods.state();
                // Releasing Ctrl+Opt hides the split drop-zone preview.
                if !win.split_modifier_held() {
                    win.clear_split_preview();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let sf = win.renderer.window().scale_factor();
                let (x, y) = (position.x / sf, position.y / sf);
                win.on_cursor_moved(shared, id, x, y);
                // `win`'s borrow of `self.windows` ends at the statement
                // above (unused for the rest of this arm) — the same NLL
                // shape `apply_move`'s callers rely on elsewhere — so this
                // can reborrow `self.windows` as a whole for the
                // cross-window drag tracker (release 2 task 3).
                if shared.drag.is_some() {
                    update_cross_window_drag(
                        &mut self.windows,
                        shared,
                        &self.focus_history,
                        id,
                        x,
                        y,
                        event_loop,
                    );
                }
            }
            // Cursor left the window — drop any tab hover so the highlight/"✕"
            // don't linger.
            WindowEvent::CursorLeft { .. } => {
                win.renderer.set_hovered_tab(None);
                win.renderer.set_hovered_link(None);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // Notch wheels scroll WHEEL_LINES per notch; trackpads report
                // pixel-precise deltas that map 1:1 to cells (no multiplier).
                // Accumulate fractions so slow two-finger drags still move.
                let cells = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y * WHEEL_LINES as f32,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / CELL_HEIGHT,
                };
                win.wheel_accum += cells;
                let lines = win.wheel_accum.trunc() as i32;
                win.wheel_accum -= lines as f32;
                win.wheel_scroll(shared, lines);
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button,
                ..
            } => match button {
                MouseButton::Left => {
                    let (x, y) = win.cursor;
                    // A blocking confirm modal captures the click: a button
                    // resolves it, elsewhere is a no-op (stays modal).
                    if win.pending_close.is_some() {
                        if let Some(idx) = win.renderer.confirm_button_at(x as f32, y as f32) {
                            let kind = win.pending_close;
                            if win.resolve_confirm(shared, idx == 1) {
                                match kind {
                                    Some(PendingClose::Quit) => {
                                        shared.shutdown_all();
                                        event_loop.exit();
                                    }
                                    _ => finish_close(
                                        &mut self.windows,
                                        shared,
                                        &mut self.focused_window,
                                        event_loop,
                                        id,
                                    ),
                                }
                            }
                        }
                    } else {
                        win.press_left(shared, x, y);
                    }
                }
                // Middle-click on a tab closes it (standard gesture); elsewhere
                // it forwards to a mouse-aware app.
                MouseButton::Middle => {
                    let (x, y) = win.cursor;
                    if let Some(TabHit::Tab(i)) = win.renderer.tab_hit(x as f32, y as f32) {
                        if let Some(tab_id) = win.tree.tabs.get(i).map(|t| t.id) {
                            if win.request_close(shared, PendingClose::Tab(tab_id)) {
                                finish_close(
                                    &mut self.windows,
                                    shared,
                                    &mut self.focused_window,
                                    event_loop,
                                    id,
                                );
                            }
                        }
                    } else {
                        win.forward_mouse_press(shared, 1);
                    }
                }
                MouseButton::Right => {
                    win.forward_mouse_press(shared, 2);
                }
                _ => {}
            },
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button,
                ..
            } => {
                win.forward_mouse_release(
                    shared,
                    match button {
                        MouseButton::Left => 0,
                        MouseButton::Middle => 1,
                        MouseButton::Right => 2,
                        _ => 0,
                    },
                );
                if button == MouseButton::Left {
                    let was_dragging = shared.drag.is_some() || win.tab_drag.is_some();
                    let ended = win.left_release(shared, id);
                    // A pane drop only STAGES its move (`WindowState::
                    // pending_move`) — run it through the same canonical
                    // `apply_move` path `ctl drag` (`run_ctl_drag`) and every
                    // other surface-mobility gesture uses, right here: `win`
                    // isn't touched again in this arm, so its borrow of
                    // `self.windows` ends at the `take()` below, freeing
                    // `self.windows` for `apply_move` to reborrow whole.
                    let pending = win.pending_move.take();
                    if was_dragging {
                        // Every window's incoming-drop/preview visual is
                        // stale the instant the drag resolves. The ctl-drag
                        // tail (`finish_ctl_drag_tail`) has always swept
                        // here; the real-mouse path missing the same sweep
                        // left stuck split-preview bands on the target
                        // window after a successful drop — the first bug a
                        // human hand found that no scripted drive could.
                        clear_all_drag_visuals(&mut self.windows);
                    }
                    if let Some((src, dest)) = pending {
                        match apply_move(
                            &mut self.windows,
                            shared,
                            &mut self.focused_window,
                            event_loop,
                            src,
                            dest,
                        ) {
                            Ok(()) => {
                                // Pour-out (v0.4.0): the landed window's own
                                // renderer plays the "pours OUT... into the
                                // landed surface's rect" morph.
                                if ended == DragEnded::Move {
                                    pour_out_after_move(
                                        &mut self.windows,
                                        shared,
                                        self.focused_window,
                                    );
                                }
                            }
                            Err(e) => eprintln!("[ember] drag drop rejected: {e}"),
                        }
                    }
                }
            }
            WindowEvent::KeyboardInput { event: key, .. } => {
                if key.state != ElementState::Pressed {
                    return;
                }
                // A torn-off surface drag captures input: Escape cancels it
                // (zero mutation), everything else is swallowed — mirrors
                // the close-confirm modal below, and matches real drag UX
                // (no keyboard shortcuts fire mid-drag).
                if shared.drag.is_some() {
                    if !key.repeat && matches!(key.logical_key, Key::Named(NamedKey::Escape)) {
                        cancel_drag_everywhere(&mut self.windows, shared);
                    }
                    return;
                }
                // The Settings overlay is interactive — it handles its own keys
                // (arrows / space / esc) rather than dismissing on any key.
                if win.settings_open {
                    win.settings_key(shared, &key.logical_key);
                    return;
                }
                // Inline tab rename captures typing, but NOT Cmd combos — those
                // stay app shortcuts (Cmd+W must not insert "w"), so fall through
                // to the Super branch below when Cmd is held.
                if win.editing_tab.is_some() && !win.modifiers.super_key() {
                    win.rename_key(shared, &key.logical_key);
                    return;
                }
                // A running-process close confirmation (modal): Left/Right/Tab
                // move focus, Enter activates it, Esc cancels. Auto-repeat is
                // ignored so a held key can't confirm.
                if win.pending_close.is_some() {
                    if !key.repeat {
                        match &key.logical_key {
                            Key::Named(NamedKey::Escape) => {
                                win.resolve_confirm(shared, false);
                            }
                            Key::Named(NamedKey::Enter) => {
                                let ok = win.confirm_focus == 1;
                                let kind = win.pending_close;
                                if win.resolve_confirm(shared, ok) {
                                    match kind {
                                        Some(PendingClose::Quit) => {
                                            shared.shutdown_all();
                                            event_loop.exit();
                                        }
                                        _ => finish_close(
                                            &mut self.windows,
                                            shared,
                                            &mut self.focused_window,
                                            event_loop,
                                            id,
                                        ),
                                    }
                                }
                            }
                            Key::Named(
                                NamedKey::ArrowLeft | NamedKey::ArrowRight | NamedKey::Tab,
                            ) => {
                                win.confirm_focus ^= 1;
                                win.update_confirm_view();
                                win.renderer.window().request_redraw();
                            }
                            _ => {}
                        }
                    }
                    return;
                }
                // Cmd+Q — quit (with confirmation if a command is running). Handled
                // here so it exits regardless of tab count, unlike pane shortcuts.
                if win.modifiers.super_key()
                    && matches!(&key.logical_key, Key::Character(c) if c.eq_ignore_ascii_case("q"))
                {
                    if win.request_close(shared, PendingClose::Quit) {
                        shared.shutdown_all();
                        event_loop.exit();
                    }
                    return;
                }
                // While a modal overlay (help / About) is up, the next *fresh*
                // key dismisses it. Escape/Enter just dismiss; any other key
                // dismisses AND falls through so the keystroke still reaches the
                // shell (typing `ls` at the help screen shouldn't eat the `l`).
                // Auto-repeat is ignored so holding Cmd+/ can't close on open.
                if win.help || win.about {
                    if key.repeat {
                        return;
                    }
                    win.dismiss_overlay();
                    let swallow = matches!(
                        &key.logical_key,
                        Key::Named(NamedKey::Escape) | Key::Named(NamedKey::Enter)
                    ) || win.modifiers.super_key();
                    if swallow {
                        return;
                    }
                    // else: fall through and process this key normally.
                }
                let mods = win.modifiers;
                if std::env::var_os("EMBER_DEBUG").is_some() {
                    eprintln!(
                        "[ember-key] {:?} super={} shift={} alt={} ctrl={}",
                        key.logical_key,
                        mods.super_key(),
                        mods.shift_key(),
                        mods.alt_key(),
                        mods.control_key()
                    );
                }
                // Super (Cmd/Win) combos are multiplexer shortcuts — consumed, never
                // forwarded to the shell.
                if mods.super_key() {
                    // Cmd+Shift+N — move the focused tab to a brand-new
                    // window. Checked BEFORE the plain Cmd+N below: a shifted
                    // "N" still matches that character check, and Move Tab
                    // to New Window is the more specific chord. Needs
                    // `event_loop`/every window's state, same as Cmd+N.
                    if mods.shift_key()
                        && matches!(&key.logical_key, Key::Character(c) if c.eq_ignore_ascii_case("n"))
                    {
                        if let Ok((src, dest)) = build_move_tab(win, shared, id, MoveTabTarget::New)
                        {
                            if let Err(e) = apply_move(
                                &mut self.windows,
                                shared,
                                &mut self.focused_window,
                                event_loop,
                                src,
                                dest,
                            ) {
                                eprintln!("[ember] move tab to new window: {e}");
                            }
                        }
                        return;
                    }
                    // Cmd+N — open a new window (fresh tab, cwd inherited from
                    // the focused pane). Handled here, not in `handle_shortcut`,
                    // because it needs `event_loop` (window creation) and every
                    // other window's state — neither is available to a
                    // `WindowState` method.
                    if matches!(&key.logical_key, Key::Character(c) if c.eq_ignore_ascii_case("n"))
                    {
                        let cwd = win
                            .focused_session_id()
                            .and_then(|sid| shared.cwd_by_session.get(&sid).cloned());
                        open_new_window(
                            &mut self.windows,
                            shared,
                            &mut self.focused_window,
                            event_loop,
                            cwd,
                        );
                        return;
                    }
                    // Cmd+Opt+T — promote the focused pane to its own tab.
                    // Opt(=Alt) combos don't otherwise route through
                    // `handle_shortcut` (that method only ever sees Super-only
                    // or Linux-translated chords), so this is checked here,
                    // both for that reason and because it's cross-window like
                    // Cmd+N above.
                    if mods.alt_key()
                        && matches!(&key.logical_key, Key::Character(c) if c.eq_ignore_ascii_case("t"))
                    {
                        if let Ok((src, dest)) =
                            build_promote_pane(win, shared, id, PromotePaneTarget::Tab)
                        {
                            if let Err(e) = apply_move(
                                &mut self.windows,
                                shared,
                                &mut self.focused_window,
                                event_loop,
                                src,
                                dest,
                            ) {
                                eprintln!("[ember] promote pane to tab: {e}");
                            }
                        }
                        return;
                    }
                    if win.handle_shortcut(shared, &key.logical_key, mods)
                        && win.tree.tabs.is_empty()
                    {
                        finish_close(
                            &mut self.windows,
                            shared,
                            &mut self.focused_window,
                            event_loop,
                            id,
                        );
                    }
                    return;
                }
                // Scrollback navigation: Shift+PageUp/Down (page), Shift+Home/End
                // (top/bottom). No-op on the alt screen (the projection gates it).
                if mods.shift_key() {
                    let amt = match &key.logical_key {
                        Key::Named(NamedKey::PageUp) => Some(ScrollAmount::PageUp),
                        Key::Named(NamedKey::PageDown) => Some(ScrollAmount::PageDown),
                        Key::Named(NamedKey::Home) => Some(ScrollAmount::Top),
                        Key::Named(NamedKey::End) => Some(ScrollAmount::Bottom),
                        _ => None,
                    };
                    if let Some(a) = amt {
                        win.scroll_focused(shared, a);
                        return;
                    }
                }
                // Linux tab jump: Alt+1..9 (the gnome-terminal convention).
                // GNOME binds Super+digits itself (dash favorites), so on
                // GNOME those never reach us; Super+N still works under WMs
                // that deliver it. Tabs win over Alt-as-Meta digits, matching
                // gnome-terminal's default.
                #[cfg(target_os = "linux")]
                if let Some(n) = alt_digit_tab(&key.logical_key, mods) {
                    win.select_tab(shared, n);
                    return;
                }
                // The GNOME-safe conventional chords (Ctrl+Shift+X, Alt+Shift+X)
                // translate onto the same shortcut handler Super uses.
                #[cfg(target_os = "linux")]
                if let Some((k, m)) = linux_chord_translate(&key.logical_key, mods) {
                    // Ctrl+Shift+N == Cmd+N (new window); Alt+Shift+N ==
                    // Cmd+Shift+N (move tab to new window) — `linux_chord_translate`
                    // tells the two apart by leaving SHIFT on the latter's
                    // translated modifiers. Same event_loop/cross-window need
                    // as the macOS branch above.
                    if matches!(&k, Key::Character(c) if c.eq_ignore_ascii_case("n")) {
                        if m.shift_key() {
                            if let Ok((src, dest)) =
                                build_move_tab(win, shared, id, MoveTabTarget::New)
                            {
                                if let Err(e) = apply_move(
                                    &mut self.windows,
                                    shared,
                                    &mut self.focused_window,
                                    event_loop,
                                    src,
                                    dest,
                                ) {
                                    eprintln!("[ember] move tab to new window: {e}");
                                }
                            }
                        } else {
                            let cwd = win
                                .focused_session_id()
                                .and_then(|sid| shared.cwd_by_session.get(&sid).cloned());
                            open_new_window(
                                &mut self.windows,
                                shared,
                                &mut self.focused_window,
                                event_loop,
                                cwd,
                            );
                        }
                        return;
                    }
                    // Alt+Shift+T == Cmd+Opt+T (promote pane to tab) —
                    // `linux_chord_translate` leaves ALT on the translated
                    // modifiers for this one key precisely so it's
                    // distinguishable from Ctrl+Shift+T's plain "new tab".
                    if m.alt_key() && matches!(&k, Key::Character(c) if c.eq_ignore_ascii_case("t"))
                    {
                        if let Ok((src, dest)) =
                            build_promote_pane(win, shared, id, PromotePaneTarget::Tab)
                        {
                            if let Err(e) = apply_move(
                                &mut self.windows,
                                shared,
                                &mut self.focused_window,
                                event_loop,
                                src,
                                dest,
                            ) {
                                eprintln!("[ember] promote pane to tab: {e}");
                            }
                        }
                        return;
                    }
                    if win.handle_shortcut(shared, &k, m) && win.tree.tabs.is_empty() {
                        finish_close(
                            &mut self.windows,
                            shared,
                            &mut self.focused_window,
                            event_loop,
                            id,
                        );
                    }
                    return;
                }
                // DECCKM from the focused pane; Option-as-Meta strips the
                // macOS compose (Opt+b = "∫") back to the plain key for the
                // ESC prefix. With the option off, composing wins (é, ñ).
                let app_cursor = win.focused_app_cursor();
                let alt_meta = mods.alt_key() && shared.config.option_as_meta;
                let logical = if alt_meta {
                    key.key_without_modifiers()
                } else {
                    key.logical_key.clone()
                };
                if let Some(bytes) = encode_key(&logical, mods, app_cursor, alt_meta) {
                    if let Some(h) = win.focused_session(shared) {
                        let _ = h
                            .control
                            .send(BackendControl::Input(bytes.into_boxed_slice()));
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                win.drain_own_frames(shared, id);
                // Frame-timing for the FPS overlay: cadence (interval between
                // redraws) + the render() call's own duration (the per-frame cost).
                let now = Instant::now();
                if let Some(last) = win.last_frame {
                    let dt = now.duration_since(last).as_secs_f32() * 1000.0;
                    win.fps_ema_ms = if win.fps_ema_ms == 0.0 {
                        dt
                    } else {
                        win.fps_ema_ms * 0.9 + dt * 0.1
                    };
                }
                win.last_frame = Some(now);
                if win.fps_overlay {
                    let fps = if win.fps_ema_ms > 0.0 {
                        1000.0 / win.fps_ema_ms
                    } else {
                        0.0
                    };
                    win.renderer.set_fps_overlay(Some(format!(
                        "{fps:.0} fps · {:.1} ms",
                        win.render_ema_ms
                    )));
                }
                let t = Instant::now();
                match win.renderer.render() {
                    // A drawable came through — the surface is ground truth, so
                    // whatever winit last said, we are visible.
                    RenderOutcome::Presented => {
                        win.occluded = false;
                        win.render_starved = false;
                    }
                    // Surface lost/outdated: it was reconfigured but this frame
                    // never presented — repaint now, not at the next input.
                    RenderOutcome::Retry => {
                        win.render_starved = false;
                        win.renderer.window().request_redraw();
                    }
                    // Starved (no drawable): do NOT re-request here — that loop
                    // is the  OOM spin — and do NOT latch state.occluded: a
                    // transient drawable shortage (startup burst) also lands
                    // here, and latching froze a fully VISIBLE window until the
                    // user clicked it. Durable occlusion state comes from winit's
                    // Occluded events; this flag makes about_to_wait retry on a
                    // bounded cadence instead.
                    RenderOutcome::Starved => win.render_starved = true,
                }
                let render_ms = t.elapsed().as_secs_f32() * 1000.0;
                win.render_ema_ms = if win.render_ema_ms == 0.0 {
                    render_ms
                } else {
                    win.render_ema_ms * 0.9 + render_ms * 0.1
                };
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(shared) = self.shared.as_mut() else {
            return;
        };

        // Poll every session's pixel lane, routing each one's deltas to the
        // window that actually owns it (`session_window`) — "drain only the
        // focused window" (the pre-multi-window shape of this function) would
        // silently drop a background window's frames: that renderer never
        // called `ensure_pane` for a session it doesn't own, so `apply_delta`
        // would find nothing to patch. A background window still gets its
        // grid updated even while occluded/unfocused; it just isn't asked to
        // repaint (Task 6 soak-tests that idle-but-visible windows don't spin).
        let mut redraw_windows: Vec<WindowId> = Vec::new();
        for (wid, w) in self.windows.iter_mut() {
            if w.drain_own_frames(shared, *wid) && !w.occluded {
                redraw_windows.push(*wid);
            }
        }
        for wid in redraw_windows {
            if let Some(w) = self.windows.get(&wid) {
                w.renderer.window().request_redraw();
            }
        }

        let Some(focused_id) = self.focused_window else {
            return;
        };
        let Some(win) = self.windows.get_mut(&focused_id) else {
            return;
        };
        // Drain the semantic lanes: focused-pane title, and any exited shells.
        let focused = win.focused_session_id();
        let mut new_title: Option<String> = None;
        let mut exited: Vec<SessionId> = Vec::new();
        let mut belled: Vec<SessionId> = Vec::new();
        let mut clipboard_set: Option<String> = None;
        let mut title_updates: Vec<(SessionId, String)> = Vec::new();
        // OSC 1337 `CurrentDir=` per session — a new split spawned from this
        // pane inherits the latest one seen. `RemoteHost` isn't consumed yet
        // (no UI surfaces it); tracked here anyway so the protocol is complete
        // and a future feature (tab title, triggers) can read it.
        let mut cwd_updates: Vec<(SessionId, String)> = Vec::new();
        for (id, handle) in &shared.sessions {
            while let Ok(event) = handle.events.try_recv() {
                match event {
                    BackendEvent::Title(t) => {
                        title_updates.push((id.clone(), t.clone()));
                        if Some(id) == focused.as_ref() {
                            new_title = Some(t);
                        }
                    }
                    BackendEvent::Exited(_) => exited.push(id.clone()),
                    BackendEvent::Bell => belled.push(id.clone()),
                    // OSC 52 copy from any pane (tmux/nvim-over-ssh).
                    BackendEvent::Clipboard(ClipboardOp::Set(text)) => {
                        clipboard_set = Some(text);
                    }
                    BackendEvent::Osc(OscEvent::CurrentDir(path)) => {
                        cwd_updates.push((id.clone(), path));
                    }
                    _ => {}
                }
            }
        }
        for (id, title) in title_updates {
            shared.titles.insert(id, title);
        }
        // Drop titles for sessions that no longer exist.
        shared
            .titles
            .retain(|id, _| shared.sessions.contains_key(id));
        for (id, cwd) in cwd_updates {
            shared.cwd_by_session.insert(id, cwd);
        }
        shared
            .cwd_by_session
            .retain(|id, _| shared.sessions.contains_key(id));
        if let Some(text) = clipboard_set {
            shared.platform.set_clipboard(&text);
        }
        if let Some(title) = new_title {
            win.renderer.window().set_title(&title);
        }
        // Route each exited/belled session to the window that actually owns
        // it (it may not be the focused one), falling back to the focused
        // window for a session `session_window` never learned about (a spawn
        // racing this drain).
        // Any window whose tree just emptied because its last shell exited —
        // tracked so it can be torn down below, once `deferred_windows`
        // exists. A BACKGROUND window can end up here (the whole point of
        // this bug fix): the organic tabs-empty check further down only ever
        // looks at `focused_id`, so nothing else notices one of these.
        let mut emptied_windows: Vec<WindowId> = Vec::new();
        for session in exited {
            let wid = shared
                .session_window
                .get(&session)
                .copied()
                .unwrap_or(focused_id);
            if let Some(w) = self.windows.get_mut(&wid) {
                if w.close_session(shared, &session) {
                    emptied_windows.push(wid);
                }
            }
        }
        for session in belled {
            let wid = shared
                .session_window
                .get(&session)
                .copied()
                .unwrap_or(focused_id);
            if let Some(w) = self.windows.get_mut(&wid) {
                w.on_bell(shared, &session);
            }
        }
        // Neither loop above touches the focused window through `win` (each
        // routes to the owning window fresh), so re-borrow it once here for
        // everything below — closing a background window's last pane/session
        // never removes the focused window itself.
        let win = self.windows.get_mut(&focused_id).expect("still open");
        // Focus reporting (DEC 1004): tell sessions when their pane gains or
        // loses focus (pane switch, tab switch, window focus/blur).
        let focus_now = if win.window_focused { focused } else { None };
        if focus_now != shared.focus_notified {
            if let Some(old) = shared.focus_notified.take() {
                if let Some(h) = shared.sessions.get(&old) {
                    let _ = h.control.send(BackendControl::Focus(false));
                }
            }
            if let Some(new) = &focus_now {
                if let Some(h) = shared.sessions.get(new) {
                    let _ = h.control.send(BackendControl::Focus(true));
                }
            }
            // Re-assert the newly focused pane's title (or the app name if it
            // hasn't set one) so a tab/pane switch never leaves a stale title.
            let title = focus_now
                .as_ref()
                .and_then(|id| shared.titles.get(id).cloned())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| "Ember".to_string());
            win.renderer.window().set_title(&title);
            shared.focus_notified = focus_now;
        }
        // Window-structural actions (open/close a whole window) can't run
        // until `win`'s borrow of `self.windows` ends — inserting/removing a
        // window while `win` still points into that same map would conflict —
        // so a `new-window`/close resolution just records what to do, applied
        // once at the very end of this function (`win`'s true last use).
        //
        // A `Vec`, not a single `Option`: up to three sites below can each
        // want to enqueue an action in the very same tick (the ctl-commands
        // loop, native-menu handling, and the organic tabs-empty check
        // further down) — a single slot let a later write silently clobber
        // an earlier one (e.g. a batched `ctl new-window` racing a
        // close-confirm resolution landing in the same drain), dropping the
        // clobbered action entirely. Every queued action is processed, in
        // write order, at the tail once `win`'s borrow ends.
        let mut deferred_windows: Vec<DeferredWindowAction> = Vec::new();

        // Tear down every BACKGROUND window emptied by the exited-shell drain
        // above. `focused_id`'s own case is deliberately excluded here — it
        // still falls through to the organic `win.tree.tabs.is_empty()` check
        // further down, which already queues `CloseThis` for it (and dedups
        // against this same tick's other close requests); handling it twice,
        // once here and once there, would just be redundant, not wrong, but
        // there's no need for two code paths to agree on one window.
        for wid in emptied_windows {
            if wid != focused_id {
                queue_close_window(&mut deferred_windows, wid);
            }
        }

        // Drain debug-control commands (EMBER_CONTROL) and act on them.
        let cmds: Vec<ControlMsg> = shared
            .control_rx
            .as_ref()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default();
        for cmd in cmds {
            if matches!(cmd, ControlMsg::NewWindow) {
                let cwd = win
                    .focused_session_id()
                    .and_then(|sid| shared.cwd_by_session.get(&sid).cloned());
                deferred_windows.push(DeferredWindowAction::OpenNew(cwd));
                continue;
            }
            // The three surface-mobility ctl verbs: same reasoning as
            // `NewWindow` above (`apply_move` needs `self.windows`/
            // `event_loop`, neither reachable from `WindowState`), but these
            // reply with a real `{ok}`/`{ok:false,error}` line, so the
            // reply channel rides along in the deferred action and gets
            // answered once `apply_move` actually runs at the tail.
            //
            // Only `focused_id` (a `WindowId`) and the symbolic op/target are
            // captured here — NOT a `build_*`-resolved `SurfaceRef`/
            // `SurfaceDest` pair. Those carry raw `window_order` indices, and
            // an earlier same-batch `Move` (processed first at the tail, in
            // write order) can shift that order before this one runs, which
            // would silently misroute it onto the wrong window. Deferring the
            // `build_*` call itself to the tail — once every earlier action in
            // this batch has already mutated `window_order` — re-resolves the
            // window index (and, for `Next`/`Prev`, the neighbor) against the
            // order as it stands then.
            if let ControlMsg::MoveTab(target, reply) = cmd {
                deferred_windows.push(DeferredWindowAction::Move(
                    focused_id,
                    DeferredMoveOp::MoveTab(target),
                    Some(reply),
                ));
                continue;
            }
            if let ControlMsg::PromotePane(target, reply) = cmd {
                deferred_windows.push(DeferredWindowAction::Move(
                    focused_id,
                    DeferredMoveOp::PromotePane(target),
                    Some(reply),
                ));
                continue;
            }
            if let ControlMsg::MergeTab(reply) = cmd {
                deferred_windows.push(DeferredWindowAction::Move(
                    focused_id,
                    DeferredMoveOp::MergeTab,
                    Some(reply),
                ));
                continue;
            }
            // `state` and `focus` (Task 5): same reasoning as the surface-
            // mobility verbs above — both need every window in `self.windows`
            // (`state` to list them all, `focus` to search/raise across all
            // of them), neither reachable while `win` still borrows
            // `self.windows` for the rest of this tick.
            if let ControlMsg::State(reply) = cmd {
                deferred_windows.push(DeferredWindowAction::State(reply));
                continue;
            }
            if let ControlMsg::Focus(query, reply) = cmd {
                deferred_windows.push(DeferredWindowAction::Focus(query, reply));
                continue;
            }
            // `ctl drag`: press/motion*/release must all run as one
            // sequence (there's no real per-event `WindowEvent` to hang each
            // step off), and — like the surface-mobility verbs above — a
            // later task's cross-window drop will need `self.windows`, not
            // reachable while `win` still borrows it. Deferred to the tail
            // even though this task's own in-window resolution doesn't
            // strictly need that access, so Tasks 3-6 don't have to move
            // this dispatch again.
            if let ControlMsg::Drag {
                x1,
                y1,
                x2,
                y2,
                steps,
                mods,
                cancel,
                paced_ms,
                reply,
            } = cmd
            {
                deferred_windows.push(DeferredWindowAction::Drag {
                    window: focused_id,
                    x1,
                    y1,
                    x2,
                    y2,
                    steps,
                    mods,
                    cancel,
                    paced_ms,
                    reply,
                });
                continue;
            }
            match win.handle_control(shared, focused_id, cmd) {
                Some(ControlClose::ExitApp) => {
                    shared.shutdown_all();
                    event_loop.exit();
                }
                Some(ControlClose::CloseWindow) => {
                    queue_close_this(&mut deferred_windows);
                }
                None => {}
            }
        }
        // Native menu items (macOS) → semantic actions.
        if let Some(action) = ember_platform::menu_action(&shared.menu) {
            match action {
                MenuAction::ShowShortcuts => win.toggle_help(),
                MenuAction::About => win.toggle_about(),
                MenuAction::Settings => win.toggle_settings(shared),
                MenuAction::NewTab => win.new_tab(shared),
                MenuAction::NewWindow => {
                    let cwd = win
                        .focused_session_id()
                        .and_then(|sid| shared.cwd_by_session.get(&sid).cloned());
                    deferred_windows.push(DeferredWindowAction::OpenNew(cwd));
                }
                MenuAction::Copy => win.copy_selection(shared),
                MenuAction::Paste => win.paste_clipboard(shared),
                MenuAction::Close | MenuAction::Quit => {
                    let is_quit = matches!(action, MenuAction::Quit);
                    let kind = if is_quit {
                        PendingClose::Quit
                    } else {
                        PendingClose::Pane
                    };
                    if win.request_close(shared, kind) {
                        if is_quit {
                            shared.shutdown_all();
                            event_loop.exit();
                        } else {
                            queue_close_this(&mut deferred_windows);
                        }
                    }
                }
                // As with the ctl verbs above, only `focused_id` + the
                // symbolic op/target are captured — the `build_*` call (and
                // any error it can raise, e.g. "only one window open") is
                // deferred to the tail, once this tick's earlier actions have
                // already been applied, so it resolves against the CURRENT
                // `window_order` rather than a possibly-stale one.
                MenuAction::MoveTabToNewWindow => {
                    deferred_windows.push(DeferredWindowAction::Move(
                        focused_id,
                        DeferredMoveOp::MoveTab(MoveTabTarget::New),
                        None,
                    ));
                }
                MenuAction::MoveTabToNextWindow => {
                    deferred_windows.push(DeferredWindowAction::Move(
                        focused_id,
                        DeferredMoveOp::MoveTab(MoveTabTarget::Next),
                        None,
                    ));
                }
                MenuAction::MoveTabToPrevWindow => {
                    deferred_windows.push(DeferredWindowAction::Move(
                        focused_id,
                        DeferredMoveOp::MoveTab(MoveTabTarget::Prev),
                        None,
                    ));
                }
                MenuAction::PromotePaneToTab => {
                    deferred_windows.push(DeferredWindowAction::Move(
                        focused_id,
                        DeferredMoveOp::PromotePane(PromotePaneTarget::Tab),
                        None,
                    ));
                }
                MenuAction::PromotePaneToWindow => {
                    deferred_windows.push(DeferredWindowAction::Move(
                        focused_id,
                        DeferredMoveOp::PromotePane(PromotePaneTarget::Window),
                        None,
                    ));
                }
                MenuAction::MergeTabIntoPrevious => {
                    deferred_windows.push(DeferredWindowAction::Move(
                        focused_id,
                        DeferredMoveOp::MergeTab,
                        None,
                    ));
                }
            }
        }
        // A session can exit organically (shell `exit`, Ctrl-D, a crash) with
        // no explicit ctl/menu close request at all — that's handled above by
        // the `for session in exited` drain, which can leave this window with
        // no tabs. Queue the close here rather than closing immediately and
        // returning: an explicit close already queued above (e.g. a
        // close-confirm resolving to `CloseWindow`) can *also* leave tabs
        // empty — it's what emptied them — so `queue_close_this` guards
        // against enqueuing the same window's close twice in one tick. A
        // duplicate `finish_close` on an already-removed window would
        // re-check `windows.len() <= 1` against post-first-close state and
        // could tear down every remaining window instead of just this one.
        if win.tree.tabs.is_empty() {
            queue_close_this(&mut deferred_windows);
        }
        // Pace animations by WALL-CLOCK elapsed since the last frame, checked here on
        // *every* wake (timer tick OR any event). Advancing off the timer's
        // `ResumeTimeReached` alone is fragile: a stream of mouse-move/resize events
        // keeps resetting the `WaitUntil` deadline so the tick never fires and the
        // sparks freeze until the mouse stops (the stutter). We only request a redraw
        // once a frame-interval has actually elapsed, so this doesn't spin either.
        //
        // Driven for EVERY window this tick, not just the focused one:
        // each window evaluates its OWN `backdrop_animating` gate (the
        // sparks dial: `always` burns while visible-unfocused, the default
        // `focused` holds still there, plus Low Power Mode/Reduce Motion) —
        // so pacing only `self.focused_window` would break the `always`
        // mode and freeze any window the gate says should animate.
        // Focus-notify and menu/ctl-command handling above stay
        // focused-window-only (they're inherently about the focused
        // window); only this pacing loop fans out (2026-07-07 fix,
        // contract updated for the dial in 0.3.1).
        let now = Instant::now();
        let mut next_wake: Option<Instant> = None;
        for w in self.windows.values_mut() {
            // A starved (no-drawable) render retries on the animation cadence while
            // the window isn't winit-occluded: the renderer's StarveGate turns most
            // ticks into instant no-ops and allows a real attempt only every 250ms,
            // so a transiently starved frame self-heals without the  spin.
            let starve_retry = w.render_starved && !w.occluded;
            let frame = if w.about || w.fps_overlay || w.bell_flashing() {
                ANIM_FRAME
            } else if w.backdrop_animating(shared) {
                shared.ember_frame()
            } else {
                ANIM_FRAME // starve retry (only reached when `animating` below)
            };
            let animating = w.about
                || w.fps_overlay
                || w.bell_flashing()
                || w.backdrop_animating(shared)
                || w.ghost_active()
                || starve_retry;
            if animating {
                if now.duration_since(w.last_anim) >= frame {
                    w.last_anim = now;
                    w.advance_animations(shared, now);
                    // Animations advance on wall-clock regardless, but don't ask an
                    // occluded window to paint them (same  spin, slower burn).
                    if !w.occluded {
                        w.renderer.window().request_redraw();
                    }
                }
                // Fixed deadline relative to each window's own last frame
                // (not `now`), so incoming events can't push it back
                // indefinitely; the loop wakes at the SOONEST deadline
                // across every animating window.
                let deadline = w.last_anim + frame;
                next_wake = Some(next_wake.map_or(deadline, |d| d.min(deadline)));
            }
            // Hold-to-wisp (v1.1): advance any live press-and-hold ring on
            // this window, independent of the animation gate above — a hold
            // can be live on a window that's otherwise doing nothing else
            // (no backdrop, no bell, no FPS overlay).
            match w.tick_hold(shared, now) {
                HoldTick::Idle => {}
                HoldTick::Waiting(deadline) => {
                    next_wake = Some(next_wake.map_or(deadline, |d| d.min(deadline)));
                }
                HoldTick::Sweeping => {
                    let deadline = now + ANIM_FRAME;
                    next_wake = Some(next_wake.map_or(deadline, |d| d.min(deadline)));
                }
                HoldTick::Completed => {
                    // The hold just tore the pane off, already `carried =
                    // true` (design: "wisp visible immediately") — nudge the
                    // wisp the same way a real motion tick would via
                    // `update_cross_window_drag`, since a completed hold has
                    // no pointer motion of its own to piggyback on.
                    let sf = w.renderer.window().scale_factor();
                    if let Ok(pos) = w.renderer.window().inner_position() {
                        let (lx, ly) = w.cursor;
                        let screen_x = pos.x as f64 + lx * sf;
                        let screen_y = pos.y as f64 + ly * sf;
                        if let Some(d) = shared.drag.as_mut() {
                            d.last_screen = (screen_x, screen_y);
                        }
                        wisp_tick(shared, event_loop, true, screen_x, screen_y);
                    }
                    let deadline = now + ANIM_FRAME;
                    next_wake = Some(next_wake.map_or(deadline, |d| d.min(deadline)));
                }
            }
            // Suck-in/pour-out morph (v0.4.0): advance any live morph on
            // this window, independent of everything above — mirrors
            // `tick_hold` exactly (a self-terminating, at-most-~200ms
            // animation, `Idle` the overwhelmingly common case).
            if let MorphTick::Running(deadline) = w.tick_morph(now) {
                next_wake = Some(next_wake.map_or(deadline, |d| d.min(deadline)));
            }
            // Carry-time source vanish: re-check whether this
            // window's own carried-surface exclusion can take visual effect
            // yet — cheap (`carried_exclusion_live` gates on a field read)
            // for every window NOT currently dragging, and idempotent for
            // the one that is (`apply_carried_exclusion` no-ops once
            // already applied). This is what turns "suck-in just finished"
            // into "now filter/hide" for a drag whose motion has gone
            // still (no further `CursorMoved`/`ctl drag` tick to piggyback
            // the transition on, same reasoning as `tick_spring_load`).
            if w.carried_exclusion_live() {
                let carried = shared
                    .drag
                    .as_ref()
                    .is_some_and(|d| d.source_window == w.renderer.window().id() && d.carried);
                w.apply_carried_exclusion(shared, carried);
            }
            // Spring-loaded tab select (finding #2): fire a pending strip
            // dwell even once the pointer goes perfectly still (no more
            // motion ticks to piggyback on) — the same `tick_hold`/
            // `tick_morph` pacing idiom, a no-op `None` the overwhelmingly
            // common case.
            if let Some(deadline) = w.tick_spring_load(shared, now) {
                next_wake = Some(next_wake.map_or(deadline, |d| d.min(deadline)));
            }
        }
        // The wisp (Task 5) isn't a `WindowState` — it's not in `self.windows`
        // and so isn't touched by the loop above — but needs the exact same
        // wall-clock pacing while fading in/out or steady-carried: not driven
        // by `RedrawRequested` at all (it's never in `self.windows` for that
        // event to route through), so this is its ONLY render call site.
        // Cost discipline: `is_active()` is `false` (skipped entirely, no
        // `next_wake` contribution) whenever no drag has been carried yet,
        // and `tick` itself returns `false` — one tick after a fade-out
        // completes — which stops this from re-arming `next_wake` forever.
        if let WispSlot::Ready(w) = &mut shared.wisp {
            if w.is_active() && w.tick(now) {
                let deadline = w.last_anim + WISP_FRAME;
                next_wake = Some(next_wake.map_or(deadline, |d| d.min(deadline)));
            }
        }
        // Dying windows (finding #4): a drop-emptied source window parked in
        // `Shared::dying_windows` plays its farewell suck-in here — like the
        // wisp above, it's no longer in `self.windows`, so `RedrawRequested`
        // can't reach it and this is its ONLY tick+render call site. The
        // `retain_mut` drops each state the tick its morph self-terminates,
        // which is what finally closes the OS window. Empty (zero cost) at
        // all times except the ~150ms after such a drop.
        shared.dying_windows.retain_mut(|w| {
            // Pace on the window's own animation clock (like the pacing loop
            // above does for live windows): `tick_morph` ends in `set_morph`,
            // whose `request_redraw` wakes this loop right back up — ticking
            // unconditionally on every wake was a hot spin (~30k iterations
            // for one 150ms morph, observed live) of exactly the class the
            // render-loop OOM postmortem warns about.
            if now.duration_since(w.last_anim) < ANIM_FRAME {
                let deadline = w.last_anim + ANIM_FRAME;
                next_wake = Some(next_wake.map_or(deadline, |d| d.min(deadline)));
                return true; // alive, just not due yet
            }
            w.last_anim = now;
            match w.tick_morph(now) {
                MorphTick::Running(deadline) => {
                    next_wake = Some(next_wake.map_or(deadline, |d| d.min(deadline)));
                    let _ = w.renderer.render();
                    true
                }
                MorphTick::Idle => false,
            }
        });
        // A paced `ctl drag --paced <ms>` (Task 6's test machinery):
        // advance at most ONE waypoint this tick, through the exact same
        // `on_cursor_moved`/`update_cross_window_drag` pair a real mouse
        // step (or an unpaced `ctl drag`) drives — never a blocking sleep,
        // which would freeze this whole loop (the wisp's own pacing above
        // included) for the gesture's whole duration. `take()` up front so
        // there's no simultaneous `&mut shared.paced_drag` / `&mut shared`
        // borrow below (both `on_cursor_moved` and `update_cross_window_drag`
        // need the latter).
        if let Some(mut paced) = shared.paced_drag.take() {
            if !self.windows.contains_key(&paced.window) {
                // The window closed mid-drag. `clear_drag_on_window_close`
                // (called from `finish_close`) already cleared `shared.drag`
                // if this was its source, but mop up defensively — cheap,
                // and correct even if this window was only the CTL DRAG's
                // window without (yet) becoming the cross-window drag's
                // recorded source.
                shared.drag = None;
                clear_all_drag_visuals(&mut self.windows);
                let _ = paced.reply.send(
                    serde_json::json!({"ok": false, "error": "window closed mid-drag"}).to_string(),
                );
                // Leave `shared.paced_drag` as `None` — already taken.
            } else if now.duration_since(paced.last_step) < paced.interval {
                // Not due yet this tick — fold its deadline into `next_wake`
                // and put it back for the next tick to find.
                let deadline = paced.last_step + paced.interval;
                next_wake = Some(next_wake.map_or(deadline, |d| d.min(deadline)));
                shared.paced_drag = Some(paced);
            } else {
                paced.last_step = now;
                match paced.waypoints.pop_front() {
                    Some((x, y)) => {
                        if let Some(win) = self.windows.get_mut(&paced.window) {
                            win.on_cursor_moved(shared, paced.window, x, y);
                        }
                        if shared.drag.is_some() {
                            update_cross_window_drag(
                                &mut self.windows,
                                shared,
                                &self.focus_history,
                                paced.window,
                                x,
                                y,
                                event_loop,
                            );
                        }
                        let deadline = paced.last_step + paced.interval;
                        next_wake = Some(next_wake.map_or(deadline, |d| d.min(deadline)));
                        shared.paced_drag = Some(paced);
                    }
                    None => {
                        // Waypoints exhausted: run the exact same
                        // release-or-cancel tail an unpaced drag runs, then
                        // finally send the reply the client's been blocked
                        // on since the original `ctl drag --paced` request.
                        let drag_active_mid = shared.drag.is_some()
                            || self
                                .windows
                                .get(&paced.window)
                                .is_some_and(|w| w.tab_drag_is_active_drag());
                        let resp = finish_ctl_drag_tail(
                            &mut self.windows,
                            shared,
                            &mut self.focused_window,
                            event_loop,
                            paced.window,
                            paced.cancel,
                            paced.saved_mods,
                            drag_active_mid,
                        );
                        let _ = paced.reply.send(resp);
                        // Leave `shared.paced_drag` as `None` — already taken.
                    }
                }
            }
        }
        if let Some(deadline) = next_wake {
            event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
        } else {
            // Nothing animating anywhere: sleep until an event, a frame-lane wake, or a
            // control command wakes us — no more ~125 Hz idle polling.
            event_loop.set_control_flow(ControlFlow::Wait);
        }

        // Every window's borrow above (via `values_mut()`) has ended (last
        // used for the animation pacing) — only now can a window actually be
        // inserted or removed. Process every queued action, in write order:
        // an explicit close queued from a control command mustn't run before
        // an earlier-queued `OpenNew` in the same tick, or a window that was
        // emptied-then-replaced within one batch could look, mid-processing,
        // like "the last window standing" and trigger a full app shutdown
        // instead of just swapping windows.
        for action in deferred_windows {
            match action {
                DeferredWindowAction::OpenNew(cwd) => {
                    open_new_window(
                        &mut self.windows,
                        shared,
                        &mut self.focused_window,
                        event_loop,
                        cwd,
                    );
                }
                DeferredWindowAction::CloseThis => {
                    finish_close(
                        &mut self.windows,
                        shared,
                        &mut self.focused_window,
                        event_loop,
                        focused_id,
                    );
                }
                DeferredWindowAction::CloseWindow(id) => {
                    finish_close(
                        &mut self.windows,
                        shared,
                        &mut self.focused_window,
                        event_loop,
                        id,
                    );
                }
                DeferredWindowAction::Move(source, op, reply) => {
                    // Resolve the op against `self.windows`/`shared.window_order`
                    // AS THEY STAND right now — not as they stood when this
                    // action was enqueued. An earlier action in this same
                    // batch (processed above, in write order) can have
                    // closed/reopened/reordered windows; building the
                    // `SurfaceRef`/`SurfaceDest` pair here, from `source`'s
                    // CURRENT window (if it still exists), is what makes this
                    // safe. `next`/`prev` targets are resolved the same way,
                    // relative to the current window count/order.
                    let built = match self.windows.get(&source) {
                        Some(win) => match op {
                            DeferredMoveOp::MoveTab(target) => {
                                build_move_tab(win, shared, source, target)
                            }
                            DeferredMoveOp::PromotePane(target) => {
                                build_promote_pane(win, shared, source, target)
                            }
                            DeferredMoveOp::MergeTab => build_merge_tab(win, shared, source),
                        },
                        None => {
                            Err("the source window closed before this move could run".to_string())
                        }
                    };
                    let result = built.and_then(|(src, dest)| {
                        apply_move(
                            &mut self.windows,
                            shared,
                            &mut self.focused_window,
                            event_loop,
                            src,
                            dest,
                        )
                    });
                    match (result, reply) {
                        (Ok(()), Some(reply)) => {
                            let _ = reply.send("{\"ok\":true}".to_string());
                        }
                        (Err(e), Some(reply)) => {
                            let _ = reply
                                .send(serde_json::json!({"ok": false, "error": e}).to_string());
                        }
                        (Ok(()), None) => {}
                        (Err(e), None) => eprintln!("[ember] move failed: {e}"),
                    }
                }
                DeferredWindowAction::State(reply) => {
                    let json = build_state_json(shared, &self.windows, self.focused_window);
                    let _ = reply.send(json);
                }
                DeferredWindowAction::Focus(query, reply) => {
                    let resp = focus_across_windows(
                        &mut self.windows,
                        shared,
                        &mut self.focused_window,
                        &query,
                    );
                    let _ = reply.send(resp);
                }
                DeferredWindowAction::Drag {
                    window,
                    x1,
                    y1,
                    x2,
                    y2,
                    steps,
                    mods,
                    cancel,
                    paced_ms,
                    reply,
                } => {
                    // Unpaced: replies synchronously, right here. Paced:
                    // stashes into `shared.paced_drag` and sends its own
                    // reply later, once `about_to_wait`'s paced tick runs
                    // the tail — nothing to send here in that case.
                    run_ctl_drag(
                        &mut self.windows,
                        shared,
                        &mut self.focused_window,
                        &self.focus_history,
                        event_loop,
                        window,
                        x1,
                        y1,
                        x2,
                        y2,
                        steps,
                        &mods,
                        cancel,
                        paced_ms,
                        reply,
                    );
                }
            }
        }
    }
}

/// A window-structural action (open a new window / close this one) that a
/// control command, menu item, or a window emptying its last tab requested
/// mid-`about_to_wait` — applied once at the very end, after every window's
/// borrow ends (see the comment at its use site). Collected into a `Vec`
/// (`deferred_windows`), not a single slot: several independent sites in one
/// `about_to_wait` tick can each want to enqueue one of these, and a single
/// `Option` let a later write silently drop an earlier one.
#[derive(Debug)]
enum DeferredWindowAction {
    OpenNew(Option<String>),
    CloseThis,
    /// Close a specific, possibly NON-focused window whose tree just emptied
    /// (a background window's last shell exited via the exited-shell drain).
    /// `CloseThis` always targets `focused_id` captured at the top of
    /// `about_to_wait`, so it can't express "close THAT other window" — this
    /// variant carries the id explicitly. Processed by `finish_close` exactly
    /// like `CloseThis`, just against an explicit id instead of `focused_id`.
    CloseWindow(WindowId),
    /// A surface-mobility op, deferred for the same reason as the other two
    /// variants: the sites that discover one (a ctl command, a native menu
    /// item) still hold `win`'s borrow of `self.windows` for the rest of the
    /// tick. Carries the SOURCE WINDOW'S IDENTITY (`WindowId`) and the
    /// symbolic op/target, NOT a pre-resolved `SurfaceRef`/`SurfaceDest` pair
    /// — those embed raw `window_order` indices, and an earlier same-batch
    /// `Move` (this `Vec` is processed in write order) can shift that order
    /// before this one runs, silently misrouting it onto the wrong window if
    /// the indices were baked at dispatch time. The actual `build_*` call —
    /// and thus the index resolution — happens at the tail, right before
    /// `apply_move`, against `window_order` as it stands then. `Some` reply
    /// channel is a `ctl` command awaiting its `{ok}`/`{ok:false,error}`
    /// line; `None` is a menu item (best-effort — an error just gets logged).
    Move(
        WindowId,
        DeferredMoveOp,
        Option<std::sync::mpsc::Sender<String>>,
    ),
    /// `ctl state` (Task 5): deferred for the same reason as `Move` — the
    /// discovery site (the ctl-commands loop) still holds `win`'s borrow of
    /// `self.windows` for the rest of the tick, but the multi-window builder
    /// needs to see every window. Built at the tail by `build_state_json`.
    State(std::sync::mpsc::Sender<String>),
    /// `ctl focus` (Task 5): same reasoning — searches (and can raise/select
    /// on) any window, not just the one `win` currently borrows. Resolved at
    /// the tail by `focus_across_windows`.
    Focus(String, std::sync::mpsc::Sender<String>),
    /// `ctl drag`: synthesize a full press→motion*→release (or →Escape, if
    /// `cancel`) gesture on `window`, through the exact same `WindowState`
    /// methods a real mouse hits. Resolved at the tail by `run_ctl_drag`.
    Drag {
        window: WindowId,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        steps: usize,
        mods: String,
        cancel: bool,
        paced_ms: Option<u64>,
        reply: std::sync::mpsc::Sender<String>,
    },
}

/// Which surface-mobility builder a deferred [`DeferredWindowAction::Move`]
/// should call once it's resolved at the tail of `about_to_wait`. Mirrors
/// `build_move_tab`/`build_promote_pane`/`build_merge_tab`'s targets exactly
/// — this just carries the CHOICE of builder + target across the deferral,
/// since the `SurfaceRef`/`SurfaceDest` those builders produce can't safely
/// be precomputed (see `DeferredWindowAction::Move`'s doc).
#[derive(Clone, Copy, Debug)]
enum DeferredMoveOp {
    MoveTab(MoveTabTarget),
    PromotePane(PromotePaneTarget),
    MergeTab,
}

/// Enqueue a `CloseThis` unless one is already queued this tick.
///
/// `CloseThis` always targets the same window (`focused_id`, captured once
/// at the top of `about_to_wait`), and applying it twice would call
/// `finish_close` on an already-removed window: `finish_close` re-checks
/// `windows.len() <= 1` at the time it runs, so a second, redundant
/// `CloseThis` right after the first one actually closed a window (out of
/// several) would see the now-smaller window count and could tear the
/// whole app down instead of a no-op. Several sites in `about_to_wait` can
/// each independently conclude "this window should close" in the same
/// tick (an explicit ctl/menu close **and** the organic tabs-emptied
/// check, since the explicit close is often exactly what emptied the
/// tabs) — this keeps that idempotent.
fn queue_close_this(deferred_windows: &mut Vec<DeferredWindowAction>) {
    if !deferred_windows
        .iter()
        .any(|a| matches!(a, DeferredWindowAction::CloseThis))
    {
        deferred_windows.push(DeferredWindowAction::CloseThis);
    }
}

/// Enqueue a `CloseWindow(id)` unless one targeting the same `id` is already
/// queued this tick. Same idempotency reasoning as `queue_close_this`: a
/// duplicate close request for the same window is harmless once
/// `finish_close`'s own guard sees `id` already removed from `windows`, but
/// this keeps `deferred_windows` from accumulating redundant entries for a
/// window that emptied via more than one exited session in the same tick
/// (e.g. a two-tab background window whose last two shells both exit
/// together).
fn queue_close_window(deferred_windows: &mut Vec<DeferredWindowAction>, id: WindowId) {
    if !deferred_windows
        .iter()
        .any(|a| matches!(a, DeferredWindowAction::CloseWindow(w) if *w == id))
    {
        deferred_windows.push(DeferredWindowAction::CloseWindow(id));
    }
}

/// Create a new OS window + GPU renderer + `WindowState` seeded with `tree`,
/// replaying every contained session's content into the new renderer (the
/// spike finding, binding): a fresh `Renderer` starts style-empty, so any
/// session already running elsewhere (a future moved/shared pane — Task 4)
/// would render black-on-black until its next real PTY delta if we didn't
/// seed it with a full-reset [`ember_render::GridModel::snapshot_delta`]
/// sourced from whichever existing window currently owns its grid. A session
/// with no existing grid anywhere (a brand-new one the caller is about to
/// spawn) just gets an empty pane registered.
///
/// Free function rather than an `App` method: every call site already holds a
/// `&mut Shared` borrowed out of `self.shared` (and often a `&mut WindowState`
/// out of `self.windows`) for the rest of its enclosing function, and a
/// `&mut self` method here would conflict with that — see `close_window` and
/// `finish_close` below for the same reasoning. Does not spawn any shell.
///
/// `position`: a physical-px screen position hint (`with_position`) — `Some`
/// only for a desktop drag drop (release 2 task 3: the new window lands at
/// roughly the drop point); every other caller passes `None` and gets the OS
/// default placement, unchanged.
fn open_window(
    windows: &mut HashMap<WindowId, WindowState>,
    shared: &mut Shared,
    event_loop: &ActiveEventLoop,
    tree: ember_core::WindowTree,
    position: Option<winit::dpi::PhysicalPosition<i32>>,
) -> WindowId {
    let w = DEFAULT_COLS as f32 * CELL_WIDTH + 2.0 * PAD;
    let h = DEFAULT_ROWS as f32 * CELL_HEIGHT + 2.0 * PAD;
    let mut attrs = ember_platform::window_attributes("Ember", w, h);
    if let Some(p) = position {
        attrs = attrs.with_position(p);
    }
    let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
    ember_platform::set_app_icon(&window, ICON_PNG);
    let window_id = window.id();
    let size = window.inner_size();
    let px = (size.width.max(1), size.height.max(1));
    let mut renderer = Renderer::new(Arc::clone(&window), &shared.config.font);

    for (_, sid) in tree.tabs.iter().flat_map(|t| t.root.leaves()) {
        // Source a replay delta from whatever window currently has this
        // session's grid (none, for a session about to be freshly spawned).
        let source = windows
            .values()
            .find_map(|w| w.renderer.grid(&sid).map(|g| (g.dims, g.snapshot_delta())));
        let dims = source
            .as_ref()
            .map(|(d, _)| *d)
            .unwrap_or(GridDims::new(DEFAULT_COLS, DEFAULT_ROWS));
        renderer.ensure_pane(&sid, dims);
        if let Some((_, delta)) = source {
            renderer.apply_delta(&sid, delta);
        }
        shared.session_window.insert(sid, window_id);
    }

    let mut win = WindowState::new(renderer, tree);
    win.px = px;
    win.sync_layout(shared);
    win.apply_appearance(shared);
    win.renderer.window().request_redraw();
    windows.insert(window_id, win);
    shared.window_order.push(window_id);
    window_id
}

/// Open a new window with one fresh tab: a new shell spawned with `cwd` (the
/// focused pane's OSC 1337 dir — the same cwd-inheritance rule the split-spawn
/// path uses). The shared path behind Cmd+N, the File → New Window menu item,
/// and `ctl new-window`. On spawn failure the empty window is closed
/// immediately rather than left on screen with a dead pane.
fn open_new_window(
    windows: &mut HashMap<WindowId, WindowState>,
    shared: &mut Shared,
    focused_window: &mut Option<WindowId>,
    event_loop: &ActiveEventLoop,
    cwd: Option<String>,
) -> WindowId {
    let pane = PaneId(shared.next_pane);
    shared.next_pane += 1;
    let session = SessionId::new(format!("s{}", shared.next_session));
    shared.next_session += 1;
    let tab_id = TabId(shared.next_tab);
    shared.next_tab += 1;
    let tree = ember_core::WindowTree {
        tabs: vec![Tab {
            id: tab_id,
            title: String::new(),
            root: LayoutNode::pane(pane, session.clone()),
            focus: pane,
        }],
        active: 0,
    };
    let window_id = open_window(windows, shared, event_loop, tree, None);
    let win = windows.get_mut(&window_id).expect("just inserted above");
    if !win.spawn_session(
        shared,
        session,
        GridDims::new(DEFAULT_COLS, DEFAULT_ROWS),
        cwd,
    ) {
        close_window(windows, shared, focused_window, window_id);
        return window_id;
    }
    win.sync_layout(shared);
    *focused_window = Some(window_id);
    win.renderer.window().request_redraw();
    window_id
}

/// Clear `shared.drag` if window `id` was its source, so a source window
/// closing mid-drag doesn't leave the global key-swallow (the
/// `shared.drag.is_some()` check in the `KeyboardInput` handler above) stuck
/// forever — nothing else ever clears `shared.drag` once the source window
/// itself is gone, since `left_release`/`cancel_drag_everywhere` (the only
/// other things that clear it) need a live `WindowState` to call into.
/// Otherwise (release 2 task 3's addition): if `id` is instead the drag's
/// current HOVER TARGET (a different window still owns the drag), just drop
/// the now-stale hover — the target's `WindowState` is discarded wholesale
/// by the caller right after this returns, so there's no visual left to
/// clear, but a later release/motion must never dereference a dead window.
fn clear_drag_on_window_close(
    windows: &mut HashMap<WindowId, WindowState>,
    shared: &mut Shared,
    id: WindowId,
) {
    let Some(drag) = shared.drag.as_ref() else {
        return;
    };
    if drag.source_window == id {
        shared.drag = None;
        shared.wisp_end_drag();
        clear_all_drag_visuals(windows);
        return;
    }
    let hover_window = match drag.hover {
        Some(DropHover::Strip { window, .. }) => Some(window),
        Some(DropHover::Pane { window, .. }) => Some(window),
        Some(DropHover::Desktop) | None => None,
    };
    if hover_window == Some(id) {
        if let Some(d) = shared.drag.as_mut() {
            d.hover = None;
        }
    }
}

/// Clear every window's drag-related visuals (lifted tab chip, split
/// preview, and — release 2 task 3's addition — any live cross-window
/// incoming-drop preview). Called whenever a drag just ended (drop, cancel,
/// or its source window closing) so nothing lingers on a window that isn't
/// the source, regardless of which one(s) were actually showing something —
/// each individual clear already no-ops cheaply when nothing was set.
fn clear_all_drag_visuals(windows: &mut HashMap<WindowId, WindowState>) {
    for w in windows.values_mut() {
        w.clear_drag_visuals();
    }
}

/// Cancel a live drag with zero tree mutation (Escape, or `ctl drag
/// --cancel`) — a free function rather than a `WindowState` method because,
/// as of release 2 task 3, the drag's current hover can be on a DIFFERENT
/// window than whichever one actually received the cancel input (OS
/// keyboard focus doesn't reliably track a captured mouse-drag's source
/// window), so clearing must reach every window, not just `self`.
fn cancel_drag_everywhere(windows: &mut HashMap<WindowId, WindowState>, shared: &mut Shared) {
    if let Some(drag) = shared.drag.take() {
        shared.wisp_end_drag();
        // Pour-out (v0.4.0): "the reverse pours OUT... Escape/cancel: the
        // pour-out plays at the SOURCE rect (the surface went home)." The
        // source window may have closed mid-drag (`clear_drag_on_window_close`
        // already handles that path); a missing source here is a no-op.
        if let Some(w) = windows.get_mut(&drag.source_window) {
            // Carry-time source vanish: "pops back exactly where
            // it was" — re-show the OS window / restore the filtered
            // layout BEFORE the pour-out below repaints over it, so the
            // very first frame of the pour-out is already showing the
            // real, restored content, not a stale exclusion.
            w.clear_carried_exclusion(shared);
            let rect = w.viewport();
            let grab = w.cursor;
            w.start_pour_out(shared, rect, grab);
        }
        clear_all_drag_visuals(windows);
        for w in windows.values_mut() {
            w.renderer.window().request_redraw();
        }
    }
}

/// Best-effort pour-out target for a completed drag `Move` (v0.4.0): the
/// window that ended up focused — a drop's landed tab/pane becomes active
/// (see `apply_move`'s `final_focused` handling) — using its content rect
/// and a point near the top of it as the emergence origin. Not the exact
/// drop pixel: a cross-window carry's hover carries no stored local point to
/// replay (see `DropHover`'s doc), so this is the same kind of v1
/// approximation as the ghost tab's always-last strip slot — honest, not
/// half-implemented.
fn pour_out_after_move(
    windows: &mut HashMap<WindowId, WindowState>,
    shared: &Shared,
    focused_window: Option<WindowId>,
) {
    let Some(wid) = focused_window else { return };
    let Some(w) = windows.get_mut(&wid) else {
        return;
    };
    let rect = w.viewport();
    let grab = (rect.x + rect.width * 0.5, rect.y + rect.height * 0.12);
    w.start_pour_out(shared, rect, grab);
}

/// Front-to-back window frames (physical px, screen space) for cross-window
/// drag hit-testing: `focus_history` first (most-recently-focused = most
/// "in front", our best available proxy for real window-manager stacking
/// order — winit exposes no actual z-order query), then any window NOT in
/// `focus_history` yet (just opened, never focused) appended via
/// `window_order` so every live window is represented exactly once. Skips a
/// window whose `inner_position()` fails (platforms where it can return an
/// error; treated as "not hit-testable this frame" rather than a panic).
fn window_frames(
    windows: &HashMap<WindowId, WindowState>,
    focus_history: &[WindowId],
    window_order: &[WindowId],
) -> Vec<(WindowId, (f64, f64, f64, f64))> {
    let mut order: Vec<WindowId> = focus_history
        .iter()
        .copied()
        .filter(|id| windows.contains_key(id))
        .collect();
    for wid in window_order {
        if !order.contains(wid) {
            order.push(*wid);
        }
    }
    order
        .into_iter()
        .filter_map(|wid| {
            let w = windows.get(&wid)?;
            let win = w.renderer.window();
            let pos = win.inner_position().ok()?;
            let size = win.inner_size();
            Some((
                wid,
                (
                    pos.x as f64,
                    pos.y as f64,
                    size.width as f64,
                    size.height as f64,
                ),
            ))
        })
        .collect()
}

/// Clear `incoming_drop` (and its visuals) on every window except `keep`
/// (`None` clears everyone). Called on each cross-window drag motion tick so
/// only the CURRENT target ever shows a preview — the previous target (if
/// any) must stop showing one the instant hover moves elsewhere.
fn clear_incoming_drop_except(
    windows: &mut HashMap<WindowId, WindowState>,
    keep: Option<WindowId>,
) {
    for (wid, w) in windows.iter_mut() {
        if Some(*wid) != keep && w.incoming_drop.is_some() {
            w.set_incoming_drop(None, None);
        }
    }
}

/// Advance a live cross-window drag on a motion tick: `source_id` is the
/// window that just received `CursorMoved`/a synthesized `ctl drag` step (the
/// ONLY window that does, per the mini-spike — macOS keeps delivering motion
/// to a drag's origin window even once the pointer leaves its bounds, so
/// there is no separate "target window's own CursorMoved" to listen for).
/// `local_x`/`local_y` are logical px in `source_id`'s own coordinate space
/// (same units `WindowState::on_cursor_moved` takes) — which is exactly
/// `on_cursor_moved`'s `(x, y)`; call this immediately after it, every tick,
/// with the same values. No-ops instantly unless `shared.drag` exists and
/// belongs to `source_id`.
///
/// When the point is still within `source_id`'s own bounds, this only tidies
/// up (clears any OTHER window's stale preview) — `on_cursor_moved` already
/// ran `WindowState::update_drag_hover` for the in-window case by the time
/// this is called, so `drag.hover` is already correct. Once the point is
/// outside those bounds (`carried` flips `true`, monotonically), this
/// resolves the real screen-space target via `window_frames`/`window_under`
/// and drives the previews: a different tracked window gets its
/// `incoming_drop` set (Strip/Edge/Center preview), no window hit at all
/// means `DropHover::Desktop`.
///
/// Also the wisp's (Task 5) one motion-tick choke point: by the time this
/// returns, `shared.drag.hover` is final for the tick, so the common tail
/// below drives `wisp_tick` off it once, covering both the in-bounds and
/// carried branches (see `wisp_tick`'s doc for why it's not just "while
/// `outside_source`" — `carried` is monotonic, `outside_source` isn't).
/// `event_loop` is needed only for the wisp's lazy, one-time window creation.
fn update_cross_window_drag(
    windows: &mut HashMap<WindowId, WindowState>,
    shared: &mut Shared,
    focus_history: &[WindowId],
    source_id: WindowId,
    local_x: f64,
    local_y: f64,
    event_loop: &ActiveEventLoop,
) {
    let Some(drag) = shared.drag.as_ref() else {
        return;
    };
    if drag.source_window != source_id {
        return;
    }
    let was_carried = drag.carried;
    // Captured now (`drag`'s last use before several `shared.drag.as_mut()`
    // borrows below, which would otherwise conflict with holding `drag`
    // alive across them) — cheap, and the title never changes mid-drag.
    let title = drag.title.clone();
    let Some(src) = windows.get(&source_id) else {
        return;
    };
    let sf = src.renderer.window().scale_factor();
    let Ok(pos) = src.renderer.window().inner_position() else {
        return;
    };
    let screen_x = pos.x as f64 + local_x * sf;
    let screen_y = pos.y as f64 + local_y * sf;
    let (sw, sh) = src.px;
    let outside_source =
        local_x < 0.0 || local_y < 0.0 || local_x * sf >= sw as f64 || local_y * sf >= sh as f64;

    if let Some(d) = shared.drag.as_mut() {
        d.last_screen = (screen_x, screen_y);
        if outside_source {
            d.carried = true;
        }
    }
    // `carried` is monotonic (never flips back to `false`); `outside_source`
    // can — the pointer can drift back over the source window mid-carry. The
    // wisp stays live through that (it's a carry-scoped token, not an
    // in-bounds-only one), so this reads the POST-update flag, not
    // `outside_source` itself.
    let just_carried = outside_source && !was_carried;
    let carried_now = was_carried || outside_source;

    if !outside_source {
        // Still (or again) inside the source's own bounds: `on_cursor_moved`
        // (called just before this, same tick) already set `drag.hover`
        // correctly via `update_drag_hover`. Just make sure no OTHER window
        // is left showing a stale target preview from a prior tick.
        clear_incoming_drop_except(windows, Some(source_id));
    } else {
        let frames = window_frames(windows, focus_history, &shared.window_order);
        let hit = ember_core::window_under(
            screen_x,
            screen_y,
            &frames.iter().map(|(_, f)| *f).collect::<Vec<_>>(),
        );
        let target = hit.and_then(|i| frames.get(i)).map(|(id, _)| *id);

        match target {
            Some(tid) if tid == source_id => {
                // The point is geometrically back over the source's own frame
                // (carried, but hovering itself again) — nothing else claims it.
                // Drop any stale cross-window hover too: its preview was just
                // cleared, and a release here must not apply an invisible target.
                if let Some(d) = shared.drag.as_mut() {
                    d.hover = None;
                }
                clear_incoming_drop_except(windows, Some(source_id));
            }
            Some(tid) => {
                // Raise the hover target the moment the carry first enters
                // it, so the drop zones are actually visible (a buried
                // target window made previews pointless — Brandon's first
                // live session). Exactly once per target change, tracked in
                // `last_raised` — NOT derived from `hover`, which is `None`
                // over body regions and would re-raise every motion tick
                // (the event-queue balloon/freeze of the same session).
                // Keyboard focus moving with the raise is fine — keys are
                // swallowed globally during a drag and Escape cancels from
                // any window; macOS mouse capture survives (the source view
                // keeps receiving motion regardless of key window).
                let already = shared.drag.as_ref().and_then(|d| d.last_raised);
                if already != Some(tid) {
                    if let Some(twin) = windows.get(&tid) {
                        twin.renderer.window().focus_window();
                    }
                    if let Some(d) = shared.drag.as_mut() {
                        d.last_raised = Some(tid);
                    }
                }
                clear_incoming_drop_except(windows, Some(tid));
                let target_pos = windows.get(&tid).and_then(|twin| {
                    let tsf = twin.renderer.window().scale_factor();
                    let tpos = twin.renderer.window().inner_position().ok()?;
                    Some((
                        (screen_x - tpos.x as f64) / tsf,
                        (screen_y - tpos.y as f64) / tsf,
                    ))
                });
                if let Some((tx, ty)) = target_pos {
                    let hover = windows
                        .get(&tid)
                        .and_then(|twin| twin.hover_at(tid, tx, ty, false));
                    if let Some(twin) = windows.get_mut(&tid) {
                        twin.set_incoming_drop(hover, Some(title.as_str()));
                    }
                    if let Some(d) = shared.drag.as_mut() {
                        d.hover = hover;
                    }
                }
            }
            None => {
                clear_incoming_drop_except(windows, None);
                if let Some(d) = shared.drag.as_mut() {
                    d.hover = Some(DropHover::Desktop);
                }
            }
        }
    }

    if carried_now {
        wisp_tick(shared, event_loop, just_carried, screen_x, screen_y);
    }
}

/// Drive the wisp (Task 5) for one carried-drag tick. Lazily creates it on
/// the FIRST carried transition of the process (`just_carried`), degrading
/// to `WispSlot::Unsupported` — permanently, no retry — if window/GPU
/// creation fails (`WispUnsupported`): every other drag mechanic is
/// unaffected either way, which is the whole point of the ladder. A no-op
/// when `config.wisp` is off, so toggling it off is a genuine zero-cost
/// skip, not just "don't show it."
fn wisp_tick(
    shared: &mut Shared,
    event_loop: &ActiveEventLoop,
    just_carried: bool,
    screen_x: f64,
    screen_y: f64,
) {
    if !shared.config.wisp {
        return;
    }
    if just_carried {
        if matches!(shared.wisp, WispSlot::Uninit) {
            shared.wisp = match WispWindow::new(event_loop) {
                Ok(w) => WispSlot::Ready(Box::new(w)),
                Err(WispUnsupported) => {
                    eprintln!(
                        "[ember] wisp: unsupported on this GPU/surface; disabling for this session"
                    );
                    WispSlot::Unsupported
                }
            };
        }
        if let WispSlot::Ready(w) = &mut shared.wisp {
            w.show();
        }
    }
    let WispSlot::Ready(w) = &mut shared.wisp else {
        return;
    };
    let intensity = if shared.drag.as_ref().is_some_and(|d| d.hover.is_some()) {
        1.0
    } else {
        0.6
    };
    w.set_target_intensity(intensity);
    w.move_to(screen_x, screen_y);
}

/// Tear down window `id` immediately: shut down every session it owns (send
/// `Shutdown`, forget it in `shared.sessions`/`session_window`, and drop its
/// per-session bookkeeping), then drop the `WindowState` itself — the last
/// reference to its winit `Window`/GPU surface, which closes the OS window.
/// Re-targets `focused_window` at any remaining window if it pointed here.
fn close_window(
    windows: &mut HashMap<WindowId, WindowState>,
    shared: &mut Shared,
    focused_window: &mut Option<WindowId>,
    id: WindowId,
) {
    clear_drag_on_window_close(windows, shared, id);
    if let Some(win) = windows.remove(&id) {
        for sid in win.window_session_ids() {
            if let Some(h) = shared.sessions.remove(&sid) {
                let _ = h.control.send(BackendControl::Shutdown);
            }
            shared.session_window.remove(&sid);
            shared.bracketed.remove(&sid);
            shared.titles.remove(&sid);
            shared.cwd_by_session.remove(&sid);
        }
    }
    shared.window_order.retain(|w| *w != id);
    if *focused_window == Some(id) {
        *focused_window = windows.keys().next().copied();
    }
}

/// Tear down window `id`'s OS window/`WindowState` WITHOUT touching a single
/// session: used by [`apply_move`] to close a window that lost its last tab
/// to a move — its sessions are still very much alive, just re-homed into
/// another window's renderer/`session_window` entry (already done by the time
/// this runs; effect order guarantees every `SessionsRehomed` for this window
/// is applied before its `WindowClosed`). `close_window` would wrongly send
/// `Shutdown` to every PTY this window's tree used to hold.
fn close_window_shell_only(
    windows: &mut HashMap<WindowId, WindowState>,
    shared: &mut Shared,
    focused_window: &mut Option<WindowId>,
    id: WindowId,
) {
    clear_drag_on_window_close(windows, shared, id);
    let removed = windows.remove(&id);
    shared.window_order.retain(|w| *w != id);
    if *focused_window == Some(id) {
        *focused_window = windows.keys().next().copied();
    }
    // Farewell suck-in (finding #4): the whole window's content collapses
    // toward the pointer (which sits at the drop that emptied this window)
    // before the OS window actually closes. Every bit of bookkeeping above
    // already ran — the window is gone from `windows`/`window_order`/focus,
    // so nothing (input routing, index math, a follow-up move) can see a
    // half-alive window; only the `WindowState` itself (hence the OS window)
    // is kept breathing in `Shared::dying_windows`, ticked+rendered by
    // `about_to_wait` until the ~150ms morph self-terminates and the drop
    // of the state closes the window for real. When the morph gate no-ops
    // (wisp off / reduced motion), the state drops right here — the
    // pre-finding instant close, unchanged.
    if let Some(mut w) = removed {
        let rect = w.full_window_rect();
        let grab = w.cursor;
        w.start_suck_in(shared, rect, grab);
        if w.morph_live() {
            w.renderer.window().request_redraw();
            shared.dying_windows.push(w);
        }
    }
}

/// The shared tail of every "this close/quit was confirmed (or needed no
/// confirmation)" path: if `id` is the only window left, closing it is
/// indistinguishable from quitting the app, so do the full app-wide shutdown;
/// otherwise close just that one window and keep running.
fn finish_close(
    windows: &mut HashMap<WindowId, WindowState>,
    shared: &mut Shared,
    focused_window: &mut Option<WindowId>,
    event_loop: &ActiveEventLoop,
    id: WindowId,
) {
    // A same-tick deferred action (e.g. a `Move` that closed and reopened
    // windows earlier in this batch) may have already removed `id` from
    // `windows` by the time this stale `CloseThis` runs. Without this guard,
    // the `windows.len() <= 1` check below would evaluate against whatever
    // window count is left AFTER that unrelated close/reopen — which can
    // easily be exactly 1 — and trigger a full `shutdown_all()`/`exit()`,
    // killing every remaining (possibly just-relocated) session over a close
    // request that no longer refers to a real window. A stale close must be
    // a no-op, never a full shutdown.
    if !windows.contains_key(&id) {
        return;
    }
    if windows.len() <= 1 {
        shared.shutdown_all();
        event_loop.exit();
    } else {
        close_window(windows, shared, focused_window, id);
    }
}

/// The window index a `SurfaceRef` names (the field is the same for both
/// variants, just not reachable through one shared pattern).
fn surface_window_index(src: SurfaceRef) -> usize {
    match src {
        SurfaceRef::Pane { window, .. } | SurfaceRef::Tab { window, .. } => window,
    }
}

/// The one function every surface-mobility gesture (menu item, keybinding,
/// `ctl move-tab`/`promote-pane`/`merge-tab`) lowers onto: build an
/// `ember_core::Windows` view of the live window set (ordered by
/// `shared.window_order`), run [`move_surface`], and carry out whatever
/// [`MoveEffect`]s it returns.
///
/// A moved pane/tab's session(s) must NEVER be killed by this — `move_surface`
/// only ever *relocates* a `WindowTree`'s leaves, it never emits
/// `LayoutEffect::KillSession`. The one danger zone is `MoveEffect::WindowClosed`
/// (the source window ran out of tabs): that must tear down the OS
/// window/`WindowState` only (`close_window_shell_only`), never the sessions
/// `close_window` would kill — by the time it fires, effect order guarantees
/// every session that window used to own has already been re-homed by an
/// earlier `SessionsRehomed` in the same batch.
///
/// Free function, not an `App`/`WindowState` method, for the same reason as
/// `open_window`/`close_window`: every call site already holds `&mut Shared`
/// (and often a `&mut WindowState`) borrowed out of `self` for the rest of
/// its enclosing function — see those functions' docs.
fn apply_move(
    windows: &mut HashMap<WindowId, WindowState>,
    shared: &mut Shared,
    focused_window: &mut Option<WindowId>,
    event_loop: &ActiveEventLoop,
    src: SurfaceRef,
    dest: SurfaceDest,
) -> Result<(), String> {
    let orig_order = shared.window_order.clone();
    let trees: Vec<ember_core::WindowTree> = orig_order
        .iter()
        .map(|wid| windows[wid].tree.clone())
        .collect();
    let focused_idx = focused_window
        .and_then(|fw| orig_order.iter().position(|w| *w == fw))
        .unwrap_or(0);
    let mut model = ember_core::Windows {
        trees,
        focused: focused_idx,
    };
    let fresh_tab_id = TabId(shared.next_tab);
    let mut effects = ember_core::move_surface(&mut model, src, dest, fresh_tab_id)
        .map_err(humanize_move_error)?;
    shared.next_tab += 1;
    let final_focused = model.focused;

    // Stable-sort so any `WindowClosed` always processes LAST, regardless of
    // where `move_surface` put it in the returned `Vec`. The rest of this
    // function (the `SessionsRehomed`-before-`WindowClosed` re-homing dance)
    // relies on that ordering, and today it happens to already hold — every
    // `close_source_if_empty` call in ember-core's `windows.rs` appends after
    // the effects vec is otherwise complete — but that's an internal emission
    // detail of Task 1's code, not a documented contract `move_surface`
    // promises to keep. Sorting here removes the reliance instead of trusting
    // it silently holds forever.
    effects.sort_by_key(|e| matches!(e, MoveEffect::WindowClosed { .. }));

    // At most one window can close per move (only the source can lose its
    // last tab), so every ORIGINAL index above it simply shifts down by one
    // in the final layout — an exact, order-independent mapping computed
    // BEFORE any of the effects below actually mutate `windows`/`window_order`.
    let closed_idx = effects.iter().find_map(|e| match e {
        MoveEffect::WindowClosed { index } => Some(*index),
        _ => None,
    });
    let mut new_order: Vec<Option<WindowId>> = vec![None; model.trees.len()];
    for (i, wid) in orig_order.iter().enumerate() {
        if Some(i) == closed_idx {
            continue; // this window is going away this round
        }
        let final_idx = match closed_idx {
            Some(c) if i > c => i - 1,
            _ => i,
        };
        new_order[final_idx] = Some(*wid);
    }

    // Write every surviving window's mutated tree back; the one slot still
    // `None` (if any) is the brand-new window `move_surface` minted — stash
    // its tree for `MoveEffect::WindowOpened` below (`open_window` seeds it).
    let mut opened_tree: Option<ember_core::WindowTree> = None;
    for (i, tree) in model.trees.into_iter().enumerate() {
        match new_order[i] {
            Some(wid) => {
                if let Some(w) = windows.get_mut(&wid) {
                    w.tree = tree;
                }
            }
            None => opened_tree = Some(tree),
        }
    }

    let src_window_id = orig_order.get(surface_window_index(src)).copied();
    let mut touched: Vec<WindowId> = src_window_id.into_iter().collect();

    for effect in effects {
        match effect {
            MoveEffect::WindowOpened { index } => {
                let Some(tree) = opened_tree.take() else {
                    continue; // shouldn't happen: move_surface mints at most one window
                };
                // A desktop drag drop staged a position hint just before this
                // `apply_move` call (`WindowState::resolve_drag_drop`'s
                // `Desktop` arm) — consume it here; every other move that
                // opens a window (promote-pane, move-tab-to-new-window)
                // leaves it `None`, so the OS default placement applies.
                let position = shared.new_window_position_hint.take();
                let wid = open_window(windows, shared, event_loop, tree, position);
                new_order[index] = Some(wid);
                touched.push(wid);
            }
            MoveEffect::WindowClosed { index } => {
                if let Some(wid) = orig_order.get(index).copied() {
                    close_window_shell_only(windows, shared, focused_window, wid);
                }
            }
            MoveEffect::SessionsRehomed {
                sessions,
                to_window,
            } => {
                let Some(dest_wid) = new_order.get(to_window).copied().flatten() else {
                    continue;
                };
                for sid in sessions {
                    shared.session_window.insert(sid.clone(), dest_wid);
                    // A fresh (or merely session-blind) renderer starts
                    // style-empty (the spike finding): source a full-reset
                    // replay delta from wherever the grid currently lives —
                    // the source window's renderer is still intact here,
                    // `WindowClosed` (if any) always comes after every
                    // `SessionsRehomed` in the same batch.
                    let source = windows
                        .values()
                        .find_map(|w| w.renderer.grid(&sid).map(|g| (g.dims, g.snapshot_delta())));
                    if let Some(dest_win) = windows.get_mut(&dest_wid) {
                        let dims = source
                            .as_ref()
                            .map(|(d, _)| *d)
                            .unwrap_or(GridDims::new(DEFAULT_COLS, DEFAULT_ROWS));
                        dest_win.renderer.ensure_pane(&sid, dims);
                        if let Some((_, delta)) = source {
                            dest_win.renderer.apply_delta(&sid, delta);
                        }
                    }
                    // It lives in the destination now — drop it from the
                    // source window's renderer/dims cache.
                    if let Some(src_win) = src_window_id.and_then(|id| windows.get_mut(&id)) {
                        src_win.renderer.remove_pane(&sid);
                        src_win.dims_cache.remove(&sid);
                    }
                    if !touched.contains(&dest_wid) {
                        touched.push(dest_wid);
                    }
                }
            }
        }
    }

    if let Some(wid) = new_order.get(final_focused).copied().flatten() {
        *focused_window = Some(wid);
    }
    for wid in touched {
        if let Some(w) = windows.get_mut(&wid) {
            w.sync_layout(shared);
            w.renderer.window().request_redraw();
        }
    }
    Ok(())
}

/// Turn a [`MoveError`] into the string every `apply_move` caller ultimately
/// surfaces (a ctl `{ok:false,error}` reply, or an `eprintln!` for a
/// menu-triggered move) — a `Debug`-formatted `WouldEmptyTab`/`Invalid("...")`
/// reads like an internal enum name, not a message meant for a human at a
/// terminal or reading a log.
fn humanize_move_error(e: MoveError) -> String {
    match e {
        MoveError::WouldEmptyTab => {
            "the tab's only pane cannot be promoted; move the tab instead".to_string()
        }
        MoveError::Invalid(msg) => msg.to_string(),
    }
}

/// The 0-based index of `id` in `order`, if present. Generic (rather than
/// hard-coded to `WindowId`) purely so it's unit-testable: winit's
/// `WindowId::dummy()` always returns the SAME value on every call (by
/// design — see its doc), so a multi-entry `window_order` fixture with
/// distinct ids can't be built from real `WindowId`s in a test.
fn resolve_index<T: PartialEq>(order: &[T], id: &T) -> Option<usize> {
    order.iter().position(|w| w == id)
}

/// The 0-based index of `id` in `shared.window_order`, if tracked. Used both
/// by `build_move_tab`/`build_promote_pane`/`build_merge_tab` for their
/// normal (immediate) resolution, and — since a same-tick deferred `Move`
/// batch can shift `window_order` between when an action is enqueued and
/// when it actually runs — to re-resolve a deferred move's captured
/// `WindowId` against `window_order` AS IT STANDS at the moment it's finally
/// applied, not as it stood at dispatch time.
pub(crate) fn resolve_window_index(shared: &Shared, id: WindowId) -> Option<usize> {
    resolve_index(&shared.window_order, &id)
}

/// The `next`/`prev` neighbor index for a "move tab to next/previous window"
/// op: modular arithmetic over `n` windows, from current window `w`. Pulled
/// out of `build_move_tab` so it's unit-testable independent of `WindowId`
/// (see `resolve_index`'s doc for why real ids don't work in a test fixture).
fn next_prev_index(w: usize, n: usize, next: bool) -> usize {
    if next { (w + 1) % n } else { (w + n - 1) % n }
}

/// Build the `SurfaceRef`/`SurfaceDest` pair for a "move tab" op (keyboard,
/// menu, or `ctl move-tab`) from the focused window's active tab.
fn build_move_tab(
    win: &WindowState,
    shared: &Shared,
    focused_id: WindowId,
    target: MoveTabTarget,
) -> Result<(SurfaceRef, SurfaceDest), String> {
    let w = resolve_window_index(shared, focused_id).ok_or("focused window not tracked")?;
    let src = SurfaceRef::Tab {
        window: w,
        tab: win.tree.active,
    };
    let n = shared.window_order.len();
    let dest = match target {
        MoveTabTarget::New => SurfaceDest::NewWindow,
        MoveTabTarget::Window(num) => {
            if num < 1 || num > n {
                return Err(format!("no window {num} (there are {n})"));
            }
            SurfaceDest::NewTab { window: num - 1 }
        }
        MoveTabTarget::Next => {
            if n < 2 {
                return Err("only one window open".to_string());
            }
            SurfaceDest::NewTab {
                window: next_prev_index(w, n, true),
            }
        }
        MoveTabTarget::Prev => {
            if n < 2 {
                return Err("only one window open".to_string());
            }
            SurfaceDest::NewTab {
                window: next_prev_index(w, n, false),
            }
        }
    };
    Ok((src, dest))
}

/// Build the `SurfaceRef`/`SurfaceDest` pair for a "promote pane" op
/// (keyboard, menu, or `ctl promote-pane`) from the focused window's active
/// tab's focused pane.
fn build_promote_pane(
    win: &WindowState,
    shared: &Shared,
    focused_id: WindowId,
    target: PromotePaneTarget,
) -> Result<(SurfaceRef, SurfaceDest), String> {
    let w = resolve_window_index(shared, focused_id).ok_or("focused window not tracked")?;
    let src = SurfaceRef::Pane {
        window: w,
        tab: win.tree.active,
        pane: win.active_tab().focus,
    };
    let dest = match target {
        PromotePaneTarget::Tab => SurfaceDest::NewTab { window: w },
        PromotePaneTarget::Window => SurfaceDest::NewWindow,
    };
    Ok((src, dest))
}

/// Build the `SurfaceRef`/`SurfaceDest` pair for `merge-tab`: the focused
/// tab, merged as a horizontal split into the tab immediately before it (that
/// tab's own focused pane). Errors if there is no previous tab.
fn build_merge_tab(
    win: &WindowState,
    shared: &Shared,
    focused_id: WindowId,
) -> Result<(SurfaceRef, SurfaceDest), String> {
    let w = resolve_window_index(shared, focused_id).ok_or("focused window not tracked")?;
    let t = win.tree.active;
    if t == 0 {
        return Err("no previous tab".to_string());
    }
    let prev = t - 1;
    let pane = win.tree.tabs[prev].focus;
    Ok((
        SurfaceRef::Tab { window: w, tab: t },
        SurfaceDest::SplitInto {
            window: w,
            tab: prev,
            pane,
            axis: Axis::Horizontal,
            before: false,
        },
    ))
}

/// A `+`-joined modifier list (`"cmd+alt"`, possibly empty) into a
/// `ModifiersState` — `ctl drag --mods`'s own tiny parser. Deliberately NOT
/// `parse_chord`: that function requires a trailing KEY token (it treats the
/// last `+`-segment as the key, not a modifier), so it can't express
/// "modifiers held, no key" on its own. Unknown tokens are ignored rather
/// than erroring — a slightly typo'd `--mods` just holds fewer modifiers,
/// which is a saner failure mode mid-drag than aborting the whole gesture.
fn parse_mods_only(spec: &str) -> ModifiersState {
    let mut mods = ModifiersState::empty();
    for tok in spec.split('+').map(str::trim).filter(|s| !s.is_empty()) {
        match tok.to_ascii_lowercase().as_str() {
            "cmd" | "super" | "win" | "meta" => mods |= ModifiersState::SUPER,
            "shift" => mods |= ModifiersState::SHIFT,
            "alt" | "option" => mods |= ModifiersState::ALT,
            "ctrl" | "control" => mods |= ModifiersState::CONTROL,
            _ => {}
        }
    }
    mods
}

/// Synthesize a full drag gesture on `window`: a left press at `(x1, y1)`,
/// then either all `steps` intermediate motions on the way to `(x2, y2)`
/// synchronously (the default) or, if `paced_ms` is set, one motion per
/// `about_to_wait` tick spaced `paced_ms` apart (`--paced`) — either way
/// finishing with a release at `(x2, y2)` or (if `cancel`) an Escape — via
/// `WindowState::press_left`/`on_cursor_moved`/`left_release`/`cancel_drag`,
/// the EXACT same methods a real mouse/keyboard hit. `mods` (already parsed
/// by [`parse_mods_only`]) is held for the whole gesture, then restored.
///
/// Unpaced, this sends `reply` itself before returning. Paced, it stashes
/// `reply` into `shared.paced_drag` and returns without sending — the
/// paced tick in `about_to_wait` sends it once the gesture's tail actually
/// runs (see [`PacedDrag`]).
///
/// A pane drop `left_release` resolves is only STAGED (`WindowState::
/// pending_move`), not applied — [`finish_ctl_drag_tail`] runs it through
/// `apply_move` inline, right there, rather than re-queueing another
/// `DeferredWindowAction`: both the unpaced tail (called from here) and the
/// paced tail (called from `about_to_wait`'s own TAIL, after
/// `deferred_windows` has already been drained by-value) would otherwise
/// have to wait a further tick to see an outcome that's already happened by
/// the time the reply is built.
///
/// Free function (not a method) for the same reason as `apply_move`/
/// `open_window`: every call site holds `&mut Shared` (and, here, `&mut
/// self.windows`) borrowed out of `self` for the rest of `about_to_wait`.
#[allow(clippy::too_many_arguments)]
fn run_ctl_drag(
    windows: &mut HashMap<WindowId, WindowState>,
    shared: &mut Shared,
    focused_window: &mut Option<WindowId>,
    focus_history: &[WindowId],
    event_loop: &ActiveEventLoop,
    window: WindowId,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    steps: usize,
    mods: &str,
    cancel: bool,
    paced_ms: Option<u64>,
    reply: Sender<String>,
) {
    let Some(saved_mods) = windows.get(&window).map(|w| w.modifiers) else {
        let _ =
            reply.send(serde_json::json!({"ok": false, "error": "window not tracked"}).to_string());
        return;
    };
    if shared.paced_drag.is_some() {
        let _ = reply.send(
            serde_json::json!({"ok": false, "error": "a paced drag is already running"})
                .to_string(),
        );
        return;
    }
    // A live drag (real mouse or a prior ctl press) must not be overwritten:
    // a stranded DragState would still resolve safely, but the synthesized
    // press/release sequence would interleave with the user's.
    if shared.drag.is_some() {
        let _ = reply.send(
            serde_json::json!({"ok": false, "error": "a drag is already in progress"}).to_string(),
        );
        return;
    }
    let steps = steps.max(1);
    {
        let win = windows.get_mut(&window).expect("checked above");
        win.modifiers = parse_mods_only(mods);
        win.press_left(shared, x1, y1);
    }
    let waypoints: VecDeque<(f64, f64)> = (1..=steps)
        .map(|i| {
            let t = i as f64 / steps as f64;
            (x1 + (x2 - x1) * t, y1 + (y2 - y1) * t)
        })
        .collect();

    if let Some(ms) = paced_ms {
        let interval = Duration::from_millis(ms.max(1));
        shared.paced_drag = Some(PacedDrag {
            waypoints,
            interval,
            last_step: Instant::now(),
            window,
            cancel,
            saved_mods,
            reply,
        });
        // This stash runs in the deferred tail — AFTER this pass's
        // `set_control_flow` already committed. With nothing else animating
        // the loop would park in a timeout-less Wait and the paced machine
        // would only inch forward on unrelated events (observed live: a
        // paced drag starved to the client's timeout while the main thread
        // sat in BlockUntilNextEvent). Re-arm the control flow HERE so the
        // first waypoint's deadline actually wakes the loop.
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + interval));
        return; // The paced tick in `about_to_wait` sends the reply later.
    }

    // Unpaced: drive every waypoint synchronously, right now. Each step
    // re-fetches `win` fresh (rather than holding one borrow across the
    // whole loop) so `update_cross_window_drag` can reborrow `windows` as a
    // whole right after — the exact same NLL shape the real
    // `WindowEvent::CursorMoved` handler uses, so a synthesized `ctl drag`
    // exercises the identical cross-window path a real drag does.
    for (x, y) in waypoints {
        if let Some(win) = windows.get_mut(&window) {
            win.on_cursor_moved(shared, window, x, y);
        }
        if shared.drag.is_some() {
            update_cross_window_drag(windows, shared, focus_history, window, x, y, event_loop);
        }
    }
    // "mid" = right before release/cancel resolves it — whether a genuine
    // drag (not just a click) was actually in flight by then.
    let drag_active_mid = shared.drag.is_some()
        || windows
            .get(&window)
            .is_some_and(|w| w.tab_drag_is_active_drag());
    let resp = finish_ctl_drag_tail(
        windows,
        shared,
        focused_window,
        event_loop,
        window,
        cancel,
        saved_mods,
        drag_active_mid,
    );
    let _ = reply.send(resp);
}

/// The release-or-cancel tail shared by an unpaced `ctl drag` (run inline,
/// synchronously, from [`run_ctl_drag`]) and a paced one (run once its
/// waypoints are exhausted, from `about_to_wait`'s paced tick): resolve the
/// gesture, restore modifiers, clear every window's drag visuals, apply any
/// staged move, and build the JSON reply. `drag_active_mid` must be
/// captured by the caller BEFORE calling this — release/cancel clears the
/// state it reads.
#[allow(clippy::too_many_arguments)]
fn finish_ctl_drag_tail(
    windows: &mut HashMap<WindowId, WindowState>,
    shared: &mut Shared,
    focused_window: &mut Option<WindowId>,
    event_loop: &ActiveEventLoop,
    window: WindowId,
    cancel: bool,
    saved_mods: ModifiersState,
    drag_active_mid: bool,
) -> String {
    let mut ended = if cancel {
        cancel_drag_everywhere(windows, shared);
        DragEnded::Cancel
    } else if let Some(win) = windows.get_mut(&window) {
        win.left_release(shared, window)
    } else {
        DragEnded::None
    };
    if let Some(win) = windows.get_mut(&window) {
        win.modifiers = saved_mods;
    }
    // Every window's `incoming_drop`/preview is stale the instant the drag
    // resolves one way or another — belt-and-suspenders alongside whatever
    // `resolve_drag_drop`/`cancel_drag_everywhere` already cleared.
    clear_all_drag_visuals(windows);
    let pending = windows.get_mut(&window).and_then(|w| w.pending_move.take());
    let mut error = None;
    if let Some((src, dest)) = pending {
        match apply_move(windows, shared, focused_window, event_loop, src, dest) {
            Ok(()) => {
                ended = DragEnded::Move;
                pour_out_after_move(windows, shared, *focused_window);
            }
            Err(e) => {
                eprintln!("[ember] drag drop rejected: {e}");
                ended = DragEnded::Cancel;
                error = Some(e);
            }
        }
    }
    let mut reply = serde_json::json!({
        "ok": true,
        "drag_ended": ended.as_str(),
        "drag_active_mid": drag_active_mid,
    });
    if let Some(e) = error {
        reply["error"] = serde_json::json!(e);
    }
    reply.to_string()
}

impl Shared {
    pub(crate) fn shutdown_all(&mut self) {
        for (_, h) in self.sessions.drain() {
            let _ = h.control.send(BackendControl::Shutdown);
        }
    }

    /// End-of-drag hook for the wisp (Task 5): starts its ~200ms fade-out if
    /// it's currently showing. Called unconditionally from every site that
    /// clears `shared.drag` (`WindowState::left_release`,
    /// `cancel_drag_everywhere`, `clear_drag_on_window_close`) — a no-op
    /// (cheap match + no-op inside `begin_fade_out`) for the common case of a
    /// plain in-window tab reorder or a drag that never left its source
    /// window, where the wisp was never created/shown in the first place.
    pub(crate) fn wisp_end_drag(&mut self) {
        if let WispSlot::Ready(w) = &mut self.wisp {
            w.begin_fade_out();
        }
    }

    /// Whether a session is running a foreground command (idle shell → false).
    pub(crate) fn session_busy(&self, sid: &SessionId) -> bool {
        self.sessions.get(sid).is_some_and(|h| h.is_busy())
    }

    /// Bind or unbind the debug control socket at runtime (the Settings toggle).
    /// When enabling, logs the socket path so it can be handed off for inspection.
    pub(crate) fn set_developer_mode(&mut self, on: bool) {
        if on {
            if self.control_server.is_some() {
                return; // already bound (e.g. via EMBER_CONTROL)
            }
            let bind = control::server_bind_path("1"); // per-PID socket
            match control::spawn_listener(&bind, self.wake.clone()) {
                Ok((rx, server)) => {
                    eprintln!(
                        "[ember] Developer Mode ON — control socket at {}",
                        server.path().display()
                    );
                    self.control_rx = Some(rx);
                    self.control_server = Some(server);
                }
                Err(e) => eprintln!("[ember] Developer Mode: control socket failed to bind: {e}"),
            }
        } else if let Some(server) = self.control_server.take() {
            server.stop();
            self.control_rx = None;
            eprintln!("[ember] Developer Mode OFF");
        }
    }

    /// The Settings overlay's rows, resolved against the live config. The row
    /// *table* (labels, kinds, formatters, mutators) lives in `ember-core`;
    /// this just asks it to format itself against `self.config`.
    pub(crate) fn settings_rows(&self) -> Vec<SettingsRowView> {
        resolve_rows(&self.config)
    }

    /// The backdrop params for the current config at animation time `t` seconds.
    ///
    /// `sparks` (drawn at all, vs. not) follows the dial being anything but
    /// `off`, further collapsed to `false` under Low Power Mode — see
    /// `low_power_mode`'s doc comment for why that's a full hide rather than
    /// just a paused animation. It's deliberately NOT gated on any window's
    /// focus here: an unfocused window under `focused` still shows its
    /// sparks, just frozen (`WindowState::backdrop_animating` is what stops
    /// `time` from advancing for that window) — see the "no dead sparks in
    /// the newly focused window" acceptance check in the guardrails report.
    pub(crate) fn backdrop_params(&self, t: f32) -> BackdropParams {
        let bg = &self.config.background;
        let sparks = !matches!(bg.sparks, SparksMode::Off) && !self.low_power_mode();
        BackdropParams {
            gradient: bg.gradient,
            scrim: bg.scrim,
            sparks,
            density: bg.ember_density,
            time: t,
            frame_dt: 1.0 / (bg.ember_fps.clamp(1, 240) as f32),
        }
    }

    /// Sparks guardrails (v0.3.1): re-checks `platform.low_power_mode()`/
    /// `platform.reduce_motion()` together at most every `POWER_STATE_TTL`
    /// and caches the pair in `power_state` — see that field's doc comment
    /// for why a cache (and why `Cell`) at all.
    fn refresh_power_state(&self) {
        if let Some((checked_at, _, _)) = self.power_state.get() {
            if checked_at.elapsed() < POWER_STATE_TTL {
                return;
            }
        }
        self.power_state.set(Some((
            Instant::now(),
            self.platform.low_power_mode(),
            self.platform.reduce_motion(),
        )));
    }

    /// Sparks guardrails (v0.3.1): true while macOS Low Power Mode is on. The
    /// animation gate (`WindowState::backdrop_animating`) and the sparks
    /// visibility bool (`backdrop_params`) both treat this the same as the
    /// dial being `off` — Low Power Mode is the user (or the OS, on low
    /// battery) explicitly asking for less power draw, so the sparks don't
    /// just pause, they stop being drawn at all. Cached — see `power_state`.
    pub(crate) fn low_power_mode(&self) -> bool {
        self.refresh_power_state();
        self.power_state.get().is_some_and(|(_, lp, _)| lp)
    }

    /// Sparks guardrails (v0.3.1): true while the OS asks apps to reduce
    /// motion. Unlike Low Power Mode this only freezes the animation
    /// (`WindowState::backdrop_animating` returns `false`); it does not
    /// affect `backdrop_params`'s `sparks` visibility bool, so already-drawn
    /// sparks stay on screen at their last phase instead of vanishing.
    /// Cached — see `power_state`.
    pub(crate) fn reduce_motion(&self) -> bool {
        self.refresh_power_state();
        self.power_state.get().is_some_and(|(_, _, rm)| rm)
    }

    /// The ambient ember animation's frame interval, from the configured `ember_fps`
    /// cap (clamped 10–120). Lower fps ≈ proportionally less CPU.
    pub(crate) fn ember_frame(&self) -> Duration {
        let fps = self.config.background.ember_fps.clamp(10, 120);
        Duration::from_millis((1000 / fps).max(1) as u64)
    }
}

/// Content for the About overlay.
fn about_info() -> ember_render::AboutInfo {
    ember_render::AboutInfo {
        title: "ember".to_string(),
        lines: vec![
            "a native terminal".to_string(),
            String::new(),
            format!("Version   {}", env!("CARGO_PKG_VERSION")),
            format!("Commit    {}", env!("EMBER_GIT_HASH")),
            "MIT OR Apache-2.0".to_string(),
            "Brandon W. King · Claude Opus 4.8".to_string(),
            String::new(),
            "emberterm.com".to_string(),
        ],
        links: vec![
            ("Docs".to_string(), "https://emberterm.com".to_string()),
            (
                "GitHub".to_string(),
                "https://github.com/kingb/ember".to_string(),
            ),
        ],
    }
}

/// How long the visual-bell ember flash takes to fully decay (seconds).
const BELL_FLASH_SECS: f32 = 0.6;

/// Visual-bell flash intensity `[0,1]` given seconds since the BEL: full at 0,
/// quadratic ease-out to 0 at [`BELL_FLASH_SECS`] (bright flare, soft fade).
fn bell_flash_intensity(elapsed: f32) -> f32 {
    if !(0.0..BELL_FLASH_SECS).contains(&elapsed) {
        return 0.0;
    }
    let x = 1.0 - elapsed / BELL_FLASH_SECS;
    x * x
}

/// Continuous ember-glow intensity `[0,1]` for the About overlay: a slow breathe
/// with faster flicker overtones so it reads like a live, crackling ember.
fn ember_glow(t: f32) -> f32 {
    use std::f32::consts::TAU;
    let breathe = 0.55 + 0.30 * (TAU * 0.45 * t).sin();
    let flicker = 0.10 * (TAU * 3.1 * t).sin() + 0.05 * (TAU * 6.7 * t).sin();
    (breathe + flicker).clamp(0.12, 1.0)
}

/// Move `sel` by `dir` (+1/-1) among `rows`, skipping `SectionHeader` rows —
/// a header is never a valid selection. Clamped: if there's no selectable
/// row further in that direction (e.g. Up from the first selectable row,
/// just below its category header), `sel` stays put rather than landing on
/// a header or going out of bounds.
fn step_selectable_row(rows: &[SettingsRowView], sel: usize, dir: i32) -> usize {
    let n = rows.len() as i32;
    let mut i = sel as i32 + dir;
    while i >= 0 && i < n && rows[i as usize].kind == RowKind::SectionHeader {
        i += dir;
    }
    if i < 0 || i >= n { sel } else { i as usize }
}

/// The one gate every link-open passes: http/https only, exact prefix. The
/// matcher only produces these, but re-check here — this is the last line
/// between untrusted terminal output and spawning an OS opener. Scheme check
/// is case-insensitive per RFC 3986 to match the upstream matcher.
fn url_is_openable(url: &str) -> bool {
    let head = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    head.starts_with("http://") || head.starts_with("https://")
}

/// The tab title as displayed and as matched: the tab's own title, or its
/// 1-based number when unset. One rule shared by the tab strip, `ctl state`,
/// and `ctl focus`, so what an external tool matches on is exactly what the
/// user sees in the strip.
fn tab_display_title(title: &str, index: usize) -> String {
    if title.is_empty() {
        format!("{}", index + 1)
    } else {
        title.to_string()
    }
}

/// First index whose title contains `query`, case-insensitively. First match
/// wins — deterministic for callers that key off tab order.
fn match_tab_title(titles: &[String], query: &str) -> Option<usize> {
    let q = query.to_lowercase();
    titles.iter().position(|t| t.to_lowercase().contains(&q))
}

/// Cross-window `ctl focus` (Task 5): search `window_titles` — one entry per
/// window, in `Shared::window_order`, each holding that window's tab titles
/// in tab order — for the first title containing `query`
/// (case-insensitive substring, same semantics as `match_tab_title`, which
/// this delegates to per-window). First window wins ties; within a window,
/// first tab wins. Returns `(window_index, tab_index)`, both 0-based.
fn match_tab_title_across(window_titles: &[Vec<String>], query: &str) -> Option<(usize, usize)> {
    window_titles
        .iter()
        .enumerate()
        .find_map(|(wi, titles)| match_tab_title(titles, query).map(|ti| (wi, ti)))
}

/// Build the `ctl state` JSON (Task 5): a top-level `windows` array — one
/// summary per window in `Shared::window_order` (`{id,focused,active_tab,
/// tabs}`, `id` 1-based) — plus `focused_window` (1-based), plus the
/// PRE-EXISTING top-level fields (`scale_factor`/`surface`/`tabs`/
/// `active_tab`/`focus_pane`/`bracketed_paste`/`panes`) describing the
/// focused window, kept so existing single-window consumers (e.g. the
/// Stream Deck plugin) keep working unchanged.
fn build_state_json(
    shared: &Shared,
    windows: &HashMap<WindowId, WindowState>,
    focused_window: Option<WindowId>,
) -> String {
    let windows_json: Vec<serde_json::Value> = shared
        .window_order
        .iter()
        .enumerate()
        .filter_map(|(i, wid)| {
            windows.get(wid).map(|w| {
                // `pos`/`size` (physical px, screen space) are diagnostic —
                // release 2's cross-window drag verification needs a way to
                // see real window placement without an accessibility API
                // (`inner_position()` can fail on some platforms; omitted
                // when it does rather than reporting a bogus (0,0)).
                let win = w.renderer.window();
                let mut entry = serde_json::json!({
                    "id": i + 1,
                    "focused": Some(*wid) == focused_window,
                    "active_tab": w.tree.active,
                    "tabs": w.tabs_summary_json(),
                });
                if let Ok(pos) = win.inner_position() {
                    let size = win.inner_size();
                    if let Some(obj) = entry.as_object_mut() {
                        obj.insert("pos".to_string(), serde_json::json!([pos.x, pos.y]));
                        obj.insert(
                            "size".to_string(),
                            serde_json::json!([size.width, size.height]),
                        );
                    }
                }
                entry
            })
        })
        .collect();
    let focused_index = focused_window.and_then(|id| resolve_window_index(shared, id));
    let mut top: serde_json::Value = focused_window
        .and_then(|id| windows.get(&id))
        .map(|w| {
            serde_json::from_str(&w.state_json(shared)).unwrap_or_else(|_| serde_json::json!({}))
        })
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(obj) = top.as_object_mut() {
        obj.insert(
            "windows".to_string(),
            serde_json::Value::Array(windows_json),
        );
        if let Some(i) = focused_index {
            obj.insert("focused_window".to_string(), serde_json::json!(i + 1));
        }
    }
    top.to_string()
}

/// `ctl focus <query>` across every window (Task 5): search
/// `shared.window_order` (then tab order within each) via
/// `match_tab_title_across`. On match: select that tab on its window, raise
/// it, and optimistically set `focused_window` to it (same pattern as
/// window-creation/open/move) so subsequent `ctl` commands route correctly
/// even before the OS `Focused` event arrives; that later event still
/// corrects it if the user focuses elsewhere meanwhile. Reply gains 1-based
/// `window`. On miss:
/// the reply's `titles` is every window's titles flattened in search order —
/// still a flat array, so existing callers that just print `titles` see no
/// shape change, only more entries.
fn focus_across_windows(
    windows: &mut HashMap<WindowId, WindowState>,
    shared: &Shared,
    focused_window: &mut Option<WindowId>,
    query: &str,
) -> String {
    let window_titles: Vec<Vec<String>> = shared
        .window_order
        .iter()
        .map(|wid| {
            windows
                .get(wid)
                .map(WindowState::tab_titles)
                .unwrap_or_default()
        })
        .collect();
    match match_tab_title_across(&window_titles, query) {
        Some((wi, ti)) => {
            let title = window_titles[wi][ti].clone();
            let wid = shared.window_order[wi];
            if let Some(w) = windows.get_mut(&wid) {
                w.select_tab(shared, ti + 1);
                w.raise_window();
                // Optimistic, mirroring window-creation/open/move: the OS
                // Focused event corrects this if the user switches away
                // meanwhile.
                *focused_window = Some(wid);
            }
            serde_json::json!({
                "ok": true, "index": ti + 1, "title": title, "window": wi + 1,
            })
            .to_string()
        }
        None => {
            let titles: Vec<String> = window_titles.into_iter().flatten().collect();
            serde_json::json!({
                "ok": false, "error": "no tab title matches", "titles": titles,
            })
            .to_string()
        }
    }
}

/// Linux tab jump: `Alt+<digit 1-9>` -> tab number. GNOME owns Super+digits
/// (dash-favorite activation) so `Super+1..9` never reaches the app there;
/// `Alt+1..9` is the gnome-terminal/Tilix convention. Pure and un-gated so
/// both platforms' test builds exercise it; call sites are Linux-only.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn alt_digit_tab(key: &Key, mods: ModifiersState) -> Option<usize> {
    if !mods.alt_key() || mods.super_key() || mods.control_key() {
        return None;
    }
    match key {
        Key::Character(s) => {
            let mut it = s.chars();
            match (it.next(), it.next()) {
                (Some(c), None) if ('1'..='9').contains(&c) => Some(c as usize - '0' as usize),
                _ => None,
            }
        }
        _ => None,
    }
}

/// The GNOME-safe Linux chord layer (issue #5). GNOME itself owns much of
/// bare Super (arrows = tiling, Shift+arrows = move-to-monitor, D = show
/// desktop, V = notifications, digits = dash favorites), so those chords
/// often never reach the app. Linux therefore gets conventional additive
/// bindings under one learnable rule:
///
///   macOS `Cmd+X`       ->  `Ctrl+Shift+X`
///   macOS `Cmd+Shift+X` ->  `Alt+Shift+X`
///   zoom follows gnome-terminal: `Ctrl+-` (out) joins `Ctrl+Shift+=` (in)
///
/// Implemented as a translation onto the existing shortcut handler so there
/// is exactly one source of truth for what each action does. Super chords
/// keep working wherever the WM lets them through. Whitelisted, not blanket:
/// only chords with an Ember meaning are consumed; everything else still
/// reaches the shell.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_chord_translate(key: &Key, mods: ModifiersState) -> Option<(Key, ModifiersState)> {
    if mods.super_key() {
        return None; // Super path is handled by the primary gate.
    }
    let (ctrl, shift, alt) = (mods.control_key(), mods.shift_key(), mods.alt_key());
    // Shift changes the delivered character; fold shifted forms back to the
    // unshifted key the macOS handler matches on.
    let unshift = |s: &str| -> Option<&'static str> {
        Some(match s {
            "c" | "C" => "c",
            "v" | "V" => "v",
            "t" | "T" => "t",
            "n" | "N" => "n",
            "w" | "W" => "w",
            "d" | "D" => "d",
            "p" | "P" => "p",
            "/" | "?" => "/",
            "," | "<" => ",",
            "[" | "{" => "[",
            "]" | "}" => "]",
            "=" | "+" => "=",
            "-" | "_" => "-",
            "0" => "0",
            _ => return None,
        })
    };
    if ctrl && shift && !alt {
        // Ctrl+Shift+X  ==  Cmd+X
        return match key {
            Key::Character(s) => {
                unshift(s).map(|k| (Key::Character(SmolStr::new(k)), ModifiersState::empty()))
            }
            Key::Named(
                a @ (NamedKey::ArrowLeft
                | NamedKey::ArrowRight
                | NamedKey::ArrowUp
                | NamedKey::ArrowDown),
            ) => Some((Key::Named(*a), ModifiersState::empty())),
            _ => None,
        };
    }
    if alt && shift && !ctrl {
        // Alt+Shift+X  ==  Cmd+Shift+X for most of these ("d"/"p"/"n"), but
        // "t" is the one exception: macOS's Promote Pane to Tab is
        // Cmd+OPT+T (not Cmd+Shift+T — there is no such binding), so its
        // Linux chord translates onto ALT instead of SHIFT. The caller tells
        // the two apart on the returned modifiers.
        return match key {
            Key::Character(s) => match unshift(s)? {
                k @ ("d" | "p" | "n") => {
                    Some((Key::Character(SmolStr::new(k)), ModifiersState::SHIFT))
                }
                "t" => Some((Key::Character(SmolStr::new("t")), ModifiersState::ALT)),
                _ => None,
            },
            Key::Named(
                a @ (NamedKey::ArrowLeft
                | NamedKey::ArrowRight
                | NamedKey::ArrowUp
                | NamedKey::ArrowDown),
            ) => Some((Key::Named(*a), ModifiersState::SHIFT)),
            _ => None,
        };
    }
    if ctrl && !shift && !alt {
        // gnome-terminal zoom-out/reset (zoom-in arrives as Ctrl+Shift+=).
        if let Key::Character(s) = key {
            if s.as_str() == "-" || s.as_str() == "0" {
                return Some((Key::Character(s.clone()), ModifiersState::empty()));
            }
        }
    }
    None
}

/// Whether releasing the left button should CLEAR the selection instead of
/// keeping it: a plain click (press+release, no drag) leaves a degenerate
/// single-cell Simple selection, and terminals treat that as "clear what was
/// selected", not "select this cell". A real drag (active moved off the
/// anchor) survives, as do word/line click-selections (mode != Simple
/// expands at copy time even while anchor == active).
fn click_selection_should_clear(sel: Option<&Selection>) -> bool {
    sel.is_some_and(|s| s.mode == SelectionMode::Simple && s.anchor == s.active)
}

/// The keyboard cheat-sheet shown by the Cmd+/ overlay. Keep in sync with
/// [`WindowState::handle_shortcut`].
/// Prepare paste bytes. When `bracketed`, wrap the text in the bracketed-paste
/// guards `ESC[200~` … `ESC[201~`, stripping any embedded guard sequences first so
/// a hostile clipboard can't close the bracket early and inject a command the shell
/// would then execute. When not bracketed, send the text unchanged.
fn bracket_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return text.as_bytes().to_vec();
    }
    let cleaned = text.replace("\x1b[200~", "").replace("\x1b[201~", "");
    let mut out = Vec::with_capacity(cleaned.len() + 12);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(cleaned.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
}

/// The keyboard cheat-sheet, grouped into sections. A row with an empty key is a
/// **section header** (rendered as an accent heading by `build_help`); the rest are
/// `(key, description)`. Keep in sync with [`WindowState::handle_shortcut`].
pub(crate) fn help_lines() -> Vec<(String, String)> {
    // Every row carries its platform's true binding. macOS uses Cmd; Linux
    // uses the GNOME-safe conventional layer (Ctrl+Shift+X for Cmd+X,
    // Alt+Shift+X for Cmd+Shift+X, Alt+1..9 for tabs) because GNOME itself
    // owns much of bare Super — see linux_chord_translate. Super variants
    // still work on Linux where the WM delivers them; the sheet shows the
    // bindings that work everywhere.
    let mac = cfg!(target_os = "macos");
    let k = |mac_k: &str, linux_k: &str| {
        if mac {
            mac_k.to_string()
        } else {
            linux_k.to_string()
        }
    };
    let r = |key: String, d: &str| (key, d.to_string());
    vec![
        r("".into(), "PANES"),
        r(k("Cmd+D", "Ctrl+Shift+D"), "Split right (side by side)"),
        r(k("Cmd+Shift+D", "Alt+Shift+D"), "Split down (stacked)"),
        r(
            k("Ctrl+Opt+Click", "Ctrl+Alt+Click"),
            "Split by drop zone (drag to preview)",
        ),
        r(k("Cmd+W", "Ctrl+Shift+W"), "Close pane"),
        r("Click pane".into(), "Focus it"),
        r(k("Cmd+Arrows", "Ctrl+Shift+Arrows"), "Focus pane"),
        r("".into(), "TABS"),
        r(k("Cmd+T", "Ctrl+Shift+T"), "New tab"),
        r(k("Cmd+N", "Ctrl+Shift+N"), "New window"),
        r(k("Cmd+Shift+Arrows", "Alt+Shift+Arrows"), "Switch tab"),
        r(k("Cmd+1..9", "Alt+1..9"), "Jump to tab"),
        r("Drag / Double-click".into(), "Reorder / rename tab"),
        r("".into(), "WINDOWS"),
        r(k("Cmd+Shift+N", "Alt+Shift+N"), "Move tab to new window"),
        r(k("Cmd+Opt+T", "Alt+Shift+T"), "Promote pane to tab"),
        r(
            "Window menu".into(),
            "Move tab to next/previous window, promote pane to new window, merge tab",
        ),
        r("".into(), "SELECTION & CLIPBOARD"),
        r(
            "Drag / 2\u{d7}/3\u{d7} click".into(),
            "Select text / word / line",
        ),
        r(
            k("Cmd+C / Cmd+V", "Ctrl+Shift+C / Ctrl+Shift+V"),
            "Copy / paste",
        ),
        r("".into(), "SCROLLBACK"),
        r("Wheel / Shift+PgUp/Dn".into(), "Scroll history"),
        r("Shift+Home / End".into(), "Jump to top / bottom"),
        r(
            k("Cmd+[ / Cmd+]", "Ctrl+Shift+[ / ]"),
            "Previous / next prompt",
        ),
        r("".into(), "VIEW"),
        r(k("Cmd+= / Cmd+-", "Ctrl+Shift+= / Ctrl+-"), "Zoom in / out"),
        r(k("Cmd+0", "Ctrl+0"), "Reset zoom"),
        r(k("Cmd+,", "Ctrl+Shift+,"), "Settings"),
        r(k("Cmd+/", "Ctrl+Shift+/"), "This cheat sheet"),
    ]
}

/// Parse a key token (`enter`, `tab`, `arrowleft`/`left`, or a single char) into a
/// winit [`Key`]. Used by the debug control surface.
fn named_key(name: &str) -> Option<Key> {
    Some(match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => Key::Named(NamedKey::Enter),
        "tab" => Key::Named(NamedKey::Tab),
        "esc" | "escape" => Key::Named(NamedKey::Escape),
        "backspace" => Key::Named(NamedKey::Backspace),
        "space" => Key::Named(NamedKey::Space),
        "left" | "arrowleft" => Key::Named(NamedKey::ArrowLeft),
        "right" | "arrowright" => Key::Named(NamedKey::ArrowRight),
        "up" | "arrowup" => Key::Named(NamedKey::ArrowUp),
        "down" | "arrowdown" => Key::Named(NamedKey::ArrowDown),
        s if s.chars().count() == 1 => Key::Character(s.into()),
        _ => return None,
    })
}

/// Parse a chord like `cmd+shift+arrowright` or `cmd+d` into a key + modifiers, so
/// the control surface can drive the same shortcut path as real keystrokes.
fn parse_chord(combo: &str) -> Option<(Key, ModifiersState)> {
    let parts: Vec<&str> = combo
        .split('+')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let (key_tok, mod_toks) = parts.split_last()?;
    let mut mods = ModifiersState::empty();
    for m in mod_toks {
        match m.to_ascii_lowercase().as_str() {
            "cmd" | "super" | "win" | "meta" => mods |= ModifiersState::SUPER,
            "shift" => mods |= ModifiersState::SHIFT,
            "alt" | "option" => mods |= ModifiersState::ALT,
            "ctrl" | "control" => mods |= ModifiersState::CONTROL,
            _ => return None,
        }
    }
    Some((named_key(key_tok)?, mods))
}

/// Map a key press to the bytes to send to the PTY. Covers the essentials for a
/// usable shell (printable text, Enter/Backspace/Tab/Esc, arrows, Ctrl-letter);
/// fuller keymap + IME routing land with Epic E.
/// Encode a key press as the bytes a VT terminal sends (xterm-compatible).
///
/// `app_cursor` = DECCKM (arrows/Home/End become `ESC O x`); `alt_meta` = the
/// user holds Option with option-as-meta enabled, so simple keys get an ESC
/// prefix (readline/emacs Meta). CSI-form keys carry all modifiers in the
/// standard `;m` parameter instead (m = 1 + shift·1 + alt·2 + ctrl·4).
fn encode_key(
    key: &Key,
    mods: ModifiersState,
    app_cursor: bool,
    alt_meta: bool,
) -> Option<Vec<u8>> {
    let m =
        1 + mods.shift_key() as u8 + (mods.alt_key() as u8) * 2 + (mods.control_key() as u8) * 4;
    let modified = m > 1;
    // "PC-style" cursor keys: CSI-with-modifiers > SS3 (app cursor) > CSI.
    let cursor = |ch: char| -> Vec<u8> {
        if modified {
            format!("\x1b[1;{m}{ch}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{ch}").into_bytes()
        } else {
            format!("\x1b[{ch}").into_bytes()
        }
    };
    // VT220-style editing/function keys: `CSI n ~`, modifiers as `CSI n;m~`.
    let tilde = |n: u8| -> Vec<u8> {
        if modified {
            format!("\x1b[{n};{m}~").into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        }
    };
    // F1–F4 are SS3 legacy; with modifiers they switch to the CSI form.
    let ss3_f = |ch: char| -> Vec<u8> {
        if modified {
            format!("\x1b[1;{m}{ch}").into_bytes()
        } else {
            format!("\x1bO{ch}").into_bytes()
        }
    };
    // ESC-prefix for Meta on the simple byte-form keys.
    let meta = |bytes: Vec<u8>| -> Vec<u8> {
        if alt_meta {
            let mut v = Vec::with_capacity(bytes.len() + 1);
            v.push(0x1b);
            v.extend(bytes);
            v
        } else {
            bytes
        }
    };
    match key {
        Key::Named(named) => {
            let bytes = match named {
                NamedKey::Enter => meta(b"\r".to_vec()),
                NamedKey::Backspace => meta(vec![0x7f]),
                NamedKey::Tab if mods.shift_key() => b"\x1b[Z".to_vec(), // backtab
                NamedKey::Tab => meta(b"\t".to_vec()),
                NamedKey::Escape => vec![0x1b],
                NamedKey::Space if mods.control_key() => meta(vec![0x00]), // NUL (C-SPC)
                NamedKey::Space => meta(b" ".to_vec()),
                NamedKey::ArrowUp => cursor('A'),
                NamedKey::ArrowDown => cursor('B'),
                NamedKey::ArrowRight => cursor('C'),
                NamedKey::ArrowLeft => cursor('D'),
                NamedKey::Home => cursor('H'),
                NamedKey::End => cursor('F'),
                NamedKey::Insert => tilde(2),
                NamedKey::Delete => tilde(3),
                NamedKey::PageUp => tilde(5),
                NamedKey::PageDown => tilde(6),
                NamedKey::F1 => ss3_f('P'),
                NamedKey::F2 => ss3_f('Q'),
                NamedKey::F3 => ss3_f('R'),
                NamedKey::F4 => ss3_f('S'),
                NamedKey::F5 => tilde(15),
                NamedKey::F6 => tilde(17),
                NamedKey::F7 => tilde(18),
                NamedKey::F8 => tilde(19),
                NamedKey::F9 => tilde(20),
                NamedKey::F10 => tilde(21),
                NamedKey::F11 => tilde(23),
                NamedKey::F12 => tilde(24),
                _ => return None,
            };
            Some(bytes)
        }
        Key::Character(s) => {
            if mods.control_key() {
                let c = s.chars().next()?;
                // Classic control-byte mapping, incl. the punctuation the old
                // path dropped (C-[ = ESC, C-\, C-], C-^, C-_, C-? = DEL) and
                // the xterm digit aliases (C-2 = NUL … C-8 = DEL).
                let ctrl = match c.to_ascii_lowercase() {
                    '@' | '2' => Some(0x00),
                    'a'..='z' => Some(c.to_ascii_lowercase() as u8 & 0x1f),
                    '[' | '3' => Some(0x1b),
                    '\\' | '4' => Some(0x1c),
                    ']' | '5' => Some(0x1d),
                    '^' | '6' => Some(0x1e),
                    '_' | '7' | '/' => Some(0x1f),
                    '?' | '8' => Some(0x7f),
                    _ => None,
                };
                if let Some(b) = ctrl {
                    return Some(meta(vec![b]));
                }
            }
            Some(meta(s.as_bytes().to_vec()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BELL_FLASH_SECS, DeferredMoveOp, DeferredWindowAction, bell_flash_intensity, bracket_paste,
        encode_key, match_tab_title, match_tab_title_across, next_prev_index, queue_close_this,
        queue_close_window, resolve_index, tab_display_title, url_is_openable,
    };
    use winit::keyboard::{Key, ModifiersState, NamedKey, SmolStr};

    fn enc(key: Key, mods: ModifiersState) -> Option<Vec<u8>> {
        encode_key(&key, mods, false, false)
    }

    #[test]
    fn named_editing_and_function_keys_encode() {
        let n = ModifiersState::empty();
        assert_eq!(enc(Key::Named(NamedKey::Home), n).unwrap(), b"\x1b[H");
        assert_eq!(enc(Key::Named(NamedKey::End), n).unwrap(), b"\x1b[F");
        assert_eq!(enc(Key::Named(NamedKey::PageUp), n).unwrap(), b"\x1b[5~");
        assert_eq!(enc(Key::Named(NamedKey::PageDown), n).unwrap(), b"\x1b[6~");
        assert_eq!(enc(Key::Named(NamedKey::Delete), n).unwrap(), b"\x1b[3~");
        assert_eq!(enc(Key::Named(NamedKey::Insert), n).unwrap(), b"\x1b[2~");
        assert_eq!(enc(Key::Named(NamedKey::F1), n).unwrap(), b"\x1bOP");
        assert_eq!(enc(Key::Named(NamedKey::F5), n).unwrap(), b"\x1b[15~");
        assert_eq!(enc(Key::Named(NamedKey::F12), n).unwrap(), b"\x1b[24~");
    }

    #[test]
    fn arrows_follow_decckm_and_modifiers() {
        let n = ModifiersState::empty();
        let up = Key::Named(NamedKey::ArrowUp);
        let right = Key::Named(NamedKey::ArrowRight);
        assert_eq!(enc(up.clone(), n).unwrap(), b"\x1b[A");
        // DECCKM: application cursor keys use SS3.
        assert_eq!(encode_key(&up, n, true, false).unwrap(), b"\x1bOA");
        // Ctrl+Right = CSI 1;5C (word-jump); modifiers beat app-cursor form.
        assert_eq!(
            encode_key(&right, ModifiersState::CONTROL, true, false).unwrap(),
            b"\x1b[1;5C"
        );
        // Shift+Alt+Down = 1 + 1 + 2 = 4.
        assert_eq!(
            enc(
                Key::Named(NamedKey::ArrowDown),
                ModifiersState::SHIFT | ModifiersState::ALT
            )
            .unwrap(),
            b"\x1b[1;4B"
        );
    }

    #[test]
    fn control_specials_encode() {
        let c = ModifiersState::CONTROL;
        // C-SPC = NUL (emacs set-mark), the old path sent a plain space.
        assert_eq!(enc(Key::Named(NamedKey::Space), c).unwrap(), vec![0x00]);
        assert_eq!(
            enc(Key::Character(SmolStr::new("[")), c).unwrap(),
            vec![0x1b]
        );
        assert_eq!(
            enc(Key::Character(SmolStr::new("]")), c).unwrap(),
            vec![0x1d]
        );
        assert_eq!(
            enc(Key::Character(SmolStr::new("_")), c).unwrap(),
            vec![0x1f]
        );
        assert_eq!(
            enc(Key::Character(SmolStr::new("?")), c).unwrap(),
            vec![0x7f]
        );
        assert_eq!(
            enc(Key::Character(SmolStr::new("c")), c).unwrap(),
            vec![0x03]
        );
    }

    #[test]
    fn shift_tab_is_backtab() {
        assert_eq!(
            enc(Key::Named(NamedKey::Tab), ModifiersState::SHIFT).unwrap(),
            b"\x1b[Z"
        );
    }

    #[test]
    fn mouse_reports_encode_sgr_and_x10() {
        use super::WindowState;
        // SGR press/release: 1-based coords, M/m terminator.
        assert_eq!(
            WindowState::mouse_report_bytes(true, 0, 4, 9, true),
            b"\x1b[<0;5;10M"
        );
        assert_eq!(
            WindowState::mouse_report_bytes(true, 0, 4, 9, false),
            b"\x1b[<0;5;10m"
        );
        // Wheel up with ctrl (+16).
        assert_eq!(
            WindowState::mouse_report_bytes(true, 64 + 16, 0, 0, true),
            b"\x1b[<80;1;1M"
        );
        // X10: +32 offsets, release is button 3.
        assert_eq!(
            WindowState::mouse_report_bytes(false, 0, 4, 9, true),
            vec![0x1b, b'[', b'M', 32, 37, 42]
        );
        assert_eq!(
            WindowState::mouse_report_bytes(false, 0, 4, 9, false),
            vec![0x1b, b'[', b'M', 35, 37, 42]
        );
    }

    #[test]
    fn option_as_meta_prefixes_esc() {
        let alt = ModifiersState::ALT;
        // Opt+b with option_as_meta: ESC b (readline backward-word).
        assert_eq!(
            encode_key(&Key::Character(SmolStr::new("b")), alt, false, true).unwrap(),
            b"\x1bb"
        );
        // Without the option, the composed char passes through untouched.
        assert_eq!(
            encode_key(&Key::Character(SmolStr::new("\u{222b}")), alt, false, false).unwrap(),
            "\u{222b}".as_bytes()
        );
    }

    #[test]
    fn bell_flash_decays_from_full_to_zero() {
        assert_eq!(bell_flash_intensity(0.0), 1.0); // full at the bel
        assert_eq!(bell_flash_intensity(BELL_FLASH_SECS), 0.0); // gone at the end
        assert_eq!(bell_flash_intensity(BELL_FLASH_SECS + 1.0), 0.0); // stays gone
        let mid = bell_flash_intensity(BELL_FLASH_SECS * 0.5);
        assert!(mid > 0.0 && mid < 1.0); // monotone decay through the middle
    }

    #[test]
    fn paste_unbracketed_is_raw() {
        assert_eq!(bracket_paste("ls -la\n", false), b"ls -la\n".to_vec());
    }

    #[test]
    fn paste_bracketed_wraps() {
        assert_eq!(bracket_paste("hi", true), b"\x1b[200~hi\x1b[201~".to_vec());
    }

    #[test]
    fn paste_bracketed_strips_embedded_end_marker() {
        // A hostile clipboard trying to break out of the bracket: the embedded
        // ESC[201~ is removed so the payload can't escape into command position.
        let got = bracket_paste("a\x1b[201~rm -rf /\n", true);
        assert_eq!(got, b"\x1b[200~arm -rf /\n\x1b[201~".to_vec());
    }

    #[test]
    fn only_http_and_https_pass_the_open_guard() {
        assert!(url_is_openable("http://example.com"));
        assert!(url_is_openable("https://example.com/a?b#c"));
        assert!(!url_is_openable("file:///etc/passwd"));
        assert!(!url_is_openable("javascript:alert(1)"));
        assert!(!url_is_openable("ftp://example.com"));
        assert!(!url_is_openable("httpss://example.com"));
        assert!(url_is_openable("HTTP://EXAMPLE.COM"));
        assert!(url_is_openable("HtTpS://example.com"));
    }

    #[test]
    fn tab_display_title_falls_back_to_the_tab_number() {
        assert_eq!(tab_display_title("build", 0), "build");
        assert_eq!(tab_display_title("", 0), "1");
        assert_eq!(tab_display_title("", 4), "5");
    }

    #[test]
    fn alt_digit_selects_tabs_one_through_nine_only() {
        use super::alt_digit_tab;
        let ch = |c: &str| Key::Character(SmolStr::new(c));
        let alt = ModifiersState::ALT;
        assert_eq!(alt_digit_tab(&ch("1"), alt), Some(1));
        assert_eq!(alt_digit_tab(&ch("9"), alt), Some(9));
        // 0 is not a tab; multi-char and named keys don't count.
        assert_eq!(alt_digit_tab(&ch("0"), alt), None);
        assert_eq!(alt_digit_tab(&ch("12"), alt), None);
        assert_eq!(alt_digit_tab(&Key::Named(NamedKey::Enter), alt), None);
        // Alt must be the sole chord modifier (Shift alone is fine for AZERTY-
        // style layouts, but Super/Ctrl combos belong to other bindings).
        assert_eq!(alt_digit_tab(&ch("2"), ModifiersState::empty()), None);
        assert_eq!(alt_digit_tab(&ch("2"), alt | ModifiersState::SUPER), None);
        assert_eq!(alt_digit_tab(&ch("2"), alt | ModifiersState::CONTROL), None);
    }

    #[test]
    fn linux_chords_translate_onto_the_mac_shortcut_table() {
        use super::linux_chord_translate as tr;
        let ch = |c: &str| Key::Character(SmolStr::new(c));
        let cs = ModifiersState::CONTROL | ModifiersState::SHIFT;
        let als = ModifiersState::ALT | ModifiersState::SHIFT;
        let none = ModifiersState::empty();
        // Ctrl+Shift+X == Cmd+X, shifted characters folded back.
        assert_eq!(tr(&ch("T"), cs), Some((ch("t"), none)));
        assert_eq!(tr(&ch("N"), cs), Some((ch("n"), none)), "new window");
        assert_eq!(tr(&ch("c"), cs), Some((ch("c"), none)));
        assert_eq!(tr(&ch("?"), cs), Some((ch("/"), none)));
        assert_eq!(tr(&ch("<"), cs), Some((ch(","), none)));
        assert_eq!(tr(&ch("{"), cs), Some((ch("["), none)));
        assert_eq!(tr(&ch("+"), cs), Some((ch("="), none)));
        assert_eq!(
            tr(&Key::Named(NamedKey::ArrowLeft), cs),
            Some((Key::Named(NamedKey::ArrowLeft), none))
        );
        // Alt+Shift+X == Cmd+Shift+X (split down, tab cycling, fps overlay,
        // move tab to new window).
        assert_eq!(tr(&ch("D"), als), Some((ch("d"), ModifiersState::SHIFT)));
        assert_eq!(
            tr(&ch("N"), als),
            Some((ch("n"), ModifiersState::SHIFT)),
            "move tab to new window"
        );
        assert_eq!(
            tr(&Key::Named(NamedKey::ArrowRight), als),
            Some((Key::Named(NamedKey::ArrowRight), ModifiersState::SHIFT))
        );
        // Alt+Shift+T is the one exception: it stands in for Cmd+OPT+T
        // (promote pane to tab), not Cmd+Shift+T (no such binding exists) —
        // so it carries ALT, not SHIFT, on the translated modifiers, which is
        // exactly how the caller tells it apart from every other alt+shift chord.
        assert_eq!(
            tr(&ch("T"), als),
            Some((ch("t"), ModifiersState::ALT)),
            "promote pane to tab"
        );
        // gnome-terminal zoom-out / reset.
        assert_eq!(tr(&ch("-"), ModifiersState::CONTROL), Some((ch("-"), none)));
        assert_eq!(tr(&ch("0"), ModifiersState::CONTROL), Some((ch("0"), none)));
        // NOT consumed: plain Ctrl+C (SIGINT!), unknown chords, Super combos.
        assert_eq!(tr(&ch("c"), ModifiersState::CONTROL), None);
        assert_eq!(tr(&ch("r"), cs), None);
        assert_eq!(tr(&ch("t"), cs | ModifiersState::SUPER), None);
    }

    #[test]
    fn plain_click_clears_but_drag_word_and_line_selections_survive() {
        use super::click_selection_should_clear as should_clear;
        use ember_render::{Point, Selection, SelectionMode};
        let sel = |anchor: (u16, u16), active: (u16, u16), mode| Selection {
            anchor: Point::new(anchor.0, anchor.1),
            active: Point::new(active.0, active.1),
            mode,
        };
        // Plain click: collapsed simple selection -> clear.
        assert!(should_clear(Some(&sel(
            (2, 3),
            (2, 3),
            SelectionMode::Simple
        ))));
        // Dragged even one cell -> keep.
        assert!(!should_clear(Some(&sel(
            (2, 3),
            (2, 4),
            SelectionMode::Simple
        ))));
        assert!(!should_clear(Some(&sel(
            (2, 3),
            (5, 1),
            SelectionMode::Simple
        ))));
        // Double/triple click: collapsed anchor but word/line mode -> keep.
        assert!(!should_clear(Some(&sel(
            (2, 3),
            (2, 3),
            SelectionMode::Word
        ))));
        assert!(!should_clear(Some(&sel(
            (2, 3),
            (2, 3),
            SelectionMode::Line
        ))));
        // No selection at all -> nothing to clear.
        assert!(!should_clear(None));
    }

    #[test]
    fn tab_title_matching_is_case_insensitive_substring_first_match() {
        let titles: Vec<String> = ["Agent Alpha", "build", "agent beta"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            match_tab_title(&titles, "agent"),
            Some(0),
            "first match wins"
        );
        assert_eq!(match_tab_title(&titles, "BETA"), Some(2));
        assert_eq!(match_tab_title(&titles, "uild"), Some(1), "substring");
        assert_eq!(match_tab_title(&titles, "gamma"), None);
    }

    #[test]
    fn tab_title_matching_across_windows_first_window_wins_ties() {
        // Both windows have a tab matching "shared" — the earlier window in
        // `window_order` (index 0) wins, even though its match is the SECOND
        // tab there (tab order within a window is still first-match).
        let windows = vec![
            vec!["alpha".to_string(), "shared".to_string()],
            vec!["shared".to_string(), "beta".to_string()],
        ];
        assert_eq!(
            match_tab_title_across(&windows, "shared"),
            Some((0, 1)),
            "first window wins the tie"
        );
    }

    #[test]
    fn tab_title_matching_across_windows_is_case_insensitive_substring() {
        let windows = vec![
            vec!["Agent Alpha".to_string()],
            vec!["agent beta".to_string()],
        ];
        assert_eq!(match_tab_title_across(&windows, "BETA"), Some((1, 0)));
        assert_eq!(
            match_tab_title_across(&windows, "gent"),
            Some((0, 0)),
            "substring, first window wins"
        );
    }

    #[test]
    fn tab_title_matching_across_windows_miss_returns_none() {
        let windows = vec![vec!["alpha".to_string()], vec!["beta".to_string()]];
        assert_eq!(match_tab_title_across(&windows, "gamma"), None);
    }

    /// `deferred_windows` is a `Vec`, not a single `Option`, specifically so a
    /// same-tick `ctl new-window` + close-confirm resolution can't clobber
    /// each other — this pins down the ordering and de-duplication contract
    /// that guarantees, without a live window.
    #[test]
    fn deferred_window_actions_preserve_write_order_and_dedup_close() {
        // Two distinct opens (e.g. two batched `ctl new-window` commands) are
        // both real, independent windows — neither is dropped or merged.
        let v: Vec<DeferredWindowAction> = vec![
            DeferredWindowAction::OpenNew(Some("a".into())),
            DeferredWindowAction::OpenNew(Some("b".into())),
        ];
        assert_eq!(v.len(), 2);

        // An OpenNew queued before a close (e.g. a batched `ctl new-window`
        // racing a close-confirm resolution in the same drain) keeps BOTH,
        // in write order — this is the exact case that used to silently
        // drop one of the two with a single `Option` slot.
        let mut v: Vec<DeferredWindowAction> = Vec::new();
        v.push(DeferredWindowAction::OpenNew(None));
        queue_close_this(&mut v);
        assert!(matches!(v[0], DeferredWindowAction::OpenNew(_)));
        assert!(matches!(v[1], DeferredWindowAction::CloseThis));

        // `CloseThis` always targets the same window (`focused_id`), so a
        // second, independent trigger for it in the same tick (e.g. an
        // explicit close that empties the window's last tab, which the
        // organic tabs-empty check then also notices) must NOT enqueue a
        // second copy — applying `finish_close` twice would re-check
        // `windows.len()` against already-mutated state and could tear down
        // every remaining window instead of a no-op.
        let mut v: Vec<DeferredWindowAction> = Vec::new();
        queue_close_this(&mut v);
        queue_close_this(&mut v);
        queue_close_this(&mut v);
        assert_eq!(v.len(), 1);
    }

    /// `queue_close_window` is `CloseWindow`'s analogue of `queue_close_this`
    /// (em-... background-window-never-torn-down fix): a background window's
    /// last shell can exit twice in the same tick — a two-tab window whose
    /// final two panes' shells both exit together, say — and `close_session`
    /// reports the tree emptied for BOTH. Without this dedup, that queues
    /// `finish_close` twice for the same already-removed id; harmless given
    /// `finish_close`'s own `!windows.contains_key` guard, but this keeps the
    /// action list free of redundant entries. A DIFFERENT window's close is
    /// always kept — this must never merge two distinct windows' closes into
    /// one.
    #[test]
    fn queue_close_window_dedups_same_id_but_keeps_distinct_ids() {
        use winit::window::WindowId;
        let a = WindowId::dummy();
        let mut v: Vec<DeferredWindowAction> = Vec::new();
        queue_close_window(&mut v, a);
        queue_close_window(&mut v, a);
        assert_eq!(v.len(), 1);
        match &v[0] {
            DeferredWindowAction::CloseWindow(id) => assert_eq!(*id, a),
            other => panic!("expected CloseWindow(a), got {other:?}"),
        }

        // `WindowId` has no public constructor besides `dummy()`, so a second
        // distinct id isn't available to test here; `winit::window::WindowId`
        // is a thin wrapper this crate doesn't control, and this is already
        // sufficient to pin the dedup-by-id (not dedup-any-CloseWindow)
        // contract down against a regression to `queue_close_this`'s
        // variant-only dedup shape.
    }

    /// `DeferredWindowAction::Move` carries the source window's IDENTITY
    /// (`WindowId`) and a symbolic op, not a pre-resolved `SurfaceRef`/
    /// `SurfaceDest` — this pins that payload shape down (a regression here
    /// would silently reintroduce the baked-index staleness a same-tick
    /// batch of moves can hit) and confirms it slots into the same
    /// write-order `Vec` as every other deferred action, alongside them.
    #[test]
    fn deferred_move_carries_window_identity_and_preserves_write_order() {
        use winit::window::WindowId;
        let wid = WindowId::dummy();
        let v: Vec<DeferredWindowAction> = vec![
            DeferredWindowAction::OpenNew(None),
            DeferredWindowAction::Move(wid, DeferredMoveOp::MergeTab, None),
        ];
        assert_eq!(v.len(), 2);
        assert!(matches!(v[0], DeferredWindowAction::OpenNew(_)));
        match &v[1] {
            DeferredWindowAction::Move(source, DeferredMoveOp::MergeTab, None) => {
                assert_eq!(*source, wid);
            }
            other => panic!("expected a MergeTab Move carrying `wid`, got {other:?}"),
        }
    }

    /// `resolve_index` (the generic core behind `resolve_window_index`):
    /// present -> its position in the order; absent -> `None`. Generic over
    /// plain integers standing in for `WindowId`s, since `WindowId::dummy()`
    /// can't produce distinct ids for a multi-entry fixture (see its doc).
    #[test]
    fn resolve_index_finds_present_ids_and_misses_absent_ones() {
        let order = [10u32, 20, 30];
        assert_eq!(resolve_index(&order, &10), Some(0));
        assert_eq!(resolve_index(&order, &20), Some(1));
        assert_eq!(resolve_index(&order, &30), Some(2));
        // Not in the order at all (e.g. a window that closed since this id
        // was captured) -> None, never a stale/wrong index.
        assert_eq!(resolve_index(&order, &99), None);
    }

    /// `next_prev_index`'s modular arithmetic against a 3-entry order:
    /// wraps at both ends, and next/prev are each other's inverse.
    #[test]
    fn next_prev_index_wraps_across_a_three_entry_order() {
        let n = 3;
        // Next: 0->1->2->0.
        assert_eq!(next_prev_index(0, n, true), 1);
        assert_eq!(next_prev_index(1, n, true), 2);
        assert_eq!(next_prev_index(2, n, true), 0, "wraps forward past the end");
        // Prev: 0->2->1->0.
        assert_eq!(
            next_prev_index(0, n, false),
            2,
            "wraps backward past the start"
        );
        assert_eq!(next_prev_index(1, n, false), 0);
        assert_eq!(next_prev_index(2, n, false), 1);
    }
}
