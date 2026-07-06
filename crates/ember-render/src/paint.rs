//! Shared draw helpers — the single source of truth for turning a neutral grid +
//! overlays into glyphs and quads. Used by both the windowed [`crate::renderer`]
//! and the headless screenshot path ([`crate::headless`]) so they render
//! identically. Stateless free functions over the renderer's colors/metrics; the
//! `Renderer` struct + GPU plumbing live in `renderer.rs`.

use ember_core::{MarkStatus, Rect, Rgb, RowKind, SettingsRowView};
use glyphon::{Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping};

use crate::grid_model::GridModel;
use crate::quads::srgb_to_linear;
use crate::renderer::{
    ABOUT_TITLE_LINE, ACCENT, AMBER, AboutInfo, BG, BackdropParams, CELL_HEIGHT, FG, HELP_PAD,
    LINE_HEIGHT, PAD, TabLabel,
};
use crate::selection::Selection;

/// Deep, cooling ember the sparks fade toward as they rise.
const EMBER_DARK: Rgb = Rgb::new(0x7a, 0x1a, 0x05);

/// Push the campfire backdrop (a warm vertical gradient + a darkening legibility
/// scrim) into the alpha-blended quad list, drawn behind the cells. Both are
/// opt-in via [`BackdropParams`]; the sparks are a separate additive pass.
pub(crate) fn push_backdrop(
    out: &mut Vec<([f32; 4], [f32; 4])>,
    params: &BackdropParams,
    logical_w: f32,
    logical_h: f32,
    sf: f32,
) {
    if params.gradient {
        // Dark warm vertical gradient: near-black at the top → ember-warm at the
        // bottom (where the "fire" is), as opaque horizontal bands.
        const TOP: Rgb = Rgb::new(0x0d, 0x08, 0x06);
        const BOT: Rgb = Rgb::new(0x40, 0x19, 0x07);
        const BANDS: usize = 48;
        let band_h = logical_h / BANDS as f32;
        for b in 0..BANDS {
            let f = b as f32 / (BANDS as f32 - 1.0); // 0 = top, 1 = bottom
            let y = f * logical_h;
            let color = lerp_rgb(TOP, BOT, f);
            // Overlap by 1px to avoid seams between bands.
            out.push((
                scaled(0.0, y, logical_w, band_h + 1.0, sf),
                lin_rgba(color, 1.0),
            ));
        }
    }
    if params.scrim > 0.0 {
        out.push((
            scaled(0.0, 0.0, logical_w, logical_h, sf),
            lin_rgba(Rgb::new(0, 0, 0), params.scrim.clamp(0.0, 1.0)),
        ));
    }
}

/// Peak alpha of the visual-bell wash — subtle by design (a fire flaring up, not a
/// blinding strobe).
const BELL_WASH_MAX: f32 = 0.16;

/// Push the visual-bell flash: a warm amber full-surface tint at `intensity`
/// (`0..1`) over the panes. No-op at 0.
pub(crate) fn bell_wash(
    out: &mut Vec<([f32; 4], [f32; 4])>,
    intensity: f32,
    logical_w: f32,
    logical_h: f32,
    sf: f32,
) {
    let a = intensity.clamp(0.0, 1.0) * BELL_WASH_MAX;
    if a <= 0.0 {
        return;
    }
    out.push((
        scaled(0.0, 0.0, logical_w, logical_h, sf),
        lin_rgba(AMBER, a),
    ));
}

/// Compute the drifting ember-spark instances for the **additive** pass:
/// `(rect_px, linear_rgba)` round glows rising from the bottom with lateral sway,
/// flicker, and a fade-in/out over each spark's looping lifetime. Procedural +
/// stateless — driven by `t` (seconds) alone, so it animates with no stored state
/// and renders identically windowed + headless.
pub(crate) fn spark_quads(
    density: f32,
    t: f32,
    logical_w: f32,
    logical_h: f32,
    sf: f32,
) -> Vec<([f32; 4], [f32; 4])> {
    use std::f32::consts::PI;
    let n = ((50.0 * density).round() as i32).clamp(0, 240) as usize;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let fi = i as f32;
        let hash = |a: f32, b: f32| {
            let s = ((fi * a + b).sin() * 43758.547).abs();
            s - s.floor()
        };
        let seed = hash(12.9898, 4.1);
        // Stratify the phase offset evenly across the cycle (`i/n` + a little
        // jitter) and take the lifetime from an *independent* seed. If the offset
        // and rate both came from one seed, similar-seeded sparks move in lockstep
        // and bunch up — embers emitted in bursts with dead gaps. Evenly-spread
        // offsets keep emission continuous.
        let offset = (fi + 0.5 * seed) / n as f32;
        let life = 2.5 + hash(78.233, 1.7) * 2.5;
        let phase = ((t / life) + offset).fract(); // 0 = born at bottom, 1 = gone at top
        let base_x = hash(37.719, 2.3) * logical_w;
        let x = base_x + (t * 0.6 + fi * 1.7).sin() * (10.0 + seed * 14.0);
        // Rise from just below the bottom edge to above the top.
        let y = logical_h + 12.0 - phase * (logical_h + 24.0);
        // Hot amber near the fire, cooling to deep ember as it rises.
        let color = if phase < 0.5 {
            lerp_rgb(AMBER, ACCENT, phase * 2.0)
        } else {
            lerp_rgb(ACCENT, EMBER_DARK, (phase - 0.5) * 2.0)
        };
        let flicker = 0.8 + 0.2 * (t * (8.0 + seed * 6.0) + fi).sin();
        let alpha = (PI * phase).sin().max(0.0) * 0.85 * flicker;
        let size = 2.0 + seed * 4.0;
        out.push((
            scaled(x - size * 0.5, y - size * 0.5, size, size, sf),
            lin_rgba(color, alpha),
        ));
    }
    out
}
/// Build the shared `FontSystem`, dropping known-degenerate faces. macOS ships
/// GB18030Bitmap, a legacy bitmap-only CJK font that fontdb classifies as
/// monospaced — cosmic-text's monospace-preferring fallback then picks it for
/// CJK, but its metrics are degenerate (infinite advance, NaN baseline) and one
/// such glyph corrupts the entire frame's vertex stream. Excising it makes
/// fallback land on a real CJK face (PingFang).
///
/// Also repoints the generic `Family::Monospace` at a font that actually
/// exists. cosmic-text hardcodes "Noto Sans Mono" as the monospace default,
/// which isn't installed on stock macOS — and when the default monospace
/// family doesn't resolve, cosmic-text's fallback iterator takes a
/// pathological path for EVERY word shaped with `Family::Monospace`: it
/// enumerates every face in the database, checks each for monospacedness,
/// and coverage-tests every monospace face against the word (the
/// font-family-switch hang was sampled almost entirely inside this
/// enumeration). With a resolvable default, the same code path fast-returns
/// after one coverage check.
pub(crate) fn new_font_system() -> FontSystem {
    let mut fs = FontSystem::new();
    let bad: Vec<glyphon::fontdb::ID> = fs
        .db()
        .faces()
        .filter(|f| f.post_script_name == "GB18030Bitmap")
        .map(|f| f.id)
        .collect();
    for id in bad {
        fs.db_mut().remove_face(id);
    }
    let mono_resolves = fs
        .db()
        .query(&glyphon::fontdb::Query {
            families: &[glyphon::fontdb::Family::Monospace],
            ..Default::default()
        })
        .is_some();
    if !mono_resolves {
        // Ordered by platform: macOS ships Menlo; most Linux distros have one
        // of the next three; Windows has Consolas.
        for name in [
            "Menlo",
            "Noto Sans Mono",
            "DejaVu Sans Mono",
            "Liberation Mono",
            "Consolas",
        ] {
            let found = fs
                .db()
                .query(&glyphon::fontdb::Query {
                    families: &[glyphon::fontdb::Family::Name(name)],
                    ..Default::default()
                })
                .is_some();
            if found {
                fs.db_mut().set_monospace_family(name);
                break;
            }
        }
    }
    fs
}

/// Resolve a config family name to a cosmic-text `Family` (empty/none → the
/// platform monospace default, which also catches an unresolvable name).
pub(crate) fn family_of(name: Option<&str>) -> Family<'_> {
    match name {
        Some(n) if !n.is_empty() => Family::Name(n),
        _ => Family::Monospace,
    }
}

/// Measure the monospace advance for `size`/`family`, so background quads line
/// up with the glyphs glyphon flows. Re-measured whenever the font zooms.
pub(crate) fn measure_cell_width(font_system: &mut FontSystem, size: f32, family: Family) -> f32 {
    let line_height = line_height_for(size);
    let mut probe = Buffer::new(font_system, Metrics::new(size, line_height));
    probe.set_text(
        font_system,
        "MMMMMMMMMM",
        &Attrs::new().family(family),
        Shaping::Advanced,
        None,
    );
    probe.shape_until_scroll(font_system, false);
    if let Some(run) = probe.layout_runs().next() {
        let glyphs = run.glyphs;
        if glyphs.len() >= 2 {
            let span = glyphs[glyphs.len() - 1].x - glyphs[0].x;
            return span / (glyphs.len() - 1) as f32;
        }
    }
    size * 0.6
}

/// Line height (cell height) for a font size — the fixed 1.25 ratio the default
/// 12pt/15px pair uses, rounded so cell rows land on whole pixels.
pub(crate) fn line_height_for(size: f32) -> f32 {
    (size * 1.25).round().max(1.0)
}

/// Linear interpolation between two sRGB colors (`t` clamped to `[0,1]`).
pub(crate) fn lerp_rgb(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Rgb::new(mix(a.r, b.r), mix(a.g, b.g), mix(a.b, b.b))
}

/// sRGB `Rgb` + alpha → linear RGBA for the sRGB surface target.
pub(crate) fn lin_rgba(c: Rgb, a: f32) -> [f32; 4] {
    [
        srgb_to_linear(c.r),
        srgb_to_linear(c.g),
        srgb_to_linear(c.b),
        a,
    ]
}

// --- Shared draw logic (windowed Renderer + headless screenshot) -------------
// These free fns are the single source of truth for how a grid becomes glyphs +
// quads, so the headless PNG matches what ships on screen pixel-for-pixel.

/// SGR 2 (dim): scale the fg toward the background (2/3 keeps ANSI colors apart).
pub(crate) fn dim_rgb(c: ember_core::Rgb) -> ember_core::Rgb {
    ember_core::Rgb {
        r: (c.r as u16 * 2 / 3) as u8,
        g: (c.g as u16 * 2 / 3) as u8,
        b: (c.b as u16 * 2 / 3) as u8,
    }
}

/// Shape one grid's rows into `buffer` as per-cell styled runs (one logical
/// line per grid row): fg color + bold/italic/dim. Underline/strikeout/overline
/// are quads (see [`grid_quads`]) — cosmic-text doesn't draw decorations.
#[allow(clippy::too_many_arguments)]
pub(crate) fn shape_grid(
    font_system: &mut FontSystem,
    buffer: &mut Buffer,
    grid: &GridModel,
    size: f32,
    line_height: f32,
    cw: f32,
    family: Family,
) {
    use ember_core::Attrs as CellAttrs;
    // Snap every glyph's advance to a multiple of the cell width — a terminal
    // is a grid, so a symbol/emoji/CJK glyph whose natural advance isn't one
    // cell must NOT shift the cells after it (the spinner-jitter bug). cosmic-
    // text rounds each advance to `round(advance/cw)*cw`: narrow symbols → 1
    // cell, wide glyphs → 2.
    buffer.set_monospace_width(font_system, Some(cw));
    // One grid row = one visual line, always. With the default word-wrap a row
    // whose shaped width overruns the pane (wide CJK/emoji advances aren't
    // exactly 2·cw) soft-wraps, shifting every row below it and clipping the
    // tail — the grid is the layout; the shaper must not re-flow it.
    buffer.set_wrap(font_system, glyphon::Wrap::None);
    // Nor may it scroll: a prior overflow (taller fallback line boxes) can
    // leave a sticky scroll offset that shifts the whole grid up.
    buffer.set_scroll(glyphon::cosmic_text::Scroll::default());
    let lines = grid.dims.screen_lines;
    let mut spans: Vec<(String, Color, CellAttrs)> = Vec::new();
    for row in 0..lines {
        for (text, fg, attrs) in grid.row_runs(row) {
            let fg = if attrs.contains(CellAttrs::DIM) {
                dim_rgb(fg)
            } else {
                fg
            };
            spans.push((text, Color::rgb(fg.r, fg.g, fg.b), attrs));
        }
        if row + 1 < lines {
            spans.push((
                "\n".to_string(),
                Color::rgb(FG.r, FG.g, FG.b),
                CellAttrs::empty(),
            ));
        }
    }
    let base = Attrs::new().family(family);
    buffer.set_rich_text(
        font_system,
        spans.iter().map(|(t, c, cell_attrs)| {
            // Pin every span's line box to the grid metrics: CJK/emoji
            // fallback fonts carry taller line heights that would otherwise
            // stretch their row and push all later rows off the cell grid.
            let mut a = Attrs::new()
                .family(family)
                .color(*c)
                .metrics(Metrics::new(size, line_height));
            if cell_attrs.contains(CellAttrs::BOLD) {
                a = a.weight(glyphon::Weight::BOLD);
            }
            if cell_attrs.contains(CellAttrs::ITALIC) {
                a = a.style(glyphon::Style::Italic);
            }
            (t.as_str(), a)
        }),
        &base,
        Shaping::Advanced,
        None,
    );
    buffer.shape_until_scroll(font_system, false);
}

/// Append a grid's bg fills + (when focused) cursor + (when focused && split)
/// focus border, for a pane at logical `rect`, scaled to physical px by `sf`.
#[allow(clippy::too_many_arguments)] // a draw helper: grid + geometry + flags + out
pub(crate) fn grid_quads(
    grid: &GridModel,
    rect: Rect,
    cw: f32,
    ch: f32,
    sf: f32,
    focused: bool,
    split: bool,
    out: &mut Vec<([f32; 4], [f32; 4])>,
) {
    use ember_core::Attrs as CellAttrs;
    let ox = rect.x as f32;
    let oy = rect.y as f32;
    const DECOR: CellAttrs = CellAttrs::UNDERLINE
        .union(CellAttrs::STRIKEOUT)
        .union(CellAttrs::OVERLINE);
    for row in 0..grid.dims.screen_lines {
        for col in 0..grid.dims.columns {
            if let Some(cell) = grid.cell(row, col) {
                let style = grid.style_of(cell.style);
                let x = ox + col as f32 * cw;
                let y = oy + row as f32 * ch;
                if style.bg != BG {
                    out.push((scaled(x, y, cw, ch, sf), lin_rgba(style.bg, 1.0)));
                }
                // Line decorations (same fg as the glyph, so draw order vs text
                // is invisible). Hidden cells get neither glyph nor lines.
                if style.attrs.intersects(DECOR) && !style.attrs.contains(CellAttrs::HIDDEN) {
                    let fg = if style.attrs.contains(CellAttrs::DIM) {
                        dim_rgb(style.fg)
                    } else {
                        style.fg
                    };
                    let line = lin_rgba(fg, 1.0);
                    if style.attrs.contains(CellAttrs::UNDERLINE) {
                        out.push((scaled(x, y + ch - 1.5, cw, 1.0, sf), line));
                    }
                    if style.attrs.contains(CellAttrs::STRIKEOUT) {
                        out.push((scaled(x, y + ch * 0.5, cw, 1.0, sf), line));
                    }
                    if style.attrs.contains(CellAttrs::OVERLINE) {
                        out.push((scaled(x, y + 0.5, cw, 1.0, sf), line));
                    }
                }
            }
        }
    }
    if focused {
        let cur = grid.cursor;
        if cur.visible && cur.shape != ember_core::CursorShape::Hidden {
            // A wide glyph's block cursor spans both columns; on a spacer the
            // cursor snaps back to its leader (contract: leader owns the pair).
            let (col, wide) = match grid.cell(cur.row, cur.col) {
                Some(c) if matches!(c.content, ember_core::CellContent::WideSpacer) => {
                    (cur.col.saturating_sub(1), true)
                }
                Some(c) => (cur.col, c.wide),
                None => (cur.col, false),
            };
            let cw_cursor = if wide { cw * 2.0 } else { cw };
            let x = ox + col as f32 * cw;
            let y = oy + cur.row as f32 * ch;
            // Shape follows DECSCUSR (vim mode-dependent cursors): a beam or
            // underline is a thin solid bar; the block stays translucent so
            // the glyph underneath remains readable.
            let (rect, alpha) = match cur.shape {
                ember_core::CursorShape::Beam => (scaled(x, y, 2.0, ch, sf), 1.0),
                ember_core::CursorShape::Underline => {
                    (scaled(x, y + ch - 2.0, cw_cursor, 2.0, sf), 1.0)
                }
                _ => (scaled(x, y, cw_cursor, ch, sf), 0.5),
            };
            out.push((rect, lin_rgba(FG, alpha)));
        }
        if split {
            push_border(out, rect, ACCENT, sf);
        }
    }
    // Shell-integration gutter: a colored bar at each command's prompt line —
    // green = exit 0, red = non-zero, amber = still running, blue = a manual
    // mark (OSC 1337 SetMark, not tied to a command). Drawn in the left pad
    // so it doesn't overlap text.
    for &(row, status) in &grid.marks {
        if row < grid.dims.screen_lines {
            let color = match status {
                MarkStatus::Ok => GUTTER_OK,
                MarkStatus::Fail => GUTTER_FAIL,
                MarkStatus::Running => GUTTER_RUN,
                MarkStatus::Manual => GUTTER_MANUAL,
                _ => GUTTER_RUN,
            };
            // Inside the pane's own left edge (not `ox - 3.5`, which reaches
            // into the padding/divider — and, for a right-hand split pane, the
            // neighbor). Quads aren't clipped per-pane, so keep it in-bounds.
            out.push((
                scaled(ox + 0.5, oy + row as f32 * ch + 1.0, 2.5, ch - 2.0, sf),
                lin_rgba(color, 1.0),
            ));
        }
    }
}

/// Underline quads for detected links: a dimmed 1px underline for every link
/// span (always visible — the affordance is not a hidden mode), and a
/// brighter 2px one for the hovered link. `origin` is the pane's inner
/// top-left in logical px.
pub(crate) fn link_quads(
    spans: &[crate::grid_model::LinkSpan],
    hovered: Option<u32>,
    origin: (f32, f32),
    cw: f32,
    ch: f32,
    sf: f32,
    out: &mut Vec<([f32; 4], [f32; 4])>,
) {
    for s in spans {
        let hovered = hovered == Some(s.link_id);
        let x = origin.0 + s.cols.start as f32 * cw;
        let w = (s.cols.end - s.cols.start) as f32 * cw;
        let (y, h, color, alpha) = if hovered {
            (origin.1 + (s.row + 1) as f32 * ch - 2.0, 2.0, ACCENT, 0.9)
        } else {
            (origin.1 + (s.row + 1) as f32 * ch - 2.0, 1.0, FG, 0.35)
        };
        out.push((scaled(x, y, w, h, sf), lin_rgba(color, alpha)));
    }
}

/// Shell-integration gutter mark colors (exit 0 / non-zero / running / manual).
const GUTTER_OK: Rgb = Rgb::new(0x3f, 0xb9, 0x50);
const GUTTER_FAIL: Rgb = Rgb::new(0xe5, 0x48, 0x4d);
const GUTTER_RUN: Rgb = Rgb::new(0xd0, 0x90, 0x30);
const GUTTER_MANUAL: Rgb = Rgb::new(0x56, 0x9c, 0xd6);

/// Translucent selection-highlight color (a calm blue, drawn over the cell bg and
/// under the glyphs so selected text stays readable).
const SELECT_BG: Rgb = Rgb::new(0x3a, 0x66, 0xb0);

/// Append the selection-highlight quads for `selection` over the pane at `rect`
/// (one quad per selected row span). Drawn after the bg fills, before text.
pub(crate) fn selection_quads(
    grid: &GridModel,
    selection: &Selection,
    rect: Rect,
    cw: f32,
    ch: f32,
    sf: f32,
    out: &mut Vec<([f32; 4], [f32; 4])>,
) {
    let ox = rect.x as f32;
    let oy = rect.y as f32;
    for row in 0..grid.dims.screen_lines {
        if let Some((c0, c1)) = selection.row_span(grid, row) {
            let x = ox + c0 as f32 * cw;
            let w = (c1 - c0 + 1) as f32 * cw;
            let y = oy + row as f32 * ch;
            out.push((scaled(x, y, w, ch, sf), lin_rgba(SELECT_BG, 0.45)));
        }
    }
}

/// Draw the visual-split drop-zone preview over a pane `rect`: a translucent
/// ember-tinted overlay on the region the NEW pane would occupy (right half if
/// `horizontal` = side-by-side, else bottom half), with a bright divider line at
/// `ratio` (the existing pane's fraction). Held Ctrl+Opt + hover; click commits.
pub(crate) fn split_preview(
    rect: Rect,
    horizontal: bool,
    ratio: f32,
    sf: f32,
    out: &mut Vec<([f32; 4], [f32; 4])>,
) {
    let (x, y, w, h) = (
        rect.x as f32,
        rect.y as f32,
        rect.width as f32,
        rect.height as f32,
    );
    let r = ratio.clamp(0.05, 0.95);
    if horizontal {
        let dx = x + w * r;
        out.push((scaled(dx, y, w * (1.0 - r), h, sf), lin_rgba(ACCENT, 0.20)));
        out.push((scaled(dx - 1.0, y, 2.0, h, sf), lin_rgba(ACCENT, 0.9)));
    } else {
        let dy = y + h * r;
        out.push((scaled(x, dy, w, h * (1.0 - r), sf), lin_rgba(ACCENT, 0.20)));
        out.push((scaled(x, dy - 1.0, w, 2.0, sf), lin_rgba(ACCENT, 0.9)));
    }
}

/// Background of the tab strip (a touch lighter than the terminal, iTerm-style).
const STRIP_BG: Rgb = Rgb::new(0x1b, 0x1b, 0x1b);
/// Fill of the active tab button.
const TAB_ACTIVE: Rgb = Rgb::new(0x3a, 0x3a, 0x3d);
/// Fill of a hovered *inactive* tab — a subtle lift between [`STRIP_BG`] and
/// [`TAB_ACTIVE`] (iTerm-style), no accent ring so it reads as hover, not select.
const TAB_HOVER: Rgb = Rgb::new(0x2b, 0x2b, 0x2e);
/// Width (in columns) of each trailing tab-strip utility button ("+", "?", "⚙").
pub(crate) const BTN_COLS: usize = 3;
/// Columns reserved at the left of a *hovered* tab for the "✕ " close affordance.
/// [`build_tabs`] draws it and [`Renderer::tab_hit`](crate::Renderer::tab_hit)
/// must mirror this to route a click there to a close.
pub(crate) const CLOSE_COLS: usize = 2;

/// Center `s` in a field `width` **display columns** wide (truncating with `…`
/// if too long). Uses Unicode display width — a CJK title char is 2 columns —
/// so wide tab titles stay aligned with their column-based button quads instead
/// of overflowing (a raw `chars().count()` under-measured them).
fn center(s: &str, width: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if width == 0 {
        return String::new();
    }
    let n = UnicodeWidthStr::width(s);
    if n >= width {
        if width == 1 {
            return "…".to_string();
        }
        // Keep whole chars until one more would exceed `width - 1` columns,
        // leaving a column for the ellipsis.
        let mut keep = String::new();
        let mut used = 0usize;
        for ch in s.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + w > width - 1 {
                break;
            }
            keep.push(ch);
            used += w;
        }
        // Pad if the truncation landed mid-way (dropping a wide char) so the
        // field stays exactly `width` columns.
        let pad = width - 1 - used;
        return format!("{keep}{}…", " ".repeat(pad));
    }
    let total = width - n;
    let left = total / 2;
    let right = total - left;
    format!("{}{}{}", " ".repeat(left), s, " ".repeat(right))
}

/// Build the tab strip (iTerm-style): a full-width bar with the trailing utility
/// buttons "+" (new tab), "?" (shortcuts), "⚙" (settings) — *always shown* so those
/// controls are discoverable even with a single tab (design §1 discoverability
/// tenet). With multiple tabs it also draws equal-width tab buttons (active one
/// lighter with an Ember-orange underline + `⌘N` hint); with one tab the tab area
/// is just an empty toolbar. Quads → `out`; the concatenated label line shapes into
/// `chrome`. All geometry is logical px, scaled by `sf`.
/// Push a rounded tab "pill" (iTerm-style): a fill inset from the strip edges,
/// with an optional 1px accent ring behind it. `x`/`w` are the tab segment in
/// logical px; `cw` gives a small horizontal gap so adjacent pills don't touch.
/// Geometry of a tab pill for segment `x`/`w`: `(left, top, width, height,
/// radius)` in logical px. Single source of truth so the fill, the accent ring,
/// and the hover "✕" (centered in the left cap) all agree.
fn pill_geom(x: f32, w: f32, strip_h: f32, cw: f32) -> (f32, f32, f32, f32, f32) {
    let inset_y = 3.0;
    let gap_x = (cw * 0.4).clamp(3.0, 8.0);
    let px = x + gap_x;
    let pw = (w - 2.0 * gap_x).max(1.0);
    let ph = (strip_h - 2.0 * inset_y).max(1.0);
    let radius = (ph * 0.5).min(9.0);
    (px, inset_y, pw, ph, radius)
}

/// Center of a pill's left rounded cap (logical px) — where the hover "✕" sits so
/// an inscribed circle matches the corner. Mirrors [`pill_geom`].
fn pill_cap_center(x: f32, w: f32, strip_h: f32, cw: f32) -> (f32, f32) {
    let (px, inset_y, _pw, ph, radius) = pill_geom(x, w, strip_h, cw);
    (px + radius, inset_y + ph * 0.5)
}

#[allow(clippy::too_many_arguments)]
fn push_pill(
    rounded: &mut Vec<([f32; 4], [f32; 4], f32)>,
    x: f32,
    w: f32,
    strip_h: f32,
    cw: f32,
    sf: f32,
    fill: Rgb,
    ring: Option<Rgb>,
) {
    let (px, inset_y, pw, ph, radius) = pill_geom(x, w, strip_h, cw);
    if let Some(c) = ring {
        // A slightly larger rounded rect behind the fill = a 1px accent ring.
        rounded.push((
            scaled(px - 1.0, inset_y - 1.0, pw + 2.0, ph + 2.0, sf),
            lin_rgba(c, 0.85),
            (radius + 1.0) * sf,
        ));
    }
    rounded.push((
        scaled(px, inset_y, pw, ph, sf),
        lin_rgba(fill, 1.0),
        radius * sf,
    ));
}

/// Shaping cache for the tab strip. `build_tabs` runs every frame,
/// but the cosmic-text work (`set_rich_text` → `shape_until_scroll` → font
/// fallback) is the expensive part — re-shaping it per redraw was 66% sustained
/// CPU under output storms (11/19 cpu_resource.diag samples). The label spans +
/// strip geometry fully determine the shaped result, so keep the last inputs
/// and re-shape only when they change. Quads are rebuilt each call: pure math,
/// cheap, and they carry the per-frame state (drag lift, bell dots).
#[derive(Default)]
pub(crate) struct TabsCache {
    /// Last-shaped label spans as `(text, packed rgba)`.
    spans: Vec<(String, u32)>,
    /// Buffer width the spans were shaped at (`f32` bits; resize/display move).
    logical_w: u32,
    /// Cell width behind the column math (`f32` bits; changes on zoom).
    cw: u32,
    /// Cell width the "✕" close glyph was last shaped at, if ever.
    close_cw: Option<u32>,
}

impl TabsCache {
    /// Whether `spans` shaped at `logical_w`/`cw` would differ from what the
    /// chrome buffer currently holds. A `Default` cache is always dirty (empty
    /// spans never match a real strip, which always has the +/?/⚙ buttons).
    fn is_dirty(&self, spans: &[(String, Color)], lw_bits: u32, cw_bits: u32) -> bool {
        self.logical_w != lw_bits
            || self.cw != cw_bits
            || self.spans.len() != spans.len()
            || self
                .spans
                .iter()
                .zip(spans)
                .any(|((ct, cc), (t, c))| *cc != c.0 || ct != t)
    }

    /// Record what was just shaped.
    fn store(&mut self, spans: Vec<(String, Color)>, lw_bits: u32, cw_bits: u32) {
        self.spans = spans.into_iter().map(|(t, c)| (t, c.0)).collect();
        self.logical_w = lw_bits;
        self.cw = cw_bits;
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_tabs(
    font_system: &mut FontSystem,
    chrome: &mut Buffer,
    close_buf: &mut Buffer,
    cache: &mut TabsCache,
    tabs: &[TabLabel],
    drag: Option<(usize, f32)>,
    hovered: Option<usize>,
    cw: f32,
    logical_w: f32,
    sf: f32,
    out: &mut Vec<([f32; 4], [f32; 4])>,
    rounded: &mut Vec<([f32; 4], [f32; 4], f32)>,
) -> Option<f32> {
    let strip_h = CELL_HEIGHT + 2.0 * PAD;
    // Center-x of the hovered tab's "✕" (in the pill's left cap); `None` when no
    // tab is hovered. The caller positions `close_buf` there.
    let mut close_cx: Option<f32> = None;
    // Full-width strip background.
    out.push((
        scaled(0.0, 0.0, logical_w, strip_h, sf),
        lin_rgba(STRIP_BG, 1.0),
    ));

    // Work in integer columns so quads and (monospace) text stay aligned. The three
    // trailing utility buttons each reserve `BTN_COLS`.
    let total_cols = (logical_w / cw).floor() as usize;
    let plus_cols = BTN_COLS.min(total_cols);
    let help_cols = BTN_COLS.min(total_cols.saturating_sub(plus_cols));
    let gear_cols = BTN_COLS.min(total_cols.saturating_sub(plus_cols + help_cols));
    let tab_cols = total_cols.saturating_sub(plus_cols + help_cols + gear_cols);

    let base = Attrs::new().family(Family::Monospace);
    let mut spans: Vec<(String, Color)> = Vec::new();
    let n = tabs.len();
    let drag_slot = drag.map(|(s, _)| s);
    if n > 1 {
        let seg = tab_cols / n;
        let mut col = 0usize;
        for (i, tab) in tabs.iter().enumerate() {
            // Last tab absorbs any leftover columns so the row fills exactly.
            let width = if i == n - 1 { tab_cols - col } else { seg };
            let x = col as f32 * cw;
            let w = width as f32 * cw;
            let dragging_this = drag_slot == Some(i);
            if dragging_this {
                // The grabbed tab's slot is a recessed "gap" (darker) — the lifted
                // copy floats over it at the cursor (drawn after the loop).
                out.push((
                    scaled(x, 0.0, w, strip_h, sf),
                    lin_rgba(Rgb::new(0x0c, 0x0c, 0x0c), 1.0),
                ));
            } else if tab.editing {
                // Inline rename: an accent-ringed rounded pill so it reads as an
                // editable field.
                push_pill(rounded, x, w, strip_h, cw, sf, TAB_ACTIVE, Some(ACCENT));
            } else if tab.active {
                // iTerm-style: an inset rounded pill with a subtle ember ring.
                push_pill(rounded, x, w, strip_h, cw, sf, TAB_ACTIVE, Some(ACCENT));
            } else if hovered == Some(i) {
                // Hover lift on an inactive tab: a subtle fill, no ring.
                push_pill(rounded, x, w, strip_h, cw, sf, TAB_HOVER, None);
            }
            // Hovering a tab (active or not) reveals a "✕" centered in the pill's
            // left rounded cap; the caller draws close_buf there. Suppressed while
            // renaming or dragging that tab.
            if hovered == Some(i) && !tab.editing && !dragging_this {
                close_cx = Some(pill_cap_center(x, w, strip_h, cw).0);
            }
            // Unseen-bell indicator: a small amber dot in the tab's top-right.
            if tab.bell {
                let d = 5.0;
                out.push((
                    scaled(x + w - d - 4.0, 4.0, d, d, sf),
                    lin_rgba(AMBER, 0.95),
                ));
            }
            // Editing → buffer + caret; dragging → title only (no ⌘N, grabbed); else
            // title + ⌘N hint.
            let label = if tab.editing {
                format!("{}\u{2503}", tab.title) // ▏-ish caret
            } else if dragging_this {
                tab.title.clone()
            } else {
                format!("{}  ⌘{}", tab.title, i + 1)
            };
            let fg = if tab.active || tab.editing || dragging_this {
                Color::rgb(0xff, 0xff, 0xff)
            } else {
                Color::rgb(0x8a, 0x8a, 0x8a)
            };
            spans.push((center(&label, width), fg));
            col += width;
        }
    } else {
        // Single tab: no tab buttons, just the control toolbar. Pad the tab area so
        // the trailing buttons land in the right columns.
        spans.push((" ".repeat(tab_cols), Color::rgb(FG.r, FG.g, FG.b)));
    }
    // A grabbed tab "lifts" and follows the cursor: a raised, accent-bordered copy
    // with a drop shadow drawn over its (recessed) slot. Pixel-smooth follow on top
    // of the slot-snapped reorder, so the drag reads clearly. (Label stays in the
    // chrome buffer at the slot, which tracks the cursor via the live reorder.)
    if let (Some((_, cursor_x)), true) = (drag, n > 1) {
        let seg = (tab_cols / n).max(1);
        let seg_w = seg as f32 * cw;
        let lift_x = (cursor_x - seg_w * 0.5).clamp(0.0, (tab_cols as f32 * cw - seg_w).max(0.0));
        out.push((
            scaled(lift_x + 3.0, 3.0, seg_w, strip_h, sf),
            lin_rgba(Rgb::new(0, 0, 0), 0.38),
        )); // drop shadow
        out.push((
            scaled(lift_x, 0.0, seg_w, strip_h, sf),
            lin_rgba(Rgb::new(0x3a, 0x3a, 0x3a), 1.0),
        )); // raised fill
        out.push((
            scaled(lift_x, 0.0, seg_w, strip_h, sf),
            lin_rgba(ACCENT, 0.18),
        )); // warm tint
        push_border(
            out,
            Rect::new(lift_x as f64, 0.0, seg_w as f64, strip_h as f64),
            ACCENT,
            sf,
        );
    }
    // Trailing utility buttons: "+" (new tab), "?" (shortcuts), "⚙" (settings).
    let btn_fg = Color::rgb(0x8a, 0x8a, 0x8a);
    spans.push((center("+", plus_cols), btn_fg));
    spans.push((center("?", help_cols), btn_fg));
    spans.push((center("⚙", gear_cols), btn_fg));

    // Re-shape only when the shaped inputs changed: identical spans at
    // the same width/zoom produce byte-identical layout, so skip the shaping —
    // the common case under an output storm, where panes churn every frame but
    // the strip doesn't.
    let (lw_bits, cw_bits) = (logical_w.to_bits(), cw.to_bits());
    if cache.is_dirty(&spans, lw_bits, cw_bits) {
        chrome.set_size(font_system, Some(logical_w), Some(LINE_HEIGHT));
        chrome.set_rich_text(
            font_system,
            spans
                .iter()
                .map(|(t, c)| (t.as_str(), Attrs::new().family(Family::Monospace).color(*c))),
            &base,
            Shaping::Advanced,
            None,
        );
        chrome.shape_until_scroll(font_system, false);
        cache.store(spans, lw_bits, cw_bits);
    }

    // Shape the hover "✕" into its own buffer so the caller can pixel-center it in
    // the pill cap (the column-based chrome line can't hit that spot exactly).
    // The glyph is constant, so shape it once per zoom level, not per hover-frame.
    if close_cx.is_some() && cache.close_cw != Some(cw_bits) {
        close_buf.set_size(font_system, Some(cw * 2.0), Some(LINE_HEIGHT));
        close_buf.set_text(
            font_system,
            "✕",
            &Attrs::new().family(Family::Monospace),
            Shaping::Advanced,
            None,
        );
        close_buf.shape_until_scroll(font_system, false);
        cache.close_cw = Some(cw_bits);
    }
    close_cx
}

/// Build the cheat-sheet overlay: a full scrim + a centered panel (accent border)
/// sized to `lines`, with the `(key, desc)` rows shaped into `buffer`. Pushes quads
/// to `out` and returns the panel rect (logical px) for text placement. Shared by
/// the windowed renderer and the headless capture so they render identically.
#[allow(clippy::too_many_arguments)] // a draw helper: title/hint/geometry/out
pub(crate) fn build_help(
    font_system: &mut FontSystem,
    buffer: &mut Buffer,
    title: &str,
    hint: &str,
    lines: &[(String, String)],
    logical_w: f32,
    logical_h: f32,
    sf: f32,
    out: &mut Vec<([f32; 4], [f32; 4])>,
) -> Rect {
    // Key column: wide enough for the longest key in THIS list ( — a fixed
    // 18-char column overflowed into the description for longer combos like
    // "Wheel / Shift+PgUp/Dn"), plus a little breathing room.
    let key_w = lines
        .iter()
        .filter(|(k, _)| !k.is_empty())
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0)
        + 2;
    // A blank row before each section header (but not the first) reads far
    // less crowded than headers packed flush against the previous group. Only
    // add them if the panel still fits the window with the extra height —
    // the smallest supported window is exactly tight enough for the
    // no-spacer layout, so this must degrade gracefully, not clip.
    let header_count = lines
        .iter()
        .filter(|(k, d)| k.is_empty() && !d.is_empty())
        .count();
    let spacers = header_count.saturating_sub(1);
    let base_rows = lines.len() as f32 + 1.0; // +1 for the title/hint line
    let spaced_h = (base_rows + spacers as f32) * LINE_HEIGHT + 2.0 * HELP_PAD;
    let fits_spaced = spaced_h <= logical_h - 8.0;

    // Panel sized to content. Clamp to the window so a tiny (min-size) window
    // never draws the panel off-screen; a little wider than before to give
    // the (now dynamic, often wider) key column room without squeezing
    // descriptions.
    let w = (logical_w * 0.72).clamp(320.0, 540.0);
    let h = (if fits_spaced {
        spaced_h
    } else {
        base_rows * LINE_HEIGHT + 2.0 * HELP_PAD
    })
    .min(logical_h - 8.0);
    let x = ((logical_w - w) * 0.5).max(0.0);
    let y = ((logical_h - h) * 0.5).max(4.0);
    let panel = Rect::new(x as f64, y as f64, w as f64, h as f64);

    // Scrim over everything, then the panel fill + Ember-orange border.
    out.push((
        scaled(0.0, 0.0, logical_w, logical_h, sf),
        lin_rgba(Rgb::new(0, 0, 0), 0.66),
    ));
    out.push((
        scaled(x, y, w, h, sf),
        lin_rgba(Rgb::new(0x20, 0x22, 0x28), 0.98),
    ));
    push_border(out, panel, ACCENT, sf);

    // Shape the cheat-sheet text (keys in accent, descriptions in fg).
    buffer.set_size(
        font_system,
        Some(w - 2.0 * HELP_PAD),
        Some(h - 2.0 * HELP_PAD),
    );
    let mut spans: Vec<(String, Color)> = Vec::new();
    spans.push((title.to_string(), Color::rgb(0xff, 0xff, 0xff)));
    spans.push((format!("   ·  {hint}\n"), Color::rgb(0x88, 0x88, 0x88)));
    // A row with an empty key is a section header (accent-amber); the rest are
    // `key  description`.
    let mut seen_header = false;
    for (key, desc) in lines {
        if key.is_empty() {
            if fits_spaced && seen_header {
                spans.push(("\n".to_string(), Color::rgb(FG.r, FG.g, FG.b)));
            }
            seen_header = true;
            spans.push((format!("{desc}\n"), Color::rgb(AMBER.r, AMBER.g, AMBER.b)));
        } else {
            spans.push((
                format!("{key:<key_w$}"),
                Color::rgb(ACCENT.r, ACCENT.g, ACCENT.b),
            ));
            spans.push((format!("{desc}\n"), Color::rgb(FG.r, FG.g, FG.b)));
        }
    }
    let base = Attrs::new().family(Family::Monospace);
    buffer.set_rich_text(
        font_system,
        spans
            .iter()
            .map(|(t, c)| (t.as_str(), Attrs::new().family(Family::Monospace).color(*c))),
        &base,
        Shaping::Advanced,
        None,
    );
    buffer.shape_until_scroll(font_system, false);
    panel
}

/// Text-placement result from [`build_about`] (logical px; title is centered).
pub(crate) struct AboutLayout {
    pub title_left: f32,
    pub title_top: f32,
    pub body_top: f32,
    /// Logical `(x, y, w, h)` click rect per link button, in `info.links` order.
    pub link_rects: Vec<[f32; 4]>,
}

/// Build the About overlay: a dark scrim, a pulsing ember-glow halo behind the
/// wordmark, the centered wordmark (shaped into `title_buf`), and centered body
/// lines (shaped into `body_buf`). `glow ∈ [0,1]` animates the halo + title heat.
/// Pushes quads to `out`; returns where to place the two text areas. Shared by the
/// windowed renderer and headless capture so they render identically.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_about(
    font_system: &mut FontSystem,
    title_buf: &mut Buffer,
    body_buf: &mut Buffer,
    info: &AboutInfo,
    glow: f32,
    t: f32,
    cw: f32,
    logical_w: f32,
    logical_h: f32,
    sf: f32,
    out: &mut Vec<([f32; 4], [f32; 4])>,
) -> AboutLayout {
    // Scrim — fully obscure the terminal behind the modal.
    out.push((
        scaled(0.0, 0.0, logical_w, logical_h, sf),
        lin_rgba(Rgb::new(0, 0, 0), 0.86),
    ));

    // Drifting ember sparks rising across the modal. Procedural + stateless — driven
    // by `t` alone, so it animates with no stored particle state and renders
    // identically windowed + headless. Small amber→ember dots that sway, rise, and
    // fade in/out over a looping lifetime; overall brightness tracks `glow`.
    {
        use std::f32::consts::PI;
        const SPARKS: usize = 28;
        for i in 0..SPARKS {
            let fi = i as f32;
            let seed = {
                let s = (fi * 12.9898).sin() * 43758.547;
                s - s.floor()
            };
            let period = 3.2 + seed * 2.6;
            let phase = ((t / period) + seed).fract();
            let base_x = ((fi * 7.0).sin() * 0.5 + 0.5) * logical_w;
            let x = base_x + (t * 0.7 + fi).sin() * 18.0;
            let y = logical_h * (1.0 - phase);
            let alpha = (PI * phase).sin().max(0.0) * 0.7 * glow;
            let size = 2.0 + seed * 2.5;
            let color = lerp_rgb(AMBER, ACCENT, phase);
            out.push((scaled(x, y, size, size, sf), lin_rgba(color, alpha)));
        }
    }

    let title_h = ABOUT_TITLE_LINE;
    let gap = 22.0;
    // Links render as a spacer line + one button line each, below the body lines.
    let link_rows = if info.links.is_empty() {
        0
    } else {
        1 + info.links.len()
    };
    let content_h = title_h + gap + (info.lines.len() + link_rows) as f32 * LINE_HEIGHT;
    let top = ((logical_h - content_h) * 0.5).max(0.0);
    let title_top = top;
    let body_top = top + title_h + gap;
    let cx = logical_w * 0.5;
    let glow_cy = title_top + title_h * 0.5;

    // Ember-glow halo: many concentric centered quads from large+faint (outer) to
    // small+bright (inner). Overlapping alpha accumulates toward the center, so the
    // hard rectangle edges blur into a soft warm gradient. Pulses with `glow`.
    const RINGS: usize = 14;
    let base_w = logical_w.min(640.0) * 0.78;
    let base_h = title_h * 3.1;
    for k in 0..RINGS {
        let f = k as f32 / (RINGS as f32 - 1.0); // 0 = outer, 1 = inner
        let w = base_w * (1.0 - 0.74 * f);
        let h = base_h * (1.0 - 0.66 * f);
        let alpha = (0.035 + 0.075 * f) * glow; // cumulative across rings
        let color = lerp_rgb(ACCENT, AMBER, f);
        out.push((
            scaled(cx - w * 0.5, glow_cy - h * 0.5, w, h, sf),
            lin_rgba(color, alpha),
        ));
    }

    // Wordmark — heat from amber toward near-white at peak glow; centered.
    title_buf.set_size(font_system, Some(logical_w), Some(title_h));
    let tc = lerp_rgb(AMBER, Rgb::new(0xff, 0xe6, 0xc2), glow);
    let base = Attrs::new().family(Family::Monospace);
    title_buf.set_rich_text(
        font_system,
        [(
            info.title.as_str(),
            Attrs::new()
                .family(Family::Monospace)
                .color(Color::rgb(tc.r, tc.g, tc.b)),
        )],
        &base,
        Shaping::Advanced,
        None,
    );
    title_buf.shape_until_scroll(font_system, false);
    let title_w = title_buf
        .layout_runs()
        .next()
        .map(|r| r.line_w)
        .unwrap_or(0.0);
    let title_left = ((logical_w - title_w) * 0.5).max(0.0);

    // Body — centered per row via leading spaces (monospace): the info lines (line 0
    // amber, rest dimmed), then a spacer, then the link buttons in accent. Link
    // buttons also get a subtle accent-tinted background quad + a click rect.
    body_buf.set_size(font_system, Some(logical_w), Some(logical_h));
    let total_cols = (logical_w / cw).floor() as usize;
    let center_pad = |s: &str| {
        let pad = total_cols.saturating_sub(s.chars().count()) / 2;
        format!("{}{}", " ".repeat(pad), s)
    };
    let mut rows: Vec<(String, Color)> = Vec::new();
    for (i, line) in info.lines.iter().enumerate() {
        let color = if i == 0 {
            Color::rgb(AMBER.r, AMBER.g, AMBER.b)
        } else {
            Color::rgb(0xaa, 0xaa, 0xaa)
        };
        rows.push((center_pad(line), color));
    }
    let mut link_rects: Vec<[f32; 4]> = Vec::new();
    if !info.links.is_empty() {
        rows.push((String::new(), Color::rgb(0, 0, 0))); // spacer
        for (label, _url) in &info.links {
            rows.push((center_pad(label), Color::rgb(ACCENT.r, ACCENT.g, ACCENT.b)));
        }
        for (i, (label, _url)) in info.links.iter().enumerate() {
            let line_idx = info.lines.len() + 1 + i; // after the spacer
            let y = body_top + line_idx as f32 * LINE_HEIGHT;
            let w = label.chars().count() as f32 * cw;
            let bx = ((logical_w - w) * 0.5).max(0.0) - cw; // pad the button
            let bw = w + 2.0 * cw;
            out.push((scaled(bx, y, bw, LINE_HEIGHT, sf), lin_rgba(ACCENT, 0.18)));
            link_rects.push([bx, y, bw, LINE_HEIGHT]);
        }
    }
    let n = rows.len();
    let spans: Vec<(String, Color)> = rows
        .into_iter()
        .enumerate()
        .map(|(i, (t, c))| (if i + 1 < n { format!("{t}\n") } else { t }, c))
        .collect();
    body_buf.set_rich_text(
        font_system,
        spans
            .iter()
            .map(|(t, c)| (t.as_str(), Attrs::new().family(Family::Monospace).color(*c))),
        &base,
        Shaping::Advanced,
        None,
    );
    body_buf.shape_until_scroll(font_system, false);

    AboutLayout {
        title_left,
        title_top,
        body_top,
        link_rects,
    }
}

/// Build the Settings overlay: a centered panel with a title, a `(label, value)`
/// row per setting (the `selected` row highlighted in Ember-orange), and a footer
/// hint. Quads → `out`; the row text is shaped into `buf`. Returns the logical
/// `(left, top)` to place the text area. Shared by windowed + headless.
///
/// `reshape`: whether to re-shape the panel's text into `buf`. The quads
/// (scrim/panel/selection highlight) are always rebuilt — they're cheap and
/// positional — but shaping is hundreds of cosmic-text runs, so
/// the windowed renderer passes `false` when rows/selection/geometry are
/// unchanged since the last shape and `buf` still holds a valid layout.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_settings(
    font_system: &mut FontSystem,
    buf: &mut Buffer,
    rows: &[SettingsRowView],
    selected: usize,
    cw: f32,
    logical_w: f32,
    logical_h: f32,
    sf: f32,
    reshape: bool,
    out: &mut Vec<([f32; 4], [f32; 4])>,
) -> (f32, f32) {
    // Scrim.
    out.push((
        scaled(0.0, 0.0, logical_w, logical_h, sf),
        lin_rgba(Rgb::new(0, 0, 0), 0.78),
    ));
    let pad = HELP_PAD;
    let line = LINE_HEIGHT;
    // title + blank + rows + blank + hint.
    let body_lines = rows.len() as f32 + 4.0;
    let w = (logical_w * 0.7).clamp(300.0, 520.0);
    let h = body_lines * line + 2.0 * pad;
    let x = ((logical_w - w) * 0.5).max(0.0);
    let y = ((logical_h - h) * 0.5).max(0.0);
    let panel = Rect::new(x as f64, y as f64, w as f64, h as f64);
    out.push((
        scaled(x, y, w, h, sf),
        lin_rgba(Rgb::new(0x20, 0x22, 0x28), 0.98),
    ));
    push_border(out, panel, ACCENT, sf);

    // Highlight the selected row (row index in the body: title(0), blank(1), rows…).
    // `selected` never lands on a SectionHeader — the interaction layer (Up/Down
    // navigation) guarantees it always points at a selectable row.
    let row_y = y + pad + (2.0 + selected as f32) * line;
    out.push((
        scaled(x + pad * 0.5, row_y, w - pad, line, sf),
        lin_rgba(ACCENT, 0.28),
    ));

    // Shape the text: title, blank, each row (a category label, or "label …… value"),
    // blank, hint.
    if !reshape {
        return (x + pad, y + pad);
    }
    buf.set_size(font_system, Some(w - 2.0 * pad), Some(h - 2.0 * pad));
    let inner_cols = ((w - 2.0 * pad) / cw).floor() as usize;
    let base = Attrs::new().family(Family::Monospace);
    let mut spans: Vec<(String, Color)> = Vec::new();
    spans.push(("Settings\n\n".to_string(), Color::rgb(0xff, 0xff, 0xff)));
    for (i, row) in rows.iter().enumerate() {
        if row.kind == RowKind::SectionHeader {
            // A category divider: no value column, not highlighted, its own
            // accent-tinted color so it reads as structure, not a settable row.
            spans.push((
                format!("{}\n", row.label),
                Color::rgb(ACCENT.r, ACCENT.g, ACCENT.b),
            ));
            continue;
        }
        let gap = inner_cols.saturating_sub(row.label.chars().count() + row.value.chars().count());
        let text = format!("{}{}{}\n", row.label, " ".repeat(gap.max(1)), row.value);
        let color = if i == selected {
            Color::rgb(0xff, 0xff, 0xff)
        } else {
            Color::rgb(0xbb, 0xbb, 0xbb)
        };
        spans.push((text, color));
    }
    spans.push((
        "\n↑/↓ select   ←/→ change   esc close".to_string(),
        Color::rgb(0x80, 0x80, 0x80),
    ));
    buf.set_rich_text(
        font_system,
        spans
            .iter()
            .map(|(t, c)| (t.as_str(), Attrs::new().family(Family::Monospace).color(*c))),
        &base,
        Shaping::Advanced,
        None,
    );
    buf.shape_until_scroll(font_system, false);
    (x + pad, y + pad)
}

/// Build the FPS/frame-time debug readout: a small dark box pinned to the
/// bottom-right with `text` (e.g. "58 fps · 17.2 ms"). Quad → `out`, text shaped
/// into `buf`; returns the logical `(left, top)` for the text area. Drawn on top
/// of the panes (not a modal), so it never steals input.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_fps(
    font_system: &mut FontSystem,
    buf: &mut Buffer,
    text: &str,
    cw: f32,
    logical_w: f32,
    logical_h: f32,
    sf: f32,
    out: &mut Vec<([f32; 4], [f32; 4])>,
) -> (f32, f32) {
    let ipad = 5.0;
    let w = text.chars().count() as f32 * cw + 2.0 * ipad;
    let h = LINE_HEIGHT + 2.0 * ipad;
    let x = (logical_w - w - 8.0).max(0.0);
    let y = (logical_h - h - 8.0).max(0.0);
    out.push((scaled(x, y, w, h, sf), lin_rgba(Rgb::new(0, 0, 0), 0.62)));
    buf.set_size(font_system, Some(w), Some(h));
    buf.set_text(
        font_system,
        text,
        &Attrs::new()
            .family(Family::Monospace)
            .color(Color::rgb(AMBER.r, AMBER.g, AMBER.b)),
        Shaping::Advanced,
        None,
    );
    buf.shape_until_scroll(font_system, false);
    (x + ipad, y + ipad)
}

/// Scrollbar width (logical px) and minimum thumb height.
pub(crate) const SCROLLBAR_W: f32 = 8.0;
const SCROLLBAR_MIN_THUMB: f32 = 20.0;

/// Logical-px geometry of a pane's scrollbar as `(track, thumb)` each `[x,y,w,h]`,
/// or `None` when there's no history to show. `screen_lines` = the pane's visible
/// rows, `history_len` = lines of scrollback above. **Shared** by the draw and the
/// app's hit-test so they can never drift. `display_offset` 0 = live bottom (thumb
/// at the bottom); `history_len` = scrolled to the top.
pub(crate) fn scrollbar_geometry(
    display_offset: u16,
    history_len: u16,
    screen_lines: u16,
    pane: Rect,
) -> Option<([f32; 4], [f32; 4])> {
    if history_len == 0 || screen_lines == 0 {
        return None;
    }
    let total = history_len as f32 + screen_lines as f32;
    let px = (pane.x + pane.width) as f32 - SCROLLBAR_W;
    let py = pane.y as f32;
    let ph = pane.height as f32;
    let thumb_h = (screen_lines as f32 / total * ph).clamp(SCROLLBAR_MIN_THUMB.min(ph), ph);
    let top_frac = history_len.saturating_sub(display_offset) as f32 / total;
    let mut thumb_y = py + top_frac * ph;
    if thumb_y + thumb_h > py + ph {
        thumb_y = py + ph - thumb_h;
    }
    if thumb_y < py {
        thumb_y = py;
    }
    Some((
        [px, py, SCROLLBAR_W, ph],
        [px + 1.0, thumb_y, SCROLLBAR_W - 2.0, thumb_h],
    ))
}

/// Draw a pane's scrollbar (dark track + warm thumb) when it has history — Ember's
/// discoverable "there's more, and here's where you are" affordance. A scrolled-up
/// view (`display_offset > 0`) brightens the thumb to `ACCENT`.
pub(crate) fn scrollbar(
    display_offset: u16,
    history_len: u16,
    screen_lines: u16,
    pane: Rect,
    sf: f32,
    out: &mut Vec<([f32; 4], [f32; 4])>,
) {
    let Some((track, thumb)) = scrollbar_geometry(display_offset, history_len, screen_lines, pane)
    else {
        return;
    };
    out.push((
        scaled(track[0], track[1], track[2], track[3], sf),
        lin_rgba(Rgb::new(0, 0, 0), 0.22),
    ));
    let (c, a) = if display_offset > 0 {
        (ACCENT, 0.9)
    } else {
        (Rgb::new(0x9a, 0x9a, 0x9a), 0.55)
    };
    out.push((
        scaled(thumb[0], thumb[1], thumb[2], thumb[3], sf),
        lin_rgba(c, a),
    ));
}

/// A `(rect_px, …)`-ready physical-pixel quad from logical `x,y,w,h` and the
/// HiDPI scale factor.
pub(crate) fn scaled(x: f32, y: f32, w: f32, h: f32, sf: f32) -> [f32; 4] {
    [x * sf, y * sf, w * sf, h * sf]
}

/// Emit a debug line to the `EMBER_DEBUG` sink: a file path if the value contains
/// `/` (so a reviewer can `cat` it on the same machine), else stderr. No-op when
/// the var is unset.
pub(crate) fn debug_emit(line: &str) {
    match std::env::var("EMBER_DEBUG") {
        Ok(v) if v.contains('/') => {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&v)
            {
                let _ = writeln!(f, "{line}");
            }
        }
        Ok(_) => eprintln!("{line}"),
        Err(_) => {}
    }
}

/// Push four thin quads outlining `rect` (logical px) in `color` — a ~1.5px
/// focus border, scaled to physical px by `sf`.
pub(crate) fn push_border(rects: &mut Vec<([f32; 4], [f32; 4])>, rect: Rect, color: Rgb, sf: f32) {
    let t = 1.5f32;
    let (x, y, w, h) = (
        rect.x as f32,
        rect.y as f32,
        rect.width as f32,
        rect.height as f32,
    );
    let c = lin_rgba(color, 1.0);
    rects.push((scaled(x, y, w, t, sf), c)); // top
    rects.push((scaled(x, y + h - t, w, t, sf), c)); // bottom
    rects.push((scaled(x, y, t, h, sf), c)); // left
    rects.push((scaled(x + w - t, y, t, h, sf), c)); // right
}

/// Text-placement result from [`build_confirm`] (logical px).
pub(crate) struct ConfirmLayout {
    pub msg_origin: (f32, f32),
    pub title_origin: (f32, f32),
    pub cancel_origin: (f32, f32),
    pub ok_origin: (f32, f32),
    /// `[(button_rect_logical, index)]` — index 0 = cancel, 1 = confirm.
    pub buttons: Vec<([f32; 4], usize)>,
}

/// Draw the blocking confirm modal: a scrim + a centered rounded panel with a
/// title, a message, and two rounded buttons (Cancel, Confirm). The focused
/// button gets an ember ring; the confirm button is ember-tinted (danger).
/// Everything goes into `rounded` (scrim is radius 0) so the modal draws over
/// all content incl. the tab pills. Returns text origins for the caller's
/// TextAreas + the button hit rects.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_confirm(
    font_system: &mut FontSystem,
    title_buf: &mut Buffer,
    msg_buf: &mut Buffer,
    cancel_buf: &mut Buffer,
    ok_buf: &mut Buffer,
    view: &crate::renderer::ConfirmView,
    cw: f32,
    logical_w: f32,
    logical_h: f32,
    sf: f32,
    rounded: &mut Vec<([f32; 4], [f32; 4], f32)>,
) -> ConfirmLayout {
    let pad = 20.0;
    let btn_h = 30.0;
    let btn_gap = 10.0;
    let label_w = |s: &str| s.chars().count() as f32 * cw + 26.0;
    let cancel_w = label_w(&view.cancel_label).max(72.0);
    let ok_w = label_w(&view.confirm_label).max(72.0);

    let w = (logical_w * 0.5).clamp(340.0, 470.0);
    let h = pad + LINE_HEIGHT + 6.0 + LINE_HEIGHT + 18.0 + btn_h + pad;
    let x = ((logical_w - w) * 0.5).max(0.0);
    let y = ((logical_h - h) * 0.5).max(4.0);

    // Scrim (radius 0) then the panel (rounded, ember border via a ring).
    rounded.push((
        scaled(0.0, 0.0, logical_w, logical_h, sf),
        lin_rgba(Rgb::new(0, 0, 0), 0.62),
        0.0,
    ));
    let r = 10.0;
    rounded.push((
        scaled(x - 1.5, y - 1.5, w + 3.0, h + 3.0, sf),
        lin_rgba(ACCENT, 0.9),
        (r + 1.5) * sf,
    ));
    rounded.push((
        scaled(x, y, w, h, sf),
        lin_rgba(Rgb::new(0x20, 0x22, 0x28), 1.0),
        r * sf,
    ));

    // Buttons: bottom-right, Confirm rightmost.
    let by = y + h - pad - btn_h;
    let ok_x = x + w - pad - ok_w;
    let cancel_x = ok_x - btn_gap - cancel_w;
    let focused = view.focused;
    // Cancel (neutral; ring when focused).
    if focused == 0 {
        rounded.push((
            scaled(cancel_x - 1.5, by - 1.5, cancel_w + 3.0, btn_h + 3.0, sf),
            lin_rgba(ACCENT, 0.9),
            (8.0 + 1.5) * sf,
        ));
    }
    rounded.push((
        scaled(cancel_x, by, cancel_w, btn_h, sf),
        lin_rgba(Rgb::new(0x3a, 0x3a, 0x3d), 1.0),
        8.0 * sf,
    ));
    // Confirm (ember-tinted danger; ring when focused).
    if focused == 1 {
        rounded.push((
            scaled(ok_x - 1.5, by - 1.5, ok_w + 3.0, btn_h + 3.0, sf),
            lin_rgba(Rgb::new(0xff, 0xff, 0xff), 0.8),
            (8.0 + 1.5) * sf,
        ));
    }
    rounded.push((
        scaled(ok_x, by, ok_w, btn_h, sf),
        lin_rgba(ACCENT, 0.92),
        8.0 * sf,
    ));

    // Shape text. Title (white), message (gray); labels centered in buttons.
    let shape = |fs: &mut FontSystem, buf: &mut Buffer, text: &str, color: Color| {
        buf.set_size(fs, Some(w), Some(LINE_HEIGHT));
        buf.set_text(
            fs,
            text,
            &Attrs::new().family(Family::Monospace).color(color),
            Shaping::Advanced,
            None,
        );
        buf.shape_until_scroll(fs, false);
    };
    shape(
        font_system,
        title_buf,
        &view.title,
        Color::rgb(0xff, 0xff, 0xff),
    );
    shape(
        font_system,
        msg_buf,
        &view.message,
        Color::rgb(0x9a, 0x9a, 0x9a),
    );
    shape(
        font_system,
        cancel_buf,
        &view.cancel_label,
        Color::rgb(0xf0, 0xf0, 0xf0),
    );
    shape(
        font_system,
        ok_buf,
        &view.confirm_label,
        Color::rgb(0xff, 0xff, 0xff),
    );

    let label_x = |bx: f32, bw: f32, s: &str| bx + (bw - s.chars().count() as f32 * cw) * 0.5;
    let btn_text_y = by + (btn_h - LINE_HEIGHT) * 0.5;
    ConfirmLayout {
        title_origin: (x + pad, y + pad),
        msg_origin: (x + pad, y + pad + LINE_HEIGHT + 6.0),
        cancel_origin: (label_x(cancel_x, cancel_w, &view.cancel_label), btn_text_y),
        ok_origin: (label_x(ok_x, ok_w, &view.confirm_label), btn_text_y),
        buttons: vec![
            ([cancel_x, by, cancel_w, btn_h], 0),
            ([ok_x, by, ok_w, btn_h], 1),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::center;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn center_pads_ascii_to_width() {
        let out = center("ab", 6);
        assert_eq!(out, "  ab  ");
        assert_eq!(UnicodeWidthStr::width(out.as_str()), 6);
    }

    #[test]
    fn center_measures_cjk_as_two_columns() {
        // "你好" is 4 display columns; centering in 8 adds 2 spaces each side.
        let out = center("你好", 8);
        assert_eq!(UnicodeWidthStr::width(out.as_str()), 8);
        assert_eq!(out, "  你好  ");
    }

    #[test]
    fn center_truncates_by_display_width_not_char_count() {
        // Four CJK chars = 8 columns; a 5-column field keeps 2 columns + "…"
        // and pads to exactly 5 (never overflows the tab button).
        let out = center("漢字漢字", 5);
        assert_eq!(UnicodeWidthStr::width(out.as_str()), 5);
        assert!(out.ends_with('…'));
    }

    // --- TabsCache: skip tab-strip re-shaping when nothing changed ---

    use super::{Color, TabsCache};

    fn spans(items: &[(&str, u32)]) -> Vec<(String, Color)> {
        items
            .iter()
            .map(|(t, c)| (t.to_string(), Color(*c)))
            .collect()
    }

    #[test]
    fn fresh_cache_is_dirty() {
        let cache = TabsCache::default();
        assert!(cache.is_dirty(&spans(&[("tab 1", 1)]), 100f32.to_bits(), 8f32.to_bits()));
    }

    /// Perf evidence for , not a regression gate (hence `#[ignore]`):
    /// run manually with
    /// `cargo test --release -p ember-render tab_shaping_cache_speedup -- --ignored --nocapture`.
    /// Asserts the cached path skips shaping (>5x faster over 200 frames).
    #[test]
    #[ignore]
    fn tab_shaping_cache_speedup() {
        use super::{TabsCache, build_tabs};
        use crate::renderer::TabLabel;
        use std::time::Instant;

        let mut fs = super::new_font_system();
        let mut chrome = glyphon::Buffer::new(
            &mut fs,
            glyphon::Metrics::new(crate::renderer::FONT_SIZE, crate::renderer::LINE_HEIGHT),
        );
        let mut close_buf = glyphon::Buffer::new(
            &mut fs,
            glyphon::Metrics::new(crate::renderer::FONT_SIZE, crate::renderer::LINE_HEIGHT),
        );
        let tabs: Vec<TabLabel> = (0..4)
            .map(|i| TabLabel {
                title: format!("tab {i}"),
                active: i == 0,
                editing: false,
                bell: false,
            })
            .collect();
        let mut run = |cache: &mut TabsCache, reset: bool| {
            let t = Instant::now();
            for _ in 0..200 {
                if reset {
                    *cache = TabsCache::default(); // force a full re-shape (old behavior)
                }
                let (mut out, mut rounded) = (Vec::new(), Vec::new());
                build_tabs(
                    &mut fs,
                    &mut chrome,
                    &mut close_buf,
                    cache,
                    &tabs,
                    None,
                    Some(1),
                    8.0,
                    1200.0,
                    2.0,
                    &mut out,
                    &mut rounded,
                );
            }
            t.elapsed()
        };
        let mut cache = TabsCache::default();
        let cold = run(&mut cache, true);
        let warm = run(&mut cache, false);
        println!(
            "200 frames: uncached={cold:?} cached={warm:?} ({:.0}x)",
            cold.as_secs_f64() / warm.as_secs_f64().max(1e-9)
        );
        assert!(
            warm < cold / 5,
            "cache should skip shaping: warm={warm:?} cold={cold:?}"
        );
    }

    #[test]
    fn stored_inputs_are_clean_until_something_changes() {
        let mut cache = TabsCache::default();
        let (lw, cw) = (100f32.to_bits(), 8f32.to_bits());
        cache.store(spans(&[("tab 1", 1), ("+", 2)]), lw, cw);
        // Same everything → clean (this is the storm fast path).
        assert!(!cache.is_dirty(&spans(&[("tab 1", 1), ("+", 2)]), lw, cw));
        // Rename, recolor (hover/active flip), resize, zoom → each dirties.
        assert!(cache.is_dirty(&spans(&[("tab 2", 1), ("+", 2)]), lw, cw));
        assert!(cache.is_dirty(&spans(&[("tab 1", 9), ("+", 2)]), lw, cw));
        assert!(cache.is_dirty(&spans(&[("tab 1", 1), ("+", 2)]), 200f32.to_bits(), cw));
        assert!(cache.is_dirty(&spans(&[("tab 1", 1), ("+", 2)]), lw, 9f32.to_bits()));
        // Tab count change → dirty.
        assert!(cache.is_dirty(&spans(&[("tab 1", 1)]), lw, cw));
    }

    use super::link_quads;

    #[test]
    fn link_quads_places_underlines_and_brightens_the_hovered_link() {
        use crate::grid_model::{LinkSource, LinkSpan};
        let spans = vec![
            LinkSpan {
                link_id: 0,
                row: 1,
                cols: 2..6,
                url: "https://a.io".into(),
                source: LinkSource::Detected,
            },
            LinkSpan {
                link_id: 1,
                row: 3,
                cols: 0..4,
                url: "https://b.io".into(),
                source: LinkSource::Detected,
            },
        ];
        let mut out = Vec::new();
        link_quads(&spans, Some(1), (10.0, 20.0), 8.0, 16.0, 1.0, &mut out);
        assert_eq!(out.len(), 2);
        // Span 0: x = 10 + 2*8, y = 20 + (1+1)*16 - 2, w = 4*8, h = 1.
        assert_eq!(out[0].0, [26.0, 50.0, 32.0, 1.0]);
        // Span 1 (hovered): thicker underline, h = 2.
        assert_eq!(out[1].0, [10.0, 82.0, 32.0, 2.0]);
        // Hovered is more opaque than idle.
        assert!(out[1].1[3] > out[0].1[3]);
    }
}
