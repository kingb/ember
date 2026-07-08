//! User configuration (design §9). Pure, serde-ready, zero IO — the file load/
//! save lives in the app layer. Every field has a default so a missing or partial
//! `config.toml` still yields a valid `Config` (`#[serde(default)]`), keeping the
//! config passive/optional and headless runs deterministic.

use serde::{Deserialize, Serialize};

/// The full user configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub background: Background,
    /// Terminal font (family + size). Size is the live-zoom baseline (Cmd+0).
    pub font: Font,
    /// Visual bell: a terminal BEL flashes an ember pulse + lights the tab that
    /// belled, instead of an audible beep. On by default.
    pub visual_bell: bool,
    /// Auto-inject OSC 133 shell integration (exit-status gutter + jump-to-prompt)
    /// into spawned zsh/bash without editing the user's rc. On by default.
    pub shell_integration: bool,
    /// macOS: treat Option as Meta — Opt+key sends `ESC key` (readline `M-b`,
    /// emacs `M-x`) instead of composing (`å`, `é`). Off by default, matching
    /// Terminal.app/Alacritty/kitty/Ghostty; composing wins for most users.
    pub option_as_meta: bool,
    /// Developer Mode: enables the debug control socket (drive + screenshot the
    /// live window from `ember-term ctl` / MCP) — and a home for future dev
    /// features. OFF by default: it's a keystroke-injection + screen-read
    /// surface. Toggle in Settings when you want to hand off inspection.
    pub developer_mode: bool,
    /// THE WISP (release 2 surface-drag task 5): a small glowing drag token
    /// that follows the pointer while a tab/pane is being carried across
    /// windows. Purely decorative — every drag mechanic (drop resolution,
    /// hover previews, cross-window tracking) behaves IDENTICALLY whether
    /// this is on or off, or if the GPU can't support the alpha compositing
    /// it needs (feature-detected on first use, degrading silently for the
    /// rest of the session). On by default.
    pub wisp: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            background: Background::default(),
            font: Font::default(),
            visual_bell: true,
            shell_integration: true,
            option_as_meta: false,
            developer_mode: false,
            wisp: true,
        }
    }
}

/// Terminal font configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Font {
    /// Font family name (e.g. "Menlo", "JetBrains Mono"). `None`/missing → the
    /// platform monospace default. A missing/unresolvable name falls back to it.
    pub family: Option<String>,
    /// Baseline point size (the Cmd+0 reset target). Clamped to a sane range.
    pub size: f32,
}

impl Default for Font {
    fn default() -> Self {
        Self {
            family: None,
            size: 12.0,
        }
    }
}

/// The ember-sparks dial (sparks guardrails, v0.3.1): how the drifting-ember
/// animation reacts to window focus and system power state.
///
/// Serialized lowercase (`"off"` / `"focused"` / `"always"`). **Backcompat:**
/// a `config.toml` written by a pre-v0.3.1 Ember has `ember_sparks = true` or
/// `= false` under `[background]`, not a `sparks` string — the custom
/// deserializer on [`Background::sparks`] accepts both the old bool and the
/// new string, mapping `true` → [`Focused`](SparksMode::Focused) and `false`
/// → [`Off`](SparksMode::Off), so an old config still loads. A config saved
/// by this version onward always writes the new string form.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SparksMode {
    /// Never animate the sparks. The gradient/scrim still draw (static, free).
    Off,
    /// Animate only in the focused window; unfocused windows keep their
    /// sparks visible but frozen. The shipping default: the campfire burns
    /// where you're looking, not behind your back.
    #[default]
    Focused,
    /// Animate in every visible window, focused or not — the pre-v0.3.1
    /// "campfire burns while you work elsewhere" behavior (Brandon's
    /// original 2026-07-04 call), now opt-in rather than the default.
    Always,
}

/// Accepts either the old `ember_sparks` boolean or the new `sparks` string
/// dial for [`Background::sparks`] — see that field's doc comment.
fn deserialize_sparks_mode<'de, D>(deserializer: D) -> Result<SparksMode, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum BoolOrMode {
        Bool(bool),
        Mode(SparksMode),
    }
    Ok(match BoolOrMode::deserialize(deserializer)? {
        BoolOrMode::Bool(true) => SparksMode::Focused,
        BoolOrMode::Bool(false) => SparksMode::Off,
        BoolOrMode::Mode(m) => m,
    })
}

/// Ambient backdrop + ember-glow appearance (the campfire aesthetic).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Background {
    /// Draw a dark warm vertical gradient behind the cells.
    pub gradient: bool,
    /// Darkening scrim over the backdrop for text legibility (`0.0`–`1.0`).
    pub scrim: f32,
    /// The ember-sparks dial: `off` | `focused` | `always` (default
    /// `focused`). Accepts the pre-v0.3.1 `ember_sparks` boolean too — see
    /// [`SparksMode`]'s doc comment for the migration mapping.
    #[serde(alias = "ember_sparks", deserialize_with = "deserialize_sparks_mode")]
    pub sparks: SparksMode,
    /// Spark count/rate multiplier (`0.0`–`2.0`).
    pub ember_density: f32,
    /// Frame-rate cap for the ember animation (fps). Lower = less CPU; the
    /// velocity trails keep the drift smooth at 15 (the default). Clamped to a
    /// sane range by the app.
    pub ember_fps: u32,
    /// Path to a backdrop image (e.g. a fire photo) drawn behind the cells. When
    /// set, it replaces the gradient; the scrim still darkens it for legibility.
    /// `None`/missing → no image (the gradient/sparks path is used instead).
    pub image: Option<String>,
    /// How the backdrop image fills the window: `cover` | `contain` | `stretch` |
    /// `tile`. Set in `config.toml` (not the Settings overlay).
    pub image_fit: String,
}

impl Default for Background {
    fn default() -> Self {
        Self {
            // The warm gradient is Ember's signature look and draws statically
            // (no continuous redraw), so it's on out of the box. The sparks
            // animation defaults to `focused` (v0.3.0 shipped plain-on at
            // 15fps trails, measured ~2% of a core + tens of mW GPU while
            // visible, 0% occluded): it animates only the window you're
            // actually looking at,
            // and the guardrails (Low Power Mode, Reduce Motion) pause it
            // further when the system asks for less power/motion.
            gradient: true,
            scrim: 0.45,
            sparks: SparksMode::Focused,
            ember_density: 1.0,
            ember_fps: 15,
            image: None,
            image_fit: "cover".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_light_the_campfire_cheaply() {
        let c = Config::default();
        // Conservative on *power*: the gradient is on (static draw, free);
        // sparks default to `focused` (animates only the window you're
        // looking at, guarded further by Low Power Mode/Reduce Motion).
        assert!(c.background.gradient);
        assert_eq!(c.background.sparks, SparksMode::Focused);
        assert_eq!(c.background.ember_density, 1.0);
        assert_eq!(c.background.ember_fps, 15);
        assert!(c.visual_bell); // visual bell on by default
        assert!(c.wisp); // decorative-only; safe to default on
    }

    #[test]
    fn partial_toml_fills_defaults() {
        // Only one key set; the rest must fall back to defaults.
        let c: Config = toml::from_str("[background]\nsparks = \"always\"\n").unwrap();
        assert_eq!(c.background.sparks, SparksMode::Always);
        assert!(c.background.gradient);
        assert_eq!(c.background.scrim, 0.45);
        assert_eq!(c.background.image, None);
        assert_eq!(c.background.image_fit, "cover");
    }

    #[test]
    fn roundtrips_through_toml() {
        let c = Config::default();
        let s = toml::to_string(&c).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(c, back);
    }

    // --- sparks dial: string form + bool backcompat -------------------------

    #[test]
    fn sparks_missing_defaults_to_focused() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.background.sparks, SparksMode::Focused);
    }

    #[test]
    fn sparks_accepts_the_three_new_strings() {
        for (s, want) in [
            ("off", SparksMode::Off),
            ("focused", SparksMode::Focused),
            ("always", SparksMode::Always),
        ] {
            let toml_src = format!("[background]\nsparks = \"{s}\"\n");
            let c: Config = toml::from_str(&toml_src).unwrap();
            assert_eq!(c.background.sparks, want, "sparks = \"{s}\"");
        }
    }

    #[test]
    fn sparks_accepts_old_bool_true_as_focused() {
        let c: Config = toml::from_str("[background]\nember_sparks = true\n").unwrap();
        assert_eq!(c.background.sparks, SparksMode::Focused);
    }

    #[test]
    fn sparks_accepts_old_bool_false_as_off() {
        let c: Config = toml::from_str("[background]\nember_sparks = false\n").unwrap();
        assert_eq!(c.background.sparks, SparksMode::Off);
    }

    #[test]
    fn sparks_roundtrips_as_a_string_not_a_bool() {
        let mut c = Config::default();
        c.background.sparks = SparksMode::Always;
        let s = toml::to_string(&c).unwrap();
        assert!(s.contains("sparks = \"always\""), "serialized as: {s}");
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.background.sparks, SparksMode::Always);
    }
}
