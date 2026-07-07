//! Per-window state (`WindowState`) split out of the old monolithic `RunState`.
//!
//! Field classification (behavior-identical split; see `Shared` in `main.rs`):
//!
//! - **`WindowState`** (this file) — everything tied to one window/surface:
//!   `renderer`, `tree`, `px`, `dims_cache`, `modifiers`, `cursor`,
//!   `pointer_cursor`, selection (`sel`/`selecting`/`last_click`/`click_count`),
//!   drags (`tab_drag`/`divider_drag`/`scrollbar_drag`/`split_preview`),
//!   overlays (`help`/`about`/`about_since`/`settings_open`/`settings_sel`/
//!   `editing_tab`/`edit_buffer`/`pending_close`/`confirm_focus`), `pressed_link`,
//!   bell state (`bell_flash_since`/`belled_tabs`), `last_tab_click`,
//!   `wheel_accum`, the backdrop-image/render bookkeeping (`image_loaded`,
//!   `window_focused`, `fps_overlay`, `last_frame`, `fps_ema_ms`, `render_ema_ms`,
//!   `last_anim`, `occluded`, `render_starved`) — plus every method that only
//!   touches those. Methods touching both take `shared: &Shared`/`&mut Shared`.
//! - **`Shared`** (`main.rs`) — `sessions`, `config`, `platform`, control server
//!   (`control_rx`/`control_server`), `backdrop_since`, `cwd_by_session`, `menu`,
//!   `wake`, plus fields ambiguous-for-now marked `// window-scoped later?`
//!   (`next_pane`/`next_session`/`next_tab`, `bracketed`, `focus_notified`,
//!   `mouse_press`, `last_mouse_cell`, `titles`).

use std::collections::HashMap;
use std::time::Instant;

use ember_core::{
    Axis, BackendControl, BackendHandle, Direction, GridDims, LayoutCommand, LayoutEffect, PaneId,
    Rect, RowKind, ScrollAmount, SessionBackend, SessionId, SettingsRowView, Tab, TabId, apply,
    layout, setting_rows,
};
use ember_platform::PlatformBackend;
use ember_render::{
    ConfirmView, ImageFit, Point, Renderer, Selection, SelectionMode, TabHit, TabLabel, VisiblePane,
};
use ember_session::{LocalPty, LocalPtyConfig};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{CursorIcon, WindowId};

use crate::config;
use crate::control::ControlMsg;
use crate::{
    ControlClose, DEFAULT_COLS, DEFAULT_ROWS, MULTI_CLICK, PAD, PendingClose, Shared, about_info,
    bell_flash_intensity, bracket_paste, click_selection_should_clear, dims_for_rect, ember_glow,
    encode_key, help_lines, inset, load_backdrop_image, named_key, parse_chord,
    step_selectable_row, tab_display_title, url_is_openable,
};
#[cfg(target_os = "linux")]
use crate::{alt_digit_tab, linux_chord_translate};

/// State for an in-progress tab drag-reorder.
pub(crate) struct TabDrag {
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
pub(crate) struct SplitPreview {
    pane: PaneId,
    horizontal: bool,
    ratio: f32,
}

/// Everything tied to a single window and its surface.
pub(crate) struct WindowState {
    pub(crate) renderer: Renderer,
    /// The multiplexer model (one tab list, one binary split tree per tab).
    pub(crate) tree: ember_core::WindowTree,
    /// Last grid dims pushed to each session — so resizes are only sent on change.
    pub(crate) dims_cache: HashMap<SessionId, GridDims>,
    pub(crate) modifiers: ModifiersState,
    /// Physical surface size in px.
    pub(crate) px: (u32, u32),
    /// Whether the keyboard cheat-sheet overlay is showing.
    pub(crate) help: bool,
    /// Whether the About overlay is showing, and when it opened (for the glow clock).
    pub(crate) about: bool,
    pub(crate) about_since: Instant,
    /// The Settings overlay state (open + selected row).
    pub(crate) settings_open: bool,
    pub(crate) settings_sel: usize,
    /// The backdrop-image path currently uploaded to the renderer, so
    /// `apply_appearance` re-decodes only when the configured path changes.
    pub(crate) image_loaded: Option<String>,
    /// Whether the window is focused (drives DEC 1004 focus reporting).
    pub(crate) window_focused: bool,
    /// FPS/frame-time debug overlay (toggle: Cmd+Shift+P / `ctl fps`). EMAs of the
    /// redraw interval (cadence) and the render() call duration (per-frame cost).
    pub(crate) fps_overlay: bool,
    pub(crate) last_frame: Option<Instant>,
    pub(crate) fps_ema_ms: f32,
    pub(crate) render_ema_ms: f32,
    /// When the last animation frame was advanced+redrawn. Animation is paced by
    /// wall-clock elapsed since this (checked on every wake), NOT by the timer's
    /// `ResumeTimeReached` — a flood of mouse-move events would otherwise keep
    /// resetting the `WaitUntil` deadline and starve the animation (visible stutter).
    pub(crate) last_anim: Instant,
    /// Visual bell: when the current ember flash started (None = no flash), and the
    /// set of tabs with an unseen bell (a background tab belled).
    pub(crate) bell_flash_since: Option<Instant>,
    pub(crate) belled_tabs: std::collections::HashSet<TabId>,
    /// Last cursor position in **logical** px.
    pub(crate) cursor: (f64, f64),
    /// Visible panes' inner rects (logical px), for mouse→cell hit-testing.
    pub(crate) pane_rects: Vec<(SessionId, Rect)>,
    /// Active text selection + the session (pane) it belongs to.
    pub(crate) sel: Option<(SessionId, Selection)>,
    /// Whether a mouse drag is currently extending the selection.
    pub(crate) selecting: bool,
    /// Last mouse-down (time, pane, cell), for double/triple-click detection.
    pub(crate) last_click: Option<(Instant, SessionId, u16, u16)>,
    /// Consecutive-click count at the same cell (1 = simple, 2 = word, 3 = line).
    pub(crate) click_count: u32,
    /// In-progress tab drag-reorder: the tab being dragged, the press x (logical),
    /// and whether the drag threshold has been crossed (below it, it's a click).
    pub(crate) tab_drag: Option<TabDrag>,
    /// In-progress scrollbar-thumb drag: the session whose scrollbar is grabbed.
    pub(crate) scrollbar_drag: Option<SessionId>,
    /// In-progress divider drag to resize a split: `(a-side pane, split axis,
    /// last cursor position along that axis in logical px)`.
    pub(crate) divider_drag: Option<(PaneId, Axis, f64)>,
    /// The pointer cursor currently shown (so we don't reset it every move).
    pub(crate) pointer_cursor: CursorIcon,
    /// Left press that started on a link: `(session, link id, row, col)`.
    /// Opens on release if the pointer is still on the same link and cell.
    pub(crate) pressed_link: Option<(SessionId, u32, u16, u16)>,
    /// Live Ctrl+Opt split drop-zone preview (hover), committed on click.
    pub(crate) split_preview: Option<SplitPreview>,
    /// Last tab-button mouse-down (time, tab index), for double-click-to-rename.
    pub(crate) last_tab_click: Option<(Instant, usize)>,
    /// Inline tab rename in progress: the tab index + the live edit buffer.
    pub(crate) editing_tab: Option<usize>,
    pub(crate) edit_buffer: String,
    /// A destructive close awaiting confirmation (a busy pane).
    pub(crate) pending_close: Option<PendingClose>,
    /// The focused confirm button: 0 = Cancel (safe default), 1 = Close/Quit.
    pub(crate) confirm_focus: usize,
    /// Fractional wheel-scroll carry (trackpad pixel deltas < one cell).
    pub(crate) wheel_accum: f32,
    /// The window is fully hidden (another window covers it): suppress the
    /// ambient animation so an idle-but-covered window doesn't burn cycles.
    pub(crate) occluded: bool,
    /// The last render attempt found no drawable (transient startup shortage,
    /// display asleep). Drives a bounded retry cadence via the animation
    /// machinery — the renderer's StarveGate caps actual frame prep at 4/s —
    /// so a missed frame repaints without the spin. Cleared by the first present.
    pub(crate) render_starved: bool,
}

impl WindowState {
    /// Build a fresh per-window state around an already-created `renderer` +
    /// seeded `tree`; every other field starts at its "nothing in progress"
    /// default. Shared by the very first window (`resumed`) and every window
    /// opened afterward (`open_window`) — the caller sets `px` to the real
    /// surface size right after construction.
    pub(crate) fn new(renderer: Renderer, tree: ember_core::WindowTree) -> Self {
        Self {
            renderer,
            tree,
            dims_cache: HashMap::new(),
            modifiers: ModifiersState::empty(),
            px: (1, 1),
            help: false,
            about: false,
            about_since: Instant::now(),
            settings_open: false,
            settings_sel: 0,
            image_loaded: None,
            window_focused: true,
            fps_overlay: false,
            last_frame: None,
            fps_ema_ms: 0.0,
            render_ema_ms: 0.0,
            last_anim: Instant::now(),
            bell_flash_since: None,
            belled_tabs: std::collections::HashSet::new(),
            cursor: (0.0, 0.0),
            pane_rects: Vec::new(),
            sel: None,
            selecting: false,
            last_click: None,
            click_count: 0,
            tab_drag: None,
            split_preview: None,
            scrollbar_drag: None,
            divider_drag: None,
            pointer_cursor: CursorIcon::Default,
            pressed_link: None,
            last_tab_click: None,
            editing_tab: None,
            edit_buffer: String::new(),
            pending_close: None,
            confirm_focus: 0,
            wheel_accum: 0.0,
            occluded: false,
            render_starved: false,
        }
    }

    /// The **logical**-pixel rect available to the layout (full surface minus the
    /// tab strip). `px` is physical; the renderer draws in logical units and scales
    /// to physical by the HiDPI factor, so layout/dims must be logical too — else a
    /// Retina shell gets 2× the columns it can show.
    pub(crate) fn viewport(&self) -> Rect {
        let sf = self.renderer.window().scale_factor();
        let chrome = Renderer::chrome_height() as f64;
        let w = self.px.0 as f64 / sf;
        let h = self.px.1 as f64 / sf;
        Rect::new(0.0, chrome, w.max(1.0), (h - chrome).max(1.0))
    }

    pub(crate) fn active_tab(&self) -> &Tab {
        &self.tree.tabs[self.tree.active]
    }

    pub(crate) fn focused_session_id(&self) -> Option<SessionId> {
        if self.tree.tabs.is_empty() {
            return None;
        }
        let tab = self.active_tab();
        tab.root.session_of(tab.focus).cloned()
    }

    pub(crate) fn focused_session<'a>(&self, shared: &'a Shared) -> Option<&'a BackendHandle> {
        self.focused_session_id()
            .and_then(|id| shared.sessions.get(&id))
    }

    /// Scroll the focused pane's scrollback by `amount`. No-op on the alternate
    /// screen (the projection gates it).
    pub(crate) fn scroll_focused(&self, shared: &Shared, amount: ScrollAmount) {
        if let Some(h) = self.focused_session(shared) {
            let _ = h.control.send(BackendControl::Scroll(amount));
        }
    }

    /// Jump the focused pane to the previous (`-1`) / next (`+1`) OSC 133 prompt.
    pub(crate) fn jump_prompt(&self, shared: &Shared, dir: i8) {
        if let Some(h) = self.focused_session(shared) {
            let _ = h.control.send(BackendControl::JumpMark(dir));
        }
    }

    /// Handle a mouse-wheel notch worth `lines` (positive = up, into history). On
    /// the primary screen this scrolls history; in a full-screen app (alt screen)
    /// with no mouse reporting it translates to arrow keys so `less`/`man`/`vim`
    /// still page; with mouse reporting on we leave it alone (that path is a future
    /// mouse-forwarding feature).
    pub(crate) fn wheel_scroll(&self, shared: &Shared, lines: i32) {
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
                if let Some(h) = shared.sessions.get(&id) {
                    let _ = h
                        .control
                        .send(BackendControl::Input(bytes.into_boxed_slice()));
                }
                return;
            }
        }
        let Some(h) = shared.sessions.get(&id) else {
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
    pub(crate) fn spawn_session(
        &mut self,
        shared: &mut Shared,
        id: SessionId,
        dims: GridDims,
        cwd: Option<String>,
    ) -> bool {
        let mut cfg = LocalPtyConfig::new(id.clone(), dims);
        cfg.shell_integration = shared.config.shell_integration;
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
        handle.frames.set_waker(shared.wake.clone());
        self.renderer.ensure_pane(&id, dims);
        self.dims_cache.insert(id.clone(), dims);
        // Register ownership BEFORE the session goes live, so the very first
        // PTY delta already has a window to route to (see `drain_own_frames`).
        shared
            .session_window
            .insert(id.clone(), self.renderer.window().id());
        shared.sessions.insert(id, handle);
        true
    }

    /// Tear down a session backend and forget its render/cache state.
    pub(crate) fn kill_session(&mut self, shared: &mut Shared, id: &SessionId) {
        if let Some(h) = shared.sessions.remove(id) {
            let _ = h.control.send(BackendControl::Shutdown);
        }
        shared.session_window.remove(id);
        self.renderer.remove_pane(id);
        self.dims_cache.remove(id);
    }

    /// Every session in this window's layout tree (every tab) — used to tear
    /// the whole window down (closing a non-last window) rather than one
    /// pane/tab at a time.
    pub(crate) fn window_session_ids(&self) -> Vec<SessionId> {
        self.tree
            .tabs
            .iter()
            .flat_map(|t| t.root.leaves().into_iter().map(|(_, s)| s))
            .collect()
    }

    /// Send raw bytes to the focused session's PTY (used by control + key paths).
    pub(crate) fn send_to_focused(&self, shared: &Shared, bytes: Vec<u8>) {
        if let Some(h) = self.focused_session(shared) {
            let _ = h
                .control
                .send(BackendControl::Input(bytes.into_boxed_slice()));
        }
    }

    /// Act on a debug-control command (see `control`): inject text/keys, run a
    /// chord, or reply with a JSON state dump.
    ///
    /// Returns what the caller must do about this window's OS lifecycle, if
    /// anything: this method has no view of how many windows exist or of
    /// `self.windows`/an `ActiveEventLoop`, so it can't itself decide "quit the
    /// app" vs. "just close this window" — the caller (`about_to_wait`) does,
    /// via [`crate::finish_close`].
    pub(crate) fn handle_control(
        &mut self,
        shared: &mut Shared,
        msg: ControlMsg,
    ) -> Option<ControlClose> {
        // Route injected keys into the interactive Settings overlay (for tests),
        // mirroring the real keyboard path.
        if self.settings_open {
            if let ControlMsg::Key(name) = &msg {
                if let Some(k) = named_key(name) {
                    self.settings_key(shared, &k);
                }
                return None;
            }
        }
        // Mirror the keyboard: the close-confirm modal captures input (arrows/Tab
        // move focus, Enter activates, Esc cancels).
        if self.pending_close.is_some() {
            if let ControlMsg::Key(name) = &msg {
                match name.as_str() {
                    "Escape" => {
                        self.resolve_confirm(shared, false);
                    }
                    "Enter" | "Return" => {
                        let kind = self.pending_close;
                        let ok = self.confirm_focus == 1;
                        if self.resolve_confirm(shared, ok) {
                            return Some(match kind {
                                Some(PendingClose::Quit) => ControlClose::ExitApp,
                                _ => ControlClose::CloseWindow,
                            });
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
            return None;
        }
        // Mirror the keyboard: while a modal overlay is up, any input dismisses it
        // (but state/screenshot still work, so the overlay can be inspected).
        if self.help || self.about {
            if let ControlMsg::Type(_) | ControlMsg::Key(_) | ControlMsg::Chord(_) = &msg {
                self.dismiss_overlay();
                return None;
            }
        }
        match msg {
            ControlMsg::Type(text) => self.send_to_focused(shared, text.into_bytes()),
            ControlMsg::Key(name) => {
                if let Some(key) = named_key(&name) {
                    let app_cursor = self.focused_app_cursor();
                    if let Some(bytes) =
                        encode_key(&key, ModifiersState::empty(), app_cursor, false)
                    {
                        self.send_to_focused(shared, bytes);
                    }
                }
            }
            ControlMsg::Chord(combo) => {
                if let Some((key, mods)) = parse_chord(&combo) {
                    #[cfg(target_os = "linux")]
                    if let Some(n) = alt_digit_tab(&key, mods) {
                        self.select_tab(shared, n);
                        return None;
                    }
                    #[cfg(target_os = "linux")]
                    if let Some((k, m)) = linux_chord_translate(&key, mods) {
                        if self.handle_shortcut(shared, &k, m) && self.tree.tabs.is_empty() {
                            return Some(ControlClose::CloseWindow);
                        }
                        return None;
                    }
                    if mods.super_key() {
                        if self.handle_shortcut(shared, &key, mods) && self.tree.tabs.is_empty() {
                            return Some(ControlClose::CloseWindow);
                        }
                    } else if let Some(bytes) =
                        encode_key(&key, mods, self.focused_app_cursor(), false)
                    {
                        self.send_to_focused(shared, bytes);
                    }
                }
            }
            // Handled by the caller (`about_to_wait`) before this method is
            // ever invoked — both need every window (`self.windows` +
            // `shared.window_order`), not just this one: `State` builds the
            // top-level `windows[]` array, and `Focus` searches (and can
            // raise/select on) any window, not only this one. Kept here only
            // so the match stays exhaustive.
            ControlMsg::State(_) => {}
            ControlMsg::Focus(..) => {}
            ControlMsg::Raise => self.raise_window(),
            ControlMsg::Screenshot(path, reply) => {
                let resp = match self.renderer.capture_to_png(std::path::Path::new(&path)) {
                    Ok(()) => serde_json::json!({"ok": true, "path": path}).to_string(),
                    Err(e) => serde_json::json!({"ok": false, "error": e.to_string()}).to_string(),
                };
                let _ = reply.send(resp);
            }
            ControlMsg::Click(x, y) => {
                // Synthesize a full click: press half (selection/tab/scrollbar
                // hit-testing, arms `pressed_link`) then release half (drag
                // teardown + the link click-to-open decision) — a bare
                // `left_click()` can never reach `open_path`, since that only
                // fires on release.
                self.cursor = (x, y);
                self.left_click(shared);
                self.left_release(shared);
            }
            ControlMsg::About => self.toggle_about(),
            ControlMsg::Settings => self.toggle_settings(shared),
            ControlMsg::Select(r1, c1, r2, c2, mode) => {
                let sid = self.focused_session_id()?;
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
            ControlMsg::Copy => self.copy_selection(shared),
            ControlMsg::Paste(text) => self.paste_into_focused(shared, &text),
            ControlMsg::Fps => self.toggle_fps(),
            ControlMsg::Scroll(amount) => self.scroll_focused(shared, amount),
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
                    self.on_bell(shared, &s);
                }
            }
            ControlMsg::ReorderTab(from, to) => {
                let vp = self.viewport();
                apply(&mut self.tree, LayoutCommand::MoveTab { from, to }, vp);
                self.sync_layout(shared);
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
                    self.sync_layout(shared);
                }
            }
            ControlMsg::EditTab(i) => self.start_rename(shared, i),
            // Handled by the caller (`about_to_wait`) before this method is
            // ever invoked — it needs `self.windows`/`event_loop` to actually
            // open the window. Kept here only so the match stays exhaustive.
            ControlMsg::NewWindow => {}
            // Same reasoning as `NewWindow`: `apply_move` needs the live
            // window set + event loop, neither of which this method has.
            ControlMsg::MoveTab(..) | ControlMsg::PromotePane(..) | ControlMsg::MergeTab(..) => {}
        }
        None
    }

    /// Display titles for every tab, in tab order — the shared substring-match
    /// input for both `ctl focus`'s single-window search (historical) and the
    /// App-level cross-window search (`match_tab_title_across` in `main.rs`).
    pub(crate) fn tab_titles(&self) -> Vec<String> {
        self.tree
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| tab_display_title(&t.title, i))
            .collect()
    }

    /// The `tabs` array shape shared by `ctl state`'s per-window `windows[]`
    /// entries and its top-level (focused-window) fields: every tab's
    /// 1-based `index`/`active`/displayed `title`/`sessions`.
    pub(crate) fn tabs_summary_json(&self) -> Vec<serde_json::Value> {
        self.tree
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let sessions: Vec<String> =
                    t.root.leaves().iter().map(|(_, s)| s.0.clone()).collect();
                serde_json::json!({
                    "index": i + 1,
                    "active": i == self.tree.active,
                    "title": tab_display_title(&t.title, i),
                    "sessions": sessions,
                })
            })
            .collect()
    }

    /// A JSON snapshot of the live app for the debug control surface: scale,
    /// surface size, tabs, and the active tab's panes (dims/cursor/styles/text).
    ///
    /// This describes ONE window. The App-level multi-window `ctl state`
    /// builder (`build_state_json` in `main.rs`) uses this for the focused
    /// window's top-level (compatibility) fields, and `tabs_summary_json` for
    /// every OTHER window's lighter `windows[]` entry.
    pub(crate) fn state_json(&self, shared: &Shared) -> String {
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
            .and_then(|id| shared.bracketed.get(&id).copied())
            .unwrap_or(false);
        // Every tab, not just the active one — external tools map a name to a
        // tab index with this (`index` is 1-based, matching `cmd+N` and
        // `ctl focus`). `title` is the strip's displayed title, same rule.
        let tabs = self.tabs_summary_json();
        serde_json::json!({
            "scale_factor": sf,
            "surface": [self.px.0, self.px.1],
            "tabs": tabs,
            "active_tab": self.tree.active,
            "focus_pane": focus.0,
            "bracketed_paste": bracketed,
            "panes": panes,
        })
        .to_string()
    }

    /// Run the `KillSession` side effects of an applied command (the layout tree is
    /// already mutated; spawns/resizes are handled by the caller + `sync_layout`).
    pub(crate) fn apply_effects(&mut self, shared: &mut Shared, effects: Vec<LayoutEffect>) {
        for effect in effects {
            if let LayoutEffect::KillSession(id) = effect {
                self.kill_session(shared, &id);
            }
        }
    }

    /// Recompute the active tab's tiling, hand it to the renderer, and resize each
    /// session's PTY whose grid dims changed. Idempotent; the single source of
    /// truth for "what's on screen and how big each shell is."
    pub(crate) fn sync_layout(&mut self, shared: &Shared) {
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
                if let Some(h) = shared.sessions.get(&session) {
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
                    } else {
                        tab_display_title(&t.title, i)
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

    /// Poll the pixel lane of every session **this window owns** (per
    /// `shared.session_window`) into its grid. Returns whether anything
    /// changed (background tabs stay current so they're right when
    /// re-shown). Scoped to `window_id` rather than iterating every session
    /// in `shared.sessions` — with N windows, an unscoped drain would also
    /// pull a DIFFERENT window's frames and silently drop them (`apply_delta`
    /// finds no matching pane in `self.renderer` and no-ops).
    pub(crate) fn drain_own_frames(&mut self, shared: &mut Shared, window_id: WindowId) -> bool {
        let mut dirty = false;
        for (id, handle) in &shared.sessions {
            if shared.session_window.get(id) != Some(&window_id) {
                continue;
            }
            while let Some(delta) = handle.frames.take() {
                shared.bracketed.insert(id.clone(), delta.bracketed_paste);
                self.renderer.apply_delta(id, delta);
                dirty = true;
            }
        }
        dirty
    }

    /// Send `text` to the focused pane as a paste: when that session enabled
    /// bracketed paste, wrap it in `ESC[200~`…`ESC[201~` (stripping any embedded
    /// markers first — see [`bracket_paste`]); otherwise send it raw.
    pub(crate) fn paste_into_focused(&self, shared: &Shared, text: &str) {
        if text.is_empty() {
            return;
        }
        let bracketed = self
            .focused_session_id()
            .and_then(|id| shared.bracketed.get(&id).copied())
            .unwrap_or(false);
        self.send_to_focused(shared, bracket_paste(text, bracketed));
    }

    /// Handle a Super-modified key as a multiplexer command. Returns whether it was
    /// a recognized shortcut (so the caller can check for an emptied tree → quit).
    pub(crate) fn handle_shortcut(
        &mut self,
        shared: &mut Shared,
        key: &Key,
        mods: ModifiersState,
    ) -> bool {
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
                self.toggle_settings(shared);
                true
            }
            // Cmd+[ / Cmd+] — jump to previous / next command prompt (OSC 133).
            Key::Character(s) if s.as_str() == "[" => {
                self.jump_prompt(shared, -1);
                true
            }
            Key::Character(s) if s.as_str() == "]" => {
                self.jump_prompt(shared, 1);
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
                self.copy_selection(shared);
                true
            }
            Key::Character(s) if s.eq_ignore_ascii_case("v") => {
                self.paste_clipboard(shared);
                true
            }
            // Cmd+D / Cmd+Shift+D — split the focused pane side-by-side / stacked.
            Key::Character(s) if s.eq_ignore_ascii_case("d") => {
                let axis = if mods.shift_key() {
                    Axis::Vertical
                } else {
                    Axis::Horizontal
                };
                self.split_focused(shared, axis);
                true
            }
            // Cmd+W — close the focused pane (and its tab if it was the last),
            // confirming first if it's running a command. The caller's
            // tabs-empty check still handles quit-on-last-pane for the
            // no-confirm path; a deferred confirm leaves tabs intact.
            Key::Character(s) if s.eq_ignore_ascii_case("w") => {
                self.request_close(shared, PendingClose::Pane);
                true
            }
            // Cmd+T — open a new tab with a fresh shell.
            Key::Character(s) if s.eq_ignore_ascii_case("t") => {
                self.new_tab(shared);
                true
            }
            // Cmd+0 — reset the font size to the config baseline.
            Key::Character(s) if s.as_str() == "0" => {
                self.zoom_to(shared, shared.config.font.size);
                true
            }
            // Cmd+= / Cmd++ — zoom in; Cmd+- / Cmd+_ — zoom out (1pt steps).
            Key::Character(s) if s.as_str() == "=" || s.as_str() == "+" => {
                self.zoom_by(shared, 1.0);
                true
            }
            Key::Character(s) if s.as_str() == "-" || s.as_str() == "_" => {
                self.zoom_by(shared, -1.0);
                true
            }
            // Cmd+1..9 — jump straight to a tab (Option/Alt is awkward on macOS, so
            // tab + pane navigation avoid it entirely).
            Key::Character(s) if s.chars().all(|c| c.is_ascii_digit()) && !s.is_empty() => {
                if let Some(n) = s.chars().next().and_then(|c| c.to_digit(10)) {
                    self.select_tab(shared, n as usize);
                }
                true
            }
            // Cmd+Shift+Arrows — cycle the active tab. (Checked before the plain
            // arrows so Shift wins.)
            Key::Named(NamedKey::ArrowRight) if mods.shift_key() => self.cycle_tab(shared, 1),
            Key::Named(NamedKey::ArrowLeft) if mods.shift_key() => self.cycle_tab(shared, -1),
            // Cmd+Ctrl+Arrows — resize the focused pane (grow toward the arrow).
            Key::Named(NamedKey::ArrowRight) if mods.control_key() => {
                self.resize_focused(shared, Axis::Horizontal, 1.0)
            }
            Key::Named(NamedKey::ArrowLeft) if mods.control_key() => {
                self.resize_focused(shared, Axis::Horizontal, -1.0)
            }
            Key::Named(NamedKey::ArrowDown) if mods.control_key() => {
                self.resize_focused(shared, Axis::Vertical, 1.0)
            }
            Key::Named(NamedKey::ArrowUp) if mods.control_key() => {
                self.resize_focused(shared, Axis::Vertical, -1.0)
            }
            // Cmd+Arrows — move focus geometrically between panes.
            Key::Named(NamedKey::ArrowLeft) => self.focus_dir(shared, Direction::Left),
            Key::Named(NamedKey::ArrowRight) => self.focus_dir(shared, Direction::Right),
            Key::Named(NamedKey::ArrowUp) => self.focus_dir(shared, Direction::Up),
            Key::Named(NamedKey::ArrowDown) => self.focus_dir(shared, Direction::Down),
            _ => false,
        }
    }

    pub(crate) fn split_focused(&mut self, shared: &mut Shared, axis: Axis) {
        self.split_pane(shared, self.active_tab().focus, axis, 0.5);
    }

    /// Minimum pane extent (px) along `axis`, from a floor of cells + padding —
    /// the value core clamps splits/resizes against (metrics live app-side).
    pub(crate) fn min_px(&self, axis: Axis) -> f64 {
        const MIN_COLS: f32 = 8.0;
        const MIN_ROWS: f32 = 3.0;
        let (cw, ch) = self.renderer.cell_size();
        let px = match axis {
            Axis::Horizontal => MIN_COLS * cw + 2.0 * PAD,
            Axis::Vertical => MIN_ROWS * ch + 2.0 * PAD,
        };
        px as f64
    }

    /// Split `target` on `axis` at `ratio` (existing pane's fraction), spawning a
    /// fresh shell in the new pane (right/bottom). Shared by Cmd+D + the visual split.
    pub(crate) fn split_pane(
        &mut self,
        shared: &mut Shared,
        target: PaneId,
        axis: Axis,
        ratio: f64,
    ) {
        let new_pane = PaneId(shared.next_pane);
        let new_session = SessionId::new(format!("s{}", shared.next_session));
        // Cwd-inheriting split (design §8.1): the new pane starts where the
        // split's parent pane last reported itself (OSC 1337 `CurrentDir`).
        let inherited_cwd = self
            .active_tab()
            .root
            .session_of(target)
            .and_then(|sid| shared.cwd_by_session.get(sid))
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
        shared.next_pane += 1;
        shared.next_session += 1;
        if !self.spawn_session(
            shared,
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
            self.apply_effects(shared, rollback);
            self.sync_layout(shared);
            return;
        }
        self.apply_effects(shared, effects);
        self.sync_layout(shared);
    }

    /// Whether Ctrl+Opt is currently held (the visual-split modifier).
    pub(crate) fn split_modifier_held(&self) -> bool {
        self.modifiers.control_key() && self.modifiers.alt_key()
    }

    /// DECCKM state of the focused pane (drives arrow/Home/End encoding).
    pub(crate) fn focused_app_cursor(&self) -> bool {
        self.focused_session_id()
            .map(|id| self.renderer.pane_modes(&id).app_cursor)
            .unwrap_or(false)
    }

    /// The pane under the mouse cursor, if any.
    pub(crate) fn session_under_cursor(&self) -> Option<SessionId> {
        let (x, y) = self.cursor;
        self.pane_rects
            .iter()
            .find(|(_, r)| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height)
            .map(|(s, _)| s.clone())
    }

    /// Recompute the Ctrl+Opt split drop-zone preview from the cursor over a pane:
    /// nearer the right edge → side-by-side (new pane right), nearer the bottom →
    /// stacked (new pane below); the divider follows the cursor for the ratio.
    pub(crate) fn update_split_preview(&mut self) {
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
    pub(crate) fn clear_split_preview(&mut self) {
        if self.split_preview.take().is_some() {
            self.renderer.set_split_preview(None);
        }
    }

    pub(crate) fn close_focused(&mut self, shared: &mut Shared) {
        let target = self.active_tab().focus;
        let vp = self.viewport();
        let effects = apply(&mut self.tree, LayoutCommand::ClosePane { target }, vp);
        self.apply_effects(shared, effects);
        if !self.tree.tabs.is_empty() {
            self.sync_layout(shared);
        }
    }

    pub(crate) fn new_tab(&mut self, shared: &mut Shared) {
        let id = TabId(shared.next_tab);
        shared.next_tab += 1;
        let pane = PaneId(shared.next_pane);
        shared.next_pane += 1;
        let session = SessionId::new(format!("s{}", shared.next_session));
        shared.next_session += 1;
        // Design §8.1 scopes cwd inheritance to splits, not new tabs — a new
        // tab starts at the shell's own default, same as today.
        if !self.spawn_session(
            shared,
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
        self.apply_effects(shared, effects);
        self.sync_layout(shared);
    }

    /// Keyboard resize of the focused pane: `dir` (±1) grows/shrinks it by a few
    /// cells along `axis` (key-repeat makes it fast). Core takes a px delta.
    pub(crate) fn resize_focused(&mut self, shared: &Shared, axis: Axis, dir: f64) -> bool {
        let (cw, ch) = self.renderer.cell_size();
        let step = 3.0
            * if matches!(axis, Axis::Horizontal) {
                cw
            } else {
                ch
            } as f64;
        let target = self.active_tab().focus;
        self.resize_pane_px(shared, target, axis, dir * step);
        true
    }

    /// Resize the split enclosing `target` along `axis` by `delta` px. Shared by
    /// keyboard resize and mouse divider drag. Core clamps against `min_px`.
    pub(crate) fn resize_pane_px(
        &mut self,
        shared: &Shared,
        target: PaneId,
        axis: Axis,
        delta: f64,
    ) {
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
        self.sync_layout(shared);
    }

    /// The split divider under logical `(x, y)`, as `(a-side pane, axis)`, when
    /// the cursor is in the gap between two adjacent panes. `None` otherwise.
    pub(crate) fn divider_at(&self, x: f64, y: f64) -> Option<(PaneId, Axis)> {
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

    pub(crate) fn focus_dir(&mut self, shared: &mut Shared, dir: Direction) -> bool {
        let vp = self.viewport();
        let effects = apply(&mut self.tree, LayoutCommand::FocusDir { dir }, vp);
        self.apply_effects(shared, effects);
        self.sync_layout(shared);
        true
    }

    pub(crate) fn cycle_tab(&mut self, shared: &Shared, delta: isize) -> bool {
        let n = self.tree.tabs.len();
        if n > 1 {
            let cur = self.tree.active as isize;
            self.tree.active = (cur + delta).rem_euclid(n as isize) as usize;
            self.sync_layout(shared);
        }
        true
    }

    /// Show the keyboard cheat-sheet overlay (closing other overlays — exclusive).
    pub(crate) fn show_help(&mut self) {
        self.hide_about();
        self.hide_settings();
        self.help = true;
        self.renderer.set_help(Some(help_lines()));
    }

    /// Track which tab the cursor is over, driving the hover highlight + "✕"
    /// close affordance. Returns `true` when the cursor is over the tab strip, so
    /// the caller treats the motion as chrome (not pane) input. Also clears a
    /// stale resize cursor when moving off a divider onto the strip.
    pub(crate) fn update_tab_hover(&mut self, x: f64, y: f64) -> bool {
        let hit = self.renderer.tab_hit(x as f32, y as f32);
        match hit {
            Some(TabHit::Tab(i)) | Some(TabHit::CloseTab(i)) => {
                self.renderer.set_hovered_tab(Some(i))
            }
            _ => self.renderer.set_hovered_tab(None),
        }
        let on_strip = hit.is_some();
        if on_strip {
            // Moving onto the tab strip is chrome, not pane input, so the
            // `CursorMoved` else-branch below never runs to refresh these —
            // clear a stale resize/link cursor and hover highlight here.
            self.renderer.set_hovered_link(None);
            if self.pointer_cursor != CursorIcon::Default {
                self.pointer_cursor = CursorIcon::Default;
                self.renderer.window().set_cursor(CursorIcon::Default);
            }
        }
        on_strip
    }

    /// Mouse-up half of a left click: drag/selection teardown, plus the
    /// click-to-open decision for a link (same link + same cell as the press,
    /// so drags still select instead of opening).
    pub(crate) fn left_release(&mut self, shared: &Shared) {
        self.tab_drag = None;
        self.renderer.set_tab_drag(None);
        let was_selecting = self.selecting;
        self.selecting = false;
        self.scrollbar_drag = None;
        self.divider_drag = None;
        // A plain click (no drag) clears the selection rather than leaving a
        // one-cell one — see click_selection_should_clear.
        if was_selecting && click_selection_should_clear(self.sel.as_ref().map(|(_, s)| s)) {
            self.clear_selection();
        }
        if let Some((psid, pid, prow, pcol)) = self.pressed_link.take() {
            if let Some((sid, id, url, row, col)) = self.link_under_cursor() {
                if sid == psid && id == pid && row == prow && col == pcol {
                    if url_is_openable(&url) {
                        shared.platform.open_path(&url);
                    } else {
                        eprintln!("[ember] refusing to open non-http(s) url");
                    }
                }
            }
        }
    }

    pub(crate) fn left_click(&mut self, shared: &mut Shared) {
        // A click on an About-overlay link button (Docs/GitHub) opens the URL
        // rather than dismissing the overlay.
        if self.about {
            let (x, y) = self.cursor;
            if let Some(url) = self.renderer.about_link_at(x as f32, y as f32) {
                shared.platform.open_path(url);
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
            self.split_pane(shared, p.pane, axis, p.ratio as f64);
            return;
        }
        // Any click commits an in-progress tab rename first.
        let was_editing = self.editing_tab.is_some();
        self.commit_rename(shared);
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
                        self.start_rename(shared, i);
                    } else {
                        self.select_tab(shared, i + 1);
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
                        let _ = self.request_close(shared, PendingClose::Tab(id));
                    }
                }
                TabHit::NewTab => self.new_tab(shared),
                TabHit::Help => self.toggle_help(),
                TabHit::Settings => self.toggle_settings(shared),
            }
            return;
        }
        // A click on a pane scrollbar grabs the thumb (priority over selection),
        // and jumps to the clicked position.
        if let Some(sid) = self.renderer.scrollbar_hit(x as f32, y as f32) {
            self.scrollbar_drag = Some(sid.clone());
            self.scroll_to_at(shared, &sid, y as f32);
            return;
        }
        // Remember a press that lands on a link; the open decision is made on
        // release (click = same link + same cell, so drags still select).
        self.pressed_link = self
            .link_under_cursor()
            .map(|(sid, id, _, row, col)| (sid, id, row, col));
        if let Some((sid, ..)) = &self.pressed_link {
            // In a mouse-reporting pane this press was claimed via the open
            // modifier — consume it so the app doesn't also react to it.
            if self.renderer.pane_modes(sid).mouse_reporting {
                return;
            }
        }
        // Mouse-aware app (vim :set mouse=a, htop): forward the click instead
        // of selecting — unless Shift is held, the universal local-selection
        // escape hatch.
        if self.forward_mouse_press(shared, 0) {
            return;
        }
        // A click in a pane body starts a selection (mode by click count).
        self.begin_selection(shared, x, y);
    }

    /// Encode one xterm mouse report. SGR (1006) when the app enabled it, else
    /// legacy X10 bytes (coordinates clamped to its 223 limit).
    pub(crate) fn mouse_report_bytes(
        sgr: bool,
        btn: u8,
        col: u16,
        row: u16,
        press: bool,
    ) -> Vec<u8> {
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
    pub(crate) fn mouse_target(&self) -> Option<(SessionId, ember_core::MouseProto, u16, u16)> {
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
    pub(crate) fn mouse_mod_bits(&self) -> u8 {
        (self.modifiers.alt_key() as u8) * 8 + (self.modifiers.control_key() as u8) * 16
    }

    /// Forward a button press to the pane under the pointer if it listens.
    /// Returns true when consumed (the caller must not start a selection).
    pub(crate) fn forward_mouse_press(&mut self, shared: &mut Shared, btn: u8) -> bool {
        let Some((sid, proto, col, row)) = self.mouse_target() else {
            return false;
        };
        if !proto.click {
            return false;
        }
        let code = btn + self.mouse_mod_bits();
        let bytes = Self::mouse_report_bytes(proto.sgr, code, col, row, true);
        if let Some(h) = shared.sessions.get(&sid) {
            let _ = h
                .control
                .send(BackendControl::Input(bytes.into_boxed_slice()));
        }
        shared.mouse_press = Some((sid, btn));
        shared.last_mouse_cell = Some((col, row));
        true
    }

    /// Forward the matching release for an in-flight forwarded press.
    pub(crate) fn forward_mouse_release(&mut self, shared: &mut Shared, btn: u8) {
        let Some((sid, pressed)) = shared.mouse_press.clone() else {
            return;
        };
        if pressed != btn {
            return;
        }
        shared.mouse_press = None;
        shared.last_mouse_cell = None;
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
        if let Some(h) = shared.sessions.get(&sid) {
            let _ = h
                .control
                .send(BackendControl::Input(bytes.into_boxed_slice()));
        }
    }

    /// Forward pointer motion: drag reports (1002) while a forwarded button is
    /// held, or all-motion reports (1003), deduped per cell.
    pub(crate) fn forward_mouse_motion(&mut self, shared: &mut Shared) {
        // Drag with a forwarded button held.
        if let Some((sid, btn)) = shared.mouse_press.clone() {
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
            if shared.last_mouse_cell == Some((col, row)) {
                return;
            }
            shared.last_mouse_cell = Some((col, row));
            let code = btn + 32 + self.mouse_mod_bits();
            let bytes = Self::mouse_report_bytes(proto.sgr, code, col, row, true);
            if let Some(h) = shared.sessions.get(&sid) {
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
        if shared.last_mouse_cell == Some((col, row)) {
            return;
        }
        shared.last_mouse_cell = Some((col, row));
        let code = 3 + 32 + self.mouse_mod_bits();
        let bytes = Self::mouse_report_bytes(proto.sgr, code, col, row, true);
        if let Some(h) = shared.sessions.get(&sid) {
            let _ = h
                .control
                .send(BackendControl::Input(bytes.into_boxed_slice()));
        }
    }

    /// Send an absolute scroll for `session` mapping the mouse `y` to a display
    /// offset via the scrollbar geometry (thumb drag / track click).
    pub(crate) fn scroll_to_at(&self, shared: &Shared, session: &SessionId, y: f32) {
        if let Some(off) = self.renderer.scroll_offset_at(session, y) {
            if let Some(h) = shared.sessions.get(session) {
                let _ = h
                    .control
                    .send(BackendControl::Scroll(ScrollAmount::To(off)));
            }
        }
    }

    /// Live tab drag-reorder: once past the threshold, move the dragged tab to the
    /// slot under the cursor as it crosses boundaries (Chrome-style).
    pub(crate) fn drag_tab_to(&mut self, shared: &Shared, x: f64) {
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
                self.sync_layout(shared);
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
    pub(crate) fn start_rename(&mut self, shared: &Shared, i: usize) {
        if i >= self.tree.tabs.len() {
            return;
        }
        self.tab_drag = None;
        self.renderer.set_tab_drag(None);
        self.editing_tab = Some(i);
        self.edit_buffer = self.tree.tabs[i].title.clone();
        self.sync_layout(shared);
    }

    /// Commit the in-progress rename (Enter / click away) → sets the tab title.
    pub(crate) fn commit_rename(&mut self, shared: &Shared) {
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
        self.sync_layout(shared);
    }

    /// Discard the in-progress rename (Esc).
    pub(crate) fn cancel_rename(&mut self, shared: &Shared) {
        if self.editing_tab.take().is_some() {
            self.edit_buffer.clear();
            self.sync_layout(shared);
        }
    }

    /// Route a key into the inline tab-rename editor.
    pub(crate) fn rename_key(&mut self, shared: &Shared, key: &Key) {
        match key {
            Key::Named(NamedKey::Enter) => self.commit_rename(shared),
            Key::Named(NamedKey::Escape) => self.cancel_rename(shared),
            Key::Named(NamedKey::Backspace) => {
                self.edit_buffer.pop();
                self.sync_layout(shared);
            }
            Key::Named(NamedKey::Space) => {
                self.edit_buffer.push(' ');
                self.sync_layout(shared);
            }
            Key::Character(s) => {
                for c in s.chars().filter(|c| !c.is_control()) {
                    self.edit_buffer.push(c);
                }
                self.sync_layout(shared);
            }
            _ => {}
        }
    }

    /// Focus the pane backing `sid` in the active tab (click-to-focus). No-op if it
    /// is already focused or the session isn't in this tab.
    pub(crate) fn focus_pane_of_session(&mut self, shared: &Shared, sid: &SessionId) {
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
            self.sync_layout(shared);
        }
    }

    /// Map a logical-px point to `(session, row, col)` in whichever visible pane
    /// contains it (clamped to that pane's grid), or `None` if outside all panes.
    pub(crate) fn pixel_to_cell(&self, x: f64, y: f64) -> Option<(SessionId, u16, u16)> {
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

    /// The platform's link-open modifier: Cmd on macOS, Ctrl elsewhere. Used
    /// only inside mouse-reporting panes, where plain clicks belong to the app.
    pub(crate) fn open_modifier_held(&self) -> bool {
        if cfg!(target_os = "macos") {
            self.modifiers.super_key()
        } else {
            self.modifiers.control_key()
        }
    }

    /// The link under the pointer, when opening/hovering is eligible: always
    /// at a plain prompt; only with the open-modifier inside mouse-reporting
    /// panes (there, plain clicks belong to the app).
    pub(crate) fn link_under_cursor(&self) -> Option<(SessionId, u32, String, u16, u16)> {
        let (x, y) = self.cursor;
        let (sid, row, col) = self.pixel_to_cell(x, y)?;
        if self.renderer.pane_modes(&sid).mouse_reporting && !self.open_modifier_held() {
            return None;
        }
        let (id, url) = self.renderer.link_at(&sid, row, col)?;
        Some((sid, id, url.to_string(), row, col))
    }

    /// Begin a selection at a pane-body point; click count picks the mode
    /// (1 = cell, 2 = word, 3 = line).
    pub(crate) fn begin_selection(&mut self, shared: &Shared, x: f64, y: f64) {
        let Some((sid, row, col)) = self.pixel_to_cell(x, y) else {
            self.clear_selection();
            return;
        };
        // Clicking into a pane focuses it (also correct for single-pane selection:
        // a selection is single-pane, so the click target must be the focused pane).
        self.focus_pane_of_session(shared, &sid);
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
    pub(crate) fn extend_selection(&mut self, x: f64, y: f64) {
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
    pub(crate) fn clear_selection(&mut self) {
        if self.sel.is_some() {
            self.sel = None;
            self.selecting = false;
            self.renderer.set_selection(None);
        }
    }

    /// Copy the current selection's text to the OS clipboard (Cmd+C).
    pub(crate) fn copy_selection(&mut self, shared: &mut Shared) {
        if let Some(text) = self.renderer.selected_text() {
            shared.platform.set_clipboard(&text);
        }
    }

    /// Paste the OS clipboard into the focused pane's PTY (Cmd+V), bracketed when
    /// the focused app enabled bracketed-paste mode.
    pub(crate) fn paste_clipboard(&mut self, shared: &mut Shared) {
        if let Some(text) = shared.platform.clipboard() {
            self.paste_into_focused(shared, &text);
        }
    }

    /// Toggle the cheat-sheet overlay (Cmd+/ and the Help menu item).
    pub(crate) fn toggle_help(&mut self) {
        if self.help {
            self.hide_help();
        } else {
            self.show_help();
        }
    }

    /// Hide the cheat-sheet overlay (no-op if not shown).
    pub(crate) fn hide_help(&mut self) {
        if self.help {
            self.help = false;
            self.renderer.set_help(None);
        }
    }

    /// Show the About overlay (closing other overlays — they're exclusive).
    pub(crate) fn show_about(&mut self) {
        self.hide_help();
        self.hide_settings();
        self.about = true;
        self.about_since = Instant::now();
        self.renderer.set_about(Some(about_info()));
    }

    /// Hide the About overlay (no-op if not shown).
    pub(crate) fn hide_about(&mut self) {
        if self.about {
            self.about = false;
            self.renderer.set_about(None);
        }
    }

    /// Toggle the About overlay (the Ember → About Ember menu item).
    pub(crate) fn toggle_about(&mut self) {
        if self.about {
            self.hide_about();
        } else {
            self.show_about();
        }
    }

    pub(crate) fn close_hits_running(&self, shared: &Shared, kind: PendingClose) -> bool {
        match kind {
            PendingClose::Quit => shared.sessions.values().any(|h| h.is_busy()),
            PendingClose::Pane => self
                .active_tab()
                .root
                .session_of(self.active_tab().focus)
                .is_some_and(|s| shared.session_busy(s)),
            PendingClose::Tab(tab) => self
                .tree
                .tabs
                .iter()
                .find(|t| t.id == tab)
                .is_some_and(|t| t.root.leaves().iter().any(|(_, s)| shared.session_busy(s))),
            // Scoped to THIS window's own sessions, unlike `Quit` — closing one
            // window must not be blocked by a busy pane in a DIFFERENT window.
            PendingClose::CloseWindow => self
                .window_session_ids()
                .iter()
                .any(|s| shared.session_busy(s)),
        }
    }

    /// Run a close, or defer it behind a confirmation if it would kill a running
    /// command. Returns true if the app should exit now.
    pub(crate) fn request_close(&mut self, shared: &mut Shared, kind: PendingClose) -> bool {
        if self.close_hits_running(shared, kind) {
            self.show_close_confirm(kind);
            return false;
        }
        self.do_close(shared, kind)
    }

    /// Actually perform a (possibly confirmed) close. Returns true to exit.
    pub(crate) fn do_close(&mut self, shared: &mut Shared, kind: PendingClose) -> bool {
        match kind {
            PendingClose::Pane => {
                self.close_focused(shared);
                self.tree.tabs.is_empty()
            }
            PendingClose::Tab(tab) => self.do_close_tab(shared, tab),
            PendingClose::Quit => true,
            // The actual teardown (killing this window's sessions, dropping
            // its `WindowState`/OS window) needs `App`-level access this
            // method doesn't have — the caller does it via `close_window`/
            // `finish_close` once this returns true.
            PendingClose::CloseWindow => true,
        }
    }

    /// Show the running-process confirmation (reuses the help-overlay panel).
    pub(crate) fn show_close_confirm(&mut self, kind: PendingClose) {
        self.hide_help();
        self.hide_about();
        self.hide_settings();
        self.pending_close = Some(kind);
        self.confirm_focus = 0; // Cancel is the safe default.
        self.update_confirm_view();
    }

    /// (Re)build the confirm modal from `pending_close` + `confirm_focus`.
    pub(crate) fn update_confirm_view(&mut self) {
        let Some(kind) = self.pending_close else {
            return;
        };
        let (title, confirm_label) = match kind {
            PendingClose::Pane => ("Close this pane?", "Close"),
            PendingClose::Tab(_) => ("Close this tab?", "Close"),
            PendingClose::Quit => ("Quit Ember?", "Quit"),
            PendingClose::CloseWindow => ("Close this window?", "Close"),
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
    pub(crate) fn resolve_confirm(&mut self, shared: &mut Shared, confirm: bool) -> bool {
        let Some(kind) = self.pending_close.take() else {
            return false;
        };
        self.renderer.set_confirm(None);
        if confirm {
            self.do_close(shared, kind)
        } else {
            false
        }
    }

    /// Dismiss whichever modal overlay is open; returns whether one was showing.
    pub(crate) fn dismiss_overlay(&mut self) -> bool {
        let shown = self.help || self.about || self.settings_open;
        self.hide_help();
        self.hide_about();
        self.hide_settings();
        shown
    }

    /// Show the Settings overlay (closing other overlays — they're exclusive).
    pub(crate) fn show_settings(&mut self, shared: &Shared) {
        self.hide_help();
        self.hide_about();
        self.settings_open = true;
        let rows = shared.settings_rows();
        self.ensure_settings_sel_selectable(&rows);
        self.renderer.set_settings(Some((rows, self.settings_sel)));
    }

    /// Hide the Settings overlay (no-op if not shown).
    pub(crate) fn hide_settings(&mut self) {
        if self.settings_open {
            self.settings_open = false;
            self.renderer.set_settings(None);
        }
    }

    /// Toggle the FPS/frame-time debug overlay (Cmd+Shift+P / `ctl fps`).
    pub(crate) fn toggle_fps(&mut self) {
        self.fps_overlay = !self.fps_overlay;
        if !self.fps_overlay {
            self.renderer.set_fps_overlay(None);
        }
        self.renderer.window().request_redraw();
    }

    /// Toggle the Settings overlay (Ember → Settings… / Cmd+,).
    pub(crate) fn toggle_settings(&mut self, shared: &Shared) {
        if self.settings_open {
            self.hide_settings();
        } else {
            self.show_settings(shared);
        }
    }

    /// Re-push the Settings rows + selection to the renderer after a change.
    pub(crate) fn refresh_settings(&mut self, shared: &Shared) {
        let rows = shared.settings_rows();
        self.renderer.set_settings(Some((rows, self.settings_sel)));
    }

    /// If `settings_sel` doesn't point at a selectable row (a `SectionHeader`,
    /// or out of bounds), snap it to the first selectable row. Guards the
    /// overlay's initial open (it starts at index 0, which is always a
    /// header) and any future row-table change.
    pub(crate) fn ensure_settings_sel_selectable(&mut self, rows: &[SettingsRowView]) {
        let invalid = match rows.get(self.settings_sel) {
            Some(r) => r.kind == RowKind::SectionHeader,
            None => true,
        };
        if invalid {
            self.settings_sel = rows
                .iter()
                .position(|r| r.kind != RowKind::SectionHeader)
                .unwrap_or(0);
        }
    }

    /// Handle a key while the Settings overlay is open: navigate + change values.
    pub(crate) fn settings_key(&mut self, shared: &mut Shared, key: &Key) {
        let rows = shared.settings_rows();
        match key {
            Key::Named(NamedKey::Escape) => {
                self.hide_settings();
                return;
            }
            Key::Named(NamedKey::ArrowUp) => {
                self.settings_sel = step_selectable_row(&rows, self.settings_sel, -1);
            }
            Key::Named(NamedKey::ArrowDown) => {
                self.settings_sel = step_selectable_row(&rows, self.settings_sel, 1);
            }
            Key::Named(NamedKey::ArrowRight) | Key::Named(NamedKey::Space) => {
                self.adjust_setting(shared, 1.0)
            }
            Key::Named(NamedKey::ArrowLeft) => self.adjust_setting(shared, -1.0),
            _ => {}
        }
        self.refresh_settings(shared);
    }

    /// Change the selected setting by `dir` (+1 / -1) via its own row's
    /// `adjust` fn — the row table *is* the dispatch, there is no positional
    /// match here to drift out of sync with it. Persists the config, then
    /// re-applies every live side effect unconditionally: backdrop
    /// appearance, font size, font family, and the developer-mode control
    /// socket. Each is already a cheap no-op when its target value hasn't
    /// changed (matching `zoom_to`'s existing no-op-if-unchanged pattern),
    /// so this never needs to know which row actually fired.
    pub(crate) fn adjust_setting(&mut self, shared: &mut Shared, dir: f32) {
        if let Some(row) = setting_rows().get(self.settings_sel) {
            if let Some(adjust) = row.adjust {
                adjust(&mut shared.config, dir);
            }
        }
        if let Err(e) = config::save(&shared.config) {
            eprintln!("[ember] config save failed: {e}");
        }
        self.apply_appearance(shared);
        shared.set_developer_mode(shared.config.developer_mode);
        let mut relayout = self.renderer.set_font_size(shared.config.font.size);
        relayout |= self.renderer.set_family(shared.config.font.family.clone());
        if relayout {
            self.sync_layout(shared);
        }
    }

    /// Push the current config's appearance (campfire backdrop + ember sparks) to
    /// the renderer. Called on startup and whenever a setting changes. Decodes the
    /// backdrop image only when the configured path changes (cheap on idle changes).
    pub(crate) fn apply_appearance(&mut self, shared: &Shared) {
        let t = shared.backdrop_since.elapsed().as_secs_f32();
        let params = shared.backdrop_params(t);
        self.renderer.set_backdrop(params);

        let want = shared.config.background.image.clone();
        if want != self.image_loaded {
            let fit = ImageFit::parse(&shared.config.background.image_fit);
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

    /// Advance every active animation to wall-clock time `now`: the About glow, the
    /// ember sparks, and the visual-bell flash decay. Each `set_*` is a function of
    /// elapsed time (not a delta), so an occasional long gap between frames just
    /// samples the curve later — no jump. Called from the loop once per frame-interval.
    pub(crate) fn advance_animations(&mut self, shared: &Shared, now: Instant) {
        if self.about {
            let t = now.duration_since(self.about_since).as_secs_f32();
            self.renderer.set_about_anim(ember_glow(t), t);
        }
        if self.backdrop_animating(shared) {
            let params =
                shared.backdrop_params(now.duration_since(shared.backdrop_since).as_secs_f32());
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
    pub(crate) fn backdrop_animating(&self, shared: &Shared) -> bool {
        shared.config.background.ember_sparks
            && !self.occluded
            && !self.help
            && !self.about
            && !self.settings_open
    }

    /// Handle a BEL from `session` (visual bell): start/refresh the ember flash,
    /// and if the belling tab isn't active, mark it with an unseen-bell indicator.
    pub(crate) fn on_bell(&mut self, shared: &Shared, session: &SessionId) {
        if !shared.config.visual_bell {
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
                    self.sync_layout(shared); // repaint the tab with its bell dot
                }
            }
        }
        // Start (or refresh) the window flash; the animation loop decays it.
        self.bell_flash_since = Some(Instant::now());
        self.renderer.set_bell_flash(bell_flash_intensity(0.0));
    }

    /// Whether the visual-bell ember flash is currently animating.
    pub(crate) fn bell_flashing(&self) -> bool {
        self.bell_flash_since.is_some()
    }

    /// Live-zoom the terminal font by `delta` points (Cmd +/-).
    pub(crate) fn zoom_by(&mut self, shared: &Shared, delta: f32) {
        let target = self.renderer.font_size() + delta;
        self.zoom_to(shared, target);
    }

    /// Set the terminal font to `size` and re-layout (the cell size, hence every
    /// pane's grid dims, changed). No-op if the size didn't change.
    pub(crate) fn zoom_to(&mut self, shared: &Shared, size: f32) {
        if self.renderer.set_font_size(size) {
            self.sync_layout(shared);
        }
    }

    /// Jump to tab `n` (1-based); no-op if out of range.
    pub(crate) fn select_tab(&mut self, shared: &Shared, n: usize) {
        if n >= 1 && n <= self.tree.tabs.len() {
            self.tree.active = n - 1;
            self.sync_layout(shared);
        }
    }

    /// Bring the window to the front and give it focus (`ctl raise`, and the
    /// tail of `ctl focus` — a Stream Deck press should land the user IN
    /// Ember, not silently switch a background window's tab).
    pub(crate) fn raise_window(&self) {
        let w = self.renderer.window();
        w.set_minimized(false);
        w.focus_window();
    }

    /// Close the pane backing `session` wherever it lives (a shell exited, or a
    /// background tab's pane was closed). Switches to that tab so `ClosePane`'s
    /// active-tab semantics apply, then restores a sane active index. Returns
    /// `true` when this call left the window's tree empty (mirrors
    /// `do_close_tab`'s contract) — the caller MUST then tear this window down
    /// (e.g. via `finish_close`): unlike a tab closed from the keyboard, this is
    /// reached from the exited-shell drain, which can route to a BACKGROUND
    /// window just as easily as the focused one, and nothing else notices an
    /// emptied background tree on its own.
    pub(crate) fn close_session(&mut self, shared: &mut Shared, session: &SessionId) -> bool {
        let found = self.tree.tabs.iter().enumerate().find_map(|(ti, tab)| {
            tab.root
                .leaves()
                .into_iter()
                .find(|(_, s)| s == session)
                .map(|(pane, _)| (ti, pane))
        });
        let Some((ti, pane)) = found else {
            // Not in the layout (already removed); just clean up the backend.
            self.kill_session(shared, session);
            return self.tree.tabs.is_empty();
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
        self.apply_effects(shared, effects);
        if self.tree.tabs.is_empty() {
            return true;
        }
        // Restore the user's tab if it still exists (it may have shifted index,
        // or been the very tab whose last pane just closed).
        self.tree.active = user_tab
            .and_then(|id| self.tree.tabs.iter().position(|t| t.id == id))
            .unwrap_or(self.tree.active)
            .min(self.tree.tabs.len() - 1);
        self.sync_layout(shared);
        false
    }

    /// Perform a tab close (after any confirmation). Returns true when this
    /// window's tree is now empty — the caller (`finish_close`) decides
    /// whether that means quitting the whole app (last window) or just
    /// closing this one. Doesn't itself call `shutdown_all`: `apply_effects`
    /// above already killed every session THIS tab held via `KillSession`
    /// effects, and a global `shutdown_all` here would also kill every OTHER
    /// window's still-running sessions.
    pub(crate) fn do_close_tab(&mut self, shared: &mut Shared, tab: TabId) -> bool {
        let user_tab = self.tree.tabs.get(self.tree.active).map(|t| t.id);
        let vp = self.viewport();
        let effects = apply(&mut self.tree, LayoutCommand::CloseTab { tab }, vp);
        self.apply_effects(shared, effects);
        if self.tree.tabs.is_empty() {
            return true;
        }
        self.tree.active = user_tab
            .and_then(|id| self.tree.tabs.iter().position(|t| t.id == id))
            .unwrap_or(self.tree.active)
            .min(self.tree.tabs.len() - 1);
        self.sync_layout(shared);
        false
    }
}
