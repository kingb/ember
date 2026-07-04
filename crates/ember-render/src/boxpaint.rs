//! Draws a [`BoxGlyph`] ('s geometry) into a [`Canvas`] alpha mask:
//! straight lines, corners, tees, crosses and dashes in light/heavy/double
//! weights, plus rounded corners and diagonals —
//! the full Box Drawing block, U+2500..=U+257F.
//!
//! Each present arm draws independently from the cell center to its edge
//! (or, for a dash, the whole straight run draws as one edge-to-edge dash
//! pattern) — see [`paint`]'s doc for why this guarantees the SEAM property:
//! adjacent cells' touching arms meet with no gap. Rounded corners keep that
//! same edge-reaching property for their straight stubs; only the
//! center-ward end recedes to make room for the arc (see [`paint_rounded`]).

use crate::boxdraw::{BoxGlyph, Dash, Diagonal, Weight};
use crate::canvas::{Canvas, Quadrant};

/// Which edge an arm reaches — perpendicular to the arm's own run, this is
/// the coordinate the *other* arm's stroke is centered on.
#[derive(Clone, Copy)]
enum Axis {
    /// `left`/`right` arms: the stroke is a horizontal band (`stroke_orthogonal`
    /// with `horizontal = true`), centered on `cy`.
    Horizontal,
    /// `up`/`down` arms: a vertical band, centered on `cx`.
    Vertical,
}

/// Light stroke thickness for a `w`x`h` cell — heavy is exactly 2x this
/// (Alacritty's convention; Ghostty derives a metric instead, which is a
/// reasonable future refinement, not required by 's acceptance bar).
fn light_thickness(w: u16, h: u16) -> f32 {
    (w.min(h) as f32 / 8.0).max(1.0)
}

/// Emphasis multiplier applied to every stroke thickness when the cell
/// carries SGR 1 (bold) — 's "respect attrs" requirement. Bold
/// text reads heavier via a bolder font weight; a procedural sprite has no
/// such variant, so it reads heavier via a thicker stroke instead.
const BOLD_EMPHASIS: f32 = 1.4;

/// Precomputed stroke thickness for a cell, bundled to keep the paint
/// helpers' argument lists short.
#[derive(Clone, Copy)]
struct Weights {
    light: f32,
    heavy: f32,
}

impl Weights {
    fn for_cell(w: u16, h: u16, bold: bool) -> Self {
        let mut light = light_thickness(w, h);
        if bold {
            light *= BOLD_EMPHASIS;
        }
        Self {
            light,
            heavy: light * 2.0,
        }
    }

    fn px(self, weight: Weight) -> f32 {
        match weight {
            Weight::Heavy => self.heavy,
            Weight::Light | Weight::Double => self.light,
        }
    }
}

/// Paint `glyph` into a fresh `w`x`h` alpha-coverage canvas. `bold` thickens
/// every stroke by [`BOLD_EMPHASIS`] (: SGR 1 on the cell, not to
/// be confused with `Weight::Heavy`, which is the box-drawing character's
/// own weight — the two compose, e.g. a bold `┃` is thicker than a plain `┃`).
///
/// SEAM property: every present arm's stroke spans `[start, end]` along its
/// axis where `end` (for `right`/`down`) is exactly `w`/`h` and `start` (for
/// `left`/`up`) is exactly `0.0` — never inset. Two horizontally (or
/// vertically) adjacent cells are rasterized from the *same* geometry at the
/// *same* physical cell size, so cell A's `right` arm and cell B's `left` arm
/// reach the identical shared boundary with no rounding gap between them.
pub fn paint(glyph: &BoxGlyph, w: u16, h: u16, bold: bool) -> Canvas {
    let mut canvas = Canvas::new(w, h);
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    let weights = Weights::for_cell(w, h, bold);

    if let Some(dash) = glyph.dash {
        let span = CellSpan {
            w: w as f32,
            h: h as f32,
        };
        paint_dash(&mut canvas, glyph, dash, span, weights);
        return canvas;
    }
    if let Some(diagonal) = glyph.diagonal {
        paint_diagonal(&mut canvas, diagonal, w, h, weights.light);
        return canvas;
    }
    if glyph.rounded {
        paint_rounded(&mut canvas, glyph, cx, cy, w, h, weights.light);
        return canvas;
    }

    if let Some(weight) = glyph.up {
        paint_arm(&mut canvas, Axis::Vertical, weight, cx, 0.0, cy, weights);
    }
    if let Some(weight) = glyph.down {
        paint_arm(
            &mut canvas,
            Axis::Vertical,
            weight,
            cx,
            cy,
            h as f32,
            weights,
        );
    }
    if let Some(weight) = glyph.left {
        paint_arm(&mut canvas, Axis::Horizontal, weight, cy, 0.0, cx, weights);
    }
    if let Some(weight) = glyph.right {
        paint_arm(
            &mut canvas,
            Axis::Horizontal,
            weight,
            cy,
            cx,
            w as f32,
            weights,
        );
    }
    canvas
}

/// Paint one arm: a single band for `Light`/`Heavy`, or two parallel rails
/// (with a gap between them, same pitch as `light`) for `Double`. `centerline`
/// is the coordinate perpendicular to the arm's run (`cy` for a horizontal
/// arm, `cx` for a vertical one); `start`/`end` bound the run itself.
fn paint_arm(
    canvas: &mut Canvas,
    axis: Axis,
    weight: Weight,
    centerline: f32,
    start: f32,
    end: f32,
    weights: Weights,
) {
    match weight {
        Weight::Light | Weight::Heavy => {
            stroke(canvas, axis, centerline, start, end, weights.px(weight))
        }
        Weight::Double => {
            // Two rails of `light` thickness, `light` gap between them —
            // "bound each segment" (Alacritty's approach, not Ghostty's
            // hollow-junction gapping; see boxdraw.rs's module notes).
            let offset = weights.light;
            stroke(canvas, axis, centerline - offset, start, end, weights.light);
            stroke(canvas, axis, centerline + offset, start, end, weights.light);
        }
    }
}

fn stroke(canvas: &mut Canvas, axis: Axis, centerline: f32, start: f32, end: f32, thickness: f32) {
    match axis {
        Axis::Horizontal => canvas.stroke_orthogonal(true, centerline, start, end, thickness),
        Axis::Vertical => canvas.stroke_orthogonal(false, centerline, start, end, thickness),
    }
}

/// Paint a rounded corner (`╭╮╯╰`): the same two straight arms as the sharp
/// corner it replaces, but each arm's center-ward end recedes by `radius` to
/// make room for a quarter-circle arc that joins them with no kink. The
/// edge-ward end is untouched, so the SEAM property (arm reaches the exact
/// cell edge) still holds — only the corner's *interior* changes shape.
///
/// The arc's center sits `radius` out from the cell center, offset toward
/// the same corner the two arms extend into (e.g. `╭` extends right+down,
/// so the arc center is at `(cx+radius, cy+radius)`); the arc itself sweeps
/// the *opposite* quadrant — the missing corner between the two arms.
fn paint_rounded(
    canvas: &mut Canvas,
    glyph: &BoxGlyph,
    cx: f32,
    cy: f32,
    w: u16,
    h: u16,
    light: f32,
) {
    let hsign = if glyph.right.is_some() { 1.0 } else { -1.0 };
    let vsign = if glyph.down.is_some() { 1.0 } else { -1.0 };
    // Radius: generous enough to read as a curve, capped well under half the
    // cell so the straight stubs never vanish.
    let radius = (w.min(h) as f32 / 3.0).max(light);

    let arc_cx = cx + hsign * radius;
    let arc_cy = cy + vsign * radius;
    let quadrant = match (hsign > 0.0, vsign > 0.0) {
        (true, true) => Quadrant::TopLeft,
        (false, true) => Quadrant::TopRight,
        (false, false) => Quadrant::BottomRight,
        (true, false) => Quadrant::BottomLeft,
    };
    canvas.quarter_arc(arc_cx, arc_cy, radius, light, quadrant);

    let h_edge = if hsign > 0.0 { w as f32 } else { 0.0 };
    stroke(
        canvas,
        Axis::Horizontal,
        cy,
        cx + hsign * radius,
        h_edge,
        light,
    );
    let v_edge = if vsign > 0.0 { h as f32 } else { 0.0 };
    stroke(
        canvas,
        Axis::Vertical,
        cx,
        cy + vsign * radius,
        v_edge,
        light,
    );
}

/// Paint a diagonal (`╱╲╳`) corner-to-corner, so adjacent diagonal cells
/// tile into continuous lines (each cell's diagonal ends exactly at the
/// shared corner with its neighbor's).
fn paint_diagonal(canvas: &mut Canvas, diagonal: Diagonal, w: u16, h: u16, thickness: f32) {
    let (w, h) = (w as f32, h as f32);
    match diagonal {
        Diagonal::Forward => canvas.diagonal(0.0, h, w, 0.0, thickness),
        Diagonal::Back => canvas.diagonal(0.0, 0.0, w, h, thickness),
        Diagonal::Cross => {
            canvas.diagonal(0.0, h, w, 0.0, thickness);
            canvas.diagonal(0.0, 0.0, w, h, thickness);
        }
    }
}

/// A cell's logical width/height, bundled to keep [`paint_dash`]'s argument
/// list short.
#[derive(Clone, Copy)]
struct CellSpan {
    w: f32,
    h: f32,
}

/// Paint a dashed straight line: `count` dashes and `count` gaps, trailing
/// gap last (Ghostty's placement — preferred per boxdraw.rs's module notes
/// because it tiles cleanly across adjacent cells on the same axis, unlike
/// Alacritty's centered dashes).
fn paint_dash(canvas: &mut Canvas, glyph: &BoxGlyph, dash: Dash, span: CellSpan, weights: Weights) {
    let segments = dash.segments();
    let (cx, cy) = (span.w / 2.0, span.h / 2.0);
    // Dash only ever pairs with a plain light/heavy straight line (never
    // `Double`) in the geometry table — `Weights::px` degrades `Double` to
    // `light` if that invariant is ever broken, rather than panicking.
    if let Some(weight) = glyph.left.or(glyph.right) {
        dash_run(
            canvas,
            Axis::Horizontal,
            cy,
            span.w,
            weights.px(weight),
            segments,
        );
    } else if let Some(weight) = glyph.up.or(glyph.down) {
        dash_run(
            canvas,
            Axis::Vertical,
            cx,
            span.h,
            weights.px(weight),
            segments,
        );
    }
}

/// `segments` dashes and `segments` gaps, alternating, spanning `[0, total]`
/// — the trailing segment is always a gap.
fn dash_run(
    canvas: &mut Canvas,
    axis: Axis,
    centerline: f32,
    total: f32,
    thickness: f32,
    segments: u32,
) {
    let seg_len = total / (2 * segments) as f32;
    for i in 0..segments {
        let start = (2 * i) as f32 * seg_len;
        stroke(canvas, axis, centerline, start, start + seg_len, thickness);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxdraw::box_glyph;

    const W: u16 = 24;
    const H: u16 = 32;

    /// Every codepoint in the Box Drawing block — as of , [`paint`]
    /// covers all 128, matching `crate::sprite::is_sprite_glyph`'s scoping.
    fn paintable_codepoints() -> impl Iterator<Item = char> {
        (0x2500u32..=0x257F).filter_map(|cp| {
            let c = char::from_u32(cp).unwrap();
            box_glyph(c).map(|_| c)
        })
    }

    #[test]
    fn covers_the_whole_box_drawing_block() {
        assert_eq!(paintable_codepoints().count(), 128);
    }

    #[test]
    fn every_in_scope_codepoint_paints_some_ink() {
        for c in paintable_codepoints() {
            let glyph = box_glyph(c).unwrap();
            let canvas = paint(&glyph, W, H, false);
            let any_ink = (0..H).any(|y| (0..W).any(|x| canvas.coverage(x, y) > 0));
            assert!(any_ink, "U+{:04X} ({c:?}) painted nothing", c as u32);
        }
    }

    /// SEAM property: a present arm's stroke reaches full coverage at the
    /// exact cell edge it points at, so a same-size neighbor cell's mirrored
    /// arm (reaching the opposite way) touches it with no gap. Dashes are
    /// exempt — their seam property is rhythm continuity (trailing gap
    /// hands off to the next cell's leading dash), not edge coverage; see
    /// `dash_produces_the_expected_segment_count` and `paint_dash`'s doc.
    #[test]
    fn arms_reach_the_exact_cell_edge() {
        for c in paintable_codepoints().filter(|c| box_glyph(*c).unwrap().dash.is_none()) {
            let glyph = box_glyph(c).unwrap();
            let canvas = paint(&glyph, W, H, false);
            if glyph.right.is_some() {
                assert!(
                    (0..H).any(|y| canvas.coverage(W - 1, y) == 255),
                    "U+{:04X} right arm doesn't reach x={}",
                    c as u32,
                    W - 1
                );
            }
            if glyph.left.is_some() {
                assert!(
                    (0..H).any(|y| canvas.coverage(0, y) == 255),
                    "U+{:04X} left arm doesn't reach x=0",
                    c as u32
                );
            }
            if glyph.down.is_some() {
                assert!(
                    (0..W).any(|x| canvas.coverage(x, H - 1) == 255),
                    "U+{:04X} down arm doesn't reach y={}",
                    c as u32,
                    H - 1
                );
            }
            if glyph.up.is_some() {
                assert!(
                    (0..W).any(|x| canvas.coverage(x, 0) == 255),
                    "U+{:04X} up arm doesn't reach y=0",
                    c as u32
                );
            }
        }
    }

    #[test]
    fn heavy_is_visibly_thicker_than_light() {
        let full_rows_at_left_edge = |c: char| {
            let canvas = paint(&box_glyph(c).unwrap(), W, H, false);
            (0..H).filter(|&y| canvas.coverage(0, y) == 255).count()
        };
        assert!(full_rows_at_left_edge('━') > full_rows_at_left_edge('─'));
    }

    #[test]
    fn bold_thickens_a_stroke_beyond_its_own_plain_weight() {
        // : SGR 1 (bold) on the cell composes with the glyph's own
        // weight — a bold light line is thicker than a plain light line,
        // and a bold heavy line thicker still than a plain heavy line.
        let full_rows = |c: char, bold: bool| {
            let canvas = paint(&box_glyph(c).unwrap(), W, H, bold);
            (0..H).filter(|&y| canvas.coverage(0, y) == 255).count()
        };
        assert!(full_rows('─', true) > full_rows('─', false));
        assert!(full_rows('━', true) > full_rows('━', false));
    }

    #[test]
    fn double_weight_renders_two_separated_rails() {
        let canvas = paint(&box_glyph('═').unwrap(), W, H, false);
        assert_eq!(full_coverage_runs(|y| canvas.coverage(0, y), H), 2);
    }

    #[test]
    fn dash_produces_the_expected_segment_count() {
        // Triple dash (3 segments) and quad dash (4 segments), both light.
        let cy = H / 2;
        let triple = paint(&box_glyph('┄').unwrap(), W, H, false);
        assert_eq!(full_coverage_runs(|x| triple.coverage(x, cy), W), 3);
        let quad = paint(&box_glyph('┈').unwrap(), W, H, false);
        assert_eq!(full_coverage_runs(|x| quad.coverage(x, cy), W), 4);
    }

    /// "No kink" for a rounded corner: ink is present at both stub-arc
    /// joints and at the arc's midpoint, so the curve connects the two
    /// straight arms with no gap (a kink or gap would show up as a hole
    /// between these three sample points).
    #[test]
    fn rounded_corner_joins_its_arms_with_no_gap() {
        // '╭' extends right+down (b(N, L, N, L)).
        let canvas = paint(&box_glyph('╭').unwrap(), W, H, false);
        let (cx, cy) = (W as f32 / 2.0, H as f32 / 2.0);
        let r = (W.min(H) as f32 / 3.0).max(light_thickness(W, H));

        // Horizontal-stub/arc joint, directly above the arc's own center.
        assert!(canvas.coverage((cx + r) as u16, cy as u16) > 0);
        // Vertical-stub/arc joint, directly left of the arc's own center.
        assert!(canvas.coverage(cx as u16, (cy + r) as u16) > 0);
        // Arc midpoint (45° around the sweep, in the missing-corner region).
        let (arc_cx, arc_cy) = (cx + r, cy + r);
        let diag = r * std::f32::consts::FRAC_1_SQRT_2;
        assert!(canvas.coverage((arc_cx - diag) as u16, (arc_cy - diag) as u16) > 0);
    }

    #[test]
    fn rounded_corners_still_reach_the_cell_edge() {
        // The straight stubs' edge-ward ends are untouched by rounding —
        // the SEAM property (arms_reach_the_exact_cell_edge) still holds.
        for c in ['╭', '╮', '╯', '╰'] {
            let glyph = box_glyph(c).unwrap();
            assert!(glyph.rounded);
            let canvas = paint(&glyph, W, H, false);
            if glyph.right.is_some() {
                assert!((0..H).any(|y| canvas.coverage(W - 1, y) == 255));
            }
            if glyph.left.is_some() {
                assert!((0..H).any(|y| canvas.coverage(0, y) == 255));
            }
            if glyph.down.is_some() {
                assert!((0..W).any(|x| canvas.coverage(x, H - 1) == 255));
            }
            if glyph.up.is_some() {
                assert!((0..W).any(|x| canvas.coverage(x, 0) == 255));
            }
        }
    }

    #[test]
    fn diagonal_reaches_its_two_corners_and_is_anti_aliased() {
        // '╱' forward: bottom-left to top-right.
        let canvas = paint(&box_glyph('╱').unwrap(), W, H, false);
        assert!(canvas.coverage(0, H - 1) > 0, "should reach bottom-left");
        assert!(canvas.coverage(W - 1, 0) > 0, "should reach top-right");
        let has_partial_coverage = (0..H).any(|y| {
            (0..W).any(|x| {
                let c = canvas.coverage(x, y);
                c > 0 && c < 255
            })
        });
        assert!(
            has_partial_coverage,
            "diagonal should be anti-aliased, not just hard on/off"
        );
    }

    #[test]
    fn cross_diagonal_reaches_all_four_corners() {
        let canvas = paint(&box_glyph('╳').unwrap(), W, H, false);
        assert!(canvas.coverage(0, 0) > 0);
        assert!(canvas.coverage(W - 1, 0) > 0);
        assert!(canvas.coverage(0, H - 1) > 0);
        assert!(canvas.coverage(W - 1, H - 1) > 0);
    }

    /// Count runs of consecutive fully-covered (`255`) samples along a
    /// 0..len scan — e.g. rails along a cross-section, or dashes along a run.
    fn full_coverage_runs(sample: impl Fn(u16) -> u8, len: u16) -> u32 {
        let mut runs = 0;
        let mut in_run = false;
        for i in 0..len {
            let on = sample(i) == 255;
            if on && !in_run {
                runs += 1;
            }
            in_run = on;
        }
        runs
    }
}
