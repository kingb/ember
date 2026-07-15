#!/usr/bin/env bash
# Throughput regression gate: does a release candidate parse a flood of output
# as fast as the last release? Builds two commits head-to-head and compares them
# on the `vt_throughput/<scenario>` criterion bench (the pure VT hot path), and
# optionally (--windowed) on a real windowed `cat` of a big payload.
#
#   scripts/bench/throughput.sh [baseline-ref] [candidate-ref] [flags]
#     baseline-ref   git ref to compare against   (default: v0.4.2, last release)
#     candidate-ref  git ref to test              (default: current working tree)
#   flags:
#     --rounds N      interleaved + balanced rounds       (default 8)
#     --threshold P   FAIL if candidate is >P% slower      (default 5)
#     --scenario S    dense_ascii | sgr_churn | cursor_motion (default dense_ascii)
#     --windowed      ALSO run the real-terminal cat-throughput check (needs a display)
#     --force         run even if the machine isn't quiet enough
#
# WHY THIS EXISTS / PROTOCOL (adds to scripts/bench/README.md):
# The throughput signal is small (single-digit %), so it needs MORE care than
# the idle benchmarks. A day was once lost chasing four different measurement
# artifacts in a row; each is neutralised below:
#   * High Power Mode or AC power   — battery throttles the clock (±10-25% noise).
#   * Quiet load (< 2.5)            — and do NOT build during the run: XProtect
#                                     (xprotectd) scans every new binary and spikes.
#   * Interleaved A/B/A/B           — cancels slow thermal drift.
#   * Balanced order (alternate who — cancels the "second run is hotter" position
#     goes first each round)          bias that a fixed A-then-B order sneaks in.
#   * Absolute medians only         — NEVER criterion's change%/"regressed" verdict;
#                                     with these outliers it mislabels an identical
#                                     binary as "+10.8% regressed".
# The criterion bench measures ONLY parse+drain (no GPU). --windowed measures what
# a user actually feels (parse + coalesced render + PTY); prefer it for the final
# call when a render-path change is in play.
set -uo pipefail

BASELINE="v0.4.2"; CANDIDATE=""; ROUNDS=8; THRESHOLD=5
SCENARIO="dense_ascii"; WINDOWED=0; FORCE=0
pos=()
while [ $# -gt 0 ]; do
  case "$1" in
    --rounds) ROUNDS="$2"; shift 2;;
    --threshold) THRESHOLD="$2"; shift 2;;
    --scenario) SCENARIO="$2"; shift 2;;
    --windowed) WINDOWED=1; shift;;
    --force) FORCE=1; shift;;
    -h|--help) sed -n '2,40p' "$0"; exit 0;;
    -*) echo "unknown flag: $1"; exit 2;;
    *) pos+=("$1"); shift;;
  esac
done
[ "${#pos[@]}" -ge 1 ] && BASELINE="${pos[0]}"
[ "${#pos[@]}" -ge 2 ] && CANDIDATE="${pos[1]}"

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO"

# ---- preconditions -------------------------------------------------------
load=$(uptime | sed -E 's/.*load averages?: ([0-9.]+).*/\1/')
power=$(pmset -g ps 2>/dev/null | head -1 | sed -E "s/.*'(.*)'.*/\1/")
echo "machine:  load=${load}  power=${power:-unknown}"
if ! printf '%s' "$power" | grep -qi 'AC Power'; then
  echo "  ! on battery — turn on High Power Mode (Settings > Battery) or plug in;"
  echo "    battery clock-throttling swamps a single-digit-% signal."
fi
if awk -v l="$load" 'BEGIN{exit !(l+0 > 2.5)}'; then
  echo "  ! load ${load} > 2.5 — close Unity/Chrome, stop dolt, and don't build during the run."
  if [ "$FORCE" != 1 ]; then
    echo "  aborting: numbers under load are noise. Re-run when quiet, or pass --force."
    exit 3
  fi
fi

# ---- build both binaries (restore the tree no matter what) ---------------
ORIG_REF="$(git symbolic-ref --quiet --short HEAD || git rev-parse HEAD)"
STASHED=0
restore() {
  git checkout -q "$ORIG_REF" 2>/dev/null || true
  [ "$STASHED" = 1 ] && git stash pop -q 2>/dev/null || true
}
trap restore EXIT

build_bench() {  # $1 = ref | WORKTREE ; $2 = output path
  local ref="$1" out="$2" bin
  [ "$ref" != WORKTREE ] && { git checkout -q "$ref" || { echo "checkout '$ref' failed"; exit 1; }; }
  touch crates/ember-session/src/projection.rs   # force a rebuild so cargo can't hand back a stale binary
  bin=$(cargo bench -p ember-session --bench throughput --no-run 2>&1 \
        | grep -oE 'target/release/deps/throughput-[a-f0-9]+' | tail -1)
  [ -n "$bin" ] && [ -x "$bin" ] || { echo "bench build failed for '$ref'"; exit 1; }
  cp "$bin" "$out"; chmod +x "$out"   # mktemp'd $out is 0600; cp won't restore the +x bit
}

CAND_BIN="$(mktemp -t bench-cand)"; BASE_BIN="$(mktemp -t bench-base)"
dirty() { ! git diff --quiet || ! git diff --cached --quiet; }

echo "building candidate (${CANDIDATE:-working tree})..."
if [ -z "$CANDIDATE" ]; then
  build_bench WORKTREE "$CAND_BIN"              # build the dirty tree AS-IS first
  if dirty; then git stash push -q; STASHED=1; fi
else
  if dirty; then git stash push -q; STASHED=1; fi
  build_bench "$CANDIDATE" "$CAND_BIN"
fi
echo "building baseline (${BASELINE})..."
build_bench "$BASELINE" "$BASE_BIN"
restore; STASHED=0                              # back on the original tree
trap 'rm -f "$CAND_BIN" "$BASE_BIN"' EXIT       # tree already restored; just drop temp binaries

[ "$(shasum "$CAND_BIN" | cut -c1-12)" = "$(shasum "$BASE_BIN" | cut -c1-12)" ] \
  && echo "  ! candidate and baseline binaries are identical (same ref?)"

# ---- measure: interleaved + balanced, absolute medians -------------------
run() { "$1" --bench "$SCENARIO" --measurement-time 3 --warm-up-time 1 --sample-size 15 2>&1 \
        | grep -oE 'time:.*\]' | grep -oE '[0-9]+\.[0-9]+' | sed -n 2p; }   # criterion's point estimate (middle value)
median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print (NR%2)?a[(NR+1)/2]:(a[NR/2]+a[NR/2+1])/2}'; }

echo
echo "criterion vt_throughput/${SCENARIO}, ${ROUNDS} rounds interleaved+balanced (ms; lower=faster):"
BASEM=(); CANDM=()
for r in $(seq 1 "$ROUNDS"); do
  if [ $((r % 2)) -eq 1 ]; then b=$(run "$BASE_BIN"); c=$(run "$CAND_BIN"); ord="base,cand"
  else                          c=$(run "$CAND_BIN"); b=$(run "$BASE_BIN"); ord="cand,base"; fi
  if [ -z "$b" ] || [ -z "$c" ]; then echo "  round $r: failed to parse a time (unit != ms? try --scenario)"; continue; fi
  printf "  round %-2d (%s)  baseline %-9s candidate %-9s  Δ %+.3f\n" "$r" "$ord" "$b" "$c" "$(awk -v c=$c -v b=$b 'BEGIN{print c-b}')"
  BASEM+=("$b"); CANDM+=("$c")
done
[ "${#BASEM[@]}" -ge 1 ] || { echo "no rounds succeeded"; exit 1; }
bmed=$(median "${BASEM[@]}"); cmed=$(median "${CANDM[@]}")
pct=$(awk -v c="$cmed" -v b="$bmed" 'BEGIN{printf "%.1f",(c-b)/b*100}')
echo
echo "  baseline median ${bmed} ms   candidate median ${cmed} ms   Δ ${pct}%"

# ---- optional: real windowed cat-throughput (needs a display) ------------
if [ "$WINDOWED" = 1 ]; then
  echo; echo "windowed cat-throughput — see scripts/bench/throughput-windowed.sh (heavier, needs a visible window)"
  "$REPO/scripts/bench/throughput-windowed.sh" "$BASELINE" "${CANDIDATE:-}" --rounds "$ROUNDS" ${FORCE:+--force} || true
fi

# ---- verdict -------------------------------------------------------------
echo
awk -v c="$cmed" -v b="$bmed" -v t="$THRESHOLD" 'BEGIN{
  d=(c-b)/b*100;
  if (d > t) { printf "RESULT: FAIL  — candidate is %.1f%% slower than baseline (gate: +%s%%)\n", d, t; exit 1 }
  else       { printf "RESULT: PASS  — %.1f%% vs baseline (gate: +%s%%)\n", d, t; exit 0 }
}'
