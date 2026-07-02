# Layout seam: derived geometry, pane-relative resize, min-size as data

Ruling: the design review, mail `` (), on the
three  contract items. Implemented alongside this note.

## The through-line

Same doctrine as the VT projection and the  wide-char ruling: **ember-core
stays ignorant of rendering units.** Everything crossing the seam is either
*derived-and-reconciled* or *self-describing data* — never a signal the app
answers by re-deriving anyway.

## (1) Resize is derived state — reconcile it, don't signal it

Pane geometry is a pure projection of `(tree, viewport)` — the same shape as the
VT grid projection. Derived state is **reconciled**, not pushed over an event
channel.

- **Deleted `LayoutEffect::ResizeBackend`.** Effects now carry only what
  derivation cannot recover: `KillSession` and `FocusChanged`.
- `layout()` remains the single geometry source. The app already walks it every
  frame in `sync_layout` to paint; that walk *is* the truth. It computes
  `dims_for_rect(inner_rect, cell_w, cell_h)` per pane and sends
  `BackendControl::Resize` only when the result differs from the cached dims.
  Idempotent, self-healing, and uniform across **every** cause — splits, tab
  switches, font-size changes, DPI changes — none of which core should know.
- The old effect channel was fiction: the app never consumed `ResizeBackend`
  (it matched only `KillSession`) and re-derived in `sync_layout` regardless.
  We deleted the fiction, not the reconcile.
- B1-safe: `BackendControl::Resize(GridDims)` is untouched; we removed an
  ember-tier `LayoutEffect` variant (pre-1.0 of that vocabulary). Verified no
  other consumer of `ResizeBackend` existed before deleting.

Rejected alternatives: (a) make `ResizeBackend` carry `GridDims` — forces cell
metrics + padding into a core that rightly lacks them; (b) keep it as an
abstract "pane geometry changed" signal — formalizes the fiction (the app
answers it by re-deriving anyway).

## (2) Pane-relative resize — no stored divider identity

The user thinks in panes, not dividers. The core op is
`resize_pane(target: PaneId, axis, delta)`: walk **up** from the leaf to the
nearest enclosing split of that axis and adjust its ratio. `delta > 0` grows the
target's side (sign is resolved by whether the leaf lives in the split's `a` or
`b` subtree).

- Covers keyboard resize with **zero** new identity that could dangle across
  tree edits.
- For mouse drag, hit-testing yields the split node; a path-of-child-indices is
  fine as a **transient** address (valid until the next tree mutation, never
  persisted, never serialized). Wanting to *store* one is the smell — so we
  don't.
- Replaces the old `ResizeSplit { target, ratio }`, which could only address the
  split whose *immediate* leaf child was the target — unreachable from a pane
  nested two levels down.

## (3) Min-size is a parameter, not a predicate

The meaningful unit is cells, but core is metrics-free — so the **app** computes
`min_px = min_cells * cell + pad` and passes it into the split/resize ops as
data. Core enforces by clamping ratios so no pane's rect violates its minimum;
a split that cannot satisfy both children **refuses** the op (returns no
effects, no mutation) rather than silently overriding.

A callback predicate would be over-general and drag app closures into core; a
value crosses the boundary cleanly. `min_px` is the minimum extent along the
split axis, so the app passes the axis-appropriate value (min width for a
horizontal split, min height for a vertical one).

This also fixes the compounding-nesting bug: `k` nested same-axis splits used to
give the innermost pane `0.05^k` of the axis (sub-pixel at 3 levels). Splits now
refuse when the container can't hold two minimum-size panes.
