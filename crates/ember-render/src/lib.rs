//! `ember-render` — wgpu + glyphon consumer (design §6).
//!
//! Owns the neutral grid and applies owned frame deltas; never borrows engine
//! memory. v1 renders monospace text via glyphon; per-cell color, the cursor
//! quad, and the egui chrome overlay land in later epics.

pub mod grid_model;
pub mod headless;
pub mod quads;
pub mod renderer;

pub use grid_model::GridModel;
pub use renderer::{CELL_HEIGHT, CELL_WIDTH, Renderer, TabLabel, VisiblePane};

/// Returns the `ember-core` version this render layer is built against.
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
