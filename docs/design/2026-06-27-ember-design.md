# iTerm2-on-Linux — Native Reimplementation Design

- Date: 2026-06-27
- Status: Design approved; ready for implementation planning
- Scope: A native, from-scratch Linux terminal emulator that reproduces the iTerm2
  experience as a daily-driver replacement.

## 1. Overview and goals

The objective is a native Linux terminal emulator built from scratch in Rust, with
real iTerm2 as the experiential specification. This is a reimplementation, not a port
of iTerm2's macOS/Objective-C source and not an extension of an existing terminal.

**North star:** fidelity to the iTerm2 experience, sufficient to serve as a daily-driver
replacement on Linux. The build is multi-phase and multi-subsystem.

### v1 feature scope (tiered)

- **Table-stakes:** split panes; tab rename + drag-reorder; profiles; regex search.
- **Big-ticket:** shell integration (top priority); `tmux -CC` (deferred to phase 2).
- **Mid-tier:** smart selection + semantic history; hotkey/Quake window; triggers
  (regex → action).
- **Deferred:** instant replay; status bar (stretch); scripting API.

## 2. Architecture

Layered, single-process. A future daemon/multi-process split is deferred, but its
boundary is front-loaded so it can be introduced later without reshaping the core.

### Workspace crates

| Crate | Responsibility | Depends on |
|---|---|---|
| `ember-core` | Pure domain: `SessionBackend` trait, `SessionCommand`, layout tree, focus/layout, profiles, OSC/trigger **matching**. No tokio/IO/winit/wgpu. | — |
| `ember-session` | Backend impls: `LocalPty` (v1), `TmuxControlMode` (phase 2), `a future out-of-process backend` (future). | `ember-core` |
| `ember-render` | wgpu + glyphon + custom GPU chrome + egui overlay. Consumes the neutral grid by owned delta. | `ember-core` |
| `ember-platform` | winit + `PlatformBackend` (hotkey, open-path, clipboard IO). macOS seam. | `ember-core` |
| `ember-app` | Binary: event loop, input routing, layout, config; trigger **dispatch**. | all |

### Two seams

- **`SessionBackend`** — the tmux / daemon / backend extension point.
- **`PlatformBackend`** — the OS / macOS extension point.

### Layering and data flow

```
            input events                     OS effects
   winit ─────────────► ember-app ──────────────────────► ember-platform
                          │  │                         (clipboard, open,
            SessionCommand│  │ LayoutCommand            hotkey, notify)
                          ▼  │
   ember-session ── owned ───┐  │
   (emulation thread)     │  ▼
     PTY bytes ─► engine  │  ember-core (layout tree, focus,
     ─► projection fn ────┤   profiles, matchers — pure)
        owned delta       │  │
                          ▼  ▼
                       ember-render ──► GPU surface
                  (owns neutral grid; cell grid +
                   native chrome + egui overlay)
```

The core invariant: pure domain logic in `ember-core` performs no IO, so whole classes of
failure cannot originate there, and the layout/matching logic is exhaustively testable
without a running system.

## 3. Technology stack

Validated by a dedicated library-validation research pass (20 sources, 95 claims, 25
verified). The candidate stack is the exact stack shipped by COSMIC Terminal (the
Pop!_OS default), which de-risks the combination as a whole.

| Layer | Choice | Notes |
|---|---|---|
| Windowing | `winit` | Bare window + event loop; all chrome is drawn by us. |
| GPU | `wgpu` | Anti-`wgpu` latency claims were refuted; present mode is the real lever (§6). |
| Text | `glyphon` / `cosmic-text` | Ligatures, emoji, complex scripts, fallback (HarfRust + swash). |
| VT engine | `alacritty_terminal` (v1), **kept swappable** | `libghostty-vt` evaluable phase-2 (§4). |
| PTY | `portable-pty` | ~160 lines of host boilerplate; pin `alacritty_terminal` (API churn). |
| Chrome UI | hand-rolled `wgpu` + `egui` (hybrid) | §6. |

### VT engine swappability

`alacritty_terminal` is the v1 VT engine, but the engine is kept swappable behind a
neutral grid type (§4). The motivation is `libghostty-vt` (Ghostty's C-ABI core,
wrappable from Rust today via the `libghostty-vt` crate): it ships Kitty-graphics and
`tmux -CC` *parsing* that alacritty lacks — a strong phase-2 pull. It is not a v1 bet:
its handles are `!Send + !Sync` (forcing one-engine-per-thread, which we adopt anyway),
it is pre-1.0, and it carries a Zig build dependency. We do not bet v1 on it; we seam it
in. The `tmux -CC` *client* is our own build regardless, since a parser is not a client.

## 4. SessionBackend layer

`SessionBackend` abstracts "a thing that runs a session and produces a renderable grid."
Its defining invariant is **zero PTY-ness**: the trait never exposes a file descriptor.
It is bytes-in, owned-events-out — never grid-borrow-out.

### Three faces

1. **Command-in** — `apply(SessionCommand)`, where `SessionCommand` is a data-only serde
   enum (`Input(bytes)`, `Resize`, `Kill`, …). It serializes unchanged onto the backend
   bus later.
2. **Grid-out as a projection** — not a borrowed shared snapshot (see below).
3. **Events-out** — a two-lane, implementor-owned sink (channels handed in at
   construction).

### Grid seam: projection, not shared buffer

The engine owns its **native** grid and mutates it in place (alacritty's `Grid<Cell>`,
or libghostty's own representation) — neither leaks past the seam. Once per frame, a
per-engine **projection function**, running on the engine's own thread, applies the
engine's native damage set into a **render-owned neutral grid** and emits an **owned,
`Send` delta** over a channel. Render owns the neutral grid and never touches engine
memory. Cost is O(damaged cells), not O(viewport).

This deliberately rejects a shared triple-buffer / atomic-swap model: a shared buffer is
still shared memory across threads, and alacritty's `Term` mutates one grid in place —
forcing it to write into a swappable external back-buffer would require either a
per-frame deep clone (the cost we are eliminating) or invasive surgery on `Term`.

- `NeutralCell` carries style as a small **interned style-id** (fg/bg/attrs/font key),
  not a fat per-cell struct. Render keys its glyph-raster cache on `(glyph, style-id)`,
  so unchanged glyphs skip rasterization even inside damaged cells.
- Damage is tracked at **line + cell-range** granularity, matching alacritty's
  `TermDamage`; the projection never derives finer damage than the engines natively
  report.
- **Swappability lives in the projection function.** alacritty-v1 and libghostty-phase-2
  differ only in which projection function is compiled.

### Event sink: two lanes, channel of owned enums

The sink is a channel of owned, `Send` enum variants — not a callback trait (a callback
re-imports lifetime/thread coupling and the `!Sync` problem). Two lanes with different
delivery semantics:

- **Pixel lane** — single-slot, **latest-wins** mailbox for `FrameReady(delta)`. If
  render falls behind, damage coalesces in the engine's neutral grid; there is never an
  unbounded queue of frame deltas.
- **Semantic lane** — **ordered, reliable** queue for title, bell, OSC, clipboard, exit,
  and (when libghostty lands) Kitty-graphics and `tmux -CC` payloads. These must not be
  dropped or reordered.

### Threading: `!Send`/`!Sync` as a protector

Each backend runs a dedicated **emulation thread per pane** that owns the VT engine. The
`!Send + !Sync` constraint of a future libghostty backend does not merely permit this —
it forecloses the `Arc<Mutex<Grid>>` shared-lock model, which was the primary
seam-erosion risk (render reaching into engine memory under a lock, eroding under perf
pressure). The projection's owned `Send` delta is the inter-thread message.

### The swappable-engine contract

Two type signatures constitute the swappable-engine contract and must be written into
this spec before render is implemented:

1. the **projection-function signature**, and
2. the **two-lane `BackendEvent` enum**.

These are to be drafted (in collaboration with the design review) and sanity-checked
against what `libghostty-vt` 0.2.0 actually hands over per cell — specifically its cell
representation and whether its damage is line- or cell-granular, the one place the model
could meet a surprise.

### Implementations (drop-ins)

- **`LocalPty`** (v1) — `portable-pty` + an emulation thread driving alacritty's `Term`
  into the neutral grid via the projection function.
- **`TmuxControlMode`** (phase 2) — the "PTY" is a tmux control-mode connection.
- **`a future out-of-process backend`** (future) — the neutral grid is fed from its high-fidelity
  view stream; no PTY.

Identity is a `SessionId` string newtype: `LocalPty` fills its own id; `a future out-of-process backend`
fills the bus `AgentRef`.

## 5. Multiplexer / UI model (`ember-core`)

The structured "what is on screen" layer, kept pure (no winit/wgpu/IO). `ember-app` owns an
instance and drives it; `ember-render` reads it; the platform and session layers receive the
resulting side effects.

### Layout tree

`Window → Tabs → binary split tree`. A `LayoutNode` is either:

- `Split { axis: Horizontal | Vertical, ratio, a, b }` (binary), or
- `Pane { id: PaneId, session: SessionId }` (leaf).

Binary-with-ratio composes to any iTerm2 layout (it mirrors nested split views) and keeps
resize and close/promote math uniform. Layout is a pure function
`(LayoutNode, viewport_rect) → Vec<(PaneId, Rect)>`, which render consumes and which also
tells each `SessionBackend` what size to be (a `Resize` `SessionCommand`).

### Focus and navigation

Active window → active tab → exactly one focused pane. Directional focus
(`left/right/up/down`) is computed **geometrically** from the laid-out rects, so movement
does the visually-correct thing across arbitrary nesting.

### Mutations as data

A `LayoutCommand` enum (`SplitPane`, `ClosePane`, `FocusDir`, `NewTab`, `MoveTab`,
`RenameTab`, `ResizeSplit`, …) follows the same data-only/serde discipline as
`SessionCommand`, so it can ride the bus later. Applying a command emits the deltas that
drive side effects: closing a pane kills its session; a split or resize re-runs layout and
issues `Resize` to affected backends.

### Parallel chrome/gate surface

A native terminal front-end has two kinds of on-screen rows, and conflating them into the
pane tree is a trap. `AppState` therefore holds **two sibling surfaces**, not one tree:

- `layout: WindowTree` — agent panes (session-backed, above).
- `chrome: ChromeState` + `gates: GateRegistry` — PTY-less structured rows (rail /
  timeline / inspector) and gate affordances (including a "needs you" state), fed by a
  **second consumer path that never goes through `SessionBackend`**. Gates are not
  pane-bound — some attach to a pane, some float.

For v1 we build the **typed place** (the `ChromeState`/`GateRegistry` fields), not the bus
plumbing that feeds it; the feed is a phase-3 concern. This ensures nothing has to be
retrofitted later.

## 6. Render (`ember-render`)

Render is a pure **consumer**, driven by the event loop in `ember-app`. It never holds a
`SessionBackend` and never borrows across the engine-thread boundary — `!Send` makes that
illegal, so the "render reads engine state under a lock" failure mode is structurally
impossible.

### Ownership and inputs

Render **owns** the neutral grid per pane (the receiving end of the §4 projection) and
applies owned `FrameReady` deltas off the pixel lane. It **consumes** the §5 layout rects
to tile panes, and reads `ChromeState`/`GateRegistry` for the non-pane surface. It owns
the glyph atlas and the GPU pipelines.

### Hot path

- **Damage-driven, never full-frame:** a delta carries line + cell-range damage; render
  re-rasterizes only those cells. The `(glyph, style-id)` cache key means unchanged glyphs
  skip rasterization even within damaged cells.
- **Managed glyph atlas from the first render pass** (a packed texture atlas with
  eviction; raster via swash/cosmic-text). Known watch-item: swash fails on some CBDT
  bitmap-emoji fonts (e.g. `NotoColorEmoji.ttf`) — a fallback path is required.
- **Present mode is the latency lever.** Default `Fifo` is a vsync queue with an
  ~3-frame latency floor; we opt into `Mailbox` (~1-frame, no tear) where available, with
  `Fifo` fallback where the platform/compositor will not honor it. Keypress-to-glyph
  latency is a CI gate, wired from the first render pass.

### Chrome: native default + minimal mode

`ChromeState` is a pure description of *what* to show; the renderer carries two style
variants for *how*: **Native** (pixel-drawn widgets) and **Minimal/TUI** (reuses the cell
renderer, so it is nearly free). A mode toggle is a render-variant switch, not a second UI
codebase. On Linux there is no OS-provided native tab bar — `winit` yields a bare window —
so "native feeling" necessarily means we pixel-draw the chrome to look like a polished
app, as every serious Linux terminal does.

### Chrome implementation: hybrid

- **Hand-rolled `wgpu`** for the identity surfaces where fidelity matters most: the
  terminal cell grid, the **tab bar**, the **split dividers**, and the Minimal mode.
- **`egui`** (via `egui-wgpu`, sharing our `wgpu` device) for rich, transient, interactive
  surfaces: command palette, search bar, preferences / profile editor, context menus,
  modals/popups, and the inspector forms. These are egui's wheelhouse (text input with
  cursor/IME/selection, scroll areas, popups) and are not reimplemented in raw wgpu.
- **Composition:** one wgpu device; render order per frame is cell grid → native chrome →
  egui pass on top (clean z-order, single window).
- **Accepted costs of two stacks:** egui is themed to match the native palette and fonts;
  font/DPI is kept consistent across both; and input routing follows one rule —
  egui-overlay-first when a popup is open, otherwise the terminal.

## 7. Platform seam (`ember-platform`)

The OS-contact layer and the macOS seam. `winit` owns the window and event loop; all UI
inside it is ours.

### `PlatformBackend` trait

Pure-domain code requests effects; the platform impl performs them:

- **Clipboard get/set** — OSC 52 *policy* lives in `ember-core`; the read/write is here.
- **Open path / open URL** — the effect side of trigger dispatch and smart-selection /
  semantic-history actions.
- **Global hotkey** — the Quake/hotkey-window summon (see Wayland note).
- **Notifications, file dialogs** — thin wrappers.

`LinuxBackend` is the v1 implementation. A future `MacBackend` (AppKit — native
`NSWindow` tabs, native hotkey) is explicitly not v1; the seam guarantees it can land
later without touching `ember-core` or `ember-render`.

### IME / compose

International input, dead keys, and compose sequences route through winit's IME, whose
fidelity varies by platform and Wayland compositor. An **early Wayland IME/compose spike**
is a first-phase de-risking task, not a late surprise.

### Wayland global hotkey

Unlike X11, Wayland has no universal global-shortcut API by design. Strategy:

- Use the **freedesktop GlobalShortcuts portal** where the compositor supports it.
- Use the normal **global grab on X11**.
- Where neither works, **degrade to an in-app hotkey only** and document the limitation.

The hotkey-window is a mid-tier feature, so best-effort reach via the portal is preferred
over compositor-specific integrations and their ongoing maintenance cost.

## 8. Feature → subsystem mapping

| Feature | Lands in | Notes |
|---|---|---|
| Split panes | `ember-core` layout tree | §5 |
| Tab rename + drag-reorder | `ember-core` state + native tab bar | reorder = `LayoutCommand::MoveTab` |
| Profiles | `ember-core` data + egui editor | profile is a config struct |
| Regex search | `ember-core` matcher + egui search bar | matcher pure, UI transient |
| Shell integration | `ember-core` (OSC parse → semantic model) + chrome marks | §8.1 |
| Smart selection + semantic history | `ember-core` matcher + `PlatformBackend` open | match pure, open is effect |
| Hotkey / Quake window | `ember-platform` | portal + fallback, §7 |
| Triggers (regex → action) | `ember-core` matcher + `ember-app` dispatch | match in core, dispatch in app |
| `tmux -CC` | `ember-session` (`TmuxControlMode`) | phase 2 |

Every feature maps onto an existing seam; no new crate or structural change is required.

### 8.1 Shell integration protocol

Shell integration is the top-priority feature. v1 uses **OSC 133 as the semantic
backbone** plus an **iTerm2 OSC 1337 subset**:

- **OSC 133** (FinalTerm semantic marks: prompt start, command start, output start,
  command end + exit code) powers jump-to-prompt, command status, and select-output. It
  is the cross-ecosystem de-facto standard (VS Code, WezTerm, Kitty, Ghostty), and most
  shell-integration scripts already emit it.
- **iTerm2 OSC 1337 subset** for iTerm2-specific behavior: `CurrentDir` (a new split
  inherits the cwd), `RemoteHost`, and `SetMark`.

This combination provides both broad ecosystem compatibility and the iTerm2 feel.

## 9. Phasing

**Within v1:**

1. **Foundation** — winit window + wgpu cell render + `LocalPty` + alacritty showing a
   live shell. The Wayland IME/compose and global-hotkey spike runs here.
2. **Multiplexer** — tabs + binary splits + geometric focus navigation.
3. **Native chrome** — pixel tab bar + split dividers + egui scaffolding.
4. **Features** — profiles, regex search, shell integration, triggers, smart selection +
   semantic history, hotkey window.

**Phase 2:** `tmux -CC` (`TmuxControlMode`); evaluate `libghostty-vt` as the VT engine.

**Phase 3:** backend front-end (each agent a real pane) and the chrome/gate bus
plumbing.

Each phase is independently runnable.

## 10. Error handling

- **Session death and backend errors** surface as semantic-lane events, so a crashed shell
  becomes a visible "pane exited (code N)" state — not a hang. The pane persists, shows its
  status, and offers restart.
- **Render failures degrade** rather than crash (drop to `Fifo`, fall back on atlas miss).
- **Config/profile errors** report through the egui layer.
- The pure core performs no IO, so whole classes of failure cannot originate there.

## 11. Testing

- `ember-core` is pure and is exhaustively unit-tested with zero IO: layout math, focus
  navigation, command application, OSC/trigger matching.
- **Dual-OS CI from the first commit** (Linux + macOS), even though the macOS platform impl
  comes later, to keep the `PlatformBackend` seam honest.
- **Keypress-to-glyph latency and flood-throughput benchmarks wired into CI early**, so
  performance regressions are caught structurally ("seen, not felt").
- Golden-frame tests for the render projection.


## 13. Open items and risks

- **Swappable-engine contract:** the projection-fn signature and two-lane `BackendEvent`
  enum must be drafted and sanity-checked against `libghostty-vt` 0.2.0's real per-cell
  representation and damage granularity (§4).
- **Wayland IME/compose + global-hotkey spike** (§7) — first-phase de-risking.
- **swash CBDT bitmap-emoji fallback** (§6) — required before emoji-font coverage is
  claimed complete.
- **Whole-pipeline latency:** a single medium-confidence benchmark showed CPU-rendered
  Foot beating all GPU terminals. The reading is that GPU is not fast by itself — present
  mode, damage tracking, and the glyph atlas are what matter. "We use wgpu" is not a
  latency win on its own.
- **Research coverage gaps** (treat as open): tao/smithay/raw-platform windowing;
  head-to-head raw-GL/Metal/skia/vello vs wgpu latency; wrap-vs-port-vs-skip for
  libvterm/Skia/GNOME-VTE. The Wayland spike addresses the windowing risk; the rest are
  revisited only on real need.
