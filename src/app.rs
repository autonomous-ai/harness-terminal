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
    /// Command palette: a typed list of named prefix-commands; Enter runs the selected action.
    CommandPalette,
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
    /// Time of the next reconnect sweep (monotonic), so a dead daemon can't hammer it every frame.
    next_reconnect: std::time::Instant,
    /// The most recently closed tab's spec, so `prefix+u` can undo a mistaken close.
    pub last_closed: Option<crate::restore::TabSpec>,
}

impl App {
    pub fn new(size: TermSize) -> App {
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
            next_reconnect: std::time::Instant::now(),
            last_closed: None,
        }
    }

    /// The session currently focused, if any.
    pub fn active_session(&self) -> Option<&Session> {
        self.tabs.get(self.active)
    }
    pub fn active_session_mut(&mut self) -> Option<&mut Session> {
        self.tabs.get_mut(self.active)
    }

    /// Create a new session tab running a local engine, and focus it.
    pub fn spawn_local(&mut self, host: &str, engine_id: &str) {
        let program = engine_cmd(engine_id).unwrap_or("bash");
        let meta = self.meta_for(host, engine_id);
        self.push_ok(Session::local(meta, program, Vec::new(), self.size), engine_id, host);
    }

    /// Create a new session tab running the engine inside a real local tmux pane, and focus it.
    pub fn spawn_tmux(&mut self, host: &str, engine_id: &str) {
        let program = engine_cmd(engine_id).unwrap_or("bash");
        let meta = self.meta_for(host, engine_id);
        self.push_ok(Session::tmux(meta, program, self.size), engine_id, host);
    }

    /// Create a remote session: the engine's pane runs on `host` (via ssh + tmux control mode).
    pub fn spawn_remote(&mut self, host: &str, engine_id: &str) {
        let program = engine_cmd(engine_id).unwrap_or("bash");
        let meta = self.meta_for(host, engine_id);
        self.push_ok(Session::remote(meta, program, self.size), engine_id, host);
    }

    /// Create a session over the harness pane-relay tunnel: the pane runs on `host` (the `@host` half
    /// of `pane@host`), reached through that machine's harness daemon at `host:port`. This is
    /// ARCHITECTURE §10 path 1 — the design-specified cross-machine transport.
    pub fn spawn_tunnel(&mut self, host: &str, port: u16, engine_id: &str) {
        let program = engine_cmd(engine_id).unwrap_or("bash");
        let meta = self.meta_for(host, engine_id);
        self.push_ok(
            Session::tunnel(meta, host, port, program, self.size),
            engine_id, host,
        );
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
        if let Ok(st) = crate::harness::HarnessClient::local().status() {
            self.fleet = st;
        }
    }

    fn push_ok(&mut self, res: std::io::Result<Session>, engine_id: &str, host: &str) {
        match res {
            Ok(session) => {
                self.tabs.push(session);
                self.active = self.tabs.len() - 1;
            }
            Err(e) => eprintln!("spawn {engine_id}@{host}: {e}"),
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
                let hay = format!("{} {} {} {}", s.meta.host, s.meta.engine, name, s.meta.title).to_lowercase();
                q.is_empty() || hay.contains(&q)
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

    /// Reopen a previously-persisted tab. Chooses the right transport from `kind` so a session
    /// comes back with the same identity (local pane vs remote ssh vs tunnel). Best-effort: a
    /// failed re-spawn just leaves no tab.
    pub fn restore_tab(&mut self, spec: &crate::restore::TabSpec) {
        let program = engine_cmd(&spec.engine).unwrap_or("bash");
        let meta = self.meta_for(&spec.host, &spec.engine);
        let res = match spec.kind.as_str() {
            "tmux" => Session::tmux(meta, program, self.size),
            "ssh" => Session::remote(meta, program, self.size),
            "tunnel" => Session::tunnel(meta, &spec.host, spec.port.unwrap_or(crate::harness::HARNESS_PORT_DEFAULT), program, self.size),
            _ => Session::local(meta, program, Vec::new(), self.size),
        };
        if let Ok(mut session) = res {
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
        }
    }
}

impl App {
    /// The engine id selected in the new-session picker.
    pub fn selected_engine(&self) -> Option<&'static str> {
        ENGINES.get(self.selected.min(ENGINES.len() - 1)).map(|e| e.id)
    }

    /// Set the picker selection to the configured default engine (case-insensitive), falling back
    /// to index 0. Called when the new-session overlay opens so Enter spawns the user's usual
    /// engine without extra keystrokes.
    pub fn select_default_engine(&mut self) {
        let want = crate::config::Config::load().default_engine.to_lowercase();
        self.selected = ENGINES
            .iter()
            .position(|e| e.id.eq_ignore_ascii_case(&want))
            .unwrap_or(0);
    }
}

/// Map an engine id to its launch command.
fn engine_cmd(id: &str) -> Option<&'static str> {
    if id == "shell" {
        return Some("bash");
    }
    ENGINES.iter().find(|e| e.id == id).map(|e| e.cmd)
}

