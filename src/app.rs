//! The application state: a collection of session tabs (the fleet) plus the UI's transient state.
//!
//! Model: `TAB = SESSION = PANE@HOST`. `App` owns `Vec<Session>` (one per tab) and renders the
//! active one. The palette is a flat index of every session across the fleet.

use crate::engines::ENGINES;
use crate::session::{Session, SessionMeta, TermSize};

/// Which overlay the TUI is currently showing on top of the terminal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Overlay {
    None,
    /// Session palette (fuzzy-find any session, jump to it).
    Palette,
    /// New-session picker (choose engine + host).
    NewSession,
    /// Remote-attach: type a host, choose an engine, attach to a pane@host on the fleet.
    RemoteAttach,
    /// Search scrollback: type is a query; Enter jumps to next match, Esc closes.
    Find,
    /// Fleet-wide search: query matches against EVERY open session's scrollback at once; pick a
    /// hit and Enter jumps to that session scrolled to the match. Esc closes.
    FleetSearch,
    /// Read-only fleet status: all harness sessions on this machine (fetch on open).
    Fleet,
    /// Keybinding reference overlay (dismiss on any key).
    Help,
    /// Rename the active tab: type a name, Enter commits, Esc cancels.
    Rename,
    /// Broadcast: type one line, Enter sends it to EVERY open session (with a trailing newline).
    Broadcast,
    /// Peek: a picker of every session with a tail preview of its last lines; Enter jumps to it.
    Peek,
    /// Fleet grid: a live multi-pane overview (Prefix+E) showing every session's live tail at once —
    /// a war-room grid. Updates each frame; 1-9 / j / k navigate; Enter dives into the focused tab.
    FleetGrid,
    /// Command palette: a typed list of named prefix-commands; Enter runs the selected action.
    CommandPalette,
    /// Session info: read-only details about the active tab (kind, host, engine, size, reconnect).
    Info,
}

pub struct App {
    pub tabs: Vec<Session>,
    pub active: usize,
    pub overlay: Overlay,
    /// Current palette/new-session query + selection row.
    pub query: String,
    pub selected: usize,
    /// Filtered candidate indices into `tabs` (for palette) — recomputed each render.
    pub filtered: Vec<usize>,
    /// Terminal geometry every new tab starts at.
    pub size: TermSize,
    /// Host text entry for the remote-attach overlay.
    pub remote_host: String,
    /// Cached harness fleet status (best-effort from the local daemon's /api/status).
    pub fleet: crate::harness::FleetStatus,
    /// Where the background fleet-status fetcher writes its latest snapshot. The main loop only
    /// takes from here (never blocks on the HTTP fetch), so a wedged daemon can't freeze the UI.
    fleet_cache: std::sync::Arc<std::sync::Mutex<Option<crate::harness::FleetStatus>>>,
    /// Time of the next reconnect sweep (monotonic), so a dead daemon can't hammer it every frame.
    next_reconnect: std::time::Instant,
    /// The most recently closed tab's spec, so `prefix+u` can undo a mistaken close.
    pub last_closed: Option<crate::restore::TabSpec>,
    /// Monotonic counter bumped on every spawn; per-engine value records when each framework was
    /// last used so the new-session picker can float recently-used engines to the top.
    spawn_counter: u64,
    /// engine id -> last spawn tick, for picker recency ordering.
    pub engine_last_used: std::collections::HashMap<String, u64>,
    /// Working directories local tabs were spawned in, most-recent first (MRU). Pre-fills the
    /// new-session picker's `dir:` so respawning in the same repo is one Enter. Cap 8.
    pub last_dirs: Vec<String>,
    /// Remote hosts/sessions the diver attached to, most-recent first (MRU). Pre-fills the
    /// Remote-Attach overlay so re-connecting to the same server is one Enter.
    pub recent_hosts: Vec<String>,
    /// How many persisted sessions failed to reopen at launch because their transport/host was
    /// unreachable. Set by the launcher before the app draws; the first frame flashes a one-time
    /// notice so an offline server isn't silently dropped from view.
    pub startup_offline: usize,
}

impl App {
    pub fn new(size: TermSize) -> App {
        // Sweep the state dir for scrollback/mute entries that no persisted tab references. This
        // runs before any tab is restored, so `load()` (the persisted specs) is exactly the set of
        // identities whose per-tab state we still want — anything else is an orphan and goes.
        let alive = crate::restore::load();
        let alive: Vec<(&str, &str, &str)> = alive
            .iter()
            .map(|s| (s.kind.as_str(), s.host.as_str(), s.engine.as_str()))
            .collect();
        crate::restore::cleanup_orphans(&alive);
        // Load persisted engine recency so the picker keeps its ordering across restarts. The live
        // counter resumes past the max stored tick so new spawns keep counting strictly upward.
        let mut recency = crate::restore::load_engine_recency();
        let counter = recency.values().copied().max().unwrap_or(0);
        if counter == 0 {
            recency.clear(); // no stored ticks yet — keep the picker in configured-default order
        }
        App {
            tabs: Vec::new(),
            active: 0,
            overlay: Overlay::None,
            query: String::new(),
            selected: 0,
            filtered: Vec::new(),
            size,
            remote_host: String::new(),
            fleet: crate::harness::FleetStatus::default(),
            fleet_cache: std::sync::Arc::new(std::sync::Mutex::new(None)),
            next_reconnect: std::time::Instant::now(),
            last_closed: None,
            spawn_counter: counter,
            engine_last_used: recency,
            last_dirs: crate::restore::load_recent_dirs(),
            recent_hosts: crate::restore::load_recent_hosts(),
            startup_offline: 0,
        }
    }

    /// The session currently focused, if any.
    pub fn active_session(&self) -> Option<&Session> {
        self.tabs.get(self.active)
    }
    pub fn active_session_mut(&mut self) -> Option<&mut Session> {
        self.tabs.get_mut(self.active)
    }

    /// Create a new session tab running a local engine, and focus it. `cwd` is an optional per-tab
    /// working directory; None falls back to the config `start_cwd` / the binary's cwd.
    pub fn spawn_local(
        &mut self,
        host: &str,
        engine_id: &str,
        cwd: Option<String>,
    ) -> Option<String> {
        let program = engine_cmd(engine_id).unwrap_or("bash");
        let meta = self.meta_for(host, engine_id);
        if let Some(dir) = &cwd {
            // Remember the working dir (MRU, capped) so the next new-session pre-fills it.
            let dir = dir.trim().to_string();
            if !dir.is_empty() {
                self.last_dirs.retain(|d| d != &dir);
                self.last_dirs.insert(0, dir);
                self.last_dirs.truncate(8);
                crate::restore::save_recent_dirs(&self.last_dirs);
            }
        }
        self.push_ok(
            Session::local(meta, program, Vec::new(), self.size, cwd),
            engine_id,
            host,
        )
    }

    /// Create a new session tab running the engine inside a real local tmux pane, and focus it.
    pub fn spawn_tmux(&mut self, host: &str, engine_id: &str) -> Option<String> {
        let program = engine_cmd(engine_id).unwrap_or("bash");
        let meta = self.meta_for(host, engine_id);
        self.push_ok(Session::tmux(meta, program, self.size), engine_id, host)
    }

    /// Create a remote session: the engine's pane runs on `host` (via ssh + tmux control mode).
    pub fn spawn_remote(&mut self, host: &str, engine_id: &str) -> Option<String> {
        let program = engine_cmd(engine_id).unwrap_or("bash");
        let meta = self.meta_for(host, engine_id);
        self.push_ok(Session::remote(meta, program, self.size), engine_id, host)
    }

    /// Create a session over the harness pane-relay tunnel: the pane runs on `host` (the `@host` half
    /// of `pane@host`), reached through that machine's harness daemon at `host:port`. This is
    /// ARCHITECTURE §10 path 1 — the design-specified cross-machine transport.
    pub fn spawn_tunnel(&mut self, host: &str, port: u16, engine_id: &str) -> Option<String> {
        let program = engine_cmd(engine_id).unwrap_or("bash");
        let meta = self.meta_for(host, engine_id);
        let res = self.push_ok(
            Session::tunnel(meta, host, port, program, self.size),
            engine_id,
            host,
        );
        if res.is_none() {
            self.note_remote(&format!("{host}:{port}"));
        }
        res
    }

    /// Attach to an EXISTING named tmux session on `host` through the harness pane-relay tunnel.
    /// No engine runs (we resume whatever is already in the pane), so it's labelled a shell pane;
    /// the session identity rides `meta.attach_session` so it persists and re-attaches on restore.
    pub fn spawn_tunnel_attach(&mut self, host: &str, port: u16, session: &str) -> Option<String> {
        let meta = SessionMeta {
            host: host.to_string(),
            engine: "shell".to_string(),
            title: format!("attach {session} @ {host}"),
            name: None,
        };
        match Session::tunnel_attach(meta, host, port, session, self.size) {
            Ok(s) => {
                // No engine recency bump — attaching isn't a framework spawn.
                self.tabs.push(s);
                self.active = self.tabs.len() - 1;
                crate::restore::save(&self.tab_specs());
                self.note_remote(&format!("{host}:{port}/{session}"));
                None
            }
            Err(e) => Some(format!("attach {session}@{host}: {e}")),
        }
    }

    fn meta_for(&self, host: &str, engine_id: &str) -> SessionMeta {
        SessionMeta {
            host: host.to_string(),
            engine: engine_id.to_string(),
            title: format!("{} @ {}", engine_id, host),
            name: None,
        }
    }

    /// Auto-heal dead tabs: any tmux/ssh/tunnel transport whose connection or pane dropped gets
    /// re-attached (same identity, same grid). Runs from the main loop at a throttled rate so a
    /// temporarily-unreachable daemon retries rather than spinning. Local PTY tabs are no-ops.
    pub fn reconnect_sweep(&mut self) {
        self.reconnect_sweep_refresh();
    }

    /// The throttled sweep: reconnect dead tabs and refresh the fleet/link-health status so the
    /// status line shows a live tunnel badge. Shared so the native loop calls one entry point.
    pub fn reconnect_sweep_refresh(&mut self) {
        if std::time::Instant::now() < self.next_reconnect {
            return;
        }
        self.next_reconnect = std::time::Instant::now() + std::time::Duration::from_secs(5);
        for i in 0..self.tabs.len() {
            let dead = !self.tabs[i].alive();
            if dead {
                match self.tabs[i].reconnect() {
                    Ok(()) => {}
                    // Re-spawn failed — leave it dead and retry on the next sweep.
                    Err(e) => {
                        let _ = e;
                    }
                }
            }
        }
        // Refresh link-health + fleet state used by the status-line badge and the fleet overlay.
        // The fetch runs on a background thread and lands in a shared cache, so a wedged daemon
        // can't stall the main loop for the HTTP timeout; we just take whatever snapshot arrived.
        if let Some(st) = self
            .fleet_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            self.fleet = st;
        }
        crate::harness::HarnessClient::local()
            .status_into(std::sync::Arc::clone(&self.fleet_cache));
    }

    /// Refresh the fleet status for the fleet overlay WITHOUT blocking the main thread. The
    /// blocking `status()` would freeze the whole terminal for the full HTTP timeout when the
    /// local daemon is wedged (accepts the connection but stops responding) — the same freeze the
    /// periodic sweep already routes around. Instead we take whatever snapshot the background
    /// fetcher has landed in `fleet_cache` (populated every frame) and kick a fresh fetch so the
    /// overlay's data is close to live and the UI never stalls.
    pub fn refresh_fleet_nonblocking(&mut self) {
        if let Some(st) = self
            .fleet_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            self.fleet = st;
        }
        crate::harness::HarnessClient::local()
            .status_into(std::sync::Arc::clone(&self.fleet_cache));
    }

    /// Record that `engine_id` was just used, so the new-session picker can float it to the top.
    pub fn note_engine_used(&mut self, engine_id: &str) {
        self.spawn_counter += 1;
        self.engine_last_used
            .insert(engine_id.to_string(), self.spawn_counter);
    }

    /// Persist the recency map so a relaunch keeps the picker's ordering. Called from the spawn
    /// paths (not from [`note_engine_used`]) so tests that bump recency by hand don't hit the disk.
    pub fn persist_engine_recency(&self) {
        crate::restore::save_engine_recency(&self.engine_last_used);
    }

    /// Remember a remote host/session the diver connected to (most-recent first, capped at 8) so
    /// the Remote-Attach overlay can pre-fill it next time. Best-effort persistence.
    pub fn note_remote(&mut self, addr: &str) {
        let addr = addr.trim().to_string();
        if addr.is_empty() {
            return;
        }
        self.recent_hosts.retain(|h| h != &addr);
        self.recent_hosts.insert(0, addr);
        self.recent_hosts.truncate(8);
        crate::restore::save_recent_hosts(&self.recent_hosts);
    }

    /// Picker order for the 12 engines: most-recently-used first, ties broken alphabetically by id.
    pub fn engine_order(&self) -> Vec<&'static crate::engines::Engine> {
        let mut v: Vec<&'static crate::engines::Engine> = crate::engines::ENGINES.iter().collect();
        v.sort_by(|a, b| {
            let ra = self.engine_last_used.get(a.id).copied().unwrap_or(0);
            let rb = self.engine_last_used.get(b.id).copied().unwrap_or(0);
            rb.cmp(&ra).then(a.id.cmp(b.id))
        });
        v
    }

    /// Push a newly-spawned session tab (focused) on success; on failure return the error message so
    /// the caller can surface it in-UI (a remote attach/spawn that silently does nothing leaves a
    /// diver guessing why their tab never appeared).
    fn push_ok(
        &mut self,
        res: std::io::Result<Session>,
        engine_id: &str,
        host: &str,
    ) -> Option<String> {
        match res {
            Ok(session) => {
                self.note_engine_used(engine_id);
                self.persist_engine_recency();
                self.tabs.push(session);
                self.active = self.tabs.len() - 1;
                None
            }
            Err(e) => Some(format!("spawn {engine_id}@{host}: {e}")),
        }
    }

    // ── palette helpers ──────────────────────────────────────────────────

    /// Recompute `filtered` (tab indices) matching the current query, substring-based.
    pub fn refresh_filter(&mut self) {
        let q = self.query.to_lowercase();
        self.filtered = (0..self.tabs.len())
            .filter(|&i| {
                let s = &self.tabs[i];
                let name = s.meta.name.clone().unwrap_or_default();
                let hay = format!(
                    "{} {} {} {}",
                    s.meta.host, s.meta.engine, name, s.meta.title
                )
                .to_lowercase();
                q.is_empty() || crate::native::fuzzy_match(&q, &hay)
            })
            .collect();
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
    }

    /// Persist every open tab's scrollback (kind+host+engine → text file) so a restart can replay
    /// it. Runs on close-quit and on Ctrl+C quit; local "pty" tabs re-spawn with a fresh prompt (not
    /// a real pane), so their history is deliberately skipped to avoid replaying a stale prompt.
    pub fn save_all_scrollbacks(&self) {
        for s in &self.tabs {
            let kind = s.kind();
            if kind == "pty" {
                continue; // no real pane to re-attach; a replayed prompt would just mislead.
            }
            let text = s.capture_scrollback();
            crate::restore::save_scrollback(kind, &s.meta.host, &s.meta.engine, &text);
        }
    }

    /// Move the active tab left/right in the bar by `delta` (-1/1), keeping it focused. Lets a
    /// diver arrange the fleet bar (frequently-dropped-into sessions toward the front) without
    /// closing/reopening. Clamps at the edges and tracks the active index.
    pub fn move_tab(&mut self, delta: isize) {
        let len = self.tabs.len();
        if len < 2 {
            return;
        }
        let to = self.active as isize + delta;
        if to < 0 || to >= len as isize {
            return;
        }
        let to = to as usize;
        self.tabs.swap(self.active, to);
        self.active = to;
    }

    /// Move the tab at `from` to final position `to` in the bar (both original indices), used by
    /// drag-to-reorder. Unlike [`move_tab`] (which swaps the *active* tab), this relocates an
    /// arbitrary tab to an arbitrary slot. The active index is tracked through the remove/insert so
    /// focus stays on the same session it was on; clamping makes out-of-range calls no-ops.
    pub fn move_tab_from_to(&mut self, from: usize, to: usize) {
        let len = self.tabs.len();
        if len < 2 || from == to || from >= len || to >= len {
            return;
        }
        let spec = self.tabs.remove(from);
        // Inserting at `to` after the removal places the moved tab at final index `to` (the array
        // is a slot shorter, so any `to < len` is a valid insertion point).
        self.tabs.insert(to, spec);
        // Re-anchor the focused session. It is the moved tab itself (now at `to`), or one of the
        // neighbors the move shifted: moving right shifts indices in `(from, to]` left by one,
        // moving left shifts `[to, from)` right by one. Anything else is untouched.
        let a = self.active;
        self.active = if a == from {
            to
        } else if from < to && a > from && a <= to {
            a - 1
        } else if from > to && a >= to && a < from {
            a + 1
        } else {
            a
        };
    }

    /// Undo the most recent tab close, re-spawning the same identity (same pane@host / engine) and
    /// focusing it. No-op when nothing has been closed or the re-spawn fails.
    pub fn reopen_last_closed(&mut self) {
        if let Some(spec) = self.last_closed.clone() {
            self.last_closed = None;
            let before = self.tabs.len();
            self.restore_tab(&spec);
            if self.tabs.len() > before {
                self.active = self.tabs.len() - 1;
                crate::restore::save(&self.tab_specs());
            }
        }
    }

    /// Focus the currently-selected palette entry.
    pub fn jump_to_selection(&mut self) {
        if let Some(&i) = self.filtered.get(self.selected) {
            self.active = i;
        }
        self.overlay = Overlay::None;
    }

    /// Duplicate the active tab: spawn a fresh session with the same identity (same transport kind,
    /// host, engine) and focus it. Effectively a "fork this session" — a diver running one agent can
    /// branch a second pane of the same engine on the same machine without re-picking through the
    /// new-session overlay. The local PTY kind is excluded because it owns a bring-up `program`;
    /// there's no single program to re-run, so we only clone tmux/ssh/tunnel (which re-attach to a
    /// real pane). No-op when there's no active tab or the clone fails.
    pub fn duplicate_active(&mut self) {
        let Some(active) = self.active_session() else {
            return;
        };
        let kind = active.kind();
        // Local PTYs can't be re-created generically (they'd need the original program/args to
        // re-run); pane-backed kinds just re-attach to a fresh clone, so clone those only.
        if kind != "tmux" && kind != "ssh" && kind != "tunnel" {
            return;
        }
        let spec = crate::restore::TabSpec {
            kind: kind.to_string(),
            host: active.meta.host.clone(),
            engine: active.meta.engine.clone(),
            port: active.port(),
            session: active.attach_session.clone(),
            name: None,
        };
        let before = self.tabs.len();
        self.restore_tab(&spec);
        if self.tabs.len() > before {
            self.active = self.tabs.len() - 1;
            crate::restore::save(&self.tab_specs());
        }
    }

    /// Reopen a previously-persisted tab. Chooses the right transport from `kind` so a session
    /// comes back with the same identity (local pane vs remote ssh vs tunnel). Best-effort: a
    /// failed re-spawn just leaves no tab.
    /// Returns `true` if the session was reopened, `false` if its host/transport was unreachable
    /// (the launcher counts these to show an offline notice).
    pub fn restore_tab(&mut self, spec: &crate::restore::TabSpec) -> bool {
        let program = engine_cmd(&spec.engine).unwrap_or("bash");
        let meta = self.meta_for(&spec.host, &spec.engine);
        let res = match spec.kind.as_str() {
            "tmux" => Session::tmux(meta, program, self.size),
            "ssh" => Session::remote(meta, program, self.size),
            "tunnel" => {
                // A tunnel tab opened as "attach existing session" resumes that exact named session
                // (no kill/recreate); a plain tunnel spawns a fresh `auton-<engine>` as before.
                if let Some(sess) = spec.session.as_deref() {
                    Session::tunnel_attach(
                        meta,
                        &spec.host,
                        spec.port.unwrap_or(crate::harness::HARNESS_PORT_DEFAULT),
                        sess,
                        self.size,
                    )
                } else {
                    Session::tunnel(
                        meta,
                        &spec.host,
                        spec.port.unwrap_or(crate::harness::HARNESS_PORT_DEFAULT),
                        program,
                        self.size,
                    )
                }
            }
            _ => Session::local(meta, program, Vec::new(), self.size, None),
        };
        let Ok(mut session) = res else {
            return false;
        };
        session.meta.name = spec.name.clone();
        // Replay the persisted scrollback into the fresh emulator so a session comes back with
        // its history intact (before live bytes arrive; the reconnect sweep appends on top).
        let history = crate::restore::load_scrollback(&spec.kind, &spec.host, &spec.engine);
        session.restore_history(&history);
        self.tabs.push(session);
        // Don't steal focus on restore — keep whatever was active (usually tab 0) meaningful.
        if self.tabs.len() == 1 {
            self.active = 0;
        }
        true
    }
}

impl App {
    /// The engine id selected in the new-session picker.
    pub fn selected_engine(&self) -> Option<&'static str> {
        ENGINES
            .get(self.selected.min(ENGINES.len() - 1))
            .map(|e| e.id)
    }

    /// Set the picker selection when the overlay opens: the most recently used engine if any
    /// (position in the recency-ordered picker), else the configured default engine (case-
    /// insensitive), else index 0. Recent-then-default means a diver's usual engine is pre-selected
    /// without extra keystrokes, and it adapts when they switch their main framework.
    pub fn select_default_engine(&mut self) {
        let ordered = self.engine_order();
        let recent = self
            .engine_last_used
            .iter()
            .max_by_key(|(_, &t)| t)
            .map(|(id, _)| id.to_lowercase());
        self.selected = if let Some(id) = recent {
            ordered
                .iter()
                .position(|e| e.id.to_lowercase() == id)
                .unwrap_or(0)
        } else {
            let want = crate::config::Config::load().default_engine.to_lowercase();
            ordered
                .iter()
                .position(|e| e.id.eq_ignore_ascii_case(&want))
                .unwrap_or(0)
        };
    }
}

/// Map an engine id to its launch command.
fn engine_cmd(id: &str) -> Option<&'static str> {
    if id == "shell" {
        return Some("bash");
    }
    ENGINES.iter().find(|e| e.id == id).map(|e| e.cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A headless App with `n` throwaway local sessions (each spawns a fast `true`). Built by hand
    /// rather than via `App::new` so it never touches the config dir (no HARNESS_CONFIG_DIR redirect,
    /// hence no cross-module env-var race with restore.rs's parallel tests).
    fn app_with(n: usize) -> App {
        let size = TermSize {
            lines: 24,
            cols: 80,
        };
        let mut app = App {
            tabs: Vec::new(),
            active: 0,
            overlay: Overlay::None,
            query: String::new(),
            selected: 0,
            filtered: Vec::new(),
            size,
            remote_host: String::new(),
            fleet: crate::harness::FleetStatus::default(),
            fleet_cache: std::sync::Arc::new(std::sync::Mutex::new(None)),
            next_reconnect: std::time::Instant::now(),
            last_closed: None,
            spawn_counter: 0,
            engine_last_used: std::collections::HashMap::new(),
            last_dirs: Vec::new(),
            recent_hosts: Vec::new(),
            startup_offline: 0,
        };
        for i in 0..n {
            let meta = SessionMeta {
                host: "h".into(),
                engine: format!("e{i}"),
                title: format!("e{i} @ h"),
                name: None,
            };
            if let Ok(s) = Session::local(meta, "/usr/bin/true", Vec::new(), app.size, None) {
                app.tabs.push(s);
            }
        }
        app
    }

    /// Reordering a tab keeps the active session's identity focused (not its slot).
    #[test]
    fn move_from_to_tracks_active_index() {
        let mut app = app_with(4);
        app.active = 2; // focus e2 initially
                        // Move e2 (slot 2) to the front (slot 0).
        app.move_tab_from_to(2, 0);
        assert_eq!(app.tabs[0].meta.engine, "e2");
        assert_eq!(app.tabs[1].meta.engine, "e0");
        assert_eq!(app.tabs[2].meta.engine, "e1");
        assert_eq!(app.tabs[3].meta.engine, "e3");
        assert_eq!(
            app.tabs[app.active].meta.engine, "e2",
            "focus follows the dragged session"
        );
    }

    /// Moving a tab past the active index shifts the active index down by one (the removed front
    /// slot reduces indices above it).
    #[test]
    fn move_from_to_forward_past_active_shifts_active() {
        let mut app = app_with(4);
        app.active = 1; // focus e1 (slot 1)
                        // Move e0 (slot 0) past the active to the end.
        app.move_tab_from_to(0, 3);
        assert_eq!(app.tabs[0].meta.engine, "e1");
        assert_eq!(app.tabs[1].meta.engine, "e2");
        assert_eq!(app.tabs[2].meta.engine, "e3");
        assert_eq!(app.tabs[3].meta.engine, "e0");
        assert_eq!(
            app.tabs[app.active].meta.engine, "e1",
            "active e1 shifted from slot 1 to slot 0"
        );
    }

    /// Duplicating a local PTY tab is a no-op: there's no generic program/args to re-run, so we
    /// only fork pane-backed (tmux/ssh/tunnel) sessions. Tab count and focus stay untouched.
    #[test]
    fn duplicate_local_pty_is_noop() {
        let mut app = app_with(3);
        app.active = 1;
        let before = app.tabs.len();
        app.duplicate_active();
        assert_eq!(app.tabs.len(), before, "local PTY must not be duplicated");
        assert_eq!(app.active, 1, "focus untouched");
    }

    /// A restored tunnel tab whose host/port is unreachable must NOT reopen a broken tab: it
    /// returns false (and leaves the tab list empty) so the launcher can count it as offline and
    /// surface a notice. Uses a just-closed ephemeral port so connection-refused is guaranteed.
    #[test]
    fn restore_unreachable_tunnel_returns_false() {
        // Bind then drop an ephemeral port so it is guaranteed closed for the connect attempt.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let mut app = app_with(0);
        let spec = crate::restore::TabSpec {
            kind: "tunnel".into(),
            host: "127.0.0.1".into(),
            engine: "shell".into(),
            port: Some(port),
            session: None,
            name: None,
        };
        let ok = app.restore_tab(&spec);
        assert!(!ok, "unreachable tunnel must not reopen as a broken tab");
        assert!(app.tabs.is_empty(), "no tab should be left behind");
    }

    /// The engine picker floats the most-recently-used engine to the top, ties broken
    /// alphabetically (and never-used ones sort after every used one).
    #[test]
    fn engine_order_puts_most_recent_first() {
        let mut app = app_with(1);
        app.note_engine_used("grok");
        app.note_engine_used("claude");
        let ordered: Vec<&str> = app.engine_order().iter().map(|e| e.id).collect();
        assert_eq!(ordered[0], "claude", "most recently used floats to top");
        assert!(
            ordered.iter().position(|&i| i == "grok").unwrap()
                < ordered.iter().position(|&i| i == "devin").unwrap()
        );
        // select_default_engine picks the most recent, not the configured default.
        app.selected = 99; // sentinel; will be overwritten
        app.select_default_engine();
        assert_eq!(app.engine_order()[app.selected].id, "claude");
    }

    /// No-op reorders (same slot, out of range, single tab) never change the list.
    #[test]
    fn move_from_to_noop_cases() {
        let mut app = app_with(1);
        app.move_tab_from_to(0, 0);
        assert_eq!(app.tabs.len(), 1);

        let mut a2 = app_with(3);
        a2.move_tab_from_to(0, 9); // out of range
        assert_eq!(a2.tabs[0].meta.engine, "e0");
        a2.move_tab_from_to(9, 0); // from out of range
        assert_eq!(a2.tabs.len(), 3);
    }

    /// Remote-host MRU: most-recent first, deduped on re-use, capped at 8, empty strings skipped.
    #[test]
    fn note_remote_tracks_mru_dedup_capped() {
        let mut app = app_with(0);
        assert!(app.recent_hosts.is_empty());
        app.note_remote(" b ");
        app.note_remote("");
        app.note_remote("10.0.0.4:18473/claude");
        app.note_remote("10.0.0.4:18473/claude"); // re-use -> move to front, not duplicate
        app.note_remote("builder:1543");
        assert_eq!(
            app.recent_hosts,
            vec![
                "builder:1543".to_string(),
                "10.0.0.4:18473/claude".to_string(),
                "b".to_string(), // " b " trimmed, pushed before the empties
            ]
        );
        // Cap at 8 total.
        let mut a8 = app_with(0);
        for i in 0..10 {
            a8.note_remote(&format!("host{i}"));
        }
        assert_eq!(a8.recent_hosts.len(), 8);
        assert_eq!(a8.recent_hosts[0], "host9");
        assert!(!a8.recent_hosts.contains(&"host0".to_string()));
    }

    /// Spawning a local tab in a cwd records it MRU-first, dedups the old entry, and caps at 8 — the
    /// new-session picker pre-fills `last_dirs[0]`.
    #[test]
    fn spawn_local_tracks_recent_dirs_mru_capped() {
        let mut app = app_with(0);
        // A blank/explicit cwd is remembered; repeated spawns float to the front without duplicating.
        app.spawn_local("this-host", "claude", Some("/a".to_string()));
        app.spawn_local("this-host", "claude", Some("/b".to_string()));
        app.spawn_local("this-host", "claude", Some("/a".to_string()));
        assert_eq!(app.last_dirs, vec!["/a".to_string(), "/b".to_string()]);
        // Blank cwd is not recorded (picker keeps its pre-fill).
        app.spawn_local("this-host", "claude", None);
        assert_eq!(app.last_dirs, vec!["/a".to_string(), "/b".to_string()]);
        // Cap at 8.
        let mut app8 = app_with(0);
        for i in 0..12 {
            app8.spawn_local("this-host", "claude", Some(format!("/d{i}")));
        }
        assert_eq!(app8.last_dirs.len(), 8);
        assert_eq!(app8.last_dirs[0], "/d11");
        assert!(!app8.last_dirs.contains(&"/d0".to_string()));
    }

    /// A pinned tab refuses close (`close_tab` returns false and leaves the tab), while an
    /// unpinned one closes normally. Mirrors the prefix+`close_tab` guard in native.rs.
    #[test]
    fn close_refuses_pinned_tab() {
        let mut app = app_with(2);
        app.active = 1;
        let before = app.tabs.len();
        // Pinned: refuse — returns false, tab count unchanged.
        assert!(!crate::native::close_tab(&mut app, true));
        assert_eq!(app.tabs.len(), before, "pinned tab must not close");
        // Unpinned: closes — returns true, tab drops.
        assert!(crate::native::close_tab(&mut app, false));
        assert_eq!(app.tabs.len(), before - 1);
    }
    /// Exhaustive check: for every (from, to, active) move, `move_tab_from_to` must (a) produce the
    /// exact reordered bar (remove `from`, insert at `to`), and (b) keep focus on the same session
    /// identity — never on a different session. Catches any latent off-by-one in the index shim.
    #[test]
    fn move_from_to_exhaustive_focus_identity() {
        for n in 2..=5usize {
            let orig: Vec<String> = (0..n).map(|i| format!("e{i}")).collect();
            for from in 0..n {
                for to in 0..n {
                    if from == to {
                        continue;
                    }
                    for active in 0..n {
                        let mut app = app_with(n);
                        app.active = active;
                        let focused = app.tabs[active].meta.engine.clone();
                        app.move_tab_from_to(from, to);

                        // (a) exact reorder
                        let mut expect = orig.clone();
                        let v = expect.remove(from);
                        expect.insert(to, v);
                        let got: Vec<String> =
                            app.tabs.iter().map(|t| t.meta.engine.clone()).collect();
                        assert_eq!(got, expect, "n={n} move {from}->{to} active={active}");

                        // (b) focus identity preserved
                        assert!(
                            app.active < app.tabs.len(),
                            "active in range n={n} move {from}->{to} active={active}"
                        );
                        assert_eq!(
                            app.tabs[app.active].meta.engine, focused,
                            "focus keeps identity n={n} move {from}->{to} active={active}"
                        );
                    }
                }
            }
        }
    }
}
