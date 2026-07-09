#!/usr/bin/env bash
# Runs INSIDE ghcr.io/homebrew/ubuntu22.04 (brew preinstalled, linuxbrew user).
# Installs Ember from the stamped formula and asserts it POURS the bottle
# (not a source build) and runs. The formula's bottle block points at the
# real release, so this exercises the whole chain: formula -> root_url
# -> uploaded bottle -> pour.
set -e
TAP="$(brew --repository)/Library/Taps/kingb/homebrew-ember"
mkdir -p "$TAP/Formula"
cp /tmp/ember.rb "$TAP/Formula/ember.rb"
( cd "$TAP" && git init -q && git add -A && git -c user.email=x@x -c user.name=x commit -qm init )

echo "=== brew install kingb/ember/ember ==="
OUT="$(brew install kingb/ember/ember 2>&1)"
echo "$OUT" | grep -iE "Pouring|installing.*from source|Error|Warning" | head -10 | sed 's/^/  /'

if echo "$OUT" | grep -qi "Pouring"; then
  echo "  PASS: poured the bottle (no source build)"
else
  echo "  FAIL: did not pour — full output:"; echo "$OUT" | tail -25 | sed 's/^/    /'; exit 1
fi

echo "=== runs + reports ${WANT:-the expected version} ==="
 V="$(ember-term --version | head -1)"
echo "  $V"
if [ -n "${WANT:-}" ]; then echo "$V" | grep -q "$WANT" && echo "  PASS: version $WANT" || { echo "  FAIL: wanted $WANT"; exit 1; }; else echo "  (version: $V)"; fi

echo "=== link sanity (GPU stack dlopened, not linked) ==="
ldd "$(brew --prefix)/bin/ember-term" | grep -viE "libc\.|libm\.|libgcc|linux-vdso|ld-linux|libpthread|libdl|librt" | sed 's/^/  /' || echo "  (only libc-family linked — correct)"
echo "POUR_VERIFY_DONE"
