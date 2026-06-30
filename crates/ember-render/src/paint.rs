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
    ABOUT_TITLE_LINE, ACCENT, AMBER, AboutInfo, BG, CELL_HEIGHT, FG, FONT_SIZE, HELP_PAD,
    LINE_HEIGHT, PAD, TabLabel,
};
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
