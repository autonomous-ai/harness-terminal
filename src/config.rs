//! User configuration: a small TOML file the user can edit instead of recompiling.
//!
//! `~/.config/harness-terminal/config.toml` (same dir as session persistence). Loaded once at
//! startup; everything there has a safe default so a missing or malformed file is a no-op. We only
//! support what's cheap to wire and genuinely useful pre-1.0 — the font size a fresh window opens
//! at (overridden by later Ctrl+= / Ctrl+-), and the engine the new-session picker starts with.

use serde::{Deserialize, Serialize};

/// The whole config file. All fields optional with defaults: `Config::default()` is what you get
/// when the file is absent or unparseable.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Base font size in px the window opens at (before display-scale/zoom factor).
    pub font_px: usize,
    /// Engine the new-session picker starts selected on (case-insensitive; falls back to index 0).
    pub default_engine: String,
}

impl Default for Config {
    fn default() -> Self {
        Config { font_px: 14, default_engine: "claude".to_string() }
    }
}

impl Config {
    /// Load from the conventional config path. Missing file or bad TOML yields defaults (never
    /// errors) — a broken config must not stop the terminal from opening.
    pub fn load() -> Config {
        let path = crate::restore::config_dir().join("config.toml");
        let Ok(raw) = std::fs::read_to_string(path) else { return Config::default() };
        toml::from_str(&raw).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fully-specified config round-trips, and absent keys fall back to defaults.
    #[test]
    fn parses_and_defaults_fields() {
        let c: Config = toml::from_str(r#"
            font_px = 18
        "#).unwrap();
        assert_eq!(c.font_px, 18);
        assert_eq!(c.default_engine, "claude"); // absent key -> default
    }

    /// Malformed TOML must fall back to defaults, never panic.
    #[test]
    fn malformed_toml_falls_back() {
        let c: Config = toml::from_str("font_px = ]bad").unwrap_or_default();
        assert_eq!(c.font_px, 14);
    }
}
