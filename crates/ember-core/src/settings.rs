//! Typed, categorized Settings row model (design doc:
//! `docs/design/2026-07-04-settings-reorg-design.md`).
//!
//! Replaces a flat `Vec<(String, String)>` matched by a hardcoded positional
//! index (`adjust_setting()`'s old `match settings_sel { 0 => ..., 1 => ... }`
//! in `ember-app`) with a static table where each row carries its own
//! formatter and mutator. The row table *is* the dispatch: there is no
//! second match statement anywhere that can drift out of sync with it, and
//! reordering, inserting, or deleting a row is purely a table edit.
//!
//! `format`/`adjust` are plain function pointers, not boxed closures: each
//! row's logic only touches its own `Config` parameter (no captured
//! environment), so non-capturing closures coerce to `fn` pointers for free.

use crate::config::Config;

/// What kind of row this is, driving both rendering and key-handling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RowKind {
    /// A boolean flip (arrow keys or Space).
    Toggle,
    /// A continuous, clamped numeric value (arrow keys step it).
    Number,
    /// Steps through a fixed discrete list, wrapping (e.g. font family).
    /// Mechanically identical to `Number` — arrow keys call `adjust` — but
    /// semantically distinct enough to keep separate for rendering/help tone.
    Cycle,
    /// Shown but not adjustable (e.g. the config.toml-only backdrop image).
    ReadOnly,
    /// Triggers an action on Enter/Space rather than adjusting a value. No
    /// row uses this yet — reserved for a future "Check for updates" row.
    Action,
    /// A category divider: not selectable, skipped by Up/Down navigation.
    SectionHeader,
}

/// Per-row help payload: data on the row, not a separate switch statement.
/// Simple settings get a one-or-two-sentence inline popup; complex ones
/// (security-relevant, e.g. Developer Mode) get a slug pointing at a fuller
/// in-app docs page that doesn't exist yet — this just reserves the field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Help {
    Inline(&'static str),
    DocsRef(&'static str),
}

/// One row in the Settings overlay.
#[derive(Clone, Copy)]
pub struct SettingRow {
    pub label: &'static str,
    pub kind: RowKind,
    pub format: fn(&Config) -> String,
    /// `None` for `ReadOnly`/`Action`/`SectionHeader` — nothing to adjust.
    pub adjust: Option<fn(&mut Config, f32)>,
    pub help: Help,
}

/// A row resolved against a live `Config`: the render layer's input. Crosses
/// the `ember-app`/`ember-render` boundary — `ember-app` builds these each
/// time the overlay needs a repaint (via [`resolve_rows`]), `ember-render`
/// only ever sees already-formatted strings + the row's `kind`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsRowView {
    pub label: &'static str,
    pub value: String,
    pub kind: RowKind,
}

/// Resolve every row in [`setting_rows`] against `config` into render-ready
/// views, in table order.
pub fn resolve_rows(config: &Config) -> Vec<SettingsRowView> {
    setting_rows()
        .iter()
        .map(|r| SettingsRowView {
            label: r.label,
            value: (r.format)(config),
            kind: r.kind,
        })
        .collect()
}

fn on_off(b: bool) -> String {
    if b {
        "on".to_string()
    } else {
        "off".to_string()
    }
}

// --- Appearance: font ------------------------------------------------------

/// The curated font-family cycle list. `None` is the platform monospace
/// default. Deliberately small and cross-platform-friendly rather than an
/// exhaustive system-font enumeration — see the design doc.
const FONT_FAMILIES: &[Option<&str>] = &[
    None,
    Some("Menlo"),
    Some("SF Mono"),
    Some("Monaco"),
    Some("JetBrains Mono"),
    Some("Fira Code"),
    Some("Cascadia Code"),
    Some("DejaVu Sans Mono"),
];

/// Index of `current` in `FONT_FAMILIES`, or `0` (platform default) if it
/// isn't there — e.g. a hand-edited `config.toml` with an unlisted name. A
/// known, acceptable minor rough edge for a rare case (see the design doc).
fn font_family_index(current: &Option<String>) -> usize {
    FONT_FAMILIES
        .iter()
        .position(|f| f.as_deref() == current.as_deref())
        .unwrap_or(0)
}

fn fmt_font_family(c: &Config) -> String {
    match c.font.family.as_deref() {
        Some(name) => name.to_string(),
        None => "System default".to_string(),
    }
}

fn adjust_font_family(c: &mut Config, dir: f32) {
    let n = FONT_FAMILIES.len();
    let idx = font_family_index(&c.font.family);
    let next = if dir >= 0.0 {
        (idx + 1) % n
    } else {
        (idx + n - 1) % n
    };
    c.font.family = FONT_FAMILIES[next].map(str::to_string);
}

/// Live-apply plumbing (`Renderer::set_font_size`) already clamps to this
/// same range — kept in sync deliberately, not derived, since ember-core
/// can't depend on ember-render.
const MIN_FONT_SIZE: f32 = 6.0;
const MAX_FONT_SIZE: f32 = 48.0;

fn fmt_font_size(c: &Config) -> String {
    format!("{}pt", c.font.size.round() as i32)
}

fn adjust_font_size(c: &mut Config, dir: f32) {
    c.font.size = (c.font.size + dir).clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
}

// --- Appearance: backdrop ---------------------------------------------------

fn fmt_gradient(c: &Config) -> String {
    on_off(c.background.gradient)
}
fn adjust_gradient(c: &mut Config, _dir: f32) {
    c.background.gradient = !c.background.gradient;
}

fn fmt_ember_sparks(c: &Config) -> String {
    on_off(c.background.ember_sparks)
}
fn adjust_ember_sparks(c: &mut Config, _dir: f32) {
    c.background.ember_sparks = !c.background.ember_sparks;
}

fn fmt_ember_density(c: &Config) -> String {
    format!("{:.1}", c.background.ember_density)
}
fn adjust_ember_density(c: &mut Config, dir: f32) {
    c.background.ember_density = (c.background.ember_density + 0.1 * dir).clamp(0.0, 2.0);
}

fn fmt_ember_fps(c: &Config) -> String {
    format!("{}", c.background.ember_fps)
}
fn adjust_ember_fps(c: &mut Config, dir: f32) {
    c.background.ember_fps =
        (c.background.ember_fps as i32 + (5.0 * dir) as i32).clamp(10, 120) as u32;
}

fn fmt_scrim(c: &Config) -> String {
    format!("{:.2}", c.background.scrim)
}
fn adjust_scrim(c: &mut Config, dir: f32) {
    c.background.scrim = (c.background.scrim + 0.05 * dir).clamp(0.0, 1.0);
}

fn fmt_backdrop_image(c: &Config) -> String {
    match c.background.image.as_deref() {
        Some(p) => {
            let name = std::path::Path::new(p)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(p);
            format!("{name} ({})", c.background.image_fit)
        }
        None => "none".to_string(),
    }
}

// --- Terminal ----------------------------------------------------------------

fn fmt_visual_bell(c: &Config) -> String {
    on_off(c.visual_bell)
}
fn adjust_visual_bell(c: &mut Config, _dir: f32) {
    c.visual_bell = !c.visual_bell;
}

fn fmt_shell_integration(c: &Config) -> String {
    on_off(c.shell_integration)
}
fn adjust_shell_integration(c: &mut Config, _dir: f32) {
    c.shell_integration = !c.shell_integration;
}

fn fmt_option_as_meta(c: &Config) -> String {
    on_off(c.option_as_meta)
}
fn adjust_option_as_meta(c: &mut Config, _dir: f32) {
    c.option_as_meta = !c.option_as_meta;
}

// --- Developer -----------------------------------------------------------------

fn fmt_developer_mode(c: &Config) -> String {
    on_off(c.developer_mode)
}
fn adjust_developer_mode(c: &mut Config, _dir: f32) {
    c.developer_mode = !c.developer_mode;
}

/// The full, ordered Settings row table. Categories are `SectionHeader` rows
/// inline in one flat list, not a nested structure — this keeps the render
/// layer's existing flat-list shape and only enriches each entry.
pub fn setting_rows() -> &'static [SettingRow] {
    &[
        SettingRow {
            label: "Appearance",
            kind: RowKind::SectionHeader,
            format: |_| String::new(),
            adjust: None,
            help: Help::Inline(""),
        },
        SettingRow {
            label: "Font family",
            kind: RowKind::Cycle,
            format: fmt_font_family,
            adjust: Some(adjust_font_family),
            help: Help::Inline(
                "The terminal's monospace font. Cycles a curated cross-platform list; \
                 System default follows the platform's own monospace font.",
            ),
        },
        SettingRow {
            label: "Font size",
            kind: RowKind::Number,
            format: fmt_font_size,
            adjust: Some(adjust_font_size),
            help: Help::Inline(
                "The terminal's baseline font size in points. Cmd +/-/0 also zoom live; \
                 this sets the size Cmd+0 resets to.",
            ),
        },
        SettingRow {
            label: "Gradient backdrop",
            kind: RowKind::Toggle,
            format: fmt_gradient,
            adjust: Some(adjust_gradient),
            help: Help::Inline("A dark warm vertical gradient drawn behind the terminal cells."),
        },
        SettingRow {
            label: "Ember sparks",
            kind: RowKind::Toggle,
            format: fmt_ember_sparks,
            adjust: Some(adjust_ember_sparks),
            help: Help::Inline(
                "Drifting glowing embers behind the panes. Purely ambient; off by default.",
            ),
        },
        SettingRow {
            label: "Ember density",
            kind: RowKind::Number,
            format: fmt_ember_density,
            adjust: Some(adjust_ember_density),
            help: Help::Inline("Spark count/rate multiplier for the ember sparks effect."),
        },
        SettingRow {
            label: "Ember FPS",
            kind: RowKind::Number,
            format: fmt_ember_fps,
            adjust: Some(adjust_ember_fps),
            help: Help::Inline(
                "Frame-rate cap for the ember spark animation. Lower means less CPU.",
            ),
        },
        SettingRow {
            label: "Scrim",
            kind: RowKind::Number,
            format: fmt_scrim,
            adjust: Some(adjust_scrim),
            help: Help::Inline(
                "Darkening overlay strength over the backdrop, for text legibility.",
            ),
        },
        SettingRow {
            label: "Backdrop image",
            kind: RowKind::ReadOnly,
            format: fmt_backdrop_image,
            adjust: None,
            help: Help::Inline(
                "A background image drawn behind the cells. Set the path in config.toml, not here.",
            ),
        },
        SettingRow {
            label: "Terminal",
            kind: RowKind::SectionHeader,
            format: |_| String::new(),
            adjust: None,
            help: Help::Inline(""),
        },
        SettingRow {
            label: "Visual bell",
            kind: RowKind::Toggle,
            format: fmt_visual_bell,
            adjust: Some(adjust_visual_bell),
            help: Help::Inline(
                "A terminal BEL flashes an ember pulse and lights the belling tab, instead of \
                 an audible beep.",
            ),
        },
        SettingRow {
            label: "Shell integration",
            kind: RowKind::Toggle,
            format: fmt_shell_integration,
            adjust: Some(adjust_shell_integration),
            help: Help::Inline(
                "Auto-injects shell-integration hooks (exit-status gutter, jump-to-prompt) into \
                 newly spawned zsh/bash sessions. Applies to new sessions only, not ones already \
                 running.",
            ),
        },
        SettingRow {
            label: "Option acts as Meta",
            kind: RowKind::Toggle,
            format: fmt_option_as_meta,
            adjust: Some(adjust_option_as_meta),
            help: Help::Inline(
                "macOS: Opt+key sends ESC key (readline/emacs Meta) instead of composing accented \
                 characters. Takes effect immediately.",
            ),
        },
        SettingRow {
            label: "Developer",
            kind: RowKind::SectionHeader,
            format: |_| String::new(),
            adjust: None,
            help: Help::Inline(""),
        },
        SettingRow {
            label: "Developer Mode",
            kind: RowKind::Toggle,
            format: fmt_developer_mode,
            adjust: Some(adjust_developer_mode),
            help: Help::DocsRef("developer-mode"),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(label: &str) -> &'static SettingRow {
        setting_rows().iter().find(|r| r.label == label).unwrap()
    }

    #[test]
    fn table_has_no_duplicate_labels() {
        let rows = setting_rows();
        for (i, a) in rows.iter().enumerate() {
            for b in &rows[i + 1..] {
                assert_ne!(a.label, b.label, "duplicate label {:?}", a.label);
            }
        }
    }

    #[test]
    fn section_headers_have_no_adjust() {
        for r in setting_rows() {
            if r.kind == RowKind::SectionHeader {
                assert!(
                    r.adjust.is_none(),
                    "{:?} header must not be adjustable",
                    r.label
                );
            }
        }
    }

    #[test]
    fn read_only_rows_have_no_adjust() {
        for r in setting_rows() {
            if r.kind == RowKind::ReadOnly {
                assert!(
                    r.adjust.is_none(),
                    "{:?} read-only must not be adjustable",
                    r.label
                );
            }
        }
    }

    #[test]
    fn font_family_row_is_a_cycle_with_adjust() {
        let r = row("Font family");
        assert_eq!(r.kind, RowKind::Cycle);
        assert!(r.adjust.is_some());
    }

    #[test]
    fn font_family_adjust_mutates_only_font_family() {
        let mut c = Config::default();
        let before = c.clone();
        adjust_font_family(&mut c, 1.0);
        assert_ne!(c.font.family, before.font.family);
        assert_eq!(c.font.size, before.font.size);
        assert_eq!(c.background, before.background);
    }

    #[test]
    fn font_family_cycles_forward_through_the_whole_list_and_wraps() {
        let mut c = Config::default();
        assert_eq!(c.font.family, None);
        let mut seen = vec![c.font.family.clone()];
        for _ in 0..FONT_FAMILIES.len() - 1 {
            adjust_font_family(&mut c, 1.0);
            seen.push(c.font.family.clone());
        }
        // Every entry in FONT_FAMILIES was visited exactly once, in order.
        let expected: Vec<Option<String>> = FONT_FAMILIES
            .iter()
            .map(|f| f.map(str::to_string))
            .collect();
        assert_eq!(seen, expected);
        // One more step wraps back to the start (None).
        adjust_font_family(&mut c, 1.0);
        assert_eq!(c.font.family, None);
    }

    #[test]
    fn font_family_cycles_backward_and_wraps() {
        let mut c = Config::default();
        adjust_font_family(&mut c, -1.0);
        assert_eq!(c.font.family.as_deref(), Some("DejaVu Sans Mono"));
        adjust_font_family(&mut c, -1.0);
        assert_eq!(c.font.family.as_deref(), Some("Cascadia Code"));
    }

    #[test]
    fn font_family_unrecognized_value_treated_as_index_zero() {
        let mut c = Config::default();
        c.font.family = Some("Comic Sans MS".to_string());
        adjust_font_family(&mut c, 1.0);
        // Not found -> index 0 -> +1 -> index 1 (Menlo).
        assert_eq!(c.font.family.as_deref(), Some("Menlo"));
    }

    #[test]
    fn font_size_adjust_mutates_only_font_size() {
        let mut c = Config::default();
        let before = c.clone();
        (row("Font size").adjust.unwrap())(&mut c, 1.0);
        assert_ne!(c.font.size, before.font.size);
        assert_eq!(c.font.family, before.font.family);
        assert_eq!(c.background, before.background);
    }

    #[test]
    fn font_size_steps_by_one_point_and_clamps() {
        let mut c = Config::default();
        c.font.size = 12.0;
        let adjust = row("Font size").adjust.unwrap();
        adjust(&mut c, 1.0);
        assert_eq!(c.font.size, 13.0);
        adjust(&mut c, -1.0);
        adjust(&mut c, -1.0);
        assert_eq!(c.font.size, 11.0);

        c.font.size = MAX_FONT_SIZE;
        adjust(&mut c, 1.0);
        assert_eq!(c.font.size, MAX_FONT_SIZE, "must clamp at the max");

        c.font.size = MIN_FONT_SIZE;
        adjust(&mut c, -1.0);
        assert_eq!(c.font.size, MIN_FONT_SIZE, "must clamp at the min");
    }

    #[test]
    fn gradient_toggle_mutates_only_gradient() {
        let mut c = Config::default();
        let before = c.clone();
        (row("Gradient backdrop").adjust.unwrap())(&mut c, 1.0);
        assert_ne!(c.background.gradient, before.background.gradient);
        assert_eq!(c.background.ember_sparks, before.background.ember_sparks);
        assert_eq!(c.visual_bell, before.visual_bell);
    }

    #[test]
    fn ember_sparks_toggle_mutates_only_ember_sparks() {
        let mut c = Config::default();
        let before = c.clone();
        (row("Ember sparks").adjust.unwrap())(&mut c, 1.0);
        assert_ne!(c.background.ember_sparks, before.background.ember_sparks);
        assert_eq!(c.background.gradient, before.background.gradient);
    }

    #[test]
    fn ember_density_steps_and_clamps() {
        let mut c = Config::default();
        c.background.ember_density = 1.0;
        let adjust = row("Ember density").adjust.unwrap();
        adjust(&mut c, 1.0);
        assert!((c.background.ember_density - 1.1).abs() < 1e-6);
        c.background.ember_density = 2.0;
        adjust(&mut c, 1.0);
        assert_eq!(c.background.ember_density, 2.0);
        c.background.ember_density = 0.0;
        adjust(&mut c, -1.0);
        assert_eq!(c.background.ember_density, 0.0);
    }

    #[test]
    fn ember_fps_steps_and_clamps() {
        let mut c = Config::default();
        c.background.ember_fps = 30;
        let adjust = row("Ember FPS").adjust.unwrap();
        adjust(&mut c, 1.0);
        assert_eq!(c.background.ember_fps, 35);
        c.background.ember_fps = 120;
        adjust(&mut c, 1.0);
        assert_eq!(c.background.ember_fps, 120);
        c.background.ember_fps = 10;
        adjust(&mut c, -1.0);
        assert_eq!(c.background.ember_fps, 10);
    }

    #[test]
    fn scrim_steps_and_clamps() {
        let mut c = Config::default();
        c.background.scrim = 0.45;
        let adjust = row("Scrim").adjust.unwrap();
        adjust(&mut c, 1.0);
        assert!((c.background.scrim - 0.50).abs() < 1e-6);
        c.background.scrim = 1.0;
        adjust(&mut c, 1.0);
        assert_eq!(c.background.scrim, 1.0);
        c.background.scrim = 0.0;
        adjust(&mut c, -1.0);
        assert_eq!(c.background.scrim, 0.0);
    }

    #[test]
    fn backdrop_image_formats_filename_and_fit_or_none() {
        let mut c = Config::default();
        assert_eq!(fmt_backdrop_image(&c), "none");
        c.background.image = Some("/opt/backdrops/fire.png".to_string());
        c.background.image_fit = "cover".to_string();
        assert_eq!(fmt_backdrop_image(&c), "fire.png (cover)");
    }

    #[test]
    fn visual_bell_toggle_mutates_only_visual_bell() {
        let mut c = Config::default();
        let before = c.clone();
        (row("Visual bell").adjust.unwrap())(&mut c, 1.0);
        assert_ne!(c.visual_bell, before.visual_bell);
        assert_eq!(c.shell_integration, before.shell_integration);
    }

    #[test]
    fn shell_integration_toggle_mutates_only_shell_integration() {
        let mut c = Config::default();
        let before = c.clone();
        (row("Shell integration").adjust.unwrap())(&mut c, 1.0);
        assert_ne!(c.shell_integration, before.shell_integration);
        assert_eq!(c.visual_bell, before.visual_bell);
    }

    #[test]
    fn option_as_meta_toggle_mutates_only_option_as_meta() {
        let mut c = Config::default();
        let before = c.clone();
        (row("Option acts as Meta").adjust.unwrap())(&mut c, 1.0);
        assert_ne!(c.option_as_meta, before.option_as_meta);
        assert_eq!(c.shell_integration, before.shell_integration);
    }

    #[test]
    fn developer_mode_toggle_mutates_only_developer_mode() {
        let mut c = Config::default();
        let before = c.clone();
        (row("Developer Mode").adjust.unwrap())(&mut c, 1.0);
        assert_ne!(c.developer_mode, before.developer_mode);
        assert_eq!(c.option_as_meta, before.option_as_meta);
    }

    #[test]
    fn developer_mode_has_docs_ref_help() {
        assert!(matches!(
            row("Developer Mode").help,
            Help::DocsRef("developer-mode")
        ));
    }

    #[test]
    fn category_order_is_appearance_terminal_developer() {
        let headers: Vec<&str> = setting_rows()
            .iter()
            .filter(|r| r.kind == RowKind::SectionHeader)
            .map(|r| r.label)
            .collect();
        assert_eq!(headers, vec!["Appearance", "Terminal", "Developer"]);
    }
}
