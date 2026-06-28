# Ember

A native Linux terminal emulator, built from scratch in Rust, with **iTerm2 as the
experiential spec** — a daily-driver replacement, not a port of the macOS source and
not an extension of an existing terminal.

- **Crate / binary:** `ember-term`
- **Status:** Design approved; implementation planning underway.
- **Design doc:** [`docs/design/2026-06-27-ember-design.md`](docs/design/2026-06-27-ember-design.md)

## Architecture at a glance

Layered, single-process Rust workspace. The daemon/multi-process split is deferred, but
its boundary is front-loaded.

| Crate | Responsibility |
|---|---|
| `ember-core` | Pure domain: `SessionBackend` trait, layout tree, focus/layout, profiles, OSC/trigger matching. No IO. |
| `ember-session` | Backend impls: `LocalPty` (v1), `TmuxControlMode` (phase 2), `a future out-of-process backend` (future). |
| `ember-render` | wgpu + glyphon + custom GPU chrome + egui overlay. |
| `ember-platform` | winit + `PlatformBackend` (clipboard, open, hotkey). macOS seam. |
| `ember-app` | Binary: event loop, input routing, layout, config; trigger dispatch. |

Two extension seams: **`SessionBackend`** (tmux / daemon / bus) and **`PlatformBackend`**
(macOS). See the design doc for the full picture, including the projection-based render
seam, the two-lane event sink, and the v1 → phase-2 → phase-3 roadmap.

## Stack

`winit` · `wgpu` · `glyphon`/`cosmic-text` · `alacritty_terminal` (swappable) ·
`portable-pty` · `egui`.
