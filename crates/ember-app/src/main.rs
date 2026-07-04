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

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use control::ControlMsg;

use ember_core::{
    Axis, BackendControl, BackendEvent, BackendHandle, ClipboardOp, Config, Direction, GridDims,
    LayoutCommand, LayoutEffect, LayoutNode, OscEvent, PaneId, Rect, ScrollAmount, SessionBackend,
    SessionId, Tab, TabId, apply, layout,
};
use ember_platform::MenuAction;
use ember_render::{
    BackdropParams, CELL_HEIGHT, CELL_WIDTH, ConfirmView, ImageFit, Point, RenderOutcome, Renderer,
    Selection, SelectionMode, TabHit, TabLabel, VisiblePane,
};
use ember_session::{LocalPty, LocalPtyConfig};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::window::{CursorIcon, WindowId};

/// The Ember app icon (embedded). Set on the window + the macOS dock at startup.
const ICON_PNG: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icon.png"));

pub(crate) const PAD: f32 = 4.0;
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

/// The winit user-event type: a wake nudge from the PTY frame lane or the
/// control socket, so the loop can idle on `ControlFlow::Wait` instead of
/// polling and only run when there is genuinely something to do.
#[derive(Debug, Clone, Copy)]
enum EmberEvent {
    Wake,
}
/// Redraw cadence (~60fps) while an animation (e.g. the About glow) is active.
const ANIM_FRAME: Duration = Duration::from_millis(16);
/// Max gap between clicks at the same cell to count as a double/triple click.
const MULTI_CLICK: Duration = Duration::from_millis(400);
/// Scrollback lines per mouse-wheel notch (Alacritty/Ghostty default).
const WHEEL_LINES: i32 = 3;

fn main() {
    let args: Vec<String> = std::env::args().collect();
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
        state: None,
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
    state: Option<RunState>,
    /// Receiver for debug-control commands (Some while the control socket is bound).
    control_rx: Option<Receiver<ControlMsg>>,
    /// The bound control listener (from EMBER_CONTROL); moved into RunState.
    control_server: Option<control::ControlServer>,
    /// Wakes the event loop from the PTY frame lane; handed to each session.
    wake: std::sync::Arc<dyn Fn() + Send + Sync>,
}

struct RunState {
    renderer: Renderer,
    /// The multiplexer model (one tab list, one binary split tree per tab).
    tree: ember_core::WindowTree,
    /// One running session per pane leaf, keyed by its `SessionId`.
    sessions: HashMap<SessionId, BackendHandle>,
    /// Last grid dims pushed to each session — so resizes are only sent on change.
    dims_cache: HashMap<SessionId, GridDims>,
    modifiers: ModifiersState,
    /// Physical surface size in px.
    px: (u32, u32),
    next_pane: u64,
    next_session: u64,
    next_tab: u64,
    /// Debug-control command receiver (drained each poll tick).
    control_rx: Option<Receiver<ControlMsg>>,
    /// The bound control listener, if the socket is currently open (Settings
    /// toggle / EMBER_CONTROL). Dropping/stopping it closes the socket.
    control_server: Option<control::ControlServer>,
    /// Whether the keyboard cheat-sheet overlay is showing.
    help: bool,
    /// Whether the About overlay is showing, and when it opened (for the glow clock).
    about: bool,
    about_since: Instant,
    /// User config + the Settings overlay state (open + selected row).
    config: Config,
    settings_open: bool,
    settings_sel: usize,
    /// The backdrop-image path currently uploaded to the renderer, so
    /// `apply_appearance` re-decodes only when the configured path changes.
    image_loaded: Option<String>,
    /// Backdrop animation clock + whether the window is focused (sparks pause when
    /// unfocused, per the perf stance).
    backdrop_since: Instant,
    window_focused: bool,
    /// FPS/frame-time debug overlay (toggle: Cmd+Shift+P / `ctl fps`). EMAs of the
    /// redraw interval (cadence) and the render() call duration (per-frame cost).
    fps_overlay: bool,
    last_frame: Option<Instant>,
    fps_ema_ms: f32,
    render_ema_ms: f32,
    /// When the last animation frame was advanced+redrawn. Animation is paced by
    /// wall-clock elapsed since this (checked on every wake), NOT by the timer's
    /// `ResumeTimeReached` — a flood of mouse-move events would otherwise keep
    /// resetting the `WaitUntil` deadline and starve the animation (visible stutter).
    last_anim: Instant,
    /// Visual bell: when the current ember flash started (None = no flash), and the
    /// set of tabs with an unseen bell (a background tab belled).
    bell_flash_since: Option<Instant>,
    belled_tabs: std::collections::HashSet<TabId>,
    /// Native menu bar (macOS); inert elsewhere. Kept alive for the app's life.
    menu: ember_platform::AppMenu,
    /// Last cursor position in **logical** px.
    cursor: (f64, f64),
    /// Visible panes' inner rects (logical px), for mouse→cell hit-testing.
    pane_rects: Vec<(SessionId, Rect)>,
    /// Active text selection + the session (pane) it belongs to.
    sel: Option<(SessionId, Selection)>,
    /// Whether a mouse drag is currently extending the selection.
    selecting: bool,
    /// Last mouse-down (time, pane, cell), for double/triple-click detection.
    last_click: Option<(Instant, SessionId, u16, u16)>,
    /// Consecutive-click count at the same cell (1 = simple, 2 = word, 3 = line).
    click_count: u32,
    /// OS clipboard handle (lazily; `None` if the platform clipboard is unavailable).
    clipboard: Option<arboard::Clipboard>,
    /// Per-session bracketed-paste (DEC 2004) mode, updated from each frame delta —
    /// so paste can wrap in `ESC[200~`…`ESC[201~` only when the app asked for it.
    bracketed: HashMap<SessionId, bool>,
    /// In-progress tab drag-reorder: the tab being dragged, the press x (logical),
    /// and whether the drag threshold has been crossed (below it, it's a click).
    tab_drag: Option<TabDrag>,
    /// In-progress scrollbar-thumb drag: the session whose scrollbar is grabbed.
    scrollbar_drag: Option<SessionId>,
    /// In-progress divider drag to resize a split: `(a-side pane, split axis,
    /// last cursor position along that axis in logical px)`.
    divider_drag: Option<(PaneId, Axis, f64)>,
    /// The resize cursor currently shown (so we don't reset it every move).
    resize_cursor: Option<Axis>,
    /// Live Ctrl+Opt split drop-zone preview (hover), committed on click.
    split_preview: Option<SplitPreview>,
    /// Last tab-button mouse-down (time, tab index), for double-click-to-rename.
    last_tab_click: Option<(Instant, usize)>,
    /// Inline tab rename in progress: the tab index + the live edit buffer.
    editing_tab: Option<usize>,
    edit_buffer: String,
    /// The session last told it has focus (DEC 1004 focus reporting) — the
    /// backend only writes `CSI I`/`CSI O` when the app enabled mode 1004.
    focus_notified: Option<SessionId>,
    /// Fractional wheel-scroll carry (trackpad pixel deltas < one cell).
    wheel_accum: f32,
    /// A mouse press being forwarded to an app (session + button code) — its
    /// drag/release go to the same session even if the pointer leaves the pane.
    mouse_press: Option<(SessionId, u8)>,
    /// Last (col, row) a motion report was sent for (dedup per cell).
    last_mouse_cell: Option<(u16, u16)>,
    /// Sessions with an OSC 133 command in flight (Command/OutputStart seen, no
    /// CommandEnd) — used to confirm before destroying a busy pane.
    /// A destructive close awaiting confirmation (a busy pane).
    pending_close: Option<PendingClose>,
    /// The focused confirm button: 0 = Cancel (safe default), 1 = Close/Quit.
    confirm_focus: usize,
    /// Latest OSC title per session, so the window title can be re-asserted on
    /// tab/pane switch (not just when a fresh Title event happens to arrive).
    titles: std::collections::HashMap<SessionId, String>,
    /// Latest OSC 1337 `CurrentDir` per session — a new split spawned FROM a
    /// pane inherits its cwd (design §8.1). Not removed on exit; only read
    /// while spawning, and a dead `SessionId` is never reused.
    cwd_by_session: std::collections::HashMap<SessionId, String>,
    /// Wakes the event loop when a session publishes a frame; registered on
    /// every session's pixel lane so the loop can idle on `ControlFlow::Wait`.
    wake: std::sync::Arc<dyn Fn() + Send + Sync>,
    /// The window is fully hidden (another window covers it): suppress the
    /// ambient animation so an idle-but-covered window doesn't burn cycles.
    occluded: bool,
    /// The last render attempt found no drawable (transient startup shortage,
    /// display asleep). Drives a bounded retry cadence via the animation
    /// machinery below — the renderer's StarveGate caps actual frame prep at
    /// 4/s — so a missed frame repaints without the  spin. Cleared by
    /// the first present.
    render_starved: bool,
}

/// A close action deferred behind a running-process confirmation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingClose {
    /// Close the focused pane (Cmd+W).
    Pane,
    /// Close a whole tab by id (middle-click).
    Tab(TabId),
    /// Quit the whole app (Cmd+Q / window close).
    Quit,
}

/// State for an in-progress tab drag-reorder.
struct TabDrag {
    /// Index of the tab currently being dragged (updated as it live-reorders).
    tab: usize,
    /// Logical-x of the initial press (to measure the drag threshold).
    press_x: f64,
    /// Whether the pointer has moved far enough to count as a drag (vs. a click).
    active: bool,
}

/// How far (logical px) the pointer must move horizontally before a tab press
/// becomes a drag-reorder rather than a click.
const TAB_DRAG_THRESHOLD: f64 = 6.0;

/// Ctrl+Opt split drop-zone preview: the hovered pane + the split that a click
/// would commit (new pane on the right if `horizontal`, else the bottom).
struct SplitPreview {
    pane: PaneId,
    horizontal: bool,
    ratio: f32,
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
        if self.state.is_some() {
            return;
        }
        let w = DEFAULT_COLS as f32 * CELL_WIDTH + 2.0 * PAD;
        let h = DEFAULT_ROWS as f32 * CELL_HEIGHT + 2.0 * PAD;
        let attrs = ember_platform::window_attributes("Ember", w, h);
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        ember_platform::set_app_icon(&window, ICON_PNG);

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

        let mut state = RunState {
            renderer,
            tree,
            sessions: HashMap::new(),
            dims_cache: HashMap::new(),
            modifiers: ModifiersState::empty(),
            px,
            next_pane: 2,
            next_session: 2,
            next_tab: 2,
            control_rx: self.control_rx.take(),
            control_server: self.control_server.take(),
            help: false,
            about: false,
            about_since: Instant::now(),
            config,
            settings_open: false,
            settings_sel: 0,
            image_loaded: None,
            backdrop_since: Instant::now(),
            window_focused: true,
            fps_overlay: false,
            last_frame: None,
            fps_ema_ms: 0.0,
            render_ema_ms: 0.0,
            last_anim: Instant::now(),
            bell_flash_since: None,
            belled_tabs: std::collections::HashSet::new(),
            menu: ember_platform::build_menu(),
            cursor: (0.0, 0.0),
            pane_rects: Vec::new(),
            sel: None,
            selecting: false,
            last_click: None,
            click_count: 0,
            clipboard: arboard::Clipboard::new().ok(),
            bracketed: HashMap::new(),
            tab_drag: None,
            split_preview: None,
            scrollbar_drag: None,
            divider_drag: None,
            resize_cursor: None,
            last_tab_click: None,
            editing_tab: None,
            edit_buffer: String::new(),
            focus_notified: None,
            wheel_accum: 0.0,
            mouse_press: None,
            last_mouse_cell: None,
            pending_close: None,
            confirm_focus: 0,
            titles: std::collections::HashMap::new(),
            cwd_by_session: std::collections::HashMap::new(),
            wake: self.wake.clone(),
            occluded: false,
            render_starved: false,
        };
        if !state.spawn_session(session, GridDims::new(DEFAULT_COLS, DEFAULT_ROWS), None) {
            // No shell at startup means nothing to show; exit with the message
            // spawn_session already printed instead of presenting a dead window.
            std::process::exit(1);
        }
        state.sync_layout();
        state.apply_appearance();
        if state.config.developer_mode && state.control_server.is_none() {
            state.set_developer_mode(true);
        }
        // Paint once now: with ControlFlow::Wait the loop won't run again until
        // an event or a frame-lane wake, and the very first frame may have been
        // published before the waker was registered.
        state.renderer.window().request_redraw();
        self.state = Some(state);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => {
                if state.request_close(PendingClose::Quit) {
                    state.shutdown_all();
                    event_loop.exit();
                }
            }
            WindowEvent::Resized(size) => {
                state.px = (size.width.max(1), size.height.max(1));
                state.renderer.resize(state.px.0, state.px.1);
                state.sync_layout();
            }
            WindowEvent::Focused(focused) => {
                state.window_focused = focused;
                if focused {
                    // A focused window is never occluded. Focus events are the
                    // reliable reveal signal when an Occluded(false) got lost
                    // (e.g. around display sleep/unlock), so also clear the
                    // renderer's starve throttle before the repaint.
                    state.occluded = false;
                    state.renderer.surface_revealed();
                    state.renderer.window().request_redraw();
                }
            }
            WindowEvent::Occluded(occluded) => {
                state.occluded = occluded;
                if !occluded {
                    // Lift the renderer's starve throttle BEFORE requesting the
                    // reveal repaint, so it isn't swallowed by the holdoff.
                    state.renderer.surface_revealed();
                    state.renderer.window().request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(mods) => {
                state.modifiers = mods.state();
                // Releasing Ctrl+Opt hides the split drop-zone preview.
                if !state.split_modifier_held() {
                    state.clear_split_preview();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let sf = state.renderer.window().scale_factor();
                state.cursor = (position.x / sf, position.y / sf);
                // Ctrl+Opt held → live split drop-zone preview over the hovered pane.
                if state.split_modifier_held() {
                    state.update_split_preview();
                    return;
                }
                if let Some((target, axis, last)) = state.divider_drag {
                    let (x, y) = state.cursor;
                    let pos = if matches!(axis, Axis::Horizontal) {
                        x
                    } else {
                        y
                    };
                    state.resize_pane_px(target, axis, pos - last);
                    state.divider_drag = Some((target, axis, pos));
                } else if state.tab_drag.is_some() {
                    state.drag_tab_to(state.cursor.0);
                } else if let Some(sid) = state.scrollbar_drag.clone() {
                    state.scroll_to_at(&sid, state.cursor.1 as f32);
                } else if state.selecting {
                    let (x, y) = state.cursor;
                    state.extend_selection(x, y);
                } else {
                    let (x, y) = state.cursor;
                    // Tab strip: track hover (highlight + "✕"); motion over the
                    // strip is chrome, not pane motion, so stop here.
                    if state.update_tab_hover(x, y) {
                        return;
                    }
                    // Show a resize cursor over a divider; else forward motion to
                    // mouse-aware apps.
                    let over = state.divider_at(x, y).map(|(_, a)| a);
                    if over != state.resize_cursor {
                        state.resize_cursor = over;
                        state.renderer.window().set_cursor(match over {
                            Some(Axis::Horizontal) => CursorIcon::EwResize,
                            Some(Axis::Vertical) => CursorIcon::NsResize,
                            None => CursorIcon::Default,
                        });
                    }
                    if over.is_none() {
                        state.forward_mouse_motion();
                    }
                }
            }
            // Cursor left the window — drop any tab hover so the highlight/"✕"
            // don't linger.
            WindowEvent::CursorLeft { .. } => {
                state.renderer.set_hovered_tab(None);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // Notch wheels scroll WHEEL_LINES per notch; trackpads report
                // pixel-precise deltas that map 1:1 to cells (no multiplier).
                // Accumulate fractions so slow two-finger drags still move.
                let cells = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y * WHEEL_LINES as f32,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / CELL_HEIGHT,
                };
                state.wheel_accum += cells;
                let lines = state.wheel_accum.trunc() as i32;
                state.wheel_accum -= lines as f32;
                state.wheel_scroll(lines);
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button,
                ..
            } => match button {
                MouseButton::Left => {
                    let (x, y) = state.cursor;
                    // A blocking confirm modal captures the click: a button
                    // resolves it, elsewhere is a no-op (stays modal).
                    if state.pending_close.is_some() {
                        if let Some(idx) = state.renderer.confirm_button_at(x as f32, y as f32) {
                            if state.resolve_confirm(idx == 1) {
                                state.shutdown_all();
                                event_loop.exit();
                            }
                        }
                    } else if let Some((target, axis)) = state.divider_at(x, y) {
                        let pos = if matches!(axis, Axis::Horizontal) {
                            x
                        } else {
                            y
                        };
                        state.divider_drag = Some((target, axis, pos));
                    } else {
                        state.left_click();
                    }
                }
                // Middle-click on a tab closes it (standard gesture); elsewhere
                // it forwards to a mouse-aware app.
                MouseButton::Middle => {
                    let (x, y) = state.cursor;
                    if let Some(TabHit::Tab(i)) = state.renderer.tab_hit(x as f32, y as f32) {
                        if let Some(id) = state.tree.tabs.get(i).map(|t| t.id) {
                            if state.request_close(PendingClose::Tab(id)) {
                                state.shutdown_all();
                                event_loop.exit();
                            }
                        }
                    } else {
                        state.forward_mouse_press(1);
                    }
                }
                MouseButton::Right => {
                    state.forward_mouse_press(2);
                }
                _ => {}
            },
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button,
                ..
            } => {
                state.forward_mouse_release(match button {
                    MouseButton::Left => 0,
                    MouseButton::Middle => 1,
                    MouseButton::Right => 2,
                    _ => 0,
                });
                if button == MouseButton::Left {
                    state.tab_drag = None;
                    state.renderer.set_tab_drag(None);
                    state.selecting = false;
                    state.scrollbar_drag = None;
                    state.divider_drag = None;
                }
            }
            WindowEvent::KeyboardInput { event: key, .. } => {
                if key.state != ElementState::Pressed {
                    return;
                }
                // The Settings overlay is interactive — it handles its own keys
                // (arrows / space / esc) rather than dismissing on any key.
                if state.settings_open {
                    state.settings_key(&key.logical_key);
                    return;
                }
                // Inline tab rename captures typing, but NOT Cmd combos — those
                // stay app shortcuts (Cmd+W must not insert "w"), so fall through
                // to the Super branch below when Cmd is held.
                if state.editing_tab.is_some() && !state.modifiers.super_key() {
                    state.rename_key(&key.logical_key);
                    return;
                }
                // A running-process close confirmation (modal): Left/Right/Tab
                // move focus, Enter activates it, Esc cancels. Auto-repeat is
                // ignored so a held key can't confirm.
                if state.pending_close.is_some() {
                    if !key.repeat {
                        match &key.logical_key {
                            Key::Named(NamedKey::Escape) => {
                                state.resolve_confirm(false);
                            }
                            Key::Named(NamedKey::Enter) => {
                                let ok = state.confirm_focus == 1;
                                if state.resolve_confirm(ok) {
                                    state.shutdown_all();
                                    event_loop.exit();
                                }
                            }
                            Key::Named(
                                NamedKey::ArrowLeft | NamedKey::ArrowRight | NamedKey::Tab,
                            ) => {
                                state.confirm_focus ^= 1;
                                state.update_confirm_view();
                                state.renderer.window().request_redraw();
                            }
                            _ => {}
                        }
                    }
                    return;
                }
                // Cmd+Q — quit (with confirmation if a command is running). Handled
                // here so it exits regardless of tab count, unlike pane shortcuts.
                if state.modifiers.super_key()
                    && matches!(&key.logical_key, Key::Character(c) if c.eq_ignore_ascii_case("q"))
                {
                    if state.request_close(PendingClose::Quit) {
                        state.shutdown_all();
                        event_loop.exit();
                    }
                    return;
                }
                // While a modal overlay (help / About) is up, the next *fresh*
                // key dismisses it. Escape/Enter just dismiss; any other key
                // dismisses AND falls through so the keystroke still reaches the
                // shell (typing `ls` at the help screen shouldn't eat the `l`).
                // Auto-repeat is ignored so holding Cmd+/ can't close on open.
                if state.help || state.about {
                    if key.repeat {
                        return;
                    }
                    state.dismiss_overlay();
                    let swallow = matches!(
                        &key.logical_key,
                        Key::Named(NamedKey::Escape) | Key::Named(NamedKey::Enter)
                    ) || state.modifiers.super_key();
                    if swallow {
                        return;
                    }
                    // else: fall through and process this key normally.
                }
                let mods = state.modifiers;
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
                    if state.handle_shortcut(&key.logical_key, mods) && state.tree.tabs.is_empty() {
                        state.shutdown_all();
                        event_loop.exit();
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
                        state.scroll_focused(a);
                        return;
                    }
                }
                // DECCKM from the focused pane; Option-as-Meta strips the
                // macOS compose (Opt+b = "∫") back to the plain key for the
                // ESC prefix. With the option off, composing wins (é, ñ).
                let app_cursor = state.focused_app_cursor();
                let alt_meta = mods.alt_key() && state.config.option_as_meta;
                let logical = if alt_meta {
                    key.key_without_modifiers()
                } else {
                    key.logical_key.clone()
                };
                if let Some(bytes) = encode_key(&logical, mods, app_cursor, alt_meta) {
                    if let Some(h) = state.focused_session() {
                        let _ = h
                            .control
                            .send(BackendControl::Input(bytes.into_boxed_slice()));
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                state.drain_frames();
                // Frame-timing for the FPS overlay: cadence (interval between
                // redraws) + the render() call's own duration (the per-frame cost).
                let now = Instant::now();
                if let Some(last) = state.last_frame {
                    let dt = now.duration_since(last).as_secs_f32() * 1000.0;
                    state.fps_ema_ms = if state.fps_ema_ms == 0.0 {
                        dt
                    } else {
                        state.fps_ema_ms * 0.9 + dt * 0.1
                    };
                }
                state.last_frame = Some(now);
                if state.fps_overlay {
                    let fps = if state.fps_ema_ms > 0.0 {
                        1000.0 / state.fps_ema_ms
                    } else {
                        0.0
                    };
                    state.renderer.set_fps_overlay(Some(format!(
                        "{fps:.0} fps · {:.1} ms",
                        state.render_ema_ms
                    )));
                }
                let t = Instant::now();
                match state.renderer.render() {
                    // A drawable came through — the surface is ground truth, so
                    // whatever winit last said, we are visible.
                    RenderOutcome::Presented => {
                        state.occluded = false;
                        state.render_starved = false;
                    }
                    // Surface lost/outdated: it was reconfigured but this frame
                    // never presented — repaint now, not at the next input.
                    RenderOutcome::Retry => {
                        state.render_starved = false;
                        state.renderer.window().request_redraw();
                    }
                    // Starved (no drawable): do NOT re-request here — that loop
                    // is the  OOM spin — and do NOT latch state.occluded: a
                    // transient drawable shortage (startup burst) also lands
                    // here, and latching froze a fully VISIBLE window until the
                    // user clicked it. Durable occlusion state comes from winit's
                    // Occluded events; this flag makes about_to_wait retry on a
                    // bounded cadence instead.
                    RenderOutcome::Starved => state.render_starved = true,
                }
                let render_ms = t.elapsed().as_secs_f32() * 1000.0;
                state.render_ema_ms = if state.render_ema_ms == 0.0 {
                    render_ms
                } else {
                    state.render_ema_ms * 0.9 + render_ms * 0.1
                };
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        // Drain the semantic lanes: focused-pane title, and any exited shells.
        let focused = state.focused_session_id();
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
        for (id, handle) in &state.sessions {
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
            state.titles.insert(id, title);
        }
        // Drop titles for sessions that no longer exist.
        state.titles.retain(|id, _| state.sessions.contains_key(id));
        for (id, cwd) in cwd_updates {
            state.cwd_by_session.insert(id, cwd);
        }
        state
            .cwd_by_session
            .retain(|id, _| state.sessions.contains_key(id));
        if let Some(text) = clipboard_set {
            if let Some(cb) = state.clipboard.as_mut() {
                if let Err(e) = cb.set_text(text) {
                    eprintln!("[ember] OSC 52 clipboard copy failed: {e}");
                }
            }
        }
        if let Some(title) = new_title {
            state.renderer.window().set_title(&title);
        }
        for session in exited {
            state.close_session(&session);
        }
        for session in belled {
            state.on_bell(&session);
        }
        // Focus reporting (DEC 1004): tell sessions when their pane gains or
        // loses focus (pane switch, tab switch, window focus/blur).
        let focus_now = if state.window_focused { focused } else { None };
        if focus_now != state.focus_notified {
            if let Some(old) = state.focus_notified.take() {
                if let Some(h) = state.sessions.get(&old) {
                    let _ = h.control.send(BackendControl::Focus(false));
                }
            }
            if let Some(new) = &focus_now {
                if let Some(h) = state.sessions.get(new) {
                    let _ = h.control.send(BackendControl::Focus(true));
                }
            }
            // Re-assert the newly focused pane's title (or the app name if it
            // hasn't set one) so a tab/pane switch never leaves a stale title.
            let title = focus_now
                .as_ref()
                .and_then(|id| state.titles.get(id).cloned())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| "Ember".to_string());
            state.renderer.window().set_title(&title);
            state.focus_notified = focus_now;
        }
        // Drain debug-control commands (EMBER_CONTROL) and act on them.
        let cmds: Vec<ControlMsg> = state
            .control_rx
            .as_ref()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default();
        for cmd in cmds {
            state.handle_control(cmd, event_loop);
        }
        // Native menu items (macOS) → semantic actions.
        if let Some(action) = ember_platform::menu_action(&state.menu) {
            match action {
                MenuAction::ShowShortcuts => state.toggle_help(),
                MenuAction::About => state.toggle_about(),
                MenuAction::Settings => state.toggle_settings(),
                MenuAction::NewTab => state.new_tab(),
                MenuAction::Copy => state.copy_selection(),
                MenuAction::Paste => state.paste_clipboard(),
                MenuAction::Close | MenuAction::Quit => {
                    let kind = if matches!(action, MenuAction::Quit) {
                        PendingClose::Quit
                    } else {
                        PendingClose::Pane
                    };
                    if state.request_close(kind) {
                        state.shutdown_all();
                        event_loop.exit();
                    }
                }
            }
        }
        if state.tree.tabs.is_empty() {
            state.shutdown_all();
            event_loop.exit();
            return;
        }
        // Poll the pixel lanes; redraw only when something changed — and only
        // when someone can see it. While occluded, content-driven redraws would
        // re-enter frame prep at PTY rate for frames that can never present
        //; the grids still update here, and Occluded(false) repaints.
        if state.drain_frames() && !state.occluded {
            state.renderer.window().request_redraw();
        }
        // Pace animations by WALL-CLOCK elapsed since the last frame, checked here on
        // *every* wake (timer tick OR any event). Advancing off the timer's
        // `ResumeTimeReached` alone is fragile: a stream of mouse-move/resize events
        // keeps resetting the `WaitUntil` deadline so the tick never fires and the
        // sparks freeze until the mouse stops (the stutter). We only request a redraw
        // once a frame-interval has actually elapsed, so this doesn't spin either.
        let now = Instant::now();
        // A starved (no-drawable) render retries on the animation cadence while
        // the window isn't winit-occluded: the renderer's StarveGate turns most
        // ticks into instant no-ops and allows a real attempt only every 250ms,
        // so a transiently starved frame self-heals without the  spin.
        let starve_retry = state.render_starved && !state.occluded;
        let frame = if state.about || state.fps_overlay || state.bell_flashing() {
            ANIM_FRAME
        } else if state.backdrop_animating() {
            state.ember_frame()
        } else {
            ANIM_FRAME // starve retry (only reached when `animating` below)
        };
        let animating = state.about
            || state.fps_overlay
            || state.bell_flashing()
            || state.backdrop_animating()
            || starve_retry;
        if animating {
            if now.duration_since(state.last_anim) >= frame {
                state.last_anim = now;
                state.advance_animations(now);
                // Animations advance on wall-clock regardless, but don't ask an
                // occluded window to paint them (same  spin, slower burn).
                if !state.occluded {
                    state.renderer.window().request_redraw();
                }
            }
            // Fixed deadline relative to the last frame (not `now`), so incoming
            // events can't push it back indefinitely.
            event_loop.set_control_flow(ControlFlow::WaitUntil(state.last_anim + frame));
        } else {
            // Nothing animating: sleep until an event, a frame-lane wake, or a
            // control command wakes us — no more ~125 Hz idle polling.
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }
}

impl RunState {
    /// The **logical**-pixel rect available to the layout (full surface minus the
    /// tab strip). `px` is physical; the renderer draws in logical units and scales
    /// to physical by the HiDPI factor, so layout/dims must be logical too — else a
    /// Retina shell gets 2× the columns it can show.
    fn viewport(&self) -> Rect {
        let sf = self.renderer.window().scale_factor();
        let chrome = Renderer::chrome_height() as f64;
        let w = self.px.0 as f64 / sf;
        let h = self.px.1 as f64 / sf;
        Rect::new(0.0, chrome, w.max(1.0), (h - chrome).max(1.0))
    }

    fn active_tab(&self) -> &Tab {
        &self.tree.tabs[self.tree.active]
    }

    fn focused_session_id(&self) -> Option<SessionId> {
        if self.tree.tabs.is_empty() {
            return None;
        }
        let tab = self.active_tab();
        tab.root.session_of(tab.focus).cloned()
    }

    fn focused_session(&self) -> Option<&BackendHandle> {
        self.focused_session_id()
            .and_then(|id| self.sessions.get(&id))
    }

    /// Scroll the focused pane's scrollback by `amount`. No-op on the alternate
    /// screen (the projection gates it).
    fn scroll_focused(&self, amount: ScrollAmount) {
        if let Some(h) = self.focused_session() {
            let _ = h.control.send(BackendControl::Scroll(amount));
        }
    }

    /// Jump the focused pane to the previous (`-1`) / next (`+1`) OSC 133 prompt.
    fn jump_prompt(&self, dir: i8) {
        if let Some(h) = self.focused_session() {
            let _ = h.control.send(BackendControl::JumpMark(dir));
        }
    }

    /// Handle a mouse-wheel notch worth `lines` (positive = up, into history). On
    /// the primary screen this scrolls history; in a full-screen app (alt screen)
    /// with no mouse reporting it translates to arrow keys so `less`/`man`/`vim`
    /// still page; with mouse reporting on we leave it alone (that path is a future
    /// mouse-forwarding feature).
    fn wheel_scroll(&self, lines: i32) {
        if lines == 0 {
            return;
        }
        // Scroll the pane under the pointer (every mainstream terminal), not
        // the focused one; fall back to focused when hovering the chrome.
        let Some(id) = self
            .session_under_cursor()
            .or_else(|| self.focused_session_id())
        else {
            return;
        };
        let m = self.renderer.pane_modes(&id);
        let (alt, mouse) = (m.alt_screen, m.mouse_reporting);
        // Mouse-aware app: the wheel is button 64 (up) / 65 (down), one report
        // per line. Shift keeps the wheel local (scrollback), same as clicks.
        if mouse && !self.modifiers.shift_key() {
            let (x, y) = self.cursor;
            if let Some((_, rect)) = self
                .pane_rects
                .iter()
                .find(|(s, r)| {
                    *s == id && x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
                })
                .cloned()
            {
                let (cw, ch) = self.renderer.cell_size();
                let col = ((x - rect.x) / cw as f64).max(0.0) as u16;
                let row = ((y - rect.y) / ch as f64).max(0.0) as u16;
                let btn = if lines > 0 { 64 } else { 65 } + self.mouse_mod_bits();
                let mut bytes = Vec::new();
                for _ in 0..lines.abs() {
                    bytes.extend(Self::mouse_report_bytes(m.mouse.sgr, btn, col, row, true));
                }
                if let Some(h) = self.sessions.get(&id) {
                    let _ = h
                        .control
                        .send(BackendControl::Input(bytes.into_boxed_slice()));
                }
                return;
            }
        }
        let Some(h) = self.sessions.get(&id) else {
            return;
        };
        if alt {
            if mouse {
                return;
            }
            // Alternate-scroll: wheel → Up/Down arrows (CSI form).
            let (seq, count): (&[u8], i32) = if lines > 0 {
                (b"\x1b[A", lines)
            } else {
                (b"\x1b[B", -lines)
            };
            let mut bytes = Vec::with_capacity(seq.len() * count as usize);
            for _ in 0..count {
                bytes.extend_from_slice(seq);
            }
            let _ = h
                .control
                .send(BackendControl::Input(bytes.into_boxed_slice()));
        } else {
            let _ = h
                .control
                .send(BackendControl::Scroll(ScrollAmount::Lines(lines)));
        }
    }

    /// Spawn a shell-backed session and register its grid with the renderer.
    /// On failure (stale `$SHELL`, fd exhaustion) the app must keep running:
    /// report, flash the bell, and let the caller abort its layout change.
    /// `cwd`: the directory to start in (design §8.1 — a new split inherits
    /// the parent pane's OSC 1337 `CurrentDir`); `None` starts at the shell's
    /// own default (a fresh tab, or the very first pane).
    fn spawn_session(&mut self, id: SessionId, dims: GridDims, cwd: Option<String>) -> bool {
        let mut cfg = LocalPtyConfig::new(id.clone(), dims);
        cfg.shell_integration = self.config.shell_integration;
        cfg.cwd = cwd.map(std::path::PathBuf::from);
        let handle = match LocalPty::spawn(cfg) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("ember: failed to spawn shell: {e}");
                self.bell_flash_since = Some(Instant::now());
                self.renderer.set_bell_flash(bell_flash_intensity(0.0));
                return false;
            }
        };
        handle.frames.set_waker(self.wake.clone());
        self.renderer.ensure_pane(&id, dims);
        self.dims_cache.insert(id.clone(), dims);
        self.sessions.insert(id, handle);
        true
    }

    /// Tear down a session backend and forget its render/cache state.
    fn kill_session(&mut self, id: &SessionId) {
        if let Some(h) = self.sessions.remove(id) {
            let _ = h.control.send(BackendControl::Shutdown);
        }
        self.renderer.remove_pane(id);
        self.dims_cache.remove(id);
    }

    fn shutdown_all(&mut self) {
        for (_, h) in self.sessions.drain() {
            let _ = h.control.send(BackendControl::Shutdown);
        }
    }

    /// Send raw bytes to the focused session's PTY (used by control + key paths).
    fn send_to_focused(&self, bytes: Vec<u8>) {
        if let Some(h) = self.focused_session() {
            let _ = h
                .control
                .send(BackendControl::Input(bytes.into_boxed_slice()));
        }
    }

    /// Act on a debug-control command (see `control`): inject text/keys, run a
    /// chord, or reply with a JSON state dump.
    fn handle_control(&mut self, msg: ControlMsg, event_loop: &ActiveEventLoop) {
        // Route injected keys into the interactive Settings overlay (for tests),
        // mirroring the real keyboard path.
        if self.settings_open {
            if let ControlMsg::Key(name) = &msg {
                if let Some(k) = named_key(name) {
                    self.settings_key(&k);
                }
                return;
            }
        }
        // Mirror the keyboard: the close-confirm modal captures input (arrows/Tab
        // move focus, Enter activates, Esc cancels).
        if self.pending_close.is_some() {
            if let ControlMsg::Key(name) = &msg {
                match name.as_str() {
                    "Escape" => {
                        self.resolve_confirm(false);
                    }
                    "Enter" | "Return" => {
                        let ok = self.confirm_focus == 1;
                        if self.resolve_confirm(ok) {
                            self.shutdown_all();
                            event_loop.exit();
                        }
                    }
                    "ArrowLeft" | "ArrowRight" | "Tab" => {
                        self.confirm_focus ^= 1;
                        self.update_confirm_view();
                        self.renderer.window().request_redraw();
                    }
                    _ => {}
                }
            }
            return;
        }
        // Mirror the keyboard: while a modal overlay is up, any input dismisses it
        // (but state/screenshot still work, so the overlay can be inspected).
        if self.help || self.about {
            if let ControlMsg::Type(_) | ControlMsg::Key(_) | ControlMsg::Chord(_) = &msg {
                self.dismiss_overlay();
                return;
            }
        }
        match msg {
            ControlMsg::Type(text) => self.send_to_focused(text.into_bytes()),
            ControlMsg::Key(name) => {
                if let Some(key) = named_key(&name) {
                    let app_cursor = self.focused_app_cursor();
                    if let Some(bytes) =
                        encode_key(&key, ModifiersState::empty(), app_cursor, false)
                    {
                        self.send_to_focused(bytes);
                    }
                }
            }
            ControlMsg::Chord(combo) => {
                if let Some((key, mods)) = parse_chord(&combo) {
                    if mods.super_key() {
                        if self.handle_shortcut(&key, mods) && self.tree.tabs.is_empty() {
                            self.shutdown_all();
                            event_loop.exit();
                        }
                    } else if let Some(bytes) =
                        encode_key(&key, mods, self.focused_app_cursor(), false)
                    {
                        self.send_to_focused(bytes);
                    }
                }
            }
            ControlMsg::State(reply) => {
                let _ = reply.send(self.state_json());
            }
            ControlMsg::Screenshot(path, reply) => {
                let resp = match self.renderer.capture_to_png(std::path::Path::new(&path)) {
                    Ok(()) => serde_json::json!({"ok": true, "path": path}).to_string(),
                    Err(e) => serde_json::json!({"ok": false, "error": e.to_string()}).to_string(),
                };
                let _ = reply.send(resp);
            }
            ControlMsg::Click(x, y) => {
                self.cursor = (x, y);
                self.left_click();
            }
            ControlMsg::About => self.toggle_about(),
            ControlMsg::Settings => self.toggle_settings(),
            ControlMsg::Select(r1, c1, r2, c2, mode) => {
                let Some(sid) = self.focused_session_id() else {
                    return;
                };
                let mode = match mode.as_str() {
                    "word" => SelectionMode::Word,
                    "line" => SelectionMode::Line,
                    _ => SelectionMode::Simple,
                };
                let mut s = Selection::new(Point::new(r1, c1), mode);
                s.update(Point::new(r2, c2));
                self.sel = Some((sid, s));
                self.renderer.set_selection(self.sel.clone());
            }
            ControlMsg::Copy => self.copy_selection(),
            ControlMsg::Paste(text) => self.paste_into_focused(&text),
            ControlMsg::Fps => self.toggle_fps(),
            ControlMsg::Scroll(amount) => self.scroll_focused(amount),
            ControlMsg::Bell(tab) => {
                // `Some(i)` = a specific tab's first session; `None` = focused pane.
                let session = match tab {
                    Some(i) => self
                        .tree
                        .tabs
                        .get(i)
                        .and_then(|t| t.root.leaves().into_iter().next().map(|(_, s)| s)),
                    None => self.focused_session_id(),
                };
                if let Some(s) = session {
                    self.on_bell(&s);
                }
            }
            ControlMsg::ReorderTab(from, to) => {
                let vp = self.viewport();
                apply(&mut self.tree, LayoutCommand::MoveTab { from, to }, vp);
                self.sync_layout();
            }
            ControlMsg::RenameTab(i, name) => {
                if let Some(t) = self.tree.tabs.get(i) {
                    let id = t.id;
                    let vp = self.viewport();
                    apply(
                        &mut self.tree,
                        LayoutCommand::RenameTab {
                            tab: id,
                            title: name,
                        },
                        vp,
                    );
                    self.sync_layout();
                }
            }
            ControlMsg::EditTab(i) => self.start_rename(i),
        }
    }

    /// A JSON snapshot of the live app for the debug control surface: scale,
    /// surface size, tabs, and the active tab's panes (dims/cursor/styles/text).
    fn state_json(&self) -> String {
        let sf = self.renderer.window().scale_factor();
        let tab = self.active_tab();
        let focus = tab.focus;
        let panes: Vec<serde_json::Value> = tab
            .root
            .leaves()
            .iter()
            .map(|(pane, sess)| {
                let snap = self.renderer.pane_snapshot(sess);
                serde_json::json!({
                    "session": sess.0,
                    "pane": pane.0,
                    "focused": *pane == focus,
                    "dims": snap.as_ref().map(|s| serde_json::json!([s.cols, s.rows])),
                    "cursor": snap.as_ref().map(|s| serde_json::json!({
                        "row": s.cursor_row, "col": s.cursor_col, "visible": s.cursor_visible,
                    })),
                    "styles_known": snap.as_ref().map(|s| s.styles_known),
                    "text": snap.as_ref().map(|s| s.text.clone()),
                })
            })
            .collect();
        let bracketed = self
            .focused_session_id()
            .and_then(|id| self.bracketed.get(&id).copied())
            .unwrap_or(false);
        serde_json::json!({
            "scale_factor": sf,
            "surface": [self.px.0, self.px.1],
            "tabs": self.tree.tabs.len(),
            "active_tab": self.tree.active,
            "focus_pane": focus.0,
            "bracketed_paste": bracketed,
            "panes": panes,
        })
        .to_string()
    }

    /// Run the `KillSession` side effects of an applied command (the layout tree is
    /// already mutated; spawns/resizes are handled by the caller + `sync_layout`).
    fn apply_effects(&mut self, effects: Vec<LayoutEffect>) {
        for effect in effects {
            if let LayoutEffect::KillSession(id) = effect {
                self.kill_session(&id);
            }
        }
    }

    /// Recompute the active tab's tiling, hand it to the renderer, and resize each
    /// session's PTY whose grid dims changed. Idempotent; the single source of
    /// truth for "what's on screen and how big each shell is."
    fn sync_layout(&mut self) {
        if self.tree.tabs.is_empty() {
            return;
        }
        let vp = self.viewport();
        let (cw, ch) = self.renderer.cell_size();
        let tab = self.active_tab();
        let focus_pane = tab.focus;
        let sessions: HashMap<PaneId, SessionId> = tab.root.leaves().into_iter().collect();
        let rects = layout(&tab.root, vp);

        let mut visible = Vec::with_capacity(rects.len());
        let mut focused_session: Option<SessionId> = None;
        let mut resizes: Vec<(SessionId, GridDims)> = Vec::new();
        for (pane, outer) in rects {
            let Some(session) = sessions.get(&pane).cloned() else {
                continue;
            };
            let inner = inset(outer, PAD as f64);
            let dims = dims_for_rect(inner, cw, ch);
            resizes.push((session.clone(), dims));
            if pane == focus_pane {
                focused_session = Some(session.clone());
            }
            visible.push(VisiblePane {
                session,
                rect: inner,
            });
        }

        for (session, dims) in resizes {
            if self.dims_cache.get(&session) != Some(&dims) {
                if let Some(h) = self.sessions.get(&session) {
                    let _ = h.control.send(BackendControl::Resize(dims));
                }
                self.dims_cache.insert(session, dims);
            }
        }

        let active = self.tree.active;
        let editing_tab = self.editing_tab;
        // Becoming the active tab clears its unseen-bell indicator.
        if let Some(id) = self.tree.tabs.get(active).map(|t| t.id) {
            self.belled_tabs.remove(&id);
        }
        let belled = &self.belled_tabs;
        let tabs: Vec<TabLabel> = self
            .tree
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let editing = editing_tab == Some(i);
                TabLabel {
                    title: if editing {
                        self.edit_buffer.clone()
                    } else if t.title.is_empty() {
                        format!("{}", i + 1)
                    } else {
                        t.title.clone()
                    },
                    active: i == active,
                    editing,
                    bell: belled.contains(&t.id),
                }
            })
            .collect();

        let focused = focused_session.unwrap_or_else(|| visible[0].session.clone());
        self.pane_rects = visible
            .iter()
            .map(|vp| (vp.session.clone(), vp.rect))
            .collect();
        self.renderer.set_visible(visible, focused, tabs);
        self.renderer.window().request_redraw();
    }

    /// Poll every session's pixel lane into its grid. Returns whether anything
    /// changed (background tabs stay current so they're right when re-shown).
    fn drain_frames(&mut self) -> bool {
        let mut dirty = false;
        for (id, handle) in &self.sessions {
            while let Some(delta) = handle.frames.take() {
                self.bracketed.insert(id.clone(), delta.bracketed_paste);
                self.renderer.apply_delta(id, delta);
                dirty = true;
            }
        }
        dirty
    }

    /// Send `text` to the focused pane as a paste: when that session enabled
    /// bracketed paste, wrap it in `ESC[200~`…`ESC[201~` (stripping any embedded
    /// markers first — see [`bracket_paste`]); otherwise send it raw.
    fn paste_into_focused(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let bracketed = self
            .focused_session_id()
            .and_then(|id| self.bracketed.get(&id).copied())
            .unwrap_or(false);
        self.send_to_focused(bracket_paste(text, bracketed));
    }

    /// Handle a Super-modified key as a multiplexer command. Returns whether it was
    /// a recognized shortcut (so the caller can check for an emptied tree → quit).
    fn handle_shortcut(&mut self, key: &Key, mods: ModifiersState) -> bool {
        match key {
            // Cmd+/ — show the cheat-sheet overlay (any key dismisses). macOS
            // reserves Cmd+? (Cmd+Shift+/) for the system Help menu and never
            // delivers it, so Cmd+/ is the real binding; "?" is accepted too in
            // case a layout delivers it.
            Key::Character(s) if s.as_str() == "/" || s.as_str() == "?" => {
                self.toggle_help();
                true
            }
            // Cmd+, — Settings (the macOS Preferences convention; also a menu item).
            Key::Character(s) if s.as_str() == "," => {
                self.toggle_settings();
                true
            }
            // Cmd+[ / Cmd+] — jump to previous / next command prompt (OSC 133).
            Key::Character(s) if s.as_str() == "[" => {
                self.jump_prompt(-1);
                true
            }
            Key::Character(s) if s.as_str() == "]" => {
                self.jump_prompt(1);
                true
            }
            // Cmd+Shift+P — toggle the FPS / frame-time debug overlay.
            Key::Character(s) if s.eq_ignore_ascii_case("p") && mods.shift_key() => {
                self.toggle_fps();
                true
            }
            // Cmd+C — copy the current selection (macOS clipboard convention;
            // Ctrl+C remains SIGINT to the shell). Cmd+V — paste.
            Key::Character(s) if s.eq_ignore_ascii_case("c") => {
                self.copy_selection();
                true
            }
            Key::Character(s) if s.eq_ignore_ascii_case("v") => {
                self.paste_clipboard();
                true
            }
            // Cmd+D / Cmd+Shift+D — split the focused pane side-by-side / stacked.
            Key::Character(s) if s.eq_ignore_ascii_case("d") => {
                let axis = if mods.shift_key() {
                    Axis::Vertical
                } else {
                    Axis::Horizontal
                };
                self.split_focused(axis);
                true
            }
            // Cmd+W — close the focused pane (and its tab if it was the last),
            // confirming first if it's running a command. The caller's
            // tabs-empty check still handles quit-on-last-pane for the
            // no-confirm path; a deferred confirm leaves tabs intact.
            Key::Character(s) if s.eq_ignore_ascii_case("w") => {
                self.request_close(PendingClose::Pane);
                true
            }
            // Cmd+T — open a new tab with a fresh shell.
            Key::Character(s) if s.eq_ignore_ascii_case("t") => {
                self.new_tab();
                true
            }
            // Cmd+0 — reset the font size to the config baseline.
            Key::Character(s) if s.as_str() == "0" => {
                self.zoom_to(self.config.font.size);
                true
            }
            // Cmd+= / Cmd++ — zoom in; Cmd+- / Cmd+_ — zoom out (1pt steps).
            Key::Character(s) if s.as_str() == "=" || s.as_str() == "+" => {
                self.zoom_by(1.0);
                true
            }
            Key::Character(s) if s.as_str() == "-" || s.as_str() == "_" => {
                self.zoom_by(-1.0);
                true
            }
            // Cmd+1..9 — jump straight to a tab (Option/Alt is awkward on macOS, so
            // tab + pane navigation avoid it entirely).
            Key::Character(s) if s.chars().all(|c| c.is_ascii_digit()) && !s.is_empty() => {
                if let Some(n) = s.chars().next().and_then(|c| c.to_digit(10)) {
                    self.select_tab(n as usize);
                }
                true
            }
            // Cmd+Shift+Arrows — cycle the active tab. (Checked before the plain
            // arrows so Shift wins.)
            Key::Named(NamedKey::ArrowRight) if mods.shift_key() => self.cycle_tab(1),
            Key::Named(NamedKey::ArrowLeft) if mods.shift_key() => self.cycle_tab(-1),
            // Cmd+Ctrl+Arrows — resize the focused pane (grow toward the arrow).
            Key::Named(NamedKey::ArrowRight) if mods.control_key() => {
                self.resize_focused(Axis::Horizontal, 1.0)
            }
            Key::Named(NamedKey::ArrowLeft) if mods.control_key() => {
                self.resize_focused(Axis::Horizontal, -1.0)
            }
            Key::Named(NamedKey::ArrowDown) if mods.control_key() => {
                self.resize_focused(Axis::Vertical, 1.0)
            }
            Key::Named(NamedKey::ArrowUp) if mods.control_key() => {
                self.resize_focused(Axis::Vertical, -1.0)
            }
            // Cmd+Arrows — move focus geometrically between panes.
            Key::Named(NamedKey::ArrowLeft) => self.focus_dir(Direction::Left),
            Key::Named(NamedKey::ArrowRight) => self.focus_dir(Direction::Right),
            Key::Named(NamedKey::ArrowUp) => self.focus_dir(Direction::Up),
            Key::Named(NamedKey::ArrowDown) => self.focus_dir(Direction::Down),
            _ => false,
        }
    }

    fn split_focused(&mut self, axis: Axis) {
        self.split_pane(self.active_tab().focus, axis, 0.5);
    }

    /// Split `target` on `axis` at `ratio` (existing pane's fraction), spawning a
    /// fresh shell in the new pane (right/bottom). Shared by Cmd+D + the visual split.
    /// Minimum pane extent (px) along `axis`, from a floor of cells + padding —
    /// the value core clamps splits/resizes against (metrics live app-side).
    fn min_px(&self, axis: Axis) -> f64 {
        const MIN_COLS: f32 = 8.0;
        const MIN_ROWS: f32 = 3.0;
        let (cw, ch) = self.renderer.cell_size();
        let px = match axis {
            Axis::Horizontal => MIN_COLS * cw + 2.0 * PAD,
            Axis::Vertical => MIN_ROWS * ch + 2.0 * PAD,
        };
        px as f64
    }

    fn split_pane(&mut self, target: PaneId, axis: Axis, ratio: f64) {
        let new_pane = PaneId(self.next_pane);
        let new_session = SessionId::new(format!("s{}", self.next_session));
        // Cwd-inheriting split (design §8.1): the new pane starts where the
        // split's parent pane last reported itself (OSC 1337 `CurrentDir`).
        let inherited_cwd = self
            .active_tab()
            .root
            .session_of(target)
            .and_then(|sid| self.cwd_by_session.get(sid))
            .cloned();
        let vp = self.viewport();
        let min_px = self.min_px(axis);
        // Spawn only if the split is actually accepted (min-size may refuse it),
        // so a refused split never leaks a shell. Probe by applying first, then
        // spawn on success — apply is pure and the session isn't wired yet.
        let effects = apply(
            &mut self.tree,
            LayoutCommand::SplitPane {
                target,
                axis,
                ratio,
                new_pane,
                new_session: new_session.clone(),
                min_px,
            },
            vp,
        );
        if effects.is_empty() {
            return; // refused (pane too small) — nothing spawned, nothing to undo
        }
        self.next_pane += 1;
        self.next_session += 1;
        if !self.spawn_session(
            new_session,
            GridDims::new(DEFAULT_COLS, DEFAULT_ROWS),
            inherited_cwd,
        ) {
            // Spawn failed after the tree accepted the split: roll the pane back
            // out so we don't render a dead pane.
            let vp = self.viewport();
            let rollback = apply(
                &mut self.tree,
                LayoutCommand::ClosePane { target: new_pane },
                vp,
            );
            self.apply_effects(rollback);
            self.sync_layout();
            return;
        }
        self.apply_effects(effects);
        self.sync_layout();
    }

    /// Whether Ctrl+Opt is currently held (the visual-split modifier).
    fn split_modifier_held(&self) -> bool {
        self.modifiers.control_key() && self.modifiers.alt_key()
    }

    /// DECCKM state of the focused pane (drives arrow/Home/End encoding).
    fn focused_app_cursor(&self) -> bool {
        self.focused_session_id()
            .map(|id| self.renderer.pane_modes(&id).app_cursor)
            .unwrap_or(false)
    }

    /// The pane under the mouse cursor, if any.
    fn session_under_cursor(&self) -> Option<SessionId> {
        let (x, y) = self.cursor;
        self.pane_rects
            .iter()
            .find(|(_, r)| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height)
            .map(|(s, _)| s.clone())
    }

    /// Recompute the Ctrl+Opt split drop-zone preview from the cursor over a pane:
    /// nearer the right edge → side-by-side (new pane right), nearer the bottom →
    /// stacked (new pane below); the divider follows the cursor for the ratio.
    fn update_split_preview(&mut self) {
        let (x, y) = self.cursor;
        let hit = self
            .pane_rects
            .iter()
            .find(|(_, r)| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height)
            .map(|(s, r)| (s.clone(), *r));
        let Some((sid, rect)) = hit else {
            self.clear_split_preview();
            return;
        };
        let fx = ((x - rect.x) / rect.width).clamp(0.0, 1.0) as f32;
        let fy = ((y - rect.y) / rect.height).clamp(0.0, 1.0) as f32;
        let horizontal = fx >= fy; // closer to the right edge than the bottom
        let ratio = if horizontal { fx } else { fy }.clamp(0.1, 0.9);
        let pane = self
            .active_tab()
            .root
            .leaves()
            .into_iter()
            .find(|(_, s)| *s == sid)
            .map(|(p, _)| p);
        let Some(pane) = pane else {
            self.clear_split_preview();
            return;
        };
        self.renderer
            .set_split_preview(Some((sid, horizontal, ratio)));
        self.split_preview = Some(SplitPreview {
            pane,
            horizontal,
            ratio,
        });
    }

    /// Clear the split preview (modifier released / cursor left the panes).
    fn clear_split_preview(&mut self) {
        if self.split_preview.take().is_some() {
            self.renderer.set_split_preview(None);
        }
    }

    fn close_focused(&mut self) {
        let target = self.active_tab().focus;
        let vp = self.viewport();
        let effects = apply(&mut self.tree, LayoutCommand::ClosePane { target }, vp);
        self.apply_effects(effects);
        if !self.tree.tabs.is_empty() {
            self.sync_layout();
        }
    }

    fn new_tab(&mut self) {
        let id = TabId(self.next_tab);
        self.next_tab += 1;
        let pane = PaneId(self.next_pane);
        self.next_pane += 1;
        let session = SessionId::new(format!("s{}", self.next_session));
        self.next_session += 1;
        // Design §8.1 scopes cwd inheritance to splits, not new tabs — a new
        // tab starts at the shell's own default, same as today.
        if !self.spawn_session(
            session.clone(),
            GridDims::new(DEFAULT_COLS, DEFAULT_ROWS),
            None,
        ) {
            return;
        }
        let vp = self.viewport();
        let effects = apply(
            &mut self.tree,
            LayoutCommand::NewTab { id, session, pane },
            vp,
        );
        self.apply_effects(effects);
        self.sync_layout();
    }

    /// Keyboard resize of the focused pane: `dir` (±1) grows/shrinks it by a few
    /// cells along `axis` (key-repeat makes it fast). Core takes a px delta.
    fn resize_focused(&mut self, axis: Axis, dir: f64) -> bool {
        let (cw, ch) = self.renderer.cell_size();
        let step = 3.0
            * if matches!(axis, Axis::Horizontal) {
                cw
            } else {
                ch
            } as f64;
        let target = self.active_tab().focus;
        self.resize_pane_px(target, axis, dir * step);
        true
    }

    /// Resize the split enclosing `target` along `axis` by `delta` px. Shared by
    /// keyboard resize and mouse divider drag. Core clamps against `min_px`.
    fn resize_pane_px(&mut self, target: PaneId, axis: Axis, delta: f64) {
        let vp = self.viewport();
        let min_px = self.min_px(axis);
        apply(
            &mut self.tree,
            LayoutCommand::ResizePane {
                target,
                axis,
                delta,
                min_px,
            },
            vp,
        );
        self.sync_layout();
    }

    /// The split divider under logical `(x, y)`, as `(a-side pane, axis)`, when
    /// the cursor is in the gap between two adjacent panes. `None` otherwise.
    fn divider_at(&self, x: f64, y: f64) -> Option<(PaneId, Axis)> {
        let leaves: HashMap<SessionId, PaneId> = self
            .active_tab()
            .root
            .leaves()
            .into_iter()
            .map(|(p, s)| (s, p))
            .collect();
        let grab = PAD as f64 + 3.0; // gap half-width + a little slop
        let gap = 2.0 * PAD as f64; // inner-rect gap between adjacent panes
        for (sid, r) in &self.pane_rects {
            let right = r.x + r.width;
            let bottom = r.y + r.height;
            // Vertical divider on this pane's right edge (a neighbor abuts it).
            if (x - right).abs() <= grab
                && y >= r.y
                && y < r.y + r.height
                && self.pane_rects.iter().any(|(_, o)| {
                    (o.x - (right + gap)).abs() <= grab && y >= o.y && y < o.y + o.height
                })
            {
                if let Some(&p) = leaves.get(sid) {
                    return Some((p, Axis::Horizontal));
                }
            }
            // Horizontal divider on this pane's bottom edge.
            if (y - bottom).abs() <= grab
                && x >= r.x
                && x < r.x + r.width
                && self.pane_rects.iter().any(|(_, o)| {
                    (o.y - (bottom + gap)).abs() <= grab && x >= o.x && x < o.x + o.width
                })
            {
                if let Some(&p) = leaves.get(sid) {
                    return Some((p, Axis::Vertical));
                }
            }
        }
        None
    }

    fn focus_dir(&mut self, dir: Direction) -> bool {
        let vp = self.viewport();
        let effects = apply(&mut self.tree, LayoutCommand::FocusDir { dir }, vp);
        self.apply_effects(effects);
        self.sync_layout();
        true
    }

    fn cycle_tab(&mut self, delta: isize) -> bool {
        let n = self.tree.tabs.len();
        if n > 1 {
            let cur = self.tree.active as isize;
            self.tree.active = (cur + delta).rem_euclid(n as isize) as usize;
            self.sync_layout();
        }
        true
    }

    /// Show the keyboard cheat-sheet overlay (closing other overlays — exclusive).
    fn show_help(&mut self) {
        self.hide_about();
        self.hide_settings();
        self.help = true;
        self.renderer.set_help(Some(help_lines()));
    }

    /// Track which tab the cursor is over, driving the hover highlight + "✕"
    /// close affordance. Returns `true` when the cursor is over the tab strip, so
    /// the caller treats the motion as chrome (not pane) input. Also clears a
    /// stale resize cursor when moving off a divider onto the strip.
    fn update_tab_hover(&mut self, x: f64, y: f64) -> bool {
        let hit = self.renderer.tab_hit(x as f32, y as f32);
        match hit {
            Some(TabHit::Tab(i)) | Some(TabHit::CloseTab(i)) => {
                self.renderer.set_hovered_tab(Some(i))
            }
            _ => self.renderer.set_hovered_tab(None),
        }
        let on_strip = hit.is_some();
        if on_strip && self.resize_cursor.is_some() {
            self.resize_cursor = None;
            self.renderer.window().set_cursor(CursorIcon::Default);
        }
        on_strip
    }

    /// Handle a left click at the current cursor position: dismiss an open overlay,
    /// else hit-test the tab strip (switch tab / close a tab / open a new tab).
    fn left_click(&mut self) {
        // A click on an About-overlay link button (Docs/GitHub) opens the URL
        // rather than dismissing the overlay.
        if self.about {
            let (x, y) = self.cursor;
            if let Some(url) = self.renderer.about_link_at(x as f32, y as f32) {
                ember_platform::open_url(url);
                return;
            }
        }
        if self.dismiss_overlay() {
            return;
        }
        // A click while the Ctrl+Opt split preview is up commits that split.
        if let Some(p) = self.split_preview.take() {
            self.renderer.set_split_preview(None);
            let axis = if p.horizontal {
                Axis::Horizontal
            } else {
                Axis::Vertical
            };
            self.split_pane(p.pane, axis, p.ratio as f64);
            return;
        }
        // Any click commits an in-progress tab rename first.
        let was_editing = self.editing_tab.is_some();
        self.commit_rename();
        let (x, y) = self.cursor;
        if let Some(hit) = self.renderer.tab_hit(x as f32, y as f32) {
            match hit {
                TabHit::Tab(i) => {
                    // Second click on the same tab (and we weren't just committing a
                    // rename) → inline rename; otherwise select it + arm a drag.
                    let now = Instant::now();
                    let dbl = !was_editing
                        && self
                            .last_tab_click
                            .is_some_and(|(t, ti)| ti == i && now.duration_since(t) < MULTI_CLICK);
                    self.last_tab_click = Some((now, i));
                    if dbl {
                        self.start_rename(i);
                    } else {
                        self.select_tab(i + 1);
                        self.tab_drag = Some(TabDrag {
                            tab: i,
                            press_x: x,
                            active: false,
                        });
                    }
                }
                TabHit::CloseTab(i) => {
                    // The "✕" only renders with ≥2 tabs, so closing one never
                    // empties the app (no exit path needed). Same close flow as
                    // middle-click: confirm-if-busy via request_close.
                    if let Some(id) = self.tree.tabs.get(i).map(|t| t.id) {
                        let _ = self.request_close(PendingClose::Tab(id));
                    }
                }
                TabHit::NewTab => self.new_tab(),
                TabHit::Help => self.toggle_help(),
                TabHit::Settings => self.toggle_settings(),
            }
            return;
        }
        // A click on a pane scrollbar grabs the thumb (priority over selection),
        // and jumps to the clicked position.
        if let Some(sid) = self.renderer.scrollbar_hit(x as f32, y as f32) {
            self.scrollbar_drag = Some(sid.clone());
            self.scroll_to_at(&sid, y as f32);
            return;
        }
        // Mouse-aware app (vim :set mouse=a, htop): forward the click instead
        // of selecting — unless Shift is held, the universal local-selection
        // escape hatch.
        if self.forward_mouse_press(0) {
            return;
        }
        // A click in a pane body starts a selection (mode by click count).
        self.begin_selection(x, y);
    }

    /// Encode one xterm mouse report. SGR (1006) when the app enabled it, else
    /// legacy X10 bytes (coordinates clamped to its 223 limit).
    fn mouse_report_bytes(sgr: bool, btn: u8, col: u16, row: u16, press: bool) -> Vec<u8> {
        if sgr {
            format!(
                "\x1b[<{btn};{};{}{}",
                col + 1,
                row + 1,
                if press { 'M' } else { 'm' }
            )
            .into_bytes()
        } else {
            // X10 has no release button id — releases send 3.
            let b = if press { btn } else { 3 };
            let cx = (col + 1).min(223) as u8 + 32;
            let cy = (row + 1).min(223) as u8 + 32;
            vec![0x1b, b'[', b'M', 32 + b, cx, cy]
        }
    }

    /// The pane under the pointer + the cell the pointer is over, when that
    /// pane has mouse reporting enabled and Shift isn't overriding it.
    fn mouse_target(&self) -> Option<(SessionId, ember_core::MouseProto, u16, u16)> {
        if self.modifiers.shift_key() {
            return None;
        }
        let (x, y) = self.cursor;
        let (sid, rect) = self
            .pane_rects
            .iter()
            .find(|(_, r)| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height)?
            .clone();
        let modes = self.renderer.pane_modes(&sid);
        if !modes.mouse_reporting {
            return None;
        }
        let (cw, ch) = self.renderer.cell_size();
        let col = ((x - rect.x) / cw as f64).max(0.0) as u16;
        let row = ((y - rect.y) / ch as f64).max(0.0) as u16;
        Some((sid, modes.mouse, col, row))
    }

    /// Modifier bits added to the button code (xterm: alt +8, ctrl +16; shift
    /// +4 is never sent — Shift is reserved as the local-selection override).
    fn mouse_mod_bits(&self) -> u8 {
        (self.modifiers.alt_key() as u8) * 8 + (self.modifiers.control_key() as u8) * 16
    }

    /// Forward a button press to the pane under the pointer if it listens.
    /// Returns true when consumed (the caller must not start a selection).
    fn forward_mouse_press(&mut self, btn: u8) -> bool {
        let Some((sid, proto, col, row)) = self.mouse_target() else {
            return false;
        };
        if !proto.click {
            return false;
        }
        let code = btn + self.mouse_mod_bits();
        let bytes = Self::mouse_report_bytes(proto.sgr, code, col, row, true);
        if let Some(h) = self.sessions.get(&sid) {
            let _ = h
                .control
                .send(BackendControl::Input(bytes.into_boxed_slice()));
        }
        self.mouse_press = Some((sid, btn));
        self.last_mouse_cell = Some((col, row));
        true
    }

    /// Forward the matching release for an in-flight forwarded press.
    fn forward_mouse_release(&mut self, btn: u8) {
        let Some((sid, pressed)) = self.mouse_press.clone() else {
            return;
        };
        if pressed != btn {
            return;
        }
        self.mouse_press = None;
        self.last_mouse_cell = None;
        let proto = self.renderer.pane_modes(&sid).mouse;
        // Coordinates relative to the pressed pane, clamped inside it.
        let Some((_, rect)) = self.pane_rects.iter().find(|(s, _)| *s == sid) else {
            return;
        };
        let (cw, ch) = self.renderer.cell_size();
        let (x, y) = self.cursor;
        let col = ((x - rect.x).max(0.0) / cw as f64) as u16;
        let row = ((y - rect.y).max(0.0) / ch as f64) as u16;
        let code = btn + self.mouse_mod_bits();
        let bytes = Self::mouse_report_bytes(proto.sgr, code, col, row, false);
        if let Some(h) = self.sessions.get(&sid) {
            let _ = h
                .control
                .send(BackendControl::Input(bytes.into_boxed_slice()));
        }
    }

    /// Forward pointer motion: drag reports (1002) while a forwarded button is
    /// held, or all-motion reports (1003), deduped per cell.
    fn forward_mouse_motion(&mut self) {
        // Drag with a forwarded button held.
        if let Some((sid, btn)) = self.mouse_press.clone() {
            let proto = self.renderer.pane_modes(&sid).mouse;
            if !(proto.drag || proto.motion) {
                return;
            }
            let Some((_, rect)) = self.pane_rects.iter().find(|(s, _)| *s == sid) else {
                return;
            };
            let (cw, ch) = self.renderer.cell_size();
            let (x, y) = self.cursor;
            let col = ((x - rect.x).max(0.0) / cw as f64) as u16;
            let row = ((y - rect.y).max(0.0) / ch as f64) as u16;
            if self.last_mouse_cell == Some((col, row)) {
                return;
            }
            self.last_mouse_cell = Some((col, row));
            let code = btn + 32 + self.mouse_mod_bits();
            let bytes = Self::mouse_report_bytes(proto.sgr, code, col, row, true);
            if let Some(h) = self.sessions.get(&sid) {
                let _ = h
                    .control
                    .send(BackendControl::Input(bytes.into_boxed_slice()));
            }
            return;
        }
        // Button-less motion (1003 only).
        let Some((sid, proto, col, row)) = self.mouse_target() else {
            return;
        };
        if !proto.motion {
            return;
        }
        if self.last_mouse_cell == Some((col, row)) {
            return;
        }
        self.last_mouse_cell = Some((col, row));
        let code = 3 + 32 + self.mouse_mod_bits();
        let bytes = Self::mouse_report_bytes(proto.sgr, code, col, row, true);
        if let Some(h) = self.sessions.get(&sid) {
            let _ = h
                .control
                .send(BackendControl::Input(bytes.into_boxed_slice()));
        }
    }

    /// Send an absolute scroll for `session` mapping the mouse `y` to a display
    /// offset via the scrollbar geometry (thumb drag / track click).
    fn scroll_to_at(&self, session: &SessionId, y: f32) {
        if let Some(off) = self.renderer.scroll_offset_at(session, y) {
            if let Some(h) = self.sessions.get(session) {
                let _ = h
                    .control
                    .send(BackendControl::Scroll(ScrollAmount::To(off)));
            }
        }
    }

    /// Live tab drag-reorder: once past the threshold, move the dragged tab to the
    /// slot under the cursor as it crosses boundaries (Chrome-style).
    fn drag_tab_to(&mut self, x: f64) {
        let (from, active, press_x) = match &self.tab_drag {
            Some(d) => (d.tab, d.active, d.press_x),
            None => return,
        };
        if !active {
            if (x - press_x).abs() < TAB_DRAG_THRESHOLD {
                return;
            }
            if let Some(d) = self.tab_drag.as_mut() {
                d.active = true;
            }
        }
        if let Some(slot) = self.renderer.tab_slot_at(x as f32) {
            if slot != from {
                if let Some(d) = self.tab_drag.as_mut() {
                    d.tab = slot;
                }
                let vp = self.viewport();
                apply(
                    &mut self.tree,
                    LayoutCommand::MoveTab { from, to: slot },
                    vp,
                );
                self.sync_layout();
            }
        }
        // Push the lifted, cursor-following tab view every move (not just on a slot
        // cross) so the drag reads as smooth motion.
        let view = self
            .tab_drag
            .as_ref()
            .filter(|d| d.active)
            .map(|d| (d.tab, x as f32));
        self.renderer.set_tab_drag(view);
    }

    /// Begin inline rename of tab `i` (double-click); seeds the buffer with its title.
    fn start_rename(&mut self, i: usize) {
        if i >= self.tree.tabs.len() {
            return;
        }
        self.tab_drag = None;
        self.renderer.set_tab_drag(None);
        self.editing_tab = Some(i);
        self.edit_buffer = self.tree.tabs[i].title.clone();
        self.sync_layout();
    }

    /// Commit the in-progress rename (Enter / click away) → sets the tab title.
    fn commit_rename(&mut self) {
        let Some(i) = self.editing_tab.take() else {
            return;
        };
        if let Some(t) = self.tree.tabs.get(i) {
            let id = t.id;
            let title = self.edit_buffer.clone();
            let vp = self.viewport();
            apply(
                &mut self.tree,
                LayoutCommand::RenameTab { tab: id, title },
                vp,
            );
        }
        self.edit_buffer.clear();
        self.sync_layout();
    }

    /// Discard the in-progress rename (Esc).
    fn cancel_rename(&mut self) {
        if self.editing_tab.take().is_some() {
            self.edit_buffer.clear();
            self.sync_layout();
        }
    }

    /// Route a key into the inline tab-rename editor.
    fn rename_key(&mut self, key: &Key) {
        match key {
            Key::Named(NamedKey::Enter) => self.commit_rename(),
            Key::Named(NamedKey::Escape) => self.cancel_rename(),
            Key::Named(NamedKey::Backspace) => {
                self.edit_buffer.pop();
                self.sync_layout();
            }
            Key::Named(NamedKey::Space) => {
                self.edit_buffer.push(' ');
                self.sync_layout();
            }
            Key::Character(s) => {
                for c in s.chars().filter(|c| !c.is_control()) {
                    self.edit_buffer.push(c);
                }
                self.sync_layout();
            }
            _ => {}
        }
    }

    /// Focus the pane backing `sid` in the active tab (click-to-focus). No-op if it
    /// is already focused or the session isn't in this tab.
    fn focus_pane_of_session(&mut self, sid: &SessionId) {
        if self.focused_session_id().as_ref() == Some(sid) {
            return;
        }
        let active = self.tree.active;
        let pane = self.tree.tabs.get(active).and_then(|t| {
            t.root
                .leaves()
                .into_iter()
                .find(|(_, s)| s == sid)
                .map(|(p, _)| p)
        });
        if let Some(pane) = pane {
            self.tree.tabs[active].focus = pane;
            self.sync_layout();
        }
    }

    /// Map a logical-px point to `(session, row, col)` in whichever visible pane
    /// contains it (clamped to that pane's grid), or `None` if outside all panes.
    fn pixel_to_cell(&self, x: f64, y: f64) -> Option<(SessionId, u16, u16)> {
        let (cw, ch) = self.renderer.cell_size();
        let (cw, ch) = (cw as f64, ch as f64);
        for (sid, rect) in &self.pane_rects {
            if x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height {
                let dims = self
                    .dims_cache
                    .get(sid)
                    .copied()
                    .unwrap_or(GridDims::new(1, 1));
                let col = (((x - rect.x) / cw).floor().max(0.0) as u16)
                    .min(dims.columns.saturating_sub(1));
                let row = (((y - rect.y) / ch).floor().max(0.0) as u16)
                    .min(dims.screen_lines.saturating_sub(1));
                return Some((sid.clone(), row, col));
            }
        }
        None
    }

    /// Begin a selection at a pane-body point; click count picks the mode
    /// (1 = cell, 2 = word, 3 = line).
    fn begin_selection(&mut self, x: f64, y: f64) {
        let Some((sid, row, col)) = self.pixel_to_cell(x, y) else {
            self.clear_selection();
            return;
        };
        // Clicking into a pane focuses it (also correct for single-pane selection:
        // a selection is single-pane, so the click target must be the focused pane).
        self.focus_pane_of_session(&sid);
        let now = Instant::now();
        let same = self.last_click.as_ref().is_some_and(|(t, s, r, c)| {
            now.duration_since(*t) < MULTI_CLICK && *s == sid && *r == row && *c == col
        });
        self.click_count = if same {
            (self.click_count + 1).min(3)
        } else {
            1
        };
        self.last_click = Some((now, sid.clone(), row, col));
        let mode = match self.click_count {
            2 => SelectionMode::Word,
            3 => SelectionMode::Line,
            _ => SelectionMode::Simple,
        };
        let selection = Selection::new(Point::new(row, col), mode);
        self.sel = Some((sid, selection));
        self.selecting = true;
        self.renderer.set_selection(self.sel.clone());
    }

    /// Extend the in-progress selection to a logical-px point (drag).
    fn extend_selection(&mut self, x: f64, y: f64) {
        let Some((sid, row, col)) = self.pixel_to_cell(x, y) else {
            return;
        };
        if let Some((ssid, selection)) = self.sel.as_mut() {
            if *ssid == sid {
                selection.update(Point::new(row, col));
                self.renderer.set_selection(self.sel.clone());
            }
        }
    }

    /// Clear any selection.
    fn clear_selection(&mut self) {
        if self.sel.is_some() {
            self.sel = None;
            self.selecting = false;
            self.renderer.set_selection(None);
        }
    }

    /// Copy the current selection's text to the OS clipboard (Cmd+C).
    fn copy_selection(&mut self) {
        if let Some(text) = self.renderer.selected_text() {
            if let Some(cb) = self.clipboard.as_mut() {
                if let Err(e) = cb.set_text(text) {
                    eprintln!("[ember] clipboard copy failed: {e}");
                }
            }
        }
    }

    /// Paste the OS clipboard into the focused pane's PTY (Cmd+V), bracketed when
    /// the focused app enabled bracketed-paste mode.
    fn paste_clipboard(&mut self) {
        let text = self.clipboard.as_mut().and_then(|cb| cb.get_text().ok());
        if let Some(text) = text {
            self.paste_into_focused(&text);
        }
    }

    /// Toggle the cheat-sheet overlay (Cmd+/ and the Help menu item).
    fn toggle_help(&mut self) {
        if self.help {
            self.hide_help();
        } else {
            self.show_help();
        }
    }

    /// Hide the cheat-sheet overlay (no-op if not shown).
    fn hide_help(&mut self) {
        if self.help {
            self.help = false;
            self.renderer.set_help(None);
        }
    }

    /// Show the About overlay (closing other overlays — they're exclusive).
    fn show_about(&mut self) {
        self.hide_help();
        self.hide_settings();
        self.about = true;
        self.about_since = Instant::now();
        self.renderer.set_about(Some(about_info()));
    }

    /// Hide the About overlay (no-op if not shown).
    fn hide_about(&mut self) {
        if self.about {
            self.about = false;
            self.renderer.set_about(None);
        }
    }

    /// Toggle the About overlay (the Ember → About Ember menu item).
    fn toggle_about(&mut self) {
        if self.about {
            self.hide_about();
        } else {
            self.show_about();
        }
    }

    /// Whether any session that a `kind` close would destroy is running a
    /// command (OSC 133). For `Pane`, only the focused pane's session; for
    /// `Quit`, any session anywhere.
    /// Whether a session is running a foreground command (idle shell → false).
    fn session_busy(&self, sid: &SessionId) -> bool {
        self.sessions.get(sid).is_some_and(|h| h.is_busy())
    }

    fn close_hits_running(&self, kind: PendingClose) -> bool {
        match kind {
            PendingClose::Quit => self.sessions.values().any(|h| h.is_busy()),
            PendingClose::Pane => self
                .active_tab()
                .root
                .session_of(self.active_tab().focus)
                .is_some_and(|s| self.session_busy(s)),
            PendingClose::Tab(tab) => self
                .tree
                .tabs
                .iter()
                .find(|t| t.id == tab)
                .is_some_and(|t| t.root.leaves().iter().any(|(_, s)| self.session_busy(s))),
        }
    }

    /// Run a close, or defer it behind a confirmation if it would kill a running
    /// command. Returns true if the app should exit now.
    fn request_close(&mut self, kind: PendingClose) -> bool {
        if self.close_hits_running(kind) {
            self.show_close_confirm(kind);
            return false;
        }
        self.do_close(kind)
    }

    /// Actually perform a (possibly confirmed) close. Returns true to exit.
    fn do_close(&mut self, kind: PendingClose) -> bool {
        match kind {
            PendingClose::Pane => {
                self.close_focused();
                self.tree.tabs.is_empty()
            }
            PendingClose::Tab(tab) => self.do_close_tab(tab),
            PendingClose::Quit => true,
        }
    }

    /// Show the running-process confirmation (reuses the help-overlay panel).
    fn show_close_confirm(&mut self, kind: PendingClose) {
        self.hide_help();
        self.hide_about();
        self.hide_settings();
        self.pending_close = Some(kind);
        self.confirm_focus = 0; // Cancel is the safe default.
        self.update_confirm_view();
    }

    /// (Re)build the confirm modal from `pending_close` + `confirm_focus`.
    fn update_confirm_view(&mut self) {
        let Some(kind) = self.pending_close else {
            return;
        };
        let (title, confirm_label) = match kind {
            PendingClose::Pane => ("Close this pane?", "Close"),
            PendingClose::Tab(_) => ("Close this tab?", "Close"),
            PendingClose::Quit => ("Quit Ember?", "Quit"),
        };
        self.renderer.set_confirm(Some(ConfirmView {
            title: title.to_string(),
            message: "A command is still running.".to_string(),
            cancel_label: "Cancel".to_string(),
            confirm_label: confirm_label.to_string(),
            focused: self.confirm_focus,
        }));
    }

    /// Resolve a pending close confirmation. `Enter` performs it; any other key
    /// cancels. Returns true if the app should exit.
    fn resolve_confirm(&mut self, confirm: bool) -> bool {
        let Some(kind) = self.pending_close.take() else {
            return false;
        };
        self.renderer.set_confirm(None);
        if confirm { self.do_close(kind) } else { false }
    }

    /// Dismiss whichever modal overlay is open; returns whether one was showing.
    fn dismiss_overlay(&mut self) -> bool {
        let shown = self.help || self.about || self.settings_open;
        self.hide_help();
        self.hide_about();
        self.hide_settings();
        shown
    }

    /// The Settings overlay rows as `(label, value)`, derived from the config.
    /// Bind or unbind the debug control socket at runtime (the Settings toggle).
    /// When enabling, logs the socket path so it can be handed off for inspection.
    fn set_developer_mode(&mut self, on: bool) {
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

    fn settings_rows(&self) -> Vec<(String, String)> {
        let bg = &self.config.background;
        let on = |b: bool| if b { "on" } else { "off" }.to_string();
        // Backdrop image is config-only (path + fit live in config.toml); shown
        // here read-only as "<filename> (<fit>)" or "none".
        let image = match bg.image.as_deref() {
            Some(p) => {
                let name = std::path::Path::new(p)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(p);
                format!("{name} ({})", bg.image_fit)
            }
            None => "none".to_string(),
        };
        vec![
            ("Gradient backdrop".into(), on(bg.gradient)),
            ("Ember sparks".into(), on(bg.ember_sparks)),
            ("Ember density".into(), format!("{:.1}", bg.ember_density)),
            ("Ember FPS".into(), format!("{}", bg.ember_fps)),
            ("Scrim".into(), format!("{:.2}", bg.scrim)),
            ("Visual bell".into(), on(self.config.visual_bell)),
            ("Developer Mode".into(), on(self.config.developer_mode)),
            ("Backdrop image".into(), image),
        ]
    }

    /// Show the Settings overlay (closing other overlays — they're exclusive).
    fn show_settings(&mut self) {
        self.hide_help();
        self.hide_about();
        self.settings_open = true;
        let rows = self.settings_rows();
        self.renderer.set_settings(Some((rows, self.settings_sel)));
    }

    /// Hide the Settings overlay (no-op if not shown).
    fn hide_settings(&mut self) {
        if self.settings_open {
            self.settings_open = false;
            self.renderer.set_settings(None);
        }
    }

    /// Toggle the FPS/frame-time debug overlay (Cmd+Shift+P / `ctl fps`).
    fn toggle_fps(&mut self) {
        self.fps_overlay = !self.fps_overlay;
        if !self.fps_overlay {
            self.renderer.set_fps_overlay(None);
        }
        self.renderer.window().request_redraw();
    }

    /// Toggle the Settings overlay (Ember → Settings… / Cmd+,).
    fn toggle_settings(&mut self) {
        if self.settings_open {
            self.hide_settings();
        } else {
            self.show_settings();
        }
    }

    /// Re-push the Settings rows + selection to the renderer after a change.
    fn refresh_settings(&mut self) {
        let rows = self.settings_rows();
        self.renderer.set_settings(Some((rows, self.settings_sel)));
    }

    /// Handle a key while the Settings overlay is open: navigate + change values.
    fn settings_key(&mut self, key: &Key) {
        let n = self.settings_rows().len();
        match key {
            Key::Named(NamedKey::Escape) => {
                self.hide_settings();
                return;
            }
            Key::Named(NamedKey::ArrowUp) => {
                self.settings_sel = self.settings_sel.saturating_sub(1);
            }
            Key::Named(NamedKey::ArrowDown) => {
                self.settings_sel = (self.settings_sel + 1).min(n - 1);
            }
            Key::Named(NamedKey::ArrowRight) | Key::Named(NamedKey::Space) => {
                self.adjust_setting(1.0)
            }
            Key::Named(NamedKey::ArrowLeft) => self.adjust_setting(-1.0),
            _ => {}
        }
        self.refresh_settings();
    }

    /// Change the selected setting by `dir` (+1 / -1): toggle bools, step numbers.
    /// Persists the config and applies the appearance.
    fn adjust_setting(&mut self, dir: f32) {
        let bg = &mut self.config.background;
        match self.settings_sel {
            0 => bg.gradient = !bg.gradient,
            1 => bg.ember_sparks = !bg.ember_sparks,
            2 => bg.ember_density = (bg.ember_density + 0.1 * dir).clamp(0.0, 2.0),
            3 => bg.ember_fps = (bg.ember_fps as i32 + (5.0 * dir) as i32).clamp(10, 120) as u32,
            4 => bg.scrim = (bg.scrim + 0.05 * dir).clamp(0.0, 1.0),
            5 => self.config.visual_bell = !self.config.visual_bell,
            6 => {
                self.config.developer_mode = !self.config.developer_mode;
                self.set_developer_mode(self.config.developer_mode);
            }
            _ => {}
        }
        if let Err(e) = config::save(&self.config) {
            eprintln!("[ember] config save failed: {e}");
        }
        self.apply_appearance();
    }

    /// The backdrop params for the current config at animation time `t` seconds.
    fn backdrop_params(&self, t: f32) -> BackdropParams {
        let bg = &self.config.background;
        BackdropParams {
            gradient: bg.gradient,
            scrim: bg.scrim,
            sparks: bg.ember_sparks,
            density: bg.ember_density,
            time: t,
        }
    }

    /// Push the current config's appearance (campfire backdrop + ember sparks) to
    /// the renderer. Called on startup and whenever a setting changes. Decodes the
    /// backdrop image only when the configured path changes (cheap on idle changes).
    fn apply_appearance(&mut self) {
        let t = self.backdrop_since.elapsed().as_secs_f32();
        let params = self.backdrop_params(t);
        self.renderer.set_backdrop(params);

        let want = self.config.background.image.clone();
        if want != self.image_loaded {
            let fit = ImageFit::parse(&self.config.background.image_fit);
            let img = want.as_deref().and_then(load_backdrop_image);
            if want.is_some() && img.is_none() {
                eprintln!(
                    "[ember] backdrop image could not be loaded: {:?} (need a readable PNG)",
                    want.as_deref().unwrap_or("")
                );
            }
            self.renderer.set_backdrop_image(img, fit);
            self.image_loaded = want;
        }
    }

    /// The ambient ember animation's frame interval, from the configured `ember_fps`
    /// cap (clamped 10–120). Lower fps ≈ proportionally less CPU.
    fn ember_frame(&self) -> Duration {
        let fps = self.config.background.ember_fps.clamp(10, 120);
        Duration::from_millis((1000 / fps).max(1) as u64)
    }

    /// Advance every active animation to wall-clock time `now`: the About glow, the
    /// ember sparks, and the visual-bell flash decay. Each `set_*` is a function of
    /// elapsed time (not a delta), so an occasional long gap between frames just
    /// samples the curve later — no jump. Called from the loop once per frame-interval.
    fn advance_animations(&mut self, now: Instant) {
        if self.about {
            let t = now.duration_since(self.about_since).as_secs_f32();
            self.renderer.set_about_anim(ember_glow(t), t);
        }
        if self.backdrop_animating() {
            let params =
                self.backdrop_params(now.duration_since(self.backdrop_since).as_secs_f32());
            self.renderer.set_backdrop(params);
        }
        if let Some(since) = self.bell_flash_since {
            let i = bell_flash_intensity(now.duration_since(since).as_secs_f32());
            self.renderer.set_bell_flash(i);
            if i <= 0.0 {
                self.bell_flash_since = None;
            }
        }
    }

    /// Whether the ember sparks should be animating right now: opt-in, whenever
    /// the window is visible (focused or not — the campfire burns while you work
    /// elsewhere; Brandon's call 2026-07-04) and no modal overlay covers the
    /// panes. Occluded/asleep windows still go fully quiet.
    fn backdrop_animating(&self) -> bool {
        self.config.background.ember_sparks
            && !self.occluded
            && !self.help
            && !self.about
            && !self.settings_open
    }

    /// Handle a BEL from `session` (visual bell): start/refresh the ember flash,
    /// and if the belling tab isn't active, mark it with an unseen-bell indicator.
    fn on_bell(&mut self, session: &SessionId) {
        if !self.config.visual_bell {
            return;
        }
        let tab_idx = self
            .tree
            .tabs
            .iter()
            .position(|t| t.root.leaves().iter().any(|(_, s)| s == session));
        if let Some(i) = tab_idx {
            if i != self.tree.active {
                let id = self.tree.tabs[i].id;
                if self.belled_tabs.insert(id) {
                    self.sync_layout(); // repaint the tab with its bell dot
                }
            }
        }
        // Start (or refresh) the window flash; the animation loop decays it.
        self.bell_flash_since = Some(Instant::now());
        self.renderer.set_bell_flash(bell_flash_intensity(0.0));
    }

    /// Whether the visual-bell ember flash is currently animating.
    fn bell_flashing(&self) -> bool {
        self.bell_flash_since.is_some()
    }

    /// Jump to tab `n` (1-based); no-op if out of range.
    /// Live-zoom the terminal font by `delta` points (Cmd +/-).
    fn zoom_by(&mut self, delta: f32) {
        let target = self.renderer.font_size() + delta;
        self.zoom_to(target);
    }

    /// Set the terminal font to `size` and re-layout (the cell size, hence every
    /// pane's grid dims, changed). No-op if the size didn't change.
    fn zoom_to(&mut self, size: f32) {
        if self.renderer.set_font_size(size) {
            self.sync_layout();
        }
    }

    fn select_tab(&mut self, n: usize) {
        if n >= 1 && n <= self.tree.tabs.len() {
            self.tree.active = n - 1;
            self.sync_layout();
        }
    }

    /// Close the pane backing `session` wherever it lives (a shell exited, or a
    /// background tab's pane was closed). Switches to that tab so `ClosePane`'s
    /// active-tab semantics apply, then restores a sane active index.
    fn close_session(&mut self, session: &SessionId) {
        let found = self.tree.tabs.iter().enumerate().find_map(|(ti, tab)| {
            tab.root
                .leaves()
                .into_iter()
                .find(|(_, s)| s == session)
                .map(|(pane, _)| (ti, pane))
        });
        let Some((ti, pane)) = found else {
            // Not in the layout (already removed); just clean up the backend.
            self.kill_session(session);
            return;
        };
        // Remember which tab the USER is on (by id) so closing a background
        // tab's pane doesn't teleport them there. ClosePane targets the active
        // tab, so switch to `ti`, close, then restore the user's tab.
        let user_tab = self.tree.tabs.get(self.tree.active).map(|t| t.id);
        self.tree.active = ti;
        let vp = self.viewport();
        let effects = apply(
            &mut self.tree,
            LayoutCommand::ClosePane { target: pane },
            vp,
        );
        self.apply_effects(effects);
        if self.tree.tabs.is_empty() {
            return;
        }
        // Restore the user's tab if it still exists (it may have shifted index,
        // or been the very tab whose last pane just closed).
        self.tree.active = user_tab
            .and_then(|id| self.tree.tabs.iter().position(|t| t.id == id))
            .unwrap_or(self.tree.active)
            .min(self.tree.tabs.len() - 1);
        self.sync_layout();
    }

    /// Perform a tab close (after any confirmation). Returns true to exit.
    fn do_close_tab(&mut self, tab: TabId) -> bool {
        let user_tab = self.tree.tabs.get(self.tree.active).map(|t| t.id);
        let vp = self.viewport();
        let effects = apply(&mut self.tree, LayoutCommand::CloseTab { tab }, vp);
        self.apply_effects(effects);
        if self.tree.tabs.is_empty() {
            self.shutdown_all();
            return true;
        }
        self.tree.active = user_tab
            .and_then(|id| self.tree.tabs.iter().position(|t| t.id == id))
            .unwrap_or(self.tree.active)
            .min(self.tree.tabs.len() - 1);
        self.sync_layout();
        false
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
                "https://github.com/kingb/ember-term".to_string(),
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

/// The keyboard cheat-sheet shown by the Cmd+/ overlay. Keep in sync with
/// [`RunState::handle_shortcut`].
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
/// `(key, description)`. Keep in sync with [`RunState::handle_shortcut`].
pub(crate) fn help_lines() -> Vec<(String, String)> {
    [
        ("", "PANES"),
        ("Cmd+D", "Split right (side by side)"),
        ("Cmd+Shift+D", "Split down (stacked)"),
        ("Ctrl+Opt+Click", "Split by drop zone (drag to preview)"),
        ("Cmd+W", "Close pane"),
        ("Click pane", "Focus it"),
        ("Cmd+Arrows", "Focus pane"),
        ("", "TABS"),
        ("Cmd+T", "New tab"),
        ("Cmd+Shift+Arrows", "Switch tab"),
        ("Cmd+1..9", "Jump to tab"),
        ("Drag / Double-click", "Reorder / rename tab"),
        ("", "SELECTION & CLIPBOARD"),
        ("Drag / 2×/3× click", "Select text / word / line"),
        ("Cmd+C / Cmd+V", "Copy / paste"),
        ("", "SCROLLBACK"),
        ("Wheel / Shift+PgUp/Dn", "Scroll history"),
        ("Shift+Home/End", "Scroll to top / bottom"),
        ("", "SHELL"),
        ("Cmd+[ / Cmd+]", "Jump to prev / next command"),
        ("", "APP"),
        ("Cmd+,", "Settings"),
        ("Cmd+/", "Show this help"),
    ]
    .iter()
    .map(|(k, d)| (k.to_string(), d.to_string()))
    .collect()
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
    use super::{BELL_FLASH_SECS, bell_flash_intensity, bracket_paste, encode_key};
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
        use super::RunState;
        // SGR press/release: 1-based coords, M/m terminator.
        assert_eq!(
            RunState::mouse_report_bytes(true, 0, 4, 9, true),
            b"\x1b[<0;5;10M"
        );
        assert_eq!(
            RunState::mouse_report_bytes(true, 0, 4, 9, false),
            b"\x1b[<0;5;10m"
        );
        // Wheel up with ctrl (+16).
        assert_eq!(
            RunState::mouse_report_bytes(true, 64 + 16, 0, 0, true),
            b"\x1b[<80;1;1M"
        );
        // X10: +32 offsets, release is button 3.
        assert_eq!(
            RunState::mouse_report_bytes(false, 0, 4, 9, true),
            vec![0x1b, b'[', b'M', 32, 37, 42]
        );
        assert_eq!(
            RunState::mouse_report_bytes(false, 0, 4, 9, false),
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
}
