//! `vt-session` — SessionBackend implementations (design §4).
//!
//! `LocalPty` (v1), `TmuxControlMode` (phase 2), `a future out-of-process backend` (future)
//! land in later epics. Empty-but-real stub that proves the `vt-core` link.

/// Returns the `vt-core` version this backend layer is built against.
pub fn core_version() -> &'static str {
    vt_core::version()
}

#[cfg(test)]
mod tests {
    #[test]
    fn links_against_core() {
        assert!(!super::core_version().is_empty());
    }
}
