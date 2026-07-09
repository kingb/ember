//! Pure geometry for layout (design §5). f64 for precise ratio splits.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn area(&self) -> f64 {
        self.width * self.height
    }

    pub fn center(&self) -> (f64, f64) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    /// The smallest rect containing both `self` and `other` — used to fold a
    /// moved surface's individual leaf-pane rects (a split's two halves, a
    /// multi-pane tab's several panes) into one overall footprint, e.g. for
    /// a pour-out morph that should cover the whole landed surface rather
    /// than any single leaf within it.
    pub fn union(&self, other: &Rect) -> Rect {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = (self.x + self.width).max(other.x + other.width);
        let bottom = (self.y + self.height).max(other.y + other.height);
        Rect::new(x, y, right - x, bottom - y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn center_is_midpoint() {
        let r = Rect::new(0.0, 0.0, 10.0, 4.0);
        assert_eq!(r.center(), (5.0, 2.0));
        assert_eq!(r.area(), 40.0);
    }

    #[test]
    fn union_covers_both_rects() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(20.0, 5.0, 10.0, 20.0);
        assert_eq!(a.union(&b), Rect::new(0.0, 0.0, 30.0, 25.0));
        // Order shouldn't matter.
        assert_eq!(b.union(&a), Rect::new(0.0, 0.0, 30.0, 25.0));
    }
}
