//! `ember-render` — wgpu + glyphon consumer (design §6).
//!
//! Owns the neutral grid and applies owned frame deltas; never borrows engine
//! memory. v1 renders monospace text via glyphon; per-cell color, the cursor
//! quad, and the egui chrome overlay land in later epics.

mod background;
pub(crate) mod boxdraw;
mod boxpaint;
mod canvas;
pub mod grid_model;
pub mod headless;
mod paint;
mod quads;
pub mod renderer;
pub mod selection;
mod sprite;
pub mod wisp;

pub use grid_model::{GridModel, LinkSource, LinkSpan};
pub use headless::CaptureError;
pub use renderer::{
    AboutInfo, BackdropParams, CELL_HEIGHT, CELL_WIDTH, ConfirmView, ImageFit, PaneModes,
    PaneSnapshot, RenderOutcome, Renderer, StripSlot, TabHit, TabLabel, VisiblePane,
};
pub use selection::{Point, Selection, SelectionMode};
pub use wisp::{WispRenderer, WispUnsupported};

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
