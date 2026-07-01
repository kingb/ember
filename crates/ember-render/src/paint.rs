//! Shared draw helpers — the single source of truth for turning a neutral grid +
//! overlays into glyphs and quads. Used by both the windowed [`crate::renderer`]
//! and the headless screenshot path ([`crate::headless`]) so they render
//! identically. Stateless free functions over the renderer's colors/metrics; the
//! `Renderer` struct + GPU plumbing live in `renderer.rs`.

use ember_core::{MarkStatus, Rect, Rgb};
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
    // OSC 133 shell-integration gutter: a colored bar at each command's prompt line
    // — green = exit 0, red = non-zero, amber = still running. Drawn in the left pad
    // so it doesn't overlap text.
    for &(row, status) in &grid.marks {
        if row < grid.dims.screen_lines {
            let color = match status {
                MarkStatus::Ok => GUTTER_OK,
                MarkStatus::Fail => GUTTER_FAIL,
                MarkStatus::Running => GUTTER_RUN,
            };
            out.push((
                scaled(ox - 3.5, oy + row as f32 * ch + 1.0, 2.5, ch - 2.0, sf),
                lin_rgba(color, 1.0),
            ));
        }
    }
}

/// Shell-integration gutter mark colors (exit 0 / non-zero / running).
const GUTTER_OK: Rgb = Rgb::new(0x3f, 0xb9, 0x50);
const GUTTER_FAIL: Rgb = Rgb::new(0xe5, 0x48, 0x4d);
const GUTTER_RUN: Rgb = Rgb::new(0xd0, 0x90, 0x30);

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
const TAB_ACTIVE: Rgb = Rgb::new(0x2e, 0x2e, 0x2e);
/// Width (in columns) of each trailing tab-strip utility button ("+", "?", "⚙").
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

/// Build the tab strip (iTerm-style): a full-width bar with the trailing utility
/// buttons "+" (new tab), "?" (shortcuts), "⚙" (settings) — *always shown* so those
/// controls are discoverable even with a single tab (design §1 discoverability
/// tenet). With multiple tabs it also draws equal-width tab buttons (active one
/// lighter with an Ember-orange underline + `⌘N` hint); with one tab the tab area
/// is just an empty toolbar. Quads → `out`; the concatenated label line shapes into
/// `chrome`. All geometry is logical px, scaled by `sf`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_tabs(
    font_system: &mut FontSystem,
    chrome: &mut Buffer,
    tabs: &[TabLabel],
    drag: Option<(usize, f32)>,
    cw: f32,
    logical_w: f32,
    sf: f32,
    out: &mut Vec<([f32; 4], [f32; 4])>,
) {
    chrome.set_size(font_system, Some(logical_w), Some(LINE_HEIGHT));
    let strip_h = CELL_HEIGHT + 2.0 * PAD;
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
                // Inline rename: accent fill + full border so it reads as an input.
                out.push((scaled(x, 0.0, w, strip_h, sf), lin_rgba(TAB_ACTIVE, 1.0)));
                push_border(
                    out,
                    Rect::new(x as f64, 0.0, w as f64, strip_h as f64),
                    ACCENT,
                    sf,
                );
            } else if tab.active {
                out.push((scaled(x, 0.0, w, strip_h, sf), lin_rgba(TAB_ACTIVE, 1.0)));
                // Ember-orange underline accent on the active tab.
                out.push((scaled(x, strip_h - 2.0, w, 2.0, sf), lin_rgba(ACCENT, 1.0)));
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
    // Panel sized to content: title + dismiss hint + one row per line. The amber
    // section headers separate groups on their own (no blank line needed). Clamp to
    // the window so a tiny (min-size) window never draws the panel off-screen.
    let w = (logical_w * 0.7).clamp(300.0, 480.0);
    // +1 for the title/hint line; the amber section headers need no extra spacing.
    let h = ((lines.len() as f32 + 1.0) * LINE_HEIGHT + 2.0 * HELP_PAD).min(logical_h - 8.0);
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
    spans.push((
        "Keyboard Shortcuts".to_string(),
        Color::rgb(0xff, 0xff, 0xff),
    ));
    spans.push((
        "   ·  any key to close\n".to_string(),
        Color::rgb(0x88, 0x88, 0x88),
    ));
    // A row with an empty key is a section header (accent-amber); the rest are
    // `key  description`. The amber headers separate groups without blank lines.
    for (key, desc) in lines {
        if key.is_empty() {
            spans.push((format!("{desc}\n"), Color::rgb(AMBER.r, AMBER.g, AMBER.b)));
        } else {
            spans.push((
                format!("{key:<18}"),
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
