//! Multi-window model (design §5 extension): a list of per-window tab trees,
//! plus [`move_surface`] — the ONE pure function every UI gesture (menu
//! command, keybinding, future drag-drop) lowers onto to relocate a pane or a
//! whole tab across the window set. Pure `ember-core`: no renderer/app types,
//! no IO. Callers (ember-app) perform the [`MoveEffect`]s this returns.

use crate::ids::{PaneId, SessionId, TabId};
use crate::layout::{Axis, LayoutNode, Tab, WindowTree, remove_pane};

/// All windows' tab trees + which window has focus. Index = window number
/// (stable order, 0-based internally; the UI/ctl show 1-based).
#[derive(Clone, Debug, PartialEq)]
pub struct Windows {
    pub trees: Vec<WindowTree>,
    pub focused: usize,
}

/// A movable surface: one leaf pane, or a whole tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceRef {
    Pane {
        window: usize,
        tab: usize,
        pane: PaneId,
    },
    Tab {
        window: usize,
        tab: usize,
    },
}

/// Where a [`SurfaceRef`] goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceDest {
    /// Append as a new tab of an existing window (becomes active there).
    NewTab { window: usize },
    /// A brand-new window whose only tab is the moved surface.
    NewWindow,
    /// Split into an existing pane's cell (the moved surface becomes a
    /// sibling of `pane` along `axis`, taking half its space).
    SplitInto {
        window: usize,
        tab: usize,
        pane: PaneId,
        axis: Axis,
        /// `true`: the moved surface becomes the left/top sibling (tree
        /// child `a`). `false`: right/bottom (tree child `b`).
        before: bool,
    },
}

/// What the app layer must do after the tree edit.
#[derive(Debug, PartialEq)]
pub enum MoveEffect {
    /// `trees[index]` is new: create an OS window + renderer for it, then
    /// replay every listed session's grid+styles into that renderer.
    WindowOpened { index: usize },
    /// The source window ran out of tabs and was removed from `trees`;
    /// close its OS window. Indices in `trees` above it shifted down by 1.
    WindowClosed { index: usize },
    /// These sessions now live in a different window: retarget their PTY
    /// delta routing and replay grid+styles into the destination renderer.
    SessionsRehomed {
        sessions: Vec<SessionId>,
        to_window: usize,
    },
}

/// Why a [`move_surface`] call was refused. The tree is left untouched.
#[derive(Debug, PartialEq)]
pub enum MoveError {
    /// src/dest indices out of range, or dest names the src's own position.
    Invalid(&'static str),
    /// Moving a tab's last pane as a `Pane` ref: use `Tab` instead (the tab
    /// would become empty). Kept an error so callers surface intent.
    WouldEmptyTab,
}

/// Move `src` to `dest` within `windows`, returning the effects the app layer
/// must carry out. On error, `windows` is left completely unchanged.
///
/// `fresh_tab_id` is used ONLY when the move mints a brand-new tab — that is,
/// when a `Pane` is promoted via `SurfaceDest::NewTab` or `SurfaceDest::NewWindow`.
/// It is ignored for every other case (Tab-sourced moves carry their existing
/// `Tab`, id included, wholesale; splits don't create a tab at all). Callers
/// (ember-app) must allocate this from their own tab-id counter so it can
/// never collide with an existing `TabId`.
pub fn move_surface(
    windows: &mut Windows,
    src: SurfaceRef,
    dest: SurfaceDest,
    fresh_tab_id: TabId,
) -> Result<Vec<MoveEffect>, MoveError> {
    validate(windows, src, dest)?;
    match src {
        SurfaceRef::Tab { window, tab } => move_tab(windows, window, tab, dest),
        SurfaceRef::Pane { window, tab, pane } => {
            move_pane(windows, window, tab, pane, dest, fresh_tab_id)
        }
    }
}

/// Bounds- and no-op-check `src`/`dest` against the current `windows`, before
/// any mutation happens. Every early return here must fire with `windows`
/// still untouched — callers rely on "Err means nothing moved".
fn validate(windows: &Windows, src: SurfaceRef, dest: SurfaceDest) -> Result<(), MoveError> {
    let (sw, st) = match src {
        SurfaceRef::Pane { window, tab, .. } | SurfaceRef::Tab { window, tab } => (window, tab),
    };
    if sw >= windows.trees.len() {
        return Err(MoveError::Invalid("source window out of range"));
    }
    if st >= windows.trees[sw].tabs.len() {
        return Err(MoveError::Invalid("source tab out of range"));
    }
    if let SurfaceRef::Pane { pane, .. } = src {
        if !windows.trees[sw].tabs[st].root.contains(pane) {
            return Err(MoveError::Invalid("source pane not found"));
        }
    }
    match dest {
        SurfaceDest::NewTab { window } => {
            if window >= windows.trees.len() {
                return Err(MoveError::Invalid("dest window out of range"));
            }
        }
        SurfaceDest::NewWindow => {}
        SurfaceDest::SplitInto {
            window, tab, pane, ..
        } => {
            if window >= windows.trees.len() {
                return Err(MoveError::Invalid("dest window out of range"));
            }
            if tab >= windows.trees[window].tabs.len() {
                return Err(MoveError::Invalid("dest tab out of range"));
            }
            if !windows.trees[window].tabs[tab].root.contains(pane) {
                return Err(MoveError::Invalid("dest pane not found"));
            }
        }
    }
    match (src, dest) {
        (
            SurfaceRef::Pane { window, tab, pane },
            SurfaceDest::SplitInto {
                window: dw,
                tab: dt,
                pane: dp,
                ..
            },
        ) if window == dw && tab == dt && pane == dp => {
            Err(MoveError::Invalid("no-op: split into self"))
        }
        (SurfaceRef::Tab { window, .. }, SurfaceDest::NewTab { window: dw }) if window == dw => {
            Err(MoveError::Invalid("no-op: tab already in that window"))
        }
        (
            SurfaceRef::Tab { window, tab },
            SurfaceDest::SplitInto {
                window: dw,
                tab: dt,
                ..
            },
        ) if window == dw && tab == dt => {
            Err(MoveError::Invalid("no-op: tab can't merge into itself"))
        }
        _ => Ok(()),
    }
}

/// Clamp `win.active` after removing the tab that was at `removed_idx`,
/// mirroring `LayoutCommand::CloseTab`'s rule: shift down if the removal was
/// at-or-before `active`, then clamp into range. No-op if `win` is now empty
/// (the caller removes empty windows separately).
fn clamp_active(win: &mut WindowTree, removed_idx: usize) {
    if win.tabs.is_empty() {
        return;
    }
    if removed_idx < win.active || win.active >= win.tabs.len() {
        win.active = win.active.saturating_sub(1).min(win.tabs.len() - 1);
    }
}

/// If window `w` lost its last tab, remove it from `trees`, emit
/// `WindowClosed`, and renumber every window index at-or-above `w` already
/// recorded in `effects` or `windows.focused` (removal shifts everything
/// above `w` down by one).
fn close_source_if_empty(windows: &mut Windows, w: usize, effects: &mut Vec<MoveEffect>) {
    if !windows.trees[w].tabs.is_empty() {
        return;
    }
    windows.trees.remove(w);
    for eff in effects.iter_mut() {
        match eff {
            MoveEffect::WindowOpened { index } if *index > w => *index -= 1,
            MoveEffect::SessionsRehomed { to_window, .. } if *to_window > w => *to_window -= 1,
            _ => {}
        }
    }
    effects.push(MoveEffect::WindowClosed { index: w });
    if w < windows.focused || windows.focused >= windows.trees.len() {
        windows.focused = windows
            .focused
            .saturating_sub(1)
            .min(windows.trees.len().saturating_sub(1));
    }
}

/// Split `node` into the tree with `pane` removed (sibling promoted, per
/// [`remove_pane`]) and the pane's own leaf. `None` if `pane` is absent, or if
/// `node` is nothing but that one leaf — nothing would remain, so callers use
/// this to detect "would empty the tab".
fn extract_leaf(node: LayoutNode, pane: PaneId) -> Option<(LayoutNode, LayoutNode)> {
    match remove_pane(node, pane) {
        (Some(remaining), Some(session)) => Some((remaining, LayoutNode::pane(pane, session))),
        _ => None,
    }
}

fn move_tab(
    windows: &mut Windows,
    w: usize,
    t: usize,
    dest: SurfaceDest,
) -> Result<Vec<MoveEffect>, MoveError> {
    match dest {
        SurfaceDest::NewTab { window: dw } => {
            let moved = windows.trees[w].tabs.remove(t);
            clamp_active(&mut windows.trees[w], t);
            let sessions = moved.root.leaves().into_iter().map(|(_, s)| s).collect();
            windows.trees[dw].tabs.push(moved);
            windows.trees[dw].active = windows.trees[dw].tabs.len() - 1;
            let mut effects = vec![MoveEffect::SessionsRehomed {
                sessions,
                to_window: dw,
            }];
            close_source_if_empty(windows, w, &mut effects);
            Ok(effects)
        }
        SurfaceDest::NewWindow => {
            let moved = windows.trees[w].tabs.remove(t);
            clamp_active(&mut windows.trees[w], t);
            let sessions: Vec<_> = moved.root.leaves().into_iter().map(|(_, s)| s).collect();
            let new_index = windows.trees.len();
            windows.trees.push(WindowTree {
                tabs: vec![moved],
                active: 0,
            });
            let mut effects = vec![
                MoveEffect::WindowOpened { index: new_index },
                MoveEffect::SessionsRehomed {
                    sessions,
                    to_window: new_index,
                },
            ];
            windows.focused = new_index;
            close_source_if_empty(windows, w, &mut effects);
            Ok(effects)
        }
        SurfaceDest::SplitInto {
            window: dw,
            tab: dt,
            pane: dp,
            axis,
            before,
        } => {
            let moved = windows.trees[w].tabs.remove(t);
            clamp_active(&mut windows.trees[w], t);
            // Removing tab `t` shifted every later tab in the SAME window's
            // vec down by one; only relevant when the merge target lives in
            // the source window too (dt != t is guaranteed by the no-op check).
            let dt_eff = if dw == w && dt > t { dt - 1 } else { dt };
            let sessions: Vec<_> = moved.root.leaves().into_iter().map(|(_, s)| s).collect();
            let dest_tab = &mut windows.trees[dw].tabs[dt_eff];
            let existing_session = dest_tab
                .root
                .session_of(dp)
                .cloned()
                .expect("dest pane validated");
            let existing = LayoutNode::pane(dp, existing_session);
            let merged = if before {
                LayoutNode::split(axis, 0.5, moved.root, existing)
            } else {
                LayoutNode::split(axis, 0.5, existing, moved.root)
            };
            dest_tab
                .root
                .replace_pane(dp, merged)
                .expect("dest pane validated");
            dest_tab.focus = moved.focus;
            let mut effects = Vec::new();
            if dw != w {
                effects.push(MoveEffect::SessionsRehomed {
                    sessions,
                    to_window: dw,
                });
            }
            close_source_if_empty(windows, w, &mut effects);
            Ok(effects)
        }
    }
}

fn move_pane(
    windows: &mut Windows,
    w: usize,
    t: usize,
    p: PaneId,
    dest: SurfaceDest,
    fresh_tab_id: TabId,
) -> Result<Vec<MoveEffect>, MoveError> {
    if windows.trees[w].tabs[t].root.pane_ids().len() == 1 {
        return Err(MoveError::WouldEmptyTab);
    }
    // Extract the leaf: sibling absorbs its space (extract_leaf/remove_pane),
    // then re-home the tab's focus if it was pointing at the moved pane.
    let dummy = LayoutNode::pane(PaneId(u64::MAX), SessionId::new(""));
    let root = std::mem::replace(&mut windows.trees[w].tabs[t].root, dummy);
    let (remaining, leaf) =
        extract_leaf(root, p).expect("pane existence and non-singleton validated above");
    windows.trees[w].tabs[t].root = remaining;
    if windows.trees[w].tabs[t].focus == p {
        let (first, _) = windows.trees[w].tabs[t].root.leaves()[0];
        windows.trees[w].tabs[t].focus = first;
    }
    let session = leaf.leaves()[0].1.clone();

    match dest {
        SurfaceDest::NewTab { window: dw } => {
            let new_tab = Tab {
                id: fresh_tab_id,
                title: String::new(),
                root: leaf,
                focus: p,
            };
            windows.trees[dw].tabs.push(new_tab);
            windows.trees[dw].active = windows.trees[dw].tabs.len() - 1;
            let mut effects = Vec::new();
            if dw != w {
                effects.push(MoveEffect::SessionsRehomed {
                    sessions: vec![session],
                    to_window: dw,
                });
            }
            Ok(effects)
        }
        SurfaceDest::NewWindow => {
            let new_tab = Tab {
                id: fresh_tab_id,
                title: String::new(),
                root: leaf,
                focus: p,
            };
            let new_index = windows.trees.len();
            windows.trees.push(WindowTree {
                tabs: vec![new_tab],
                active: 0,
            });
            windows.focused = new_index;
            Ok(vec![
                MoveEffect::WindowOpened { index: new_index },
                MoveEffect::SessionsRehomed {
                    sessions: vec![session],
                    to_window: new_index,
                },
            ])
        }
        SurfaceDest::SplitInto {
            window: dw,
            tab: dt,
            pane: dp,
            axis,
            before,
        } => {
            let dest_tab = &mut windows.trees[dw].tabs[dt];
            let existing_session = dest_tab
                .root
                .session_of(dp)
                .cloned()
                .expect("dest pane validated");
            let existing = LayoutNode::pane(dp, existing_session);
            let merged = if before {
                LayoutNode::split(axis, 0.5, leaf, existing)
            } else {
                LayoutNode::split(axis, 0.5, existing, leaf)
            };
            dest_tab
                .root
                .replace_pane(dp, merged)
                .expect("dest pane validated");
            dest_tab.focus = p;
            let mut effects = Vec::new();
            if dw != w {
                effects.push(MoveEffect::SessionsRehomed {
                    sessions: vec![session],
                    to_window: dw,
                });
            }
            Ok(effects)
        }
    }
}

// ---------------------------------------------------------------------------
// Drop-zone geometry (release 2): pure hit-testing for surface drags. Every
// drag hover in the UI lowers onto one of these two pure functions to decide
// "where would this land" — inside a pane ([`drop_zone_for`]) or which
// on-screen window frame a screen point is over ([`window_under`]). Callers
// (ember-app) translate the result into a [`SurfaceDest`] for [`move_surface`].
// ---------------------------------------------------------------------------

/// Where a carried surface would land inside a pane, by pointer position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DropZone {
    /// Outer edge band: split along the axis, new surface on the named side.
    Edge { axis: Axis, before: bool },
    /// Inner region: append as a new tab of that window.
    Center,
}

/// Classify a point (pane-local coordinates) against a pane of size `w` x `h`.
///
/// The edge band is the outer 30% on each side, one band per side (West,
/// East, North, South); the remaining middle 40% x 40% square is `Center`
/// (the 30%/70% boundary lines themselves belong to `Center`, not the band).
///
/// Axis mapping — verified against [`Axis`]'s own doc comment (`Horizontal` =
/// side-by-side panes, divider runs vertically; `Vertical` = stacked panes,
/// divider runs horizontally) and against how `ember-app`'s pane-resize
/// keybindings pair Left/Right with `Axis::Horizontal` and Up/Down with
/// `Axis::Vertical` (`window_state.rs`'s `Cmd+Ctrl+Arrows` handler):
///   - **West** band (left 30% of `w`) -> `Edge { axis: Horizontal, before: true }`:
///     the moved surface becomes the LEFT sibling of a side-by-side split.
///   - **East** band (right 30% of `w`) -> `Edge { axis: Horizontal, before: false }`:
///     the moved surface becomes the RIGHT sibling of a side-by-side split.
///   - **North** band (top 30% of `h`) -> `Edge { axis: Vertical, before: true }`:
///     the moved surface becomes the TOP sibling of a stacked split.
///   - **South** band (bottom 30% of `h`) -> `Edge { axis: Vertical, before: false }`:
///     the moved surface becomes the BOTTOM sibling of a stacked split.
///
/// A point can fall in two bands at once (a corner): the NEARER edge wins,
/// i.e. the smaller of the two edge distances. An exact tie resolves
/// horizontal-first (West/East beats North/South).
///
/// Degenerate panes (`w` or `h` zero, negative, or NaN) resolve to `Center`:
/// the band math (`x < w * BAND`, etc.) produces geometrically meaningless
/// results on such dimensions — e.g. a 0-width pane still satisfies `x <
/// w * BAND` for negative `x` — so this is guarded explicitly rather than
/// left to fall out of the general case.
/// [`drop_zone_for`]'s sibling for SAME-WINDOW drags, where `Center` ("add as
/// a tab of this window") is meaningless — the surface already lives here, so
/// a center drop would be a guaranteed no-op. Instead the WHOLE pane is split
/// territory: the nearest edge wins everywhere (half-pane semantics, matching
/// what dragging a tab onto a pane meant before cross-window drops existed).
/// Degenerate dimensions still resolve `Center` (callers treat it as "no
/// zone" and the drop cancels harmlessly).
pub fn split_zone_for(x: f64, y: f64, w: f64, h: f64) -> DropZone {
    if w <= 0.0 || w.is_nan() || h <= 0.0 || h.is_nan() {
        return DropZone::Center;
    }
    // Distances to each edge, normalized by the pane's own dimension so a
    // wide-flat pane doesn't bias toward its long sides. Ties horizontal-
    // first, mirroring drop_zone_for's corner rule.
    let candidates = [
        (x / w, Axis::Horizontal, true),        // west
        ((w - x) / w, Axis::Horizontal, false), // east
        (y / h, Axis::Vertical, true),          // north
        ((h - y) / h, Axis::Vertical, false),   // south
    ];
    let (_, axis, before) = candidates
        .into_iter()
        .min_by(|a, b| a.0.partial_cmp(&b.0).expect("finite distances"))
        .expect("non-empty");
    DropZone::Edge { axis, before }
}

pub fn drop_zone_for(x: f64, y: f64, w: f64, h: f64) -> DropZone {
    if w <= 0.0 || w.is_nan() || h <= 0.0 || h.is_nan() {
        return DropZone::Center;
    }
    // Started at 0.3, which made ~84% of a pane's area mean "split" and
    // turned casual tab drops into rogue splits (first live session).
    // 0.15 flips the emphasis: the middle 70%x70% (~half the pane) reads
    // "add as tab" — the forgiving default — and splitting is a deliberate
    // aim at an edge.
    const BAND: f64 = 0.15;
    let west = x < w * BAND;
    let east = x > w * (1.0 - BAND);
    let north = y < h * BAND;
    let south = y > h * (1.0 - BAND);

    // Order matters: ties resolve to the first-inserted candidate, so
    // West/East (horizontal) are pushed before North/South (vertical) to
    // give the required horizontal-first tie-break.
    let mut candidates: Vec<(f64, DropZone)> = Vec::with_capacity(4);
    if west {
        candidates.push((
            x,
            DropZone::Edge {
                axis: Axis::Horizontal,
                before: true,
            },
        ));
    }
    if east {
        candidates.push((
            w - x,
            DropZone::Edge {
                axis: Axis::Horizontal,
                before: false,
            },
        ));
    }
    if north {
        candidates.push((
            y,
            DropZone::Edge {
                axis: Axis::Vertical,
                before: true,
            },
        ));
    }
    if south {
        candidates.push((
            h - y,
            DropZone::Edge {
                axis: Axis::Vertical,
                before: false,
            },
        ));
    }

    candidates
        .into_iter()
        .min_by(|a, b| a.0.partial_cmp(&b.0).expect("finite distances"))
        .map(|(_, zone)| zone)
        .unwrap_or(DropZone::Center)
}

/// Topmost window frame containing a screen point. `frames` is ordered
/// front-to-back (the caller passes focus order) as `(x, y, width, height)`
/// tuples; returns the index into `frames` of the first (frontmost) hit, or
/// `None` if the point misses every frame. Containment is edge-inclusive on
/// the left/top and edge-exclusive on the right/bottom — `px` in
/// `[frame.0, frame.0 + frame.2)`, `py` in `[frame.1, frame.1 + frame.3)` —
/// so two abutting frames never both claim the shared boundary point.
pub fn window_under(px: f64, py: f64, frames: &[(f64, f64, f64, f64)]) -> Option<usize> {
    frames
        .iter()
        .position(|&(x, y, w, h)| px >= x && px < x + w && py >= y && py < y + h)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(n: u64) -> LayoutNode {
        LayoutNode::pane(PaneId(n), SessionId::new(format!("s{n}")))
    }

    /// Left-leaning chain of splits over one leaf per id in `sessions`, e.g.
    /// `[10,20,30]` -> `Split(Split(p10,p20), p30)`. Focus starts on the first
    /// pane. Mirrors `layout.rs`'s own test convention: pane id, tab id, and
    /// session name all share the same numeric id.
    fn tab(id: u64, sessions: &[u64]) -> Tab {
        let mut iter = sessions.iter();
        let first = *iter.next().expect("at least one session");
        let mut root = p(first);
        for &n in iter {
            root = LayoutNode::split(Axis::Horizontal, 0.5, root, p(n));
        }
        Tab {
            id: TabId(id),
            title: String::new(),
            root,
            focus: PaneId(first),
        }
    }

    /// One window per slice, one single-pane tab per id in that slice.
    fn windows(spec: &[&[u64]]) -> Windows {
        let trees = spec
            .iter()
            .map(|ids| WindowTree {
                tabs: ids
                    .iter()
                    .map(|&id| Tab {
                        id: TabId(id),
                        title: String::new(),
                        root: p(id),
                        focus: PaneId(id),
                    })
                    .collect(),
                active: 0,
            })
            .collect();
        Windows { trees, focused: 0 }
    }

    /// All session-id strings living in window `win`, across every tab —
    /// for invariant asserts.
    fn session_ids(w: &Windows, win: usize) -> Vec<String> {
        w.trees[win]
            .tabs
            .iter()
            .flat_map(|t| t.root.leaves().into_iter().map(|(_, s)| s.0))
            .collect()
    }

    /// Every session-id string anywhere in `w`, for whole-model conservation checks.
    fn all_session_ids(w: &Windows) -> Vec<String> {
        w.trees
            .iter()
            .enumerate()
            .flat_map(|(i, _)| session_ids(w, i))
            .collect()
    }

    fn sorted(mut v: Vec<String>) -> Vec<String> {
        v.sort();
        v
    }

    #[test]
    fn tab_to_new_window_opens_and_focuses() {
        let mut w = windows(&[&[1, 2], &[3]]);
        let effects = move_surface(
            &mut w,
            SurfaceRef::Tab { window: 0, tab: 0 },
            SurfaceDest::NewWindow,
            TabId(9000),
        )
        .unwrap();
        assert_eq!(w.trees.len(), 3);
        assert_eq!(w.trees[0].tabs.len(), 1);
        assert_eq!(w.trees[0].tabs[0].id, TabId(2));
        assert_eq!(w.trees[2].tabs.len(), 1);
        assert_eq!(w.trees[2].tabs[0].id, TabId(1));
        assert_eq!(w.focused, 2);
        assert_eq!(
            effects,
            vec![
                MoveEffect::WindowOpened { index: 2 },
                MoveEffect::SessionsRehomed {
                    sessions: vec![SessionId::new("s1")],
                    to_window: 2,
                },
            ]
        );
    }

    #[test]
    fn tab_to_other_window_appends_and_activates() {
        let mut w = windows(&[&[1, 2], &[3]]);
        let effects = move_surface(
            &mut w,
            SurfaceRef::Tab { window: 0, tab: 0 },
            SurfaceDest::NewTab { window: 1 },
            TabId(9001),
        )
        .unwrap();
        assert_eq!(w.trees.len(), 2);
        assert_eq!(w.trees[0].tabs.len(), 1);
        assert_eq!(w.trees[0].tabs[0].id, TabId(2));
        assert_eq!(
            w.trees[1].tabs.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![TabId(3), TabId(1)]
        );
        assert_eq!(w.trees[1].active, 1);
        assert_eq!(
            effects,
            vec![MoveEffect::SessionsRehomed {
                sessions: vec![SessionId::new("s1")],
                to_window: 1,
            }]
        );
    }

    #[test]
    fn moving_last_tab_out_closes_source_window_and_shifts_indices() {
        let mut w = windows(&[&[1], &[2], &[3]]);
        let effects = move_surface(
            &mut w,
            SurfaceRef::Tab { window: 0, tab: 0 },
            SurfaceDest::NewTab { window: 2 },
            TabId(9002),
        )
        .unwrap();
        assert_eq!(w.trees.len(), 2);
        assert_eq!(w.trees[0].tabs[0].id, TabId(2)); // former window1 slides to 0
        assert_eq!(
            w.trees[1].tabs.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![TabId(3), TabId(1)]
        );
        assert!(effects.contains(&MoveEffect::WindowClosed { index: 0 }));
        assert!(effects.contains(&MoveEffect::SessionsRehomed {
            sessions: vec![SessionId::new("s1")],
            to_window: 1,
        }));
    }

    #[test]
    fn pane_to_new_tab_extracts_leaf_and_sibling_absorbs() {
        let mut w = Windows {
            trees: vec![WindowTree {
                tabs: vec![tab(1, &[10, 20, 30])],
                active: 0,
            }],
            focused: 0,
        };
        let effects = move_surface(
            &mut w,
            SurfaceRef::Pane {
                window: 0,
                tab: 0,
                pane: PaneId(20),
            },
            SurfaceDest::NewTab { window: 0 },
            TabId(9003),
        )
        .unwrap();
        assert_eq!(
            w.trees[0].tabs[0].root,
            LayoutNode::split(Axis::Horizontal, 0.5, p(10), p(30))
        );
        assert_eq!(w.trees[0].tabs[0].focus, PaneId(10)); // untouched: focus wasn't on 20
        assert_eq!(w.trees[0].tabs.len(), 2);
        assert_eq!(w.trees[0].tabs[1].root, p(20));
        assert_eq!(w.trees[0].tabs[1].id, TabId(9003));
        assert_eq!(w.trees[0].active, 1);
        assert!(effects.is_empty()); // same window: nothing rehomed
    }

    #[test]
    fn pane_to_new_window() {
        let mut w = Windows {
            trees: vec![WindowTree {
                tabs: vec![tab(1, &[10, 20])],
                active: 0,
            }],
            focused: 0,
        };
        let effects = move_surface(
            &mut w,
            SurfaceRef::Pane {
                window: 0,
                tab: 0,
                pane: PaneId(20),
            },
            SurfaceDest::NewWindow,
            TabId(9004),
        )
        .unwrap();
        assert_eq!(w.trees.len(), 2);
        assert_eq!(w.trees[0].tabs[0].root, p(10));
        assert_eq!(w.trees[1].tabs[0].root, p(20));
        assert_eq!(w.trees[1].tabs[0].id, TabId(9004));
        assert_eq!(w.focused, 1);
        assert_eq!(
            effects,
            vec![
                MoveEffect::WindowOpened { index: 1 },
                MoveEffect::SessionsRehomed {
                    sessions: vec![SessionId::new("s20")],
                    to_window: 1,
                },
            ]
        );
    }

    /// Regression for the `TabId(pane.0)` collision bug: a promoted pane's new
    /// tab must get the id the caller passed in, not one derived from the
    /// pane's own id (which desyncs from ember-app's independent tab counter
    /// and can collide with an existing `TabId`). A Tab-sourced move must
    /// ignore `fresh_tab_id` entirely and keep the tab's own id.
    #[test]
    fn promoted_pane_gets_the_caller_supplied_tab_id() {
        // Pane -> NewTab: the newly minted tab gets `fresh_tab_id`.
        let mut w = Windows {
            trees: vec![WindowTree {
                tabs: vec![tab(1, &[10, 20])],
                active: 0,
            }],
            focused: 0,
        };
        move_surface(
            &mut w,
            SurfaceRef::Pane {
                window: 0,
                tab: 0,
                pane: PaneId(20),
            },
            SurfaceDest::NewTab { window: 0 },
            TabId(12345),
        )
        .unwrap();
        assert_eq!(w.trees[0].tabs[1].id, TabId(12345));
        assert_ne!(w.trees[0].tabs[1].id, TabId(20)); // NOT derived from PaneId(20)

        // Pane -> NewWindow: same contract.
        let mut w = Windows {
            trees: vec![WindowTree {
                tabs: vec![tab(1, &[10, 20])],
                active: 0,
            }],
            focused: 0,
        };
        move_surface(
            &mut w,
            SurfaceRef::Pane {
                window: 0,
                tab: 0,
                pane: PaneId(20),
            },
            SurfaceDest::NewWindow,
            TabId(54321),
        )
        .unwrap();
        assert_eq!(w.trees[1].tabs[0].id, TabId(54321));
        assert_ne!(w.trees[1].tabs[0].id, TabId(20));

        // Tab-sourced move: `fresh_tab_id` is ignored, the moved tab keeps
        // its own id. `windows(&[&[1, 2], &[3]])` makes 2 windows, so the new
        // window promoted tab 0 (id 1) out of window 0 lands at index 2.
        let mut w = windows(&[&[1, 2], &[3]]);
        move_surface(
            &mut w,
            SurfaceRef::Tab { window: 0, tab: 0 },
            SurfaceDest::NewWindow,
            TabId(99999),
        )
        .unwrap();
        assert_eq!(w.trees[2].tabs[0].id, TabId(1));
        assert_ne!(w.trees[2].tabs[0].id, TabId(99999));
    }

    #[test]
    fn pane_split_into_other_windows_pane() {
        let mut w = Windows {
            trees: vec![
                WindowTree {
                    tabs: vec![tab(1, &[10, 20])],
                    active: 0,
                },
                WindowTree {
                    tabs: vec![tab(2, &[30])],
                    active: 0,
                },
            ],
            focused: 0,
        };
        let effects = move_surface(
            &mut w,
            SurfaceRef::Pane {
                window: 0,
                tab: 0,
                pane: PaneId(10),
            },
            SurfaceDest::SplitInto {
                window: 1,
                tab: 0,
                pane: PaneId(30),
                axis: Axis::Vertical,
                before: false,
            },
            TabId(9005),
        )
        .unwrap();
        assert_eq!(w.trees[0].tabs[0].root, p(20));
        assert_eq!(w.trees[0].tabs[0].focus, PaneId(20)); // was on 10, extracted -> lands on remaining
        assert_eq!(
            w.trees[1].tabs[0].root,
            LayoutNode::split(Axis::Vertical, 0.5, p(30), p(10))
        );
        assert_eq!(w.trees[1].tabs[0].focus, PaneId(10));
        assert_eq!(
            effects,
            vec![MoveEffect::SessionsRehomed {
                sessions: vec![SessionId::new("s10")],
                to_window: 1,
            }]
        );
    }

    #[test]
    fn tab_merges_into_other_tab_as_split() {
        let mut w = Windows {
            trees: vec![
                WindowTree {
                    tabs: vec![tab(1, &[10, 20]), tab(99, &[99])],
                    active: 0,
                },
                WindowTree {
                    tabs: vec![tab(2, &[30])],
                    active: 0,
                },
            ],
            focused: 0,
        };
        let effects = move_surface(
            &mut w,
            SurfaceRef::Tab { window: 0, tab: 0 },
            SurfaceDest::SplitInto {
                window: 1,
                tab: 0,
                pane: PaneId(30),
                axis: Axis::Horizontal,
                before: false,
            },
            TabId(9006),
        )
        .unwrap();
        assert_eq!(w.trees[0].tabs.len(), 1);
        assert_eq!(w.trees[0].tabs[0].id, TabId(99));
        assert_eq!(
            w.trees[1].tabs[0].root,
            LayoutNode::split(
                Axis::Horizontal,
                0.5,
                p(30),
                LayoutNode::split(Axis::Horizontal, 0.5, p(10), p(20))
            )
        );
        assert_eq!(w.trees[1].tabs[0].focus, PaneId(10));
        assert_eq!(
            effects,
            vec![MoveEffect::SessionsRehomed {
                sessions: vec![SessionId::new("s10"), SessionId::new("s20")],
                to_window: 1,
            }]
        );
    }

    /// `before` steers which tree child the moved surface lands as: `true` ->
    /// left/top (child `a`), `false` -> right/bottom (child `b`, the existing
    /// behavior already exercised above). Covers both `SurfaceRef` shapes:
    /// a lone pane (`move_pane`) and a whole tab merging in (`move_tab`).
    #[test]
    fn split_into_before_places_moved_surface_left_or_top() {
        // Pane case: PaneId(10) moves, `before: true` -> it becomes child `a`.
        let mut w = Windows {
            trees: vec![
                WindowTree {
                    tabs: vec![tab(1, &[10, 20])],
                    active: 0,
                },
                WindowTree {
                    tabs: vec![tab(2, &[30])],
                    active: 0,
                },
            ],
            focused: 0,
        };
        move_surface(
            &mut w,
            SurfaceRef::Pane {
                window: 0,
                tab: 0,
                pane: PaneId(10),
            },
            SurfaceDest::SplitInto {
                window: 1,
                tab: 0,
                pane: PaneId(30),
                axis: Axis::Vertical,
                before: true,
            },
            TabId(9500),
        )
        .unwrap();
        // Moved pane (10) is child `a`; existing pane (30) is child `b` — the
        // mirror image of `pane_split_into_other_windows_pane`'s `before: false`.
        assert_eq!(
            w.trees[1].tabs[0].root,
            LayoutNode::split(Axis::Vertical, 0.5, p(10), p(30))
        );

        // Tab-merge case: the whole tab (10, 20) moves in `before: true`, so
        // it becomes child `a` and the existing pane (30) becomes child `b`.
        let mut w = Windows {
            trees: vec![
                WindowTree {
                    tabs: vec![tab(1, &[10, 20]), tab(99, &[99])],
                    active: 0,
                },
                WindowTree {
                    tabs: vec![tab(2, &[30])],
                    active: 0,
                },
            ],
            focused: 0,
        };
        move_surface(
            &mut w,
            SurfaceRef::Tab { window: 0, tab: 0 },
            SurfaceDest::SplitInto {
                window: 1,
                tab: 0,
                pane: PaneId(30),
                axis: Axis::Horizontal,
                before: true,
            },
            TabId(9501),
        )
        .unwrap();
        assert_eq!(
            w.trees[1].tabs[0].root,
            LayoutNode::split(
                Axis::Horizontal,
                0.5,
                LayoutNode::split(Axis::Horizontal, 0.5, p(10), p(20)),
                p(30)
            )
        );
        // Focus-after-merge is order-independent: it's set directly to the
        // moved tab's own focus pane id, not derived from tree position.
        assert_eq!(w.trees[1].tabs[0].focus, PaneId(10));
    }

    #[test]
    fn last_pane_as_pane_ref_errors_would_empty_tab() {
        let mut w = windows(&[&[1]]);
        let result = move_surface(
            &mut w,
            SurfaceRef::Pane {
                window: 0,
                tab: 0,
                pane: PaneId(1),
            },
            SurfaceDest::NewTab { window: 0 },
            TabId(9007),
        );
        assert_eq!(result, Err(MoveError::WouldEmptyTab));
    }

    #[test]
    fn out_of_range_and_noop_moves_error() {
        let mut w = windows(&[&[1, 2], &[3]]);
        let before = w.clone();

        assert!(matches!(
            move_surface(
                &mut w,
                SurfaceRef::Tab { window: 9, tab: 0 },
                SurfaceDest::NewWindow,
                TabId(9100)
            ),
            Err(MoveError::Invalid(_))
        ));
        assert!(matches!(
            move_surface(
                &mut w,
                SurfaceRef::Tab { window: 0, tab: 9 },
                SurfaceDest::NewWindow,
                TabId(9101)
            ),
            Err(MoveError::Invalid(_))
        ));
        assert!(matches!(
            move_surface(
                &mut w,
                SurfaceRef::Pane {
                    window: 0,
                    tab: 0,
                    pane: PaneId(999)
                },
                SurfaceDest::NewWindow,
                TabId(9102)
            ),
            Err(MoveError::Invalid(_))
        ));
        assert!(matches!(
            move_surface(
                &mut w,
                SurfaceRef::Tab { window: 0, tab: 0 },
                SurfaceDest::NewTab { window: 9 },
                TabId(9103)
            ),
            Err(MoveError::Invalid(_))
        ));
        assert!(matches!(
            move_surface(
                &mut w,
                SurfaceRef::Tab { window: 0, tab: 0 },
                SurfaceDest::SplitInto {
                    window: 1,
                    tab: 9,
                    pane: PaneId(3),
                    axis: Axis::Horizontal,
                    before: false
                },
                TabId(9104)
            ),
            Err(MoveError::Invalid(_))
        ));
        assert!(matches!(
            move_surface(
                &mut w,
                SurfaceRef::Tab { window: 0, tab: 0 },
                SurfaceDest::SplitInto {
                    window: 1,
                    tab: 0,
                    pane: PaneId(999),
                    axis: Axis::Horizontal,
                    before: false
                },
                TabId(9105)
            ),
            Err(MoveError::Invalid(_))
        ));
        assert!(matches!(
            move_surface(
                &mut w,
                SurfaceRef::Tab { window: 0, tab: 0 },
                SurfaceDest::NewTab { window: 0 },
                TabId(9106)
            ),
            Err(MoveError::Invalid(_))
        ));
        assert!(matches!(
            move_surface(
                &mut w,
                SurfaceRef::Pane {
                    window: 0,
                    tab: 0,
                    pane: PaneId(1)
                },
                SurfaceDest::SplitInto {
                    window: 0,
                    tab: 0,
                    pane: PaneId(1),
                    axis: Axis::Horizontal,
                    before: false
                },
                TabId(9107)
            ),
            Err(MoveError::Invalid(_))
        ));
        assert!(matches!(
            move_surface(
                &mut w,
                SurfaceRef::Tab { window: 0, tab: 0 },
                SurfaceDest::SplitInto {
                    window: 0,
                    tab: 0,
                    pane: PaneId(1),
                    axis: Axis::Horizontal,
                    before: false
                },
                TabId(9108)
            ),
            Err(MoveError::Invalid(_))
        ));

        assert_eq!(
            w, before,
            "every rejected move must leave the tree untouched"
        );
    }

    #[test]
    fn focus_lands_on_remaining_pane_in_source_tab() {
        let mut t = tab(1, &[10, 20, 30]);
        t.focus = PaneId(20); // focus starts on the pane we're about to extract
        let mut w = Windows {
            trees: vec![WindowTree {
                tabs: vec![t],
                active: 0,
            }],
            focused: 0,
        };
        move_surface(
            &mut w,
            SurfaceRef::Pane {
                window: 0,
                tab: 0,
                pane: PaneId(20),
            },
            SurfaceDest::NewWindow,
            TabId(9200),
        )
        .unwrap();
        let remaining_focus = w.trees[0].tabs[0].focus;
        assert!(w.trees[0].tabs[0].root.contains(remaining_focus));
        assert_ne!(remaining_focus, PaneId(20));
    }

    /// A 3-window fixture with a mix of multi- and single-pane tabs, used by
    /// the property tests below.
    fn fixture() -> Windows {
        Windows {
            trees: vec![
                WindowTree {
                    tabs: vec![tab(1, &[10, 20]), tab(2, &[21])],
                    active: 0,
                },
                WindowTree {
                    tabs: vec![tab(3, &[30, 31, 32])],
                    active: 0,
                },
                WindowTree {
                    tabs: vec![tab(4, &[40])],
                    active: 0,
                },
            ],
            focused: 0,
        }
    }

    /// Every possible `SurfaceRef` (each tab, each pane) in `w`.
    fn all_srcs(w: &Windows) -> Vec<SurfaceRef> {
        let mut out = Vec::new();
        for (wi, win) in w.trees.iter().enumerate() {
            for (ti, t) in win.tabs.iter().enumerate() {
                out.push(SurfaceRef::Tab {
                    window: wi,
                    tab: ti,
                });
                for (pane, _) in t.root.leaves() {
                    out.push(SurfaceRef::Pane {
                        window: wi,
                        tab: ti,
                        pane,
                    });
                }
            }
        }
        out
    }

    /// Every possible `SurfaceDest` (each window, `NewWindow`, each existing
    /// pane under both axes) in `w`.
    fn all_dests(w: &Windows) -> Vec<SurfaceDest> {
        let mut out = Vec::new();
        for wi in 0..w.trees.len() {
            out.push(SurfaceDest::NewTab { window: wi });
        }
        out.push(SurfaceDest::NewWindow);
        for (wi, win) in w.trees.iter().enumerate() {
            for (ti, t) in win.tabs.iter().enumerate() {
                for (pane, _) in t.root.leaves() {
                    out.push(SurfaceDest::SplitInto {
                        window: wi,
                        tab: ti,
                        pane,
                        axis: Axis::Horizontal,
                        before: false,
                    });
                    out.push(SurfaceDest::SplitInto {
                        window: wi,
                        tab: ti,
                        pane,
                        axis: Axis::Vertical,
                        before: false,
                    });
                }
            }
        }
        out
    }

    #[test]
    fn sessions_are_never_duplicated_or_dropped() {
        let base = fixture();
        let srcs = all_srcs(&base);
        let dests = all_dests(&base);

        // Every id here must be distinct from every other id used across the
        // whole loop AND from every id already present in `fixture()`
        // (1..=4, 99) — offset well clear of both so a colliding TabId can
        // never mask a real bug in the assertions below.
        let mut next_id: u64 = 90_000;
        for src in &srcs {
            for dest in &dests {
                let mut w = base.clone();
                next_id += 1;
                match move_surface(&mut w, *src, *dest, TabId(next_id)) {
                    Ok(_) => {
                        assert_eq!(
                            sorted(all_session_ids(&base)),
                            sorted(all_session_ids(&w)),
                            "src={src:?} dest={dest:?} lost or duplicated a session"
                        );
                    }
                    Err(_) => {
                        assert_eq!(w, base, "src={src:?} dest={dest:?} mutated on error");
                    }
                }
            }
        }
    }

    #[test]
    fn windows_never_left_empty_and_tabs_never_paneless() {
        let base = fixture();
        let srcs = all_srcs(&base);
        let dests = all_dests(&base);

        let mut next_id: u64 = 90_000;
        for src in &srcs {
            for dest in &dests {
                let mut w = base.clone();
                next_id += 1;
                if move_surface(&mut w, *src, *dest, TabId(next_id)).is_ok() {
                    for win in &w.trees {
                        assert!(!win.tabs.is_empty(), "window left with zero tabs");
                        for t in &win.tabs {
                            assert!(!t.root.pane_ids().is_empty(), "tab left paneless");
                        }
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------
    // Drop-zone geometry (release 2). See the `drop` section above for the
    // verified Axis <-> left/right/up/down mapping this encodes.
    // -----------------------------------------------------------------

    #[test]
    fn drop_zone_edges_map_to_axis_and_side() {
        // Pane is 100x100. Probes sit well inside a single band (the
        // orthogonal coordinate stays in [40,60], clear of any corner) so
        // each assertion exercises exactly one edge, not a tie.
        let west = [(5.0, 40.0), (5.0, 50.0), (5.0, 60.0)];
        let east = [(95.0, 40.0), (95.0, 50.0), (95.0, 60.0)];
        let north = [(40.0, 5.0), (50.0, 5.0), (60.0, 5.0)];
        let south = [(40.0, 95.0), (50.0, 95.0), (60.0, 95.0)];

        for &(x, y) in &west {
            assert_eq!(
                drop_zone_for(x, y, 100.0, 100.0),
                DropZone::Edge {
                    axis: Axis::Horizontal,
                    before: true
                },
                "west probe ({x},{y})"
            );
        }
        for &(x, y) in &east {
            assert_eq!(
                drop_zone_for(x, y, 100.0, 100.0),
                DropZone::Edge {
                    axis: Axis::Horizontal,
                    before: false
                },
                "east probe ({x},{y})"
            );
        }
        for &(x, y) in &north {
            assert_eq!(
                drop_zone_for(x, y, 100.0, 100.0),
                DropZone::Edge {
                    axis: Axis::Vertical,
                    before: true
                },
                "north probe ({x},{y})"
            );
        }
        for &(x, y) in &south {
            assert_eq!(
                drop_zone_for(x, y, 100.0, 100.0),
                DropZone::Edge {
                    axis: Axis::Vertical,
                    before: false
                },
                "south probe ({x},{y})"
            );
        }
    }

    #[test]
    fn drop_zone_center_is_the_middle_square() {
        // Strictly inside the middle 40% square.
        for &(x, y) in &[
            (50.0, 50.0),
            (31.0, 31.0),
            (69.0, 69.0),
            (31.0, 69.0),
            (69.0, 31.0),
        ] {
            assert_eq!(
                drop_zone_for(x, y, 100.0, 100.0),
                DropZone::Center,
                "interior probe ({x},{y})"
            );
        }
        // Exactly on the 30%/70% boundary lines: the band is the OUTER 30%,
        // so the boundary itself belongs to the center square, not the band.
        for &(x, y) in &[(30.0, 50.0), (70.0, 50.0), (50.0, 30.0), (50.0, 70.0)] {
            assert_eq!(
                drop_zone_for(x, y, 100.0, 100.0),
                DropZone::Center,
                "boundary probe ({x},{y})"
            );
        }
    }

    /// Degenerate pane dimensions (zero, negative, NaN) must resolve to
    /// `Center` rather than falling through to whatever the band math
    /// produces, which is geometrically meaningless on such sizes (e.g. a
    /// point outside a 0-width pane can still satisfy `x < w * BAND`).
    #[test]
    fn degenerate_pane_sizes_resolve_to_center() {
        assert_eq!(drop_zone_for(5.0, 5.0, 0.0, 0.0), DropZone::Center);
        assert_eq!(drop_zone_for(5.0, 5.0, -10.0, -10.0), DropZone::Center);
        assert_eq!(drop_zone_for(5.0, 5.0, f64::NAN, 100.0), DropZone::Center);
        assert_eq!(drop_zone_for(5.0, 5.0, 100.0, f64::NAN), DropZone::Center);
    }

    #[test]
    fn drop_zone_corners_pick_the_nearer_edge() {
        // Probes sit inside BOTH 15% bands (x and y < 15 on a 100-pane) so
        // the nearer-edge tiebreak is what's under test.
        // Top-left corner region: closer to the west edge than the north edge.
        assert_eq!(
            drop_zone_for(4.0, 12.0, 100.0, 100.0),
            DropZone::Edge {
                axis: Axis::Horizontal,
                before: true
            }
        );
        // Same corner region, but closer to the north edge than the west edge.
        assert_eq!(
            drop_zone_for(12.0, 4.0, 100.0, 100.0),
            DropZone::Edge {
                axis: Axis::Vertical,
                before: true
            }
        );
        // Top-right corner: closer to east than north.
        assert_eq!(
            drop_zone_for(96.0, 12.0, 100.0, 100.0),
            DropZone::Edge {
                axis: Axis::Horizontal,
                before: false
            }
        );
        // Bottom-left corner: closer to south than west.
        assert_eq!(
            drop_zone_for(12.0, 96.0, 100.0, 100.0),
            DropZone::Edge {
                axis: Axis::Vertical,
                before: false
            }
        );
        // Exact tie in the top-left corner resolves horizontal-first.
        assert_eq!(
            drop_zone_for(10.0, 10.0, 100.0, 100.0),
            DropZone::Edge {
                axis: Axis::Horizontal,
                before: true
            }
        );
        // Exact tie in the bottom-right corner: still horizontal-first.
        assert_eq!(
            drop_zone_for(90.0, 90.0, 100.0, 100.0),
            DropZone::Edge {
                axis: Axis::Horizontal,
                before: false
            }
        );
    }

    #[test]
    fn split_zone_always_picks_the_nearest_edge_no_center() {
        // Dead center of a square pane: tie -> horizontal-first (west).
        assert_eq!(
            split_zone_for(50.0, 50.0, 100.0, 100.0),
            DropZone::Edge {
                axis: Axis::Horizontal,
                before: true
            }
        );
        // Clearly in the left half.
        assert_eq!(
            split_zone_for(20.0, 50.0, 100.0, 100.0),
            DropZone::Edge {
                axis: Axis::Horizontal,
                before: true
            }
        );
        // Right half.
        assert_eq!(
            split_zone_for(80.0, 50.0, 100.0, 100.0),
            DropZone::Edge {
                axis: Axis::Horizontal,
                before: false
            }
        );
        // Near the left end of a wide pane: normalized (envelope-quadrant)
        // distance keeps this WEST (0.075) over north (0.1), even though the
        // absolute pixel distance to the top edge is smaller — the X-diagonal
        // division users know from editor drop zones.
        assert_eq!(
            split_zone_for(30.0, 10.0, 400.0, 100.0),
            DropZone::Edge {
                axis: Axis::Horizontal,
                before: true
            }
        );
        // Middle of the top band: genuinely north.
        assert_eq!(
            split_zone_for(200.0, 10.0, 400.0, 100.0),
            DropZone::Edge {
                axis: Axis::Vertical,
                before: true
            }
        );
        // Bottom half.
        assert_eq!(
            split_zone_for(200.0, 90.0, 400.0, 100.0),
            DropZone::Edge {
                axis: Axis::Vertical,
                before: false
            }
        );
        // Degenerate stays Center (harmless cancel).
        assert_eq!(split_zone_for(5.0, 5.0, 0.0, 0.0), DropZone::Center);
    }

    #[test]
    fn window_under_returns_topmost_first_hit() {
        // Two overlapping frames; front (index 0) covers the shared region.
        let frames = [(0.0, 0.0, 50.0, 50.0), (25.0, 25.0, 50.0, 50.0)];
        assert_eq!(window_under(30.0, 30.0, &frames), Some(0));
        // Only the back frame covers this point.
        assert_eq!(window_under(60.0, 60.0, &frames), Some(1));
        // Miss: outside both frames.
        assert_eq!(window_under(200.0, 200.0, &frames), None);

        // Edge inclusivity: left/top inclusive, right/bottom exclusive.
        let one = [(10.0, 10.0, 20.0, 20.0)];
        assert_eq!(window_under(10.0, 10.0, &one), Some(0)); // top-left corner: inside
        assert_eq!(window_under(30.0, 10.0, &one), None); // right edge: outside
        assert_eq!(window_under(10.0, 30.0, &one), None); // bottom edge: outside
        assert_eq!(window_under(29.999, 29.999, &one), Some(0)); // just inside bottom-right
    }
}
