//! `ember-core` — pure domain layer for Ember (design §2, §5).
//!
//! Session/layout/matching logic with zero IO. Concrete domain types
//! (the `SessionBackend` contract, layout tree, matchers) land in later
//! epics; this is the empty-but-real foundation.

/// The crate version, surfaced for diagnostics and the `ember-term --version`
/// banner. Acts as the workspace's first real, linkable symbol.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_matches_package() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
    }
}
