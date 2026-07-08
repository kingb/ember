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

/// Ambient backdrop + ember-glow appearance (the campfire aesthetic).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Background {
    /// Draw a dark warm vertical gradient behind the cells.
    pub gradient: bool,
    /// Darkening scrim over the backdrop for text legibility (`0.0`–`1.0`).
    pub scrim: f32,
    /// Drifting glowing ember sparks (velocity trails). On by default: at the
    /// default 15fps the animation measures ~2% of one core and ~tens of mW
    /// of GPU while the window is visible, and zero when occluded. Guardrails
    /// (pause on unfocus / battery / Reduce Motion) arrive in the next patch.
    pub ember_sparks: bool,
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
            // The campfire is Ember's signature and it's lit out of the box:
            // static gradient (free) + spark trails at 15fps (measured ~2% of
            // a core + ~tens of mW GPU while visible; 0% occluded). Trails are
            // what make 15fps look smooth — see spark_quads. The dials remain
            // for anyone who wants it calmer, faster, or off.
            gradient: true,
            scrim: 0.45,
            ember_sparks: true,
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
        // The signature look is on by default, at the measured-cheap settings:
        // gradient (static, free) + sparks at 15fps with trails (~2% of a core
        // visible, 0% occluded). Anything costlier stays a user choice.
        assert!(c.background.gradient);
        assert!(c.background.ember_sparks);
        assert_eq!(c.background.ember_density, 1.0);
        assert_eq!(c.background.ember_fps, 15);
        assert!(c.visual_bell); // visual bell on by default
        assert!(c.wisp); // decorative-only; safe to default on
    }

    #[test]
    fn partial_toml_fills_defaults() {
        // Only one key set; the rest must fall back to defaults.
        let c: Config = toml::from_str("[background]\nember_sparks = true\n").unwrap();
        assert!(c.background.ember_sparks);
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
}
