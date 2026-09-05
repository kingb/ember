# Releasing Ember

The ordered, gated checklist for cutting a release. Work top to bottom. A step
marked **⛔ GATE** must pass before you continue: gates are where past releases
broke, so they fail loud instead of shipping something wrong.

Mechanics (script internals, signing setup) live in [BUILDING.md](BUILDING.md);
this file is the operator's runbook.

## Once the operator says go

"Go," "release," "ship it," or equivalent authorizes running every phase in
this file through to the end, in order, without stopping to ask permission
between phases or between steps within a phase. A **⛔ GATE** passing is not
a checkpoint to report and wait on — it's the reason to move to the next
line. Don't ask "should I continue to Phase N?"; continue.

Stop and report instead of proceeding when:

- **A GATE fails**, or a script exits non-zero, or a result is ambiguous
  rather than clean.
- **Something outside your access blocks a step** — an invalid or expired
  token, a locked credential, a missing permission — that you cannot safely
  route around. Say what's blocked and what you need from the operator; don't
  work around it via a side channel (e.g. deploying from a raw file tree
  because the CLI's stored login is stale).
- **A step is destructive or hard to reverse in a way this checklist doesn't
  already cover.** The steps written here — tagging, notarizing, publishing
  the release, pushing the formula/cask, deploying the site — are
  pre-authorized by "go." Anything *not* on this list (force-pushing,
  rewriting history, deleting a prior release) still needs to be asked for
  separately, same as any other session.
- **A result contradicts something already established this run** — a
  benchmark that inverts a prior measurement, a verification that disagrees
  with itself, a check that passed by accident (see the throughput-harness
  saga this runbook's later gates exist because of). That's grounds to pause
  and dig, not to push through on the newer number.

Finishing a phase, a gate passing, or a script completing cleanly are not
pause points — that's the entire value of saying "go" once instead of at
every step.

## Before you start: the scars

Every one of these has bitten a real release. Read once.

- **Keychain locks silently.** A locked login keychain makes the `ember-notary`
  credential report *"not found"* (not "locked") — signing and notarization
  die mid-chain as if the profile were deleted. Fix: unlock (open Keychain
  Access once), don't re-store. Run the whole macOS chain under `caffeinate`.
- **Screen lock = occlusion.** A locked/sleeping screen occludes windows;
  Ember stops rendering when occluded, which invalidates benchmarks and the
  windowed smoke, and also locks the keychain. Stay awake for the macOS chain
  and any measurement.
- **Commit identity.** Some shells and CI set `GIT_AUTHOR_*` env vars that
  override `git config`. Every release commit must pass all four explicitly
  (`GIT_AUTHOR_NAME/EMAIL`, `GIT_COMMITTER_NAME/EMAIL`) so history carries the
  maintainer identity, not an environment default.
- **No internal references in public history.** Internal issue IDs and internal
  names must not land in commit messages, release notes, or the website. A
  pre-commit + commit-msg guard that blocks them (checking both the diff and
  the message) is worth keeping installed.
- **Linux bottle = native 22.04.** Build on `ubuntu:22.04` with the *system*
  toolchain (rustup + system linker), never Homebrew's rust (its glibc 2.39
  breaks GPU-driver loading on 22.04). Enforce the glibc-2.35 ceiling.
- **Bottle filename is single-dash** (`ember-X.Y.Z.arch.bottle.tar.gz`), and
  `--platform` must be pinned per arch or Docker reuses the last-pulled image.
- **macOS zips use `ditto`,** never plain `zip` (timestamps change the hash).

---

## Phase 0 — Prep

- [ ] Scope agreed; `CHANGELOG.md` `[Unreleased]` reflects every shipped change.
- [ ] Version chosen per SemVer: patch (fixes), minor (features), major (breaks).
- [ ] `main` pulled; `cargo test --workspace` green; `cargo clippy` clean.
- [ ] Any feature branches merged and seam-reviewed.
- [ ] **⛔ GATE — throughput.** `scripts/bench/throughput.sh <last-release-tag>
      main --rounds 8` passes. AC power, High Power Mode, machine quiet. This
      compares against the LAST RELEASED TAG, not an older one — the script's
      built-in default goes stale the moment you ship.
- [ ] **⛔ GATE — re-run after any late change.** If ANY commit lands after
      that gate ran — a revert, a fix, a merge, anything — the gate result is
      void. Run it again against the tip you actually intend to tag.

      This is not hypothetical bookkeeping. 0.5.0 was held at this gate, the
      offending commit was reverted, and the release shipped on the pre-revert
      measurement. The tag went out with `dense_ascii` parse time roughly
      doubled versus 0.4.2 on some Apple Silicon hardware, and nobody knew for
      seven weeks. The gate did its job; the process discarded the answer.
- [ ] **Sanity-check any large delta with the slot-swap control** before acting
      on it: put each ref in the baseline slot in turn. If the effect follows
      the slot rather than the ref, it is harness bias, not a finding.

## Phase 1 — Cut the version

- [ ] `CHANGELOG.md`: promote `[Unreleased]` → `## [X.Y.Z] - YYYY-MM-DD`
      (leave `[Unreleased]` empty above it); add the compare link at the bottom.
- [ ] Bump workspace `Cargo.toml` `version = "X.Y.Z"`.
- [ ] Commit (explicit identity) + push `main`.
- [ ] Annotated tag: `git tag -a vX.Y.Z -m "…"` && `git push origin vX.Y.Z`.
- [ ] **⛔ GATE:** `cargo build --release` succeeds and
      `target/release/ember-term --version` prints `X.Y.Z`.

## Phase 2 — macOS assets (the notarization chain)

- [ ] **⛔ GATE (pre-flight):** keychain unlocked —
      `xcrun notarytool history --keychain-profile ember-notary` succeeds.
      If it errors, unlock the keychain; do **not** re-store credentials.
- [ ] Run the per-arch chain **under `caffeinate -dimsu`** from a `vX.Y.Z`
      worktree: arm64 (build → hardened-runtime codesign → notarize → staple →
      dmg → notarize dmg → staple) then x86_64 (cross-build → swap binary →
      re-sign → notarize → staple). (Fully scripting this per-arch chain
      end-to-end is a tracked follow-up; today it runs from a scripted chain in
      the release worktree.)
- [ ] **⛔ GATE:** every `notarytool submit` returns `status: Accepted` and
      `stapler validate` passes for each artifact.
- [ ] Upload all three: `Ember-X.Y.Z-macos-arm64.zip`, `-arm64.dmg`,
      `-x86_64.zip`.

## Phase 3 — GitHub release

- [ ] `gh release create vX.Y.Z` with notes written from the changelog but
      **public-safe** (no internal refs), in Ember's voice, with the benchmark
      table when relevant.
- [ ] **⛔ GATE:** the release lists all three macOS assets.

## Phase 4 — macOS cask

- [ ] Stamp `Casks/ember.rb`: `version` + per-arch **zip** sha256s.
- [ ] Commit + push to the main repo; copy to the `homebrew-ember` tap, commit
      + push.
- [ ] **⛔ GATE:** `brew livecheck --cask kingb/ember/ember` shows
      `old ==> X.Y.Z`.

## Phase 5 — Linux bottles

- [ ] Build each arch natively: `docker run --platform linux/{arm64,amd64}
      … ubuntu:22.04 bash scripts/release/bottle-build.sh X.Y.Z {arm64,x86_64}_linux`.
- [ ] **⛔ GATE (in-build):** the script's own checks pass — glibc ≤ 2.35 and
      the Xvfb windowed smoke. (The script exits non-zero otherwise.)
- [ ] Upload both bottles (single-dash names) to the release.
- [ ] Stamp `Formula/ember.rb`: `url` + source tarball sha256, `root_url`, both
      bottle sha256s. Verify the arm64 line matches the uploaded asset's sha.
- [ ] **⛔ GATE:** `scripts/release/pour-verify.sh` on a clean
      `ghcr.io/homebrew/brew` container **pours** the bottle (no source build)
      and reports `X.Y.Z` — run this **before** pushing the formula.
- [ ] Push the formula to the tap.
- [ ] **⛔ GATE:** `brew livecheck --formula kingb/ember/ember` shows `X.Y.Z`.

## Phase 6 — Website

- [ ] Sync `content/CHANGELOG.md` ← repo `CHANGELOG.md`.
- [ ] Deploy (`vercel deploy --prod`).
- [ ] **⛔ GATE:** `emberterm.com/changelog` shows `X.Y.Z`, and the download
      page shows `X.Y.Z` (it auto-derives from the newest release whose macOS
      assets all exist — no manual bump).

## Phase 7 — Docs (only when the release adds user-facing features)

- [ ] New/updated sections for each feature, each with an
      `Added in vX.Y.Z` pill (keep the macOS/Linux shortcut toggle in sync with
      `help_lines()`).
- [ ] Regenerate the screenshot set on the new binary:
      `scripts/docs-screenshots/{demo-env,shots}.sh`, copy to
      `public/screenshots/`, deploy.
- [ ] Tracked separately as the multi-page docs restructure effort.

## Phase 8 — Close-out

- [ ] Close GitHub issues shipped by this release, commenting the release link.
- [ ] Update the issue tracker: close shipped work, note follow-ups.
- [ ] Notify the team of the release and hand off any follow-on work.
- [ ] Confirm `brew upgrade` picks up the new version on a real machine.
- [ ] **Symmetry:** the release carries assets for every platform (macOS
      zip/dmg + both Linux bottles). Build a tag's Linux bottles before cutting
      the next version, even if a patch supersedes it quickly, so no tagged
      release is left half-populated.

---

## Script index

| Script | Phase | What it does |
|---|---|---|
| `scripts/release-macos.sh` | 2 | Single-arch build + zip + cask stamp (per-arch chain wraps it) |
| `scripts/release/bottle-build.sh` | 5 | Native 22.04 bottle build + glibc-ceiling + Xvfb smoke |
| `scripts/release/pour-verify.sh` | 5 | Clean-container `brew install` pour assertion |
| `scripts/smoke/x11-container-smoke.sh` | test | Windowed app + drag on X11 under Xvfb |
| `scripts/bench/throughput.sh` | 0 | **⛔ Throughput gate** vs the last released tag; re-run after any late change |
| `scripts/bench/{idle-cpu,gpu-idle}.sh` | notes | Idle CPU / GPU cost for the release benchmark table |
