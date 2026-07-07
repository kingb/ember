# Multi-window + keyboard surface mobility — design

Status: approved direction (2026-07-06); spec pending review.

## Vision context (two releases)

The end goal is full surface mobility — pane ↔ tab ↔ window and back — with
drag, snapping, and an animated drag token (a glowing ember-wisp that a
grabbed surface gets pulled into, carried across the desktop, and poured out
of at the drop target). It ships as two releases:

1. **This spec: "it works."** Multiple OS windows plus the complete mobility
   matrix via keyboard, menus, and the control surface.
2. **Next spec: "it's magic."** Drag + snap + the wisp, built on this
   release's re-parenting model. No intermediate plain-drag ghost ships.

Everything here is designed so release 2 is additive: the core operation is
"re-parent a *surface*" (a pane subtree or a whole tab), not "move a tab."

## UX (release 1)

### Operations

| Operation | Effect | macOS | Linux (GNOME-safe) |
|---|---|---|---|
| New Window | fresh tab; cwd inherited from the focused pane (same rule as splits) | `Cmd+N` | `Ctrl+Shift+N` |
| Move Tab to New Window | this tab becomes a new window's only tab | `Cmd+Shift+N` | `Alt+Shift+N` |
| Move Tab to Next / Previous Window | tab leaves this window, lands active in the target | menu + ctl only (v1) | menu + ctl only (v1) |
| Promote Pane to Tab | focused split becomes its own tab in this window | `Cmd+Opt+T` | `Alt+Shift+T` |
| Promote Pane to New Window | focused split becomes a new window's only tab | menu + ctl only (v1) | menu + ctl only (v1) |
| Merge Tab into Previous Tab (as split) | this tab's tree becomes a split inside the previous tab | menu + ctl only (v1) | menu + ctl only (v1) |

- The table closes the pane↔tab↔window cycle in both directions.
- Keybinds are proposals, flagged for review; the less-common operations are
  menu/ctl-only in v1 to avoid binding sprawl (release 2's drag makes them
  gestural anyway). All six get menu items and control-surface verbs.
- **Linux keybind rule** (learned from the Super+1..9 GNOME collision, issue
  #5): nothing new binds on bare Super. Linux bindings use `Ctrl+Shift+…` /
  `Alt+Shift+…`, audited against GNOME defaults.
- Sessions/PTYs survive every operation untouched; a move is a tree edit,
  never a respawn.
- Closing a window closes its tabs (with the existing running-process
  confirmation). Closing the last window quits the app on both platforms
  (current behavior, kept for v1; the macOS stay-running convention is a
  possible follow-up).
- Every window is a peer: same config, own tab strip, own overlays (Settings
  opens in the window that asked), own focus. `Cmd+,` etc. act on the window
  that received the key.

## Model

- `Windows` (new, ember-core): an ordered list of `WindowTree`s plus the
  focused-window index. `WindowTree` already models one window's tabs — it
  was named for this. Sessions/PTY handles stay in a global registry keyed
  by `SessionId`, owned above any window.
- One new core operation, pure and unit-testable:
  `move_surface(windows, src: SurfaceRef, dst: SurfaceDest) -> Result<Effects>`
  where `SurfaceRef` names a pane subtree or a whole tab, and `SurfaceDest`
  is one of `{NewTab(window), TabOfWindow(window, index), NewWindow,
  SplitInto(window, tab, pane, axis)}`. All six UX operations lower onto it,
  and release 2's drops lower onto the SAME function.
- Invariants (enforced + tested): a window always has ≥1 tab (moving the
  last tab out closes the window); a tab always has ≥1 pane; session ids are
  never duplicated or dropped by any move; focus lands somewhere sensible on
  both sides of a move.

## Architecture

- **App-layer split** (the big refactor): today's `RunState` assumes one
  window. It splits into `Shared` (config, session registry + PTY backends,
  platform seam, control socket, clipboard, marks) and `WindowState`
  (renderer + GPU surface, tab tree view, overlays, hover/drag bookkeeping,
  pointer/focus state, occlusion + redraw gating). The winit
  `ApplicationHandler` routes every `WindowEvent` by `WindowId` to the right
  `WindowState`; `about_to_wait` drains shared queues (PTY deltas, control
  messages) and dispatches to the owning window.
- **Renderer per window**, and for v1 a font system + glyph atlas per window
  (isolation, zero locking). Memory cost is real but bounded (measured
  before ship; optimization to a shared font system is a flagged follow-up,
  not a blocker).
- **Occlusion/redraw invariants hold per window**: each window independently
  gates redraws on its own occlusion/visibility state. The render-loop
  regressions this codebase has already paid for (idle spin, starved-frame
  latching) get an explicit two-window soak test before release.
- **PTY delta routing**: a session's deltas go to whichever window currently
  hosts it — the registry maps `SessionId -> WindowId` and is updated by
  `move_surface` effects.
- **Menus**: macOS gets a real Window menu (New Window, the mobility items,
  window list); Linux keeps keybinds + ctl.

## Control surface

- `state` grows `windows: [{id, focused, tabs: [...]}]` — the existing tabs
  array nests one level down. `active_tab` etc. remain per window. This is a
  breaking change to `state` consumers; called out in the changelog like the
  tabs-array change was.
- `ctl focus <query>` searches ALL windows' tab titles and raises the window
  that wins — the Stream Deck workflow gets cross-window jump for free.
- New verbs mirroring each operation (`new-window`,
  `move-tab --to-window N|new`, `promote-pane`, `merge-tab`), which is also
  how CI exercises the matrix headless.
- `--screenshot` stays single-window; a `--window N` selector is a nice-to-
  have, not v1.

## De-risking and testing

1. **Spike gate (before the refactor lands):** a throwaway branch proving
   two winit windows + two wgpu surfaces render Ember panes on macOS and
   Linux (X11 + Wayland), including per-window occlusion events. If winit
   multi-window has platform landmines, we learn in a day, not mid-refactor.
2. **Pure-op tests:** the whole mobility matrix (all `SurfaceRef` ×
   `SurfaceDest` combinations, plus the invariants above) as unit tests on
   `move_surface` — no GPU, no window.
3. **Live verification via ctl:** create two windows, move tabs/panes every
   direction, `state` reflects the topology at each step, screenshots per
   window where the capture path allows.
4. **Occlusion soak:** two windows, one occluded/minimized, heavy output in
   both; assert no render spin and no starved-frame freeze (the July
   regression class).
5. `ctl focus` cross-window behavior verified against the Stream Deck use
   case shape (title in window 2 while window 1 is focused).

## Non-goals (release 1)

- All dragging, snapping, drop zones, tear-off gestures, and the wisp
  (release 2, its own spec).
- Session/window restore across app restarts.
- Window position/size persistence.
- Per-window configuration profiles.
- Shared-font-system memory optimization (follow-up if profiling says so).

## Open items flagged for spec review

- Final keybind choices in the table above (especially `Cmd+Shift+N`, which
  browsers use for incognito — alternatives welcome).
- Whether "Move Tab to Next/Previous Window" deserves a keybind in v1.
