# Benchmarks

Reproducible measurements of what Ember costs at runtime. Every release
publishes results from these scripts, so they need to be trustworthy: the
protocol below exists because ad-hoc numbers on a busy machine swing wildly
(we've measured 85% run-to-run variance on the same binary under load).

## Protocol

- **Quiet machine.** No builds, no indexing, browsers minimized. Numbers taken
  under load are noise, not data.
- **Hands off during the run.** Input wakes compositors and skews idle numbers.
- **Window visible.** Ember intentionally stops rendering when occluded, so a
  covered window measures the wrong thing.
- **Alternating passes.** Each script runs every scenario twice, interleaved.
  Report both; if the passes disagree noticeably, the machine wasn't quiet —
  rerun rather than average away the disagreement.
- **Isolated config.** Scripts point `XDG_CONFIG_HOME` at a throwaway dir;
  your real settings are never touched.

## Scripts

| Script | Measures | Needs |
|---|---|---|
| `idle-cpu.sh` | Process CPU% while idling: flat vs gradient vs sparks | a release build |
| `gpu-idle.sh` | GPU power (mW) + residency, per-process GPU ms/s, same scenarios plus a no-Ember baseline | `sudo` (powermetrics) |

```sh
cargo build --release -p ember-app
scripts/bench/idle-cpu.sh
sudo scripts/bench/gpu-idle.sh
```

## Reference results

**v0.2.0** (release binary, 2026-07-06, MacBook Apple Silicon, macOS 26,
quiet machine), idle CPU (`idle-cpu.sh`) — published in the release body:

| Scenario | Pass 1 | Pass 2 |
|---|---|---|
| flat (gradient off, sparks off) | 0.90% | 1.00% |
| gradient (on, sparks off) | 1.03% | 1.00% |
| sparks (gradient + sparks) | 0.60% | 0.77% |

An earlier dev-build run on the same machine measured 0.27–0.43% across the
same scenarios: absolute numbers move with machine state, which is why each
table records both passes and its conditions. Two findings have held across
every run so far: gradient == flat, and sparks consistently *below* the
un-animated scenarios (the capped 30fps cadence appears to batch wakeups
better than the idle event loop's scattered ones — unexplained, worth a
profile someday).

Gradient is identical to flat (it draws statically), which is why it's on by
default. Sparks are CPU-negligible; their real cost is GPU/display power,
which is what `gpu-idle.sh` exists to measure. The sparks-slightly-lower
reading is at the edge of cputime resolution; treat differences under ~0.2%
as noise.

GPU reference numbers are pending a valid run. The first attempt was
discarded: the screen locked partway through, and a locked screen occludes
the window — Ember intentionally stops rendering when occluded, so the run
measured "Ember doing nothing," not "sparks are cheap." Two lessons now baked
into `gpu-idle.sh`: it holds the display awake with `caffeinate`, and it
stamps every scenario with the screen-lock state so a contaminated run
identifies itself instead of masquerading as a good one.
