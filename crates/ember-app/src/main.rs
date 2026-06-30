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
    Axis, BackendControl, BackendEvent, BackendHandle, Config, Direction, GridDims, LayoutCommand,
    LayoutEffect, LayoutNode, PaneId, Rect, SessionBackend, SessionId, Tab, TabId, apply, layout,
};
use ember_platform::MenuAction;
use ember_render::{
    BackdropParams, CELL_HEIGHT, CELL_WIDTH, ImageFit, Point, Renderer, Selection, SelectionMode,
    TabHit, TabLabel, VisiblePane,
};
use ember_session::{LocalPty, LocalPtyConfig};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::WindowId;

/// The Ember app icon (embedded). Set on the window + the macOS dock at startup.
const ICON_PNG: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icon.png"));

pub(crate) const PAD: f32 = 4.0;
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;
/// How often the loop polls the pixel lanes when idle (~125 Hz). A proxy-waker on
/// frame push is the noted refinement; this keeps CPU sane without it.
const POLL: Duration = Duration::from_millis(8);
/// Redraw cadence (~60fps) while an animation (e.g. the About glow) is active.
const ANIM_FRAME: Duration = Duration::from_millis(16);
/// Max gap between clicks at the same cell to count as a double/triple click.
const MULTI_CLICK: Duration = Duration::from_millis(400);

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        print_banner();
        return;
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
    // Optional debug control surface. `EMBER_CONTROL=1` binds a per-PID socket
    // under $TMPDIR/ember-ctl/ (so multiple instances coexist); an explicit path
    // is used verbatim. `ember-term ctl`/`mcp` then drive + introspect this window.
    let control_rx = match std::env::var("EMBER_CONTROL") {
        Ok(val) if !val.is_empty() => {
            let bind = control::server_bind_path(&val);
            match control::spawn_listener(&bind) {
                Ok(rx) => {
                    eprintln!("[ember] control socket listening at {}", bind.display());
                    Some(rx)
                }
                Err(e) => {
                    eprintln!("[ember] control socket failed: {e}");
                    None
                }
            }
        }
        _ => None,
    };
    let event_loop = EventLoop::new().expect("create event loop");
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App {
        state: None,
        control_rx,
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

struct App {
    state: Option<RunState>,
    /// Receiver for debug-control commands (Some when `EMBER_CONTROL` is set).
    control_rx: Option<Receiver<ControlMsg>>,
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

impl ApplicationHandler for App {
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
        let renderer = Renderer::new(Arc::clone(&window));

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
            help: false,
            about: false,
            about_since: Instant::now(),
            config: config::load(),
            settings_open: false,
            settings_sel: 0,
            image_loaded: None,
            backdrop_since: Instant::now(),
            window_focused: true,
            menu: ember_platform::build_menu(),
            cursor: (0.0, 0.0),
            pane_rects: Vec::new(),
            sel: None,
            selecting: false,
            last_click: None,
            click_count: 0,
            clipboard: arboard::Clipboard::new().ok(),
        };
        state.spawn_session(session, GridDims::new(DEFAULT_COLS, DEFAULT_ROWS));
        state.sync_layout();
        state.apply_appearance();
        self.state = Some(state);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => {
                state.shutdown_all();
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                state.px = (size.width.max(1), size.height.max(1));
                state.renderer.resize(state.px.0, state.px.1);
                state.sync_layout();
            }
            WindowEvent::Focused(focused) => {
                state.window_focused = focused;
                if focused {
                    state.renderer.window().request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(mods) => state.modifiers = mods.state(),
            WindowEvent::CursorMoved { position, .. } => {
                let sf = state.renderer.window().scale_factor();
                state.cursor = (position.x / sf, position.y / sf);
                if state.selecting {
                    let (x, y) = state.cursor;
                    state.extend_selection(x, y);
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => state.left_click(),
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => state.selecting = false,
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
                // While a modal overlay (help / About) is up, the next *fresh* key
                // press dismisses it. Auto-repeat is ignored — otherwise holding
                // Cmd+/ for a moment repeats "/" and closes it right after it opens.
                if state.help || state.about {
                    if !key.repeat {
                        state.dismiss_overlay();
                    }
                    return;
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
                if let Some(bytes) = encode_key(&key.logical_key, mods) {
                    if let Some(h) = state.focused_session() {
                        let _ = h
                            .control
                            .send(BackendControl::Input(bytes.into_boxed_slice()));
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                state.drain_frames();
                state.renderer.render();
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
        for (id, handle) in &state.sessions {
            while let Ok(event) = handle.events.try_recv() {
                match event {
                    BackendEvent::Title(t) => {
                        if Some(id) == focused.as_ref() {
                            new_title = Some(t);
                        }
                    }
                    BackendEvent::Exited(_) => exited.push(id.clone()),
                    _ => {}
                }
            }
        }
        if let Some(title) = new_title {
            state.renderer.window().set_title(&title);
        }
        for session in exited {
            state.close_session(&session);
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
            }
        }
        if state.tree.tabs.is_empty() {
            state.shutdown_all();
            event_loop.exit();
            return;
        }
        // Poll the pixel lanes; redraw only when something changed.
        if state.drain_frames() {
            state.renderer.window().request_redraw();
        }
        // Pace the loop: ~60fps while an animation is active, else the idle poll.
        // The per-frame animation redraw is driven from `new_events` on the timer
        // tick (not here) — that's the difference between sleeping between frames
        // and spinning at full speed. Requesting a redraw *every* `about_to_wait`
        // makes winit service it immediately, defeating `WaitUntil` (the CPU spike).
        let animating = state.about || state.backdrop_animating();
        let wait = if animating { ANIM_FRAME } else { POLL };
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + wait));
    }

    /// Drive animations on the timer tick set by `about_to_wait`. This is the only
    /// place that advances + redraws animations, so the loop genuinely sleeps
    /// `ANIM_FRAME` between frames instead of busy-looping. The `set_*` calls
    /// request the redraw internally; we don't request one every wait cycle.
    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        if !matches!(cause, StartCause::ResumeTimeReached { .. }) {
            return;
        }
        let Some(state) = self.state.as_mut() else {
            return;
        };
        let now = Instant::now();
        if state.about {
            let t = now.duration_since(state.about_since).as_secs_f32();
            state.renderer.set_about_anim(ember_glow(t), t);
        }
        if state.backdrop_animating() {
            let params =
                state.backdrop_params(now.duration_since(state.backdrop_since).as_secs_f32());
            state.renderer.set_backdrop(params);
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
        let chrome = Renderer::chrome_height(self.tree.tabs.len()) as f64;
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

    /// Spawn a shell-backed session and register its grid with the renderer.
    fn spawn_session(&mut self, id: SessionId, dims: GridDims) {
        let handle = LocalPty::spawn(LocalPtyConfig::new(id.clone(), dims)).expect("spawn shell");
        self.renderer.ensure_pane(&id, dims);
        self.dims_cache.insert(id.clone(), dims);
        self.sessions.insert(id, handle);
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
                    if let Some(bytes) = encode_key(&key, ModifiersState::empty()) {
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
                    } else if let Some(bytes) = encode_key(&key, mods) {
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
                    Err(e) => serde_json::json!({"ok": false, "error": e}).to_string(),
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
            ControlMsg::Paste(text) => {
                if !text.is_empty() {
                    self.send_to_focused(text.into_bytes());
                }
            }
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
        serde_json::json!({
            "scale_factor": sf,
            "surface": [self.px.0, self.px.1],
            "tabs": self.tree.tabs.len(),
            "active_tab": self.tree.active,
            "focus_pane": focus.0,
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
        let tabs: Vec<TabLabel> = self
            .tree
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| TabLabel {
                title: if t.title.is_empty() {
                    format!("{}", i + 1)
                } else {
                    t.title.clone()
                },
                active: i == active,
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
                self.renderer.apply_delta(id, delta);
                dirty = true;
            }
        }
        dirty
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
            // Cmd+W — close the focused pane (and its tab if it was the last).
            Key::Character(s) if s.eq_ignore_ascii_case("w") => {
                self.close_focused();
                true
            }
            // Cmd+T — open a new tab with a fresh shell.
            Key::Character(s) if s.eq_ignore_ascii_case("t") => {
                self.new_tab();
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
            // Cmd+Arrows — move focus geometrically between panes.
            Key::Named(NamedKey::ArrowLeft) => self.focus_dir(Direction::Left),
            Key::Named(NamedKey::ArrowRight) => self.focus_dir(Direction::Right),
            Key::Named(NamedKey::ArrowUp) => self.focus_dir(Direction::Up),
            Key::Named(NamedKey::ArrowDown) => self.focus_dir(Direction::Down),
            _ => false,
        }
    }

    fn split_focused(&mut self, axis: Axis) {
        let target = self.active_tab().focus;
        let new_pane = PaneId(self.next_pane);
        self.next_pane += 1;
        let new_session = SessionId::new(format!("s{}", self.next_session));
        self.next_session += 1;
        self.spawn_session(
            new_session.clone(),
            GridDims::new(DEFAULT_COLS, DEFAULT_ROWS),
        );
        let vp = self.viewport();
        let effects = apply(
            &mut self.tree,
            LayoutCommand::SplitPane {
                target,
                axis,
                ratio: 0.5,
                new_pane,
                new_session,
            },
            vp,
        );
        self.apply_effects(effects);
        self.sync_layout();
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
        self.spawn_session(session.clone(), GridDims::new(DEFAULT_COLS, DEFAULT_ROWS));
        let vp = self.viewport();
        let effects = apply(
            &mut self.tree,
            LayoutCommand::NewTab { id, session, pane },
            vp,
        );
        self.apply_effects(effects);
        self.sync_layout();
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

    /// Handle a left click at the current cursor position: dismiss an open overlay,
    /// else hit-test the tab strip (switch tab / open a new tab).
    fn left_click(&mut self) {
        if self.dismiss_overlay() {
            return;
        }
        let (x, y) = self.cursor;
        if let Some(hit) = self.renderer.tab_hit(x as f32, y as f32) {
            match hit {
                TabHit::Tab(i) => self.select_tab(i + 1),
                TabHit::NewTab => self.new_tab(),
                TabHit::Help => self.toggle_help(),
            }
            return;
        }
        // A click in a pane body starts a selection (mode by click count).
        self.begin_selection(x, y);
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

    /// Paste the OS clipboard into the focused pane's PTY (Cmd+V). Non-bracketed
    /// for v1 — bracketed-paste mode isn't surfaced across the seam yet (see bead).
    fn paste_clipboard(&mut self) {
        let text = self.clipboard.as_mut().and_then(|cb| cb.get_text().ok());
        if let Some(text) = text {
            if !text.is_empty() {
                self.send_to_focused(text.into_bytes());
            }
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

    /// Dismiss whichever modal overlay is open; returns whether one was showing.
    fn dismiss_overlay(&mut self) -> bool {
        let shown = self.help || self.about || self.settings_open;
        self.hide_help();
        self.hide_about();
        self.hide_settings();
        shown
    }

    /// The Settings overlay rows as `(label, value)`, derived from the config.
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
            ("Scrim".into(), format!("{:.2}", bg.scrim)),
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
            3 => bg.scrim = (bg.scrim + 0.05 * dir).clamp(0.0, 1.0),
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

    /// Whether the ember sparks should be animating right now (opt-in, only while
    /// focused and no modal overlay is covering the panes).
    fn backdrop_animating(&self) -> bool {
        self.config.background.ember_sparks
            && self.window_focused
            && !self.help
            && !self.about
            && !self.settings_open
    }

    /// Jump to tab `n` (1-based); no-op if out of range.
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
        self.tree.active = self.tree.active.min(self.tree.tabs.len() - 1);
        self.sync_layout();
    }
}

/// Content for the About overlay.
fn about_info() -> ember_render::AboutInfo {
    ember_render::AboutInfo {
        title: "ember".to_string(),
        lines: vec![
            "a native terminal".to_string(),
            String::new(),
            format!("v{}", env!("CARGO_PKG_VERSION")),
            "MIT OR Apache-2.0".to_string(),
            "Brandon W. King · Claude Opus 4.8".to_string(),
        ],
    }
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
fn help_lines() -> Vec<(String, String)> {
    [
        ("Cmd+D", "Split right (side by side)"),
        ("Cmd+Shift+D", "Split down (stacked)"),
        ("Cmd+W", "Close pane"),
        ("Cmd+T", "New tab"),
        ("Cmd+Arrows", "Focus pane"),
        ("Cmd+Shift+Arrows", "Switch tab"),
        ("Cmd+1..9", "Jump to tab"),
        ("Drag / 2×/3× click", "Select text / word / line"),
        ("Cmd+C / Cmd+V", "Copy selection / paste"),
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
fn encode_key(key: &Key, mods: ModifiersState) -> Option<Vec<u8>> {
    match key {
        Key::Named(NamedKey::Enter) => Some(b"\r".to_vec()),
        Key::Named(NamedKey::Backspace) => Some(vec![0x7f]),
        Key::Named(NamedKey::Tab) => Some(b"\t".to_vec()),
        Key::Named(NamedKey::Escape) => Some(vec![0x1b]),
        Key::Named(NamedKey::Space) => Some(b" ".to_vec()),
        Key::Named(NamedKey::ArrowUp) => Some(b"\x1b[A".to_vec()),
        Key::Named(NamedKey::ArrowDown) => Some(b"\x1b[B".to_vec()),
        Key::Named(NamedKey::ArrowRight) => Some(b"\x1b[C".to_vec()),
        Key::Named(NamedKey::ArrowLeft) => Some(b"\x1b[D".to_vec()),
        Key::Character(s) => {
            if mods.control_key() {
                let c = s.chars().next()?;
                if c.is_ascii_alphabetic() {
                    return Some(vec![(c.to_ascii_lowercase() as u8) & 0x1f]);
                }
            }
            Some(s.as_bytes().to_vec())
        }
        _ => None,
    }
}
