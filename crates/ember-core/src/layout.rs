//! Layout tree (design §5): Window -> Tabs -> binary split tree.

use crate::ids::{PaneId, SessionId, TabId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Axis {
    /// Side-by-side panes (a | b); the divider runs vertically.
    Horizontal,
    /// Stacked panes (a above b); the divider runs horizontally.
    Vertical,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum LayoutNode {
    Split {
        axis: Axis,
        ratio: f64,
        a: Box<LayoutNode>,
        b: Box<LayoutNode>,
    },
    Pane {
        id: PaneId,
        session: SessionId,
    },
}

impl LayoutNode {
    pub fn pane(id: PaneId, session: SessionId) -> Self {
        LayoutNode::Pane { id, session }
    }

    /// Build a split, clamping `ratio` to a sane visible range.
    pub fn split(axis: Axis, ratio: f64, a: LayoutNode, b: LayoutNode) -> Self {
        LayoutNode::Split {
            axis,
            ratio: ratio.clamp(0.05, 0.95),
            a: Box::new(a),
            b: Box::new(b),
        }
    }

    /// All pane ids in left-to-right / top-to-bottom (a before b) order.
    pub fn pane_ids(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.collect_ids(&mut out);
        out
    }

    fn collect_ids(&self, out: &mut Vec<PaneId>) {
        match self {
            LayoutNode::Pane { id, .. } => out.push(*id),
            LayoutNode::Split { a, b, .. } => {
                a.collect_ids(out);
                b.collect_ids(out);
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tab {
    pub id: TabId,
    pub title: String,
    pub root: LayoutNode,
    pub focus: PaneId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowTree {
    pub tabs: Vec<Tab>,
    pub active: usize,
}

impl WindowTree {
    pub fn active_tab(&self) -> &Tab {
        &self.tabs[self.active]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(n: u64) -> LayoutNode {
        LayoutNode::pane(PaneId(n), SessionId::new(format!("s{n}")))
    }

    #[test]
    fn ratio_is_clamped() {
        let LayoutNode::Split { ratio, .. } = LayoutNode::split(Axis::Horizontal, 1.5, p(1), p(2))
        else {
            panic!("expected split");
        };
        assert_eq!(ratio, 0.95);
    }

    #[test]
    fn pane_ids_in_a_before_b_order() {
        let tree = LayoutNode::split(
            Axis::Horizontal,
            0.5,
            p(1),
            LayoutNode::split(Axis::Vertical, 0.5, p(2), p(3)),
        );
        assert_eq!(tree.pane_ids(), vec![PaneId(1), PaneId(2), PaneId(3)]);
    }
}
