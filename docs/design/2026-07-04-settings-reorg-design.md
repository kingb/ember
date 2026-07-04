# Settings reorg — typed + categorized row model

Status: approved (Brandon, 2026-07-04)

## Problem

The Settings overlay (Cmd+, / menu / gear tab button) is a flat positional
list. `settings_rows()` (ember-app/src/main.rs) returns `Vec<(String,
String)>`; `adjust_setting()` matches a hardcoded `settings_sel` index
`0..7`. Reordering or inserting a row silently breaks the match — nothing
catches it at compile time. The overlay is also incomplete: `Config` has
`font.family`, `font.size`, `shell_integration`, and `option_as_meta`,
none of which are surfaced today.

## Data model

New types in `ember-core` (shared by `ember-app`, which owns the
interaction/dispatch, and `ember-render`, which owns the paint):

```rust
pub enum RowKind {
    Toggle,
    Number,
    Cycle,        // steps through a fixed discrete list, wrapping — e.g. font family
    ReadOnly,
    Action,       // no row uses this yet; reserved for  ("Check now")
    SectionHeader,
}

pub enum Help {
    Inline(&'static str),   // simple settings: 1-2 sentence popup
    DocsRef(&'static str),  // complex settings: a slug for 's future in-app docs page
}

pub struct SettingRow {
    pub label: &'static str,
    pub kind: RowKind,
    pub format: fn(&Config) -> String,
    pub adjust: Option<fn(&mut Config, f32)>,  // None for ReadOnly/Action/SectionHeader
    pub help: Help,
}
```

`Cycle` is one addition beyond the 5 named kinds (Toggle / Number /
Action / ReadOnly / SectionHeader). Mechanically it's identical to
`Number` (arrow keys call `adjust`), but semantically distinct — stepping
through a fixed list (font family) vs a continuous clamped value (font
size, scrim) — and worth keeping separate for rendering/help-text tone.

`format`/`adjust` are plain function pointers, not boxed closures: each
row's logic only touches its own `Config` parameter (no captured
environment), so non-capturing closures coerce to `fn` pointers for
free. The row table *is* the dispatch — there is no second
`match settings_sel { 0 => ..., 1 => ... }` anywhere to drift out of
sync with the table. This is the change that actually kills the bug
class an agent flagged.

## Row table

`fn setting_rows() -> &'static [SettingRow]` lives in `ember-core`,
co-located with `SettingRow`/`RowKind`/`Help` and the `Config` type it
reads — it only ever touches `Config`, never `RunState`, so there's no
reason for it to live in `ember-app`. Fully static, no `&self` needed.
Categories are `SectionHeader` rows inline in one flat list, not a
nested structure — this keeps `build_settings()`'s existing flat-list
shape and only enriches each entry.

**Appearance**
- Font family and font size are deferred post-0.1.0 — see
  "Out of scope" below. Not rows in this bead.
- Gradient backdrop (`Toggle`)
- Ember sparks (`Toggle`)
- Ember density (`Number`, step 0.1, clamp 0.0–2.0)
- Ember FPS (`Number`, step 5, clamp 10–120)
- Scrim (`Number`, step 0.05, clamp 0.0–1.0)
- Backdrop image (`ReadOnly`) — config.toml-only, shown as
  `<filename> (<fit>)` or `none`

**Terminal**
- Visual bell (`Toggle`)
- Shell integration (`Toggle`) — help text notes this applies to newly
  spawned sessions/tabs only, not already-running ones (matches how
  `cfg.shell_integration` is actually consumed, copied at session-spawn
  time in `main.rs`)
- Option acts as Meta (`Toggle`) — takes effect immediately (read live
  per-keystroke via `state.config.option_as_meta`, no relayout needed)

**Developer**
- Developer Mode (`Toggle`, `Help::DocsRef("developer-mode")` — the only
  docs-tier row; opens the keystroke-injection + screen-read control
  socket, has real security implications worth more than one sentence)

No Updates category yet —  introduces it (and `RowKind::Action`'s
first real row, "Check now") together with the update-check mechanism.
An empty placeholder category would just be confusing UI.

## Rendering (`ember-render/src/paint.rs`)

`build_settings()` currently takes `rows: &[(String, String)]`. It needs
the row's `RowKind` too, to render `SectionHeader` rows differently: a
dim/bold category label, no value column, not highlighted, not part of
the selectable set. Every other kind renders exactly as today
(`label` …… `value`, highlighted when selected).

## Interaction (`ember-app/src/main.rs`)

- `settings_key()`: ArrowUp/ArrowDown skip `SectionHeader` rows when
  moving `settings_sel` (a header is never a valid selection).
- ArrowLeft/ArrowRight/Space call `row.adjust` when `Some` (Toggle,
  Number, Cycle all go through this uniformly — the numeric `dir`
  argument is ignored by Toggle's closure).
- `Action` gets no key-handling in this bead — nothing uses it yet.
   adds Enter/Space handling when it adds the first Action row.
- Post-adjust side effects stay generic, not row-specific: after *any*
  row's `adjust` runs and the config saves, call the existing
  `apply_appearance()` (backdrop) unconditionally — it already no-ops
  cheaply when nothing backdrop-related changed (matching `zoom_to`'s
  existing no-op-if-unchanged pattern) — so the table-driven design
  never needs to know which row fired to know which side effect to run.

## Testing

- One test per row's `adjust` fn: verify it mutates the intended
  `Config` field and no other.
- Navigation test: ArrowDown/ArrowUp skip over `SectionHeader` rows.
- Extend the existing config-roundtrip test
  (`crates/ember-core/src/config.rs`) to cover the 2 newly-surfaced
  fields (`shell_integration`, `option_as_meta` — already serde fields,
  just confirming the reorg doesn't change persistence).
- Live-app verification: screenshot the categorized overlay (headers +
  all rows visible, correct highlight/skip behavior) via the control
  socket, same method used throughout this session.

## Out of scope (filed separately)

- Font family + font size Settings rows — , explicitly deferred
  post-0.1.0 (Brandon, 2026-07-04). Font size has existing live-apply
  plumbing (`set_font_size`/`zoom_to`, clamped 6–48pt) and would be
  near-free to include, but is deferred alongside font family (which
  needs brand-new `Renderer::set_family` plumbing + a curated cycle
  list) to keep the Appearance category's font UI consistent when it
  lands as one piece, not half now / half later.
- Updates category + `RowKind::Action`'s first real row — .
- First-run wizard —  (depends on this landing first; shares
  this same row table + widgets, not a second UI).
- Per-row help *rendering* (inline popup / docs-page jump) — 
  (depends on this landing first; the `help: Help` field this bead adds
  is exactly its "data on the row" prerequisite).
