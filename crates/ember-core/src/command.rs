//! Layout mutations as data (design §5, §8). `LayoutCommand` is a data-only,
//! serde, bus-ready enum; [`apply`] mutates the [`WindowTree`] and *returns*
//! side-effect descriptions ([`LayoutEffect`]) — it performs no IO itself.

use serde::{Deserialize, Serialize};

use crate::focus::{Direction, focus_dir};
use crate::geom::Rect;
use crate::ids::{PaneId, SessionId, TabId};
use crate::layout::{Axis, LayoutNode, Tab, WindowTree, layout, remove_pane};

/// A multiplexer mutation (design §8 variant list). Data-only + serde so it can
/// ride the backend bus later; `#[non_exhaustive]` so adding a variant is a minor
/// change that does not break downstream matches.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum LayoutCommand {
    SplitPane {
        target: PaneId,
        axis: Axis,
        ratio: f64,
        new_pane: PaneId,
        new_session: SessionId,
        /// Smallest extent (px) either resulting pane may have along `axis`; the
        /// app computes it from `min_cells * cell + pad`. The split is REFUSED
        /// (no effect) if the target can't hold two panes this size.
        min_px: f64,
    },
    ClosePane {
        target: PaneId,
    },
    FocusDir {
        dir: Direction,
    },
    NewTab {
        id: TabId,
        session: SessionId,
        pane: PaneId,
    },
    MoveTab {
        from: usize,
        to: usize,
    },
    RenameTab {
        tab: TabId,
        title: String,
    },
    /// Grow `target`'s side of the nearest enclosing split of `axis` by `delta`
    /// physical pixels. Pane-relative — no divider identity. Used by keyboard
    /// resize, where there's only ever one pane (the focused one) to key off.
    ResizePane {
        target: PaneId,
        axis: Axis,
        delta: f64,
        /// Minimum extent (px) either child may shrink to (the clamp).
        min_px: f64,
    },
    /// Grow `a_side`'s side of the split that separates `a_side` and `b_side`
    /// by `delta` physical pixels. Divider-relative: identifies the split by
    /// BOTH panes flanking the divider, so it can't be confused with a
    /// same-axis split elsewhere in the tree (see [`LayoutNode::resize_split`]
    /// for why `ResizePane`'s single-pane targeting is ambiguous). Used by
    /// mouse divider drag, which always knows both flanking panes.
    ///
    /// [`LayoutNode::resize_split`]: crate::layout::LayoutNode::resize_split
    ResizeSplit {
        a_side: PaneId,
        b_side: PaneId,
        axis: Axis,
        delta: f64,
        /// Minimum extent (px) either child may shrink to (the clamp).
        min_px: f64,
    },
    /// Close a whole tab (by id), killing every session it holds. Works on any
    /// tab, active or not — the app doesn't have to loop `ClosePane` or mutate
    /// the tree by hand.
    CloseTab {
        tab: TabId,
    },
}

/// A described side effect of applying a [`LayoutCommand`]. The owner (ember-app)
/// performs the actual IO; the core only emits the intent.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum LayoutEffect {
    /// The pane's session must be terminated.
    KillSession(SessionId),
    /// Focus moved to this pane.
    FocusChanged(PaneId),
}

// NOTE (layout-seam ruling, 2026-07-02): there is deliberately no resize effect.
// Pane geometry is DERIVED state — the app reconciles it in `sync_layout` by
// walking `layout()` and sending `BackendControl::Resize` on a dims change. See
// docs/design/2026-07-02-layout-seam.md.

/// Apply `cmd` to `tree` within `viewport`, returning the side effects to run.
pub fn apply(tree: &mut WindowTree, cmd: LayoutCommand, viewport: Rect) -> Vec<LayoutEffect> {
    let mut effects = Vec::new();
    // The pure core must never panic on a valid command sequence. Every arm but
    // NewTab indexes `tree.active`; an empty tree (last pane just closed) makes
    // that an out-of-bounds panic. A winit app quits on the last pane, but a
    // bus-driven consumer may replay commands — guard structurally instead.
    if tree.tabs.is_empty() {
        if let LayoutCommand::NewTab { id, session, pane } = cmd {
            tree.tabs.push(Tab {
                id,
                title: String::new(),
                root: LayoutNode::pane(pane, session.clone()),
                focus: pane,
            });
            tree.active = 0;
            let _ = &session; // geometry is reconciled by the app, not signaled
            effects.push(LayoutEffect::FocusChanged(pane));
        }
        return effects;
    }
    // A stale `active` (e.g. left dangling by an earlier close) would also panic;
    // clamp it into range before any arm indexes it.
    if tree.active >= tree.tabs.len() {
        tree.active = tree.tabs.len() - 1;
    }
    match cmd {
        LayoutCommand::SplitPane {
            target,
            axis,
            ratio,
            new_pane,
            new_session,
            min_px,
        } => {
            let tab = &mut tree.tabs[tree.active];
            // Refuse a split the target can't fit two min-size panes into (the
            // fix for compounding nested splits shrinking to sub-pixel).
            let extent = layout(&tab.root, viewport)
                .into_iter()
                .find(|(id, _)| *id == target)
                .map(|(_, r)| crate::layout::axis_extent(axis, r));
            let fits = extent.is_some_and(|e| e >= 2.0 * min_px && e > 0.0);
            if !fits {
                return effects;
            }
            let extent = extent.unwrap();
            let min_r = (min_px / extent).min(0.5);
            let ratio = ratio.clamp(min_r, 1.0 - min_r);
            if let Some(existing) = tab.root.session_of(target).cloned() {
                let replacement = LayoutNode::split(
                    axis,
                    ratio,
                    LayoutNode::pane(target, existing),
                    LayoutNode::pane(new_pane, new_session),
                );
                if tab.root.replace_pane(target, replacement).is_ok() {
                    tab.focus = new_pane;
                    effects.push(LayoutEffect::FocusChanged(new_pane));
                }
            }
        }
        LayoutCommand::ClosePane { target } => {
            let active = tree.active;
            let dummy = LayoutNode::pane(PaneId(u64::MAX), SessionId::new(""));
            let root = std::mem::replace(&mut tree.tabs[active].root, dummy);
            let (new_root, removed) = remove_pane(root, target);
            match removed {
                None => {
                    // Target absent: restore the unchanged tree.
                    tree.tabs[active].root = new_root.expect("unchanged root returned");
                }
                Some(sess) => {
                    effects.push(LayoutEffect::KillSession(sess));
                    match new_root {
                        Some(r) => {
                            tree.tabs[active].root = r;
                            if tree.tabs[active].focus == target {
                                if let Some((first, _)) =
                                    tree.tabs[active].root.leaves().into_iter().next()
                                {
                                    tree.tabs[active].focus = first;
                                    effects.push(LayoutEffect::FocusChanged(first));
                                }
                            }
                        }
                        None => {
                            // Closed the last pane in the tab → close the tab.
                            tree.tabs.remove(active);
                            if !tree.tabs.is_empty() {
                                tree.active = active.min(tree.tabs.len() - 1);
                            }
                        }
                    }
                }
            }
        }
        LayoutCommand::FocusDir { dir } => {
            let tab = &mut tree.tabs[tree.active];
            let rects = layout(&tab.root, viewport);
            if let Some(next) = focus_dir(&rects, tab.focus, dir) {
                tab.focus = next;
                effects.push(LayoutEffect::FocusChanged(next));
            }
        }
        LayoutCommand::NewTab { id, session, pane } => {
            tree.tabs.push(Tab {
                id,
                title: String::new(),
                root: LayoutNode::pane(pane, session.clone()),
                focus: pane,
            });
            tree.active = tree.tabs.len() - 1;
            let _ = &session; // geometry reconciled by the app
            effects.push(LayoutEffect::FocusChanged(pane));
        }
        LayoutCommand::MoveTab { from, to } => {
            // Active follows the tab the user is on (see WindowTree::move_tab), so
            // reordering a background tab doesn't yank focus to it.
            tree.move_tab(from, to);
        }
        LayoutCommand::RenameTab { tab, title } => {
            if let Some(t) = tree.tabs.iter_mut().find(|t| t.id == tab) {
                t.title = title;
            }
        }
        LayoutCommand::ResizePane {
            target,
            axis,
            delta,
            min_px,
        } => {
            let tab = &mut tree.tabs[tree.active];
            // Geometry (and thus the backend resize) is reconciled by the app;
            // core only mutates the ratio.
            tab.root.resize_pane(target, axis, delta, viewport, min_px);
        }
        LayoutCommand::ResizeSplit {
            a_side,
            b_side,
            axis,
            delta,
            min_px,
        } => {
            let tab = &mut tree.tabs[tree.active];
            tab.root
                .resize_split(a_side, b_side, axis, delta, viewport, min_px);
        }
        LayoutCommand::CloseTab { tab } => {
            if let Some(idx) = tree.tabs.iter().position(|t| t.id == tab) {
                // Kill every session the tab holds.
                for (_, sess) in tree.tabs[idx].root.leaves() {
                    effects.push(LayoutEffect::KillSession(sess));
                }
                let was_active = idx == tree.active;
                tree.tabs.remove(idx);
                // Keep `active` pointing at the same tab the user was on: closing
                // a tab before it shifts the index down; closing the active tab
                // lands on its neighbor.
                if tree.tabs.is_empty() {
                    // Caller (app) decides whether to quit; nothing to focus.
                } else {
                    if idx < tree.active || tree.active >= tree.tabs.len() {
                        tree.active = tree.active.saturating_sub(1).min(tree.tabs.len() - 1);
                    }
                    if was_active {
                        let focus = tree.tabs[tree.active].focus;
                        effects.push(LayoutEffect::FocusChanged(focus));
                    }
                }
            }
        }
    }
    effects
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vp() -> Rect {
        Rect::new(0.0, 0.0, 100.0, 100.0)
    }

    fn single_tab() -> WindowTree {
        WindowTree {
            tabs: vec![Tab {
                id: TabId(1),
                title: "one".into(),
                root: LayoutNode::pane(PaneId(1), SessionId::new("s1")),
                focus: PaneId(1),
            }],
            active: 0,
        }
    }

    #[test]
    fn split_replaces_leaf_and_focuses_new_pane() {
        let mut tree = single_tab();
        let effects = apply(
            &mut tree,
            LayoutCommand::SplitPane {
                target: PaneId(1),
                axis: Axis::Horizontal,
                ratio: 0.5,
                new_pane: PaneId(2),
                new_session: SessionId::new("s2"),
                min_px: 0.0,
            },
            vp(),
        );
        assert_eq!(
            tree.active_tab().root.pane_ids(),
            vec![PaneId(1), PaneId(2)]
        );
        assert_eq!(tree.active_tab().focus, PaneId(2));
        assert!(effects.contains(&LayoutEffect::FocusChanged(PaneId(2))));
        // Geometry is derived (no resize effect): the tree lays out into halves.
        let rects = layout(&tree.active_tab().root, vp());
        assert_eq!(
            rects,
            vec![
                (PaneId(1), Rect::new(0.0, 0.0, 50.0, 100.0)),
                (PaneId(2), Rect::new(50.0, 0.0, 50.0, 100.0)),
            ]
        );
    }

    #[test]
    fn close_promotes_sibling_and_kills_session() {
        let mut tree = single_tab();
        apply(
            &mut tree,
            LayoutCommand::SplitPane {
                target: PaneId(1),
                axis: Axis::Horizontal,
                ratio: 0.5,
                new_pane: PaneId(2),
                new_session: SessionId::new("s2"),
                min_px: 0.0,
            },
            vp(),
        );
        let effects = apply(
            &mut tree,
            LayoutCommand::ClosePane { target: PaneId(2) },
            vp(),
        );
        assert!(effects.contains(&LayoutEffect::KillSession(SessionId::new("s2"))));
        // Sibling promoted: tree is a lone pane 1 filling the viewport.
        assert_eq!(tree.active_tab().root.pane_ids(), vec![PaneId(1)]);
        // Promoted sibling fills the viewport (geometry derived, not signaled).
        assert_eq!(
            layout(&tree.active_tab().root, vp()),
            vec![(PaneId(1), vp())]
        );
    }

    #[test]
    fn close_focused_pane_moves_focus_to_survivor() {
        let mut tree = single_tab();
        apply(
            &mut tree,
            LayoutCommand::SplitPane {
                target: PaneId(1),
                axis: Axis::Horizontal,
                ratio: 0.5,
                new_pane: PaneId(2),
                new_session: SessionId::new("s2"),
                min_px: 0.0,
            },
            vp(),
        );
        // focus is now pane 2; close it.
        let effects = apply(
            &mut tree,
            LayoutCommand::ClosePane { target: PaneId(2) },
            vp(),
        );
        assert_eq!(tree.active_tab().focus, PaneId(1));
        assert!(effects.contains(&LayoutEffect::FocusChanged(PaneId(1))));
    }

    #[test]
    fn close_last_pane_closes_tab() {
        let mut tree = single_tab();
        // Add a second tab so we can observe the first closing.
        apply(
            &mut tree,
            LayoutCommand::NewTab {
                id: TabId(2),
                session: SessionId::new("s2"),
                pane: PaneId(2),
            },
            vp(),
        );
        assert_eq!(tree.tabs.len(), 2);
        // Close the only pane in tab 2 (currently active).
        let effects = apply(
            &mut tree,
            LayoutCommand::ClosePane { target: PaneId(2) },
            vp(),
        );
        assert!(effects.contains(&LayoutEffect::KillSession(SessionId::new("s2"))));
        assert_eq!(tree.tabs.len(), 1);
        assert_eq!(tree.active, 0);
    }

    #[test]
    fn focus_dir_updates_focus() {
        let mut tree = single_tab();
        apply(
            &mut tree,
            LayoutCommand::SplitPane {
                target: PaneId(1),
                axis: Axis::Horizontal,
                ratio: 0.5,
                new_pane: PaneId(2),
                new_session: SessionId::new("s2"),
                min_px: 0.0,
            },
            vp(),
        );
        // focus is pane 2 (right); move left → pane 1.
        let effects = apply(
            &mut tree,
            LayoutCommand::FocusDir {
                dir: Direction::Left,
            },
            vp(),
        );
        assert_eq!(tree.active_tab().focus, PaneId(1));
        assert_eq!(effects, vec![LayoutEffect::FocusChanged(PaneId(1))]);
    }

    #[test]
    fn resize_pane_grows_targets_side() {
        let mut tree = single_tab();
        apply(
            &mut tree,
            LayoutCommand::SplitPane {
                target: PaneId(1),
                axis: Axis::Horizontal,
                ratio: 0.5,
                new_pane: PaneId(2),
                new_session: SessionId::new("s2"),
                min_px: 0.0,
            },
            vp(),
        );
        // Shrink pane 1 by 25 px (delta is px; negative shrinks the target).
        apply(
            &mut tree,
            LayoutCommand::ResizePane {
                target: PaneId(1),
                axis: Axis::Horizontal,
                delta: -25.0,
                min_px: 0.0,
            },
            vp(),
        );
        let rects = layout(&tree.active_tab().root, vp());
        assert_eq!(rects[0], (PaneId(1), Rect::new(0.0, 0.0, 25.0, 100.0)));
        assert_eq!(rects[1], (PaneId(2), Rect::new(25.0, 0.0, 75.0, 100.0)));
    }

    #[test]
    fn resize_pane_reaches_enclosing_split_from_nested_leaf() {
        // Layout: H-split [ p1 | V-split[ p2 / p3 ] ]. Resizing p3 along the
        // HORIZONTAL axis must move the OUTER divider (its nearest enclosing
        // horizontal split), which set_split_ratio could never reach.
        let mut tree = single_tab();
        apply(
            &mut tree,
            LayoutCommand::SplitPane {
                target: PaneId(1),
                axis: Axis::Horizontal,
                ratio: 0.5,
                new_pane: PaneId(2),
                new_session: SessionId::new("s2"),
                min_px: 0.0,
            },
            vp(),
        );
        apply(
            &mut tree,
            LayoutCommand::SplitPane {
                target: PaneId(2),
                axis: Axis::Vertical,
                ratio: 0.5,
                new_pane: PaneId(3),
                new_session: SessionId::new("s3"),
                min_px: 0.0,
            },
            vp(),
        );
        let before = layout(&tree.active_tab().root, vp());
        apply(
            &mut tree,
            LayoutCommand::ResizePane {
                target: PaneId(3),
                axis: Axis::Horizontal,
                delta: 0.1,
                min_px: 0.0,
            },
            vp(),
        );
        let after = layout(&tree.active_tab().root, vp());
        // p3 is in the outer split's `b` subtree, so growing p3 shrinks p1
        // (proof the op reached the OUTER divider from a doubly-nested leaf).
        let w = |rs: &[(PaneId, Rect)], id| rs.iter().find(|(p, _)| *p == id).unwrap().1.width;
        assert!(
            w(&after, PaneId(1)) < w(&before, PaneId(1)),
            "p3 grew → p1 should shrink: {after:?}"
        );
    }

    #[test]
    fn resize_split_targets_the_separating_divider_not_the_deepest_match() {
        // The left-leaning tree from the tab-merge bug: H-split[ H-split[p1,p2],
        // p3 ] — panes read p1 | p2 | p3. This shape isn't reachable through a
        // sequence of `SplitPane`s (which always grows a leaf into a 2-pane
        // split, never wraps an existing subtree), but IS what a tab-merge
        // drop builds — so construct it directly, as tab-merge does.
        // `ResizePane { target: p2, .. }` (the old, ambiguous, single-pane
        // resolution) always hits the INNER p1|p2 split because it's deeper;
        // a user dragging the p2|p3 divider would move the wrong one.
        // `ResizeSplit` with both flanking panes must hit the OUTER p2|p3
        // divider instead.
        let mut tree = single_tab();
        tree.tabs[0].root = LayoutNode::split(
            Axis::Horizontal,
            0.5,
            LayoutNode::split(
                Axis::Horizontal,
                0.5,
                LayoutNode::pane(PaneId(1), SessionId::new("s1")),
                LayoutNode::pane(PaneId(2), SessionId::new("s2")),
            ),
            LayoutNode::pane(PaneId(3), SessionId::new("s3")),
        );
        apply(
            &mut tree,
            LayoutCommand::ResizeSplit {
                a_side: PaneId(2),
                b_side: PaneId(3),
                axis: Axis::Horizontal,
                delta: 10.0,
                min_px: 0.0,
            },
            vp(),
        );
        let (outer_ratio, inner_ratio) = match &tree.active_tab().root {
            LayoutNode::Split {
                ratio: outer, a, ..
            } => match a.as_ref() {
                LayoutNode::Split { ratio: inner, .. } => (*outer, *inner),
                _ => panic!("expected nested split"),
            },
            _ => panic!("expected split"),
        };
        // The OUTER (p2|p3-separating) ratio moved; the INNER p1|p2 ratio
        // (the bug's wrong target) is untouched.
        assert_eq!(outer_ratio, 0.5 + 10.0 / 100.0);
        assert_eq!(inner_ratio, 0.5);
        // Sanity via rendered rects: p3 (outside the divider's group) shrank;
        // p1 and p2 (both inside the group the divider grew) grew together,
        // in lockstep — that's the inner ratio staying fixed while the
        // group's total share of the window increased.
        let before = layout(
            &LayoutNode::split(
                Axis::Horizontal,
                0.5,
                LayoutNode::split(
                    Axis::Horizontal,
                    0.5,
                    LayoutNode::pane(PaneId(1), SessionId::new("s1")),
                    LayoutNode::pane(PaneId(2), SessionId::new("s2")),
                ),
                LayoutNode::pane(PaneId(3), SessionId::new("s3")),
            ),
            vp(),
        );
        let after = layout(&tree.active_tab().root, vp());
        let w = |rs: &[(PaneId, Rect)], id| rs.iter().find(|(p, _)| *p == id).unwrap().1.width;
        assert!(w(&after, PaneId(1)) > w(&before, PaneId(1)));
        assert!(w(&after, PaneId(2)) > w(&before, PaneId(2)));
        assert!(w(&after, PaneId(3)) < w(&before, PaneId(3)));
    }

    #[test]
    fn split_refused_when_pane_too_small_for_two_minimums() {
        // vp is 100x100; a min of 60px can't fit two panes across a 100px pane.
        let mut tree = single_tab();
        let effects = apply(
            &mut tree,
            LayoutCommand::SplitPane {
                target: PaneId(1),
                axis: Axis::Horizontal,
                ratio: 0.5,
                new_pane: PaneId(2),
                new_session: SessionId::new("s2"),
                min_px: 60.0,
            },
            vp(),
        );
        assert!(effects.is_empty(), "split should be refused");
        assert_eq!(tree.active_tab().root.pane_ids(), vec![PaneId(1)]);
    }

    #[test]
    fn rename_and_move_tab() {
        let mut tree = single_tab();
        apply(
            &mut tree,
            LayoutCommand::NewTab {
                id: TabId(2),
                session: SessionId::new("s2"),
                pane: PaneId(2),
            },
            vp(),
        );
        apply(
            &mut tree,
            LayoutCommand::RenameTab {
                tab: TabId(2),
                title: "renamed".into(),
            },
            vp(),
        );
        assert_eq!(tree.tabs[1].title, "renamed");
        // Move tab 2 (index 1) to the front (index 0).
        apply(&mut tree, LayoutCommand::MoveTab { from: 1, to: 0 }, vp());
        assert_eq!(tree.tabs[0].id, TabId(2));
        assert_eq!(tree.active, 0);
    }

    #[test]
    fn empty_tree_does_not_panic_and_only_newtab_takes_effect() {
        let mut tree = WindowTree {
            tabs: Vec::new(),
            active: 3, // deliberately stale/out-of-range
        };
        // Non-NewTab commands are no-ops on an empty tree (no panic).
        let e = apply(
            &mut tree,
            LayoutCommand::FocusDir {
                dir: Direction::Left,
            },
            vp(),
        );
        assert!(e.is_empty());
        assert!(tree.tabs.is_empty());
        let e = apply(
            &mut tree,
            LayoutCommand::ClosePane { target: PaneId(9) },
            vp(),
        );
        assert!(e.is_empty());
        // NewTab rebuilds a valid tree.
        apply(
            &mut tree,
            LayoutCommand::NewTab {
                id: TabId(1),
                session: SessionId::new("s1"),
                pane: PaneId(1),
            },
            vp(),
        );
        assert_eq!(tree.tabs.len(), 1);
        assert_eq!(tree.active, 0);
    }

    #[test]
    fn closing_last_pane_then_next_command_is_safe() {
        // Reproduces the original panic: ClosePane empties the tree, leaving
        // `active` stale; the NEXT command used to index out of bounds.
        let mut tree = single_tab();
        apply(
            &mut tree,
            LayoutCommand::ClosePane { target: PaneId(1) },
            vp(),
        );
        assert!(tree.tabs.is_empty());
        // Any follow-up command must not panic.
        let e = apply(
            &mut tree,
            LayoutCommand::SplitPane {
                target: PaneId(1),
                axis: Axis::Vertical,
                ratio: 0.5,
                new_pane: PaneId(2),
                new_session: SessionId::new("s2"),
                min_px: 0.0,
            },
            vp(),
        );
        assert!(e.is_empty());
    }

    #[test]
    fn close_tab_kills_all_its_sessions_and_keeps_active_stable() {
        let mut tree = single_tab();
        // Add a 2nd tab, then split it so it has two sessions.
        apply(
            &mut tree,
            LayoutCommand::NewTab {
                id: TabId(2),
                session: SessionId::new("s2"),
                pane: PaneId(2),
            },
            vp(),
        );
        apply(
            &mut tree,
            LayoutCommand::SplitPane {
                target: PaneId(2),
                axis: Axis::Vertical,
                ratio: 0.5,
                new_pane: PaneId(3),
                new_session: SessionId::new("s3"),
                min_px: 0.0,
            },
            vp(),
        );
        // Focus is on tab 2 (index 1); close the BACKGROUND tab 1.
        assert_eq!(tree.active, 1);
        let e = apply(&mut tree, LayoutCommand::CloseTab { tab: TabId(1) }, vp());
        let kills: Vec<_> = e
            .iter()
            .filter_map(|f| match f {
                LayoutEffect::KillSession(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(kills, vec![SessionId::new("s1")]);
        assert_eq!(tree.tabs.len(), 1);
        // Still on the same (formerly index 1, now 0) tab — no teleport.
        assert_eq!(tree.tabs[tree.active].id, TabId(2));
    }

    #[test]
    fn close_active_tab_with_multiple_panes_kills_both() {
        let mut tree = single_tab();
        apply(
            &mut tree,
            LayoutCommand::SplitPane {
                target: PaneId(1),
                axis: Axis::Horizontal,
                ratio: 0.5,
                new_pane: PaneId(2),
                new_session: SessionId::new("s2"),
                min_px: 0.0,
            },
            vp(),
        );
        let e = apply(&mut tree, LayoutCommand::CloseTab { tab: TabId(1) }, vp());
        let kills: std::collections::HashSet<_> = e
            .iter()
            .filter_map(|f| match f {
                LayoutEffect::KillSession(s) => Some(s.0.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            kills,
            ["s1".to_string(), "s2".to_string()].into_iter().collect()
        );
        assert!(tree.tabs.is_empty());
    }

    #[test]
    fn command_roundtrips_through_serde() {
        let cmd = LayoutCommand::SplitPane {
            target: PaneId(7),
            axis: Axis::Vertical,
            ratio: 0.3,
            min_px: 0.0,
            new_pane: PaneId(8),
            new_session: SessionId::new("s8"),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: LayoutCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
    }
}
