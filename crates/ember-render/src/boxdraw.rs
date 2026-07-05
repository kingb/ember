//! Box-drawing geometry: the pure `codepoint -> stroke spec` mapping
//! for the whole Box Drawing block U+2500..=U+257F.
//!
//! This is data only — no GPU. It's the heart of the completeness guarantee: the
//! range is a closed, standardized Unicode block, so [`box_glyph`] maps every one
//! of the 128 codepoints and the test module proves it exhaustively. The sprite
//! rasterizer ( / 2.5) turns a [`BoxGlyph`] into an alpha mask; anything
//! this returns `None` for falls through to the font (never a regression).
//!
//! Arms point from the cell center out to each edge.
//!
//! **Cross-checked 2026-07-04** against Alacritty `builtin_font.rs` (master)
//! and Ghostty `src/font/sprite/draw/box.zig` (main): all 128 codepoints agree
//! with both references — zero spec differences, including the mixed-weight
//! tees/crosses (U+251C–254B), the double/single junctions (U+2550–256C), and
//! the mixed half-lines (U+257C–257F). Dash counts: Triple/Quad = Alacritty
//! `num_gaps` 2/3 = Ghostty `count` 3/4.
//!
//! Reference differences that live in the RASTERIZER (/2.5), not in
//! this table:
//! - Vertical dash placement: Ghostty leaves a full gap at the cell bottom
//!   (tiles better when stacked); Alacritty centers the dashes. Prefer Ghostty.
//! - Double-line junctions carve a hollow interior (e.g. ╬'s open center):
//!   Ghostty gaps the arms at the junction; Alacritty bounds each segment.
//!   `BoxGlyph` is weight-only, so this is junction logic in the drawer.
//! - Heavy stroke width: Alacritty = exactly 2× light; Ghostty uses a metric.

/// Stroke weight of one arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Weight {
    Light,
    Heavy,
    Double,
}

/// Dash pattern across a straight stroke — the count of dashes spanning the cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dash {
    Double,
    Triple,
    Quad,
}

impl Dash {
    /// Dash segments across the stroke (never paired with `Weight::Double` in
    /// the table above — dashes only appear on plain light/heavy lines).
    pub fn segments(self) -> u32 {
        match self {
            Dash::Double => 2,
            Dash::Triple => 3,
            Dash::Quad => 4,
        }
    }
}

/// A diagonal stroke (the `╱ ╲ ╳` family) — not axis-aligned, drawn corner to
/// corner so adjacent diagonals tile into continuous lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Diagonal {
    /// `╱` bottom-left to top-right.
    Forward,
    /// `╲` top-left to bottom-right.
    Back,
    /// `╳` both.
    Cross,
}

/// A resolved box-drawing glyph: per-arm weights (up/down/left/right), an optional
/// dash pattern on the straight strokes, rounded corners, and an optional diagonal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BoxGlyph {
    pub up: Option<Weight>,
    pub down: Option<Weight>,
    pub left: Option<Weight>,
    pub right: Option<Weight>,
    pub dash: Option<Dash>,
    pub rounded: bool,
    pub diagonal: Option<Diagonal>,
}

// Terse arm aliases for the table below.
const N: Option<Weight> = None;
const L: Option<Weight> = Some(Weight::Light);
const H: Option<Weight> = Some(Weight::Heavy);
const D: Option<Weight> = Some(Weight::Double);

/// Build a glyph from its four arms (up, down, left, right).
const fn b(
    up: Option<Weight>,
    down: Option<Weight>,
    left: Option<Weight>,
    right: Option<Weight>,
) -> BoxGlyph {
    BoxGlyph {
        up,
        down,
        left,
        right,
        dash: None,
        rounded: false,
        diagonal: None,
    }
}

/// Add a dash pattern to a straight-line glyph.
const fn dashed(mut g: BoxGlyph, d: Dash) -> BoxGlyph {
    g.dash = Some(d);
    g
}

/// Mark a corner glyph as rounded (`╭ ╮ ╰ ╯`).
const fn rounded(mut g: BoxGlyph) -> BoxGlyph {
    g.rounded = true;
    g
}

/// A pure diagonal glyph (`╱ ╲ ╳`).
const fn diag(d: Diagonal) -> BoxGlyph {
    BoxGlyph {
        up: N,
        down: N,
        left: N,
        right: N,
        dash: None,
        rounded: false,
        diagonal: Some(d),
    }
}

/// Map a char to its box-drawing spec, or `None` if it isn't a Box Drawing glyph
/// (those keep the font). Exhaustive over U+2500..=U+257F.
pub fn box_glyph(c: char) -> Option<BoxGlyph> {
    use Dash::{Double, Quad, Triple};
    use Diagonal::{Back, Cross, Forward};
    Some(match c {
        // Straight lines.
        '\u{2500}' => b(N, N, L, L), // ─
        '\u{2501}' => b(N, N, H, H), // ━
        '\u{2502}' => b(L, L, N, N), // │
        '\u{2503}' => b(H, H, N, N), // ┃
        // Dashed lines (triple / quadruple).
        '\u{2504}' => dashed(b(N, N, L, L), Triple), // ┄
        '\u{2505}' => dashed(b(N, N, H, H), Triple), // ┅
        '\u{2506}' => dashed(b(L, L, N, N), Triple), // ┆
        '\u{2507}' => dashed(b(H, H, N, N), Triple), // ┇
        '\u{2508}' => dashed(b(N, N, L, L), Quad),   // ┈
        '\u{2509}' => dashed(b(N, N, H, H), Quad),   // ┉
        '\u{250A}' => dashed(b(L, L, N, N), Quad),   // ┊
        '\u{250B}' => dashed(b(H, H, N, N), Quad),   // ┋
        // Corners (down+right, down+left, up+right, up+left) in all weight mixes.
        '\u{250C}' => b(N, L, N, L), // ┌
        '\u{250D}' => b(N, L, N, H), // ┍
        '\u{250E}' => b(N, H, N, L), // ┎
        '\u{250F}' => b(N, H, N, H), // ┏
        '\u{2510}' => b(N, L, L, N), // ┐
        '\u{2511}' => b(N, L, H, N), // ┑
        '\u{2512}' => b(N, H, L, N), // ┒
        '\u{2513}' => b(N, H, H, N), // ┓
        '\u{2514}' => b(L, N, N, L), // └
        '\u{2515}' => b(L, N, N, H), // ┕
        '\u{2516}' => b(H, N, N, L), // ┖
        '\u{2517}' => b(H, N, N, H), // ┗
        '\u{2518}' => b(L, N, L, N), // ┘
        '\u{2519}' => b(L, N, H, N), // ┙
        '\u{251A}' => b(H, N, L, N), // ┚
        '\u{251B}' => b(H, N, H, N), // ┛
        // Vertical + right tees (├ family).
        '\u{251C}' => b(L, L, N, L), // ├
        '\u{251D}' => b(L, L, N, H), // ┝
        '\u{251E}' => b(H, L, N, L), // ┞
        '\u{251F}' => b(L, H, N, L), // ┟
        '\u{2520}' => b(H, H, N, L), // ┠
        '\u{2521}' => b(H, L, N, H), // ┡
        '\u{2522}' => b(L, H, N, H), // ┢
        '\u{2523}' => b(H, H, N, H), // ┣
        // Vertical + left tees (┤ family).
        '\u{2524}' => b(L, L, L, N), // ┤
        '\u{2525}' => b(L, L, H, N), // ┥
        '\u{2526}' => b(H, L, L, N), // ┦
        '\u{2527}' => b(L, H, L, N), // ┧
        '\u{2528}' => b(H, H, L, N), // ┨
        '\u{2529}' => b(H, L, H, N), // ┩
        '\u{252A}' => b(L, H, H, N), // ┪
        '\u{252B}' => b(H, H, H, N), // ┫
        // Down + horizontal tees (┬ family).
        '\u{252C}' => b(N, L, L, L), // ┬
        '\u{252D}' => b(N, L, H, L), // ┭
        '\u{252E}' => b(N, L, L, H), // ┮
        '\u{252F}' => b(N, L, H, H), // ┯
        '\u{2530}' => b(N, H, L, L), // ┰
        '\u{2531}' => b(N, H, H, L), // ┱
        '\u{2532}' => b(N, H, L, H), // ┲
        '\u{2533}' => b(N, H, H, H), // ┳
        // Up + horizontal tees (┴ family).
        '\u{2534}' => b(L, N, L, L), // ┴
        '\u{2535}' => b(L, N, H, L), // ┵
        '\u{2536}' => b(L, N, L, H), // ┶
        '\u{2537}' => b(L, N, H, H), // ┷
        '\u{2538}' => b(H, N, L, L), // ┸
        '\u{2539}' => b(H, N, H, L), // ┹
        '\u{253A}' => b(H, N, L, H), // ┺
        '\u{253B}' => b(H, N, H, H), // ┻
        // Crosses (┼ family).
        '\u{253C}' => b(L, L, L, L), // ┼
        '\u{253D}' => b(L, L, H, L), // ┽
        '\u{253E}' => b(L, L, L, H), // ┾
        '\u{253F}' => b(L, L, H, H), // ┿
        '\u{2540}' => b(H, L, L, L), // ╀
        '\u{2541}' => b(L, H, L, L), // ╁
        '\u{2542}' => b(H, H, L, L), // ╂
        '\u{2543}' => b(H, L, H, L), // ╃
        '\u{2544}' => b(H, L, L, H), // ╄
        '\u{2545}' => b(L, H, H, L), // ╅
        '\u{2546}' => b(L, H, L, H), // ╆
        '\u{2547}' => b(H, L, H, H), // ╇
        '\u{2548}' => b(L, H, H, H), // ╈
        '\u{2549}' => b(H, H, H, L), // ╉
        '\u{254A}' => b(H, H, L, H), // ╊
        '\u{254B}' => b(H, H, H, H), // ╋
        // Double-dash lines.
        '\u{254C}' => dashed(b(N, N, L, L), Double), // ╌
        '\u{254D}' => dashed(b(N, N, H, H), Double), // ╍
        '\u{254E}' => dashed(b(L, L, N, N), Double), // ╎
        '\u{254F}' => dashed(b(H, H, N, N), Double), // ╏
        // Double lines + single/double corners and junctions.
        '\u{2550}' => b(N, N, D, D), // ═
        '\u{2551}' => b(D, D, N, N), // ║
        '\u{2552}' => b(N, L, N, D), // ╒
        '\u{2553}' => b(N, D, N, L), // ╓
        '\u{2554}' => b(N, D, N, D), // ╔
        '\u{2555}' => b(N, L, D, N), // ╕
        '\u{2556}' => b(N, D, L, N), // ╖
        '\u{2557}' => b(N, D, D, N), // ╗
        '\u{2558}' => b(L, N, N, D), // ╘
        '\u{2559}' => b(D, N, N, L), // ╙
        '\u{255A}' => b(D, N, N, D), // ╚
        '\u{255B}' => b(L, N, D, N), // ╛
        '\u{255C}' => b(D, N, L, N), // ╜
        '\u{255D}' => b(D, N, D, N), // ╝
        '\u{255E}' => b(L, L, N, D), // ╞
        '\u{255F}' => b(D, D, N, L), // ╟
        '\u{2560}' => b(D, D, N, D), // ╠
        '\u{2561}' => b(L, L, D, N), // ╡
        '\u{2562}' => b(D, D, L, N), // ╢
        '\u{2563}' => b(D, D, D, N), // ╣
        '\u{2564}' => b(N, L, D, D), // ╤
        '\u{2565}' => b(N, D, L, L), // ╥
        '\u{2566}' => b(N, D, D, D), // ╦
        '\u{2567}' => b(L, N, D, D), // ╧
        '\u{2568}' => b(D, N, L, L), // ╨
        '\u{2569}' => b(D, N, D, D), // ╩
        '\u{256A}' => b(L, L, D, D), // ╪
        '\u{256B}' => b(D, D, L, L), // ╫
        '\u{256C}' => b(D, D, D, D), // ╬
        // Rounded corners.
        '\u{256D}' => rounded(b(N, L, N, L)), // ╭
        '\u{256E}' => rounded(b(N, L, L, N)), // ╮
        '\u{256F}' => rounded(b(L, N, L, N)), // ╯
        '\u{2570}' => rounded(b(L, N, N, L)), // ╰
        // Diagonals.
        '\u{2571}' => diag(Forward), // ╱
        '\u{2572}' => diag(Back),    // ╲
        '\u{2573}' => diag(Cross),   // ╳
        // Half-lines (stubs).
        '\u{2574}' => b(N, N, L, N), // ╴
        '\u{2575}' => b(L, N, N, N), // ╵
        '\u{2576}' => b(N, N, N, L), // ╶
        '\u{2577}' => b(N, L, N, N), // ╷
        '\u{2578}' => b(N, N, H, N), // ╸
        '\u{2579}' => b(H, N, N, N), // ╹
        '\u{257A}' => b(N, N, N, H), // ╺
        '\u{257B}' => b(N, H, N, N), // ╻
        // Mixed-weight half-lines.
        '\u{257C}' => b(N, N, L, H), // ╼
        '\u{257D}' => b(L, H, N, N), // ╽
        '\u{257E}' => b(N, N, H, L), // ╾
        '\u{257F}' => b(H, L, N, N), // ╿
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Completeness: every codepoint in the closed Box Drawing block resolves.
    /// This is the mechanical guarantee that we "got all of them."
    #[test]
    fn maps_every_box_drawing_codepoint() {
        for cp in 0x2500u32..=0x257F {
            let c = char::from_u32(cp).unwrap();
            assert!(
                box_glyph(c).is_some(),
                "U+{cp:04X} ({c:?}) has no box-drawing spec",
            );
        }
    }

    /// Non-box codepoints must fall through to the font (return `None`).
    #[test]
    fn non_box_codepoints_are_none() {
        assert!(box_glyph('\u{24FF}').is_none()); // just below the block
        assert!(box_glyph('\u{2580}').is_none()); // block elements — a later feature
        assert!(box_glyph('█').is_none()); // block element
        assert!(box_glyph('⏺').is_none()); // the tool bullet
        assert!(box_glyph('a').is_none());
        assert!(box_glyph(' ').is_none());
    }

    /// Spot-checks against known glyphs (weights + specials).
    #[test]
    fn known_glyphs_resolve_correctly() {
        // ─ light horizontal
        assert_eq!(box_glyph('─').unwrap(), b(N, N, L, L));
        // ┃ heavy vertical
        assert_eq!(box_glyph('┃').unwrap(), b(H, H, N, N));
        // ┼ light cross
        assert_eq!(box_glyph('┼').unwrap(), b(L, L, L, L));
        // ╋ heavy cross
        assert_eq!(box_glyph('╋').unwrap(), b(H, H, H, H));
        // ║ double vertical
        assert_eq!(box_glyph('║').unwrap(), b(D, D, N, N));
        // ╬ double cross
        assert_eq!(box_glyph('╬').unwrap(), b(D, D, D, D));
        // ┏ heavy down+right corner
        assert_eq!(box_glyph('┏').unwrap(), b(N, H, N, H));
        // ┡ up heavy + down light + right heavy (mixed tee)
        assert_eq!(box_glyph('┡').unwrap(), b(H, L, N, H));
        // ╭ rounded down+right
        let arc = box_glyph('╭').unwrap();
        assert!(arc.rounded && arc.down == L && arc.right == L);
        // ┄ light triple-dash horizontal
        assert_eq!(box_glyph('┄').unwrap().dash, Some(Dash::Triple));
        // ╱ forward diagonal
        assert_eq!(box_glyph('╱').unwrap().diagonal, Some(Diagonal::Forward));
    }

    /// Weight sanity: heavy and double variants differ from their light base.
    #[test]
    fn weights_are_distinct() {
        assert_ne!(box_glyph('─').unwrap(), box_glyph('━').unwrap()); // light vs heavy
        assert_ne!(box_glyph('─').unwrap(), box_glyph('═').unwrap()); // light vs double
    }
}
