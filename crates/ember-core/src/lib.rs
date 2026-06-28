//! `ember-core` — pure domain layer for Ember (design §2, §5).
//!
//! Session/layout/matching logic with zero IO. The `SessionBackend` contract
//! and matchers land in later epics; the multiplexer model (layout tree,
//! layout fn, focus, commands) lives here.

pub mod geom;
pub mod ids;
pub mod layout;

pub use geom::Rect;
pub use ids::{PaneId, SessionId, TabId};
pub use layout::{Axis, LayoutNode, Tab, WindowTree};

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
