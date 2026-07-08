#!/usr/bin/env bash
# X11 smoke test of current main (multi-window + drag/wisp era) inside
# ubuntu:24.04: build, start Xvfb, run the REAL WINDOWED app on X11 with
# software Vulkan, then drive + introspect it via the ctl socket.
set -e
export DEBIAN_FRONTEND=noninteractive
echo "=== apt deps (build + X11 runtime incl. libxi6 + xvfb) ==="
apt-get update -qq
apt-get install -y -qq \
  curl ca-certificates git build-essential pkg-config \
  libwayland-dev libxkbcommon-dev libx11-dev libxcursor-dev libxi-dev \
  libxrandr-dev libxcb1-dev libvulkan-dev mesa-vulkan-drivers vulkan-tools \
  libfontconfig1-dev fonts-dejavu-core zsh \
  xvfb x11-apps imagemagick libxi6 libxcursor1 libxkbcommon-x11-0 >/dev/null 2>&1
echo "  ok"
echo "=== rust + clone + build main ==="
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null 2>&1
. "$HOME/.cargo/env"
git clone -q --depth 1 https://github.com/kingb/ember /src
cd /src
echo "  at $(git rev-parse --short HEAD)"
export CARGO_BUILD_JOBS=4
cargo build --release -p ember-app --bin ember-term 2>&1 | tail -1
BIN=/src/target/release/ember-term

echo "=== X11 display (Xvfb) + software Vulkan ==="
Xvfb :99 -screen 0 1280x800x24 >/dev/null 2>&1 &
sleep 2
export DISPLAY=:99
export VK_ICD_FILENAMES="$(ls /usr/share/vulkan/icd.d/lvp_icd*.json | head -1)"
export WGPU_BACKEND=vulkan
export WINIT_UNIX_BACKEND=x11

echo "=== SMOKE 1: windowed app launches on X11 ==="
EMBER_CONTROL=1 "$BIN" >/tmp/app.log 2>&1 &
APP=$!
sleep 8
kill -0 $APP 2>/dev/null && echo "  PASS: app alive after 8s (pid $APP)" || { echo "  FAIL: app died"; cat /tmp/app.log | tail -20; exit 1; }

echo "=== SMOKE 2: ctl reaches it + grid state sane ==="
"$BIN" ctl state > /tmp/state1.json 2>&1 && echo "  PASS: ctl state ok" || { echo "  FAIL: ctl"; cat /tmp/state1.json; }
head -c 400 /tmp/state1.json; echo

echo "=== SMOKE 3: typed input round-trips through the X11 shell ==="
"$BIN" ctl type 'echo x11-smoke-$((6*7))' >/dev/null
"$BIN" ctl key Enter >/dev/null
sleep 2
if "$BIN" ctl state | grep -q "x11-smoke-42"; then echo "  PASS: shell echoed x11-smoke-42"; else echo "  FAIL: output not found"; fi

echo "=== SMOKE 4: visual evidence (X11 root capture) ==="
xwd -root -silent | convert xwd:- /out/x11-smoke.png 2>/dev/null && echo "  captured /out/x11-smoke.png"

echo "=== SMOKE 4b: drag/carry on X11 (tab reorder, pane split-by-drag, cancel) ==="
# Second tab so there's something to reorder.
"$BIN" ctl chord cmd+t >/dev/null; sleep 1
TABS_BEFORE=$("$BIN" ctl state | python3 -c "import json,sys; print(len(json.load(sys.stdin)['state']['tabs']))")
echo "  tabs open: $TABS_BEFORE (want 2)"
# Surface dims drive the coordinates (strip along the top).
read -r W H <<<"$("$BIN" ctl state | python3 -c "import json,sys; s=json.load(sys.stdin)['state']['surface']; print(s[0], s[1])")"
# Reorder: drag tab 1's chip toward tab 2's slot.
R1=$("$BIN" ctl drag 40 12 160 12 --steps 12 --paced 16 2>&1)
echo "  reorder drag -> $(echo "$R1" | python3 -c "import json,sys; print(json.load(sys.stdin).get('drag_ended','parse-fail'))" 2>/dev/null || echo "$R1" | head -c 120)"
# Cancelled drag: must end 'cancel' and change nothing.
PANES_BEFORE=$("$BIN" ctl state | python3 -c "import json,sys; print(len(json.load(sys.stdin)['state']['panes']))")
R2=$("$BIN" ctl drag $((W/2)) $((H/2)) $((W-30)) $((H/2)) --steps 10 --cancel 2>&1)
echo "  cancelled drag -> $(echo "$R2" | python3 -c "import json,sys; print(json.load(sys.stdin).get('drag_ended','parse-fail'))" 2>/dev/null || echo "$R2" | head -c 120)"
# Content drag across the pane: must be TEXT SELECTION (a plain click-drag
# inside pane content selects; pane-carry requires the hold-to-wisp gesture,
# which ctl drag cannot express yet — needs a --hold-ms arg, flagged upstream).
R3=$("$BIN" ctl drag $((W/2)) $((H/2)) $((W-10)) $((H/2)) --steps 14 --paced 16 2>&1)
ENDED=$(echo "$R3" | python3 -c "import json,sys; print(json.load(sys.stdin).get('drag_ended','parse-fail'))" 2>/dev/null || echo parse-fail)
PANES_AFTER=$("$BIN" ctl state | python3 -c "import json,sys; print(len(json.load(sys.stdin)['state']['panes']))")
if [ "$ENDED" = "selection" ] && [ "$PANES_AFTER" = "$PANES_BEFORE" ]; then
  echo "  PASS: content drag is selection, pane count stable ($ENDED)"
else
  echo "  FAIL: content drag ended '$ENDED', panes $PANES_BEFORE -> $PANES_AFTER"
fi
xwd -root -silent | convert xwd:- /out/x11-drag.png 2>/dev/null && echo "  captured /out/x11-drag.png"

echo "=== SMOKE 5: stability — 30s idle, still alive, log clean ==="
sleep 30
kill -0 $APP 2>/dev/null && echo "  PASS: alive after idle" || echo "  FAIL: died during idle"
grep -icE "panic|error" /tmp/app.log | xargs echo "  panic/error lines in app log:"
tail -5 /tmp/app.log | sed 's/^/  log: /'
kill $APP 2>/dev/null || true
echo "X11_SMOKE_DONE"
