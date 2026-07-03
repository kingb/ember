#!/usr/bin/env bash
# Build Ember.app — a double-clickable, Finder-launchable macOS bundle.
#
#   scripts/bundle-macos.sh              # release build, ad-hoc signed
#   scripts/bundle-macos.sh --debug      # debug build (faster, for testing)
#   CODESIGN_ID="Developer ID Application: …" scripts/bundle-macos.sh
#
# Output: target/Ember.app  (open it, or drag to /Applications).
set -euo pipefail

cd "$(dirname "$0")/.."

PROFILE="release"
CARGO_FLAGS=(--release)
INSTALL=0
for arg in "$@"; do
  case "$arg" in
    --debug)   PROFILE="debug"; CARGO_FLAGS=() ;;
    --install) INSTALL=1 ;;
  esac
done

APP_NAME="Ember"
BIN_NAME="ember-term"
BUNDLE_ID="com.emberterm.ember"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
ICON="crates/ember-app/assets/icon.icns"

APP="target/${APP_NAME}.app"
CONTENTS="${APP}/Contents"

echo "→ building ${BIN_NAME} (${PROFILE})…"
cargo build ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"} -p ember-app --bin "${BIN_NAME}"

echo "→ assembling ${APP}…"
rm -rf "${APP}"
mkdir -p "${CONTENTS}/MacOS" "${CONTENTS}/Resources"
cp "target/${PROFILE}/${BIN_NAME}" "${CONTENTS}/MacOS/${BIN_NAME}"
cp "${ICON}" "${CONTENTS}/Resources/icon.icns"

cat > "${CONTENTS}/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>            <string>${APP_NAME}</string>
    <key>CFBundleDisplayName</key>     <string>${APP_NAME}</string>
    <key>CFBundleExecutable</key>      <string>${BIN_NAME}</string>
    <key>CFBundleIdentifier</key>      <string>${BUNDLE_ID}</string>
    <key>CFBundleShortVersionString</key> <string>${VERSION}</string>
    <key>CFBundleVersion</key>         <string>${VERSION}</string>
    <key>CFBundleIconFile</key>        <string>icon</string>
    <key>CFBundlePackageType</key>     <string>APPL</string>
    <key>CFBundleInfoDictionaryVersion</key> <string>6.0</string>
    <key>LSMinimumSystemVersion</key>  <string>11.0</string>
    <key>NSHighResolutionCapable</key> <true/>
    <!-- A terminal is not a document-based app; it manages its own windows. -->
    <key>LSApplicationCategoryType</key> <string>public.app-category.developer-tools</string>
    <key>NSSupportsAutomaticGraphicsSwitching</key> <true/>
</dict>
</plist>
PLIST

# Sign: an explicit Developer ID if provided, else ad-hoc (`-`), which is
# enough for local double-click launch (Gatekeeper still warns on first open
# for un-notarized apps — right-click → Open, or notarize for distribution).
SIGN_ID="${CODESIGN_ID:--}"
echo "→ codesign (${SIGN_ID})…"
codesign --force --deep --sign "${SIGN_ID}" "${APP}"
codesign --verify --verbose "${APP}" 2>&1 | sed 's/^/   /'

echo "✓ built ${APP}  (v${VERSION})"

if [[ "${INSTALL}" == "1" ]]; then
  DEST="/Applications/${APP_NAME}.app"
  echo "→ installing to ${DEST} (clean replace)…"
  # Remove the old bundle FIRST. Copying into an existing .app merges files and
  # leaves a stale _CodeSignature, which macOS then refuses ("can't be opened").
  rm -rf "${DEST}"
  ditto "${APP}" "${DEST}"   # ditto preserves the bundle + code signature
  echo "✓ installed ${DEST}"
else
  echo "   open ${APP}                          # launch"
  echo "   scripts/bundle-macos.sh --install    # clean-install to /Applications"
fi
