//! User configuration: a small TOML file the user can edit instead of recompiling.
//!
//! `~/.config/harness-terminal/config.toml` (same dir as session persistence). Loaded once at
//! startup; everything there has a safe default so a missing or malformed file is a no-op. We only
//! support what's cheap to wire and genuinely useful pre-1.0 — the font size a fresh window opens
//! at (overridden by later Ctrl+= / Ctrl+-), and the engine the new-session picker starts with.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};

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
    /// In-memory scrollback limit in lines held per session's terminal grid (the history you can
    /// scroll/find/copy). Absent uses the alacritty default (10000 lines). Lower it to cap memory
    /// on long agent runs; raise it to keep more context for scrollback search/export. Stop-and-go
    /// history is bounded regardless, so this only caps what stays in RAM.
    #[serde(default)]
    pub scrollback_lines: Option<usize>,
    /// Directory new local (PTY) tabs start in, overriding the app's own cwd. A diver who keeps one
    /// repo open can set this so a fresh `prefix+n` tab lands in the repo instead of wherever the
    /// binary was launched. Absent/empty = use the app's current working directory.
    #[serde(default)]
    pub start_cwd: Option<String>,
    /// How many seconds a live, backgrounded, unprotected tab must sit without producing output
    /// before it counts as "quiet" (a likely-done / waiting-for-input signal) in the fleet triage
    /// and `prefix+z` jump. Absent = 120s. Large values damp a chatty fleet; small values surface
    /// a stuck run faster.
    #[serde(default)]
    pub quiet_after_secs: Option<u64>,
    /// Prefix chord: the key pressed with Ctrl to enter tmux-style command mode. Default `h`
    /// (Ctrl+H — "Ctrl Harness"; tmux uses Ctrl+B). The special literals `space` and `\` name
    /// those chords, otherwise any single character works (case-insensitive). Ctrl+Space and
    /// Ctrl+\ are ALWAYS accepted as fallback chords too, so macOS's claim on Ctrl+Space (its
    /// input-source switcher, when a second layout is enabled) can never fully break the prefix —
    /// this option only renames the advertised primary, not the safety nets. Absent = "h".
    #[serde(default)]
    pub prefix_key: Option<String>,
    /// Optional color theme. Absent (or a broken `[theme]` block) keeps the built-in palette.
    #[serde(default)]
    pub theme: Option<Theme>,
    /// Optional prefix-key remapping: an action name -> the key that triggers it after the
    /// prefix chord (`Ctrl+H` by default — see `prefix_key`; `Ctrl+Space`/`Ctrl+\` are the fixed
    /// fallback chords).
    /// Only actions named here that exist are remapped; everything else keeps its default. Absent
    /// (or an empty block) = today's exact keybindings. See `crate::keys`.
    #[serde(default)]
    pub keybindings: Option<BTreeMap<String, String>>,
    /// Opt into macOS native window-level tabs (the system title-bar tab bar) instead of the
    /// framebuffer-drawn tab strip. When enabled, each session gets its own real `NSWindow` and they
    /// are grouped into AppKit's native tab set. Absent defaults to `false` — the in-app tab strip —
    /// which is the always-works fallback.
    #[serde(default)]
    pub native_tabs: Option<bool>,
}

/// A user-configurable color theme. Every field is optional; unset entries keep the built-in
/// default palette, so a partial `[theme]` block works. Colors are `[r, g, b]`, each 0–255.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct Theme {
    /// A named preset palette to start from (`tokyo-night`, `gruvbox-dark`, `solarized-dark`,
    /// `nord`, `dracula`, `github-dark`). Any field set below (foreground/background/ansi/…) layers
    /// on top of the preset; absent/unknown falls back to the built-in `tokyo-night` defaults.
    #[serde(default)]
    pub preset: Option<String>,
    /// Default foreground (normal text). Falls back to the light `0xEAEAEA`.
    pub foreground: Option<[u8; 3]>,
    /// Default background. Falls back to black.
    pub background: Option<[u8; 3]>,
    /// Cursor color used for underline/beam cursors. Falls back to the foreground.
    pub cursor: Option<[u8; 3]>,
    /// Text-selection highlight background. Falls back to soft blue.
    pub selection: Option<[u8; 3]>,
    /// Copy-mode read cursor block color. Falls back to bright green.
    pub copy_cursor: Option<[u8; 3]>,
    /// Per-engine accent tints (the inactive-tab label color), keyed by engine id (`claude`,
    /// `codex`, …). Absent or unknown engines keep their built-in brand color. Sparse map.
    #[serde(default)]
    pub accents: std::collections::BTreeMap<String, [u8; 3]>,
    /// Overrides for the 16-color ANSI palette. Only `Some` slots override the built-in defaults;
    /// the rest keep the classic palette. Index order: black, red, green, yellow, blue, magenta,
    /// cyan, white, bright black, bright red, … bright white.
    ///
    /// Accepts either a sparse `[theme.ansi]` map (`0 = [r,g,b]`, …) or a full inline 16-entry
    /// array. Unset slots default to `None` (keep the built-in color).
    #[serde(default, deserialize_with = "deserialize_ansi")]
    pub ansi: Option<[Option<[u8; 3]>; 16]>,
}

/// Deserialize `ansi` from either a sparse map (`0 = [r,g,b]`, `1 = …`, up to 15) or a full
/// 16-element array. Only present slots are written; the rest stay `None` for the built-in palette.
#[allow(clippy::type_complexity)]
fn deserialize_ansi<'de, D>(d: D) -> Result<Option<[Option<[u8; 3]>; 16]>, D::Error>
where
    D: Deserializer<'de>,
{
    struct V;
    impl<'de> serde::de::Visitor<'de> for V {
        type Value = Option<[Option<[u8; 3]>; 16]>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a map of index -> [r,g,b] or an array of 16 colors")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Self::Value, A::Error> {
            let mut arr = [None; 16];
            while let Some((k, v)) = map.next_entry::<String, [u8; 3]>()? {
                if let Ok(i) = k.parse::<usize>() {
                    if i < 16 {
                        arr[i] = Some(v);
                    }
                }
            }
            Ok(Some(arr))
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut arr = [None; 16];
            let mut i = 0;
            while let Some(v) = seq.next_element::<Option<[u8; 3]>>()? {
                if i < 16 {
                    arr[i] = v;
                }
                i += 1;
            }
            Ok(Some(arr))
        }
    }
    d.deserialize_map(V)
}

impl Default for Config {
    fn default() -> Self {
        Config {
            font_px: 14,
            default_engine: "claude".to_string(),
            font_path: None,
            scrollback_cap: None,
            scrollback_lines: None,
            start_cwd: None,
            quiet_after_secs: None,
            theme: None,
            keybindings: None,
            prefix_key: None,
            native_tabs: None,
        }
    }
}

impl Config {
    /// Load from the conventional config path. Missing file or bad TOML yields defaults (never
    /// errors) — a broken config must not stop the terminal from opening.
    pub fn load() -> Config {
        let path = crate::restore::config_dir().join("config.toml");
        let Ok(raw) = std::fs::read_to_string(path) else {
            return Config::default();
        };
        toml::from_str(&raw).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fully-specified config round-trips, and absent keys fall back to defaults.
    #[test]
    fn parses_and_defaults_fields() {
        let c: Config = toml::from_str(
            r#"
            font_px = 18
        "#,
        )
        .unwrap();
        assert_eq!(c.font_px, 18);
        assert_eq!(c.default_engine, "claude"); // absent key -> default
        assert_eq!(c.font_path, None); // absent -> None (platform default)
    }

    /// A custom font path is honoured and round-trips.
    #[test]
    fn custom_font_path_roundtrips() {
        let c: Config = toml::from_str(
            r#"
            font_path = "/Users/d/.fonts/JetBrainsMono-Nerd-Font.ttf"
        "#,
        )
        .unwrap();
        assert_eq!(
            c.font_path.as_deref(),
            Some("/Users/d/.fonts/JetBrainsMono-Nerd-Font.ttf")
        );
    }

    /// An explicit scrollback cap round-trips; absent falls back to None (built-in default).
    #[test]
    fn scrollback_cap_roundtrips() {
        let c: Config = toml::from_str("scrollback_cap = 1048576").unwrap();
        assert_eq!(c.scrollback_cap, Some(1_048_576));
        let d: Config = toml::from_str("font_px = 14").unwrap();
        assert_eq!(d.scrollback_cap, None);
    }

    /// An explicit in-memory scrollback line limit round-trips; absent falls back to None.
    #[test]
    fn scrollback_lines_roundtrips() {
        let c: Config = toml::from_str("scrollback_lines = 50000").unwrap();
        assert_eq!(c.scrollback_lines, Some(50_000));
        let d: Config = toml::from_str("font_px = 14").unwrap();
        assert_eq!(d.scrollback_lines, None);
    }

    /// A quiet threshold round-trips; absent falls back to None (default 120s in the UI).
    #[test]
    fn quiet_after_secs_roundtrips() {
        let c: Config = toml::from_str("quiet_after_secs = 60").unwrap();
        assert_eq!(c.quiet_after_secs, Some(60));
        let d: Config = toml::from_str("font_px = 14").unwrap();
        assert_eq!(d.quiet_after_secs, None);
    }

    /// A start-working-directory round-trips; absent falls back to None (app cwd).
    #[test]
    fn start_cwd_roundtrips() {
        let c: Config = toml::from_str(r#"start_cwd = "/Users/d/dev/harness-terminal""#).unwrap();
        assert_eq!(
            c.start_cwd.as_deref(),
            Some("/Users/d/dev/harness-terminal")
        );
        let d: Config = toml::from_str("font_px = 14").unwrap();
        assert_eq!(d.start_cwd, None);
    }

    /// Malformed TOML must fall back to defaults, never panic.
    #[test]
    fn malformed_toml_falls_back() {
        let c: Config = toml::from_str("font_px = ]bad").unwrap_or_default();
        assert_eq!(c.font_px, 14);
    }

    /// Absent `theme` -> default (built-in palette).
    #[test]
    fn absent_theme_is_none() {
        let c: Config = toml::from_str("font_px = 14").unwrap();
        assert_eq!(c.theme, None);
    }

    /// Partial theme: only foreground set; the rest keep their None (built-in defaults).
    #[test]
    fn partial_theme_parses() {
        let c: Config = toml::from_str(
            r#"
            [theme]
            foreground = [255, 0, 128]
        "#,
        )
        .unwrap();
        let t = c.theme.expect("theme should parse");
        assert_eq!(t.foreground, Some([255, 0, 128]));
        assert_eq!(t.background, None);
        assert_eq!(t.cursor, None);
        assert_eq!(t.ansi, None);
        assert!(t.accents.is_empty(), "absent accents stay empty");
    }

    /// Full theme with ANSI overrides round-trips.
    #[test]
    fn full_theme_roundtrips() {
        let src = r#"
            [theme]
            foreground = [250, 250, 250]
            background = [10, 10, 10]
            cursor = [0, 255, 0]
            selection = [30, 40, 50]
            copy_cursor = [255, 0, 255]

            [theme.ansi]
            0 = [0, 0, 0]
            1 = [200, 0, 0]
            9 = [255, 80, 80]
            15 = [255, 255, 255]
        "#;
        let c: Config = toml::from_str(src).unwrap();
        let t = c.theme.expect("theme should parse");
        assert_eq!(t.foreground, Some([250, 250, 250]));
        assert_eq!(t.background, Some([10, 10, 10]));
        assert_eq!(t.cursor, Some([0, 255, 0]));
        assert_eq!(t.selection, Some([30, 40, 50]));
        assert_eq!(t.copy_cursor, Some([255, 0, 255]));
        let ansi = t.ansi.expect("ansi should parse");
        assert_eq!(ansi[0], Some([0, 0, 0]));
        assert_eq!(ansi[1], Some([200, 0, 0]));
        assert_eq!(ansi[2], None); // unset slot keeps built-in default
        assert_eq!(ansi[9], Some([255, 80, 80]));
        assert_eq!(ansi[15], Some([255, 255, 255]));
    }

    /// Malformed theme block falls back to defaults (never panics, theme stays as-typed or None).
    #[test]
    fn malformed_theme_falls_back() {
        let c: Config = toml::from_str("[theme]\nforeground = [9999]\n").unwrap_or_default();
        // A bad value type means the whole theme block is rejected; config itself must still parse.
        assert!(c.font_px == 14);
    }

    /// A `[keybindings]` block parses; absent falls back to None (default keys).
    #[test]
    fn keybindings_parse_and_default() {
        let c: Config =
            toml::from_str("[keybindings]\nnew_session = \"N\"\nsearch = \"f\"\n").unwrap();
        let kb = c.keybindings.expect("keybindings should parse");
        assert_eq!(kb.get("new_session").map(String::as_str), Some("N"));
        assert_eq!(kb.get("search").map(String::as_str), Some("f"));
        let d: Config = toml::from_str("font_px = 14").unwrap();
        assert_eq!(d.keybindings, None);
    }

    /// A `[theme.accents]` block overrides per-engine tab tints; absent keyed entries keep defaults.
    #[test]
    fn theme_accents_parse() {
        let c: Config = toml::from_str(
            r#"
            [theme]
            foreground = [0, 0, 0]

            [theme.accents]
            claude = [255, 0, 0]
            codex = [0, 255, 0]
        "#,
        )
        .unwrap();
        let t = c.theme.expect("theme should parse");
        assert_eq!(t.accents.get("claude"), Some(&[255, 0, 0]));
        assert_eq!(t.accents.get("codex"), Some(&[0, 255, 0]));
        assert!(
            !t.accents.contains_key("opencode"),
            "absent engine stays unset"
        );
    }
}
