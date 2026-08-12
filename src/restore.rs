//! Session persistence: remember open tabs across restarts.
//!
//! A fleet-dive terminal is used to keep a handful of sessions warm (a local engine, one or two
//! remote panes). Losing them on quit is annoying. We persist a small JSON file describing each
//! open tab — its transport kind, host and engine — and re-hydrate them on the next launch. Local
//! PTY panes re-spawn in place; tmux/ssh/tunnel panes re-attach to the same pane@host identity
//! (the grid starts empty and the reconnect sweep re-fills it).

use serde::{Deserialize, Serialize};

use crate::app::App;

/// A persisted tab descriptor. `kind` matches the transport's `kind()` ("pty"/"tmux"/"ssh"/"tunnel").
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TabSpec {
    pub kind: String,
    pub host: String,
    pub engine: String,
    /// Tunnel/remote ports; only meaningful for "tunnel".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

impl App {
    /// Capture the current tab list as persistable specs (kind + host + engine).
    pub fn tab_specs(&self) -> Vec<TabSpec> {
        self.tabs
            .iter()
            .map(|s| TabSpec {
                kind: s.kind().to_string(),
                host: s.meta.host.clone(),
                engine: s.meta.engine.clone(),
                port: None,
            })
            .collect()
    }
}

// ── config-file path ───────────────────────────────────────────────────────

/// The config dir for this app: `~/.config/harness-terminal` on Linux/BSD, `~/Library/...` on
/// macOS — a conventional, per-user location we can write without prompting. An explicit
/// `HARNESS_CONFIG_DIR` overrides it (used by tests to isolate, handy for portable/CI setups too).
pub fn config_dir() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("HARNESS_CONFIG_DIR") {
        return std::path::PathBuf::from(dir);
    }
    let base = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    // Keep it simple and predictable across OSes (the app is pre-1.0; a proper dirs crate can
    // replace this later).
    base.join(".config").join("harness-terminal")
}

fn state_path() -> std::path::PathBuf {
    config_dir().join("tabs.json")
}

/// Save the given tab specs to disk. Best-effort: a full disk / missing home dir must not crash
/// the terminal.
pub fn save(specs: &[TabSpec]) {
    let path = state_path();
    if let Some(dir) = path.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
    }
    let _ = std::fs::write(&path, serde_json::to_string_pretty(specs).unwrap_or_default());
}

/// Load previously-saved tab specs. Empty vec on any error (missing file, bad JSON).
pub fn load() -> Vec<TabSpec> {
    let path = state_path();
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn zoom_path() -> std::path::PathBuf {
    config_dir().join("zoom.json")
}

/// Persist the font-zoom factor so a window comes back at the size the user left it.
/// (`Ctrl+=`/`Ctrl+-`/`Ctrl+0` adjust it at runtime.) Best-effort like [`save`].
pub fn save_zoom(zoom: f32) {
    if !(0.5..=3.0).contains(&zoom) {
        return;
    }
    let _ = std::fs::create_dir_all(config_dir());
    let _ = std::fs::write(zoom_path(), format!("{}", zoom));
}

/// Load the persisted font-zoom factor. 1.0 on error (missing/corrupt file), clamped to range.
pub fn load_zoom() -> f32 {
    let Ok(raw) = std::fs::read_to_string(zoom_path()) else { return 1.0 };
    raw.trim().parse::<f32>().unwrap_or(1.0).clamp(0.5, 3.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run a closure with `HARNESS_CONFIG_DIR` pointed at a throwaway dir, then remove it. Gives
    /// each file-backed test its own isolated config dir so parallel tests never fight over HOME.
    fn with_isolated_dir<F: FnOnce(&std::path::Path) + std::panic::UnwindSafe>(f: F) {
        let dir = std::env::temp_dir().join(format!("ht-test-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("HARNESS_CONFIG_DIR", &dir);
        let r = std::panic::catch_unwind(|| f(&dir));
        std::env::remove_var("HARNESS_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(r.is_ok());
    }

    /// A spec round-trips through JSON losslessly, including a tunnelled (port-carrying) tab.
    #[test]
    fn tab_spec_roundtrips_through_json() {
        let specs = vec![
            TabSpec { kind: "pty".into(), host: "this-host".into(), engine: "shell".into(), port: None },
            TabSpec { kind: "tunnel".into(), host: "10.0.0.4".into(), engine: "codex".into(), port: Some(4321) },
        ];
        let json = serde_json::to_string(&specs).unwrap();
        let back: Vec<TabSpec> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].kind, "pty");
        assert_eq!(back[1].kind, "tunnel");
        assert_eq!(back[1].host, "10.0.0.4");
        assert_eq!(back[1].port, Some(4321));
    }

    /// Missing/absent state file loads as an empty vec, never an error.
    #[test]
    fn load_of_missing_file_is_empty() {
        with_isolated_dir(|_| {
            assert!(load().is_empty());
        });
    }

    /// Font zoom round-trips through its own file, and is clamped to the usable range.
    #[test]
    fn zoom_roundtrips_and_is_clamped() {
        with_isolated_dir(|_| {
            save_zoom(1.7);
            let back = load_zoom();
            // Out-of-range save is refused.
            save_zoom(9.0);
            let still = load_zoom();
            assert_eq!(back, 1.7);
            // A refused save must not clobber the previously-persisted value.
            assert_eq!(still, 1.7);
        });
    }
}
