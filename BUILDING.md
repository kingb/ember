# Building & releasing Ember

Everyday development is plain Cargo:

```sh
cargo build                 # debug build
cargo run -p ember-app      # run it
cargo test --workspace      # tests
```

The binary is `ember-term` (crate `ember-app`).

## Linux build dependencies

On Debian/Ubuntu:

```sh
sudo apt install build-essential pkg-config libwayland-dev libxkbcommon-dev \
    libx11-dev libxcursor-dev libxi-dev libxrandr-dev libxcb1-dev \
    libvulkan-dev mesa-vulkan-drivers libfontconfig1-dev
```

Ember renders through wgpu, so it needs a working Vulkan driver (Mesa's
`lavapipe` software rasterizer is enough for headless or CI use).

At **runtime** the windowed app additionally needs `libxi6` (the X11 input
extension; winit fails at event-loop creation without it) and
`libxkbcommon-x11-0` (a separate library from `libxkbcommon`; the app panics
at startup on X11 without it), alongside the `libxkbcommon`/`libxcursor`
runtime libraries. Desktop installs usually have them all; minimal containers
don't — and note that headless `--screenshot` runs never create a window, so
they can't catch a missing windowed-only dependency. The X11 smoke test
(windowed app under Xvfb) is what catches this class.

## Packaging (macOS)

Build a double-clickable `Ember.app`:

```sh
scripts/bundle-macos.sh              # release, ad-hoc signed → target/Ember.app
scripts/bundle-macos.sh --debug      # debug build (faster iteration)
CODESIGN_ID="Developer ID Application: …" scripts/bundle-macos.sh   # signed
```

Then `open target/Ember.app` to launch, or install to /Applications:

```sh
scripts/bundle-macos.sh --install   # clean replace (rm -rf + ditto)
```

Don't `cp -r` over an existing `/Applications/Ember.app`: copying *into* the
old bundle merges files and leaves a stale code signature, which macOS refuses
with "the application can't be opened." The `--install` flag removes the old one
first.

## Cutting a release

The cask lives in [`Casks/ember.rb`](Casks/ember.rb) and installs from a GitHub
release artifact. To cut a release:

```sh
scripts/release-macos.sh            # build + zip (+ --dmg), stamp the cask's
                                    # version + sha256 from the built artifact
gh release create v0.1.0 target/dist/Ember-0.1.0-macos-arm64.zip \
    --title "Ember 0.1.0" --generate-notes
```

Upload **the exact zip `release-macos.sh` just built** (the `ditto` zip embeds
timestamps, so re-packaging changes the hash). `scripts/release-macos.sh` signs
with a Developer ID and notarizes through Apple when `CODESIGN_ID` and
`NOTARY_PROFILE` are set, so the app launches without a Gatekeeper warning.

Then publish the cask through the [tap](https://github.com/kingb/homebrew-ember),
a repo named `homebrew-ember`:

```sh
cp Casks/ember.rb ../homebrew-ember/Casks/ && \
    (cd ../homebrew-ember && git commit -am "ember 0.1.0" && git push)
```

The website's download page needs no manual bump: it reads the newest
release whose macOS assets are fully attached (checked every 10 minutes), so
uploading the release assets is what flips the page.

Users install with:

```sh
brew install --cask kingb/ember/ember     # brew maps kingb/ember → homebrew-ember
```

## Documentation screenshots

The website's doc screenshots regenerate deterministically per release from a
sanitized demo environment. See
[`scripts/docs-screenshots/README.md`](scripts/docs-screenshots/README.md).
