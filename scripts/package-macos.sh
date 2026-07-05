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
  echo "→ building ${DMG}…"
  rm -f "${DMG}"
  BG="scripts/assets/dmg-background.png"
  SIGN_ID="${CODESIGN_ID:--}"
  STAGE="$(mktemp -d)"
  cp -R "${APP}" "${STAGE}/"

  if command -v create-dmg >/dev/null 2>&1 && [[ -f "${BG}" ]]; then
    # Styled: branded background, positioned icons, a drag arrow to Applications.
    # create-dmg adds the Applications link and (given --codesign) signs the dmg.
    csargs=(); [[ "${SIGN_ID}" != "-" ]] && csargs=(--codesign "${SIGN_ID}")
    create-dmg \
      --volname "Ember" --background "${BG}" \
      --window-pos 200 120 --window-size 660 400 \
      --icon-size 128 --icon "Ember.app" 175 190 --app-drop-link 485 190 \
      --text-size 15 --no-internet-enable --hdiutil-quiet \
      "${csargs[@]}" "${DMG}" "${STAGE}"
  else
    # Fallback (no create-dmg): a plain drag-install dmg.
    ln -s /Applications "${STAGE}/Applications"
    hdiutil create -quiet -volname "Ember" -srcfolder "${STAGE}" -ov -format UDZO "${DMG}"
    [[ "${SIGN_ID}" != "-" ]] && codesign --force --sign "${SIGN_ID}" --timestamp "${DMG}"
  fi
  rm -rf "${STAGE}"

  # The dmg is its own quarantined container: Apple expects it Developer ID
  # signed (done above) AND notarized, or it fails Gatekeeper assessment.
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
