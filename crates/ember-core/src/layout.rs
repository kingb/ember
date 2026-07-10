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
        /// A custom tab title this pane carries after its named tab was
        /// dissolved into a split ([`crate::move_surface`], Tab ->
        /// `SplitInto`). Restored (and consumed) when the pane is promoted
        /// back out to its own tab, so a user-renamed tab keeps its name
        /// across a merge/promote round-trip. `None` for panes that never
        /// were a named tab.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        carried_title: Option<String>,
    },
}

impl LayoutNode {
    pub fn pane(id: PaneId, session: SessionId) -> Self {
        LayoutNode::Pane {
            id,
            session,
            carried_title: None,
        }
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
            LayoutNode::Pane { id, session, .. } => out.push((*id, session.clone())),
            LayoutNode::Split { a, b, .. } => {
                a.collect_leaves(out);
                b.collect_leaves(out);
            }
        }
    }

    /// The session backing `target`, if present.
    pub fn session_of(&self, target: PaneId) -> Option<&SessionId> {
        match self {
            LayoutNode::Pane { id, session, .. } => (*id == target).then_some(session),
            LayoutNode::Split { a, b, .. } => a.session_of(target).or_else(|| b.session_of(target)),
        }
    }

    /// The carried tab title on the `target` pane leaf, if any — see the
    /// field's doc on [`LayoutNode::Pane`].
    pub fn carried_title_of(&self, target: PaneId) -> Option<&str> {
        match self {
            LayoutNode::Pane {
                id, carried_title, ..
            } => (*id == target).then_some(carried_title.as_deref()).flatten(),
            LayoutNode::Split { a, b, .. } => a
                .carried_title_of(target)
                .or_else(|| b.carried_title_of(target)),
        }
    }

    /// Set (or clear, with `None`) the carried tab title on the `target`
    /// pane leaf. Returns whether the target was found.
    pub fn set_carried_title(&mut self, target: PaneId, title: Option<String>) -> bool {
        match self {
            LayoutNode::Pane {
                id, carried_title, ..
            } if *id == target => {
                *carried_title = title;
                true
            }
            LayoutNode::Pane { .. } => false,
            LayoutNode::Split { a, b, .. } => {
                a.set_carried_title(target, title.clone()) || b.set_carried_title(target, title)
            }
        }
    }

    /// Remove and return the carried tab title from the `target` pane leaf —
    /// the "restore on promote" half of the round-trip.
    pub fn take_carried_title(&mut self, target: PaneId) -> Option<String> {
        match self {
            LayoutNode::Pane {
                id, carried_title, ..
            } if *id == target => carried_title.take(),
            LayoutNode::Pane { .. } => None,
            LayoutNode::Split { a, b, .. } => a
                .take_carried_title(target)
                .or_else(|| b.take_carried_title(target)),
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

    /// Whether `target` is a leaf anywhere in this subtree.
    pub fn contains(&self, target: PaneId) -> bool {
        match self {
            LayoutNode::Pane { id, .. } => *id == target,
            LayoutNode::Split { a, b, .. } => a.contains(target) || b.contains(target),
        }
    }

    /// Grow `target`'s side of the **nearest enclosing split of `axis`** (walking
    /// up from the leaf) by `delta` **physical pixels** — core converts px→ratio
    /// using the split's own extent, so both a keyboard step and a mouse-drag
    /// delta work uniformly. Positive `delta` grows the target. `area` is this
    /// node's current rect; `min_px` is the smallest extent either child may
    /// shrink to (the clamp). Returns whether a matching split was adjusted.
    pub fn resize_pane(
        &mut self,
        target: PaneId,
        axis: Axis,
        delta: f64,
        area: Rect,
        min_px: f64,
    ) -> bool {
        let LayoutNode::Split {
            axis: split_axis,
            ratio,
            a,
            b,
        } = self
        else {
            return false;
        };
        let split_axis = *split_axis;
        let in_a = a.contains(target);
        if !in_a && !b.contains(target) {
            return false;
        }
        let (ra, rb) = split_child_rects(split_axis, *ratio, area);
        // Nearest enclosing = deepest: try to adjust a matching split further
        // down first; only handle it here if nothing deeper did.
        let (child, child_area) = if in_a { (a, ra) } else { (b, rb) };
        if child.resize_pane(target, axis, delta, child_area, min_px) {
            return true;
        }
        if split_axis != axis {
            return false;
        }
        let extent = axis_extent(axis, area);
        if extent <= 0.0 {
            return false;
        }
        // Clamp so neither child drops below `min_px` (best-effort: if the split
        // can't even hold two minimums, pin to the midpoint rather than refuse —
        // resize is continuous, unlike a split which refuses outright).
        let min_r = (min_px / extent).min(0.5);
        let signed = if in_a { delta } else { -delta };
        *ratio = (*ratio + signed / extent).clamp(min_r, 1.0 - min_r);
        true
    }

    /// Grow `a_side`'s side of the split that **separates** `a_side` and
    /// `b_side` by `delta` **physical pixels**. A divider is bordered by
    /// exactly two panes; this identifies the split by BOTH of them rather
    /// than by one pane + axis, which is ambiguous whenever that pane sits
    /// inside more than one same-axis split (e.g. a left-leaning
    /// `Split(H, Split(H, P1, P2), P3)`: `resize_pane(P2, H, ..)` would hit
    /// the *inner* P1|P2 split first, even when the caller meant the outer
    /// P2|P3 divider). The separating split is the unique node whose two
    /// children respectively contain `a_side` and `b_side` — their nearest
    /// common ancestor. Positive `delta` grows `a_side`. `area` is this
    /// node's current rect; `min_px` is the smallest extent either child may
    /// shrink to (the clamp, same semantics as [`resize_pane`]). Returns
    /// whether a matching split was found and adjusted.
    ///
    /// [`resize_pane`]: LayoutNode::resize_pane
    pub fn resize_split(
        &mut self,
        a_side: PaneId,
        b_side: PaneId,
        axis: Axis,
        delta: f64,
        area: Rect,
        min_px: f64,
    ) -> bool {
        let LayoutNode::Split {
            axis: split_axis,
            ratio,
            a,
            b,
        } = self
        else {
            return false;
        };
        let split_axis = *split_axis;
        let a_has_a_side = a.contains(a_side);
        let a_has_b_side = a.contains(b_side);
        let b_has_a_side = b.contains(a_side);
        let b_has_b_side = b.contains(b_side);

        // This split's children hold one flanking pane each: it separates
        // them, so it's the divider's split — adjust here regardless of
        // depth. (Unlike `resize_pane`, we do NOT prefer a deeper match: a
        // deeper split may contain one of the two panes without being their
        // separator.) Handle both orderings defensively, though `a_side` is
        // always spatially left/top of `b_side` (see `divider_at`), so it
        // should always land in this split's `a` child.
        let (grow_a, found) = if a_has_a_side && b_has_b_side {
            (true, true)
        } else if a_has_b_side && b_has_a_side {
            (false, true)
        } else {
            (false, false)
        };
        if found {
            if split_axis != axis {
                return false;
            }
            let extent = axis_extent(axis, area);
            if extent <= 0.0 {
                return false;
            }
            let min_r = (min_px / extent).min(0.5);
            let signed = if grow_a { delta } else { -delta };
            *ratio = (*ratio + signed / extent).clamp(min_r, 1.0 - min_r);
            return true;
        }

        // Both panes live in the same child: this split doesn't separate
        // them, so descend without touching its ratio.
        let (ra, rb) = split_child_rects(split_axis, *ratio, area);
        if a_has_a_side && a_has_b_side {
            return a.resize_split(a_side, b_side, axis, delta, ra, min_px);
        }
        if b_has_a_side && b_has_b_side {
            return b.resize_split(a_side, b_side, axis, delta, rb, min_px);
        }
        // Neither pane found together in either subtree (malformed input).
        false
    }
}

/// The two child rects of a split, given its axis/ratio and outer area.
pub(crate) fn split_child_rects(axis: Axis, ratio: f64, area: Rect) -> (Rect, Rect) {
    match axis {
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
    }
}

/// A rect's extent along `axis` (width for horizontal splits, height for vertical).
pub(crate) fn axis_extent(axis: Axis, area: Rect) -> f64 {
    match axis {
        Axis::Horizontal => area.width,
        Axis::Vertical => area.height,
    }
}

/// Remove the `target` pane from `node`, promoting its sibling into the removed
/// split. Returns the rebuilt tree (`None` if `target` was the whole tree) and
/// the removed pane's session (`None` if `target` was absent). Consumes `node`.
pub fn remove_pane(node: LayoutNode, target: PaneId) -> (Option<LayoutNode>, Option<SessionId>) {
    match node {
        LayoutNode::Pane {
            id,
            session,
            carried_title,
        } => {
            if id == target {
                (None, Some(session))
            } else {
                (
                    Some(LayoutNode::Pane {
                        id,
                        session,
                        carried_title,
                    }),
                    None,
                )
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

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WindowTree {
    pub tabs: Vec<Tab>,
    pub active: usize,
}

impl WindowTree {
    pub fn active_tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    /// Move the tab at index `from` to index `to`, shifting the tabs in between.
    /// `active` follows the tab that was active before the move (by id), so the
    /// user stays on the same tab. No-op if either index is out of range or equal.
    pub fn move_tab(&mut self, from: usize, to: usize) {
        let n = self.tabs.len();
        if from >= n || to >= n || from == to {
            return;
        }
        let active_id = self.tabs[self.active].id;
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        if let Some(pos) = self.tabs.iter().position(|t| t.id == active_id) {
            self.active = pos;
        }
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
            let (ra, rb) = split_child_rects(*axis, *ratio, area);
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

    fn tree_with_tabs(n: u64) -> WindowTree {
        WindowTree {
            tabs: (1..=n)
                .map(|i| Tab {
                    id: TabId(i),
                    title: format!("t{i}"),
                    root: p(i),
                    focus: PaneId(i),
                })
                .collect(),
            active: 0,
        }
    }

    fn ids(w: &WindowTree) -> Vec<u64> {
        w.tabs.iter().map(|t| t.id.0).collect()
    }

    #[test]
    fn move_tab_last_to_front_active_follows() {
        let mut w = tree_with_tabs(3);
        w.active = 2; // on tab id 3
        w.move_tab(2, 0);
        assert_eq!(ids(&w), vec![3, 1, 2]);
        assert_eq!(w.active, 0); // still on id 3, now at front
    }

    #[test]
    fn move_tab_front_to_back_active_follows() {
        let mut w = tree_with_tabs(3);
        w.active = 1; // on tab id 2
        w.move_tab(0, 2);
        assert_eq!(ids(&w), vec![2, 3, 1]);
        assert_eq!(w.active, 0); // id 2 now at front
    }

    #[test]
    fn move_tab_noop_when_equal_or_out_of_range() {
        let mut w = tree_with_tabs(3);
        let before = w.clone();
        w.move_tab(1, 1);
        w.move_tab(5, 0);
        w.move_tab(0, 9);
        assert_eq!(w, before);
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

    // --- resize_split: divider identified by BOTH flanking panes ----------
    //
    // Regression coverage for the tab-merge divider-resize bug: `resize_pane`
    // identifies a divider by one pane + axis and always adjusts the
    // *deepest* enclosing same-axis split, so in a left-leaning tree
    // `Split(H, Split(H, P1, P2), P3)`, resizing what the user sees as the
    // P2|P3 divider (a-side pane P2) actually moved the inner P1|P2 ratio.
    // `resize_split` instead takes both flanking panes and adjusts the split
    // that is their separating ancestor, so it can't pick the wrong one.

    fn outer_ratio(tree: &LayoutNode) -> f64 {
        match tree {
            LayoutNode::Split { ratio, .. } => *ratio,
            LayoutNode::Pane { .. } => panic!("expected split"),
        }
    }

    fn inner_ratio(tree: &LayoutNode, side: &str) -> f64 {
        match tree {
            LayoutNode::Split { a, b, .. } => match side {
                "a" => outer_ratio(a),
                "b" => outer_ratio(b),
                _ => panic!("side must be \"a\" or \"b\""),
            },
            LayoutNode::Pane { .. } => panic!("expected split"),
        }
    }

    #[test]
    fn resize_split_left_leaning_outer_divider_moves_outer_ratio_only() {
        // Split(H, Split(H, P1, P2), P3) — panes read P1 | P2 | P3.
        let mut tree = LayoutNode::split(
            Axis::Horizontal,
            0.5,
            LayoutNode::split(Axis::Horizontal, 0.5, p(1), p(2)),
            p(3),
        );
        let area = Rect::new(0.0, 0.0, 120.0, 100.0);
        // The P2|P3 divider: a-side P2, b-side P3.
        let moved = tree.resize_split(PaneId(2), PaneId(3), Axis::Horizontal, 10.0, area, 5.0);
        assert!(moved);
        assert_eq!(outer_ratio(&tree), 0.5 + 10.0 / 120.0);
        assert_eq!(inner_ratio(&tree, "a"), 0.5); // P1|P2 (inner) untouched
    }

    #[test]
    fn resize_split_left_leaning_inner_divider_moves_inner_ratio_only() {
        let mut tree = LayoutNode::split(
            Axis::Horizontal,
            0.5,
            LayoutNode::split(Axis::Horizontal, 0.5, p(1), p(2)),
            p(3),
        );
        let area = Rect::new(0.0, 0.0, 120.0, 100.0);
        // The P1|P2 divider: a-side P1, b-side P2. The inner split's rect is
        // the left 60px of the outer area (outer ratio 0.5 of 120).
        let moved = tree.resize_split(PaneId(1), PaneId(2), Axis::Horizontal, 10.0, area, 5.0);
        assert!(moved);
        assert_eq!(outer_ratio(&tree), 0.5); // outer (P2|P3-separating) untouched
        assert_eq!(inner_ratio(&tree, "a"), 0.5 + 10.0 / 60.0);
    }

    #[test]
    fn resize_split_right_leaning_outer_divider_moves_outer_ratio_only() {
        // Split(H, P1, Split(H, P2, P3)) — panes read P1 | P2 | P3.
        let mut tree = LayoutNode::split(
            Axis::Horizontal,
            0.5,
            p(1),
            LayoutNode::split(Axis::Horizontal, 0.5, p(2), p(3)),
        );
        let area = Rect::new(0.0, 0.0, 120.0, 100.0);
        // The P1|P2 divider: a-side P1, b-side P2.
        let moved = tree.resize_split(PaneId(1), PaneId(2), Axis::Horizontal, 10.0, area, 5.0);
        assert!(moved);
        assert_eq!(outer_ratio(&tree), 0.5 + 10.0 / 120.0);
        assert_eq!(inner_ratio(&tree, "b"), 0.5); // P2|P3 (inner) untouched
    }

    #[test]
    fn resize_split_right_leaning_inner_divider_moves_inner_ratio_only() {
        let mut tree = LayoutNode::split(
            Axis::Horizontal,
            0.5,
            p(1),
            LayoutNode::split(Axis::Horizontal, 0.5, p(2), p(3)),
        );
        let area = Rect::new(0.0, 0.0, 120.0, 100.0);
        // The P2|P3 divider: a-side P2, b-side P3. The inner split's rect is
        // the right 60px of the outer area.
        let moved = tree.resize_split(PaneId(2), PaneId(3), Axis::Horizontal, 10.0, area, 5.0);
        assert!(moved);
        assert_eq!(outer_ratio(&tree), 0.5); // outer (P1|P2-separating) untouched
        assert_eq!(inner_ratio(&tree, "b"), 0.5 + 10.0 / 60.0);
    }

    #[test]
    fn resize_split_vertical_nest_moves_correct_ratio() {
        // Split(V, Split(V, P1, P2), P3) — panes read P1 above P2 above P3.
        let mut tree = LayoutNode::split(
            Axis::Vertical,
            0.5,
            LayoutNode::split(Axis::Vertical, 0.5, p(1), p(2)),
            p(3),
        );
        let area = Rect::new(0.0, 0.0, 100.0, 120.0);
        // Outer divider (P2|P3).
        let moved = tree.resize_split(PaneId(2), PaneId(3), Axis::Vertical, 10.0, area, 5.0);
        assert!(moved);
        assert_eq!(outer_ratio(&tree), 0.5 + 10.0 / 120.0);
        assert_eq!(inner_ratio(&tree, "a"), 0.5);

        // Inner divider (P1|P2) on a fresh tree of the same shape.
        let mut tree2 = LayoutNode::split(
            Axis::Vertical,
            0.5,
            LayoutNode::split(Axis::Vertical, 0.5, p(1), p(2)),
            p(3),
        );
        let moved2 = tree2.resize_split(PaneId(1), PaneId(2), Axis::Vertical, 10.0, area, 5.0);
        assert!(moved2);
        assert_eq!(outer_ratio(&tree2), 0.5);
        assert_eq!(inner_ratio(&tree2, "a"), 0.5 + 10.0 / 60.0);
    }

    #[test]
    fn resize_split_mixed_axis_nest_does_not_confuse_axes() {
        // Split(H, P1, Split(V, P2, P3)) — P1 beside a P2-above-P3 stack.
        let mut tree = LayoutNode::split(
            Axis::Horizontal,
            0.5,
            p(1),
            LayoutNode::split(Axis::Vertical, 0.5, p(2), p(3)),
        );
        let area = Rect::new(0.0, 0.0, 100.0, 100.0);
        // Outer P1|P2 divider is a Horizontal-axis divider.
        let moved = tree.resize_split(PaneId(1), PaneId(2), Axis::Horizontal, 10.0, area, 5.0);
        assert!(moved);
        assert_eq!(outer_ratio(&tree), 0.5 + 10.0 / 100.0);
        assert_eq!(inner_ratio(&tree, "b"), 0.5); // inner V-split untouched

        // Inner P2|P3 divider is a Vertical-axis divider, on a fresh tree.
        let mut tree2 = LayoutNode::split(
            Axis::Horizontal,
            0.5,
            p(1),
            LayoutNode::split(Axis::Vertical, 0.5, p(2), p(3)),
        );
        let moved2 = tree2.resize_split(PaneId(2), PaneId(3), Axis::Vertical, 10.0, area, 5.0);
        assert!(moved2);
        assert_eq!(outer_ratio(&tree2), 0.5); // outer H-split untouched
        // Inner split's rect is the right half (50x100) of the outer area.
        assert_eq!(inner_ratio(&tree2, "b"), 0.5 + 10.0 / 100.0);
    }

    #[test]
    fn resize_split_clamps_to_min_px() {
        let mut tree = LayoutNode::split(Axis::Horizontal, 0.5, p(1), p(2));
        let area = Rect::new(0.0, 0.0, 100.0, 100.0);
        // A huge negative delta should clamp a_side (P1) down to min_px, not
        // go negative or push b_side below its own minimum.
        let moved = tree.resize_split(PaneId(1), PaneId(2), Axis::Horizontal, -1000.0, area, 10.0);
        assert!(moved);
        assert_eq!(outer_ratio(&tree), 0.1); // min_px 10 / extent 100

        let mut tree2 = LayoutNode::split(Axis::Horizontal, 0.5, p(1), p(2));
        let moved2 = tree2.resize_split(PaneId(1), PaneId(2), Axis::Horizontal, 1000.0, area, 10.0);
        assert!(moved2);
        assert_eq!(outer_ratio(&tree2), 0.9); // 1.0 - min_r
    }
}
