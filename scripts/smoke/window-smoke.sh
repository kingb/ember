#!/usr/bin/env bash
# Real-window smoke assertions for a PREBUILT ember-term binary against an
# ALREADY-RUNNING display + software Vulkan. The caller sets up the environment;
# this file is only the assertions, so both the standalone container harness
# (x11-container-smoke.sh) and the CI gate (.github/workflows/ci.yml) share one
# source of truth.
#
#   EMBER_SMOKE_BIN=/path/to/ember-term \
#   DISPLAY=:99 VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.*.json \
#   WGPU_BACKEND=vulkan WINIT_UNIX_BACKEND=x11 \
#   scripts/smoke/window-smoke.sh [out-dir]
#
# Unlike `cargo test`, this exercises the real winit+wgpu window/surface path
# (renderer/paint/headless), so a windowing regression fails the build instead
# of shipping. Every check that matters exits non-zero on failure — this is a
# gate, not a report.
set -uo pipefail

BIN="${EMBER_SMOKE_BIN:-${1:-}}"
OUT="${EMBER_SMOKE_OUT:-${2:-/tmp}}"
[ -n "$BIN" ] && [ -x "$BIN" ] || { echo "window-smoke: no executable binary (EMBER_SMOKE_BIN=$BIN)"; exit 2; }
mkdir -p "$OUT"
: "${DISPLAY:?window-smoke: DISPLAY must be set (start Xvfb first)}"

fail() { echo "  FAIL: $*"; [ -f /tmp/ember-smoke.log ] && tail -20 /tmp/ember-smoke.log | sed 's/^/    /'; exit 1; }
st() { "$BIN" ctl state 2>/dev/null; }
jq_py() { python3 -c "$1"; }

echo "=== SMOKE 1: windowed app launches on the real display ==="
EMBER_CONTROL=1 "$BIN" >/tmp/ember-smoke.log 2>&1 &
APP=$!
sleep 8
kill -0 "$APP" 2>/dev/null && echo "  PASS: app alive after 8s (pid $APP)" || fail "app died on launch"

echo "=== SMOKE 2: ctl reaches it + grid state parses ==="
st > /tmp/state1.json 2>&1 || fail "ctl state failed"
jq_py "import json,sys; s=json.load(open('/tmp/state1.json'))['state']; assert s['surface'][0]>0 and s['surface'][1]>0, 'bad surface'; print('  PASS: ctl state ok, surface', s['surface'])" \
  || fail "ctl state JSON unparseable / degenerate surface"

echo "=== SMOKE 3: typed input round-trips through the shell ==="
"$BIN" ctl type 'echo win-smoke-$((6*7))' >/dev/null 2>&1 || fail "ctl type failed"
"$BIN" ctl key Enter >/dev/null 2>&1 || fail "ctl key failed"
sleep 2
st | grep -q "win-smoke-42" && echo "  PASS: shell echoed win-smoke-42" || fail "typed command did not round-trip"

echo "=== SMOKE 4: visual capture (real window renders) ==="
if command -v xwd >/dev/null && command -v convert >/dev/null; then
  xwd -root -silent | convert xwd:- "$OUT/window-smoke.png" 2>/dev/null \
    && echo "  captured $OUT/window-smoke.png" || echo "  WARN: capture failed (non-fatal)"
fi

echo "=== SMOKE 5: drag/carry on the real window (reorder, cancel, content=selection) ==="
"$BIN" ctl chord cmd+t >/dev/null 2>&1 || fail "ctl chord (new tab) failed"
sleep 1
TABS=$(st | jq_py "import json,sys; print(len(json.load(sys.stdin)['state']['tabs']))") || fail "tab count unreadable"
[ "$TABS" = "2" ] && echo "  PASS: second tab opened (tabs=$TABS)" || fail "expected 2 tabs, got $TABS"
read -r W H <<<"$(st | jq_py "import json,sys; s=json.load(sys.stdin)['state']['surface']; print(s[0], s[1])")"
PANES_BEFORE=$(st | jq_py "import json,sys; print(len(json.load(sys.stdin)['state']['panes']))")
# Tab reorder: drag tab-1 chip toward tab-2 slot; just assert the drag resolves.
R1=$("$BIN" ctl drag 40 12 160 12 --steps 12 --paced 16 2>&1)
echo "$R1" | jq_py "import json,sys; d=json.load(sys.stdin); assert d.get('drag_ended'); print('  reorder drag ->', d['drag_ended'])" \
  || fail "reorder drag did not resolve: $(echo "$R1" | head -c 160)"
# Cancelled drag: must end 'cancel' and change nothing.
R2=$("$BIN" ctl drag $((W/2)) $((H/2)) $((W-30)) $((H/2)) --steps 10 --cancel 2>&1)
echo "$R2" | jq_py "import json,sys; d=json.load(sys.stdin); assert d.get('drag_ended')=='cancel', d.get('drag_ended'); print('  cancelled drag -> cancel')" \
  || fail "cancelled drag did not end 'cancel': $(echo "$R2" | head -c 160)"
# Content drag inside a pane is a TEXT SELECTION (pane-carry needs hold-to-wisp,
# which ctl drag cannot express); pane count must not change.
R3=$("$BIN" ctl drag $((W/2)) $((H/2)) $((W-10)) $((H/2)) --steps 14 --paced 16 2>&1)
ENDED=$(echo "$R3" | jq_py "import json,sys; print(json.load(sys.stdin).get('drag_ended','parse-fail'))" 2>/dev/null || echo parse-fail)
PANES_AFTER=$(st | jq_py "import json,sys; print(len(json.load(sys.stdin)['state']['panes']))")
{ [ "$ENDED" = "selection" ] && [ "$PANES_AFTER" = "$PANES_BEFORE" ]; } \
  && echo "  PASS: content drag is selection, panes stable ($PANES_BEFORE)" \
  || fail "content drag ended '$ENDED', panes $PANES_BEFORE -> $PANES_AFTER"

echo "=== SMOKE 6: stability — idle soak, still alive, log clean of panics ==="
sleep "${EMBER_SMOKE_SOAK:-30}"
kill -0 "$APP" 2>/dev/null || fail "app died during idle soak"
PANICS=$(grep -icE 'panic|thread .* panicked' /tmp/ember-smoke.log || true)
[ "$PANICS" = "0" ] && echo "  PASS: alive after soak, no panics" || fail "$PANICS panic line(s) in app log"

kill "$APP" 2>/dev/null || true
echo "WINDOW_SMOKE_DONE"
