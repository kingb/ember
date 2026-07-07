# Changelog

All notable changes to Ember are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and Ember aims to
follow [Semantic Versioning](https://semver.org).

## [Unreleased]

### Added

- External tools can now find and jump to tabs. `ctl state` reports every
  tab (index, active, title, sessions), `ctl focus <query>` selects the
  first tab whose title matches and brings the Ember window to the front,
  and `ctl raise` raises the window on its own. All three are also exposed
  as MCP tools. Built for hardware macro decks and agent dashboards that
  map a name to "the tab running that thing".

### Changed

- In `ctl state`, the `tabs` field changed from a count to the array above.
  Callers that read it as a number should use the array's length.

### Fixed

- A plain click (no drag) now clears the selection, like other terminals,
  instead of leaving a single cell selected. Drag selections and
  double/triple-click word and line selections behave as before.

## [0.2.1] - 2026-07-06

### Fixed

- Ember now launches on Ubuntu 22.04. The 0.2.0 Linux builds were made
  against a newer glibc, which made Homebrew interpose its own C library on
  22.04 hosts; the GPU drivers then failed to load and the app never opened
  a window. Linux builds are now made against Ubuntu 22.04's glibc, so one
  build runs cleanly on 22.04, 24.04, and 26.04, and Ubuntu 22.04 is part
  of continuous testing from now on.

## [0.2.0] - 2026-07-06

### Added

- Font settings, live from the Settings panel: pick the font family from a
  curated monospace list and set the size (6 to 48pt). Both apply
  immediately, no restart.
- The Settings panel is organized into Appearance, Terminal, and Developer
  sections, and now also surfaces shell integration and Option-as-Meta.
- Clickable URLs. Web links in terminal output are subtly underlined; click
  one at the prompt to open it in your browser. Inside mouse-driven apps like
  vim or tmux, hold Cmd (macOS) or Ctrl (Linux) and click.

### Changed

- The warm gradient backdrop is now on by default, so a fresh install opens
  with Ember's signature look. It draws statically and costs nothing while
  idle; the ember sparks animation stays opt-in. Turn the gradient off in
  Settings if you prefer a flat background.
- The Settings panel re-shapes its text only when something actually
  changes, not on every frame, keeping the app responsive while it is open.

### Fixed

- On Linux, the keyboard shortcuts overlay now shows Super instead of Cmd,
  matching the keys you actually press.
- The Settings panel no longer misaligns its value column at very large or
  very small terminal font sizes.

## [0.1.0] - 2026-07-04

The first release. Ember is a GPU-accelerated terminal built around a
campfire aesthetic, running natively on macOS and Linux.

Ember's birthday release, launched on America's 250th, July 4th, 2026. Happy
Fourth of July. 🎆

### Added

- **Split panes and tabs.** Split side-by-side or stacked, resize by
  dragging any divider, navigate between panes by direction, drag tabs to
  reorder, rename them inline, and jump straight to a tab by number.
- **Native, GPU-rendered text**, including full text attributes (bold,
  italic, underline, strikeout, overline, dim, concealed) and full
  wide-character/CJK rendering.
- **Hand-rasterized box-drawing.** Every light, heavy, double, dashed,
  rounded, and diagonal box-drawing glyph is drawn as a real vector shape
  rather than a font glyph, so borders and diagrams stay crisp and
  perfectly joined at any font size or display scale. Cross-checked
  against Alacritty and Ghostty's rendering for the full block.
- **Shell integration.** Automatic exit-status markers in the gutter
  (green, red, or amber), jump to the previous or next prompt, and
  cwd-inheriting splits and manual navigable marks when your shell
  supports iTerm2's shell-integration escape codes. Installs itself into
  zsh and bash with no manual setup.
- **Mouse support**: text selection (click, word, and line modes) with
  system clipboard copy/paste, bracketed paste, wrapped-line-aware copy,
  a draggable scrollbar, and full mouse reporting (clicks, drags, motion,
  wheel) forwarded to mouse-aware terminal apps.
- **A campfire backdrop** with drifting ember sparks, off by default and
  fully configurable (density, frame rate, scrim).
- **A visual bell**: an ember flash instead of an audible beep, with a
  bell indicator on background tabs.
- **Settings** (Cmd+,), an **About** page with version and build info, and
  a keyboard shortcuts cheat sheet (Cmd+/).
- A confirmation prompt before closing a pane or window with a running
  process.
- **Developer Mode** (off by default): a control socket and MCP server for
  driving and inspecting a running instance, useful for scripting and
  automated testing.
- Configurable font family and size, with live zoom (Cmd +/-/0).
- A macOS Homebrew cask, app bundle, dock icon, and native menu bar.
- Linux support built and verified on Ubuntu 24.04 and 26.04, x86_64 and
  arm64, on both X11 and Wayland.

### Fixed

A handful of hardening fixes worth knowing about, found and fixed before
this first release:

- A GPU resource leak that could exhaust memory over a long session, and a
  related issue where an occluded or sleeping window would spin the GPU
  allocating frames it never presented.
- A hairline gap that could appear between adjacent box-drawing cells at
  certain font-size and display-scale combinations.
- Glyph-advance jitter that made spinners and other rapidly-updating
  symbols appear to twitch.

### Security

- The developer-mode control socket and shell-integration directory are
  created owner-only, use no fixed or predictable paths, and return
  JSON-encoded errors rather than leaking internal state.

[Unreleased]: https://github.com/kingb/ember/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/kingb/ember/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/kingb/ember/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/kingb/ember/releases/tag/v0.1.0
