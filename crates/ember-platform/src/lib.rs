//! `ember-platform` — winit + PlatformBackend / OS seam (design §7).
//!
//! Clipboard, open-path/URL, global hotkey, notifications. `LinuxBackend` (v1)
//! and the future `MacBackend` land in later epics. Empty-but-real stub that
//! proves the `ember-core` link.

/// Returns the `ember-core` version this platform layer is built against.
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
