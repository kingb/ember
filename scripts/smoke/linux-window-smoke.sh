#!/usr/bin/env bash
# Real-window smoke test for Linux, X11 or Wayland.
#
#   linux-window-smoke.sh <x11|wayland> [path-to-ember-term] [out-dir]
#
# `cargo test` never opens a window, so it has never caught a Linux windowing
# regression: a missing runtime .so, a broken winit backend, a wgpu surface
# that fails to configure. Those only appear once a real window exists. This
# script opens one against a headless display server (Xvfb for X11, weston's
# headless backend for Wayland), then drives and introspects it through the
# control socket. It is the gate behind CI's `linux-smoke` job; run it the
# same way locally, or inside a container via `container-smoke.sh`.
#
# Assumes the display server, the binary, and the runtime libraries are all
# already installed — it deliberately does no apt work, so CI and containers
# can each install deps their own way.

set -uo pipefail

BACKEND="${1:-x11}"
BIN="${2:-target/release/ember-term}"
OUT="${3:-${TMPDIR:-/tmp}/ember-smoke}"

case "$BACKEND" in
x11 | wayland) ;;
*)
    echo "usage: $0 <x11|wayland> [binary] [out-dir]" >&2
    exit 2
    ;;
esac
if [ ! -x "$BIN" ]; then
    echo "no such binary: $BIN" >&2
    exit 2
fi
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
mkdir -p "$OUT"

FAILURES=0
APP_LOG="$OUT/app-$BACKEND.log"
SRV_LOG="$OUT/display-$BACKEND.log"
# The app chmods its socket's parent dir to 0700 and refuses to bind if the dir
# isn't ours — so the socket gets its own private dir, never the artifact dir
# (which is a bind mount from the host under container-smoke.sh, where a chmod
# can fail outright).
SOCKDIR="${TMPDIR:-/tmp}/ember-smoke-sock-$$"
mkdir -p "$SOCKDIR"
SOCK="$SOCKDIR/$BACKEND.sock"

note() { echo "  $*"; }
pass() { echo "  PASS: $*"; }
fail() {
    echo "  FAIL: $*"
    FAILURES=$((FAILURES + 1))
}
step() { echo "=== $* ==="; }

APP_PID=""
SRV_PID=""
cleanup() {
    [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null
    [ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null
    rm -rf "$SOCKDIR"
    return 0
}
trap cleanup EXIT

ctl() { "$BIN" ctl --sock "$SOCK" "$@" 2>&1; }

# --- display server ---------------------------------------------------------

step "starting a headless $BACKEND display"
# Software Vulkan (lavapipe) — CI runners and containers have no GPU. Set
# before the app starts; wgpu picks its adapter at surface-creation time.
LVP="$(ls /usr/share/vulkan/icd.d/lvp_icd*.json 2>/dev/null | head -1)"
if [ -n "$LVP" ]; then
    export VK_ICD_FILENAMES="$LVP"
    note "vulkan icd: $LVP"
else
    note "no lavapipe icd found — relying on the system default adapter"
fi
export WGPU_BACKEND=vulkan

if [ "$BACKEND" = x11 ]; then
    export DISPLAY=:99
    unset WAYLAND_DISPLAY
    export WINIT_UNIX_BACKEND=x11
    Xvfb :99 -screen 0 1280x800x24 >"$SRV_LOG" 2>&1 &
    SRV_PID=$!
    for _ in $(seq 1 30); do
        xdpyinfo -display :99 >/dev/null 2>&1 && break
        sleep 1
    done
    if ! xdpyinfo -display :99 >/dev/null 2>&1; then
        echo "  FAIL: Xvfb never came up on :99" >&2
        tail -20 "$SRV_LOG" >&2
        exit 1
    fi
    note "Xvfb up on :99 (1280x800)"
else
    # Our own runtime dir, not the ambient one: weston wants 0700 and we would
    # rather not chmod a dir the rest of the session depends on (and under
    # container-smoke.sh the artifact dir is a bind mount where chmod can fail).
    export XDG_RUNTIME_DIR="$SOCKDIR/xdg"
    mkdir -p "$XDG_RUNTIME_DIR"
    chmod 700 "$XDG_RUNTIME_DIR"
    export WAYLAND_DISPLAY=wayland-smoke
    unset DISPLAY
    export WINIT_UNIX_BACKEND=wayland
    # weston renamed the headless backend between 13 and 14 (`headless-backend.so`
    # -> `headless`), and Ubuntu 24.04 and 26.04 straddle that change. Try the
    # modern spelling first, fall back to the old one.
    for b in headless headless-backend.so; do
        weston --backend="$b" --width=1280 --height=800 \
            --socket="$WAYLAND_DISPLAY" --no-config --idle-time=0 \
            >"$SRV_LOG" 2>&1 &
        SRV_PID=$!
        for _ in $(seq 1 20); do
            [ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ] && break
            sleep 1
        done
        if [ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ]; then
            note "weston up (--backend=$b), socket $XDG_RUNTIME_DIR/$WAYLAND_DISPLAY"
            break
        fi
        kill "$SRV_PID" 2>/dev/null
        SRV_PID=""
    done
    if [ -z "$SRV_PID" ]; then
        echo "  FAIL: weston never exposed a wayland socket" >&2
        tail -20 "$SRV_LOG" >&2
        exit 1
    fi
fi

# --- 1: the real windowed app launches --------------------------------------

step "SMOKE 1: windowed app launches on $BACKEND"
rm -f "$SOCK"
EMBER_CONTROL="$SOCK" "$BIN" >"$APP_LOG" 2>&1 &
APP_PID=$!
# Poll rather than sleep: a cold llvmpipe pipeline can take several seconds to
# reach first frame, and a fixed sleep either flakes or wastes CI minutes.
READY=0
for _ in $(seq 1 60); do
    if ! kill -0 "$APP_PID" 2>/dev/null; then break; fi
    if [ -S "$SOCK" ] && ctl state | grep -q '"ok":true'; then
        READY=1
        break
    fi
    sleep 1
done
if [ "$READY" = 1 ]; then
    pass "app alive and answering on the control socket (pid $APP_PID)"
else
    fail "app never became ready — this is the missing-runtime-lib / winit / wgpu gate"
    tail -30 "$APP_LOG"
    exit 1
fi

# --- 2: a real surface with a real grid -------------------------------------

step "SMOKE 2: surface and grid dimensions are sane"
STATE="$(ctl state)"
echo "$STATE" >"$OUT/state-$BACKEND.json"
if echo "$STATE" | grep -qE '"surface":\[[1-9][0-9]*,[1-9][0-9]*\]'; then
    pass "surface is non-zero: $(echo "$STATE" | grep -oE '"surface":\[[0-9]+,[0-9]+\]')"
else
    fail "surface is missing or zero-sized (window/surface creation problem)"
fi
if echo "$STATE" | grep -qE '"dims":\[[1-9][0-9]*,[1-9][0-9]*\]'; then
    pass "pane grid is non-empty: $(echo "$STATE" | grep -oE '"dims":\[[0-9]+,[0-9]+\]' | head -1)"
else
    fail "pane reports no grid dimensions (layout never sized the PTY)"
fi

# --- 3: typed input round-trips through the real shell ----------------------

step "SMOKE 3: typed input round-trips through the shell"
MARKER="smoke-$BACKEND-$((6 * 7))"
ctl type "echo $MARKER" >/dev/null
ctl key Enter >/dev/null
FOUND=0
for _ in $(seq 1 20); do
    if ctl state | grep -q "$MARKER"; then
        FOUND=1
        break
    fi
    sleep 1
done
if [ "$FOUND" = 1 ]; then
    pass "shell echoed $MARKER (keyboard -> PTY -> grid -> projection)"
else
    fail "typed output never reached the grid"
fi

# --- 4: the live window actually renders pixels -----------------------------

step "SMOKE 4: live window renders to a PNG"
SHOT="$OUT/smoke-$BACKEND.png"
rm -f "$SHOT"
SHOT_RESP="$(ctl screenshot "$SHOT")"
SHOT_BYTES=$(wc -c <"$SHOT" 2>/dev/null || echo 0)
# The capture comes off the live window's own surface, not an offscreen
# re-render, so a byte-bearing PNG here means the on-screen path drew.
if echo "$SHOT_RESP" | grep -q '"ok":true' && [ "$SHOT_BYTES" -gt 1024 ]; then
    pass "captured $SHOT ($SHOT_BYTES bytes)"
else
    fail "live screenshot failed: $SHOT_RESP (${SHOT_BYTES} bytes)"
fi

# --- 5: short idle soak, clean log ------------------------------------------

step "SMOKE 5: idle soak, then a clean log"
sleep 10
if kill -0 "$APP_PID" 2>/dev/null; then
    pass "alive after idle"
else
    fail "app died while idle"
fi
if grep -qiE "panicked at|thread '.*' panicked" "$APP_LOG"; then
    fail "panic in the app log:"
    grep -iE "panicked at|thread '.*' panicked" "$APP_LOG" | head -5 | sed 's/^/    /'
else
    pass "no panics in the app log"
fi
note "last log lines:"
tail -5 "$APP_LOG" | sed 's/^/    /'

# --- verdict ----------------------------------------------------------------

echo
if [ "$FAILURES" = 0 ]; then
    echo "SMOKE_${BACKEND}_PASS"
    exit 0
fi
echo "SMOKE_${BACKEND}_FAIL ($FAILURES failing check(s))"
exit 1
