//! Application state: the two sibling on-screen surfaces (design §5).
//!
//! A native terminal front-end has two kinds of rows, and conflating them into
//! the pane tree is a trap. `AppState` therefore holds **two siblings**, not one
//! tree: the agent-pane [`WindowTree`] and the PTY-less [`ChromeState`] /
//! [`GateRegistry`]. For v1 this is the *typed place* only — the second consumer
//! path that feeds chrome/gates (never through `SessionBackend`) is phase-3.

use serde::{Deserialize, Serialize};

use crate::ids::PaneId;
use crate::layout::WindowTree;

/// Kind of a structured, PTY-less chrome row (rail / timeline / inspector).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChromeRowKind {
    Rail,
    Timeline,
    Inspector,
}

/// One PTY-less structured row in the chrome surface.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChromeRow {
    pub kind: ChromeRowKind,
    pub text: String,
}

/// The non-pane structured surface (rail / timeline / inspector rows). Typed
/// placeholder; the bus feed that populates it is phase-3.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChromeState {
    pub rows: Vec<ChromeRow>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GateId(pub u64);

/// A gate affordance. Some gates attach to a pane; some float (`attached =
/// None`). `needs_you` marks the "needs you" state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Gate {
    pub id: GateId,
    pub label: String,
    pub needs_you: bool,
    pub attached: Option<PaneId>,
}

/// The registry of gate affordances. Typed placeholder; phase-3 feeds it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GateRegistry {
    pub gates: Vec<Gate>,
}

impl GateRegistry {
    /// Gates currently in the "needs you" state.
    pub fn needs_you(&self) -> impl Iterator<Item = &Gate> {
        self.gates.iter().filter(|g| g.needs_you)
    }
}

/// The two sibling surfaces of the app (design §5): agent panes + the PTY-less
/// chrome/gate surface. **Not** one tree.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AppState {
    pub layout: WindowTree,
    pub chrome: ChromeState,
    pub gates: GateRegistry,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_app_state_has_empty_surfaces() {
        let app = AppState::default();
        assert!(app.layout.tabs.is_empty());
        assert!(app.chrome.rows.is_empty());
        assert!(app.gates.gates.is_empty());
    }

    #[test]
    fn needs_you_yields_only_flagged_gates() {
        let registry = GateRegistry {
            gates: vec![
                Gate {
                    id: GateId(1),
                    label: "approve deploy".into(),
                    needs_you: true,
                    attached: Some(PaneId(3)),
                },
                Gate {
                    id: GateId(2),
                    label: "idle".into(),
                    needs_you: false,
                    attached: None,
                },
            ],
        };
        let flagged: Vec<GateId> = registry.needs_you().map(|g| g.id).collect();
        assert_eq!(flagged, vec![GateId(1)]);
    }

    #[test]
    fn gates_can_float_or_attach_to_a_pane() {
        let floating = Gate {
            id: GateId(9),
            label: "global".into(),
            needs_you: false,
            attached: None,
        };
        let bound = Gate {
            id: GateId(10),
            label: "pane".into(),
            needs_you: false,
            attached: Some(PaneId(5)),
        };
        assert_eq!(floating.attached, None);
        assert_eq!(bound.attached, Some(PaneId(5)));
    }
}
