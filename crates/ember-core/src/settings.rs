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

use crate::config::{Config, RestoreMode, SparksMode, WispStyleSelection};

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
    /// Triggers an action on Enter/Space rather than adjusting a value —
    /// e.g. "Delete saved sessions (N)…". Has no `Config`-mutating
    /// `SettingRow::adjust`: the action isn't a config field, so the app
    /// layer (`ember-app`'s `settings_key`) handles it by matching on this
    /// kind rather than calling `adjust`.
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
///
/// `label` is owned (not `&'static str` like `SettingRow::label`) because
/// one synthesized row — "Delete saved sessions (N)…" — bakes a live count
/// into its label text; every other row's label is just its static
/// `SettingRow::label` copied in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsRowView {
    pub label: String,
    pub value: String,
    pub kind: RowKind,
}

/// The "Session restore" cycle row's static label — also the anchor
/// `resolve_rows` inserts the synthesized "Delete saved sessions" row
/// after, so the two can't drift apart.
const SESSION_RESTORE_LABEL: &str = "Session restore";
/// The "Capture commands" toggle row's static label — also what
/// `resolve_rows` matches on to hide the row while `restore.mode == Off`
/// (nothing to capture) and to anchor the delete row's insertion point when
/// it's visible.
const CAPTURE_COMMANDS_LABEL: &str = "Capture commands";
/// The "Shell color overrides manual" toggle row's static label — also
/// what `resolve_rows` matches on to synthesize the read-only "Tab color
/// rules (N)" info row right after it (v1: rule editing is config.toml-only,
/// this row is informational — see the design doc `2026-09-04-tab-colors`).
const SHELL_OVERRIDES_MANUAL_LABEL: &str = "Shell color overrides manual";

/// Resolve every row in [`setting_rows`] against `config` into render-ready
/// views, in table order — plus the "Delete saved sessions (N)…" action row
/// synthesized right after the Session-restore rows (it isn't a `Config`
/// field, so it has no entry in [`setting_rows`]'s static table).
///
/// `saved_sessions` is the count of on-disk session-state files (the live
/// snapshot, its quarantined-corrupt sibling, and every `.prev-*` archive —
/// `ember-app`'s `session_state::saved_state_count`); the delete row is
/// omitted entirely when it's `0` (nothing to delete). `Capture commands` is
/// likewise omitted while `config.restore.mode == RestoreMode::Off` — there
/// is nothing to capture, and no `SettingRow::adjust` fn for it to mutate
/// meaningfully in that state.
pub fn resolve_rows(config: &Config, saved_sessions: usize) -> Vec<SettingsRowView> {
    let mut rows = Vec::new();
    for r in setting_rows() {
        if r.label == CAPTURE_COMMANDS_LABEL && config.restore.mode == RestoreMode::Off {
            continue;
        }
        rows.push(SettingsRowView {
            label: r.label.to_string(),
            value: (r.format)(config),
            kind: r.kind,
        });
        let is_last_session_row = (r.label == SESSION_RESTORE_LABEL
            && config.restore.mode == RestoreMode::Off)
            || r.label == CAPTURE_COMMANDS_LABEL;
        if is_last_session_row && saved_sessions > 0 {
            rows.push(SettingsRowView {
                label: format!("Delete saved sessions ({saved_sessions})…"),
                value: String::new(),
                kind: RowKind::Action,
            });
        }
        // "Tab color rules (N)": a read-only info row (v1 — full rule
        // editing in Settings is not this task) synthesized right after the
        // knob, always shown (even at N=0 — "no rules configured" is useful
        // too), so it can't drift from the static table like the delete-row
        // anchor above.
        if r.label == SHELL_OVERRIDES_MANUAL_LABEL {
            rows.push(SettingsRowView {
                label: format!("Tab color rules ({})", config.tab_colors.rules.len()),
                value: String::new(),
                kind: RowKind::ReadOnly,
            });
        }
    }
    rows
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

/// The sparks dial's three-state cycle, `off → focused → always → off`.
/// Direction-agnostic (like the other flat toggles here): a three-way dial
/// doesn't need Left/Right to mean different things, so this always steps
/// forward regardless of `dir` (kept `f32` to match [`SettingRow::adjust`]'s
/// shared signature).
fn fmt_sparks(c: &Config) -> String {
    match c.background.sparks {
        SparksMode::Off => "off".to_string(),
        SparksMode::Focused => "focused".to_string(),
        SparksMode::Always => "always".to_string(),
    }
}
fn adjust_sparks(c: &mut Config, _dir: f32) {
    c.background.sparks = match c.background.sparks {
        SparksMode::Off => SparksMode::Focused,
        SparksMode::Focused => SparksMode::Always,
        SparksMode::Always => SparksMode::Off,
    };
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

/// The wisp style dial's seven-state cycle: `cinder → coal → willowisp →
/// comet → goo → star → random → cinder`. Direction-agnostic (like the
/// sparks dial): always steps forward regardless of `dir`.
fn fmt_wisp_style(c: &Config) -> String {
    match c.wisp_style {
        WispStyleSelection::Cinder => "cinder".to_string(),
        WispStyleSelection::Coal => "coal".to_string(),
        WispStyleSelection::WillOWisp => "willowisp".to_string(),
        WispStyleSelection::Comet => "comet".to_string(),
        WispStyleSelection::Goo => "goo".to_string(),
        WispStyleSelection::Star => "star".to_string(),
        WispStyleSelection::Random => "random".to_string(),
    }
}
fn adjust_wisp_style(c: &mut Config, _dir: f32) {
    c.wisp_style = match c.wisp_style {
        WispStyleSelection::Cinder => WispStyleSelection::Coal,
        WispStyleSelection::Coal => WispStyleSelection::WillOWisp,
        WispStyleSelection::WillOWisp => WispStyleSelection::Comet,
        WispStyleSelection::Comet => WispStyleSelection::Goo,
        WispStyleSelection::Goo => WispStyleSelection::Star,
        WispStyleSelection::Star => WispStyleSelection::Random,
        WispStyleSelection::Random => WispStyleSelection::Cinder,
    };
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

// --- Session restore ---------------------------------------------------------

/// The restore-mode dial's three-state cycle, `off → ask → always → off`.
/// Direction-agnostic (like the sparks/wisp-style dials): always steps
/// forward regardless of `dir`.
fn fmt_restore_mode(c: &Config) -> String {
    match c.restore.mode {
        RestoreMode::Off => "off".to_string(),
        RestoreMode::Ask => "ask".to_string(),
        RestoreMode::Always => "always".to_string(),
    }
}
fn adjust_restore_mode(c: &mut Config, _dir: f32) {
    c.restore.mode = match c.restore.mode {
        RestoreMode::Off => RestoreMode::Ask,
        RestoreMode::Ask => RestoreMode::Always,
        RestoreMode::Always => RestoreMode::Off,
    };
}

fn fmt_capture_commands(c: &Config) -> String {
    on_off(c.restore.capture_commands)
}
fn adjust_capture_commands(c: &mut Config, _dir: f32) {
    c.restore.capture_commands = !c.restore.capture_commands;
}

// --- Tab colors ------------------------------------------------------------

fn fmt_osc6_overrides_manual(c: &Config) -> String {
    on_off(c.tab_colors.osc6_overrides_manual)
}
fn adjust_osc6_overrides_manual(c: &mut Config, _dir: f32) {
    c.tab_colors.osc6_overrides_manual = !c.tab_colors.osc6_overrides_manual;
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
            kind: RowKind::Cycle,
            format: fmt_sparks,
            adjust: Some(adjust_sparks),
            help: Help::Inline(
                "Drifting glowing embers behind the panes: off, focused-window-only (default), \
                 or always. Paused automatically under Low Power Mode or Reduce Motion.",
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
            label: "Wisp style",
            kind: RowKind::Cycle,
            format: fmt_wisp_style,
            adjust: Some(adjust_wisp_style),
            help: Help::Inline(
                "The glowing drag token's look while carrying a tab/pane between windows: \
                 ember, coal, will-o'-the-wisp, comet, goo, or random (a fresh pick each drag).",
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
            label: "Session",
            kind: RowKind::SectionHeader,
            format: |_| String::new(),
            adjust: None,
            help: Help::Inline(""),
        },
        SettingRow {
            label: SESSION_RESTORE_LABEL,
            kind: RowKind::Cycle,
            format: fmt_restore_mode,
            adjust: Some(adjust_restore_mode),
            help: Help::Inline(
                "Snapshot open windows/tabs/splits and offer to restore them on next launch: \
                 off, ask first (default), or always restore silently. Off leaves any \
                 already-saved session on disk; delete it with the row below.",
            ),
        },
        SettingRow {
            label: CAPTURE_COMMANDS_LABEL,
            kind: RowKind::Toggle,
            format: fmt_capture_commands,
            adjust: Some(adjust_capture_commands),
            help: Help::Inline(
                "Capture each pane's last shell command line so restore can re-type it \
                 (unsent) at the prompt. Off keeps layout/cwd restore but immediately clears \
                 any commands already saved.",
            ),
        },
        SettingRow {
            label: "Tab colors",
            kind: RowKind::SectionHeader,
            format: |_| String::new(),
            adjust: None,
            help: Help::Inline(""),
        },
        SettingRow {
            label: SHELL_OVERRIDES_MANUAL_LABEL,
            kind: RowKind::Toggle,
            format: fmt_osc6_overrides_manual,
            adjust: Some(adjust_osc6_overrides_manual),
            help: Help::Inline(
                "When on, a shell's OSC 6 tab-color report beats a manually picked color \
                 (a pinned default still blocks it). Off by default: your manual pick wins. \
                 Regex rules are edited in config.toml; the row below just shows how many \
                 are configured.",
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
    use crate::config::TabColorRule;

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
        assert_eq!(c.background.sparks, before.background.sparks);
        assert_eq!(c.visual_bell, before.visual_bell);
    }

    #[test]
    fn sparks_cycle_mutates_only_sparks() {
        let mut c = Config::default();
        let before = c.clone();
        (row("Ember sparks").adjust.unwrap())(&mut c, 1.0);
        assert_ne!(c.background.sparks, before.background.sparks);
        assert_eq!(c.background.gradient, before.background.gradient);
    }

    #[test]
    fn sparks_cycle_visits_all_three_states_and_wraps() {
        let mut c = Config::default();
        c.background.sparks = SparksMode::Off;
        let adjust = row("Ember sparks").adjust.unwrap();
        adjust(&mut c, 1.0);
        assert_eq!(c.background.sparks, SparksMode::Focused);
        adjust(&mut c, 1.0);
        assert_eq!(c.background.sparks, SparksMode::Always);
        adjust(&mut c, 1.0);
        assert_eq!(c.background.sparks, SparksMode::Off);
    }

    #[test]
    fn sparks_row_is_a_cycle() {
        assert_eq!(row("Ember sparks").kind, RowKind::Cycle);
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
    fn wisp_style_row_is_a_cycle() {
        assert_eq!(row("Wisp style").kind, RowKind::Cycle);
    }

    #[test]
    fn wisp_style_cycle_mutates_only_wisp_style() {
        let mut c = Config::default();
        let before = c.clone();
        (row("Wisp style").adjust.unwrap())(&mut c, 1.0);
        assert_ne!(c.wisp_style, before.wisp_style);
        assert_eq!(c.background, before.background);
        assert_eq!(c.wisp, before.wisp);
    }

    #[test]
    fn wisp_style_cycle_visits_all_seven_states_and_wraps() {
        // `Config::default()` already starts at `Ember` — the cycle's home
        // position.
        let mut c = Config::default();
        let adjust = row("Wisp style").adjust.unwrap();
        let expected = [
            WispStyleSelection::Coal,
            WispStyleSelection::WillOWisp,
            WispStyleSelection::Comet,
            WispStyleSelection::Goo,
            WispStyleSelection::Star,
            WispStyleSelection::Random,
            WispStyleSelection::Cinder,
        ];
        for want in expected {
            adjust(&mut c, 1.0);
            assert_eq!(c.wisp_style, want);
        }
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
    fn category_order_is_appearance_terminal_session_tab_colors_developer() {
        let headers: Vec<&str> = setting_rows()
            .iter()
            .filter(|r| r.kind == RowKind::SectionHeader)
            .map(|r| r.label)
            .collect();
        assert_eq!(
            headers,
            vec![
                "Appearance",
                "Terminal",
                "Session",
                "Tab colors",
                "Developer"
            ]
        );
    }

    // --- session restore: config rows, hide/show, delete-action synthesis ---

    #[test]
    fn settings_include_restore_rows() {
        let rows = resolve_rows(&Config::default(), 0);
        assert!(rows.iter().any(|r| r.label.starts_with("Session restore")));
        assert!(rows.iter().any(|r| r.label.starts_with("Capture commands")));
    }

    #[test]
    fn session_restore_row_is_a_cycle() {
        assert_eq!(row(SESSION_RESTORE_LABEL).kind, RowKind::Cycle);
    }

    #[test]
    fn session_restore_cycle_visits_all_three_states_and_wraps() {
        let mut c = Config::default();
        c.restore.mode = RestoreMode::Off;
        let adjust = row(SESSION_RESTORE_LABEL).adjust.unwrap();
        adjust(&mut c, 1.0);
        assert_eq!(c.restore.mode, RestoreMode::Ask);
        adjust(&mut c, 1.0);
        assert_eq!(c.restore.mode, RestoreMode::Always);
        adjust(&mut c, 1.0);
        assert_eq!(c.restore.mode, RestoreMode::Off);
    }

    #[test]
    fn session_restore_cycle_mutates_only_restore_mode() {
        let mut c = Config::default();
        let before = c.clone();
        (row(SESSION_RESTORE_LABEL).adjust.unwrap())(&mut c, 1.0);
        assert_ne!(c.restore.mode, before.restore.mode);
        assert_eq!(c.restore.capture_commands, before.restore.capture_commands);
        assert_eq!(c.developer_mode, before.developer_mode);
    }

    #[test]
    fn capture_commands_row_is_a_toggle() {
        assert_eq!(row(CAPTURE_COMMANDS_LABEL).kind, RowKind::Toggle);
    }

    #[test]
    fn capture_commands_toggle_mutates_only_capture_commands() {
        let mut c = Config::default();
        let before = c.clone();
        (row(CAPTURE_COMMANDS_LABEL).adjust.unwrap())(&mut c, 1.0);
        assert_ne!(c.restore.capture_commands, before.restore.capture_commands);
        assert_eq!(c.restore.mode, before.restore.mode);
    }

    #[test]
    fn capture_commands_row_hidden_when_restore_is_off() {
        let mut c = Config::default();
        c.restore.mode = RestoreMode::Off;
        let rows = resolve_rows(&c, 0);
        assert!(!rows.iter().any(|r| r.label == CAPTURE_COMMANDS_LABEL));
    }

    #[test]
    fn capture_commands_row_shown_when_restore_is_ask_or_always() {
        for mode in [RestoreMode::Ask, RestoreMode::Always] {
            let mut c = Config::default();
            c.restore.mode = mode;
            let rows = resolve_rows(&c, 0);
            assert!(
                rows.iter().any(|r| r.label == CAPTURE_COMMANDS_LABEL),
                "capture commands row missing for mode {mode:?}"
            );
        }
    }

    #[test]
    fn delete_saved_sessions_row_hidden_when_count_is_zero() {
        let rows = resolve_rows(&Config::default(), 0);
        assert!(
            !rows
                .iter()
                .any(|r| r.label.starts_with("Delete saved sessions"))
        );
    }

    #[test]
    fn delete_saved_sessions_row_shown_with_count_in_label_when_nonzero() {
        let rows = resolve_rows(&Config::default(), 4);
        let r = rows
            .iter()
            .find(|r| r.label.starts_with("Delete saved sessions"))
            .expect("delete row present");
        assert_eq!(r.label, "Delete saved sessions (4)…");
        assert_eq!(r.kind, RowKind::Action);
    }

    #[test]
    fn delete_saved_sessions_row_shown_even_when_restore_is_off() {
        // Off keeps files on disk (ruling) — the delete action must still be
        // reachable to clear them.
        let mut c = Config::default();
        c.restore.mode = RestoreMode::Off;
        let rows = resolve_rows(&c, 2);
        assert!(rows.iter().any(|r| r.label == "Delete saved sessions (2)…"));
    }

    #[test]
    fn action_rows_have_no_adjust() {
        for r in setting_rows() {
            if r.kind == RowKind::Action {
                assert!(
                    r.adjust.is_none(),
                    "{:?} action row must not be adjustable",
                    r.label
                );
            }
        }
    }

    // --- tab colors: knob row + read-only rule-count row ---------------------

    #[test]
    fn shell_overrides_manual_row_is_a_toggle() {
        assert_eq!(row(SHELL_OVERRIDES_MANUAL_LABEL).kind, RowKind::Toggle);
    }

    #[test]
    fn shell_overrides_manual_toggle_mutates_only_that_field() {
        let mut c = Config::default();
        let before = c.clone();
        (row(SHELL_OVERRIDES_MANUAL_LABEL).adjust.unwrap())(&mut c, 1.0);
        assert!(c.tab_colors.osc6_overrides_manual);
        assert_eq!(c.tab_colors.rules, before.tab_colors.rules);
        assert_eq!(c.developer_mode, before.developer_mode);
    }

    #[test]
    fn settings_include_the_shell_overrides_manual_row() {
        let rows = resolve_rows(&Config::default(), 0);
        assert!(rows.iter().any(|r| r.label == SHELL_OVERRIDES_MANUAL_LABEL));
    }

    #[test]
    fn tab_color_rules_info_row_shows_zero_by_default() {
        let rows = resolve_rows(&Config::default(), 0);
        let r = rows
            .iter()
            .find(|r| r.label.starts_with("Tab color rules"))
            .expect("tab color rules row present");
        assert_eq!(r.label, "Tab color rules (0)");
        assert_eq!(r.kind, RowKind::ReadOnly);
    }

    #[test]
    fn tab_color_rules_info_row_reflects_the_configured_count() {
        let mut c = Config::default();
        c.tab_colors.rules = vec![
            TabColorRule {
                pattern: "^ssh ".to_string(),
                color: "#ff0000".to_string(),
            },
            TabColorRule {
                pattern: "prod".to_string(),
                color: "#00ff00".to_string(),
            },
        ];
        let rows = resolve_rows(&c, 0);
        assert!(
            rows.iter()
                .any(|r| r.label == "Tab color rules (2)" && r.kind == RowKind::ReadOnly)
        );
    }

    #[test]
    fn tab_color_rules_info_row_is_shown_even_when_zero() {
        // Unlike "Delete saved sessions", this row is always present — "no
        // rules configured" is informational too, not something to hide.
        let rows = resolve_rows(&Config::default(), 0);
        assert!(rows.iter().any(|r| r.label == "Tab color rules (0)"));
    }

    #[test]
    fn tab_color_rules_row_immediately_follows_the_knob_row() {
        let rows = resolve_rows(&Config::default(), 0);
        let knob_idx = rows
            .iter()
            .position(|r| r.label == SHELL_OVERRIDES_MANUAL_LABEL)
            .expect("knob row present");
        assert_eq!(rows[knob_idx + 1].label, "Tab color rules (0)");
    }
}
