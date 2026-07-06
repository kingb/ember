#!/usr/bin/env bash
# Idle CPU cost of the backdrop features: flat vs gradient vs sparks.
#
#   scripts/bench/idle-cpu.sh [path/to/ember-term]
#
# Launches the real app with an isolated config (your ~/.config/ember is
# untouched), lets it settle, then reads the process cputime delta over a
# fixed window. Scenarios alternate across two passes so run-to-run noise
# shows up as inconsistency instead of hiding in an average.
#
# Protocol notes (see scripts/bench/README.md): quiet machine, window must
# stay visible (Ember stops rendering when occluded), CPU only — the sparks
# animation's real cost is GPU/power, measured by gpu-idle.sh.
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="${1:-$REPO/target/release/ember-term}"
[ -x "$BIN" ] || { echo "no binary at $BIN (cargo build --release first?)"; exit 1; }
SETTLE=6
WINDOW=30

cpu_seconds() { # pid -> cumulative cpu seconds
  ps -o cputime= -p "$1" 2>/dev/null | python3 -c "
import sys
t = sys.stdin.read().strip()
if not t: print(-1); sys.exit()
parts = t.replace('.', ':').split(':')  # mm:ss.cc or hh:mm:ss.cc
if len(parts) == 3: m, s, c = parts; print(int(m)*60 + int(s) + int(c)/100)
elif len(parts) == 4: h, m, s, c = parts; print(int(h)*3600 + int(m)*60 + int(s) + int(c)/100)
else: print(-1)"
}

run_scenario() { # name gradient sparks
  local name="$1" gradient="$2" sparks="$3"
  local cfg; cfg="$(mktemp -d)"
  mkdir -p "$cfg/ember"
  printf '[background]\ngradient = %s\nember_sparks = %s\n' "$gradient" "$sparks" \
    > "$cfg/ember/config.toml"
  XDG_CONFIG_HOME="$cfg" "$BIN" >/dev/null 2>&1 &
  local pid=$!
  sleep "$SETTLE"
  local t1 t2
  t1="$(cpu_seconds "$pid")"
  sleep "$WINDOW"
  t2="$(cpu_seconds "$pid")"
  kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
  rm -rf "$cfg"
  if [ "$t1" = "-1" ] || [ "$t2" = "-1" ]; then
    echo "$name: FAILED (process died?)"
  else
    python3 -c "print(f'$name: {(($t2)-($t1))/$WINDOW*100:.2f}% CPU  ({($t2)-($t1):.2f}s cpu over ${WINDOW}s idle)')"
  fi
}

echo "=== ember idle CPU (settle ${SETTLE}s, measure ${WINDOW}s, 2 passes) ==="
for pass in 1 2; do
  echo "--- pass $pass ---"
  run_scenario "flat      (gradient off, sparks off)" false false
  run_scenario "gradient  (gradient on,  sparks off)" true  false
  run_scenario "sparks    (gradient on,  sparks on) " true  true
done
echo "BENCH_DONE"
