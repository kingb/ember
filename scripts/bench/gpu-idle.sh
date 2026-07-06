#!/usr/bin/env bash
# GPU-side idle cost of the backdrop features. Root-only (powermetrics):
#
#   sudo scripts/bench/gpu-idle.sh [path/to/ember-term]
#
# For each scenario (baseline / flat / gradient / sparks), samples GPU power
# and residency via powermetrics while Ember idles with an isolated config.
# 'baseline' runs no Ember at all: the ambient floor to subtract, since these
# numbers are system-wide (WindowServer, browsers, everything).
#
# Protocol: quiet machine, hands off for the ~5 min run, Ember window left
# unoccluded (Ember stops rendering when covered). Two alternating passes.
set -uo pipefail
[ "$(id -u)" = "0" ] || { echo "needs sudo (powermetrics is root-only)"; exit 1; }
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="${1:-$REPO/target/release/ember-term}"
[ -x "$BIN" ] || { echo "no binary at $BIN (cargo build --release first?)"; exit 1; }
REAL_USER="${SUDO_USER:-$(stat -f %Su /dev/console)}"
SETTLE=6
WINDOW=30
OUTDIR="$(mktemp -d)"

# Hold the display awake for the whole run. A locked/sleeping screen occludes
# the window, and Ember intentionally stops rendering when occluded — which
# silently turns the benchmark into a measurement of nothing (learned the
# hard way; see README).
caffeinate -dimsu -w $$ &

screen_locked() { # -> "locked" | "awake" | "unknown"
  case "$(ioreg -n Root -d1 -a 2>/dev/null | grep -A1 IOConsoleLocked | tail -1)" in
    *true*)  echo "locked" ;;
    *false*) echo "awake" ;;
    *)       echo "unknown" ;;
  esac
}

sample() { # name -> writes $OUTDIR/$name.txt
  local lock_before lock_after
  lock_before="$(screen_locked)"
  powermetrics --samplers gpu_power,tasks --show-process-gpu \
    -i 1000 -n "$WINDOW" > "$OUTDIR/$1.txt" 2>/dev/null
  lock_after="$(screen_locked)"
  echo "SCREEN $lock_before -> $lock_after" >> "$OUTDIR/$1.txt"
  if [ "$lock_before" != "awake" ] || [ "$lock_after" != "awake" ]; then
    echo "  !! $1: screen not awake for the full window ($lock_before -> $lock_after) — DISCARD this scenario"
  fi
}

report() { # name
  python3 - "$OUTDIR/$1.txt" "$1" <<'PY'
import re, sys
text = open(sys.argv[1]).read()
name = sys.argv[2]
power = [float(m) for m in re.findall(r'GPU Power:\s*(\d+)\s*mW', text)]
resid = [float(m) for m in re.findall(r'GPU HW active residency:\s*([\d.]+)%', text)]
# per-process GPU ms/s for ember-term, if the tasks sampler exposes it
gpums = []
for line in re.findall(r'ember-term\S*\s+\d+.*', text):
    nums = [c for c in line.split() if re.fullmatch(r'[\d.]+', c)]
    if nums: gpums.append(float(nums[-1]))
avg = lambda v: sum(v)/len(v) if v else float('nan')
print(f"{name:10s} GPU power avg {avg(power):7.1f} mW   "
      f"active residency {avg(resid):5.2f}%   "
      f"ember-term GPU ms/s {avg(gpums):6.2f}" + ("" if gpums else " (n/a)"))
PY
}

run_scenario() { # name gradient sparks   (omit gradient -> no ember launched)
  local name="$1" gradient="${2:-}" sparks="${3:-}"
  local pid=""
  if [ -n "$gradient" ]; then
    local cfg; cfg="$(mktemp -d)"; mkdir -p "$cfg/ember"
    printf '[background]\ngradient = %s\nember_sparks = %s\n' "$gradient" "$sparks" \
      > "$cfg/ember/config.toml"
    chown -R "$REAL_USER" "$cfg"
    # launch as the logged-in user so the window lands in their GUI session
    sudo -u "$REAL_USER" env XDG_CONFIG_HOME="$cfg" "$BIN" >/dev/null 2>&1 &
    pid=$!
    sleep "$SETTLE"
  fi
  sample "$name"
  [ -n "$pid" ] && { kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null; }
  report "$name"
}

echo "=== ember GPU idle cost (settle ${SETTLE}s, sample ${WINDOW}s, 2 passes) ==="
echo "=== keep hands off; do not cover the Ember window ==="
for pass in 1 2; do
  echo "--- pass $pass ---"
  run_scenario "baseline"
  run_scenario "flat"     false false
  run_scenario "gradient" true  false
  run_scenario "sparks"   true  true
done
echo "raw samples kept in: $OUTDIR"
echo "GPU_BENCH_DONE"
