//! Auto-contrast ink + same-hue accent derivation for a tab's pill color
//! (design: tab-colors, live-demo feedback items 3/4). Pure math, no IO, no
//! render-layer dependency — every color here is packed `0xRRGGBB`, the same
//! representation `SWATCHES`/`TabColorChoice::Color` already use, so callers
//! never round-trip through a render-layer `Rgb` just to ask "what ink goes
//! on this pill?".

/// Near-black ink for text/glyphs drawn over a LIGHT pill — the same value
/// `ember_render`'s `BG` background constant uses, not pure `#000`, so the
/// derived ink reads as part of this app's palette rather than a generic
/// web black.
pub const INK_DARK: u32 = 0x101010;
/// Near-white ink for text/glyphs drawn over a DARK pill — the same value
/// `ember_render`'s `FG` default text color uses, not pure `#fff`.
pub const INK_LIGHT: u32 = 0xcccccc;

/// WCAG relative-luminance threshold (design doc, item 3): a pill lighter
/// than this takes the dark ink, at or below it takes the light ink.
const LUMINANCE_THRESHOLD: f64 = 0.179;

/// WCAG non-text contrast minimum (design doc, item 4): below this, a
/// derived accent isn't legible enough against its own pill and the caller
/// should fall back to the plain ink color instead.
const MIN_ACCENT_CONTRAST: f64 = 3.0;

fn channel(c: u32, shift: u32) -> f64 {
    (((c >> shift) & 0xff) as f64) / 255.0
}

fn pack(r: f64, g: f64, b: f64) -> u32 {
    let clamp = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    (clamp(r) << 16) | (clamp(g) << 8) | clamp(b)
}

/// sRGB gamma -> linear (the WCAG / sRGB EOTF): `c/12.92` below the
/// `0.04045` knee, else `((c+0.055)/1.055)^2.4`.
fn linearize(c: f64) -> f64 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Linear -> sRGB gamma (the EOTF's inverse).
fn delinearize(c: f64) -> f64 {
    if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// WCAG relative luminance of a packed `0xRRGGBB` color: linearize each sRGB
/// channel, then `0.2126R + 0.7152G + 0.0722B`.
pub fn relative_luminance(c: u32) -> f64 {
    let r = linearize(channel(c, 16));
    let g = linearize(channel(c, 8));
    let b = linearize(channel(c, 0));
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// WCAG contrast ratio between two packed colors: `(L_lighter + 0.05) /
/// (L_darker + 0.05)`. Order-independent — always at least `1.0`.
pub fn contrast_ratio(a: u32, b: u32) -> f64 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Auto-contrast ink for text/glyphs drawn over a pill filled with `bg`
/// (design doc, item 3): [`INK_DARK`] when `bg` is light enough (relative
/// luminance strictly above [`LUMINANCE_THRESHOLD`]), else [`INK_LIGHT`].
pub fn ink_for(bg: u32) -> u32 {
    if relative_luminance(bg) > LUMINANCE_THRESHOLD {
        INK_DARK
    } else {
        INK_LIGHT
    }
}

/// sRGB (packed `0xRRGGBB`) -> Oklab `(L, a, b)`. Bjorn Ottosson's reference
/// formulas (<https://bottosson.github.io/posts/oklab/>): linearize sRGB,
/// project through the LMS-ish intermediate, cube-root, then a fixed 3x3
/// mix into Oklab.
fn srgb_to_oklab(c: u32) -> (f64, f64, f64) {
    let r = linearize(channel(c, 16));
    let g = linearize(channel(c, 8));
    let b = linearize(channel(c, 0));

    let l = 0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b;
    let m = 0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b;
    let s = 0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b;

    let l_ = l.cbrt();
    let m_ = m.cbrt();
    let s_ = s.cbrt();

    (
        0.2104542553 * l_ + 0.7936177850 * m_ - 0.0040720468 * s_,
        1.9779984951 * l_ - 2.4285922050 * m_ + 0.4505937099 * s_,
        0.0259040425 * l_ + 0.7827717125 * m_ - 0.8086757549 * s_,
    )
}

/// Oklab `(L, a, b)` -> sRGB, packed `0xRRGGBB`. Bjorn Ottosson's reference
/// inverse: the 3x3 mix back to the LMS-ish intermediate, cube, a fixed 3x3
/// mix into linear sRGB, then the sRGB EOTF's inverse. Out-of-gamut results
/// are clamped per-channel after conversion (design doc, item 4: "simple
/// channel clamp", not a proper gamut-mapping algorithm).
fn oklab_to_srgb(l: f64, a: f64, b: f64) -> u32 {
    let l_ = l + 0.3963377774 * a + 0.2158037573 * b;
    let m_ = l - 0.1055613458 * a - 0.0638541728 * b;
    let s_ = l - 0.0894841775 * a - 1.2914855480 * b;

    let l3 = l_ * l_ * l_;
    let m3 = m_ * m_ * m_;
    let s3 = s_ * s_ * s_;

    let r = 4.0767416621 * l3 - 3.3077115913 * m3 + 0.2309699292 * s3;
    let g = -1.2684380046 * l3 + 2.6097574011 * m3 - 0.3413193965 * s3;
    let b = -0.0041960863 * l3 - 0.7034186147 * m3 + 1.7076147010 * s3;

    pack(delinearize(r), delinearize(g), delinearize(b))
}

/// Oklab `(a, b)` -> OKLCH `(C, H)`; `H` in radians, `atan2` range
/// (`-pi..=pi`).
fn lab_to_lch(a: f64, b: f64) -> (f64, f64) {
    (a.hypot(b), b.atan2(a))
}

/// OKLCH `(C, H)` -> Oklab `(a, b)`; the inverse of [`lab_to_lch`].
fn lch_to_lab(c: f64, h: f64) -> (f64, f64) {
    (c * h.cos(), c * h.sin())
}

/// How far [`derive_accent`] shifts `L` toward the ink side (design doc,
/// item 4).
const ACCENT_LIGHTNESS_SHIFT: f64 = 0.35;
/// How much [`derive_accent`] scales chroma down (design doc, item 4).
const ACCENT_CHROMA_SCALE: f64 = 0.9;

/// Derive a same-hue accent from a tab's pill color `pill` (design doc, item
/// 4): convert to OKLCH, shift `L` by [`ACCENT_LIGHTNESS_SHIFT`] toward
/// whichever ink side `pill` contrasts against (darker if `pill` takes the
/// dark ink, lighter if it takes the light ink), scale chroma by
/// [`ACCENT_CHROMA_SCALE`], convert back (hue unchanged), clamped to gamut
/// by a simple per-channel clamp. If the result's contrast against `pill`
/// falls below the WCAG non-text minimum (3:1), falls back to the plain ink
/// color from [`ink_for`] instead — a same-hue accent that's actually
/// illegible against its own pill is worse than no hue at all.
pub fn derive_accent(pill: u32) -> u32 {
    let ink = ink_for(pill);
    let (l, a, b) = srgb_to_oklab(pill);
    let (c, h) = lab_to_lch(a, b);

    let shift = if ink == INK_DARK {
        -ACCENT_LIGHTNESS_SHIFT
    } else {
        ACCENT_LIGHTNESS_SHIFT
    };
    let new_l = (l + shift).clamp(0.0, 1.0);
    let new_c = c * ACCENT_CHROMA_SCALE;
    let (na, nb) = lch_to_lab(new_c, h);
    let accent = oklab_to_srgb(new_l, na, nb);

    if contrast_ratio(accent, pill) < MIN_ACCENT_CONTRAST {
        ink
    } else {
        accent
    }
}

/// Blend `c` toward `toward` by `t` (`0.0` = `c` unchanged, `1.0` = fully
/// `toward`), per sRGB channel — the redesign's inactive-tab pill treatment
/// (design doc, item 2): a colored inactive tab blends its color toward the
/// strip background rather than rendering at full strength, so the active
/// tab still reads as clearly distinct.
pub fn blend_toward(c: u32, toward: u32, t: f64) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |shift: u32| {
        let a = channel(c, shift);
        let b = channel(toward, shift);
        a + (b - a) * t
    };
    pack(mix(16), mix(8), mix(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tabcolor::SWATCHES;

    // --- relative_luminance / ink_for (item 3) ---------------------------

    #[test]
    fn pure_white_is_dark_ink() {
        assert_eq!(relative_luminance(0xffffff), 1.0);
        assert_eq!(ink_for(0xffffff), INK_DARK);
    }

    #[test]
    fn pure_black_is_light_ink() {
        assert_eq!(relative_luminance(0x000000), 0.0);
        assert_eq!(ink_for(0x000000), INK_LIGHT);
    }

    #[test]
    fn threshold_crossover_both_sides() {
        // Two grays straddling the L=0.179 crossover point (solved from the
        // sRGB EOTF: v ~= 0.46 gray gives L ~= 0.179). 0x777777's L is just
        // above the threshold, 0x707070's just below.
        let above = relative_luminance(0x777777);
        let below = relative_luminance(0x707070);
        assert!(above > LUMINANCE_THRESHOLD, "expected {above} > 0.179");
        assert!(below <= LUMINANCE_THRESHOLD, "expected {below} <= 0.179");
        assert_eq!(ink_for(0x777777), INK_DARK);
        assert_eq!(ink_for(0x707070), INK_LIGHT);
    }

    #[test]
    fn every_swatch_ink_meets_wcag_text_contrast() {
        // Design doc, item 3: each curated SWATCHES color must clear 4.5:1
        // against whichever ink `ink_for` picks for it.
        for &c in SWATCHES.iter() {
            let ink = ink_for(c);
            let ratio = contrast_ratio(c, ink);
            assert!(
                ratio >= 4.5,
                "swatch {c:#08x} only gets {ratio:.2}:1 against ink {ink:#08x}"
            );
        }
    }

    // --- Oklab/OKLCH round-trip sanity -----------------------------------

    #[test]
    fn oklab_round_trip_is_stable_for_swatches() {
        for &c in SWATCHES.iter() {
            let (l, a, b) = srgb_to_oklab(c);
            let back = oklab_to_srgb(l, a, b);
            let (r0, g0, b0) = (
                ((c >> 16) & 0xff) as i32,
                ((c >> 8) & 0xff) as i32,
                (c & 0xff) as i32,
            );
            let (r1, g1, b1) = (
                ((back >> 16) & 0xff) as i32,
                ((back >> 8) & 0xff) as i32,
                (back & 0xff) as i32,
            );
            // Rounding through cube roots/cubes: allow a couple 8-bit steps.
            assert!((r0 - r1).abs() <= 2, "{c:#08x} r round-trip: {r0} vs {r1}");
            assert!((g0 - g1).abs() <= 2, "{c:#08x} g round-trip: {g0} vs {g1}");
            assert!((b0 - b1).abs() <= 2, "{c:#08x} b round-trip: {b0} vs {b1}");
        }
    }

    // --- derive_accent (item 4) -------------------------------------------

    #[test]
    fn hue_is_preserved_when_the_shift_stays_in_gamut() {
        // Chosen (by search) so the lightness-shifted, chroma-scaled OKLCH
        // point still lands inside the sRGB gamut — no channel clamp fires,
        // isolating the OKLCH round-trip math itself from the "simple
        // channel clamp" gamut approximation the design doc accepts for
        // colors that DO clip (several curated SWATCHES clip enough to
        // drift several degrees; that's the accepted tradeoff of a simple
        // clamp, not a bug in the hue math this test targets).
        let pill = 0x204f89;
        let accent = derive_accent(pill);
        assert_ne!(accent, ink_for(pill), "fallback shouldn't trigger here");
        let (_, a0, b0) = srgb_to_oklab(pill);
        let (_, h0) = lab_to_lch(a0, b0);
        let (_, a1, b1) = srgb_to_oklab(accent);
        let (_, h1) = lab_to_lch(a1, b1);
        let mut diff = (h0 - h1).abs().to_degrees();
        if diff > 180.0 {
            diff = 360.0 - diff;
        }
        assert!(diff <= 1.0, "hue drifted {diff:.3} degrees");
    }

    #[test]
    fn fallback_triggers_on_a_pathological_base() {
        // Pure black: `ink_for` picks the LIGHT ink (lighten), but a fixed
        // 0.35 Oklab lightness step from L=0 lands at a mid-dark gray whose
        // WCAG luminance is still far too close to black's own — WCAG
        // luminance isn't perceptually uniform the way Oklab L is, so a
        // perceptually-even step near black buys little WCAG contrast —
        // under the 3:1 non-text minimum, so the fallback kicks in.
        let base = 0x000000;
        assert_eq!(derive_accent(base), ink_for(base));
    }

    #[test]
    fn every_swatch_accent_meets_non_text_contrast() {
        for &c in SWATCHES.iter() {
            let accent = derive_accent(c);
            let ratio = contrast_ratio(accent, c);
            assert!(
                ratio >= 3.0,
                "swatch {c:#08x}'s accent {accent:#08x} only gets {ratio:.2}:1"
            );
        }
    }

    // --- blend_toward (item 2) --------------------------------------------

    #[test]
    fn blend_toward_endpoints() {
        assert_eq!(blend_toward(0xff0000, 0x001122, 0.0), 0xff0000);
        assert_eq!(blend_toward(0xff0000, 0x001122, 1.0), 0x001122);
    }

    #[test]
    fn blend_toward_is_between_the_two_colors() {
        let mid = blend_toward(0xff0000, 0x000000, 0.5);
        // Halfway to black: red channel roughly halves, others stay ~0.
        let r = (mid >> 16) & 0xff;
        assert!((100..=140).contains(&r), "r={r:#04x}");
    }
}
