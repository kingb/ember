//! CPU alpha-coverage canvas: the pure rasterizer primitives the
//! box-drawing sprite rasterizer (/2.5) composes glyphs from. Each
//! primitive writes into an 8-bit coverage buffer — glyphon's
//! `ContentType::Mask` — via analytic anti-aliasing (per-pixel coverage from
//! exact overlap/distance, no supersampling). No GPU; pure and unit-testable.

/// An 8-bit alpha-coverage buffer, row-major, one byte per pixel.
pub struct Canvas {
    width: u16,
    height: u16,
    data: Vec<u8>,
}

impl Canvas {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            data: vec![0u8; width as usize * height as usize],
        }
    }

    /// Hands the raw buffer off to glyphon as `RasterizedCustomGlyph::data`
    /// (paired with `ContentType::Mask`).
    pub fn into_data(self) -> Vec<u8> {
        self.data
    }

    /// Coverage at `(x, y)` in `0..=255`, out-of-bounds reading as `0`. Only
    /// ever read back by tests (here and in `boxpaint.rs`) — production code
    /// just hands the buffer to glyphon via `into_data`.
    #[cfg(test)]
    pub fn coverage(&self, x: u16, y: u16) -> u8 {
        if x >= self.width || y >= self.height {
            return 0;
        }
        self.data[y as usize * self.width as usize + x as usize]
    }

    /// Max-blend one pixel's coverage (`0.0..=1.0`) — overlapping strokes
    /// (e.g. a cross's arms meeting at the center) saturate instead of
    /// double-darkening at the join.
    fn blend(&mut self, x: i32, y: i32, coverage: f32) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let idx = y as usize * self.width as usize + x as usize;
        let px = (coverage.clamp(0.0, 1.0) * 255.0).round() as u8;
        self.data[idx] = self.data[idx].max(px);
    }

    /// Filled axis-aligned rectangle `[x0, x1) x [y0, y1)`. Anti-aliased on
    /// fractional edges: each pixel's coverage is the overlap area between
    /// its unit square and the rect.
    pub fn fill_rect(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        let (x0, x1) = (x0.min(x1), x0.max(x1));
        let (y0, y1) = (y0.min(y1), y0.max(y1));
        let px0 = x0.floor().max(0.0) as i32;
        let px1 = x1.ceil().min(self.width as f32) as i32;
        let py0 = y0.floor().max(0.0) as i32;
        let py1 = y1.ceil().min(self.height as f32) as i32;
        for y in py0..py1 {
            let cov_y = overlap(y as f32, y as f32 + 1.0, y0, y1);
            if cov_y <= 0.0 {
                continue;
            }
            for x in px0..px1 {
                let cov_x = overlap(x as f32, x as f32 + 1.0, x0, x1);
                if cov_x <= 0.0 {
                    continue;
                }
                self.blend(x, y, cov_x * cov_y);
            }
        }
    }

    /// A straight orthogonal stroke of the given `thickness`, centered on
    /// `center` (the perpendicular axis), spanning `[start, end]` along the
    /// stroke axis. `horizontal = true` draws a `─`-style band; `false` draws
    /// a `│`-style band. Both ends are square (butt caps) — box-drawing arms
    /// meet at the cell center/edges, not rounded tips.
    pub fn stroke_orthogonal(
        &mut self,
        horizontal: bool,
        center: f32,
        start: f32,
        end: f32,
        thickness: f32,
    ) {
        let half = thickness / 2.0;
        if horizontal {
            self.fill_rect(start, center - half, end, center + half);
        } else {
            self.fill_rect(center - half, start, center + half, end);
        }
    }

    /// An anti-aliased quarter circular arc (rounded corner): the annulus
    /// `radius ± thickness/2` clipped to one quadrant around `(cx, cy)`.
    /// Coverage falls off over ~1px at the inner/outer edge.
    pub fn quarter_arc(
        &mut self,
        cx: f32,
        cy: f32,
        radius: f32,
        thickness: f32,
        quadrant: Quadrant,
    ) {
        let outer = radius + thickness / 2.0 + 1.0;
        let px0 = (cx - outer).floor().max(0.0) as i32;
        let px1 = (cx + outer).ceil().min(self.width as f32) as i32;
        let py0 = (cy - outer).floor().max(0.0) as i32;
        let py1 = (cy + outer).ceil().min(self.height as f32) as i32;
        let (sx, sy) = quadrant.sign();
        for y in py0..py1 {
            for x in px0..px1 {
                let sample_x = x as f32 + 0.5;
                let sample_y = y as f32 + 0.5;
                let dx = sample_x - cx;
                let dy = sample_y - cy;
                if dx * sx < 0.0 || dy * sy < 0.0 {
                    continue;
                }
                let dist = (dx * dx + dy * dy).sqrt();
                let coverage = (thickness / 2.0 - (dist - radius).abs() + 0.5).clamp(0.0, 1.0);
                if coverage > 0.0 {
                    self.blend(x, y, coverage);
                }
            }
        }
    }

    /// An anti-aliased diagonal stroke from `(x0,y0)` to `(x1,y1)` — a
    /// capsule (line segment thickened by `thickness`), coverage falling off
    /// over ~1px at the two long edges.
    pub fn diagonal(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, thickness: f32) {
        let half = thickness / 2.0;
        let px0 = (x0.min(x1) - half - 1.0).floor().max(0.0) as i32;
        let px1 = (x0.max(x1) + half + 1.0).ceil().min(self.width as f32) as i32;
        let py0 = (y0.min(y1) - half - 1.0).floor().max(0.0) as i32;
        let py1 = (y0.max(y1) + half + 1.0).ceil().min(self.height as f32) as i32;
        let (dx, dy) = (x1 - x0, y1 - y0);
        let len_sq = dx * dx + dy * dy;
        for y in py0..py1 {
            for x in px0..px1 {
                let sample_x = x as f32 + 0.5;
                let sample_y = y as f32 + 0.5;
                let t = if len_sq > 0.0 {
                    (((sample_x - x0) * dx + (sample_y - y0) * dy) / len_sq).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let nx = x0 + t * dx;
                let ny = y0 + t * dy;
                let dist = ((sample_x - nx).powi(2) + (sample_y - ny).powi(2)).sqrt();
                let coverage = (half - dist + 0.5).clamp(0.0, 1.0);
                if coverage > 0.0 {
                    self.blend(x, y, coverage);
                }
            }
        }
    }
}

/// 1D overlap length between `[a0, a1)` and `[b0, b1)`, `0.0` if disjoint.
fn overlap(a0: f32, a1: f32, b0: f32, b1: f32) -> f32 {
    (a1.min(b1) - a0.max(b0)).max(0.0)
}

/// One quadrant of a rounded corner, named by which cell corner it curves
/// into (matches `boxdraw::BoxGlyph::rounded`'s four codepoints).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quadrant {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Quadrant {
    fn sign(self) -> (f32, f32) {
        match self {
            Quadrant::TopLeft => (-1.0, -1.0),
            Quadrant::TopRight => (1.0, -1.0),
            Quadrant::BottomLeft => (-1.0, 1.0),
            Quadrant::BottomRight => (1.0, 1.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stroke of thickness `t` covers exactly `t` rows, centered.
    #[test]
    fn horizontal_stroke_covers_expected_rows() {
        let mut c = Canvas::new(10, 10);
        c.stroke_orthogonal(true, 5.0, 0.0, 10.0, 4.0);
        for y in 0..10u16 {
            let expect_full = (3..7).contains(&y);
            for x in 0..10u16 {
                if expect_full {
                    assert_eq!(
                        c.coverage(x, y),
                        255,
                        "row {y} col {x} should be fully covered"
                    );
                } else {
                    assert_eq!(c.coverage(x, y), 0, "row {y} col {x} should be empty");
                }
            }
        }
    }

    /// A vertical stroke covers exactly `t` columns, centered.
    #[test]
    fn vertical_stroke_covers_expected_cols() {
        let mut c = Canvas::new(10, 10);
        c.stroke_orthogonal(false, 5.0, 0.0, 10.0, 4.0);
        for x in 0..10u16 {
            let expect_full = (3..7).contains(&x);
            for y in 0..10u16 {
                if expect_full {
                    assert_eq!(
                        c.coverage(x, y),
                        255,
                        "col {x} row {y} should be fully covered"
                    );
                } else {
                    assert_eq!(c.coverage(x, y), 0, "col {x} row {y} should be empty");
                }
            }
        }
    }

    /// An odd thickness centered on an integer boundary splits one row's
    /// coverage across two pixels (each gets half coverage) — the AA edge is
    /// proportional to the fractional overlap, not rounded to a whole pixel.
    #[test]
    fn stroke_with_odd_thickness_antialiases_the_edge_row() {
        let mut c = Canvas::new(10, 10);
        c.stroke_orthogonal(true, 5.0, 0.0, 10.0, 3.0); // spans y in [3.5, 6.5)
        assert_eq!(c.coverage(0, 4), 255);
        assert_eq!(c.coverage(0, 5), 255);
        // Edge rows 3 and 6 are half-covered (0.5 * 255, rounded).
        let half = (0.5f32 * 255.0).round() as u8;
        assert_eq!(c.coverage(0, 3), half);
        assert_eq!(c.coverage(0, 6), half);
        assert_eq!(c.coverage(0, 2), 0);
        assert_eq!(c.coverage(0, 7), 0);
    }

    /// `fill_rect` clips to the canvas bounds instead of panicking or
    /// wrapping.
    #[test]
    fn fill_rect_clips_to_canvas_bounds() {
        let mut c = Canvas::new(4, 4);
        c.fill_rect(-2.0, -2.0, 6.0, 6.0);
        for y in 0..4u16 {
            for x in 0..4u16 {
                assert_eq!(c.coverage(x, y), 255);
            }
        }
    }

    /// Two strokes crossing at the center max-blend (saturate) at the
    /// overlap instead of stacking coverage past full.
    #[test]
    fn crossing_strokes_saturate_at_the_join() {
        let mut c = Canvas::new(10, 10);
        c.stroke_orthogonal(true, 5.0, 0.0, 10.0, 4.0);
        c.stroke_orthogonal(false, 5.0, 0.0, 10.0, 4.0);
        assert_eq!(c.coverage(5, 5), 255);
    }

    /// A quarter arc only paints within its quadrant, and only within the
    /// annulus band around `radius`.
    #[test]
    fn quarter_arc_stays_within_its_quadrant_and_band() {
        let mut c = Canvas::new(20, 20);
        c.quarter_arc(10.0, 10.0, 8.0, 2.0, Quadrant::BottomRight);
        // On-band, in-quadrant point (down-right of center): fully covered.
        assert_eq!(c.coverage(15, 15), 255);
        // Same distance, mirrored into the top-left quadrant: untouched.
        assert_eq!(c.coverage(4, 4), 0);
        // Far outside the annulus band, even in-quadrant: untouched.
        assert_eq!(c.coverage(19, 19), 0);
    }

    /// A diagonal stroke covers points on its centerline and leaves points
    /// far from the segment untouched.
    #[test]
    fn diagonal_stroke_covers_centerline_not_far_corners() {
        let mut c = Canvas::new(10, 10);
        c.diagonal(0.0, 0.0, 10.0, 10.0, 2.0);
        assert_eq!(c.coverage(5, 5), 255);
        assert_eq!(c.coverage(0, 9), 0);
        assert_eq!(c.coverage(9, 0), 0);
    }
}
