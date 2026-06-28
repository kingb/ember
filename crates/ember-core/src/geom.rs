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
}
