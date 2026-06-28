//! Layout mutations as data (design §5, §8). `LayoutCommand` is a data-only,
//! serde, bus-ready enum; [`apply`] mutates the [`WindowTree`] and *returns*
//! side-effect descriptions ([`LayoutEffect`]) — it performs no IO itself.

use std::collections::HashMap;

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
    ResizeSplit {
        target: PaneId,
        ratio: f64,
    },
}

/// A described side effect of applying a [`LayoutCommand`]. The owner (ember-app)
/// performs the actual IO; the core only emits the intent.
#[derive(Clone, Debug, PartialEq)]
pub enum LayoutEffect {
    /// The pane's session must be terminated.
    KillSession(SessionId),
    /// The session's backend must be resized to this rect.
    ResizeBackend(SessionId, Rect),
    /// Focus moved to this pane.
    FocusChanged(PaneId),
}

/// Re-layout `tab` and emit a `ResizeBackend` for every pane in it. Over-emitting
/// is harmless — a resize is idempotent — and keeps the affected-set logic simple.
fn resize_all(tab: &Tab, viewport: Rect) -> Vec<LayoutEffect> {
    let sessions: HashMap<PaneId, SessionId> = tab.root.leaves().into_iter().collect();
    layout(&tab.root, viewport)
        .into_iter()
        .filter_map(|(id, rect)| {
            sessions
                .get(&id)
                .map(|s| LayoutEffect::ResizeBackend(s.clone(), rect))
        })
        .collect()
}

/// Apply `cmd` to `tree` within `viewport`, returning the side effects to run.
pub fn apply(tree: &mut WindowTree, cmd: LayoutCommand, viewport: Rect) -> Vec<LayoutEffect> {
    let mut effects = Vec::new();
    match cmd {
        LayoutCommand::SplitPane {
            target,
            axis,
            ratio,
            new_pane,
            new_session,
        } => {
            let tab = &mut tree.tabs[tree.active];
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
                    effects.extend(resize_all(tab, viewport));
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
                            effects.extend(resize_all(&tree.tabs[active], viewport));
                        }
                        None => {
                            // Closed the last pane in the tab → close the tab.
                            tree.tabs.remove(active);
                            if !tree.tabs.is_empty() {
                                tree.active = active.min(tree.tabs.len() - 1);
                                effects.extend(resize_all(&tree.tabs[tree.active], viewport));
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
            effects.push(LayoutEffect::ResizeBackend(session, viewport));
            effects.push(LayoutEffect::FocusChanged(pane));
        }
        LayoutCommand::MoveTab { from, to } => {
            if from < tree.tabs.len() && to < tree.tabs.len() && from != to {
                let tab = tree.tabs.remove(from);
                tree.tabs.insert(to, tab);
                tree.active = to;
            }
        }
        LayoutCommand::RenameTab { tab, title } => {
            if let Some(t) = tree.tabs.iter_mut().find(|t| t.id == tab) {
                t.title = title;
            }
        }
        LayoutCommand::ResizeSplit { target, ratio } => {
            let tab = &mut tree.tabs[tree.active];
            if tab.root.set_split_ratio(target, ratio) {
                effects.extend(resize_all(tab, viewport));
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
            },
            vp(),
        );
        assert_eq!(
            tree.active_tab().root.pane_ids(),
            vec![PaneId(1), PaneId(2)]
        );
        assert_eq!(tree.active_tab().focus, PaneId(2));
        assert!(effects.contains(&LayoutEffect::FocusChanged(PaneId(2))));
        // Both panes resized to their halves.
        assert!(effects.contains(&LayoutEffect::ResizeBackend(
            SessionId::new("s1"),
            Rect::new(0.0, 0.0, 50.0, 100.0)
        )));
        assert!(effects.contains(&LayoutEffect::ResizeBackend(
            SessionId::new("s2"),
            Rect::new(50.0, 0.0, 50.0, 100.0)
        )));
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
        assert!(effects.contains(&LayoutEffect::ResizeBackend(SessionId::new("s1"), vp())));
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
    fn resize_split_changes_ratio() {
        let mut tree = single_tab();
        apply(
            &mut tree,
            LayoutCommand::SplitPane {
                target: PaneId(1),
                axis: Axis::Horizontal,
                ratio: 0.5,
                new_pane: PaneId(2),
                new_session: SessionId::new("s2"),
            },
            vp(),
        );
        let effects = apply(
            &mut tree,
            LayoutCommand::ResizeSplit {
                target: PaneId(1),
                ratio: 0.25,
            },
            vp(),
        );
        // Pane 1 now occupies the left 25%.
        assert!(effects.contains(&LayoutEffect::ResizeBackend(
            SessionId::new("s1"),
            Rect::new(0.0, 0.0, 25.0, 100.0)
        )));
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
    fn command_roundtrips_through_serde() {
        let cmd = LayoutCommand::SplitPane {
            target: PaneId(7),
            axis: Axis::Vertical,
            ratio: 0.3,
            new_pane: PaneId(8),
            new_session: SessionId::new("s8"),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: LayoutCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
    }
}
