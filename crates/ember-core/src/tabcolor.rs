//! Per-tab color: the choice model and the precedence resolver (design:
//! tab-colors). Four layers, highest wins: hand-picked color > shell OSC 6 >
//! regex rule > none. A knob can swap the top two layers so OSC beats a
//! plain manual color pick, while a `PinnedDefault` (the user explicitly
//! asking for "no color") still blocks the rule layer.

use serde::{Deserialize, Serialize};

/// What the user has said about a tab's color, independent of what the shell
/// (OSC 6) or a regex rule would otherwise paint.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TabColorChoice {
    /// The user has said nothing — lower layers (OSC, rule) may paint.
    #[default]
    Unset,
    /// The user explicitly chose the uncolored look — blocks lower layers.
    PinnedDefault,
    /// The user picked a color, as `0xRRGGBB`.
    Color(u32),
}

/// Curated swatches, legible on the app's default background (`0x101010`).
pub const SWATCHES: [u32; 12] = [
    0xE25A1C, 0xFF9D3C, 0xE8C547, 0x4FAE6A, 0x3FB8AF, 0x5EA0E0, 0x8B7BD8, 0xC678DD, 0xD9534A,
    0xE05C8A, 0xA1887F, 0x8A96A3,
];

/// Resolve the color a tab should actually render, applying the four-layer
/// precedence: manual pick > OSC 6 > regex rule > none.
///
/// `osc_overrides_manual` is the knob that swaps the top two layers (OSC
/// beats a manual `Color`), but a `PinnedDefault` is not a manual color pick
/// to be out-ranked — it is a request to suppress the rule layer. Under the
/// knob, only a present OSC color beats the pin; the rule layer stays
/// blocked either way.
pub fn effective_color(
    user: TabColorChoice,
    osc: Option<u32>,
    rule: Option<u32>,
    osc_overrides_manual: bool,
) -> Option<u32> {
    match user {
        TabColorChoice::Color(c) => {
            if osc_overrides_manual {
                osc.or(Some(c))
            } else {
                Some(c)
            }
        }
        TabColorChoice::PinnedDefault => {
            if osc_overrides_manual {
                osc
            } else {
                None
            }
        }
        TabColorChoice::Unset => osc.or(rule),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const C: u32 = 0x112233;
    const O: u32 = 0x445566;
    const R: u32 = 0x778899;

    #[test]
    fn manual_beats_osc_beats_rule_by_default() {
        assert_eq!(
            effective_color(TabColorChoice::Color(C), Some(O), Some(R), false),
            Some(C)
        );
        assert_eq!(
            effective_color(TabColorChoice::Unset, Some(O), Some(R), false),
            Some(O)
        );
        assert_eq!(
            effective_color(TabColorChoice::Unset, None, Some(R), false),
            Some(R)
        );
        assert_eq!(
            effective_color(TabColorChoice::Unset, None, None, false),
            None
        );
    }

    #[test]
    fn pinned_default_blocks_everything_below() {
        assert_eq!(
            effective_color(TabColorChoice::PinnedDefault, Some(O), Some(R), false),
            None
        );
        // ...even when the knob makes OSC beat manual COLORS, a pin still pins:
        // the knob swaps layers 1 and 2; OSC present -> OSC wins over the pin.
        assert_eq!(
            effective_color(TabColorChoice::PinnedDefault, Some(O), Some(R), true),
            Some(O)
        );
        // knob on, no OSC -> the pin still blocks the rule layer.
        assert_eq!(
            effective_color(TabColorChoice::PinnedDefault, None, Some(R), true),
            None
        );
    }

    #[test]
    fn knob_swaps_only_the_top_two_layers() {
        assert_eq!(
            effective_color(TabColorChoice::Color(C), Some(O), Some(R), true),
            Some(O)
        );
        assert_eq!(
            effective_color(TabColorChoice::Color(C), None, Some(R), true),
            Some(C)
        );
        assert_eq!(
            effective_color(TabColorChoice::Unset, None, Some(R), true),
            Some(R)
        );
    }

    #[test]
    fn choice_serde_defaults_unset() {
        // Old snapshots / trees without the field must load as Unset.
        let t: TabColorChoice = serde_json::from_str("\"Unset\"").unwrap();
        assert_eq!(t, TabColorChoice::Unset);
    }

    /// Carried in from the Task 1 review: a whole `Tab` JSON written before
    /// this feature existed has no `"color"` key at all. `#[serde(default)]`
    /// on `Tab::color` (`layout.rs`) must fill it with `Unset` rather than
    /// failing to parse — this pins that against regression, alongside the
    /// unit-level `choice_serde_defaults_unset` above.
    #[test]
    fn tab_missing_color_field_deserializes_as_unset() {
        use crate::ids::{PaneId, SessionId, TabId};
        use crate::layout::{LayoutNode, Tab};

        let json = serde_json::json!({
            "id": 1,
            "title": "old snapshot",
            "root": { "Pane": { "id": 1, "session": "s1" } },
            "focus": 1,
        });
        let tab: Tab = serde_json::from_value(json).unwrap();
        assert_eq!(tab.color, TabColorChoice::Unset);
        // Sanity: the rest of the struct still parsed correctly, not just
        // fell back to defaults everywhere.
        assert_eq!(tab.id, TabId(1));
        assert_eq!(tab.title, "old snapshot");
        assert_eq!(tab.focus, PaneId(1));
        assert_eq!(tab.root, LayoutNode::pane(PaneId(1), SessionId::new("s1")));
    }
}
