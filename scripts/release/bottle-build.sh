#!/usr/bin/env bash
# Build one Linux bottle for Ember inside ubuntu:22.04 (run with --platform
# pinned by the caller). Recipe per the v0.2.1 lessons:
#   - SYSTEM toolchain only: rustup with the system linker (build-essential),
#     never brew's (brew rust links brew glibc and the GPU drivers won't load)
#   - enforce the glibc ceiling: no versioned symbol above GLIBC_2.35
#   - bottle layout = cargo install --root (matches the 0.2.1 bottles)
#   - windowed smoke under Xvfb inside the container before packing
# Args: $1 = version (e.g. 0.3.1), $2 = bottle arch tag (arm64_linux|x86_64_linux)
set -euo pipefail
V="${1:?version}"
ARCH_TAG="${2:?arch tag}"
export DEBIAN_FRONTEND=noninteractive

echo "=== deps (build + windowed runtime + xvfb) ==="
apt-get update -qq
apt-get install -y -qq \
  curl ca-certificates git build-essential pkg-config \
  libwayland-dev libxkbcommon-dev libx11-dev libxcursor-dev libxi-dev \
  libxrandr-dev libxcb1-dev libvulkan-dev mesa-vulkan-drivers \
  libfontconfig1-dev fonts-dejavu-core binutils \
  libxi6 libxrandr2 libxfixes3 libxrender1 libxkbcommon-x11-0 \
  xvfb >/dev/null 2>&1
echo "  ok (glibc: $(ldd --version | head -1 | grep -oE '[0-9]+\.[0-9]+$'))"

echo "=== rustup (system linker) ==="
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null 2>&1
. "$HOME/.cargo/env"
echo "  $(rustc --version)"

echo "=== clone tag + bottle-layout install ==="
git clone -q --depth 1 --branch "v${V}" https://github.com/kingb/ember /src
cd /src
export CARGO_BUILD_JOBS=4
STAGE=/bottle
mkdir -p "$STAGE/ember"
cargo install --path crates/ember-app --root "$STAGE/ember/${V}" 2>&1 | tail -1
BIN="$STAGE/ember/${V}/bin/ember-term"
"$BIN" --version | head -1 | sed 's/^/  /'

echo "=== glibc ceiling check (max versioned symbol must be <= 2.35) ==="
MAX=$(objdump -T "$BIN" | grep -oE 'GLIBC_[0-9]+\.[0-9]+' | sort -Vu | tail -1)
echo "  max symbol: ${MAX}"
case "$MAX" in
  GLIBC_2.3[0-5]|GLIBC_2.[0-9]|GLIBC_2.1[0-9]|GLIBC_2.2[0-9])
    echo "  PASS: within the 22.04 ceiling" ;;
  *) echo "  FAIL: exceeds glibc 2.35 — bottle would repeat the 0.2.0 bug"; exit 1 ;;
esac

echo "=== windowed smoke under Xvfb (catches windowed-only dep misses) ==="
Xvfb :99 -screen 0 1024x700x24 >/dev/null 2>&1 &
sleep 2
export DISPLAY=:99 WGPU_BACKEND=vulkan WINIT_UNIX_BACKEND=x11
export VK_ICD_FILENAMES="$(ls /usr/share/vulkan/icd.d/lvp_icd*.json | head -1)"
EMBER_CONTROL=1 "$BIN" >/tmp/app.log 2>&1 &
APP=$!
sleep 8
kill -0 $APP 2>/dev/null && echo "  PASS: windowed app alive on X11" || { echo "  FAIL:"; tail -5 /tmp/app.log; exit 1; }
"$BIN" ctl state >/dev/null 2>&1 && echo "  PASS: ctl reaches it" || echo "  WARN: ctl unreachable"
kill $APP 2>/dev/null || true

echo "=== pack ==="
cd "$STAGE"
TARBALL="ember-${V}.${ARCH_TAG}.bottle.tar.gz"
tar czf "/out/${TARBALL}" ember
shasum -a 256 "/out/${TARBALL}" | awk '{print "  sha256 " $1}'
echo "BOTTLE_DONE ${ARCH_TAG}"
