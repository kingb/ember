//! `ember-session` — SessionBackend implementations (design §4).
//!
//! `LocalPty` (v1) drives `alacritty_terminal` through the [`projection`] into
//! the neutral grid. `TmuxControlMode` (phase 2) and `a future out-of-process backend` (future)
//! land later.

pub mod palette;
pub mod projection;

pub use palette::Palette;
pub use projection::AlacrittyProjection;

/// Returns the `ember-core` version this backend layer is built against.
pub fn core_version() -> &'static str {
    ember_core::version()
}

#[cfg(test)]
mod tests {
    #[test]
    fn links_against_core() {
        assert!(!super::core_version().is_empty());
    }
}
