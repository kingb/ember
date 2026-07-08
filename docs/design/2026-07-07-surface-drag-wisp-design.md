# Surface drag, snap & the wisp — design (release 2)

Status: vision approved 2026-07-06 ("pane to tab to window and back, with
dragging and snapping… sucked into a wisp"); this spec pins the concrete
gestures and mechanics. Builds entirely on release 1's `move_surface`
(every drop lowers onto the same pure operation the keyboard uses).

## The experience

Grab a surface and pull: it resists elastically for a few pixels, then
snaps free and is **sucked into a wisp** — a small glowing ember that rides
the pointer, trailing sparks. Carry it anywhere on the desktop. Over a drop
target the wisp brightens and the target shows a snap preview; release and
the wisp **pours out** into place. Release over nothing and the surface
becomes a new window right there; press Escape mid-drag and the wisp flies
home and pours back.

## Gestures (v1 of release 2)

| Drag source | How it starts |
|---|---|
| Tab | the existing tab drag, once the pointer leaves the strip's band (inside the band it stays the existing reorder) |
| Pane | `Cmd+Opt+drag` (macOS) / `Ctrl+Alt+Shift+drag` (Linux) on the pane body — chord-gated so plain drag stays text selection (Linux adds Shift because bare Ctrl+Alt is the split-preview gesture, which arms on pointer motion and would always win) |

| Drop target | Result (all via `move_surface`) |
|---|---|
| Another window's tab strip | tab lands at the drop position (activates) |
| A pane's edge zones (N/S/E/W quadrant) | split into that pane along the matching axis |
| A pane's center | append as a new tab of that window |
| Empty desktop | new window at the drop point |
| Own strip (tab source) | existing reorder, unchanged |

Escape cancels. The pane-source case with a single-pane tab is treated as a
tab drag (release 1's `WouldEmptyTab` rule made this explicit).

## The wisp

- A dedicated tiny OS window: transparent, decorationless, always-on-top,
  non-focusable, ~140 logical px square, spawned when a drag crosses out of
  the source window's bounds (until then the drag renders in-window).
- Contents: a particle cluster reusing the ember-sparks renderer (attractor
  at center, short trail against drag velocity) around a glowing core, with
  a one-line label (the tab/pane title) that fades after a beat.
- It follows the pointer via the source window's captured `CursorMoved`
  stream translated to screen coordinates (during a button-held drag the
  source window keeps receiving motion on macOS and X11), positioning the
  wisp window each frame.
- Suck-in: ~150 ms — the surface's rect shrinks toward the grab point while
  particles emit inward. Pour-out: ~200 ms reversed at the target. Both
  skipped under reduced-motion (instant transfer, wisp becomes a plain
  outline).
- Drop hit-testing: screen-space point against our own windows' frames
  (`outer_position`+`outer_size`), topmost-first by focus order; inside a
  window, the existing split-preview quadrant math picks the zone; the
  target window renders the snap preview through its own renderer.
- Degradation ladder (feature-detected at first drag): no always-on-top or
  no transparency support → skip the wisp window; the drag shows as the
  existing lifted-tab/outline inside source + target windows only, drops
  still work everywhere. The wisp is presentation; the mechanics never
  depend on it.

## Architecture notes

- `DragState` on the app: `{ surface: SurfaceRef, grab_offset, phase:
  InWindow | Carried { wisp: WindowId }, hover: Option<DropZone> }` owned by
  `Shared` (a drag spans windows by definition).
- The wisp window is a `WindowState`-less special: its own tiny renderer
  (sparks pipeline only — no text atlas, no grid) to keep per-drag cost
  trivial; created lazily on first tear-off, hidden (not destroyed) between
  drags.
- Drop zones reuse `set_split_preview` (pane quadrants) and the tab strip's
  existing insertion-index math; "new window at drop point" passes a
  position hint to `open_window`.
- Everything lands through `apply_move` (release 1's effect applier), so
  session survival, style replay, focus, and window lifecycle are already
  proven paths.

## v1 trims (shipped behavior vs. this document)

The first implementation ships the mechanics in full and trims three
presentation details described above: the wisp carries no text label, the
suck-in/pour-out is an intensity fade rather than a rect morph, and a
cross-window strip drop appends as the last tab rather than landing at the
pointer's position. All three are follow-up candidates, none affect where
surfaces land or session survival.

## Hold-to-wisp (v1.1, from first live sessions)

Hotkey-free pane grabbing: press and HOLD left on a pane body without
moving. After a short arm delay a thin accent ring draws itself clockwise
around the cursor; when the ring completes, the pane is wisped away (the
suck-in) and the drag is live, carried, exactly as if chord-dragged. Moving
past a small tolerance before the ring completes cancels the ring and the
press falls back to what it always was (selection drag / mouse-mode
forwarding); releasing early is a normal click. Starting numbers, all
tunable live: arm 300 ms, ring sweep 600 ms, tolerance 6 logical px.
Mouse-mode apps got the press forwarded at press time, so wisp-away sends
the matching synthetic release to the PTY before the carry starts.

The strip's empty background is also a drag handle for the ACTIVE tab, so a
window is grabbable even where its strip has no chip under the pointer (and
the lone-tab chip stays visible as the discoverable affordance).

## Non-goals (this release)

- Multi-surface drags, drag-out to OTHER apps (no OS drag-and-drop payload),
  Wayland global cursor tracking beyond what winit provides (Wayland may sit
  on the degradation ladder for cross-window drops in v1 — verified during
  implementation and documented honestly in the changelog if so).
- Undo/redo of drops (Ghostty has it; ours is a follow-up).
- Touch/gesture input.

## Verification

- Pure: drop-zone quadrant math + screen-space hit-test order as unit tests.
- Live (ctl): scripted press-move-release drags via a new `ctl drag x1 y1 x2
  y2 [--chord]` verb (press+motion+release through the same handlers), tab
  tear-off → second window appears; pane drag into other window splits it;
  Escape cancel restores; sessions alive throughout.
- Visual: screenshots of snap previews; the wisp itself verified by eye on
  macOS (its window is out of screenshot reach of the pane capture — noted).
- Reduced-motion path exercised via the accessibility setting.
