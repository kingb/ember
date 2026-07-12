//! Per-window state (`WindowState`) split out of the old monolithic `RunState`.
//!
//! Field classification (behavior-identical split; see `Shared` in `main.rs`):
//!
//! - **`WindowState`** (this file) — everything tied to one window/surface:
//!   `renderer`, `tree`, `px`, `dims_cache`, `modifiers`, `cursor`,
//!   `pointer_cursor`, selection (`sel`/`selecting`/`last_click`/`click_count`),
//!   drags (`tab_drag`/`divider_drag`/`scrollbar_drag`/`split_preview`/`hold`),
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
use std::time::{Duration, Instant};

use ember_core::{
    Axis, BackendControl, BackendHandle, Direction, DropZone, GridDims, LayoutCommand,
    LayoutEffect, PaneId, Rect, RowKind, ScrollAmount, SessionBackend, SessionId, SettingsRowView,
    SparksMode, SurfaceDest, SurfaceRef, Tab, TabId, apply, drop_zone_for, layout, remove_pane,
    setting_rows,
};
use ember_platform::PlatformBackend;
use ember_render::{
    AbsPoint, AnchoredSelection, ConfirmView, ImageFit, Point, Renderer, SelectionMode, TabHit,
    TabLabel, VisiblePane,
};
use ember_session::{LocalPty, LocalPtyConfig};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{CursorIcon, WindowId};

use crate::config;
use crate::control::ControlMsg;
use crate::{
    ControlClose, DEFAULT_COLS, DEFAULT_ROWS, DragState, DropHover, MULTI_CLICK, PAD, PendingClose,
    Shared, about_info, bell_flash_intensity, bracket_paste, dims_for_rect, ember_glow, encode_key,
    help_lines, inset, load_backdrop_image, named_key, parse_chord, resolve_window_index,
    shell_escape_path, step_selectable_row, tab_display_title, url_is_openable,
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
    /// The `TabId` of the tab that was active immediately BEFORE this press
    /// (before the press's own `select_tab` switched to the pressed tab).
    /// Restored as the displayed tab the moment a drag tears off the strip
    /// band, so an in-window pane drop (this task's only wired destination)
    /// targets some OTHER tab's pane, not the dragged tab's own —
    /// `move_surface` always rejects a tab dropped into itself ("no-op: tab
    /// can't merge into itself"), and that other tab can only be
    /// hit-testable if it's the one actually rendered underneath the lifted
    /// tab. Deliberately an ID, not the index captured at press time: a
    /// live in-strip reorder (this same drag, before it tears off) can
    /// shift which tab sits at that original INDEX, so the index alone
    /// would go stale — it's re-resolved against the current tab order at
    /// tear-off time.
    origin_tab: TabId,
}

// --- Hold-to-wisp (v1.1, docs/design/2026-07-07-surface-drag-wisp-design.md
// §"Hold-to-wisp") — press-and-hold on a pane body, no modifier chord: after
// `HOLD_ARM_MS` a thin ring sweeps clockwise over `HOLD_SWEEP_MS`, and
// completing it tears the pane off exactly like the chord-gated drag.
// Starting numbers from the design doc; all three tunable live by eye.

/// Delay (ms), from press, before the ring starts sweeping.
const HOLD_ARM_MS: u64 = 300;
/// Sweep duration (ms) once armed — the hold completes at `HOLD_ARM_MS +
/// HOLD_SWEEP_MS` and the drag goes live.
const HOLD_SWEEP_MS: u64 = 600;
/// How far (logical px) the pointer may drift from the press origin before
/// an ARMING hold (ring not yet visible) cancels — this phase is what
/// distinguishes "I'm holding" from "I'm starting a drag/selection", so it
/// stays modest. Trackpad fingers drift; 6 was too tight in the first live
/// test (every hold decayed into a selection).
const HOLD_TOLERANCE_PX: f64 = 12.0;
/// Drift allowance once the ring is visibly SWEEPING: the user is clearly
/// committed to the gesture by then, so the leash is much longer — only a
/// real yank away cancels.
const HOLD_SWEEP_TOLERANCE_PX: f64 = 28.0;

/// How far (logical px) the pointer must move horizontally before a tab press
/// becomes a drag-reorder rather than a click.
const TAB_DRAG_THRESHOLD: f64 = 6.0;

/// Spring-loaded tab select (finding #2, macOS-Finder-folder style): how long
/// (ms) a live drag must dwell over a strip tab's own chip before that tab
/// becomes the DISPLAYED tab (`select_tab`) — long enough that skating
/// across the strip toward a target doesn't thrash the display, short enough
/// to feel like hover-navigation. Fixes the "tear tab 3, drop as pane into
/// tab 1 lands unpredictably" live finding: the drop target used to be
/// whatever revert tab happened to be displayed at tear-off; now the user
/// NAVIGATES — dwell on tab 1's chip, it becomes visible, move down into
/// its pane, drop exactly there.
const SPRING_LOAD_MS: u64 = 150;

/// How far (logical px) the pointer must move BELOW the tab strip's bottom
/// edge before an in-strip tab drag tears off into a [`crate::DragState`]
/// (a surface drag capable of leaving the strip — a pane drop this task, a
/// desktop/cross-window drop in later ones).
const TEAR_OFF_THRESHOLD: f64 = 24.0;

/// Pure band-exit check for tab tear-off: has the pointer moved far enough
/// below the strip's bottom edge (`strip_bottom`, logical px) to convert an
/// in-strip reorder into a tear-off? All three arguments are logical px;
/// `threshold` is normally [`TEAR_OFF_THRESHOLD`] (a parameter so this is
/// unit-testable independent of that constant, and so a caller can probe the
/// boundary exactly). Strictly-greater: a pointer sitting exactly on the
/// threshold is still "in the strip" (matches every other edge-band
/// convention in this codebase — see `ember_core::drop_zone_for`'s doc).
pub(crate) fn strip_band_exit(pointer_y: f64, strip_bottom: f64, threshold: f64) -> bool {
    pointer_y - strip_bottom > threshold
}

/// The display-revert target at tear-off, when the pre-press origin tab
/// (whatever was active before this press — see [`TabDrag::origin_tab`]'s
/// doc) can't be found by id anymore, e.g. something closed it while the
/// drag was in flight. The revert's only job is to show a tab OTHER than
/// `dragged` (so an in-window pane drop has a different tab's pane to
/// target — `move_surface` always rejects a tab dropped into its own pane),
/// so any surviving tab does: the nearest one BY INDEX to `dragged` itself.
/// `None` only when `dragged` is the sole survivor (`n_tabs <= 1`) — nothing
/// else to revert to, and a drop-cancel is the correct, unchanged outcome
/// there (every drop self-rejects, same as before this fallback existed).
pub(crate) fn revert_target_tab(n_tabs: usize, dragged: usize) -> Option<usize> {
    if n_tabs <= 1 {
        return None;
    }
    Some(if dragged == 0 { 1 } else { dragged - 1 })
}

/// Which [`SurfaceRef`] a chord-gated pane-body press should carry: the
/// pressed pane itself, unless its tab has exactly one pane — dragging that
/// pane out would empty the tab, so the whole tab becomes the dragged
/// surface instead (the design spec's sole-pane-tab rule, chosen up front
/// rather than surfacing `ember_core::move_surface`'s `WouldEmptyTab` error
/// after the fact: a single-pane tab drag should just feel like a tab
/// drag). Pure: no window/session state, so unit-testable directly.
pub(crate) fn pane_drag_source(
    window: usize,
    tab: usize,
    pane: PaneId,
    panes_in_tab: usize,
) -> SurfaceRef {
    if panes_in_tab <= 1 {
        SurfaceRef::Tab { window, tab }
    } else {
        SurfaceRef::Pane { window, tab, pane }
    }
}

/// Map a resolved [`DropZone`] hover to the [`SurfaceDest`] `resolve_drag_drop`
/// stages, given the hovered window's INDEX (already resolved from its
/// `WindowId` by the caller) and the hovered pane's `(tab, pane)`. Pure: no
/// window/session state — the "drop → dest" mapping release 2 builds on,
/// shared by same-window and cross-window pane drops alike (see
/// `resolve_drag_drop`'s doc for why those two cases merged into one call
/// site here).
pub(crate) fn drop_zone_to_dest(
    zone: DropZone,
    window: usize,
    tab: usize,
    pane: PaneId,
) -> SurfaceDest {
    match zone {
        DropZone::Edge { axis, before } => SurfaceDest::SplitInto {
            window,
            tab,
            pane,
            axis,
            before,
        },
        DropZone::Center => SurfaceDest::NewTab { window },
    }
}

/// Whether hovering `pane` in tab `hovered_tab` of a drag's OWN SOURCE
/// window would resolve to one of `move_surface`'s no-op self-merge
/// rejections — mirrors `windows.rs`'s `validate` exactly (window equality
/// is already guaranteed by every caller, which only ever checks a hover on
/// the drag's own source window):
/// - a `Tab`-sourced drag over any pane of its OWN tab ("tab can't merge
///   into itself" — any pane within that tab is an equally dead drop, not
///   just its own historical rect).
/// - a `Pane`-sourced drag over its own exact pane ("split into self").
///   Structurally rare in practice (a `Pane` exclusion removes that pane's
///   own rect from `pane_rects`, so it's not normally hoverable), but
///   checked anyway so this predicate matches `validate`'s conditions
///   exactly rather than relying on that staying true forever.
pub(crate) fn hover_is_self_merge(
    surface: SurfaceRef,
    hovered_tab: usize,
    hovered_pane: PaneId,
) -> bool {
    match surface {
        SurfaceRef::Tab { tab, .. } => tab == hovered_tab,
        SurfaceRef::Pane { tab, pane, .. } => tab == hovered_tab && pane == hovered_pane,
    }
}

/// The new window's SCREEN-space top-left `(x, y)`, physical px, for a
/// desktop drag drop: the drop point (`screen`) minus the grab offset
/// (`grab_logical`, captured at tear-off in the SOURCE window's logical px —
/// see `DragState::grab`'s doc) converted to physical via `scale`, clamped
/// to never go negative. A deliberately simple "stay roughly on-screen"
/// heuristic, not full multi-monitor-aware clamping (querying real monitor
/// geometry needs `ActiveEventLoop`, not available at this call site) —
/// noted honestly rather than half-implemented.
pub(crate) fn desktop_drop_position(
    screen: (f64, f64),
    grab_logical: (f64, f64),
    scale: f64,
) -> (i32, i32) {
    let (sx, sy) = screen;
    let (gx, gy) = grab_logical;
    (
        (sx - gx * scale).max(0.0).round() as i32,
        (sy - gy * scale).max(0.0).round() as i32,
    )
}

/// What a drag/reorder gesture resolved to on release — `ctl drag`'s
/// `drag_ended` reply field, and the return of [`WindowState::left_release`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DragEnded {
    /// An in-strip tab reorder (whether it stayed in the strip the whole
    /// time, or tore off and was dropped back on the strip).
    Reorder,
    /// A tear-off resolved to an in-window (or, in a later task,
    /// cross-window) surface move.
    Move,
    /// A tear-off with no valid drop target, or an explicit cancel
    /// (Escape / `--cancel`): zero mutation from the tear-off onward.
    Cancel,
    /// A text selection drag ended (no tab/surface drag was in progress).
    Selection,
    /// Nothing was in progress (a plain click, or a release with no
    /// preceding drag/selection state at all).
    None,
}

impl DragEnded {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            DragEnded::Reorder => "reorder",
            DragEnded::Move => "move",
            DragEnded::Cancel => "cancel",
            DragEnded::Selection => "selection",
            DragEnded::None => "none",
        }
    }
}

/// Ctrl+Opt split drop-zone preview: the hovered pane + the split that a click
/// would commit (new pane on the right if `horizontal`, else the bottom).
pub(crate) struct SplitPreview {
    pane: PaneId,
    horizontal: bool,
    ratio: f32,
}

/// What a live [`Hold`] is armed on — a pane body (the original v1.1 gesture)
/// or a tab-strip chip (this task's addition: press-and-hold a tab tears it
/// off exactly like a pane hold does, reusing the tab-strip's own tear-off
/// path rather than `start_pane_drag`'s).
#[derive(Clone, Copy)]
enum HoldTarget {
    Pane {
        pane: PaneId,
        /// The pressed pane's own rect (logical px, this window's space) at
        /// press time — carried through to `start_pane_drag` unchanged.
        rect: Rect,
    },
    /// Index of the pressed tab (this window's `tree.tabs`, stable across
    /// the hold: nothing reorders tabs while a plain press-and-hold, as
    /// opposed to a live drag, is in progress).
    Tab { tab: usize },
}

/// A press-and-hold in progress on a pane body or a tab-strip chip
/// (hold-to-wisp, v1.1 + this task's tab extension): timing + identity
/// needed to draw the ring and, on completion, tear the target off into a
/// carried drag — [`WindowState::start_pane_drag`] for a pane,
/// [`WindowState::tear_off_tab`] for a tab — exactly as the chord gesture
/// (pane) or a motion-driven tear-off (tab) already do. Armed alongside
/// whatever the press already does (selection / mouse-mode forward for a
/// pane; tab select for a tab) — see [`WindowState::left_click`]'s doc.
struct Hold {
    target: HoldTarget,
    /// Press origin (logical px) — both the ring's center and the point
    /// motion is measured from for [`HOLD_TOLERANCE_PX`] cancellation.
    origin: (f64, f64),
    started: Instant,
}

/// What [`WindowState::tick_hold`] found this tick — `about_to_wait` folds
/// the result into its own animation pacing.
pub(crate) enum HoldTick {
    /// No hold in progress on this window.
    Idle,
    /// Armed, not yet sweeping (still inside `HOLD_ARM_MS`) — no ring drawn
    /// yet, so nothing to redraw; fold this deadline into `next_wake`.
    Waiting(Instant),
    /// The ring was just advanced and pushed to the renderer (which requests
    /// its own redraw) — fold `now + ANIM_FRAME` into `next_wake`.
    Sweeping,
    /// The ring just completed: `shared.drag` is now `Some`, already
    /// `carried = true` (design: "wisp visible immediately"). The caller
    /// must give the wisp the one-tick nudge a real motion tick would have
    /// via `update_cross_window_drag` — a completed hold has no pointer
    /// motion of its own to piggyback on.
    Completed,
}

// --- Suck-in/pour-out morph (v0.4.0, docs/design/2026-07-07-surface-drag-
// wisp-design.md's "v1 trims": "the suck-in/pour-out is an intensity fade
// rather than a rect morph" — this is that follow-up). SOURCE window plays a
// suck-in at tear-off; the TARGET (or, on cancel/reorder, the SOURCE itself —
// "the surface went home") plays a pour-out at resolution.

/// Duration of the suck-in morph (tear-off) — design doc "~150 ms".
const SUCK_IN_MS: u64 = 150;
/// Duration of the pour-out morph (drop/cancel) — design doc "~200 ms".
const POUR_OUT_MS: u64 = 200;
/// Redraw cadence while a morph is live — matches every other animation
/// pacer in this file (`ANIM_FRAME` in `main.rs`, `HOLD_SWEEP`'s implicit
/// per-tick redraw).
const MORPH_FRAME: Duration = Duration::from_millis(16);

/// A live suck-in/pour-out morph on this window's renderer: timing +
/// geometry needed to advance `Renderer::set_morph` every tick, mirroring
/// [`Hold`]'s shape exactly. Self-terminating (see [`WindowState::
/// tick_morph`]) — NOT part of `clear_drag_visuals`'s sweep.
struct MorphAnim {
    started: Instant,
    duration: Duration,
    /// The surface's own rect, logical px, THIS window's space.
    rect: Rect,
    /// The suck/pour point (grab or drop/cancel point), logical px, THIS
    /// window's space.
    grab: (f64, f64),
    /// `true` = suck-in (collapsing toward `grab`), `false` = pour-out
    /// (expanding from `grab`) — see `ember_render::Renderer::set_morph`'s doc.
    inward: bool,
}

/// What [`WindowState::tick_morph`] found this tick — `about_to_wait` folds
/// the result into its own animation pacing, mirroring [`HoldTick`].
pub(crate) enum MorphTick {
    /// No morph in progress on this window (the overwhelmingly common case).
    Idle,
    /// The morph was just advanced and pushed to the renderer (which
    /// requests its own redraw); fold `deadline` into `next_wake`.
    Running(Instant),
}

// --- Carry-time source vanish (docs/design/2026-07-07-surface-
// drag-wisp-design.md's "sucked into a wisp… carry it anywhere"): the design
// says the carried surface leaves its SOURCE the instant the suck-in
// finishes — not just the momentary collapse animation, the pane/tab/window
// itself stops being on-screen there until a drop lands it (or a cancel
// pours it back). Purely a rendering-time exclusion: `self.tree` is NEVER
// touched by this — a drop is still exactly one `move_surface` call, a
// cancel is still zero tree mutation.

/// What a torn-off drag visually excludes from THIS (source) window's own
/// render, once its suck-in has finished. Set alongside the tear-off
/// (`start_pane_drag`/`update_drag`'s tab-tear-off branch), cleared the
/// instant the drag resolves ([`WindowState::clear_carried_exclusion`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CarriedExclusion {
    /// A single pane, still part of a multi-pane tab: `sync_layout`
    /// computes rects on a CLONED, pane-removed tree
    /// ([`ember_core::remove_pane`]) so the sibling visually re-flows into
    /// the freed space.
    Pane(PaneId),
    /// A whole tab, in a window that has others: hidden from the strip's
    /// `TabLabel` list — the revealed/active tab (already switched at
    /// tear-off, see `update_drag`'s `origin_tab`/`revert_target_tab`
    /// handling) keeps rendering normally underneath.
    Tab(TabId),
    /// The dragged tab IS this window's only tab: excluding it would leave
    /// an empty strip/viewport, so instead the WHOLE OS window hides
    /// (`set_visible(false)`) — see [`WindowState::apply_carried_exclusion`]'s
    /// doc for why this one case additionally waits for `carried`.
    WholeWindow,
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
    /// Active text selection + the session (pane) it belongs to. Anchored to
    /// absolute scrollback lines so it stays glued to its text as output
    /// scrolls (projected into the viewport at paint time).
    pub(crate) sel: Option<(SessionId, AnchoredSelection)>,
    /// The selected text captured at the last selection change (its rows were
    /// visible then). Copy uses this, so it stays correct even after the
    /// selected text scrolls out of the viewport.
    pub(crate) sel_snapshot: Option<String>,
    /// Whether a mouse drag is currently extending the selection.
    pub(crate) selecting: bool,
    /// Scrollback-search bar (Cmd+F): open flag + the live query text.
    pub(crate) search_open: bool,
    pub(crate) search_query: String,
    /// Latest match position for the bar's "i / N" readout: `Some((i, n))`
    /// (n == 0 means "no matches"), or `None` while a search is in flight.
    pub(crate) search_count: Option<(u32, u32)>,
    /// IME composition in progress (preedit text); non-empty = composing,
    /// during which raw key events are suppressed (they belong to the IME).
    pub(crate) ime_preedit: String,
    /// Command palette (Cmd+Shift+P): open flag, query, selected row index
    /// (into the CURRENT filtered list).
    pub(crate) palette_open: bool,
    pub(crate) palette_query: String,
    pub(crate) palette_sel: usize,
    /// Last mouse-down (time, pane, cell), for double/triple-click detection.
    pub(crate) last_click: Option<(Instant, SessionId, u16, u16)>,
    /// Consecutive-click count at the same cell (1 = simple, 2 = word, 3 = line).
    pub(crate) click_count: u32,
    /// In-progress tab drag-reorder: the tab being dragged, the press x (logical),
    /// and whether the drag threshold has been crossed (below it, it's a click).
    pub(crate) tab_drag: Option<TabDrag>,
    /// In-progress scrollbar-thumb drag: the session whose scrollbar is grabbed.
    pub(crate) scrollbar_drag: Option<SessionId>,
    /// In-progress divider drag to resize a split: `(a-side pane, b-side pane,
    /// split axis, last cursor position along that axis in logical px)`. Both
    /// flanking panes are carried (not just the a-side) because a divider is
    /// only unambiguously identified by the pair — see `divider_at`.
    pub(crate) divider_drag: Option<(PaneId, PaneId, Axis, f64)>,
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
    /// A drag drop `resolve_drag_drop` resolved to an in-window surface move,
    /// awaiting application. Set instead of applying it directly — the
    /// canonical apply path is `apply_move` in `main.rs`, which needs
    /// `&mut HashMap<WindowId, WindowState>`/`&ActiveEventLoop`, neither
    /// reachable from a `WindowState` method — so `left_release`'s caller
    /// (the real `WindowEvent::MouseInput` release handler, or `ctl drag`'s
    /// `run_ctl_drag`) drains this immediately after and runs it through
    /// `apply_move`, exactly like every other surface-mobility gesture
    /// (`move-tab`/`promote-pane`/`merge-tab`). `None` outside that one-tick
    /// window; nothing else should read or hold onto it.
    pub(crate) pending_move: Option<(SurfaceRef, SurfaceDest)>,
    /// A live cross-window drag currently targeting THIS window (release 2
    /// task 3) — set by `App::update_cross_window_drag` (`main.rs`) when
    /// this window's frame is under the carried pointer, cleared the instant
    /// hover moves elsewhere. Drives this window's own preview visuals
    /// (`set_incoming_drop`); `None` on the drag's source window itself
    /// (which shows the lifted tab chip via `update_drag_hover` instead) and
    /// on every window outside the drag entirely.
    pub(crate) incoming_drop: Option<DropHover>,
    /// Spring-loaded tab-select dwell for a live drag hovering THIS window's
    /// strip (finding #2): `(chip index, when this chip started being
    /// hovered)`. `None` while no strip chip is currently being dwelled on.
    /// See [`WindowState::spring_load_hover`].
    spring_load: Option<(usize, Instant)>,
    /// A press-and-hold in progress on a pane body (hold-to-wisp, v1.1) —
    /// see [`Hold`]'s doc. `None` outside a live hold; cancelled (cleared)
    /// by motion past [`HOLD_TOLERANCE_PX`] or any release.
    hold: Option<Hold>,
    /// A live suck-in/pour-out morph (v0.4.0) — see [`MorphAnim`]'s doc.
    /// `None` outside a live morph; self-terminating (clears itself the tick
    /// its duration elapses), never cancelled early.
    morph: Option<MorphAnim>,
    /// What a live drag sourced from THIS window is visually excluding —
    /// see [`CarriedExclusion`]'s doc. Set at tear-off; `None` outside a
    /// drag this window originated.
    carried_exclusion: Option<CarriedExclusion>,
    /// Whether `carried_exclusion` has actually been made visible yet
    /// (suck-in finished and, for `WholeWindow`, the carry has left this
    /// window) — see [`WindowState::apply_carried_exclusion`]. Tracked
    /// separately from `carried_exclusion.is_some()` so `sync_layout`
    /// keeps rendering normally WHILE the suck-in is still collapsing over
    /// the real content, and so `clear_carried_exclusion` only undoes work
    /// that was actually done (a very fast release inside the suck-in
    /// window hid/filtered nothing).
    exclusion_applied: bool,
    /// Set when a `WholeWindow` carried exclusion hid this OS window
    /// (`set_visible(false)`) and the matching re-show has been DEFERRED
    /// rather than applied immediately — see [`Self::clear_carried_exclusion`]
    /// and [`Self::finish_carry_reshow`]'s docs: a sole-tab window whose
    /// drag resolves to a `Move` might be about to be DESTROYED by that
    /// same move, in which case it must never re-appear first.
    hidden_for_carry: bool,
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
            sel_snapshot: None,
            selecting: false,
            search_open: false,
            search_query: String::new(),
            search_count: None,
            ime_preedit: String::new(),
            palette_open: false,
            palette_query: String::new(),
            palette_sel: 0,
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
            pending_move: None,
            incoming_drop: None,
            spring_load: None,
            hold: None,
            morph: None,
            carried_exclusion: None,
            exclusion_applied: false,
            hidden_for_carry: false,
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
        cfg.osc52_read = shared.config.osc52_read;
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
        window_id: WindowId,
        msg: ControlMsg,
    ) -> Option<ControlClose> {
        // Route injected keys into the open palette (for tests). A picked
        // chord dispatches through the same ControlMsg::Chord path below.
        if self.palette_open {
            if let ControlMsg::Key(name) = &msg {
                if let Some(k) = named_key(name) {
                    if let Some(chord) = self.palette_key(&k) {
                        return self.handle_control(shared, window_id, ControlMsg::Chord(chord));
                    }
                }
                return None;
            }
        }
        // Route injected keys into the open search bar (for tests), mirroring
        // the real keyboard path.
        if self.search_open {
            if let ControlMsg::Key(name) = &msg {
                if let Some(k) = named_key(name) {
                    self.search_key(shared, &k);
                }
                return None;
            }
        }
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
                self.left_release(shared, window_id);
            }
            ControlMsg::Drag { .. } => {
                // Handled by the caller (`about_to_wait`) before this method
                // is ever invoked — press/motion/release must run as a
                // sequence, and `run_ctl_drag` needs `self.windows` (a
                // future cross-window drop) which isn't reachable here.
                // Kept here only so the match stays exhaustive.
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
                let grid = self.renderer.grid(&sid)?;
                let mut s = AnchoredSelection::new(grid, Point::new(r1, c1), mode);
                s.update(grid, Point::new(r2, c2));
                self.sel = Some((sid, s));
                self.renderer.set_selection(self.sel.clone());
                self.sel_snapshot = self.renderer.selected_text();
            }
            ControlMsg::ImePreedit(text) => self.set_ime_preedit(text),
            ControlMsg::ImeCommit(text) => self.ime_commit(shared, &text),
            ControlMsg::Search(pattern, forward) => {
                if let Some(h) = self.focused_session(shared) {
                    let _ = h
                        .control
                        .send(ember_core::BackendControl::Search { pattern, forward });
                }
            }
            ControlMsg::Copy => self.copy_selection(shared),
            ControlMsg::Paste(text) => self.paste_into_focused(shared, &text),
            ControlMsg::DropFile(path) => self.drop_file_into_focused(shared, &path),
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
        // Carried-pane visual exclusion: once `apply_carried_
        // exclusion` has flipped this on, rects are computed on a CLONED,
        // pane-removed tree — `self.tree` itself is never mutated here (a
        // drop is still one pure `move_surface`, a cancel still zero
        // mutation) — so the sibling visually re-flows into the freed
        // space. `exclusion_applied` (not just `carried_exclusion.is_some()`)
        // gates this: while the suck-in is still collapsing over the real
        // content, layout stays normal underneath it.
        let excluded_pane = match self.carried_exclusion {
            Some(CarriedExclusion::Pane(p)) if self.exclusion_applied => Some(p),
            _ => None,
        };
        let rects = match excluded_pane {
            Some(p) => match remove_pane(tab.root.clone(), p).0 {
                Some(root) => layout(&root, vp),
                // Unreachable in practice: a pane drag never carries a
                // tab's sole pane (`pane_drag_source` upgrades that case to
                // a whole-`Tab` drag instead) — falling back to the
                // unfiltered layout is safer than an empty screen if that
                // invariant is ever violated.
                None => layout(&tab.root, vp),
            },
            None => layout(&tab.root, vp),
        };

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
        // Carried-tab visual exclusion: drop the dragged tab's
        // label from the strip entirely once applied — same `exclusion_
        // applied` gate as the pane case above. The revert tab
        // (`update_drag`'s tear-off) is already the displayed `active` one,
        // so nothing else here needs to change.
        let excluded_tab = match self.carried_exclusion {
            Some(CarriedExclusion::Tab(id)) if self.exclusion_applied => Some(id),
            _ => None,
        };
        let tabs: Vec<TabLabel> = self
            .tree
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, t)| Some(t.id) != excluded_tab)
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

    /// A file dropped from the OS (Finder / file manager) onto this window:
    /// insert its shell-escaped path plus a trailing space at the focused
    /// pane's prompt, through the paste path (bracketed-paste aware) —
    /// iTerm2 parity. Called once per file (multi-file drops arrive as one
    /// `WindowEvent::DroppedFile` each), so the per-path trailing space
    /// leaves several drops space-separated.
    pub(crate) fn drop_file_into_focused(&self, shared: &Shared, path: &str) {
        if path.is_empty() {
            return;
        }
        let mut text = shell_escape_path(path);
        text.push(' ');
        self.paste_into_focused(shared, &text);
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
            // Cmd+Shift+P — the command palette (every action, fuzzy-searchable).
            Key::Character(s) if s.eq_ignore_ascii_case("p") && mods.shift_key() => {
                self.open_palette();
                true
            }
            // Cmd+F — scrollback search (find bar).
            Key::Character(s) if s.as_str() == "f" && !mods.shift_key() && !mods.alt_key() => {
                self.open_search();
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

    /// Whether the pane-drag chord is held: Cmd+Opt on macOS, Ctrl+Alt
    /// elsewhere — mirrors [`Self::open_modifier_held`]'s `cfg!`-gated
    /// per-platform pattern (release 1's convention for a platform-specific
    /// modifier, rather than a `#[cfg(target_os = ...)]` item split).
    /// Deliberately a separate check from `split_modifier_held`, not a
    /// reuse: on macOS the two chords are physically distinct (Cmd+Opt vs.
    /// Ctrl+Opt). On Linux, Ctrl+Alt alone would coincide with the
    /// split-preview gesture — and since `on_cursor_moved` arms the preview
    /// on every motion tick while that chord is held, a real mouse could
    /// never reach the pane-drag branch (positioning the cursor arms the
    /// preview; the press commits a split). So Linux adds Shift:
    /// Ctrl+Alt+Shift+drag, which shares no gate with the preview.
    pub(crate) fn pane_drag_modifier_held(&self) -> bool {
        if cfg!(target_os = "macos") {
            self.modifiers.super_key() && self.modifiers.alt_key()
        } else {
            self.modifiers.control_key() && self.modifiers.alt_key() && self.modifiers.shift_key()
        }
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
        // The manual Ctrl+Opt split always appends the new pane on the far
        // side (`before: false`).
        self.renderer
            .set_split_preview(Some((sid, horizontal, ratio, false)));
        self.split_preview = Some(SplitPreview {
            pane,
            horizontal,
            ratio,
        });
    }

    /// Clear the split preview (modifier released / cursor left the panes).
    /// Clear BOTH halves of the split-preview state: the model (the
    /// quick-split commit target) and the renderer's visual. The visual has
    /// two INDEPENDENT writers — the Ctrl+Opt quick-split path arms model +
    /// renderer together, but the drag-drop preview paths (`set_incoming_
    /// drop`, `update_drag_hover`) arm the renderer alone — so this must
    /// never gate the renderer clear on the model being armed. It did once:
    /// every drop-preview clear was a silent no-op and the abandoned quad
    /// haunted the pane until process exit (the "rogue split" band).
    pub(crate) fn clear_split_preview(&mut self) {
        self.split_preview = None;
        self.renderer.set_split_preview(None);
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

    /// Resize the split enclosing `target` along `axis` by `delta` px. Used by
    /// keyboard resize, which only ever has one pane (the focused one) to key
    /// off. Core clamps against `min_px`.
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

    /// Resize the split that separates `a_side` and `b_side` along `axis` by
    /// `delta` px. Used by mouse divider drag, which always knows both
    /// flanking panes (from `divider_at`) — identifying the divider by the
    /// pair, rather than by one pane + axis, is what fixes the divider
    /// picking the wrong (too-deep) same-axis split in a nested layout. Core
    /// clamps against `min_px`.
    pub(crate) fn resize_split_px(
        &mut self,
        shared: &Shared,
        a_side: PaneId,
        b_side: PaneId,
        axis: Axis,
        delta: f64,
    ) {
        let vp = self.viewport();
        let min_px = self.min_px(axis);
        apply(
            &mut self.tree,
            LayoutCommand::ResizeSplit {
                a_side,
                b_side,
                axis,
                delta,
                min_px,
            },
            vp,
        );
        self.sync_layout(shared);
    }

    /// The split divider under logical `(x, y)`, as `(a-side pane, b-side
    /// pane, axis)`, when the cursor is in the gap between two adjacent
    /// panes. `None` otherwise. Both flanking panes are returned — a divider
    /// is the split whose two children SEPARATE these two panes, and a
    /// single pane + axis doesn't uniquely identify that split when the pane
    /// sits inside more than one same-axis split (see
    /// `LayoutNode::resize_split`).
    pub(crate) fn divider_at(&self, x: f64, y: f64) -> Option<(PaneId, PaneId, Axis)> {
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
                && let Some((osid, _)) = self.pane_rects.iter().find(|(_, o)| {
                    (o.x - (right + gap)).abs() <= grab && y >= o.y && y < o.y + o.height
                })
            {
                if let (Some(&p), Some(&op)) = (leaves.get(sid), leaves.get(osid)) {
                    return Some((p, op, Axis::Horizontal));
                }
            }
            // Horizontal divider on this pane's bottom edge.
            if (y - bottom).abs() <= grab
                && x >= r.x
                && x < r.x + r.width
                && let Some((osid, _)) = self.pane_rects.iter().find(|(_, o)| {
                    (o.y - (bottom + gap)).abs() <= grab && x >= o.x && x < o.x + o.width
                })
            {
                if let (Some(&p), Some(&op)) = (leaves.get(sid), leaves.get(osid)) {
                    return Some((p, op, Axis::Vertical));
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

    /// Whether an in-strip tab drag is currently past the click-vs-drag
    /// threshold (i.e. a genuine reorder, not just an armed press) — `ctl
    /// drag`'s `drag_active_mid` reply field needs this, and `TabDrag::
    /// active` is private to this module.
    pub(crate) fn tab_drag_is_active_drag(&self) -> bool {
        self.tab_drag.as_ref().is_some_and(|d| d.active)
    }

    /// Left-button press at logical `(x, y)`: the non-modal press path (a
    /// divider grab, or a full `left_click`). Shared by the real
    /// `WindowEvent::MouseInput` press handler (the pending-close modal case
    /// stays there — it needs `event_loop`/`self.windows`, neither reachable
    /// from here) and `ctl drag`'s synthesized press — both must hit this
    /// exact path so a synthesized drag behaves exactly like a real mouse
    /// press.
    pub(crate) fn press_left(&mut self, shared: &mut Shared, x: f64, y: f64) {
        self.cursor = (x, y);
        if let Some((a_side, b_side, axis)) = self.divider_at(x, y) {
            let pos = if matches!(axis, Axis::Horizontal) {
                x
            } else {
                y
            };
            self.divider_drag = Some((a_side, b_side, axis, pos));
        } else {
            self.left_click(shared);
        }
    }

    /// Mouse-up half of a left click: drag/selection teardown, plus the
    /// click-to-open decision for a link (same link + same cell as the press,
    /// so drags still select instead of opening). Returns what (if anything)
    /// the release resolved — the classification `ctl drag` replies with.
    pub(crate) fn left_release(&mut self, shared: &mut Shared, window_id: WindowId) -> DragEnded {
        // Hold-to-wisp: a live hold that never completed is just a normal
        // release (early release = normal click, per the design doc) —
        // clear it before anything else below. Already-completed holds
        // cleared themselves in `tick_hold`, so this is a no-op then.
        if self.hold.take().is_some() {
            self.renderer.set_hold_ring(None);
        }
        let mut ended = DragEnded::None;
        if let Some(drag) = shared.drag.take() {
            ended = self.resolve_drag_drop(shared, window_id, drag);
            // Task 5: end the wisp's fade-out here too — this is the ONLY
            // release path shared by both a real mouse-up and `ctl drag`'s
            // synthesized one (`run_ctl_drag` calls this same method).
            shared.wisp_end_drag();
        } else if let Some(d) = self.tab_drag.take() {
            self.renderer.set_tab_drag(None);
            ended = if d.active {
                DragEnded::Reorder
            } else {
                DragEnded::None
            };
        }
        let was_selecting = self.selecting;
        self.selecting = false;
        self.scrollbar_drag = None;
        self.divider_drag = None;
        // A plain click (no drag) clears the selection rather than leaving a
        // one-cell one — see AnchoredSelection::is_empty_click.
        if was_selecting {
            if ended == DragEnded::None {
                ended = DragEnded::Selection;
            }
            if self.sel.as_ref().is_some_and(|(_, s)| s.is_empty_click()) {
                self.clear_selection();
            }
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
        ended
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
        // A click while the Ctrl+Opt split preview is up commits that split —
        // but ONLY while the chord is still held. The drop-preview visuals
        // share this state's renderer half, and a preview that outlived its
        // gesture must never turn a plain click into a surprise split with a
        // fresh shell (the "rogue split" from the first live drag session).
        if self.split_modifier_held() {
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
        } else if self.split_preview.take().is_some() {
            // Stale preview without the chord: dissolve it, treat the click
            // as an ordinary click.
            self.renderer.set_split_preview(None);
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
                        let origin_tab = self.tree.tabs[self.tree.active].id;
                        self.select_tab(shared, i + 1);
                        self.tab_drag = Some(TabDrag {
                            tab: i,
                            press_x: x,
                            active: false,
                            origin_tab,
                        });
                        // Hold-to-wisp on a tab chip (this task's addition):
                        // armed alongside the reorder-capable `tab_drag`
                        // above exactly like a pane-body press arms it
                        // alongside the forwarded click/selection below —
                        // motion past tolerance (`on_cursor_moved`) cancels
                        // it same as the pane case, and a quick release
                        // (`left_release`) with neither this completed nor
                        // `tab_drag` gone active leaves a plain tab select,
                        // unaffected either way.
                        self.hold = Some(Hold {
                            target: HoldTarget::Tab { tab: i },
                            origin: (x, y),
                            started: Instant::now(),
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
        // Cmd+Opt (macOS) / Ctrl+Alt (Linux) on a pane body starts a
        // chord-gated pane drag, ALREADY torn off, instead of a forwarded
        // click or a selection — checked here, at the exact point the
        // mouse-aware-app-forward/selection branch on modifiers already
        // lived, so a chord-gated press never reaches either.
        if self.pane_drag_modifier_held() {
            if let Some((pane, rect)) = self.pane_at(x, y) {
                self.start_pane_drag(shared, pane, rect, x, y);
                return;
            }
        }
        // Hold-to-wisp (v1.1): a PLAIN press on a pane body arms a hold
        // timer alongside whatever the press already does below (forwarded
        // click / selection) — completing the ring (HOLD_ARM_MS +
        // HOLD_SWEEP_MS, driven from `about_to_wait` via `tick_hold`) tears
        // the pane off exactly like the chord gesture above. Moving past
        // HOLD_TOLERANCE_PX (`on_cursor_moved`) or releasing early
        // (`left_release`) just clears this — the press's own behavior,
        // set below, is never touched by arming it.
        if let Some((pane, rect)) = self.pane_at(x, y) {
            self.hold = Some(Hold {
                target: HoldTarget::Pane { pane, rect },
                origin: (x, y),
                started: Instant::now(),
            });
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

    /// The pane (and its own rect, logical px) under point `(x, y)`, this
    /// window's own coordinate space — `None` outside every visible pane
    /// (strip, scrollbar, dividers, or empty margin). Shared by the
    /// chord-gated pane drag and the hold-to-wisp arm check, which both need
    /// exactly this hit-test.
    fn pane_at(&self, x: f64, y: f64) -> Option<(PaneId, Rect)> {
        let (sid, rect) = self
            .pane_rects
            .iter()
            .find(|(_, r)| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height)
            .cloned()?;
        let pane = self
            .active_tab()
            .root
            .leaves()
            .into_iter()
            .find(|(_, s)| *s == sid)
            .map(|(p, _)| p)?;
        Some((pane, rect))
    }

    /// Start a chord-gated pane drag at a pane-body press: computes the
    /// dragged surface via [`pane_drag_source`] (Pane, unless this is the
    /// tab's only pane — then Tab, per the sole-pane-tab rule) and sets
    /// `shared.drag` directly, ALREADY torn off. Unlike tab tear-off
    /// (`update_drag`'s `TabDrag` -> `DragState` conversion once the
    /// pointer leaves the strip band), a pane drag has no in-strip phase to
    /// pass through first — the chord-gated press itself is the tear-off.
    /// `origin_tab`-style display-revert doesn't apply here either: this
    /// press never switched the active tab (unlike a tab-strip press's
    /// `select_tab`), so there's nothing to restore. `rect` is the pressed
    /// pane's own rect (logical px, this window's coordinate space) —
    /// `grab` is the pointer's offset within it, mirroring tab tear-off's
    /// `grab` (offset within the lifted visual).
    fn start_pane_drag(&mut self, shared: &mut Shared, pane: PaneId, rect: Rect, x: f64, y: f64) {
        let window_id = self.renderer.window().id();
        let window = resolve_window_index(shared, window_id).unwrap_or(0);
        let tab = self.tree.active;
        let panes_in_tab = self.active_tab().root.pane_ids().len();
        let surface = pane_drag_source(window, tab, pane, panes_in_tab);
        // `tab_display_title`, not the raw (often-empty) `Tab::title` — the
        // ghost should show what the user actually SEES on the strip (the
        // "1"/"2" position fallback), not fall back further to the "＋"
        // glyph for the common untitled case.
        let title = tab_display_title(&self.active_tab().title, tab);
        shared.drag = Some(DragState {
            surface,
            source_window: window_id,
            grab: (x - rect.x, y - rect.y),
            carried: false,
            hover: None,
            last_screen: (0.0, 0.0), // set for real on this same tick's
            last_raised: None,
            // motion, immediately below by
            // `update_drag_hover`'s caller — see
            // `App::window_event`'s CursorMoved arm
            // (mirrors tab tear-off's identical
            // placeholder).
            title,
        });
        // Suck-in (v0.4.0): the grabbed surface visibly collapses toward
        // the grab point — see `start_suck_in`'s doc for the wisp-off/
        // reduced-motion no-op. The rect must cover the WHOLE carried
        // surface (finding #4): the pressed pane's own rect for a true
        // pane drag, but when the sole-pane-tab rule upgraded the surface
        // to the whole Tab, the whole tab content area — or the entire
        // window when it's also the window's only tab.
        let morph_rect = match surface {
            SurfaceRef::Pane { .. } => rect,
            SurfaceRef::Tab { .. } => self.tab_morph_rect(),
        };
        // Carry-time source vanish: arm the exclusion now, apply
        // it immediately after — a no-op while the suck-in above is still
        // live (wisp on), but instant when it no-opped (wisp off/reduced
        // motion: "or tear-off, if no morph").
        self.begin_carried_exclusion(surface);
        self.start_suck_in(shared, morph_rect, (x, y));
        self.apply_carried_exclusion(shared, false);
    }

    /// Advance a live hold-to-wisp gesture by one tick (`about_to_wait`'s
    /// per-window pacing loop calls this every wake, mirroring how it paces
    /// backdrop/bell animations). `Idle` when there's nothing to do — the
    /// overwhelmingly common case, so this stays cheap for every window that
    /// isn't mid-hold. On completion, reuses [`Self::start_pane_drag`]
    /// EXACTLY as the chord gesture does (same sole-pane-tab upgrade, same
    /// `DragState` shape), then marks it `carried` immediately so the wisp
    /// shows without waiting for a motion tick (design: "wisp visible
    /// immediately") — a mouse-reporting app that had this press forwarded
    /// gets the matching synthetic release first, so it never sees a stuck
    /// button.
    pub(crate) fn tick_hold(&mut self, shared: &mut Shared, now: Instant) -> HoldTick {
        let Some(hold) = self.hold.as_ref() else {
            return HoldTick::Idle;
        };
        let armed_at = hold.started + Duration::from_millis(HOLD_ARM_MS);
        if now < armed_at {
            return HoldTick::Waiting(armed_at);
        }
        let sweep = Duration::from_millis(HOLD_SWEEP_MS);
        let elapsed = now.duration_since(armed_at);
        if elapsed >= sweep {
            let (target, origin) = (hold.target, hold.origin);
            self.hold = None;
            self.renderer.set_hold_ring(None);
            match target {
                HoldTarget::Pane { pane, rect } => {
                    self.forward_mouse_release(shared, 0);
                    // The same press began a selection (and jitter may have
                    // grown it); the gesture is the drag's now — drop the
                    // selection so it neither lingers under the carried pane
                    // nor fights the release.
                    self.selecting = false;
                    self.clear_selection();
                    self.start_pane_drag(shared, pane, rect, origin.0, origin.1);
                }
                HoldTarget::Tab { tab } => {
                    // A tab press has no forwarded click/selection to undo
                    // (unlike a pane body, `left_click`'s `TabHit::Tab` arm
                    // returns before either): the only thing armed alongside
                    // this hold is `tab_drag`, which `tear_off_tab` clears.
                    // `origin_tab` mirrors `update_drag`'s own tear-off:
                    // whichever tab was active immediately before THIS press
                    // (captured in `tab_drag` at press time, before its own
                    // `select_tab` switched to the pressed one).
                    let origin_tab = self
                        .tab_drag
                        .as_ref()
                        .map(|d| d.origin_tab)
                        .unwrap_or_else(|| self.tree.tabs[self.tree.active].id);
                    let window_id = self.renderer.window().id();
                    // No lateral motion to report (a completed hold means
                    // the pointer never left `HOLD_TOLERANCE_PX`/
                    // `HOLD_SWEEP_TOLERANCE_PX`) — the chip is grabbed at
                    // its own press point, same as `start_pane_drag`'s
                    // `grab` offset is zero at the exact press pixel.
                    self.tear_off_tab(
                        shared, window_id, tab, origin_tab, origin.0, origin.0, origin.1,
                    );
                }
            }
            if let Some(d) = shared.drag.as_mut() {
                d.carried = true;
            }
            return HoldTick::Completed;
        }
        let progress = elapsed.as_secs_f32() / sweep.as_secs_f32();
        let (x, y) = hold.origin;
        self.renderer
            .set_hold_ring(Some((x as f32, y as f32, progress)));
        HoldTick::Sweeping
    }

    /// Start a morph (suck-in or pour-out) on THIS window's renderer.
    /// No-ops (leaves/sets `self.morph` to `None`) when the wisp is off
    /// (`shared.config.wisp`) or reduced motion is active — the design
    /// doc's "both skipped under reduced-motion (instant transfer)" applies
    /// to this v1 approximation exactly as it would the full wisp.
    fn start_morph(&mut self, shared: &Shared, rect: Rect, grab: (f64, f64), inward: bool) {
        if !shared.config.wisp || shared.reduce_motion() {
            self.morph = None;
            self.renderer.set_morph(None);
            return;
        }
        let duration = Duration::from_millis(if inward { SUCK_IN_MS } else { POUR_OUT_MS });
        self.morph = Some(MorphAnim {
            started: Instant::now(),
            duration,
            rect,
            grab,
            inward,
        });
        self.tick_morph(Instant::now());
    }

    /// Start the tear-off suck-in on THIS (SOURCE) window: `rect` is the
    /// grabbed surface's own rect, `grab` the point it's collapsing toward
    /// (both logical px, this window's space).
    pub(crate) fn start_suck_in(&mut self, shared: &Shared, rect: Rect, grab: (f64, f64)) {
        self.start_morph(shared, rect, grab, true);
    }

    /// The whole window's logical rect (strip included) — the suck-in's
    /// canvas when the carried surface IS effectively the whole window (its
    /// only tab), per finding #4's "entire shell/tab/(and window if it's
    /// the only tab)".
    pub(crate) fn full_window_rect(&self) -> Rect {
        let sf = self.renderer.window().scale_factor();
        Rect::new(
            0.0,
            0.0,
            (self.px.0 as f64 / sf).max(1.0),
            (self.px.1 as f64 / sf).max(1.0),
        )
    }

    /// The rect a TAB-surface drag's suck-in should swallow (finding #4):
    /// the whole tab content area (everything below the strip), escalating
    /// to the ENTIRE window when the dragged tab is this window's only one —
    /// carrying it away carries the whole window, and the collapse should
    /// read that way.
    pub(crate) fn tab_morph_rect(&self) -> Rect {
        if self.tree.tabs.len() <= 1 {
            self.full_window_rect()
        } else {
            self.viewport()
        }
    }

    /// Start a pour-out on THIS window: `rect` is the landed (or, on
    /// cancel/reorder, the going-home) surface's rect, `grab` the point it's
    /// expanding from (both logical px, this window's space).
    pub(crate) fn start_pour_out(&mut self, shared: &Shared, rect: Rect, grab: (f64, f64)) {
        self.start_morph(shared, rect, grab, false);
    }

    /// Whether a suck-in/pour-out morph is live on this window right now —
    /// `close_window_shell_only` (`main.rs`) uses this to decide whether a
    /// drop-emptied source window gets parked in `Shared::dying_windows` to
    /// play its farewell collapse (finding #4), or just drops immediately
    /// (wisp off / reduced motion: the `start_morph` gate no-opped).
    pub(crate) fn morph_live(&self) -> bool {
        self.morph.is_some()
    }

    /// Advance a live morph by one tick, mirroring [`Self::tick_hold`]'s
    /// shape: `Idle` the overwhelmingly common case. Self-terminates the
    /// instant its duration elapses — clears both `self.morph` and the
    /// renderer's mirrored state right here, the sole place morph visuals
    /// turn off (see [`MorphAnim`]'s doc for why `clear_drag_visuals`
    /// deliberately never touches them).
    pub(crate) fn tick_morph(&mut self, now: Instant) -> MorphTick {
        let Some(anim) = self.morph.as_ref() else {
            return MorphTick::Idle;
        };
        let elapsed = now.saturating_duration_since(anim.started);
        if elapsed >= anim.duration {
            self.morph = None;
            self.renderer.set_morph(None);
            return MorphTick::Idle;
        }
        let t01 = elapsed.as_secs_f32() / anim.duration.as_secs_f32();
        let rect = [
            anim.rect.x as f32,
            anim.rect.y as f32,
            anim.rect.width as f32,
            anim.rect.height as f32,
        ];
        let grab = (anim.grab.0 as f32, anim.grab.1 as f32);
        self.renderer
            .set_morph(Some((rect, grab, t01, anim.inward)));
        MorphTick::Running(now + MORPH_FRAME)
    }

    /// Which [`CarriedExclusion`] a torn-off `surface` should visually apply
    /// on THIS (source) window — mirrors [`Self::tab_morph_rect`]'s
    /// window-vs-tab escalation: a `Tab` surface that's this window's only
    /// tab carries the whole window away, so the WHOLE OS window hides
    /// rather than trying to render an empty strip. Reads `surface`'s own
    /// `tab`/`pane` fields directly rather than `self.active_tab()` — at the
    /// tab-tear-off call site, the active tab has already been switched to
    /// the revert target by the time this runs (see `update_drag`'s
    /// `origin_tab` handling), so it's no longer the dragged one.
    fn exclusion_for(&self, surface: SurfaceRef) -> CarriedExclusion {
        match surface {
            SurfaceRef::Pane { pane, .. } => CarriedExclusion::Pane(pane),
            SurfaceRef::Tab { tab, .. } => {
                if self.tree.tabs.len() <= 1 {
                    CarriedExclusion::WholeWindow
                } else {
                    self.tree
                        .tabs
                        .get(tab)
                        .map(|t| CarriedExclusion::Tab(t.id))
                        .unwrap_or(CarriedExclusion::WholeWindow)
                }
            }
        }
    }

    /// Arm this window's carried-surface exclusion for a fresh tear-off:
    /// records WHAT will vanish (`exclusion_for`) without yet making
    /// anything vanish — [`Self::apply_carried_exclusion`] does that once
    /// the suck-in (if any) finishes. Called once per tear-off, right
    /// alongside `start_suck_in`/`start_pane_drag`'s `shared.drag =
    /// Some(...)`.
    fn begin_carried_exclusion(&mut self, surface: SurfaceRef) {
        self.carried_exclusion = Some(self.exclusion_for(surface));
        self.exclusion_applied = false;
    }

    /// Re-apply this window's carried-surface visual exclusion if it isn't
    /// already in effect — idempotent, so it's safe to call every tick a
    /// drag sourced from this window is live (`about_to_wait`'s per-window
    /// pacing loop does exactly that). No-ops while a suck-in is still
    /// animating (`self.morph_live()`) — the design's "from the moment the
    /// suck-in completes" — or while nothing is excluded at all. `carried`
    /// (`DragState::carried`: has the pointer left this window's own bounds
    /// yet) gates ONLY the `WholeWindow` case: hiding the OS window out from
    /// under a pointer that's still on it would pull the rug out from under
    /// its own mouse capture, which the whole cross-window carry depends on
    /// staying alive (the source window keeps receiving motion past its own
    /// bounds only while it's a live, visible OS window) — so a sole-tab
    /// window keeps showing its (about to be carried) content until the
    /// carry genuinely leaves it. `Pane`/`Tab` exclusion has no such constraint:
    /// the surface reflows/disappears the instant the suck-in ends, whether
    /// or not the pointer has left the window yet.
    pub(crate) fn apply_carried_exclusion(&mut self, shared: &Shared, carried: bool) {
        let Some(ex) = self.carried_exclusion else {
            return;
        };
        if self.exclusion_applied || self.morph_live() {
            return;
        }
        match ex {
            CarriedExclusion::Pane(_) | CarriedExclusion::Tab(_) => {
                self.exclusion_applied = true;
                self.sync_layout(shared);
            }
            CarriedExclusion::WholeWindow => {
                if carried {
                    self.exclusion_applied = true;
                    self.renderer.window().set_visible(false);
                }
            }
        }
    }

    /// Whether this window currently has a live carried-surface exclusion
    /// (armed or applied) — `about_to_wait`'s cheap per-tick gate for
    /// whether [`Self::apply_carried_exclusion`] is worth calling at all.
    pub(crate) fn carried_exclusion_live(&self) -> bool {
        self.carried_exclusion.is_some()
    }

    /// End this window's carried-surface visual exclusion — called the
    /// instant a drag sourced from this window resolves, whatever the
    /// outcome (drop, cancel, or an own-strip reorder): restores the
    /// filtered layout, and re-syncs it so a surviving tab/pane reflects
    /// reality again. A no-op past the `.take()` when the exclusion never
    /// actually took visual effect (a release inside the suck-in window
    /// itself hid/filtered nothing, so there's nothing to undo).
    ///
    /// Deliberately does NOT `set_visible(true)` the `WholeWindow` case here:
    /// a sole-tab window's drag can resolve to a `Move`
    /// that's about to CLOSE this very window (its only tab moved out); a
    /// synchronous re-show-then-close reads as a visible flash of the old
    /// window before it vanishes. Instead this marks `hidden_for_carry` and
    /// leaves the OS window hidden; the caller re-shows it only once it
    /// knows the window survived — see [`Self::finish_carry_reshow`]'s doc
    /// for exactly where that happens.
    pub(crate) fn clear_carried_exclusion(&mut self, shared: &Shared) {
        let was_applied = self.exclusion_applied;
        let ex = self.carried_exclusion.take();
        self.exclusion_applied = false;
        if !was_applied {
            return;
        }
        if ex == Some(CarriedExclusion::WholeWindow) {
            self.hidden_for_carry = true;
        }
        self.sync_layout(shared);
    }

    /// Re-show an OS window left hidden by [`Self::clear_carried_exclusion`]'s
    /// deferred `WholeWindow` re-show — a no-op when nothing is deferred,
    /// so it's safe to call on every window unconditionally.
    /// Callers run this AFTER `apply_move` (or, on a cancel, immediately —
    /// see `cancel_drag_everywhere`, which never closes the source): a
    /// window `apply_move` closed is gone from `self.windows` by then and
    /// this method is simply never reached for it, so a window on the verge
    /// of destruction never re-appears first.
    pub(crate) fn finish_carry_reshow(&mut self) {
        if self.hidden_for_carry {
            self.renderer.window().set_visible(true);
            self.hidden_for_carry = false;
        }
    }

    /// Record what strip chip (if any) a live drag is hovering on THIS
    /// window right now — the spring-loaded tab select's input (finding #2).
    /// Restarts the dwell clock when the chip changes; keeps it running
    /// across repeated same-chip calls (every motion tick re-reports the
    /// hover); clears it on `None` (left the chips: a pane, the ghost
    /// segment, another window, anywhere). Pure bookkeeping — the actual
    /// select fires from [`Self::tick_spring_load`], which is also what
    /// makes a perfectly still pointer (no further motion ticks) still
    /// complete its dwell.
    pub(crate) fn note_spring_hover(&mut self, chip: Option<usize>) {
        match (chip, &self.spring_load) {
            (Some(c), Some((cur, _))) if *cur == c => {} // same chip: clock runs on
            (Some(c), _) => self.spring_load = Some((c, Instant::now())),
            (None, _) => self.spring_load = None,
        }
    }

    /// Advance the spring-loaded tab select by one tick (`about_to_wait`'s
    /// per-window loop, mirroring [`Self::tick_hold`]/[`Self::tick_morph`]):
    /// once the dwell elapses, the hovered chip's tab becomes the displayed
    /// tab, exactly as if the user had clicked it — the whole point: they
    /// can now navigate INTO that tab's panes and drop precisely. Returns
    /// the pending dwell deadline for `next_wake` while one is live (`None`
    /// otherwise — the overwhelmingly common case). No-select cases (chip
    /// already active, or index stale after a mid-drag close) still clear
    /// the dwell so it doesn't re-arm every tick.
    pub(crate) fn tick_spring_load(&mut self, shared: &Shared, now: Instant) -> Option<Instant> {
        let (chip, since) = self.spring_load?;
        let deadline = since + Duration::from_millis(SPRING_LOAD_MS);
        if now < deadline {
            return Some(deadline);
        }
        self.spring_load = None;
        if chip != self.tree.active && chip < self.tree.tabs.len() {
            self.select_tab(shared, chip + 1);
            self.renderer.window().request_redraw();
        }
        None
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

    /// Handle pointer motion at logical `(x, y)`: divider resize, tab
    /// reorder/tear-off/hover, scrollbar drag, text selection, or (when
    /// nothing is in progress) hover/cursor-icon bookkeeping + motion
    /// forwarding to a mouse-aware app. Shared by the real
    /// `WindowEvent::CursorMoved` handler and `ctl drag`'s synthesized
    /// motion steps — both must hit this exact path so a synthesized drag
    /// behaves exactly like a real mouse move.
    pub(crate) fn on_cursor_moved(
        &mut self,
        shared: &mut Shared,
        window_id: WindowId,
        x: f64,
        y: f64,
    ) {
        self.cursor = (x, y);
        // Hold-to-wisp: moving past HOLD_TOLERANCE_PX before the ring
        // completes cancels it — the press falls back to whatever it
        // already was (selection / mouse-mode forward, both driven further
        // below, untouched by this). Checked unconditionally, before every
        // other branch, so it cancels regardless of what else this motion
        // is doing (e.g. extending a selection the same press started).
        if let Some(hold) = &self.hold {
            let (ox, oy) = hold.origin;
            let sweeping = hold.started.elapsed() >= Duration::from_millis(HOLD_ARM_MS);
            let leash = if sweeping {
                HOLD_SWEEP_TOLERANCE_PX
            } else {
                HOLD_TOLERANCE_PX
            };
            if (x - ox).hypot(y - oy) > leash {
                self.hold = None;
                self.renderer.set_hold_ring(None);
            }
        }
        // Ctrl+Opt held → live split drop-zone preview over the hovered pane.
        if self.split_modifier_held() {
            self.update_split_preview();
            return;
        }
        if let Some((a_side, b_side, axis, last)) = self.divider_drag {
            let pos = if matches!(axis, Axis::Horizontal) {
                x
            } else {
                y
            };
            self.resize_split_px(shared, a_side, b_side, axis, pos - last);
            self.divider_drag = Some((a_side, b_side, axis, pos));
        } else if self.tab_drag.is_some() || shared.drag.is_some() {
            self.update_drag(shared, window_id, x, y);
        } else if let Some(sid) = self.scrollbar_drag.clone() {
            self.scroll_to_at(shared, &sid, y as f32);
        } else if self.selecting {
            self.extend_selection(x, y);
        } else {
            // Tab strip: track hover (highlight + "✕"); motion over the strip
            // is chrome, not pane motion, so stop here.
            if self.update_tab_hover(x, y) {
                return;
            }
            // Show a resize cursor over a divider, a pointer over a link
            // (divider wins), else forward motion to mouse-aware apps.
            let over = self.divider_at(x, y).map(|(_, _, a)| a);
            let link = if over.is_none() {
                self.link_under_cursor()
            } else {
                None
            };
            self.renderer
                .set_hovered_link(link.as_ref().map(|(sid, id, ..)| (sid.clone(), *id)));
            let want = match (over, &link) {
                (Some(Axis::Horizontal), _) => CursorIcon::EwResize,
                (Some(Axis::Vertical), _) => CursorIcon::NsResize,
                (None, Some(_)) => CursorIcon::Pointer,
                (None, None) => CursorIcon::Default,
            };
            if self.pointer_cursor != want {
                self.pointer_cursor = want;
                self.renderer.window().set_cursor(want);
            }
            if over.is_none() {
                self.forward_mouse_motion(shared);
            }
        }
    }

    /// Advance an in-progress tab reorder/tear-off as the pointer moves:
    /// while still inside the strip band, this is exactly `drag_tab_to`
    /// (unregressed — the live in-strip reorder); once the pointer crosses
    /// [`TEAR_OFF_THRESHOLD`] below the strip, converts to `shared.drag`
    /// (clearing `tab_drag`) and reveals the tab that was active before this
    /// press (re-resolving `origin_tab`'s CURRENT index — a live in-strip
    /// reorder before tear-off can have moved it) so an in-window pane drop
    /// has something hoverable underneath that ISN'T the tab being dragged
    /// (see [`TabDrag::origin_tab`]'s doc). Once torn off, delegates to
    /// [`Self::update_drag_hover`] every subsequent move.
    fn update_drag(&mut self, shared: &mut Shared, window_id: WindowId, x: f64, y: f64) {
        if let Some(d) = &self.tab_drag {
            let (press_x, tab, origin_tab) = (d.press_x, d.tab, d.origin_tab);
            let mut active = d.active;
            if !active {
                if (x - press_x).abs() < TAB_DRAG_THRESHOLD {
                    return;
                }
                active = true;
                if let Some(d) = self.tab_drag.as_mut() {
                    d.active = true;
                }
            }
            let strip_bottom = Renderer::chrome_height() as f64;
            if active && strip_band_exit(y, strip_bottom, TEAR_OFF_THRESHOLD) {
                self.tear_off_tab(shared, window_id, tab, origin_tab, press_x, x, y);
            } else {
                // Still an in-strip reorder — unchanged existing behavior.
                self.drag_tab_to(shared, x);
                return;
            }
        }
        if shared.drag.is_some() {
            self.update_drag_hover(shared, window_id, x, y);
        }
    }

    /// Tear tab `tab` off into a freshly carried `shared.drag` — the shared
    /// core of both a motion-driven tear-off (`update_drag`, once the
    /// pointer crosses [`TEAR_OFF_THRESHOLD`] below the strip) and a
    /// completed hold-to-wisp on a tab chip ([`Self::tick_hold`]'s
    /// `HoldTarget::Tab` branch, a STATIONARY tear-off with no drag motion
    /// of its own). `press_x` is the original press point (used for the
    /// carried chip's `grab` offset — zero for a hold, since it never moved);
    /// `(x, y)` is the current pointer position (== the press point for a
    /// hold). `origin_tab` is whichever tab was active immediately BEFORE
    /// this press (see [`TabDrag::origin_tab`]'s doc) — reveals a real merge
    /// target underneath rather than re-displaying the tab being dragged.
    #[allow(clippy::too_many_arguments)]
    fn tear_off_tab(
        &mut self,
        shared: &mut Shared,
        window_id: WindowId,
        tab: usize,
        origin_tab: TabId,
        press_x: f64,
        x: f64,
        y: f64,
    ) {
        self.tab_drag = None;
        let strip_bottom = Renderer::chrome_height() as f64;
        // Reveal a tab that ISN'T the dragged one, so in-window pane
        // drops have a real merge target underneath. The origin tab
        // qualifies only when it's a DIFFERENT tab: dragging the
        // already-active tab resolves origin == dragged, which would
        // re-display the dragged tab and make every in-window drop
        // self-reject ("tab can't merge into itself") — same failure
        // as a closed origin, same fallback.
        let origin_idx = self
            .tree
            .tabs
            .iter()
            .position(|t| t.id == origin_tab)
            .filter(|idx| *idx != tab);
        if let Some(idx) = origin_idx.or_else(|| revert_target_tab(self.tree.tabs.len(), tab)) {
            self.tree.active = idx;
        }
        self.sync_layout(shared);
        let window = resolve_window_index(shared, window_id).unwrap_or(0);
        // `tab_display_title` — see `start_pane_drag`'s identical
        // comment for why this beats the raw (often-empty) title.
        let title = self
            .tree
            .tabs
            .get(tab)
            .map(|t| tab_display_title(&t.title, tab))
            .unwrap_or_default();
        let surface = SurfaceRef::Tab { window, tab };
        shared.drag = Some(DragState {
            surface,
            source_window: window_id,
            grab: (x - press_x, y - strip_bottom),
            carried: false,
            hover: None,
            last_screen: (0.0, 0.0), // set for real on this same tick's
            last_raised: None,
            // motion, immediately below by `update_drag_hover`'s
            // caller — see `App::window_event`'s CursorMoved arm.
            title,
        });
        // Suck-in (v0.4.0): the torn-off tab's WHOLE content area
        // (everything below the strip) collapses toward the tear-off
        // point — the entire window when this is its only tab
        // (finding #4: the surface being carried is the tab, so the
        // collapse must swallow all of it, not just one pane).
        // Carry-time source vanish — see
        // `start_pane_drag`'s identical arm/apply pair.
        self.begin_carried_exclusion(surface);
        self.start_suck_in(shared, self.tab_morph_rect(), (x, strip_bottom));
        self.apply_carried_exclusion(shared, false);
    }

    /// Classify a local point `(x, y)` — logical px, THIS window's own
    /// coordinate space — as a drop hover target: the tab strip band (an
    /// insertion index), else a pane hit + [`drop_zone_for`] (`Edge`/
    /// `Center`), else `None` (nothing here — the caller decides what that
    /// means: an in-window caller treats it as "release here cancels", a
    /// cross-window caller treats a total miss across every tracked window
    /// as `DropHover::Desktop`). Bounds-checks first: a point outside this
    /// window's own logical size is never a hit, however far out it is —
    /// without this, `tab_slot_at`'s deliberate clamping (so a live in-strip
    /// drag never returns `None` mid-window) would otherwise misclassify a
    /// point that's actually left the window entirely (e.g. now hovering a
    /// DIFFERENT window's strip) as "still hovering THIS window's strip".
    /// Pure read: no `self` mutation, no dependency on which surface (if
    /// any) is being dragged — used both for the in-window hover
    /// (`update_drag_hover`, called on the drag's own source) and the
    /// cross-window target's hover (`App::update_cross_window_drag` in
    /// `main.rs`, called on whichever OTHER window the pointer is over).
    /// `in_window`: the hovering drag ORIGINATES in this same window. Center
    /// ("add as a tab here") is a guaranteed no-op for a same-window drag, so
    /// pane zones switch to nearest-edge half-pane splits (`split_zone_for`)
    /// — restoring the pre-multi-window "drag a tab onto a pane to split"
    /// gesture that a huge Center region had silently eaten (nine rejected
    /// drops in one live session).
    pub(crate) fn hover_at(
        &self,
        window_id: WindowId,
        x: f64,
        y: f64,
        in_window: bool,
    ) -> Option<DropHover> {
        let sf = self.renderer.window().scale_factor();
        let (lw, lh) = (self.px.0 as f64 / sf, self.px.1 as f64 / sf);
        if x < 0.0 || y < 0.0 || x >= lw || y >= lh {
            return None;
        }
        let strip_bottom = Renderer::chrome_height() as f64;
        if y <= strip_bottom {
            // `tab_slot_or_ghost_at` (ghost-aware) rather than `tab_slot_at`
            // (drop-position math, always a real tab) — this needs to know
            // whether the hover is over a REAL chip (spring-loads a
            // `select_tab`, finding #2) or the trailing ghost/append segment
            // (finding #3), which `tab_slot_at` alone can't distinguish.
            // `insert_at` keeps `tab_slot_at`'s own real-tab-index meaning
            // for the Ghost case too (the last real tab) — every existing
            // reorder/append caller only reads `insert_at`, unchanged.
            //
            // Carry-time source vanish: while this window's own
            // exclusion is APPLIED, the renderer's own `tabs` list is
            // missing the excluded tab (see `sync_layout`'s `excluded_tab`
            // filter) — so a `StripSlot::Tab(i)` from it is an index into
            // that SHORTER list, not `self.tree.tabs`. Every downstream
            // consumer (`update_drag_hover`'s spring-load select,
            // `resolve_drag_drop`'s own-strip reorder) expects a REAL tree
            // index, so remap it back here, the one place both spaces are
            // in scope together.
            let excluded_idx = if self.exclusion_applied {
                match self.carried_exclusion {
                    Some(CarriedExclusion::Tab(id)) => {
                        self.tree.tabs.iter().position(|t| t.id == id)
                    }
                    _ => None,
                }
            } else {
                None
            };
            return self.renderer.tab_slot_or_ghost_at(x as f32).map(|slot| {
                let (insert_at, chip) = match slot {
                    ember_render::StripSlot::Tab(i) => {
                        let real_i = match excluded_idx {
                            Some(e) if i >= e => i + 1,
                            _ => i,
                        };
                        (real_i, Some(real_i))
                    }
                    ember_render::StripSlot::Ghost => {
                        (self.tree.tabs.len().saturating_sub(1), None)
                    }
                };
                DropHover::Strip {
                    window: window_id,
                    insert_at,
                    chip,
                }
            });
        }
        let hit = self
            .pane_rects
            .iter()
            .find(|(_, r)| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height)
            .cloned()?;
        let (sid, rect) = hit;
        let pane = self
            .active_tab()
            .root
            .leaves()
            .into_iter()
            .find(|(_, s)| *s == sid)
            .map(|(p, _)| p)?;
        let zone = if in_window {
            ember_core::split_zone_for(x - rect.x, y - rect.y, rect.width, rect.height)
        } else {
            drop_zone_for(x - rect.x, y - rect.y, rect.width, rect.height)
        };
        Some(DropHover::Pane {
            window: window_id,
            tab: self.tree.active,
            pane,
            zone,
        })
    }

    /// Update the live drop-hover for a torn-off drag as the pointer moves,
    /// re-deriving the preview (`set_split_preview`/the lifted tab chip)
    /// from it, ON THE DRAG'S OWN SOURCE WINDOW. `App::update_cross_window_drag`
    /// (`main.rs`) handles the cross-window case (a different window's
    /// `incoming_drop`) right after this returns each tick; this method only
    /// ever sees `drag.source_window == window_id`.
    fn update_drag_hover(&mut self, shared: &mut Shared, window_id: WindowId, x: f64, y: f64) {
        let Some(drag) = shared.drag.as_ref() else {
            return;
        };
        // A tab drag keeps the lifted chip following the cursor (below); a
        // pane drag shows the wispy ghost tab on a strip hover instead
        // (finding #3 — it used to show NOTHING there, even though the drop
        // itself was already wired: "promote pane to a new tab").
        let dragged_tab = match drag.surface {
            SurfaceRef::Tab { tab, .. } => Some(tab),
            SurfaceRef::Pane { .. } => None,
        };
        let surface = drag.surface;
        if drag.source_window != window_id {
            return;
        }
        let title = drag.title.clone();
        let mut hover = self.hover_at(window_id, x, y, true);
        // A same-window Pane hover that would resolve to one of
        // `move_surface`'s no-op self-merge rejections ("split into self" /
        // "tab can't merge into itself" — see `windows.rs`'s `validate`)
        // must never show a split preview: with the reveal-a-different-tab
        // step this method's caller (`update_drag`/`tear_off_tab`) always
        // runs at tear-off, this is structurally rare, but a Pane-sourced
        // drag that got sole-pane-tab-promoted to a `Tab` surface
        // (`pane_drag_source`) doesn't switch the active tab at all — so its
        // own (still-displayed) panes can still be hovered. Suppressing here
        // (rather than only in `resolve_drag_drop`) also clears `drag.hover`
        // itself, so a release on a phantom band cleanly cancels instead of
        // surfacing `apply_move`'s rejection as a console error.
        if let Some(DropHover::Pane {
            tab: hovered_tab,
            pane: hovered_pane,
            ..
        }) = &hover
        {
            if hover_is_self_merge(surface, *hovered_tab, *hovered_pane) {
                hover = None;
            }
        }
        match &hover {
            Some(DropHover::Strip { chip, .. }) => {
                // Spring-loaded tab select (finding #2): dwelling on a REAL
                // chip selects it; the ghost/append segment never does.
                self.note_spring_hover(*chip);
                self.clear_split_preview();
                if dragged_tab.is_none() {
                    // Pane-sourced drag hovering the strip: the same wispy
                    // ghost tab a cross-window strip hover shows (finding
                    // #3), with the same title threading — an honest "drop
                    // here appends a new tab" preview. Tab drags keep their
                    // lifted-chip visual instead (below).
                    self.renderer.set_ghost_tab(Some(title));
                }
            }
            Some(DropHover::Pane {
                pane,
                zone: DropZone::Edge { axis, before },
                ..
            }) => {
                self.note_spring_hover(None);
                self.renderer.set_ghost_tab(None);
                if let Some(sid) = self.active_tab().root.session_of(*pane).cloned() {
                    let horizontal = matches!(axis, Axis::Horizontal);
                    // A fixed 50/50 preview — a drag-drop split is always
                    // even (`move_surface`'s own split ratio), unlike the
                    // Ctrl+Opt manual-split preview this same visual also
                    // drives, which follows the cursor. `before` now
                    // threads through to `set_split_preview` so the
                    // highlighted half matches the side the moved surface
                    // will actually land on (release 2, task 3 — was
                    // deferred in task 2's report).
                    self.renderer
                        .set_split_preview(Some((sid, horizontal, 0.5, *before)));
                }
            }
            _ => {
                self.note_spring_hover(None);
                self.renderer.set_ghost_tab(None);
                self.clear_split_preview();
            }
        }
        // Keep the lifted tab chip following the cursor the whole time (the
        // crude "lift" visual this task reuses — a full ghost is Task 5's).
        // `build_tabs` clamps the visible position to the strip's own
        // bounds, so this stays a sane (if pinned-at-the-edge) cue even once
        // `x` has gone far outside the window during a cross-window carry.
        // Skipped for a pane drag (no chip equivalent yet — see above).
        if let Some(dragged_tab) = dragged_tab {
            self.renderer.set_tab_drag(Some((dragged_tab, x as f32)));
        }
        if let Some(drag) = shared.drag.as_mut() {
            drag.hover = hover;
        }
    }

    /// Set/clear THIS window's live cross-window drop preview — called by
    /// `App::update_cross_window_drag` when this window is (or stops being)
    /// the current hover TARGET of some OTHER window's drag. `title` is the
    /// carried surface's tab title (`DragState::title`, captured once at
    /// tear-off), used only by the `Strip` case's ghost tab. Mirrors the
    /// visual vocabulary `update_drag_hover` drives for an in-window hover: a
    /// sided `Edge` split preview, a near-full-pane `Center` tint (reusing
    /// the same split-preview quad with `ratio: 0.0` rather than adding a
    /// new one — the manual-split ratio floor caps the tinted region at 95%,
    /// not a hard 100%, but it reads clearly as "drop here"), or a wispy
    /// ghost tab on the strip (v0.4.0, replacing the old bare insertion
    /// caret — `set_ghost_tab`, always the LAST segment: v1 keeps append
    /// semantics, see `crate::paint::build_tabs`'s doc, so the ghost is
    /// honest about where the drop actually lands).
    ///
    /// Unlike `update_drag_hover`'s in-window Pane arm, this never needs a
    /// `hover_is_self_merge` check: `App::update_cross_window_drag` only
    /// ever calls this on a window whose id is NOT `drag.source_window` (it
    /// special-cases the carry drifting back over its own source as a
    /// hover-clear, not a call here) — a self-merge requires the same window
    /// on both ends, which is structurally unreachable through this path.
    pub(crate) fn set_incoming_drop(&mut self, hover: Option<DropHover>, title: Option<&str>) {
        self.incoming_drop = hover;
        self.clear_split_preview();
        self.renderer.set_tab_drag(None);
        // Deliberately NOT an unconditional `set_ghost_tab(None)` here before
        // the match (as an earlier version of this method had): every real
        // mouse motion tick over the strip calls this again with the SAME
        // Strip hover, and a `Some -> None -> Some` round trip on every one
        // of those ticks defeated `set_ghost_tab`'s own "same label ->
        // preserve the shimmer clock" dedup (it never got a chance to see
        // two `Some`s in a row) — the ghost's flicker looked frozen while
        // the pointer was moving, which is most of a live hover (live-test
        // finding). Each non-Strip arm below clears it explicitly instead,
        // so the ghost still disappears the instant the hover leaves the
        // strip.
        match hover {
            Some(DropHover::Strip { chip, .. }) => {
                // Spring-loaded tab select (finding #2), cross-window flavor:
                // dwelling on one of THIS (target) window's real chips
                // selects it here, exactly like the in-window path — the
                // ghost/append segment never does (chip is `None` there).
                self.note_spring_hover(chip);
                let label = title.filter(|t| !t.is_empty()).unwrap_or("＋");
                self.renderer.set_ghost_tab(Some(label.to_string()));
            }
            Some(DropHover::Pane {
                pane,
                zone: DropZone::Edge { axis, before },
                ..
            }) => {
                self.note_spring_hover(None);
                self.renderer.set_ghost_tab(None);
                if let Some(sid) = self.active_tab().root.session_of(pane).cloned() {
                    let horizontal = matches!(axis, Axis::Horizontal);
                    self.renderer
                        .set_split_preview(Some((sid, horizontal, 0.5, before)));
                }
            }
            Some(DropHover::Pane {
                pane,
                zone: DropZone::Center,
                ..
            }) => {
                self.note_spring_hover(None);
                self.renderer.set_ghost_tab(None);
                if let Some(sid) = self.active_tab().root.session_of(pane).cloned() {
                    self.renderer
                        .set_split_preview(Some((sid, true, 0.0, false)));
                }
            }
            Some(DropHover::Desktop) | None => {
                self.note_spring_hover(None);
                self.renderer.set_ghost_tab(None);
            }
        }
    }

    /// Clear this window's own drag-related visuals: the lifted tab chip,
    /// the ghost tab, the split preview, and any live cross-window
    /// incoming-drop preview. Safe to call on any window regardless of its
    /// role in a drag (source, target, or neither) — each individual clear
    /// already no-ops cheaply when nothing was set. Deliberately does NOT
    /// touch `self.morph` — see [`MorphAnim`]'s doc: a self-terminating
    /// suck-in/pour-out animation plays independently of this sweep (it's
    /// started right around where this is called — clearing it here would
    /// cut the very animation a drop/cancel just started).
    pub(crate) fn clear_drag_visuals(&mut self) {
        self.renderer.set_tab_drag(None);
        self.renderer.set_ghost_tab(None);
        self.clear_split_preview();
        self.incoming_drop = None;
        // A dwell that hadn't fired yet must die with the drag — without
        // this, a release/cancel within SPRING_LOAD_MS of touching a chip
        // would still flip the displayed tab ~150ms AFTER the drag ended.
        self.spring_load = None;
    }

    /// Resolve a torn-off drag on release: `Strip` hover on the SOURCE's own
    /// window → the same reorder a live in-strip drag commits progressively
    /// (applied here directly — an in-strip reorder is pure `LayoutCommand`
    /// bookkeeping on THIS window's own tree, nothing `apply_move` needs to
    /// arbitrate); a `Strip` hover on any OTHER window, or a `Pane` hover on
    /// this window OR another (release 2 task 3's cross-window addition), or
    /// `Desktop` (ditto) → builds the `(SurfaceRef, SurfaceDest)` pair and
    /// stashes it in `self.pending_move` for the caller to run through
    /// `apply_move` (the canonical surface-mobility path every other gesture
    /// — `move-tab`/`promote-pane`/`merge-tab` — already uses; see
    /// `pending_move`'s doc for why this method can't call it directly); no
    /// hover → cancel with zero mutation.
    fn resolve_drag_drop(
        &mut self,
        shared: &mut Shared,
        window_id: WindowId,
        drag: DragState,
    ) -> DragEnded {
        // Carry-time source vanish ends here, whatever the
        // outcome below turns out to be: `self` is this drag's SOURCE
        // window (mirrors this method's own existing convention — see the
        // `Cancel`/`Reorder` arms below already calling `self.start_pour_out`
        // and describing `self` as "the SOURCE rect"). Restores any hidden
        // OS window / filtered layout before a real move (if any) or the
        // pour-out below repaints over it — a stale exclusion must never
        // outlive the drag that armed it.
        self.clear_carried_exclusion(shared);
        self.renderer.set_tab_drag(None);
        self.clear_split_preview();
        let Some(src_w) = resolve_window_index(shared, drag.source_window) else {
            return DragEnded::Cancel; // shouldn't happen: this IS (or was) that window
        };
        match drag.surface {
            SurfaceRef::Tab { tab: src_tab, .. } => match drag.hover {
                Some(DropHover::Strip {
                    window, insert_at, ..
                }) if window == window_id => {
                    if src_tab >= self.tree.tabs.len() {
                        self.start_pour_out(shared, self.viewport(), self.cursor);
                        return DragEnded::Cancel;
                    }
                    let to = insert_at.min(self.tree.tabs.len() - 1);
                    if to != src_tab {
                        let vp = self.viewport();
                        apply(
                            &mut self.tree,
                            LayoutCommand::MoveTab { from: src_tab, to },
                            vp,
                        );
                    }
                    self.tree.active = to;
                    self.sync_layout(shared);
                    // Pour-out (v0.4.0): torn off, then dropped back onto its
                    // own strip — the surface "lands" here again, so it pours
                    // back into place exactly like a same-window Move would.
                    self.start_pour_out(shared, self.viewport(), self.cursor);
                    DragEnded::Reorder
                }
                Some(DropHover::Strip { window, .. }) => {
                    // Cross-window: append as a new tab of `window` (v1: append
                    // only — `insert_at` isn't honored positionally across
                    // windows yet, noted honestly rather than half-implemented).
                    let Some(w) = resolve_window_index(shared, window) else {
                        return DragEnded::Cancel; // target window vanished mid-drag
                    };
                    self.pending_move = Some((
                        SurfaceRef::Tab {
                            window: src_w,
                            tab: src_tab,
                        },
                        SurfaceDest::NewTab { window: w },
                    ));
                    DragEnded::Move
                }
                Some(DropHover::Pane {
                    window,
                    tab,
                    pane,
                    zone,
                }) => {
                    // Same-window or cross-window pane drop: both lower onto
                    // `apply_move` identically — Task 2 special-cased `window ==
                    // window_id` here for no functional reason (`w` resolved the
                    // same either way); merged into one arm.
                    let Some(w) = resolve_window_index(shared, window) else {
                        return DragEnded::Cancel; // target window vanished mid-drag
                    };
                    // Same-window `NewTab` is always rejected by `move_surface`
                    // as a no-op ("the tab's already in that window") —
                    // `apply_move` surfaces that as a humanized `Err`, which the
                    // caller turns into `Cancel`. A cross-window Center drop is
                    // a real append.
                    let dest = drop_zone_to_dest(zone, w, tab, pane);
                    self.pending_move = Some((
                        SurfaceRef::Tab {
                            window: src_w,
                            tab: src_tab,
                        },
                        dest,
                    ));
                    DragEnded::Move
                }
                Some(DropHover::Desktop) => {
                    // A brand-new window at (roughly) the drop point, minus the
                    // grab offset captured at tear-off — see `Shared::
                    // new_window_position_hint`'s doc for how `apply_move`
                    // threads this through to `open_window`.
                    let sf = self.renderer.window().scale_factor();
                    let (px, py) = desktop_drop_position(drag.last_screen, drag.grab, sf);
                    shared.new_window_position_hint =
                        Some(winit::dpi::PhysicalPosition::new(px, py));
                    self.pending_move = Some((
                        SurfaceRef::Tab {
                            window: src_w,
                            tab: src_tab,
                        },
                        SurfaceDest::NewWindow,
                    ));
                    DragEnded::Move
                }
                None => {
                    // Escape/no-hover cancel (v0.4.0): "the pour-out plays at
                    // the SOURCE rect — the surface went home."
                    self.start_pour_out(shared, self.viewport(), self.cursor);
                    DragEnded::Cancel
                }
            },
            SurfaceRef::Pane {
                tab: src_tab,
                pane: src_pane,
                ..
            } => {
                // A pane drag has no in-strip reorder equivalent (there's no
                // tab to reorder): ANY strip hover, own window's or
                // another's, promotes the pane to a new tab there — the
                // same "promote pane to tab"/"...to window" op release 1's
                // keyboard/menu path already exposes, just reached via a
                // drop. Edge/Center pane hovers reuse `drop_zone_to_dest`
                // exactly like the Tab arm; `move_surface`'s own `validate`
                // rejects the one genuinely degenerate case (an Edge hover
                // back over the pane's own source rect: "no-op: split into
                // self") — same "let core reject it, don't special-case it
                // here" convention the Tab arm's same-window `NewTab` case
                // already relies on.
                let dest = match drag.hover {
                    Some(DropHover::Strip { window, .. }) => resolve_window_index(shared, window)
                        .map(|w| SurfaceDest::NewTab { window: w }),
                    Some(DropHover::Pane {
                        window,
                        tab,
                        pane,
                        zone,
                    }) => resolve_window_index(shared, window)
                        .map(|w| drop_zone_to_dest(zone, w, tab, pane)),
                    Some(DropHover::Desktop) => {
                        let sf = self.renderer.window().scale_factor();
                        let (px, py) = desktop_drop_position(drag.last_screen, drag.grab, sf);
                        shared.new_window_position_hint =
                            Some(winit::dpi::PhysicalPosition::new(px, py));
                        Some(SurfaceDest::NewWindow)
                    }
                    None => None,
                };
                let Some(dest) = dest else {
                    // Cancel (v0.4.0): "the pour-out plays at the SOURCE
                    // rect — the surface went home."
                    self.start_pour_out(shared, self.viewport(), self.cursor);
                    return DragEnded::Cancel; // no hover, or the target window vanished mid-drag
                };
                self.pending_move = Some((
                    SurfaceRef::Pane {
                        window: src_w,
                        tab: src_tab,
                        pane: src_pane,
                    },
                    dest,
                ));
                DragEnded::Move
            }
        }
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
        let Some(grid) = self.renderer.grid(&sid) else {
            return;
        };
        let selection = AnchoredSelection::new(grid, Point::new(row, col), mode);
        self.sel = Some((sid, selection));
        self.selecting = true;
        self.renderer.set_selection(self.sel.clone());
        // Word/line clicks select immediately; capture their text right away.
        self.sel_snapshot = self.renderer.selected_text();
    }

    /// Extend the in-progress selection to a logical-px point (drag).
    pub(crate) fn extend_selection(&mut self, x: f64, y: f64) {
        let Some((sid, row, col)) = self.pixel_to_cell(x, y) else {
            return;
        };
        let mut changed = false;
        if let Some((ssid, selection)) = self.sel.as_mut() {
            if *ssid == sid {
                if let Some(grid) = self.renderer.grid(&sid) {
                    selection.update(grid, Point::new(row, col));
                    changed = true;
                }
            }
        }
        if changed {
            self.renderer.set_selection(self.sel.clone());
            // Refresh the copy snapshot while the rows are still on screen.
            self.sel_snapshot = self.renderer.selected_text();
        }
    }

    /// Clear any selection.
    /// A search reply from the backend: highlight the hit by anchoring the
    /// selection to its absolute coordinates (the display already scrolled to
    /// show it — the frame shipped before this event). `None` leaves any
    /// existing selection alone; the search bar reports "no match" itself.
    pub(crate) fn on_search_result(
        &mut self,
        session: &SessionId,
        hit: Option<ember_core::SearchHit>,
    ) {
        // Update the bar's "i / N" readout. A non-empty query with no hit =
        // no matches; ignore stale replies once the bar is closed.
        if self.search_open {
            self.search_count = match &hit {
                Some(h) => Some((h.ordinal, h.total)),
                None if !self.search_query.is_empty() => Some((0, 0)),
                None => None,
            };
            self.refresh_search_bar();
        }
        let Some(h) = hit else { return };
        let sel = AnchoredSelection {
            anchor: AbsPoint {
                line: h.start.0,
                col: h.start.1,
            },
            active: AbsPoint {
                line: h.end.0,
                col: h.end.1,
            },
            mode: SelectionMode::Simple,
        };
        self.sel = Some((session.clone(), sel));
        self.sel_snapshot = None;
        self.renderer.set_selection(self.sel.clone());
    }

    /// IME preedit update: track + draw the in-progress composition at the
    /// cursor, and position the OS candidate window there.
    pub(crate) fn set_ime_preedit(&mut self, text: String) {
        self.ime_preedit = text.clone();
        self.renderer
            .set_ime_preedit((!text.is_empty()).then_some(text));
        // Put the IME candidate window at the focused pane's cursor.
        if let Some(sid) = self.focused_session_id() {
            if let Some((_, rect)) = self.pane_rects.iter().find(|(s, _)| *s == sid) {
                if let Some(grid) = self.renderer.grid(&sid) {
                    let (cw, ch) = self.renderer.cell_metrics();
                    let cur = grid.cursor;
                    let x = rect.x as f32 + cur.col as f32 * cw;
                    let y = rect.y as f32 + (cur.row + 1) as f32 * ch;
                    self.renderer.window().set_ime_cursor_area(
                        winit::dpi::LogicalPosition::new(x, y),
                        winit::dpi::LogicalSize::new(cw, ch),
                    );
                }
            }
        }
    }

    /// IME commit: the composition is final — clear the overlay and send the
    /// committed text to the focused pane's PTY.
    pub(crate) fn ime_commit(&mut self, shared: &Shared, text: &str) {
        self.ime_preedit.clear();
        self.renderer.set_ime_preedit(None);
        if !text.is_empty() {
            self.send_to_focused(shared, text.as_bytes().to_vec());
        }
    }

    /// The palette's action list: every chord-bound entry of the shortcut
    /// cheat sheet (`help_lines`), fuzzy-filtered by `query` (case-insensitive
    /// subsequence). Returns `(description, chord)` rows.
    fn palette_rows(&self) -> Vec<(String, String)> {
        let q: Vec<char> = self.palette_query.to_lowercase().chars().collect();
        let fuzzy = |hay: &str| -> bool {
            if q.is_empty() {
                return true;
            }
            let mut it = q.iter();
            let mut want = it.next();
            for c in hay.to_lowercase().chars() {
                if Some(&c) == want {
                    want = it.next();
                    if want.is_none() {
                        return true;
                    }
                }
            }
            false
        };
        help_lines()
            .into_iter()
            .filter(|(k, _)| !k.is_empty() && parse_chord(&k.to_lowercase()).is_some())
            .filter(|(k, d)| fuzzy(d) || fuzzy(k))
            .map(|(k, d)| (d, k))
            .collect()
    }

    /// Open the command palette (Cmd+Shift+P). Typing-capture overlays are
    /// mutually exclusive: opening one closes the other, so keystrokes always
    /// have exactly one unambiguous destination (stacked capture overlays are
    /// how a terminal "stops responding to typing").
    pub(crate) fn open_palette(&mut self) {
        self.close_search();
        self.palette_open = true;
        self.palette_query.clear();
        self.palette_sel = 0;
        self.refresh_palette();
    }

    pub(crate) fn close_palette(&mut self) {
        self.palette_open = false;
        self.renderer.set_palette(None);
    }

    fn refresh_palette(&mut self) {
        let rows = self.palette_rows();
        self.palette_sel = self.palette_sel.min(rows.len().saturating_sub(1));
        self.renderer
            .set_palette(Some((self.palette_query.clone(), rows, self.palette_sel)));
    }

    /// One keystroke routed to the open palette. Returns `Some(chord)` when
    /// Enter picks an action - the CALLER dispatches it through the normal
    /// shortcut path (so follow-ups like closing an emptied window happen).
    pub(crate) fn palette_key(&mut self, key: &Key) -> Option<String> {
        match key {
            Key::Named(NamedKey::Escape) => self.close_palette(),
            Key::Named(NamedKey::ArrowDown) => {
                self.palette_sel += 1;
                self.refresh_palette();
            }
            Key::Named(NamedKey::ArrowUp) => {
                self.palette_sel = self.palette_sel.saturating_sub(1);
                self.refresh_palette();
            }
            Key::Named(NamedKey::Enter) => {
                let rows = self.palette_rows();
                if let Some((_, chord)) = rows.get(self.palette_sel) {
                    let chord = chord.to_lowercase();
                    self.close_palette();
                    return Some(chord);
                }
                self.close_palette();
            }
            Key::Named(NamedKey::Backspace) => {
                self.palette_query.pop();
                self.palette_sel = 0;
                self.refresh_palette();
            }
            Key::Named(NamedKey::Space) => {
                self.palette_query.push(' ');
                self.palette_sel = 0;
                self.refresh_palette();
            }
            Key::Character(text) => {
                self.palette_query.push_str(text);
                self.palette_sel = 0;
                self.refresh_palette();
            }
            _ => {}
        }
        None
    }

    /// Open the scrollback-search bar (Cmd+F). Closes the palette first —
    /// see [`Self::open_palette`] on capture-overlay exclusivity.
    pub(crate) fn open_search(&mut self) {
        self.close_palette();
        self.search_open = true;
        self.refresh_search_bar();
    }

    /// Close the bar. The current hit's highlight (an anchored selection)
    /// stays, matching iTerm2 — Escape again / a click clears it.
    pub(crate) fn close_search(&mut self) {
        self.search_open = false;
        self.search_query.clear();
        self.search_count = None;
        self.renderer.set_search_bar(None);
    }

    /// One keystroke routed to the open search bar: printable chars edit the
    /// query (searching incrementally), Enter = next match, Shift+Enter =
    /// previous, Backspace edits, Escape closes.
    pub(crate) fn search_key(&mut self, shared: &Shared, key: &Key) {
        match key {
            Key::Named(NamedKey::Escape) => self.close_search(),
            Key::Named(NamedKey::Enter) => {
                let forward = !self.modifiers.shift_key();
                self.submit_search(shared, forward);
            }
            Key::Named(NamedKey::Backspace) => {
                self.search_query.pop();
                self.search_count = None;
                self.refresh_search_bar();
                if !self.search_query.is_empty() {
                    self.submit_search(shared, true);
                }
            }
            Key::Named(NamedKey::Space) => {
                self.search_query.push(' ');
                self.search_count = None;
                self.refresh_search_bar();
                self.submit_search(shared, true);
            }
            Key::Character(text) => {
                self.search_query.push_str(text);
                self.search_count = None;
                self.refresh_search_bar();
                self.submit_search(shared, true);
            }
            _ => {}
        }
    }

    fn refresh_search_bar(&mut self) {
        let line = if self.search_query.is_empty() {
            "find: type to search".to_string()
        } else {
            let count = match self.search_count {
                Some((_, 0)) => "  no matches".to_string(),
                Some((i, n)) => format!("  {i}/{n}"),
                None => String::new(), // searching…
            };
            format!("find: {}\u{2038}{}", self.search_query, count)
        };
        self.renderer.set_search_bar(Some(line));
    }

    fn submit_search(&mut self, shared: &Shared, forward: bool) {
        if self.search_query.is_empty() {
            return;
        }
        if let Some(h) = self.focused_session(shared) {
            let _ = h.control.send(ember_core::BackendControl::Search {
                pattern: self.search_query.clone(),
                forward,
            });
        }
    }

    pub(crate) fn clear_selection(&mut self) {
        if self.sel.is_some() {
            self.sel = None;
            self.sel_snapshot = None;
            self.selecting = false;
            self.renderer.set_selection(None);
        }
    }

    /// Copy the current selection's text to the OS clipboard (Cmd+C).
    pub(crate) fn copy_selection(&mut self, shared: &mut Shared) {
        // The snapshot (captured while the selection's rows were visible) is
        // authoritative: it stays correct even after the text scrolls out of
        // the viewport. Live read is the fallback for stale-snapshot edges.
        let text = self
            .sel_snapshot
            .clone()
            .filter(|t| !t.is_empty())
            .or_else(|| self.renderer.selected_text());
        if let Some(text) = text {
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
    ///
    /// Sparks guardrails (v0.3.1): the `backdrop_animating` gate below is the
    /// ONLY place `time` advances for the sparks. When Reduce Motion (or the
    /// `off` dial, or an unfocused window under `focused`) makes that gate
    /// false, this whole block is simply skipped — the renderer keeps
    /// whatever `BackdropParams` it was last given by `apply_appearance`
    /// (startup, or the last settings change), sparks included. That's the
    /// entire "freeze" implementation: no separate frozen-frame code path,
    /// no extra redraw to force one — which is also why it can never add a
    /// tick to `next_wake` on its own.
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
        // Ghost tab shimmer (v0.4.0): re-mark the scene dirty each tick so
        // the procedural flicker keeps advancing even while the pointer
        // isn't moving — a no-op when no ghost is showing.
        self.renderer.touch_ghost();
    }

    /// Whether an incoming-drag ghost tab is currently showing on this
    /// window's strip — `about_to_wait`'s per-window animation gate ORs this
    /// in alongside `backdrop_animating` so the shimmer keeps ticking.
    pub(crate) fn ghost_active(&self) -> bool {
        self.renderer.ghost_active()
    }

    /// Whether the ember sparks should be animating right now (sparks
    /// guardrails, v0.3.1). Gated by the sparks dial:
    /// - `Off`: never.
    /// - `Focused` (the shipping default): only in the focused window — the
    ///   campfire burns where you're looking, not behind your back.
    /// - `Always`: whenever the window is visible, focused or not — this is
    ///   where the OLD unconditional "campfire burns while you work
    ///   elsewhere" behavior (Brandon's original 2026-07-04 call) now lives,
    ///   as an opt-in rather than the default.
    ///
    /// Two OS-level guardrails override the dial on top of that: macOS Low
    /// Power Mode collapses it to `Off` (no animation, sparks not even drawn
    /// — see `Shared::backdrop_params`), and Reduce Motion freezes the
    /// animation without hiding already-visible sparks (they hold their last
    /// phase; see the module-level note on `advance_animations` for why no
    /// extra code is needed to make that "freeze" happen). A modal overlay
    /// or an occluded/asleep window still goes fully quiet regardless.
    pub(crate) fn backdrop_animating(&self, shared: &Shared) -> bool {
        if self.occluded || self.help || self.about || self.settings_open {
            return false;
        }
        if shared.low_power_mode() || shared.reduce_motion() {
            return false;
        }
        match shared.config.background.sparks {
            SparksMode::Off => false,
            SparksMode::Focused => self.window_focused,
            SparksMode::Always => true,
        }
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

#[cfg(test)]
mod tests {
    use super::{TEAR_OFF_THRESHOLD, strip_band_exit};

    #[test]
    fn strip_band_exit_stays_false_inside_the_band() {
        let strip_bottom = 32.0;
        assert!(!strip_band_exit(
            strip_bottom,
            strip_bottom,
            TEAR_OFF_THRESHOLD
        ));
        assert!(!strip_band_exit(
            strip_bottom + TEAR_OFF_THRESHOLD - 1.0,
            strip_bottom,
            TEAR_OFF_THRESHOLD
        ));
        // Exactly on the threshold is still "in the strip" (strict `>`,
        // matching every other edge-band convention in this codebase).
        assert!(!strip_band_exit(
            strip_bottom + TEAR_OFF_THRESHOLD,
            strip_bottom,
            TEAR_OFF_THRESHOLD
        ));
    }

    #[test]
    fn strip_band_exit_trips_just_past_the_threshold() {
        let strip_bottom = 32.0;
        assert!(strip_band_exit(
            strip_bottom + TEAR_OFF_THRESHOLD + 0.01,
            strip_bottom,
            TEAR_OFF_THRESHOLD
        ));
        assert!(strip_band_exit(1000.0, strip_bottom, TEAR_OFF_THRESHOLD));
    }

    #[test]
    fn strip_band_exit_false_above_the_strip() {
        // A pointer still inside (or above) the strip is never "below" it,
        // regardless of the threshold.
        assert!(!strip_band_exit(0.0, 32.0, TEAR_OFF_THRESHOLD));
        assert!(!strip_band_exit(-50.0, 32.0, TEAR_OFF_THRESHOLD));
    }

    #[test]
    fn strip_band_exit_respects_a_custom_threshold() {
        assert!(!strip_band_exit(10.0, 0.0, 12.0));
        assert!(strip_band_exit(13.0, 0.0, 12.0));
    }

    #[test]
    fn revert_target_tab_falls_back_to_the_nearest_survivor() {
        use super::revert_target_tab;
        // Middle tab dragged → the one just before it.
        assert_eq!(revert_target_tab(4, 2), Some(1));
        // Dragged tab is index 0 → nothing before it, so the one after.
        assert_eq!(revert_target_tab(4, 0), Some(1));
        // Last tab dragged → the one just before it.
        assert_eq!(revert_target_tab(4, 3), Some(2));
        // Only one tab left (the dragged one itself) → nothing to revert to.
        assert_eq!(revert_target_tab(1, 0), None);
    }

    #[test]
    fn drop_zone_to_dest_maps_edge_to_split_into_with_axis_and_side() {
        use super::drop_zone_to_dest;
        use ember_core::{Axis, DropZone, PaneId, SurfaceDest};

        let left = drop_zone_to_dest(
            DropZone::Edge {
                axis: Axis::Horizontal,
                before: true,
            },
            2,
            5,
            PaneId(9),
        );
        assert_eq!(
            left,
            SurfaceDest::SplitInto {
                window: 2,
                tab: 5,
                pane: PaneId(9),
                axis: Axis::Horizontal,
                before: true,
            }
        );

        let bottom = drop_zone_to_dest(
            DropZone::Edge {
                axis: Axis::Vertical,
                before: false,
            },
            0,
            0,
            PaneId(1),
        );
        assert_eq!(
            bottom,
            SurfaceDest::SplitInto {
                window: 0,
                tab: 0,
                pane: PaneId(1),
                axis: Axis::Vertical,
                before: false,
            }
        );
    }

    #[test]
    fn drop_zone_to_dest_maps_center_to_new_tab_of_the_hovered_window() {
        use super::drop_zone_to_dest;
        use ember_core::{DropZone, PaneId, SurfaceDest};

        let dest = drop_zone_to_dest(DropZone::Center, 3, 7, PaneId(4));
        assert_eq!(dest, SurfaceDest::NewTab { window: 3 });
    }

    #[test]
    fn desktop_drop_position_subtracts_the_scaled_grab_offset() {
        use super::desktop_drop_position;
        // A drop at physical (500, 400), grabbed 10 logical px right of and
        // 4 below the tab's origin, at 2x scale → (500 - 20, 400 - 8).
        assert_eq!(
            desktop_drop_position((500.0, 400.0), (10.0, 4.0), 2.0),
            (480, 392)
        );
    }

    #[test]
    fn desktop_drop_position_clamps_to_never_go_negative() {
        use super::desktop_drop_position;
        // A grab offset larger than the drop point itself would otherwise
        // push the new window off-screen negative — clamp to (0, 0)-ish.
        assert_eq!(
            desktop_drop_position((10.0, 10.0), (100.0, 100.0), 1.0),
            (0, 0)
        );
        // Only one axis clamps.
        assert_eq!(
            desktop_drop_position((10.0, 500.0), (100.0, 50.0), 1.0),
            (0, 450)
        );
    }

    #[test]
    fn pane_drag_source_picks_the_pane_in_a_multi_pane_tab() {
        use super::pane_drag_source;
        use ember_core::{PaneId, SurfaceRef};

        assert_eq!(
            pane_drag_source(1, 2, PaneId(9), 2),
            SurfaceRef::Pane {
                window: 1,
                tab: 2,
                pane: PaneId(9),
            }
        );
        // Any panes_in_tab > 1 keeps the Pane ref, not just 2.
        assert_eq!(
            pane_drag_source(0, 0, PaneId(3), 5),
            SurfaceRef::Pane {
                window: 0,
                tab: 0,
                pane: PaneId(3),
            }
        );
    }

    #[test]
    fn pane_drag_source_promotes_to_a_tab_ref_for_a_sole_pane_tab() {
        use super::pane_drag_source;
        use ember_core::{PaneId, SurfaceRef};

        assert_eq!(
            pane_drag_source(1, 2, PaneId(9), 1),
            SurfaceRef::Tab { window: 1, tab: 2 }
        );
        // A degenerate 0 also reads as "not multi-pane" — still Tab, never
        // panics/underflows.
        assert_eq!(
            pane_drag_source(4, 0, PaneId(7), 0),
            SurfaceRef::Tab { window: 4, tab: 0 }
        );
    }
}
