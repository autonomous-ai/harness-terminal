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
    /// For a tunnel tab opened as "attach an existing session": the remote tmux session name to
    /// re-attach to. `None` for every other kind (fresh spawns derive their own `auton-<engine>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
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
                port: s.port(),
                session: s.attach_session.clone(),
                name: s.meta.name.clone(),
            })
            .collect()
    }
}

// ── config-file path ───────────────────────────────────────────────────────

// Per-thread override of the config dir, used by the tests so they can isolate the persistence
// directory WITHOUT mutating the process-global `HARNESS_CONFIG_DIR` env var.
//
// The old test approach called `std::env::set_var` to redirect the config dir, which is unsound
// under parallel test execution: another thread reading `config_dir()` concurrently while the env
// is being mutated is undefined behavior in Rust 2021, and it made the suite flaky (intermittent
// panics in the shared file-backed tests). A thread-local override is race-free by construction.
thread_local! {
    static CONFIG_OVERRIDE: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

// `#[cfg(test)]`-only: set/clear this thread's config-dir override.
#[cfg(test)]
fn set_config_override(dir: Option<std::path::PathBuf>) {
    CONFIG_OVERRIDE.with(|c| *c.borrow_mut() = dir);
}

/// The config dir for this app: `~/.config/harness-terminal` on Linux/BSD, `~/Library/...` on
/// macOS — a conventional, per-user location we can write without prompting. An explicit
/// `HARNESS_CONFIG_DIR` overrides it (handy for portable/CI setups too).
pub fn config_dir() -> std::path::PathBuf {
    if let Some(dir) = CONFIG_OVERRIDE.with(|c| c.borrow().clone()) {
        return dir;
    }
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
    let _ = std::fs::write(
        &path,
        serde_json::to_string_pretty(specs).unwrap_or_default(),
    );
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

fn engine_recency_path() -> std::path::PathBuf {
    config_dir().join("engines.json")
}

/// Persist when each engine was last used (monotonic spawn tick -> engine id), so the new-session
/// picker's recency ordering survives a restart. Best-effort like the others.
pub fn save_engine_recency(recency: &std::collections::HashMap<String, u64>) {
    let _ = std::fs::create_dir_all(config_dir());
    let _ = std::fs::write(
        engine_recency_path(),
        serde_json::to_string_pretty(recency).unwrap_or_default(),
    );
}

/// Load engine recency (empty on missing file / bad JSON). Caller seeds the live counter from the
/// max tick so future spawns keep counting strictly upward.
pub fn load_engine_recency() -> std::collections::HashMap<String, u64> {
    let Ok(raw) = std::fs::read_to_string(engine_recency_path()) else {
        return std::collections::HashMap::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn recent_dirs_path() -> std::path::PathBuf {
    config_dir().join("recent-dirs.json")
}

/// Persist the MRU working-directory list (most-recent first, already capped by the caller), so the
/// new-session picker pre-fills the last repo you spawned in. Best-effort like the others.
pub fn save_recent_dirs(dirs: &[String]) {
    let _ = std::fs::create_dir_all(config_dir());
    let _ = std::fs::write(
        recent_dirs_path(),
        serde_json::to_string_pretty(dirs).unwrap_or_default(),
    );
}

/// Load the recent working-directory list (empty on missing file / bad JSON).
pub fn load_recent_dirs() -> Vec<String> {
    let Ok(raw) = std::fs::read_to_string(recent_dirs_path()) else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn recent_hosts_path() -> std::path::PathBuf {
    config_dir().join("recent-hosts.json")
}

/// Persist the MRU remote-host list (most-recent first, already capped by the caller), so the
/// Remote-Attach overlay can pre-fill the last server you attached to. Best-effort like the others.
pub fn save_recent_hosts(hosts: &[String]) {
    let _ = std::fs::create_dir_all(config_dir());
    let _ = std::fs::write(
        recent_hosts_path(),
        serde_json::to_string_pretty(hosts).unwrap_or_default(),
    );
}

/// Load the recent remote-host list (empty on missing file / bad JSON).
pub fn load_recent_hosts() -> Vec<String> {
    let Ok(raw) = std::fs::read_to_string(recent_hosts_path()) else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Persist which tab was focused last, so a relaunch opens on it rather than always tab 0.
/// Best-effort like the others.
pub fn save_active(idx: usize) {
    let _ = std::fs::create_dir_all(config_dir());
    let _ = std::fs::write(active_path(), format!("{}", idx));
}

/// Load the last-focused tab index (0 on error/missing). Caller clamps to the tab count.
pub fn load_active() -> usize {
    let Ok(raw) = std::fs::read_to_string(active_path()) else {
        return 0;
    };
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
    let Ok(raw) = std::fs::read_to_string(zoom_path()) else {
        return 1.0;
    };
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

fn position_path() -> std::path::PathBuf {
    config_dir().join("position.json")
}

/// Persist the window's outer position (top-left, physical px) so a relaunch returns to the same
/// spot on screen, not just the same size. Best-effort like the others. Saved on move and on close.
pub fn save_position(x: i32, y: i32) {
    let payload = format!("{{\"x\":{},\"y\":{}}}", x, y);
    let _ = std::fs::create_dir_all(config_dir());
    let _ = std::fs::write(position_path(), payload);
}

/// Load the persisted window position. None on error/missing so the OS places a fresh window.
pub fn load_position() -> Option<(i32, i32)> {
    let raw = std::fs::read_to_string(position_path()).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let x = v.get("x")?.as_i64()? as i32;
    let y = v.get("y")?.as_i64()? as i32;
    Some((x, y))
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

fn scrollback_file(
    kind: &str,
    host: &str,
    engine: &str,
    attach: Option<&str>,
) -> std::path::PathBuf {
    // Bullet-proof file name: only alnum/`_`/`-` survive, host may be an IP or machine id. For an
    // attach tab the remote session name disambiguates multiple distinct `host/session` agents on
    // the same host+engine (each keeps its own persisted history). Spawns (attach=None) share the
    // same `auton-<engine>` pane, so their identity is exactly kind+host+engine.
    let base = kind.to_owned() + host + engine;
    let with_session = match attach {
        Some(sess) if !sess.trim().is_empty() => base + "--" + sess,
        _ => base,
    };
    let k: String = with_session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    scrollback_path().join(format!("{}.txt", k))
}

/// Persist a captured scrollback for one tab's identity (kind + host + engine), capped at the last
/// ~256KB (or the config's `scrollback_cap`) so a giant history can't balloon the config dir.
/// Best-effort like the other state files; a full disk must not crash the app. Old snapshots of the
/// same identity are overwritten.
pub fn save_scrollback(kind: &str, host: &str, engine: &str, attach: Option<&str>, text: &str) {
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
    let path = scrollback_file(kind, host, engine, attach);
    let _ = std::fs::create_dir_all(scrollback_path());
    let _ = std::fs::write(path, tail);
}

/// Load a previously-captured scrollback for a tab identity. Empty string on any error/missing.
pub fn load_scrollback(kind: &str, host: &str, engine: &str, attach: Option<&str>) -> String {
    let path = scrollback_file(kind, host, engine, attach);
    std::fs::read_to_string(path).unwrap_or_default()
}

/// The file name (relative to the `scrollback/` dir) a tab identity maps to. Public so cleanup can
/// match an on-disk file back to a tab that still references it.
fn scrollback_name(kind: &str, host: &str, engine: &str, attach: Option<&str>) -> String {
    scrollback_file(kind, host, engine, attach)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// Best-effort sweep of the state dir for state that no open/persisted tab references anymore.
///
/// Long-lived scrollback and muted entries accumulate per tab identity; when a session is closed
/// (or a renamed tab's host/engine changes) its `.txt` file and its `kind:host:engine` mute entry
/// become orphans that would otherwise linger forever. At startup we know exactly which identities
/// are still alive — the persisted `TabSpec`s (and any tabs currently open) — so we delete the
/// scrollback files and prune the muted set for everything not referenced. Never errors; a full
/// disk or partial read is just skipped.
///
/// The `alive` slice is the set of identities we want to keep: for every entry its scrollback file
/// and mute entry survive; everything else in the scrollback dir / muted set is removed.
pub fn cleanup_orphans(alive: &[(&str, &str, &str, Option<&str>)]) {
    // Resolve the live identities to their file names / mute keys so we can match on-disk state.
    // The scrollback file name includes the attach session (to distinguish distinct `host/session`
    // agents on one host); the mute key stays kind+host+engine (mute is a per-machine agent toggle).
    let keep_names: Vec<String> = alive
        .iter()
        .map(|(k, h, e, a)| scrollback_name(k, h, e, *a))
        .collect();
    let keep_keys: Vec<String> = alive
        .iter()
        .map(|(k, h, e, a)| mute_key(k, h, e, *a))
        .collect();

    // Scavenge the scrollback dir: delete any *.txt not backed by a live identity.
    if let Ok(entries) = std::fs::read_dir(scrollback_path()) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if name.ends_with(".txt") && !keep_names.contains(&name) {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    // Prune the muted set to only entries that are still live; rewrite only if something changed.
    let current = load_muted();
    let kept: Vec<String> = current
        .into_iter()
        .filter(|k| keep_keys.contains(k))
        .collect();
    if kept.len() != load_muted().len() {
        // Reuse the same write path as save_muted so format/behavior stay identical.
        let pairs: Vec<(&str, &str, &str)> = kept
            .iter()
            .filter_map(|k| {
                let mut it = k.splitn(3, ':');
                Some((it.next()?, it.next()?, it.next()?))
            })
            .collect();
        if pairs.is_empty() {
            let _ = std::fs::remove_file(muted_path());
        } else {
            let _ = std::fs::create_dir_all(config_dir());
            let payload: Vec<String> = pairs
                .iter()
                .map(|(k, h, e)| format!("{k}:{h}:{e}"))
                .collect();
            let _ = std::fs::write(
                muted_path(),
                serde_json::to_string(&payload).unwrap_or_default(),
            );
        }
    }
}

// ── mute persistence ────────────────────────────────────────────────────────

fn muted_path() -> std::path::PathBuf {
    config_dir().join("muted.json")
}

/// The persistent identity key for a muted tab: `kind:host:engine`, plus the remote attach session
/// name (`kind:host:engine/session`) when present, so two distinct `host/session` agents on the same
/// host+engine keep independent mute state. Mirrors the scrollback-identity disambiguation; spawned
/// panes (attach=None) share the single `auton-<engine>` identity as before.
pub fn mute_key(kind: &str, host: &str, engine: &str, attach: Option<&str>) -> String {
    match attach {
        Some(a) if !a.trim().is_empty() => format!("{kind}:{host}:{engine}/{a}"),
        _ => format!("{kind}:{host}:{engine}"),
    }
}

/// Persist the identities of tabs the user has muted (prefix+m), so a tab stays muted across a
/// restart instead of nagging again the moment the window reopens. Best-effort like the other state
/// files; only a non-empty list is written.
pub fn save_muted(kinds_engines: &[(&str, &str, &str, Option<&str>)]) {
    if kinds_engines.is_empty() {
        return;
    }
    let payload: Vec<String> = kinds_engines
        .iter()
        .map(|(k, h, e, a)| mute_key(k, h, e, *a))
        .collect();
    let _ = std::fs::create_dir_all(config_dir());
    let _ = std::fs::write(
        muted_path(),
        serde_json::to_string(&payload).unwrap_or_default(),
    );
}

/// Load the set of muted identity keys, each `kind:host:engine[/session]`. Empty set on error/missing.
pub fn load_muted() -> Vec<String> {
    let Ok(raw) = std::fs::read_to_string(muted_path()) else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

// ── last-broadcast persistence ───────────────────────────────────────────────

fn last_broadcast_path() -> std::path::PathBuf {
    config_dir().join("last-broadcast.json")
}

/// Remember the last broadcast line so the next `prefix+a` pre-fills it (repeating a command to
/// every host survives a restart without retyping). Best-effort like the other state files; an
/// empty line is not written (nothing to pre-fill).
pub fn save_last_broadcast(line: &str) {
    if line.trim().is_empty() {
        return;
    }
    let _ = std::fs::create_dir_all(config_dir());
    let _ = std::fs::write(last_broadcast_path(), line.as_bytes());
}

/// Load the last-sent broadcast line; "" on error/missing/empty.
pub fn load_last_broadcast() -> String {
    let Ok(raw) = std::fs::read_to_string(last_broadcast_path()) else {
        return String::new();
    };
    let s = raw.trim();
    if s.is_empty() {
        String::new()
    } else {
        s.to_string()
    }
}

fn broadcast_history_path() -> std::path::PathBuf {
    config_dir().join("broadcast-history.json")
}

/// Persist the MRU of broadcast lines actually sent (most-recent first, already capped by the
/// caller), so Shift+Up/Down recall survives a restart. Best-effort like the others.
pub fn save_broadcast_history(hist: &[String]) {
    let _ = std::fs::create_dir_all(config_dir());
    let _ = std::fs::write(
        broadcast_history_path(),
        serde_json::to_string_pretty(hist).unwrap_or_default(),
    );
}

/// Load the broadcast history (empty on missing file / bad JSON).
pub fn load_broadcast_history() -> Vec<String> {
    let Ok(raw) = std::fs::read_to_string(broadcast_history_path()) else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

// ── pin persistence ─────────────────────────────────────────────────────────

// ── find history persistence ────────────────────────────────────────────────

fn find_history_path() -> std::path::PathBuf {
    config_dir().join("find-history.json")
}

/// Persist the MRU of find queries actually run (most-recent first, already capped by the caller),
/// so recalling a search (Up in the find bar with an empty query) survives a restart. Best-effort.
pub fn save_find_history(hist: &[String]) {
    let _ = std::fs::create_dir_all(config_dir());
    let _ = std::fs::write(
        find_history_path(),
        serde_json::to_string_pretty(hist).unwrap_or_default(),
    );
}

/// Load the find history (empty on missing file / bad JSON).
pub fn load_find_history() -> Vec<String> {
    let Ok(raw) = std::fs::read_to_string(find_history_path()) else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn pinned_path() -> std::path::PathBuf {
    config_dir().join("pinned.json")
}

/// Persist the identities (kind+host+engine[/session]) of tabs the user pinned (prefix+a), so a
/// pinned agent run stays protected across a restart. A distinct `host/session` agent on the same
/// host+engine keeps its own pin, mirroring the scrollback/mute disambiguation. Unlike muted (which
/// skips empty), an empty list is written too — unpinning the last tab must clear the file, not
/// leave the old set behind.
pub fn save_pinned(kinds_engines: &[(&str, &str, &str, Option<&str>)]) {
    let payload: Vec<String> = kinds_engines
        .iter()
        .map(|(k, h, e, a)| mute_key(k, h, e, *a))
        .collect();
    let _ = std::fs::create_dir_all(config_dir());
    let _ = std::fs::write(
        pinned_path(),
        serde_json::to_string(&payload).unwrap_or_default(),
    );
}

/// Load the set of pinned identity keys, each `kind:host:engine[/session]`. Empty set on error/missing.
pub fn load_pinned() -> Vec<String> {
    let Ok(raw) = std::fs::read_to_string(pinned_path()) else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
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
        let dir = std::env::temp_dir().join(format!(
            "ht-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        // Thread-local, not env: avoids the unsound concurrent `set_var` that made these tests race
        // (and intermittent panics) under parallel execution.
        set_config_override(Some(dir.clone()));
        let r = std::panic::catch_unwind(|| f(&dir));
        set_config_override(None);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(r.is_ok());
    }

    /// A spec round-trips through JSON losslessly, including a tunnelled (port-carrying) tab.
    #[test]
    fn tab_spec_roundtrips_through_json() {
        let specs = vec![
            TabSpec {
                kind: "pty".into(),
                host: "this-host".into(),
                engine: "shell".into(),
                port: None,
                session: None,
                name: None,
            },
            TabSpec {
                kind: "tunnel".into(),
                host: "10.0.0.4".into(),
                engine: "codex".into(),
                port: Some(4321),
                session: Some("agent-runs".into()),
                name: Some("db-migrate".into()),
            },
        ];
        let json = serde_json::to_string(&specs).unwrap();
        let back: Vec<TabSpec> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].kind, "pty");
        assert_eq!(back[1].kind, "tunnel");
        assert_eq!(back[1].host, "10.0.0.4");
        assert_eq!(back[1].port, Some(4321));
        assert_eq!(back[1].session.as_deref(), Some("agent-runs"));
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

    /// Recent remote hosts round-trip through their file in MRU order; missing file -> empty.
    #[test]
    fn recent_hosts_roundtrip_in_order() {
        with_isolated_dir(|_| {
            assert!(load_recent_hosts().is_empty(), "no file yet");
            save_recent_hosts(&["10.0.0.4:18473/claude".to_string(), "builder".to_string()]);
            assert_eq!(
                load_recent_hosts(),
                vec!["10.0.0.4:18473/claude".to_string(), "builder".to_string(),]
            );
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

    /// Engine-recency round-trips; a missing file loads as an empty map.
    #[test]
    fn engine_recency_roundtrips_missing_is_empty() {
        with_isolated_dir(|_| {
            let mut m = std::collections::HashMap::new();
            m.insert("claude".to_string(), 5u64);
            m.insert("grok".to_string(), 9u64);
            save_engine_recency(&m);
            let back = load_engine_recency();
            assert_eq!(back.get("claude"), Some(&5));
            assert_eq!(back.get("grok"), Some(&9));
            assert_eq!(back.len(), 2);
        });
    }

    /// Pinned identities round-trip; an empty save CLEARS the file (unpinning the last tab must not
    /// leave a stale pin set behind — unlike muted which skips empty writes).
    #[test]
    fn pinned_roundtrips_and_empty_clears() {
        with_isolated_dir(|_| {
            save_pinned(&[("tmux", "box1", "claude", None)]);
            assert_eq!(load_pinned(), vec!["tmux:box1:claude".to_string()]);
            save_pinned(&[]);
            assert!(
                load_pinned().is_empty(),
                "empty pin set must clear the file"
            );
        });
    }

    /// A tab's custom name survives a save→load round-trip and stays None when unset.
    #[test]
    fn tab_name_roundtrips_through_specs() {
        with_isolated_dir(|_| {
            let specs = vec![
                TabSpec {
                    kind: "ssh".into(),
                    host: "build-host".into(),
                    engine: "claude".into(),
                    port: None,
                    session: None,
                    name: None,
                },
                TabSpec {
                    kind: "tunnel".into(),
                    host: "10.0.0.9".into(),
                    engine: "opencode".into(),
                    port: Some(7000),
                    session: None,
                    name: Some("staging-deploy".into()),
                },
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
            assert_eq!(load_scrollback("tmux", "build-host", "claude", None), "");
            // A snapshot with wrapped lines and unicode survives verbatim.
            let text = "line one\nbeta-beta-beta-beta-gamma\nελληνικά ωμέγα\n";
            save_scrollback("tmux", "build-host", "claude", None, text);
            assert_eq!(load_scrollback("tmux", "build-host", "claude", None), text);
            // Distinct identities don't collide (host differs → separate file).
            assert!(load_scrollback("tmux", "other-host", "claude", None).is_empty());
            // An all-whitespace snapshot is refused (nothing meaningful to persist).
            save_scrollback("ssh", "10.0.0.9", "codex", None, "   \n  ");
            assert_eq!(load_scrollback("ssh", "10.0.0.9", "codex", None), "");

            // A huge snapshot is capped at MAX_SCROLLBACK_BYTES so a multi-megabyte history can't
            // balloon the config dir; the persisted tail keeps the most recent bytes.
            let hugo = "x".repeat(MAX_SCROLLBACK_BYTES + 10) + "TAIL";
            save_scrollback("tunnel", "10.0.0.7", "claude", None, &hugo);
            let loaded = load_scrollback("tunnel", "10.0.0.7", "claude", None);
            assert!(loaded.len() <= MAX_SCROLLBACK_BYTES);
            assert!(
                loaded.ends_with("TAIL"),
                "cap must keep the tail, not the head"
            );
        });
    }

    /// Distinct remote agents on the same host+engine keep their own persisted history when they
    /// attach distinct tmux sessions (`host/session`), instead of collapsing into one last-writer-wins
    /// file. Spawned panes (attach=None) still share the single `auton-<engine>` identity.
    #[test]
    fn attach_session_disambiguates_scrollback() {
        with_isolated_dir(|_| {
            // Two claude agents on the same host, different remote session names.
            save_scrollback(
                "tunnel",
                "10.0.0.4",
                "claude",
                Some("work-a"),
                "alpha history",
            );
            save_scrollback(
                "tunnel",
                "10.0.0.4",
                "claude",
                Some("work-b"),
                "beta history",
            );
            // A spawn (no attach) on the same host+engine is a third, separate identity.
            save_scrollback("tunnel", "10.0.0.4", "claude", None, "fresh spawn history");

            // Each attach session round-trips its own text; nothing bleeds across sessions.
            assert_eq!(
                load_scrollback("tunnel", "10.0.0.4", "claude", Some("work-a")),
                "alpha history"
            );
            assert_eq!(
                load_scrollback("tunnel", "10.0.0.4", "claude", Some("work-b")),
                "beta history"
            );
            assert_eq!(
                load_scrollback("tunnel", "10.0.0.4", "claude", None),
                "fresh spawn history"
            );

            // cleanup keeps every distinct live identity's file.
            let alive: Vec<(&str, &str, &str, Option<&str>)> = vec![
                ("tunnel", "10.0.0.4", "claude", Some("work-a")),
                ("tunnel", "10.0.0.4", "claude", Some("work-b")),
                ("tunnel", "10.0.0.4", "claude", None),
            ];
            cleanup_orphans(&alive);
            assert_eq!(
                load_scrollback("tunnel", "10.0.0.4", "claude", Some("work-a")),
                "alpha history"
            );
            assert_eq!(
                load_scrollback("tunnel", "10.0.0.4", "claude", Some("work-b")),
                "beta history"
            );
        });
    }

    /// A config `scrollback_cap` override tightens the persisted tail below the built-in default.
    /// Guard against the cap silently ignoring the user's config (which would balloon the dir for
    /// anyone who raised it, or keep too little for anyone who lowered it).
    #[test]
    fn scrollback_cap_honors_config_override() {
        with_isolated_dir(|dir| {
            // The isolated dir isn't pre-created; make it (as the app would) before writing config.
            std::fs::create_dir_all(dir).unwrap();
            // Write a tiny config cap so the built-in 256KB isn't what trims it.
            let cfg = dir.join("config.toml");
            std::fs::write(&cfg, "scrollback_cap = 64\n").unwrap();

            let big = "y".repeat(4096) + "TAIL";
            save_scrollback("ssh", "10.0.0.9", "claude", None, &big);
            let loaded = load_scrollback("ssh", "10.0.0.9", "claude", None);
            assert!(loaded.len() <= 64);
            assert!(
                loaded.ends_with("TAIL"),
                "even a tight override keeps the tail"
            );
        });
    }

    /// Window position round-trips through its file; a missing file loads None (let the OS place it).
    #[test]
    fn position_roundtrips_missing_is_none() {
        with_isolated_dir(|_| {
            assert!(load_position().is_none());
            save_position(320, 180);
            assert_eq!(load_position(), Some((320, 180)));
            // Negative coords (multi-monitor to the upper-left) survive too.
            save_position(-800, 40);
            assert_eq!(load_position(), Some((-800, 40)));
        });
    }

    /// Muted identities round-trip through their file; an absent file loads empty.
    #[test]
    fn muted_roundtrips_through_file() {
        with_isolated_dir(|_| {
            assert!(load_muted().is_empty());
            let keys = vec![
                ("tmux", "build-host", "claude", None),
                ("tunnel", "10.0.0.7", "codex", Some("work-a")),
            ];
            save_muted(&keys);
            let back = load_muted();
            assert_eq!(back.len(), 2);
            assert!(back.contains(&"tmux:build-host:claude".to_string()));
            assert!(
                back.contains(&"tunnel:10.0.0.7:codex/work-a".to_string()),
                "attach session must be part of the mute identity"
            );
            // A stale mute for a vanished tab is harmless — restore matches on exact identity.
            assert!(!back.contains(&"pty:ghost:shell".to_string()));
            // Two distinct agents on the same host+engine keep independent mute state.
            save_muted(&[
                ("tunnel", "10.0.0.7", "codex", Some("work-a")),
                ("tunnel", "10.0.0.7", "codex", Some("work-b")),
            ]);
            let two = load_muted();
            assert_eq!(two.len(), 2);
            assert!(two.contains(&"tunnel:10.0.0.7:codex/work-a".to_string()));
            assert!(two.contains(&"tunnel:10.0.0.7:codex/work-b".to_string()));
        });
    }

    #[test]
    fn last_broadcast_roundtrips_through_file() {
        with_isolated_dir(|_| {
            // Nothing saved yet -> empty pre-fill.
            assert_eq!(load_last_broadcast(), "");
            // Empty lines are not worth persisting.
            save_last_broadcast("");
            assert_eq!(load_last_broadcast(), "");
            // A real repeatable command survives the round trip.
            save_last_broadcast("git pull");
            assert_eq!(load_last_broadcast(), "git pull");
            // A second send overwrites, not appends.
            save_last_broadcast("git pull --rebase");
            assert_eq!(load_last_broadcast(), "git pull --rebase");
        });
    }

    /// cleanup_orphans removes scrollback files and muted entries that no alive identity references,
    /// while keeping the ones that are still live.
    #[test]
    fn cleanup_orphans_removes_stale_and_keeps_live() {
        with_isolated_dir(|_| {
            // Live identities (still persisted/open) and a vanished one.
            save_scrollback("tmux", "build-host", "claude", None, "keep this");
            save_scrollback("tunnel", "10.0.0.7", "codex", None, "keep this too");
            save_scrollback("pty", "ghost", "shell", None, "stale — should be deleted");
            save_muted(&[
                ("tmux", "build-host", "claude", None),
                ("tunnel", "10.0.0.7", "codex", None),
                ("pty", "ghost", "shell", None),
            ]);
            assert_eq!(
                load_scrollback("pty", "ghost", "shell", None),
                "stale — should be deleted"
            );

            // Only the two live identities survive.
            let alive: Vec<(&str, &str, &str, Option<&str>)> = vec![
                ("tmux", "build-host", "claude", None),
                ("tunnel", "10.0.0.7", "codex", None),
            ];
            cleanup_orphans(&alive);

            // Live scrollbacks kept; the ghost's deleted.
            assert_eq!(
                load_scrollback("tmux", "build-host", "claude", None),
                "keep this"
            );
            assert_eq!(
                load_scrollback("tunnel", "10.0.0.7", "codex", None),
                "keep this too"
            );
            assert_eq!(load_scrollback("pty", "ghost", "shell", None), "");

            // Live mutes kept; the ghost's pruned.
            let muted = load_muted();
            assert!(muted.contains(&"tmux:build-host:claude".to_string()));
            assert!(muted.contains(&"tunnel:10.0.0.7:codex".to_string()));
            assert!(!muted.contains(&"pty:ghost:shell".to_string()));
        });
    }

    /// cleanup_orphans with nothing alive empties the state dir (fully discarded sessions never
    /// come back), including rewinding the muted file entirely.
    #[test]
    fn cleanup_orphans_with_no_live_empties_state() {
        with_isolated_dir(|_| {
            save_scrollback("pty", "solo", "claude", None, "all gone");
            save_muted(&[("pty", "solo", "claude", None)]);
            cleanup_orphans(&[]);
            assert_eq!(load_scrollback("pty", "solo", "claude", None), "");
            assert!(load_muted().is_empty());
        });
    }
}
