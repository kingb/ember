//! `ember-core` — pure domain layer for Ember (design §2, §5).
//!
//! Session/layout/matching logic with zero IO. The `SessionBackend` contract
//! and matchers land in later epics; the multiplexer model (layout tree,
//! layout fn, focus, commands) lives here.

pub mod app;
pub mod backend;
pub mod command;
pub mod config;
pub mod focus;
pub mod geom;
pub mod grid;
pub mod ids;
pub mod layout;
pub mod links;
pub mod settings;
pub mod windows;

pub use app::{AppState, ChromeRow, ChromeRowKind, ChromeState, Gate, GateId, GateRegistry};
pub use backend::{
    BackendControl, BackendEvent, BackendHandle, ClipboardOp, ExitStatus, FrameRx, FrameTx,
    OscEvent, PassthroughEvent, ScrollAmount, SessionBackend, VtProjection, frame_channel,
};
pub use command::{LayoutCommand, LayoutEffect, apply};
pub use config::{Background, Config, Font};
pub use focus::{Direction, focus_dir};
pub use geom::Rect;
pub use grid::{
    Attrs, CellContent, CellPatch, CursorShape, CursorState, GridDelta, GridDims, MarkStatus,
    MouseProto, NeutralCell, Rgb, Style, StyleId,
};
pub use ids::{PaneId, SessionId, TabId};
pub use layout::{Axis, LayoutNode, Tab, WindowTree, layout};
pub use links::{UrlMatch, find_urls};
pub use settings::{Help, RowKind, SettingRow, SettingsRowView, resolve_rows, setting_rows};
pub use windows::{MoveEffect, MoveError, SurfaceDest, SurfaceRef, Windows, move_surface};

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
