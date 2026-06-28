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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ember_core::{
    Axis, BackendControl, BackendEvent, BackendHandle, Direction, GridDims, LayoutCommand,
    LayoutEffect, LayoutNode, PaneId, Rect, SessionBackend, SessionId, Tab, TabId, apply, layout,
};
use ember_render::{CELL_HEIGHT, CELL_WIDTH, Renderer, TabLabel, VisiblePane};
use ember_session::{LocalPty, LocalPtyConfig};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::WindowId;

/// The Ember app icon (embedded). Set on the window + the macOS dock at startup.
const ICON_PNG: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icon.png"));

const PAD: f32 = 4.0;
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;
/// How often the loop polls the pixel lanes when idle (~125 Hz). A proxy-waker on
/// frame push is the noted refinement; this keeps CPU sane without it.
const POLL: Duration = Duration::from_millis(8);

fn main() {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        print_banner();
        return;
    }
    let event_loop = EventLoop::new().expect("create event loop");
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App::default();
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

#[derive(Default)]
struct App {
    state: Option<RunState>,
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
}

/// Inset a rect by `p` on every side (clamped to stay positive).
fn inset(r: Rect, p: f64) -> Rect {
    Rect::new(
        r.x + p,
        r.y + p,
        (r.width - 2.0 * p).max(1.0),
        (r.height - 2.0 * p).max(1.0),
    )
}

/// Cell grid that fits an inner pixel rect.
fn dims_for_rect(r: Rect, cw: f32, ch: f32) -> GridDims {
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
        };
        state.spawn_session(session, GridDims::new(DEFAULT_COLS, DEFAULT_ROWS));
        state.sync_layout();
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
            WindowEvent::ModifiersChanged(mods) => state.modifiers = mods.state(),
            WindowEvent::KeyboardInput { event: key, .. } => {
                if key.state != ElementState::Pressed {
                    return;
                }
                let mods = state.modifiers;
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
        if state.tree.tabs.is_empty() {
            state.shutdown_all();
            event_loop.exit();
            return;
        }
        // Poll the pixel lanes; redraw only when something changed.
        if state.drain_frames() {
            state.renderer.window().request_redraw();
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + POLL));
    }
}

impl RunState {
    /// The pixel rect available to the layout (full surface minus the tab strip).
    fn viewport(&self) -> Rect {
        let chrome = Renderer::chrome_height(self.tree.tabs.len()) as f64;
        Rect::new(
            0.0,
            chrome,
            self.px.0 as f64,
            (self.px.1 as f64 - chrome).max(1.0),
        )
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
            // Cmd+Alt+Arrows — move focus geometrically between panes.
            Key::Named(NamedKey::ArrowLeft) if mods.alt_key() => self.focus_dir(Direction::Left),
            Key::Named(NamedKey::ArrowRight) if mods.alt_key() => self.focus_dir(Direction::Right),
            Key::Named(NamedKey::ArrowUp) if mods.alt_key() => self.focus_dir(Direction::Up),
            Key::Named(NamedKey::ArrowDown) if mods.alt_key() => self.focus_dir(Direction::Down),
            // Cmd+Shift+Arrows — cycle the active tab.
            Key::Named(NamedKey::ArrowRight) if mods.shift_key() => self.cycle_tab(1),
            Key::Named(NamedKey::ArrowLeft) if mods.shift_key() => self.cycle_tab(-1),
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
