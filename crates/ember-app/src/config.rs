//! Config file load/save (the IO half of [`ember_core::Config`], which is pure).
//! Lives at `$XDG_CONFIG_HOME/ember/config.toml` (else `~/.config/ember/config.toml`).
//! Loading is forgiving: a missing or malformed file yields defaults, so Ember
//! always starts and the config stays optional.

use std::path::PathBuf;

use ember_core::Config;

/// The config file path, if a home/config dir can be determined.
pub fn path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(xdg).join("ember/config.toml"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/ember/config.toml"))
}

/// Load the config, falling back to defaults on any missing/parse error.
pub fn load() -> Config {
    let Some(p) = path() else {
        return Config::default();
    };
    match std::fs::read_to_string(&p) {
        Ok(s) => match toml::from_str(&s) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("[ember] config parse error ({}): {e}", p.display());
                Config::default()
            }
        },
        Err(_) => Config::default(),
    }
}

/// Write the config back to disk (creating the directory). Best-effort.
pub fn save(cfg: &Config) -> std::io::Result<()> {
    let Some(p) = path() else {
        return Ok(());
    };
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = toml::to_string_pretty(cfg).map_err(std::io::Error::other)?;
    std::fs::write(p, text)
}
