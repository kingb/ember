# Contributing to Ember

Ember started as an answer to "where's iTerm2 for Linux?" and grew into its own
thing: a native terminal emulator in Rust for macOS and Linux. See
[`docs/design/2026-06-27-ember-design.md`](docs/design/2026-06-27-ember-design.md)
for the architecture and roadmap.

## Ground rules

- **`ember-core` stays pure.** No `tokio`/IO/`winit`/`wgpu` in `ember-core` — that
  invariant is what makes the domain exhaustively testable.
- **The seams stay honest.** Changes must respect the `SessionBackend` and
  `PlatformBackend` boundaries (design §4, §7).
- **Green before merge.** `cargo fmt --all -- --check`, `cargo clippy
  --all-targets -- -D warnings`, and `cargo test --all` must pass on Linux
  and macOS. CI enforces all three.

## Workflow

1. Branch from `main`.
2. Keep commits small and focused; write a failing test first where practical.
3. Run the three gates above locally before pushing.

## License

By contributing you agree your work is dual-licensed under
`MIT OR Apache-2.0`, matching the project.
