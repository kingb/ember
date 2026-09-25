#!/usr/bin/env bash
# Standalone X11 real-window smoke of ember-term inside ubuntu:24.04: install
# deps, build, start Xvfb + software Vulkan, then run the SHARED smoke
# assertions (scripts/smoke/window-smoke.sh) — the same ones the CI gate runs,
# so the container prototype and CI can never drift.
#
#   docker run --rm -v "$PWD/out:/out" ubuntu:24.04 \
#     bash -c 'apt-get update && apt-get install -y git &&
#              git clone --depth 1 https://github.com/kingb/ember /src &&
#              bash /src/scripts/smoke/x11-container-smoke.sh'
#
# Or point it at an already-checked-out tree by setting EMBER_SRC=/path.
set -e
export DEBIAN_FRONTEND=noninteractive
SRC="${EMBER_SRC:-/src}"

echo "=== apt deps (build + X11 runtime incl. libxcursor1/libxkbcommon-x11-0 + xvfb) ==="
apt-get update -qq
apt-get install -y -qq \
  curl ca-certificates git build-essential pkg-config \
  libwayland-dev libxkbcommon-dev libx11-dev libxcursor-dev libxi-dev \
  libxrandr-dev libxcb1-dev libvulkan-dev mesa-vulkan-drivers vulkan-tools \
  libfontconfig1-dev fonts-dejavu-core zsh python3 \
  xvfb x11-apps imagemagick libxi6 libxcursor1 libxkbcommon-x11-0 >/dev/null 2>&1
echo "  ok"

echo "=== rust + (clone if needed) + build ==="
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null 2>&1
. "$HOME/.cargo/env"
if [ ! -d "$SRC" ]; then
  git clone -q --depth 1 https://github.com/kingb/ember "$SRC"
fi
cd "$SRC"
echo "  at $(git rev-parse --short HEAD 2>/dev/null || echo '<local>')"
export CARGO_BUILD_JOBS=4
cargo build --release -p ember-app --bin ember-term 2>&1 | tail -1

echo "=== X11 display (Xvfb) + software Vulkan ==="
Xvfb :99 -screen 0 1280x800x24 >/dev/null 2>&1 &
sleep 2
export DISPLAY=:99
export VK_ICD_FILENAMES="$(ls /usr/share/vulkan/icd.d/lvp_icd*.json | head -1)"
export WGPU_BACKEND=vulkan
export WINIT_UNIX_BACKEND=x11

# Delegate to the shared assertions (also run by .github/workflows/ci.yml).
export EMBER_SMOKE_BIN="$SRC/target/release/ember-term"
export EMBER_SMOKE_OUT="${EMBER_SMOKE_OUT:-/out}"
exec bash "$SRC/scripts/smoke/window-smoke.sh"
