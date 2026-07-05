# Documentation screenshots

Repeatable, sanitized, cross-platform screenshot generation for the docs. The
docs images change every release as the UI evolves, so these scripts regenerate
the whole set deterministically, in a throwaway environment that never exposes a
real machine.

## Usage

```sh
# Build the release binary first:
cargo build --release -p ember-app

# Build the sanitized demo HOME, then render the shot set:
scripts/docs-screenshots/demo-env.sh /tmp/ember-docs-home
scripts/docs-screenshots/shots.sh target/release/ember-term /tmp/ember-docs-home out/macos
```

`ember-term --screenshot` renders a deterministic scene to a PNG headlessly (no
window needed), which is what lets a display-less CI or container produce images.

## Sanitization

`demo-env.sh` builds a fake `$HOME` with a neutral prompt (a public handle, a
branded host), a small demo project, and a fake git history. Every shot runs
against it, so `ls`, `git log`, and the prompt show demo content, never real
user data. **Never point the screenshotter at a real `$HOME`.**

## Cross-platform parity

Run the same `shots.sh` on macOS natively and inside the Linux test container,
into `out/macos` and `out/linux`. Comparing the two sets is a free QA pass:
matching images mean the renderer is consistent across platforms; differences
flag a platform-specific rendering bug (fonts, box-drawing, colors, layout).

## Per release

Re-run both platforms, diff against the previous set, and update only the images
that changed plus any doc page describing changed UI. Deterministic scenes (fixed
size, scale, settle time, ember phase, and git dates) keep unrelated images
byte-stable between runs.
