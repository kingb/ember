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
    /// Drifting glowing ember sparks (opt-in; forces continuous redraw).
    pub ember_sparks: bool,
    /// Spark count/rate multiplier (`0.0`–`2.0`).
    pub ember_density: f32,
    /// Frame-rate cap for the ember animation (fps). Lower = less CPU; the sparks
    /// drift fine at 30. Clamped to a sane range by the app.
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
            gradient: false,
            scrim: 0.45,
            ember_sparks: false,
            ember_density: 1.0,
            ember_fps: 30,
            image: None,
            image_fit: "cover".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_conservative() {
        let c = Config::default();
        assert!(!c.background.gradient);
        assert!(!c.background.ember_sparks);
        assert_eq!(c.background.ember_density, 1.0);
        assert_eq!(c.background.ember_fps, 30);
        assert!(c.visual_bell); // visual bell on by default
    }

    #[test]
    fn partial_toml_fills_defaults() {
        // Only one key set; the rest must fall back to defaults.
        let c: Config = toml::from_str("[background]\nember_sparks = true\n").unwrap();
        assert!(c.background.ember_sparks);
        assert!(!c.background.gradient);
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
