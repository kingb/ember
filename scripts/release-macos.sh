#!/usr/bin/env bash
# Cut a macOS release: package Ember.app, then stamp the cask's version +
# sha256 so it points at the artifact you're about to upload.
#
#   scripts/release-macos.sh            # zip only
#   scripts/release-macos.sh --dmg      # zip + dmg
#
# Then:
#   gh release create "v${VERSION}" target/dist/Ember-${VERSION}.zip \
#       --title "Ember ${VERSION}" --notes "…"
#   (copy Casks/ember.rb into your homebrew-ember tap and push)
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
CASK="Casks/ember.rb"

# Build the artifact(s) and capture the zip's sha256.
scripts/package-macos.sh "$@"
ZIP="target/dist/Ember-${VERSION}.zip"
SHA="$(shasum -a 256 "${ZIP}" | awk '{print $1}')"

# Stamp the cask (version + sha256) in place.
/usr/bin/sed -i '' \
  -e "s/^  version \".*\"/  version \"${VERSION}\"/" \
  -e "s/^  sha256 \".*\"/  sha256 \"${SHA}\"/" \
  "${CASK}"

echo
echo "✓ stamped ${CASK} → v${VERSION} / ${SHA}"
echo "  next:"
echo "    gh release create v${VERSION} ${ZIP} --title \"Ember ${VERSION}\" --generate-notes"
echo "    cp ${CASK} <path-to>/homebrew-ember/Casks/  &&  git -C <tap> commit -am \"ember ${VERSION}\" && git -C <tap> push"
