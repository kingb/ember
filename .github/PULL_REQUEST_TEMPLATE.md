## What does this change?

<!-- Describe the change and why it's needed. Link any related issues. -->

## Checklist

- [ ] `cargo fmt --all -- --check` passes
- [ ] `cargo clippy --all-targets -- -D warnings` passes
- [ ] `cargo test --all` passes
- [ ] Commits are small and focused (see `CONTRIBUTING.md`)
- [ ] `ember-core` stays free of `tokio`/IO/`winit`/`wgpu`, if touched
- [ ] `SessionBackend` / `PlatformBackend` seam boundaries are respected, if touched
