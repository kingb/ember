//! Layout tree (design §5): Window -> Tabs -> binary split tree.

use crate::geom::Rect;
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

    /// All `(pane, session)` leaves in a-before-b order.
    pub fn leaves(&self) -> Vec<(PaneId, SessionId)> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves(&self, out: &mut Vec<(PaneId, SessionId)>) {
        match self {
            LayoutNode::Pane { id, session } => out.push((*id, session.clone())),
            LayoutNode::Split { a, b, .. } => {
                a.collect_leaves(out);
                b.collect_leaves(out);
            }
        }
    }

    /// The session backing `target`, if present.
    pub fn session_of(&self, target: PaneId) -> Option<&SessionId> {
        match self {
            LayoutNode::Pane { id, session } => (*id == target).then_some(session),
            LayoutNode::Split { a, b, .. } => a.session_of(target).or_else(|| b.session_of(target)),
        }
    }

    /// Replace the `target` pane leaf with `replacement`. Returns `Ok` if the
    /// target was found; on `Err` the (unused) replacement is handed back so a
    /// caller can keep trying without cloning.
    pub fn replace_pane(
        &mut self,
        target: PaneId,
        replacement: LayoutNode,
    ) -> Result<(), LayoutNode> {
        match self {
            LayoutNode::Pane { id, .. } if *id == target => {
                *self = replacement;
                Ok(())
            }
            LayoutNode::Pane { .. } => Err(replacement),
            LayoutNode::Split { a, b, .. } => match a.replace_pane(target, replacement) {
                Ok(()) => Ok(()),
                Err(r) => b.replace_pane(target, r),
            },
        }
    }

    /// Set the ratio of the split that directly encloses `target`. Returns
    /// whether such a split was found. Ratio is clamped to `[0.05, 0.95]`.
    pub fn set_split_ratio(&mut self, target: PaneId, new_ratio: f64) -> bool {
        if let LayoutNode::Split { ratio, a, b, .. } = self {
            let immediate = matches!(a.as_ref(), LayoutNode::Pane { id, .. } if *id == target)
                || matches!(b.as_ref(), LayoutNode::Pane { id, .. } if *id == target);
            if immediate {
                *ratio = new_ratio.clamp(0.05, 0.95);
                return true;
            }
            return a.set_split_ratio(target, new_ratio) || b.set_split_ratio(target, new_ratio);
        }
        false
    }
}

/// Remove the `target` pane from `node`, promoting its sibling into the removed
/// split. Returns the rebuilt tree (`None` if `target` was the whole tree) and
/// the removed pane's session (`None` if `target` was absent). Consumes `node`.
pub fn remove_pane(node: LayoutNode, target: PaneId) -> (Option<LayoutNode>, Option<SessionId>) {
    match node {
        LayoutNode::Pane { id, session } => {
            if id == target {
                (None, Some(session))
            } else {
                (Some(LayoutNode::Pane { id, session }), None)
            }
        }
        LayoutNode::Split { axis, ratio, a, b } => {
            let (na, sa) = remove_pane(*a, target);
            if let Some(s) = sa {
                // `a` yielded the target; promote `b` (or `a`'s remainder).
                let rebuilt = match na {
                    Some(x) => Some(LayoutNode::Split {
                        axis,
                        ratio,
                        a: Box::new(x),
                        b,
                    }),
                    None => Some(*b),
                };
                return (rebuilt, Some(s));
            }
            let (nb, sb) = remove_pane(*b, target);
            if let Some(s) = sb {
                let kept_a = na.expect("a kept when not removed");
                let rebuilt = match nb {
                    Some(x) => Some(LayoutNode::Split {
                        axis,
                        ratio,
                        a: Box::new(kept_a),
                        b: Box::new(x),
                    }),
                    None => Some(kept_a),
                };
                return (rebuilt, Some(s));
            }
            // Not found in either subtree; reconstruct unchanged.
            (
                Some(LayoutNode::Split {
                    axis,
                    ratio,
                    a: Box::new(na.expect("a present")),
                    b: Box::new(nb.expect("b present")),
                }),
                None,
            )
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

/// Pure layout: tile `area` into one rect per pane (design §5). Output order
/// matches [`LayoutNode::pane_ids`] (a before b). Child `a` takes the leading
/// `ratio` fraction of the split axis; `b` takes the remainder.
pub fn layout(node: &LayoutNode, area: Rect) -> Vec<(PaneId, Rect)> {
    let mut out = Vec::new();
    layout_into(node, area, &mut out);
    out
}

fn layout_into(node: &LayoutNode, area: Rect, out: &mut Vec<(PaneId, Rect)>) {
    match node {
        LayoutNode::Pane { id, .. } => out.push((*id, area)),
        LayoutNode::Split { axis, ratio, a, b } => {
            let (ra, rb) = match axis {
                Axis::Horizontal => {
                    let wa = area.width * ratio;
                    (
                        Rect::new(area.x, area.y, wa, area.height),
                        Rect::new(area.x + wa, area.y, area.width - wa, area.height),
                    )
                }
                Axis::Vertical => {
                    let ha = area.height * ratio;
                    (
                        Rect::new(area.x, area.y, area.width, ha),
                        Rect::new(area.x, area.y + ha, area.width, area.height - ha),
                    )
                }
            };
            layout_into(a, ra, out);
            layout_into(b, rb, out);
        }
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

    #[test]
    fn single_pane_fills_area() {
        let area = Rect::new(0.0, 0.0, 100.0, 50.0);
        assert_eq!(layout(&p(1), area), vec![(PaneId(1), area)]);
    }

    #[test]
    fn horizontal_split_divides_width() {
        let area = Rect::new(0.0, 0.0, 100.0, 50.0);
        let tree = LayoutNode::split(Axis::Horizontal, 0.5, p(1), p(2));
        assert_eq!(
            layout(&tree, area),
            vec![
                (PaneId(1), Rect::new(0.0, 0.0, 50.0, 50.0)),
                (PaneId(2), Rect::new(50.0, 0.0, 50.0, 50.0)),
            ]
        );
    }

    #[test]
    fn vertical_split_divides_height_by_ratio() {
        let area = Rect::new(0.0, 0.0, 100.0, 100.0);
        let tree = LayoutNode::split(Axis::Vertical, 0.25, p(1), p(2));
        assert_eq!(
            layout(&tree, area),
            vec![
                (PaneId(1), Rect::new(0.0, 0.0, 100.0, 25.0)),
                (PaneId(2), Rect::new(0.0, 25.0, 100.0, 75.0)),
            ]
        );
    }

    #[test]
    fn nested_split_recurses() {
        let area = Rect::new(0.0, 0.0, 100.0, 100.0);
        let tree = LayoutNode::split(
            Axis::Horizontal,
            0.5,
            p(1),
            LayoutNode::split(Axis::Vertical, 0.5, p(2), p(3)),
        );
        assert_eq!(
            layout(&tree, area),
            vec![
                (PaneId(1), Rect::new(0.0, 0.0, 50.0, 100.0)),
                (PaneId(2), Rect::new(50.0, 0.0, 50.0, 50.0)),
                (PaneId(3), Rect::new(50.0, 50.0, 50.0, 50.0)),
            ]
        );
    }
}
