//! Shared draw helpers — the single source of truth for turning a neutral grid +
//! overlays into glyphs and quads. Used by both the windowed [`crate::renderer`]
//! and the headless screenshot path ([`crate::headless`]) so they render
//! identically. Stateless free functions over the renderer's colors/metrics; the
//! `Renderer` struct + GPU plumbing live in `renderer.rs`.

use ember_core::{Rect, Rgb};
use glyphon::{Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping};

use crate::grid_model::GridModel;
use crate::quads::srgb_to_linear;
use crate::renderer::{
    ABOUT_TITLE_LINE, ACCENT, AMBER, AboutInfo, BG, BackdropParams, CELL_HEIGHT, FG, FONT_SIZE,
    HELP_PAD, LINE_HEIGHT, PAD, TabLabel,
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
/// Measure the monospace advance for the current font/size, so background quads
/// line up with the glyphs glyphon flows.
pub(crate) fn measure_cell_width(font_system: &mut FontSystem) -> f32 {
    let mut probe = Buffer::new(font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
    probe.set_text(
        font_system,
        "MMMMMMMMMM",
        &Attrs::new().family(Family::Monospace),
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
    FONT_SIZE * 0.6
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

/// Shape one grid's rows into `buffer` as per-cell fg-colored runs (one logical
/// line per grid row).
pub(crate) fn shape_grid(font_system: &mut FontSystem, buffer: &mut Buffer, grid: &GridModel) {
    let lines = grid.dims.screen_lines;
    let mut spans: Vec<(String, Color)> = Vec::new();
    for row in 0..lines {
        for (text, fg) in grid.row_runs(row) {
            spans.push((text, Color::rgb(fg.r, fg.g, fg.b)));
        }
        if row + 1 < lines {
            spans.push(("\n".to_string(), Color::rgb(FG.r, FG.g, FG.b)));
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
}

/// Append a grid's bg fills + (when focused) cursor + (when focused && split)
/// focus border, for a pane at logical `rect`, scaled to physical px by `sf`.
pub(crate) fn grid_quads(
    grid: &GridModel,
    rect: Rect,
    cw: f32,
    sf: f32,
    focused: bool,
    split: bool,
    out: &mut Vec<([f32; 4], [f32; 4])>,
) {
    let ox = rect.x as f32;
    let oy = rect.y as f32;
    let ch = CELL_HEIGHT;
    for row in 0..grid.dims.screen_lines {
        for col in 0..grid.dims.columns {
            if let Some(cell) = grid.cell(row, col) {
                let bg = grid.style_of(cell.style).bg;
                if bg != BG {
                    out.push((
                        scaled(ox + col as f32 * cw, oy + row as f32 * ch, cw, ch, sf),
                        lin_rgba(bg, 1.0),
                    ));
                }
            }
        }
    }
    if focused {
        let cur = grid.cursor;
        if cur.visible {
            out.push((
                scaled(
                    ox + cur.col as f32 * cw,
                    oy + cur.row as f32 * ch,
                    cw,
                    ch,
                    sf,
                ),
                lin_rgba(FG, 0.5),
            ));
        }
        if split {
            push_border(out, rect, ACCENT, sf);
        }
    }
}

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
    sf: f32,
    out: &mut Vec<([f32; 4], [f32; 4])>,
) {
    let ox = rect.x as f32;
    let oy = rect.y as f32;
    let ch = CELL_HEIGHT;
    for row in 0..grid.dims.screen_lines {
        if let Some((c0, c1)) = selection.row_span(grid, row) {
            let x = ox + c0 as f32 * cw;
            let w = (c1 - c0 + 1) as f32 * cw;
            let y = oy + row as f32 * ch;
            out.push((scaled(x, y, w, ch, sf), lin_rgba(SELECT_BG, 0.45)));
        }
    }
}

/// Background of the tab strip (a touch lighter than the terminal, iTerm-style).
const STRIP_BG: Rgb = Rgb::new(0x1b, 0x1b, 0x1b);
/// Fill of the active tab button.
const TAB_ACTIVE: Rgb = Rgb::new(0x2e, 0x2e, 0x2e);
/// Width (in columns) of each trailing tab-strip utility button ("+", "?").
pub(crate) const BTN_COLS: usize = 3;

/// Center `s` in a field `width` columns wide (truncating with `…` if too long).
fn center(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if width == 0 {
        return String::new();
    }
    if n >= width {
        if width == 1 {
            return "…".to_string();
        }
        let keep: String = s.chars().take(width - 1).collect();
        return format!("{keep}…");
    }
    let total = width - n;
    let left = total / 2;
    let right = total - left;
    format!("{}{}{}", " ".repeat(left), s, " ".repeat(right))
}

/// Build the tab strip (iTerm-style): a full-width bar, equal-width tab buttons
/// (active one lighter with an Ember-orange underline), `⌘N` hints, and a `+`
/// button. Quads go to `out`; the single concatenated label line is shaped into
/// `chrome`. No-op for a single tab. All geometry is logical px, scaled by `sf`.
pub(crate) fn build_tabs(
    font_system: &mut FontSystem,
    chrome: &mut Buffer,
    tabs: &[TabLabel],
    cw: f32,
    logical_w: f32,
    sf: f32,
    out: &mut Vec<([f32; 4], [f32; 4])>,
) {
    if tabs.len() <= 1 {
        return;
    }
    chrome.set_size(font_system, Some(logical_w), Some(LINE_HEIGHT));
    let strip_h = CELL_HEIGHT + 2.0 * PAD;
    // Full-width strip background.
    out.push((
        scaled(0.0, 0.0, logical_w, strip_h, sf),
        lin_rgba(STRIP_BG, 1.0),
    ));

    // Work in integer columns so quads and (monospace) text stay aligned. The two
    // trailing utility buttons ("+" new-tab, "?" help) each reserve `BTN_COLS`.
    let total_cols = (logical_w / cw).floor() as usize;
    let plus_cols = BTN_COLS.min(total_cols);
    let help_cols = BTN_COLS.min(total_cols.saturating_sub(plus_cols));
    let tab_cols = total_cols.saturating_sub(plus_cols + help_cols);
    let n = tabs.len(); // >= 2 (single-tab strips return early above)
    let seg = tab_cols / n;

    let base = Attrs::new().family(Family::Monospace);
    let mut spans: Vec<(String, Color)> = Vec::new();
    let mut col = 0usize;
    for (i, tab) in tabs.iter().enumerate() {
        // Last tab absorbs any leftover columns so the row fills exactly.
        let width = if i == n - 1 { tab_cols - col } else { seg };
        let x = col as f32 * cw;
        let w = width as f32 * cw;
        if tab.active {
            out.push((scaled(x, 0.0, w, strip_h, sf), lin_rgba(TAB_ACTIVE, 1.0)));
            // Ember-orange underline accent on the active tab.
            out.push((scaled(x, strip_h - 2.0, w, 2.0, sf), lin_rgba(ACCENT, 1.0)));
        }
        let label = format!("{}  ⌘{}", tab.title, i + 1);
        let fg = if tab.active {
            Color::rgb(0xff, 0xff, 0xff)
        } else {
            Color::rgb(0x8a, 0x8a, 0x8a)
        };
        spans.push((center(&label, width), fg));
        col += width;
    }
    // Trailing utility buttons: "+" (new tab) and "?" (keyboard shortcuts).
    let btn_fg = Color::rgb(0x8a, 0x8a, 0x8a);
    spans.push((center("+", plus_cols), btn_fg));
    spans.push((center("?", help_cols), btn_fg));

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
}

/// Build the cheat-sheet overlay: a full scrim + a centered panel (accent border)
/// sized to `lines`, with the `(key, desc)` rows shaped into `buffer`. Pushes quads
/// to `out` and returns the panel rect (logical px) for text placement. Shared by
/// the windowed renderer and the headless capture so they render identically.
pub(crate) fn build_help(
    font_system: &mut FontSystem,
    buffer: &mut Buffer,
    lines: &[(String, String)],
    logical_w: f32,
    logical_h: f32,
    sf: f32,
    out: &mut Vec<([f32; 4], [f32; 4])>,
) -> Rect {
    // Panel sized to content: title + dismiss hint + blank + one row per line.
    let w = (logical_w * 0.7).clamp(280.0, 460.0);
    let h = (lines.len() as f32 + 3.0) * LINE_HEIGHT + 2.0 * HELP_PAD;
    let x = ((logical_w - w) * 0.5).max(0.0);
    let y = ((logical_h - h) * 0.5).max(0.0);
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
    spans.push((
        "Keyboard Shortcuts\n".to_string(),
        Color::rgb(0xff, 0xff, 0xff),
    ));
    spans.push((
        "press any key to dismiss\n\n".to_string(),
        Color::rgb(0x88, 0x88, 0x88),
    ));
    for (key, desc) in lines {
        spans.push((
            format!("{key:<18}"),
            Color::rgb(ACCENT.r, ACCENT.g, ACCENT.b),
        ));
        spans.push((format!("{desc}\n"), Color::rgb(FG.r, FG.g, FG.b)));
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
    let content_h = title_h + gap + info.lines.len() as f32 * LINE_HEIGHT;
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

    // Body lines — centered per line via leading spaces (monospace). Line 0 (the
    // tagline) in amber, the rest dimmed.
    body_buf.set_size(font_system, Some(logical_w), Some(logical_h));
    let total_cols = (logical_w / cw).floor() as usize;
    let mut spans: Vec<(String, Color)> = Vec::new();
    let last = info.lines.len().saturating_sub(1);
    for (i, line) in info.lines.iter().enumerate() {
        let pad = total_cols.saturating_sub(line.chars().count()) / 2;
        let centered = format!("{}{}", " ".repeat(pad), line);
        let text = if i < last {
            format!("{centered}\n")
        } else {
            centered
        };
        let color = if i == 0 {
            Color::rgb(AMBER.r, AMBER.g, AMBER.b)
        } else {
            Color::rgb(0xaa, 0xaa, 0xaa)
        };
        spans.push((text, color));
    }
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
    }
}

/// Build the Settings overlay: a centered panel with a title, a `(label, value)`
/// row per setting (the `selected` row highlighted in Ember-orange), and a footer
/// hint. Quads → `out`; the row text is shaped into `buf`. Returns the logical
/// `(left, top)` to place the text area. Shared by windowed + headless.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_settings(
    font_system: &mut FontSystem,
    buf: &mut Buffer,
    rows: &[(String, String)],
    selected: usize,
    cw: f32,
    logical_w: f32,
    logical_h: f32,
    sf: f32,
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
    let row_y = y + pad + (2.0 + selected as f32) * line;
    out.push((
        scaled(x + pad * 0.5, row_y, w - pad, line, sf),
        lin_rgba(ACCENT, 0.28),
    ));

    // Shape the text: title, blank, each "label …… value", blank, hint.
    buf.set_size(font_system, Some(w - 2.0 * pad), Some(h - 2.0 * pad));
    let inner_cols = ((w - 2.0 * pad) / cw).floor() as usize;
    let base = Attrs::new().family(Family::Monospace);
    let mut spans: Vec<(String, Color)> = Vec::new();
    spans.push(("Settings\n\n".to_string(), Color::rgb(0xff, 0xff, 0xff)));
    for (i, (label, value)) in rows.iter().enumerate() {
        let gap = inner_cols.saturating_sub(label.chars().count() + value.chars().count());
        let text = format!("{label}{}{value}\n", " ".repeat(gap.max(1)));
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
