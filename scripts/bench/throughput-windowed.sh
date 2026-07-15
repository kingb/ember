#!/usr/bin/env bash
# Real windowed cat-throughput: what a user actually feels when a program floods
# the terminal — parse + coalesced GPU render + PTY, not the headless projection.
# Builds two commits' release apps, launches both as (isolated-config) windows,
# times `cat`-ing a big payload through each in BALANCED order so thermal drift
# and position bias cancel.
#
#   scripts/bench/throughput-windowed.sh [baseline-ref] [candidate-ref] [flags]
#     --rounds N      balanced rounds        (default 6)
#     --mb N          payload size in MB     (default 35; big enough to be stable,
#                                             small enough to limit thermal buildup)
#     --threshold P   FAIL if candidate is >P% slower per round (median)  (default 8)
#     --force         run even under load
#
# Needs a VISIBLE display (Ember stops rendering when occluded — leave the windows
# on-screen and hands off). Uses an isolated XDG_CONFIG_HOME, so your real
# ~/.config/ember is never touched. Read scripts/bench/README.md first.
set -uo pipefail

BASELINE="v0.4.2"; CANDIDATE=""; ROUNDS=6; MB=35; THRESHOLD=8; FORCE=0
pos=()
while [ $# -gt 0 ]; do
  case "$1" in
    --rounds) ROUNDS="$2"; shift 2;; --mb) MB="$2"; shift 2;;
    --threshold) THRESHOLD="$2"; shift 2;; --force) FORCE=1; shift;;
    -h|--help) sed -n '2,20p' "$0"; exit 0;;
    -*) echo "unknown flag: $1"; exit 2;; *) pos+=("$1"); shift;;
  esac
done
[ "${#pos[@]}" -ge 1 ] && BASELINE="${pos[0]}"
[ "${#pos[@]}" -ge 2 ] && [ -n "${pos[1]}" ] && CANDIDATE="${pos[1]}"

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"; cd "$REPO"

load=$(uptime | sed -E 's/.*load averages?: ([0-9.]+).*/\1/')
awk -v l="$load" 'BEGIN{exit !(l+0 > 2.5)}' && [ "$FORCE" != 1 ] && {
  echo "load $load > 2.5 — quiet the machine or pass --force"; exit 3; }

# ---- build both release apps (restore tree on exit) ----------------------
ORIG_REF="$(git symbolic-ref --quiet --short HEAD || git rev-parse HEAD)"; STASHED=0
restore() { git checkout -q "$ORIG_REF" 2>/dev/null || true; [ "$STASHED" = 1 ] && git stash pop -q 2>/dev/null || true; }
trap restore EXIT
dirty() { ! git diff --quiet || ! git diff --cached --quiet; }
build_app() {  # $1 ref|WORKTREE  $2 out
  [ "$1" != WORKTREE ] && { git checkout -q "$1" || { echo "checkout '$1' failed"; exit 1; }; }
  cargo build --release -p ember-app >/dev/null 2>&1 || { echo "app build failed for '$1'"; exit 1; }
  cp target/release/ember-term "$2"; chmod +x "$2"   # mktemp'd $2 is 0600; cp won't restore +x
}
CAND_APP="$(mktemp -t ember-cand)"; BASE_APP="$(mktemp -t ember-base)"
echo "building candidate app (${CANDIDATE:-working tree})..."
if [ -z "$CANDIDATE" ]; then build_app WORKTREE "$CAND_APP"; dirty && { git stash push -q; STASHED=1; };
else dirty && { git stash push -q; STASHED=1; }; build_app "$CANDIDATE" "$CAND_APP"; fi
echo "building baseline app (${BASELINE})..."; build_app "$BASELINE" "$BASE_APP"
restore; trap - EXIT; STASHED=0

# ---- payload + in-shell timer -------------------------------------------
PAY="$(mktemp -t bigfile)"
seq 1 $((MB * 8620)) | awk '{printf "line %d xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx %d\n",$1,$1}' > "$PAY"
TIMER="$(mktemp -t realbench).sh"
cat > "$TIMER" <<EOF
zmodload zsh/datetime; s=\$EPOCHREALTIME; cat "$PAY"; e=\$EPOCHREALTIME
printf '\nREALBENCH_ELAPSED=%.3f\n' \$((e - s))
EOF

# ---- launch both windows (isolated config, control socket on) ------------
CFG="$(mktemp -d)"
XDG_CONFIG_HOME="$CFG" EMBER_CONTROL=1 "$BASE_APP" >/dev/null 2>&1 & BASE_PID=$!
XDG_CONFIG_HOME="$CFG" EMBER_CONTROL=1 "$CAND_APP" >/dev/null 2>&1 & CAND_PID=$!
cleanup_apps() { kill "$BASE_PID" "$CAND_PID" 2>/dev/null || true; }
trap cleanup_apps EXIT
sleep 3
CTL() { local p=$1; shift; "$CAND_APP" ctl --pid "$p" "$@" >/dev/null 2>&1; }
CTL "$BASE_PID" rename-tab 0 "BASELINE $BASELINE"
CTL "$CAND_PID" rename-tab 0 "CANDIDATE ${CANDIDATE:-worktree}"
read_elapsed() { "$CAND_APP" ctl --pid "$1" state 2>/dev/null | python3 -c "import sys,json;t=json.load(sys.stdin)['state']['panes'][0]['text'];m=[l for l in t.splitlines() if 'REALBENCH_ELAPSED=' in l];print(m[-1].split('=')[1].strip() if m else '')" 2>/dev/null; }
runb() { local p=$1; CTL $p type 'clear'; CTL $p key Enter; sleep 1; CTL $p type "zsh $TIMER"; CTL $p key Enter
  for _ in $(seq 1 90); do sleep 1; v=$(read_elapsed $p); [ -n "$v" ] && { echo "$v"; return; }; done; echo TIMEOUT; }

# ---- measure balanced, gap = candidate - baseline ------------------------
echo; echo "windowed cat of ${MB}MB, ${ROUNDS} rounds balanced (seconds; lower=faster). Leave the windows visible."
GAPS=()
for r in $(seq 1 "$ROUNDS"); do
  if [ $((r % 2)) -eq 1 ]; then b=$(runb "$BASE_PID"); c=$(runb "$CAND_PID"); ord="base,cand"
  else                          c=$(runb "$CAND_PID"); b=$(runb "$BASE_PID"); ord="cand,base"; fi
  case "$b$c" in *TIMEOUT*) echo "  round $r: TIMEOUT (payload too big? window occluded?)"; continue;; esac
  g=$(awk -v c=$c -v b=$b 'BEGIN{printf "%.2f",c-b}')
  printf "  round %-2d (%s)  baseline %ss  candidate %ss  gap %ss\n" "$r" "$ord" "$b" "$c" "$g"
  GAPS+=("$g")
done
[ "${#GAPS[@]}" -ge 1 ] || { echo "no rounds succeeded"; exit 1; }
median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print (NR%2)?a[(NR+1)/2]:(a[NR/2]+a[NR/2+1])/2}'; }
gmed=$(median "${GAPS[@]}")
echo; echo "  median gap (candidate - baseline): ${gmed}s"
