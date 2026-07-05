#!/usr/bin/env bash
# Package Ember.app into distributable artifacts for a Homebrew cask / release.
#
#   scripts/package-macos.sh            # → target/dist/Ember-<ver>.zip  (+ sha256)
#   scripts/package-macos.sh --dmg      # also → target/dist/Ember-<ver>.dmg
#
# The .zip is what the Homebrew cask installs (signature-preserving via ditto).
# Upload the artifact(s) to a GitHub release tagged v<ver>; the printed sha256
# goes into the cask (scripts/release-macos.sh does this for you).
set -euo pipefail
cd "$(dirname "$0")/.."

MAKE_DMG=0
[[ "${1:-}" == "--dmg" ]] && MAKE_DMG=1

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
APP="target/Ember.app"
DIST="target/dist"
ZIP="${DIST}/Ember-${VERSION}.zip"
DMG="${DIST}/Ember-${VERSION}.dmg"

# (Re)build the signed bundle.
scripts/bundle-macos.sh >/dev/null
mkdir -p "${DIST}"

# ditto preserves the bundle layout + code signature (plain `zip` can corrupt
# extended attributes / the signature).
echo "→ zipping ${ZIP}…"
rm -f "${ZIP}"
ditto -c -k --keepParent "${APP}" "${ZIP}"

# Notarize + staple if a notary keychain profile is configured (set up once via
# `xcrun notarytool store-credentials`). The zip is submitted to Apple; on
# success the ticket is stapled INTO the .app so it validates offline, and the
# zip is rebuilt from the stapled bundle. Any dmg below is then built from the
# stapled app too.
if [[ -n "${NOTARY_PROFILE:-}" ]]; then
  echo "→ notarizing via profile '${NOTARY_PROFILE}' (a few minutes)…"
  xcrun notarytool submit "${ZIP}" --keychain-profile "${NOTARY_PROFILE}" --wait
  echo "→ stapling the ticket to ${APP}…"
  xcrun stapler staple "${APP}"
  xcrun stapler validate "${APP}" 2>&1 | sed 's/^/   /'
  echo "→ re-zipping the stapled bundle…"
  rm -f "${ZIP}"
  ditto -c -k --keepParent "${APP}" "${ZIP}"
fi

if [[ "${MAKE_DMG}" == "1" ]]; then
  echo "→ building ${DMG} (drag-to-Applications)…"
  rm -f "${DMG}"
  STAGE="$(mktemp -d)"
  cp -R "${APP}" "${STAGE}/"
  ln -s /Applications "${STAGE}/Applications"   # drag-install target
  hdiutil create -quiet -volname "Ember" -srcfolder "${STAGE}" \
    -ov -format UDZO "${DMG}"
  rm -rf "${STAGE}"

  # The dmg contains the already-stapled app, but the dmg is its own quarantined
  # container, so notarize + staple it too for a clean download-and-open.
  if [[ -n "${NOTARY_PROFILE:-}" ]]; then
    echo "→ notarizing the dmg via profile '${NOTARY_PROFILE}'…"
    xcrun notarytool submit "${DMG}" --keychain-profile "${NOTARY_PROFILE}" --wait
    xcrun stapler staple "${DMG}"
    xcrun stapler validate "${DMG}" 2>&1 | sed 's/^/   /'
  fi
fi

echo
echo "artifacts (v${VERSION}):"
for f in "${ZIP}" $([[ "${MAKE_DMG}" == "1" ]] && echo "${DMG}"); do
  sha="$(shasum -a 256 "${f}" | awk '{print $1}')"
  printf "  %-40s  %s\n" "${f}" "${sha}"
done
