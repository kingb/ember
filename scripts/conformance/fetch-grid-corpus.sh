#!/usr/bin/env bash
# Fetch the shared grid conformance corpus from the public gridwire repo at the
# revision pinned in conformance/GRIDWIRE_REV, for the grid_conformance test
# (crates/ember-session/tests/grid_conformance.rs).
#
#   scripts/conformance/fetch-grid-corpus.sh [dest]   # default: target/gridwire-corpus
#
# Bump the pin deliberately: a new corpus revision can add cases the projection
# must now agree with.
set -euo pipefail
cd "$(dirname "$0")/../.."

REV="$(tr -d '[:space:]' < conformance/GRIDWIRE_REV)"
DEST="${1:-target/gridwire-corpus}"
URL="https://github.com/kingb/gridwire"

rm -rf "$DEST"
git init -q "$DEST"
git -C "$DEST" remote add origin "$URL"
git -C "$DEST" fetch -q --depth 1 origin "$REV"
git -C "$DEST" -c advice.detachedHead=false checkout -q FETCH_HEAD

[[ "$(git -C "$DEST" rev-parse HEAD)" == "$REV" ]] || { echo "fetched revision does not match the pin $REV"; exit 1; }
n="$(find "$DEST/conformance/grid" -name '*.expect.json' | wc -l | tr -d ' ')"
[[ "$n" -gt 0 ]] || { echo "no corpus cases at $DEST/conformance/grid"; exit 1; }
echo "grid corpus: $n case(s) at $DEST/conformance/grid (gridwire ${REV:0:7})"
