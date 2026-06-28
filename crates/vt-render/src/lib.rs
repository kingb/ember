//! `vt-render` — wgpu + glyphon + egui consumer (design §6).
//!
//! Owns the neutral grid and applies owned frame deltas; never borrows engine
//! memory. The GPU pipelines and glyph atlas land in later epics. Empty-but-real
//! stub that proves the `vt-core` link.

/// Returns the `vt-core` version this render layer is built against.
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
