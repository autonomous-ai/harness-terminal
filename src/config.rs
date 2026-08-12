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
    /// Optional path to a TTF/OTF monospace font to render the grid with. Absent/empty falls back
    /// to the platform default (macOS SF Mono), which `HARNESS_FONT` can override in a pinch.
    #[serde(default)]
    pub font_path: Option<String>,
    /// Cap on persisted per-tab scrollback, in bytes. Absent uses the built-in default (~256KB);
    /// raise it to keep more cross-restart history, or lower it to trim disk use. Only the tail
    /// beyond the cap is dropped, so the newest lines always survive.
    #[serde(default)]
    pub scrollback_cap: Option<usize>,
    /// Directory new local (PTY) tabs start in, overriding the app's own cwd. A diver who keeps one
    /// repo open can set this so a fresh `prefix+n` tab lands in the repo instead of wherever the
    /// binary was launched. Absent/empty = use the app's current working directory.
    #[serde(default)]
    pub start_cwd: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            font_px: 14,
            default_engine: "claude".to_string(),
            font_path: None,
            scrollback_cap: None,
            start_cwd: None,
        }
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
        assert_eq!(c.font_path, None); // absent -> None (platform default)
    }

    /// A custom font path is honoured and round-trips.
    #[test]
    fn custom_font_path_roundtrips() {
        let c: Config = toml::from_str(r#"
            font_path = "/Users/d/.fonts/JetBrainsMono-Nerd-Font.ttf"
        "#).unwrap();
        assert_eq!(c.font_path.as_deref(), Some("/Users/d/.fonts/JetBrainsMono-Nerd-Font.ttf"));
    }

    /// An explicit scrollback cap round-trips; absent falls back to None (built-in default).
    #[test]
    fn scrollback_cap_roundtrips() {
        let c: Config = toml::from_str("scrollback_cap = 1048576").unwrap();
        assert_eq!(c.scrollback_cap, Some(1_048_576));
        let d: Config = toml::from_str("font_px = 14").unwrap();
        assert_eq!(d.scrollback_cap, None);
    }

    /// A start-working-directory round-trips; absent falls back to None (app cwd).
    #[test]
    fn start_cwd_roundtrips() {
        let c: Config = toml::from_str(r#"start_cwd = "/Users/d/dev/harness-terminal""#).unwrap();
        assert_eq!(c.start_cwd.as_deref(), Some("/Users/d/dev/harness-terminal"));
        let d: Config = toml::from_str("font_px = 14").unwrap();
        assert_eq!(d.start_cwd, None);
    }

    /// Malformed TOML must fall back to defaults, never panic.
    #[test]
    fn malformed_toml_falls_back() {
        let c: Config = toml::from_str("font_px = ]bad").unwrap_or_default();
        assert_eq!(c.font_px, 14);
    }
}
