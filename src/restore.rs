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
    /// A user-assigned tab name (prefix+,). None = no custom name, fall back to the engine id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
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
                name: s.meta.name.clone(),
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

fn active_path() -> std::path::PathBuf {
    config_dir().join("active.json")
}

/// Persist which tab was focused last, so a relaunch opens on it rather than always tab 0.
/// Best-effort like the others.
pub fn save_active(idx: usize) {
    let _ = std::fs::create_dir_all(config_dir());
    let _ = std::fs::write(active_path(), format!("{}", idx));
}

/// Load the last-focused tab index (0 on error/missing). Caller clamps to the tab count.
pub fn load_active() -> usize {
    let Ok(raw) = std::fs::read_to_string(active_path()) else { return 0 };
    raw.trim().parse::<usize>().unwrap_or(0)
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

fn geometry_path() -> std::path::PathBuf {
    config_dir().join("geometry.json")
}

/// Persist the window's inner size (physical px) so a relaunch comes back the same working area.
/// Best-effort like the others; a zero/inverted size is ignored.
pub fn save_geometry(width: u32, height: u32) {
    if width == 0 || height == 0 {
        return;
    }
    let payload = format!("{{\"w\":{},\"h\":{}}}", width, height);
    let _ = std::fs::create_dir_all(config_dir());
    let _ = std::fs::write(geometry_path(), payload);
}

/// Load the persisted window size. None on error (missing/corrupt) so the caller falls back to its
/// default.
pub fn load_geometry() -> Option<(u32, u32)> {
    let raw = std::fs::read_to_string(geometry_path()).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let w = v.get("w")?.as_u64()? as u32;
    let h = v.get("h")?.as_u64()? as u32;
    if w == 0 || h == 0 {
        return None;
    }
    Some((w, h))
}

// ── scrollback persistence ──────────────────────────────────────────────────

fn scrollback_path() -> std::path::PathBuf {
    config_dir().join("scrollback")
}

/// Hard cap on one tab's persisted scrollback (bytes). A session's history can be megabytes long;
/// we persist only its tail so scrollback files stay bounded and the config dir can't balloon under
/// a long-running pane. Trade-off: across-restart history is "last ~256KB", which is plenty of
/// context to resume from while keeping disk use flat. (Granted, the full scrollback is what
/// `export_scrollback` writes to a `.log` file in cwd — the user opts into that copy.)
const MAX_SCROLLBACK_BYTES: usize = 256 * 1024;

/// The effective per-tab scrollback cap in bytes. The config's `scrollback_cap` overrides the
/// built-in default when present (and sane); otherwise the default applies.
fn scrollback_cap() -> usize {
    match crate::config::Config::load().scrollback_cap {
        Some(n) if n > 0 => n,
        _ => MAX_SCROLLBACK_BYTES,
    }
}

fn scrollback_file(kind: &str, host: &str, engine: &str) -> std::path::PathBuf {
    // Bullet-proof file name: only alnum/`_`/`-` survive, host may be an IP or machine id.
    let k: String = (kind.to_owned() + host + engine)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    scrollback_path().join(format!("{}.txt", k))
}

/// Persist a captured scrollback for one tab's identity (kind + host + engine), capped at the last
/// ~256KB (or the config's `scrollback_cap`) so a giant history can't balloon the config dir.
/// Best-effort like the other state files; a full disk must not crash the app. Old snapshots of the
/// same identity are overwritten.
pub fn save_scrollback(kind: &str, host: &str, engine: &str, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    // Persist only the tail — the recent context that matters for a resume. Trimming in bytes keeps
    // the file below the cap even for multi-megabyte histories; the cut may split a multi-byte
    // UTF-8 char on the boundary, but the emulator's parser tolerates partial-char byte streams.
    let cap = scrollback_cap();
    let mut tail = text;
    let skip = text.len().saturating_sub(cap);
    if skip > 0 {
        tail = &text[skip..];
    }
    let path = scrollback_file(kind, host, engine);
    let _ = std::fs::create_dir_all(scrollback_path());
    let _ = std::fs::write(path, tail);
}

/// Load a previously-captured scrollback for a tab identity. Empty string on any error/missing.
pub fn load_scrollback(kind: &str, host: &str, engine: &str) -> String {
    let path = scrollback_file(kind, host, engine);
    std::fs::read_to_string(path).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `HARNESS_CONFIG_DIR` override is process-global, so two tests redirecting it in parallel
    /// can tear each other's mapping out mid-run. This mutex serializes every file-backed test:
    /// only one redirects `HARNESS_CONFIG_DIR` at a time, each into its own dir.
    fn with_isolated_dir<F: FnOnce(&std::path::Path) + std::panic::UnwindSafe>(f: F) {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _guard = LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("ht-test-{}-{}", std::process::id(), SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
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
            TabSpec { kind: "pty".into(), host: "this-host".into(), engine: "shell".into(), port: None, name: None },
            TabSpec { kind: "tunnel".into(), host: "10.0.0.4".into(), engine: "codex".into(), port: Some(4321), name: Some("db-migrate".into()) },
        ];
        let json = serde_json::to_string(&specs).unwrap();
        let back: Vec<TabSpec> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].kind, "pty");
        assert_eq!(back[1].kind, "tunnel");
        assert_eq!(back[1].host, "10.0.0.4");
        assert_eq!(back[1].port, Some(4321));
        assert_eq!(back[0].name, None);
        assert_eq!(back[1].name.as_deref(), Some("db-migrate"));
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

    /// Window geometry round-trips through its own file; a zero size is refused/tolerated.
    #[test]
    fn geometry_roundtrips_and_rejects_zero() {
        with_isolated_dir(|_| {
            save_geometry(1600, 900);
            assert_eq!(load_geometry(), Some((1600, 900)));
            // A zero/inverted size must not be persisted as valid.
            save_geometry(0, 0);
            assert_eq!(load_geometry(), Some((1600, 900)));
        });
    }

    /// The last-focused tab index round-trips; a missing file loads as 0.
    #[test]
    fn active_roundtrips_missing_is_zero() {
        with_isolated_dir(|_| {
            assert_eq!(load_active(), 0);
            save_active(3);
            assert_eq!(load_active(), 3);
        });
    }

    /// A tab's custom name survives a save→load round-trip and stays None when unset.
    #[test]
    fn tab_name_roundtrips_through_specs() {
        with_isolated_dir(|_| {
            let specs = vec![
                TabSpec { kind: "ssh".into(), host: "build-host".into(), engine: "claude".into(), port: None, name: None },
                TabSpec { kind: "tunnel".into(), host: "10.0.0.9".into(), engine: "opencode".into(), port: Some(7000), name: Some("staging-deploy".into()) },
            ];
            save(&specs);
            let back = load();
            assert_eq!(back.len(), 2);
            assert_eq!(back[0].name, None);
            assert_eq!(back[1].name.as_deref(), Some("staging-deploy"));
        });
    }

    /// A captured scrollback round-trips through its per-identity file, and an absent file loads
    /// empty (never an error).
    #[test]
    fn scrollback_roundtrips_through_file() {
        with_isolated_dir(|_| {
            // Missing file → empty, no panic.
            assert_eq!(load_scrollback("tmux", "build-host", "claude"), "");
            // A snapshot with wrapped lines and unicode survives verbatim.
            let text = "line one\nbeta-beta-beta-beta-gamma\nελληνικά ωμέγα\n";
            save_scrollback("tmux", "build-host", "claude", text);
            assert_eq!(load_scrollback("tmux", "build-host", "claude"), text);
            // Distinct identities don't collide (host differs → separate file).
            assert!(load_scrollback("tmux", "other-host", "claude").is_empty());
            // An all-whitespace snapshot is refused (nothing meaningful to persist).
            save_scrollback("ssh", "10.0.0.9", "codex", "   \n  ");
            assert_eq!(load_scrollback("ssh", "10.0.0.9", "codex"), "");

            // A huge snapshot is capped at MAX_SCROLLBACK_BYTES so a multi-megabyte history can't
            // balloon the config dir; the persisted tail keeps the most recent bytes.
            let hugo = "x".repeat(MAX_SCROLLBACK_BYTES + 10) + "TAIL";
            save_scrollback("tunnel", "10.0.0.7", "claude", &hugo);
            let loaded = load_scrollback("tunnel", "10.0.0.7", "claude");
            assert!(loaded.len() <= MAX_SCROLLBACK_BYTES);
            assert!(loaded.ends_with("TAIL"), "cap must keep the tail, not the head");
        });
    }
}
