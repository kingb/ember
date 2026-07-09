#!/usr/bin/env bash
# Generate the documentation screenshot set from the sanitized demo env.
#
#   shots.sh <ember-binary> <demo-home> <out-dir>
#
# Run demo-env.sh first to build <demo-home>. Every shot is deterministic
# (fixed size/scale/settle and a pinned ember phase) so the same command
# produces the same image on macOS and inside the Linux container — that is
# what makes the cross-platform parity comparison meaningful.
set -euo pipefail

BIN="${1:?usage: shots.sh <ember-binary> <demo-home> <out-dir>}"
HOMEDIR="${2:?}"
OUT="${3:?}"
mkdir -p "$OUT"
cd "$HOMEDIR/project/ember"

# The demo prompt and the cd into the project live in the shell rc files, so
# Ember must spawn a shell that reads them. Ember runs $SHELL, falling back to
# /bin/sh which reads neither. Prefer zsh (its .zshrc), then bash, so the demo
# renders identically on macOS (default zsh) and inside the Linux container.
export SHELL="$(command -v zsh || command -v bash || echo "${SHELL:-/bin/sh}")"
# Trail-segment length matches the 15fps the app ships as the sparks default.
export EMBER_SPARK_DT="${EMBER_SPARK_DT:-0.0667}"

# The campfire (warm gradient + ember-trail sparks) is the default look as of
# 0.3.1, so every shot carries it — the docs should show what a user opens to.
COMMON=(--width 1000 --height 620 --scale 2 --settle 900 --font-size 13 \
  --backdrop --ember --ember-phase 1.6)

shot() {
  local name="$1"; shift
  HOME="$HOMEDIR" "$BIN" --screenshot "$OUT/$name" "$@" "${COMMON[@]}" >/dev/null
  echo "  $name"
}

# One clean pane — getting started.
shot getting-started.png --run "ls"

# Splits, both orientations.
shot splits-vertical.png   --split v --run "git status -sb" --run "cat src/main.rs"
shot splits-horizontal.png --split h --run "ls -la"         --run "git log --oneline -5"

# Tabs (the strip).
shot tabs.png --tabs 4 --run "git log --oneline -6"

# Surface mobility: a tab lifted mid-drag (tear-off / move between windows).
shot tab-tearoff.png --tabs 4 --tab-drag 1 340 --run "git log --oneline -5"

# Hold-to-wisp: the ember ring closing around the cursor before a pane lifts.
shot hold-to-wisp.png --hold-ring 640 330 0.66 --run "git log --oneline -5"

# Keyboard-shortcut cheat sheet.
shot shortcuts.png --help-overlay

# Appearance: campfire backdrop + drifting embers.
shot appearance.png --backdrop --ember --run "git log --oneline -5"

# Settings overlay with the font-family picker highlighted (Menlo on macOS;
# the font row falls back to an available family elsewhere).
shot settings.png --settings --font "Menlo" --run "ls" --backdrop

# Text selection (line mode).
shot selection.png --run "git log --oneline -6" --select "3,0,3,44" --select-mode line

# Clickable URLs: the demo README carries a URL, rendered with the subtle
# always-on underline (v0.2.0+ binaries only).
shot urls.png --run "cat README.md"

echo "done -> $OUT"
