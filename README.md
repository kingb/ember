# Ember

Ember started with a simple question, **where's iTerm2 for Linux?**, and grew into
its own thing: a native terminal emulator, built from scratch in Rust for macOS and
Linux. Not a port of the macOS source, not an extension of an existing terminal, but
a daily-driver replacement in its own right.

- **Crate / binary:** `ember-term`
- **Status:** Design approved; implementation planning underway.
- **Design doc:** [`docs/design/2026-06-27-ember-design.md`](docs/design/2026-06-27-ember-design.md)

## Architecture at a glance

Layered, single-process Rust workspace. The daemon/multi-process split is deferred, but
its boundary is front-loaded.

| Crate | Responsibility |
|---|---|
| `ember-core` | Pure domain: `SessionBackend` trait, layout tree, focus/layout, profiles, OSC/trigger matching. No IO. |
| `ember-session` | Backend impls: `LocalPty` (v1), `TmuxControlMode` (phase 2), `a future out-of-process backend` (future). |
| `ember-render` | wgpu + glyphon + custom GPU chrome + egui overlay. |
| `ember-platform` | winit + `PlatformBackend` (clipboard, open, hotkey). macOS seam. |
| `ember-app` | Binary: event loop, input routing, layout, config; trigger dispatch. |

Two extension seams: **`SessionBackend`** (tmux / daemon / bus) and **`PlatformBackend`**
(macOS). See the design doc for the full picture, including the projection-based render
seam, the two-lane event sink, and the v1 → phase-2 → phase-3 roadmap.

## Stack

`winit` · `wgpu` · `glyphon`/`cosmic-text` · `alacritty_terminal` (swappable) ·
`portable-pty` · `egui`.

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

Don't `cp -r` over an existing `/Applications/Ember.app` — copying *into* the
old bundle merges files and leaves a stale code signature, which macOS refuses
with "the application can't be opened." The `--install` flag removes the old one
first.

### Distributing via Homebrew

The cask lives in [`Casks/ember.rb`](Casks/ember.rb) and installs from a GitHub
release artifact. To cut a release:

```sh
scripts/release-macos.sh            # build + zip (+ --dmg), stamp the cask's
                                    # version + sha256 from the built artifact
gh release create v0.1.0 target/dist/Ember-0.1.0.zip \
    --title "Ember 0.1.0" --generate-notes
```

Upload **the exact zip `release-macos.sh` just built** (the `ditto` zip embeds
timestamps, so re-packaging changes the hash). Then publish the cask through a
[tap](https://docs.brew.sh/Taps) — a repo named `homebrew-ember`:

```sh
# one-time: create github.com/kingb/homebrew-ember with a Casks/ dir
cp Casks/ember.rb ../homebrew-ember/Casks/ && (cd ../homebrew-ember && git commit -am "ember 0.1.0" && git push)
```

Once the tap is published, users will install with:

```sh
brew install --cask kingb/ember/ember     # brew maps kingb/ember → homebrew-ember
```

The Homebrew tap is not published yet. Until it is, build from source (above).

Because the build is ad-hoc signed (not notarized), the first launch still hits
a Gatekeeper warning — the cask's `caveats` tell users to right-click → Open or
strip the quarantine attribute. Sign with a Developer ID and notarize to remove
that step; `scripts/bundle-macos.sh` already accepts `CODESIGN_ID`.

