#!/usr/bin/env bash
# Run the Linux real-window smoke tests against THIS working tree, inside an
# ubuntu:24.04 container. Same script CI runs, same assertions — this is how
# you reproduce a CI `linux-smoke` failure (or check a change before pushing)
# from a Mac.
#
#   scripts/smoke/container-smoke.sh [x11|wayland|both]   (default: both)
#
# The repo is mounted read-only; the build lands in a named docker volume so
# repeat runs are incremental and the host's target/ is never touched.
set -euo pipefail

WHICH="${1:-both}"
case "$WHICH" in x11 | wayland | both) ;; *)
    echo "usage: $0 [x11|wayland|both]" >&2
    exit 2
    ;;
esac
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT="$REPO/target/smoke-container"
mkdir -p "$OUT"

BACKENDS="x11 wayland"
[ "$WHICH" != both ] && BACKENDS="$WHICH"

echo "repo:     $REPO (mounted read-only at /src)"
echo "backends: $BACKENDS"
echo "output:   $OUT"

docker run --rm -t \
    -v "$REPO:/src:ro" \
    -v ember-smoke-target:/build \
    -v "$OUT:/out" \
    -e BACKENDS="$BACKENDS" \
    -e DEBIAN_FRONTEND=noninteractive \
    -e CARGO_TARGET_DIR=/build \
    ubuntu:24.04 bash -euo pipefail -c '
echo "=== apt: build deps + X11/Wayland runtime + headless display servers ==="
apt-get update -qq
apt-get install -y -qq --no-install-recommends \
  curl ca-certificates build-essential pkg-config \
  libwayland-dev libxkbcommon-dev libx11-dev libxcursor-dev libxi-dev \
  libxrandr-dev libxcb1-dev libvulkan-dev libfontconfig1-dev \
  mesa-vulkan-drivers fonts-dejavu-core \
  libxi6 libxcursor1 libxkbcommon-x11-0 libwayland-client0 \
  xvfb x11-utils weston >/dev/null
echo "  ok"

echo "=== rust + build ember-term from the mounted tree ==="
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null 2>&1
. "$HOME/.cargo/env"
# Copy out of the read-only mount: cargo wants a writable tree (lockfile,
# .cargo lock files) even when the target dir lives elsewhere.
mkdir -p /work
tar -C /src --exclude=./target --exclude=./.git -cf - . | tar -C /work -xf -
cd /work
cargo build --release -p ember-app --bin ember-term 2>&1 | tail -2
BIN=/build/release/ember-term
ls -l "$BIN"

rc=0
for b in $BACKENDS; do
  echo
  echo "############ $b ############"
  /work/scripts/smoke/linux-window-smoke.sh "$b" "$BIN" /out || rc=1
done
exit $rc
'
