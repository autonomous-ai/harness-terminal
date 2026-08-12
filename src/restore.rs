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
/// macOS — a conventional, per-user location we can write without prompting.
pub fn config_dir() -> std::path::PathBuf {
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

#[cfg(test)]
mod tests {
    use super::*;

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
        // Point at a guaranteed-nonexistent config path by redirecting HOME, then verify load()
        // returns empty instead of erroring.
        let stash = std::env::var_os("HOME");
        let tmp = std::env::temp_dir().join(format!("ht-restore-test-{}", std::process::id()));
        std::env::set_var("HOME", &tmp);
        let result = std::panic::catch_unwind(load);
        // Restore HOME first so later tests aren't affected.
        match stash {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }
}
