//! Geometric directional focus (design §5): movement follows laid-out rects,
//! not tree position, so it does the visually-correct thing across nesting.

use crate::geom::Rect;
use crate::ids::PaneId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Length of the overlap between ranges `[a0, a1]` and `[b0, b1]` (0 if none).
fn overlap(a0: f64, a1: f64, b0: f64, b1: f64) -> f64 {
    (a1.min(b1) - a0.max(b0)).max(0.0)
}

/// Nearest pane in `dir` from `current`, by rect geometry. `None` at an edge.
///
/// Among panes whose center lies in the requested direction, the winner is
/// chosen by: panes that **overlap** `current` on the perpendicular axis first
/// (so moving down stays in the same column), then the smallest primary-axis
/// gap, then the smallest perpendicular offset, then the smaller [`PaneId`]
/// for determinism.
pub fn focus_dir(panes: &[(PaneId, Rect)], current: PaneId, dir: Direction) -> Option<PaneId> {
    let cur = panes.iter().find(|(id, _)| *id == current)?.1;
    let (cx, cy) = cur.center();
    panes
        .iter()
        .filter(|(id, _)| *id != current)
        .filter_map(|(id, r)| {
            let (x, y) = r.center();
            let in_dir = match dir {
                Direction::Left => x < cx,
                Direction::Right => x > cx,
                Direction::Up => y < cy,
                Direction::Down => y > cy,
            };
            if !in_dir {
                return None;
            }
            let (primary, perp) = match dir {
                Direction::Left | Direction::Right => ((x - cx).abs(), (y - cy).abs()),
                Direction::Up | Direction::Down => ((y - cy).abs(), (x - cx).abs()),
            };
            // Perpendicular-axis overlap of the rects: 0 (overlaps) sorts before
            // 1 (disjoint), so an aligned pane beats a closer-but-offset one.
            let disjoint = match dir {
                Direction::Left | Direction::Right => {
                    overlap(cur.y, cur.y + cur.height, r.y, r.y + r.height) <= 0.0
                }
                Direction::Up | Direction::Down => {
                    overlap(cur.x, cur.x + cur.width, r.x, r.x + r.width) <= 0.0
                }
            };
            Some((u8::from(disjoint), primary, perp, id.0, *id))
        })
        .min_by(|a, b| {
            a.0.cmp(&b.0)
                .then(a.1.partial_cmp(&b.1).unwrap())
                .then(a.2.partial_cmp(&b.2).unwrap())
                .then(a.3.cmp(&b.3))
        })
        .map(|(_, _, _, _, id)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Three panes: 1 = left half, 2 = top-right, 3 = bottom-right.
    fn panes() -> Vec<(PaneId, Rect)> {
        vec![
            (PaneId(1), Rect::new(0.0, 0.0, 50.0, 100.0)),
            (PaneId(2), Rect::new(50.0, 0.0, 50.0, 50.0)),
            (PaneId(3), Rect::new(50.0, 50.0, 50.0, 50.0)),
        ]
    }

    #[test]
    fn right_from_left_picks_nearest_vertically() {
        // From pane 1 (center y=50) going right, panes 2 (cy=25) and 3 (cy=75)
        // are equidistant; the tie breaks toward the smaller id.
        assert_eq!(
            focus_dir(&panes(), PaneId(1), Direction::Right),
            Some(PaneId(2))
        );
    }

    #[test]
    fn left_from_top_right_finds_left_pane() {
        assert_eq!(
            focus_dir(&panes(), PaneId(2), Direction::Left),
            Some(PaneId(1))
        );
    }

    #[test]
    fn down_from_top_right_finds_bottom_right() {
        assert_eq!(
            focus_dir(&panes(), PaneId(2), Direction::Down),
            Some(PaneId(3))
        );
    }

    #[test]
    fn up_at_edge_returns_none() {
        assert_eq!(focus_dir(&panes(), PaneId(2), Direction::Up), None);
    }
}
