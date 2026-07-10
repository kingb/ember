# Changelog

All notable changes to Ember are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and Ember aims to
follow [Semantic Versioning](https://semver.org).

## [Unreleased]

### Added

- Six selectable wisp styles. The glowing drag token now has a look you
  can pick in Settings: Cinder (the original amber core and orbiting
  sparks, renamed from Ember; the old name still parses), Coal (a small
  charcoal rock rendered by its own procedural shader, with pulsing hot
  cracks, a breathing core, and a gentle shower of embers rising off its
  surface), Will-o'-the-wisp (a soft, cool, breathing orb with a wispy
  vapor tail), Comet (a clean white-hot head with a soft glow), Goo (a
  wobbling molten droplet shedding embers that float up as they cool),
  and Star (a dazzling white core with a blue-white bloom and a
  lens-flare sparkle). A Random option steps to a fresh style on each
  drag.

## [0.4.0] - 2026-07-08

### Added

- Ghost tab. When you drag a tab or pane over a tab strip, yours or another
  window's, the strip shows a wispy, shimmering tab chip in the spot it
  will land, labeled with the surface's title, instead of the old bare
  insertion caret. The other tabs shift over to make room, and the ghost
  lands as the real tab the moment you drop.
- Spring-loaded tabs while dragging. Hover a strip tab for a beat during a
  live drag (about 150 ms, like macOS Finder folders) and it becomes the
  displayed tab, so you can navigate to any tab mid-drag, move down into
  its panes, and drop exactly where you mean to. Skating across the strip
  does not thrash the display, and the ghost's own landing spot never
  switches tabs.
- Suck-in and pour-out. Tearing a surface off (tab drag, pane drag, or
  hold-to-wisp) visibly collapses the whole surface toward your cursor as
  it goes: the full pane for a pane drag, the whole tab content area for a
  tab drag, the entire window when it is the window's only tab. Dropping
  pours the rect back out at the landing spot, Escape pours it back out
  where it started, and a window that gave up its last tab plays its
  collapse before closing. All skipped under Reduce Motion for an instant
  transfer.
- Hold to wisp now works on tabs, not just panes. Press and hold a tab and
  the same ring sweeps it into the wisp to carry, no drag needed.

### Fixed

- Dragging a divider now moves the divider you grabbed. In a window with
  nested splits, most easily made by merging a multi-pane tab into another
  window, grabbing one divider could move an adjacent one instead.
- Ember no longer grows its memory use while the display sleeps. A window
  whose GPU surface goes away now backs off between attempts instead of
  retrying as fast as it can, which could balloon memory over a long sleep.
- The control surface serves several connections at once and drops idle
  ones, so a stuck client can no longer block other `ctl` commands.

## [0.3.1] - 2026-07-08

### Added

- Ember sparks guardrails. The `ember_sparks` on/off switch is now a
  three-way dial, `sparks = "off" | "focused" | "always"` in config.toml
  (default `focused`), cycled from the Settings overlay's "Ember sparks"
  row. `focused` animates sparks only in the window you're actually looking
  at; unfocused windows keep their sparks visible but hold still until you
  switch back. Old configs with `ember_sparks = true`/`false` still load,
  mapped to `focused`/`off`. Two system signals pause the animation
  automatically on macOS: Low Power Mode turns sparks fully off, and Reduce
  Motion freezes them without hiding them, regardless of the dial.

## [0.3.0] - 2026-07-08

### Added

- External tools can now find and jump to tabs. `ctl state` reports every
  tab (index, active, title, sessions), `ctl focus <query>` selects the
  first tab whose title matches and brings the Ember window to the front,
  and `ctl raise` raises the window on its own. All three are also exposed
  as MCP tools. Built for hardware macro decks and agent dashboards that
  map a name to "the tab running that thing".
- Multiple windows. Open new windows (Cmd+N on macOS, Ctrl+Shift+N on
  Linux), and move terminal surfaces freely between them: promote a split
  pane to its own tab or window, move tabs to another window, and merge a
  tab back into another as a split. Shells keep running through every move.
  Available from the Window menu on macOS, keyboard shortcuts on both
  platforms, and the control surface (move-tab, promote-pane, merge-tab,
  new-window).
- The control surface's state now reports every window (windows array plus
  focused_window), and focus <query> finds and raises the right window
  across all of them.
- Drag tabs and panes between windows. Tear a tab off the strip or hold
  Cmd+Opt (macOS) or Ctrl+Alt+Shift (Linux) and drag a pane: edges of a
  target pane split it, the center adds a tab, and empty desktop makes a
  new window right there. While you carry a surface between windows it
  becomes a wisp, a small ember that rides the pointer (turn it off with
  wisp = false). Escape cancels any drag, and shells keep running through
  every drop.
- Hold to wisp. Press and hold on any pane and a small ring draws itself
  closed around your cursor; when it completes, the pane is swept into the
  wisp and you are carrying it, no keyboard involved. Move before the ring
  closes and the press falls back to an ordinary selection. Within a
  window, dragging a surface onto a pane splits it on the nearest side;
  the window a carried surface hovers comes forward so you can see where
  it will land, and a window's only tab now shows its chip so there is
  always something to grab.

### Changed

- The campfire is now fully lit by default: a fresh install opens with the
  warm gradient and drifting ember sparks. The sparks render as short glowing
  trails (velocity-stretched motion blur) instead of plain dots, which keeps
  the drift smooth at the gentle default of 15 frames per second. Measured on
  an Apple Silicon MacBook, the animation costs about 2% of one core and a
  few tens of milliwatts of GPU while the window is visible, and nothing when
  the window is hidden. Tune or disable it in Settings (Ember sparks, Ember
  FPS, Ember density); automatic pauses on unfocused windows, on battery, and
  under Reduce Motion arrive in the next patch release.
- In `ctl state`, the `tabs` field changed from a count to the array above.
  Callers that read it as a number should use the array's length.

### Fixed

- Linux keyboard shortcuts no longer fight the GNOME shell. GNOME reserves
  many Super combinations (Super+1..9, Super+Arrows, Super+D, Super+V), so
  Ember now also binds the conventional Linux forms: Ctrl+Shift+C/V/T/W/D,
  Ctrl+Shift+Arrows to focus panes, Alt+Shift+D and Alt+Shift+Arrows,
  Alt+1..9 to jump to a tab, and Ctrl+- / Ctrl+0 for zoom. Super still works
  where the window manager passes it through, and the shortcuts overlay now
  shows the bindings that work everywhere.
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

[Unreleased]: https://github.com/kingb/ember/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/kingb/ember/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/kingb/ember/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/kingb/ember/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/kingb/ember/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/kingb/ember/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/kingb/ember/releases/tag/v0.1.0
