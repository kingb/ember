//! User configuration (design §9). Pure, serde-ready, zero IO — the file load/
//! save lives in the app layer. Every field has a default so a missing or partial
//! `config.toml` still yields a valid `Config` (`#[serde(default)]`), keeping the
//! config passive/optional and headless runs deterministic.

use serde::{Deserialize, Serialize};

/// The full user configuration.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub background: Background,
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
