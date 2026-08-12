//! Standalone native window (our terminal — no host emulator).
//!
//! winit provides the window + event loop; softbuffer provides a CPU framebuffer we draw the
//! alacritty grid into; ab_glyph rasterizes glyphs. This replaces the ratatui/crossterm TUI as the
//! default shell — the fleet/tunnel/reconnect machinery in `session.rs`/`transport.rs` is untouched
//! and shared. Chrome (tab bar, palette, status) is drawn natively with `draw_text`.

use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalSize, Size};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, ModifiersState};
use winit::window::{Window, WindowId};

use softbuffer::{Context, Surface};

use crate::app::{App, Overlay};
use crate::engines::ENGINES;
use crate::render::{argb, draw_grid, draw_text, Framebuffer, GlyphCache};
use crate::session::TermSize;

/// Chromeless text colors (macOS style).
const CHROME_FG: (u8, u8, u8) = (0xcc, 0xcc, 0xcc);
const CHROME_DIM: (u8, u8, u8) = (0x66, 0x66, 0x66);
/// Muted red for the fleet-triage "N panes down" count — a host went dark, not a busy signal.
const CHROME_ERR: (u8, u8, u8) = (0xf0, 0x6a, 0x6a);
const WHITE: (u8, u8, u8) = (0xff, 0xff, 0xff);

/// One fleet-search hit: which tab (index into `app.tabs`), the grid line in that session's
/// scrollback, and the column where the match text begins. Sorted by (tab, line).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FleetMatch {
    tab: usize,
    line: i32,
    col: usize,
}

/// A named action the command palette can run (the prefix+; palette). Each variant mirrors the
/// effect of one existing prefix command, so the palette is one discoverable home for the growing
/// set of prefix bindings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PaletteAction {
    NewSession,
    RemoteAttach,
    SessionPalette,
    FindInTab,
    SearchAll,
    CopyScrollback,
    ExportLog,
    CopyIdentity,
    CopyFleet,
    Broadcast,
    Peek,
    FleetGrid,
    UndoClose,
    Duplicate,
    SessionInfo,
    ToggleFocus,
    Pin,
    NextPinned,
    NextDown,
    NextHost,
    Dnd,
    Reconnect,
    ReconnectAll,
    Destroy,
    Interrupt,
    CloseQuiet,
    Help,
    Quit,
}

impl PaletteAction {
    /// The full, ordered list of palette rows, built once at startup.
    fn all_rows() -> Vec<(&'static str, PaletteAction)> {
        use PaletteAction::*;
        vec![
            ("new session (engine picker)", NewSession),
            ("attach to a remote pane@host", RemoteAttach),
            ("jump to a session (palette)", SessionPalette),
            ("find in current tab", FindInTab),
            ("search all sessions", SearchAll),
            ("copy whole scrollback", CopyScrollback),
            ("export scrollback to .log file", ExportLog),
            ("copy session identity (engine@host)", CopyIdentity),
            ("copy whole fleet summary (all tabs)", CopyFleet),
            ("broadcast a line to all sessions", Broadcast),
            ("peek at all session tails", Peek),
            ("fleet grid: live tails of every session", FleetGrid),
            ("undo close (reopen last)", UndoClose),
            ("duplicate active tab (fork same engine@host)", Duplicate),
            ("show session info (kind/host/task)", SessionInfo),
            ("toggle focus mode (hide tab bar + status)", ToggleFocus),
            ("pin/unpin active tab (protect from close)", Pin),
            ("jump to next pinned tab", NextPinned),
            ("jump to next down/reconnecting tab", NextDown),
            ("jump to next host (page fleet by machine)", NextHost),
            ("toggle do-not-disturb (mute all OS notifications)", Dnd),
            ("force reconnect active tab (bypass backoff)", Reconnect),
            ("force reconnect ALL down panes", ReconnectAll),
            ("kill active tab's pane (destroy remote session)", Destroy),
            ("send Ctrl-C to active tab (stop the run)", Interrupt),
            ("close all quiet (done) tabs", CloseQuiet),
            ("show this help", Help),
            ("quit", Quit),
        ]
    }
}

/// The native application: window + framebuffer surface + app state + glyph cache.
struct Application {
    window: Option<Rc<Window>>,
    context: Option<Context<Rc<Window>>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,
    size: PhysicalSize<u32>,
    font_px: u32,
    cell_w: u32,
    cell_h: u32,
    app: App,
    cache: GlyphCache,
    /// Resolved render palette (built-in colors unless a `[theme]` block is configured).
    colors: crate::render::Colors,
    prefix_down: bool,
    mods: ModifiersState,
    /// The tab that was active before the current one, so prefix+l can flip back to it (tmux
    /// last-window muscle memory). Cleared to None when tabs get rearranged out from under it.
    last_active: Option<usize>,
    /// Active search query ("" when the Find overlay is closed).
    find_query: String,
    /// In-progress rename for the active tab ("" when the Rename overlay is closed).
    rename_query: String,
    /// In-progress broadcast line ("" when the Broadcast overlay is closed). Enter sends it to every
    /// session; backspace/escape edit/cancel it.
    broadcast_query: String,
    /// The last broadcast line that was actually sent, pre-filled on the next open so a repeat
    /// command (e.g. `git pull` on every host) survives without retyping. Kept across restarts via
    /// `restore::save_last_broadcast`.
    last_broadcast: String,
    /// MRU of broadcast lines actually sent, most-recent first (cap 16). Shift+Up/Down in the
    /// broadcast overlay recalls old commands to repeat, e.g. alternating `git pull` / `make build`
    /// across machines. Persisted via `restore::save_broadcast_history`.
    broadcast_hist: Vec<String>,
    /// Index into `broadcast_hist` that a Shift+Up/Down recall currently points at; None = editing
    /// a fresh line.
    hist_sel: Option<usize>,
    /// Per-tab broadcast target selection. Grows/shrinks to match `tabs`; open targets are is_static, closed toggled off. Defaults all-on.
    broadcast_targets: Vec<bool>,
    /// Index of the row the broadcast overlay's focus is on (all-on by default).
    broadcast_sel: usize,
    /// In-progress working-directory for the NewSession picker ("" when blank → config start_cwd).
    new_cwd: String,
    /// Per-tab mute state (prefix+m). A muted tab's busy nudge + badge are suppressed so a noisy
    /// pane a diver doesn't care about stops nagging. Grows/shrinks with the tab set.
    muted: Vec<bool>,
    /// Per-tab pin state (prefix+a, default key `A`). A pinned tab can't be closed with `x` /
    /// prefix+`close_tab` (or via the palette), so a long-running agent a diver cares about can't
    /// be fat-fingered away — you must unpin first. Grows/shrinks with the tab set.
    pinned: Vec<bool>,
    /// Per-tab count of new scrollback lines produced since we last looked (output delta). Populated
    /// by `activity_flags`; the tab bar shows it as the badge magnitude so a diver reads how hot a
    /// pane is, not just *that* it moved.
    grew_delta: Vec<usize>,
    /// Pending OS notifications not yet delivered this frame, (kind "busy"|"bell", session index).
    /// Queued by `poll_bells`/`activity_flags` and drained together at the end of the frame so
    /// simultaneous busy/bell events across the fleet coalesce into ONE notification instead of one
    /// osascript popup per tab (a `broadcast` to every host would otherwise fan out N launches).
    /// Muted tabs are never queued. Emptied each frame by `flush_notifications`.
    pending_notify: Vec<(String, usize)>,
    /// Per-tab: whether the session was down (disconnected) on the previous frame. Used to detect the
    /// down→alive recovery edge and nudge the diver once (a `↻` badge + one notification), so a box
    /// that comes back is announced rather than silently reappearing.
    was_down: Vec<bool>,
    /// Monotonic instant until which each tab shows a `↻` recovery badge after its pane reconnected.
    /// Mirrors `bell_until`'s self-fading timeline.
    recover_until: Vec<Option<std::time::Instant>>,
    /// Global Do-Not-Disturb (prefix+M): while on, NO OS notifications are fired fleet-wide — neither
    /// the backgrounded-busy nag nor the terminal-bell popup. In-bar busy badges stay (they're not
    /// interruptions), just no popups. A single bool so nothing else in the fleet state shifts.
    dnd: bool,
    /// The currently-focused search match (absolute line, col, width); recomputed on each query
    /// change / Enter and passed to draw_grid for highlighting.
    find_hit: Option<crate::render::Find>,
    /// Every match of the active query (line, col, width), so draw_grid can highlight all of them
    /// in yellow while the focused one shows orange.
    find_all: Vec<crate::render::Find>,
    /// Index into `find_all` of the currently-focused match (the "N of M" cursor).
    find_index: usize,
    /// Live fleet-search query (the FleetSearch overlay). Same anatomy as `find_query` but the
    /// recompute sweeps every tab once instead of only the active one.
    fleet_q: String,
    /// Every matching (tab, line, col) across all open sessions, sorted by tab then line.
    fleet_matches: Vec<FleetMatch>,
    /// Index into `fleet_matches` of the currently-selected hit (the highlighted list row).
    fleet_sel: usize,
    /// Whether we're in tmux-style copy mode (prefix+[). While active, keystrokes navigate a read
    /// cursor instead of reaching the shell, and `v` starts/extends a selection to copy.
    copy_mode: bool,
    /// Copy-mode read cursor: (line, col) grid coordinates in the scrollback.
    copy_pos: (i32, usize),
    /// Copy-mode anchor: where the block selection started (Some while selecting), in grid coords.
    copy_anchor: Option<(i32, usize)>,
    /// Live copy-mode search query (the `/` prompt). While non-empty, `n`/`N` move cursor between
    /// matches of this text; Enter/`g` jump the cursor to the next match then drop back to nav.
    copy_query: String,
    /// Whether the copy-mode `/` prompt is currently active (accepting typing).
    copy_searching: bool,
    /// Fleet-overlay filter: typing narrows the session list (by session id / tmux pane / engine).
    /// An empty string shows every session; the highlighted row indexes `fleet_filtered`.
    fleet_query: String,
    /// Indices into `app.fleet.fleet` matching the current fleet query, for the overlay.
    fleet_filtered: Vec<usize>,
    /// Peek-overlay selection: which session row the highlight is on (index into `app.tabs`).
    peek_sel: usize,
    /// Peek list scroll offset — how many (capped) rows to skip at the top so ALL tabs are
    /// reachable, not just the first ~10. Bumped when the selection moves below the visible window.
    peek_scroll: usize,
    /// Command-palette filter text (the prefix+; palette). Typing narrows `palette_filtered`.
    palette_q: String,
    /// The full, static list of (label, action) rows the palette filters over.
    palette_rows: Vec<(&'static str, PaletteAction)>,
    /// Indices into `palette_rows` matching `palette_q` (recomputed each render).
    palette_filtered: Vec<usize>,
    /// Command-palette selection: index into `palette_filtered`.
    palette_sel: usize,
    /// Set when any quit path fires (prefix+q, or the palette's Quit action); the event loop honors
    /// it at the next `about_to_wait`, applying the same save-then-exit dance as CloseRequested.
    quit_requested: bool,
    /// Mouse state: the cell anchor where a drag-selection started (Some while left button held).
    /// With winit 0.30 we track presses/releases ourselves; dragging updates the selection end.
    mouse_anchor: Option<Point>,
    /// Latest cursor position in framebuffer px (winit's MouseInput has no position; we read this).
    cursor: (f64, f64),
    /// True while the pointer sits over a clickable URL so we show a hand instead of the arrow.
    /// Updated on CursorMoved so the affordance tracks the pointer without a full redraw.
    over_link: bool,
    /// Last (press-time, press-position, accumulated-click-count) to detect double/triple clicks.
    /// winit 0.30 doesn't hand us a click count, so we time consecutive presses ourselves.
    last_press: Option<(std::time::Instant, (f64, f64), u32)>,
    /// Last OS window title we set, so we only call set_title when it changes (each call is a
    /// platform round-trip).
    window_title: String,
    /// Base font scale (1.0 = 14px). Zoom multiplies font/margins; persisted in restore.
    zoom: f32,
    /// Base font size in px from config (the size a fresh window opens at, before display-scale
    /// and zoom). `zoom` still scales on top; Ctrl+0 resets zoom but not this.
    base_font: f32,
    /// Last-seen scrollback line count per tab index, used to flag tabs that produced output while
    /// not focused. `None` until a tab has been sampled once (so a freshly-spawned tab doesn't
    /// immediately badge).
    seen_history: Vec<usize>,
    /// Monotonic instant each tab last produced output (as observed by `activity_flags` — a grown
    /// scrollback while backgrounded). The focused tab is never stamped, so the tab you're actively
    /// reading never counts as "quiet". Feeds the quiet/waiting triage count and `prefix+b`.
    last_output: Vec<std::time::Instant>,
    /// A transient status-line toast (text, shown-at) for one-shot confirmations like an export
    /// path. Displayed for a couple seconds, then fades on its own. None = no toast.
    flash: Option<(String, std::time::Instant)>,
    /// Resolved prefix keybindings, reversed: key (the char pressed after Ctrl+Space) -> action
    /// name. Built from `crate::keys::resolve`, so it mirrors the hardcoded defaults unless the user
    /// overrides them in `[keybindings]`.
    key_action: std::collections::BTreeMap<String, String>,
    /// Per-tab: whether a backgrounded-output notification has ALREADY fired since the tab last went
    /// quiet. Reset when the tab is focused (looked at) or stops growing, so each silent→busy
    /// transition nudges the user exactly once.
    notified: Vec<bool>,
    /// Focus mode (prefix+v): hide the tab bar + status line so the grid gets the whole window.
    /// A distraction-free dive into one session. Toggle again to bring the chrome back.
    focus: bool,
    /// Monotonic instant until which a terminal-bell badge is shown for each tab (index). A bell
    /// (a long agent run finishing) shows a 🔔 badge for a few seconds, then fades on its own.
    bell_until: Vec<Option<std::time::Instant>>,
    /// The tab index currently under the pointer (for a hover-preview tooltip of its tail), or
    /// None when the cursor isn't over a tab. Recompute on every CursorMoved against the tab bar.
    hover_tab: Option<usize>,
    /// The tab index being drag-reordered (left button pressed on a tab), or None when idle. While
    /// this is Some, drags reorder the tab bar instead of growing a text selection, and release
    /// lands the dragged tab. Clicking a tab (press→release without moving) still switches to it.
    drag_tab: Option<usize>,
    /// The focused tile in the fleet-grid overlay (prefix+e); Enter dives into this session.
    grid_sel: usize,
}

impl Application {
    fn new(app: App) -> Self {
        let cfg = crate::config::Config::load();
        let base_font = cfg.font_px as f32;
        // Resolve prefix bindings once: action name -> key. Reverse it to key -> action so the
        // command handler can look up a pressed key directly. Unknown actions in config are dropped
        // by `resolve`, so this always covers every action with a valid key.
        let key_action = crate::keys::resolve(&cfg.keybindings.unwrap_or_default())
            .into_iter()
            .map(|(action, key)| (key, action))
            .collect::<std::collections::BTreeMap<String, String>>();
        let colors = match &cfg.theme {
            Some(t) => crate::render::Colors::from(t),
            None => crate::render::Colors::default(),
        };
        let tab_count = app.tabs.len();
        let seen_history = vec![usize::MAX; tab_count];
        // Quiet threshold from config: how long a live, backgrounded, unprotected tab can sit silent
        // before it counts as waiting-on-input. Fresh tabs start stamped "now" so they never read
        // instantly quiet on open.
        let now = std::time::Instant::now();
        let last_output = vec![now; tab_count];
        // Bring back any tabs the user muted last session (prefix+m) so they stay muted across a
        // restart instead of nagging again the moment the window reopens.
        let mut muted = vec![false; tab_count];
        {
            let saved = crate::restore::load_muted();
            if !saved.is_empty() {
                for (i, s) in app.tabs.iter().enumerate() {
                    let key = format!("{}:{}:{}", s.kind(), s.meta.host, s.meta.engine);
                    if saved.contains(&key) {
                        muted[i] = true;
                    }
                }
            }
        }
        // Bring back pinned tabs (prefix+a) from the last session — a pinned agent run that was
        // still going the previous session stays just as protected across a restart.
        let mut pinned = vec![false; tab_count];
        {
            let saved = crate::restore::load_pinned();
            if !saved.is_empty() {
                for (i, s) in app.tabs.iter().enumerate() {
                    let key = format!("{}:{}:{}", s.kind(), s.meta.host, s.meta.engine);
                    if saved.contains(&key) {
                        pinned[i] = true;
                    }
                }
            }
        }
        Application {
            window: None,
            context: None,
            surface: None,
            size: PhysicalSize::new(800, 600),
            font_px: 14,
            cell_w: 8,
            cell_h: 18,
            app,
            cache: GlyphCache::load(),
            colors,
            prefix_down: false,
            mods: ModifiersState::default(),
            last_active: None,
            find_query: String::new(),
            rename_query: String::new(),
            broadcast_query: String::new(),
            last_broadcast: crate::restore::load_last_broadcast(),
            broadcast_hist: crate::restore::load_broadcast_history(),
            hist_sel: None,
            broadcast_targets: vec![true; tab_count],
            broadcast_sel: 0,
            new_cwd: String::new(),
            muted,
            pinned,
            grew_delta: vec![0; tab_count],
            pending_notify: Vec::new(),
            find_hit: None,
            find_all: Vec::new(),
            find_index: 0,
            fleet_q: String::new(),
            fleet_matches: Vec::new(),
            fleet_sel: 0,
            copy_mode: false,
            copy_pos: (0, 0),
            copy_anchor: None,
            copy_query: String::new(),
            copy_searching: false,
            fleet_query: String::new(),
            fleet_filtered: Vec::new(),
            mouse_anchor: None,
            cursor: (0.0, 0.0),
            over_link: false,
            last_press: None,
            window_title: String::new(),
            zoom: crate::restore::load_zoom(),
            base_font,
            seen_history,
            last_output,
            notified: vec![false; tab_count],
            flash: None,
            peek_sel: 0,
            peek_scroll: 0,
            palette_q: String::new(),
            palette_rows: PaletteAction::all_rows(),
            palette_filtered: Vec::new(),
            palette_sel: 0,
            quit_requested: false,
            key_action,
            focus: false,
            dnd: false,
            bell_until: vec![None; tab_count],
            was_down: vec![false; tab_count],
            recover_until: vec![None; tab_count],
            hover_tab: None,
            drag_tab: None,
            grid_sel: 0,
        }
    }

    fn metrics_from_scale(&mut self) {
        if let Some(w) = &self.window {
            let s = w.scale_factor() as f32 * self.zoom;
            self.font_px = (self.base_font * s).round().clamp(8.0, 40.0) as u32;
            self.cell_w = (8.0 * s).round().max(2.0) as u32;
            self.cell_h = (18.0 * s).round() as u32;
        }
    }

    /// Zoom the terminal font by a multiplicative factor, applied on top of the display scale.
    /// Clamps so the grid never becomes unusable, then re-derives cell metrics.
    fn zoom_font(&mut self, delta: f32) {
        self.zoom = (self.zoom * delta).clamp(0.5, 3.0);
        crate::restore::save_zoom(self.zoom);
        self.metrics_from_scale();
        if let Some(active) = self.app.active_session() {
            let lines = (self.size.height - self.cell_h * 2) as usize / self.cell_h as usize;
            let cols = self.size.width as usize / self.cell_w as usize;
            active.resize(crate::session::TermSize { lines, cols });
        }
    }

    /// Return which tab indices have produced output since we last looked at them (are
    /// backgrounded AND have grown scrollback since our last sample). The focused tab is never
    /// flagged. Unknown/gone tabs (never sampled) are not flagged.
    /// Check every tab's bell flag and turn a fresh ring into a short-lived 🔔 badge. Runs once per
    /// frame (via `activity_flags`); `take_bell` is reset-on-read so each ring badges exactly once,
    /// and the badge fades after a few seconds without being re-armed.
    fn poll_bells(&mut self) {
        let n = self.app.tabs.len();
        self.bell_until.resize(n, None);
        self.was_down.resize(n, false);
        self.recover_until.resize(n, None);
        let now = std::time::Instant::now();
        // Events drained together at the end of this frame so same-burst events coalesce.
        let mut bells: Vec<usize> = Vec::new();
        let mut recovered: Vec<usize> = Vec::new();
        for (i, s) in self.app.tabs.iter().enumerate() {
            if s.take_bell() {
                // A bell while focused is a "your run finished" cue in view — just a badge. A bell in
                // a backgrounded (unmuted) tab queues an OS notification (coalesced below).
                if i != self.app.active && !self.muted.get(i).copied().unwrap_or(false) {
                    bells.push(i);
                }
                self.bell_until[i] = Some(now + std::time::Duration::from_secs(5));
            }
            // Fade stale badges on their own; bell_until goes None once expired.
            if let Some(until) = self.bell_until[i] {
                if until < now {
                    self.bell_until[i] = None;
                }
            }
            // Recovery edge: a remote (non-PTY) pane was down last frame and is alive again — a box
            // came back. Show a short `↻` badge and, when backgrounded and unmuted, queue ONE
            // notification so a diver notices (a host that silently reappears is easy to miss). PTYs
            // never flap, and reconnects that never complete (still down) don't fire.
            let alive_now = s.alive();
            if self.was_down[i] && alive_now && s.kind() != "pty" {
                self.recover_until[i] = Some(now + std::time::Duration::from_secs(8));
                if i != self.app.active && !self.muted.get(i).copied().unwrap_or(false) {
                    recovered.push(i);
                }
            }
            self.was_down[i] = !alive_now;
            if let Some(until) = self.recover_until[i] {
                if until < now {
                    self.recover_until[i] = None;
                }
            }
        }
        // Queue each bell for (coalesced) delivery rather than notifying per-tab here.
        for i in bells {
            self.queue_notify("bell", i);
        }
        for i in recovered {
            self.queue_notify("recover", i);
        }
    }

    /// Queue an OS notification for later this frame. Events of the same kind in the same frame are
    /// merged by `flush_notifications` into ONE popup listing every tab, so a `broadcast` to every
    /// host (which produces output in N tabs at once) fans out as a single notification instead of N
    /// osascript launches.
    fn queue_notify(&mut self, kind: &str, tab: usize) {
        // Global Do-Not-Disturb swallows every fleet notification (busy nag + bell popup) at the
        // source, so nothing even reaches the merge/launch path. In-bar badges are unaffected — a
        // diver who mutes popups still sees the `!N`/🔔 in the bar to act on later.
        if self.dnd {
            return;
        }
        self.pending_notify.push((kind.to_string(), tab));
    }

    /// Drain queued notifications, merging same-kind events that arrived in this frame into one
    /// notification listing every affected tab. Runs once at the end of each `activity_flags` pass;
    /// a frame with nothing queued drains nothing.
    fn flush_notifications(&mut self) {
        if self.pending_notify.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending_notify);
        for (kind, tabs) in group_notifications(&pending) {
            self.fire(&kind, &tabs);
        }
    }

    /// Imperatively fire one coalesced notification of `kind` for the given (deduped) tab indices.
    fn fire(&mut self, kind: &str, tabs: &[usize]) {
        let labels: Vec<String> = tabs
            .iter()
            .filter_map(|&i| self.app.tabs.get(i))
            .map(|s| s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone()))
            .collect();
        let n = labels.len();
        if n == 0 {
            return;
        }
        let list = join_labels(&labels);
        match kind {
            "bell" => {
                let title = if n == 1 {
                    format!("{list} — bell")
                } else {
                    format!("{n} sessions — bell")
                };
                let body = if n == 1 {
                    format!("Session {list} rang the terminal bell.")
                } else {
                    format!("Fleet bells: {list}.")
                };
                notify_simple(&title, &body);
            }
            "busy" => {
                let title = if n == 1 {
                    format!("{list} · busy")
                } else {
                    format!("{n} sessions busy")
                };
                let body = if n == 1 {
                    format!("{list} produced new output.")
                } else {
                    format!("New output from {list}.")
                };
                notify_simple(&title, &body);
            }
            "recover" => {
                let title = if n == 1 {
                    format!("{list} · reconnected")
                } else {
                    format!("{n} sessions reconnected")
                };
                let body = if n == 1 {
                    format!("Pane {list} is back online.")
                } else {
                    format!("Back online: {list}.")
                };
                notify_simple(&title, &body);
            }
            _ => {}
        }
    }

    fn activity_flags(&mut self) -> Vec<bool> {
        self.poll_bells();
        let n = self.app.tabs.len();
        self.seen_history.resize(n, usize::MAX);
        self.notified.resize(n, false);
        self.muted.resize(n, false);
        self.pinned.resize(n, false);
        self.grew_delta.resize(n, 0);
        let mut flags = vec![false; n];
        // Collect the indices that first went busy so we can queue after the immutable tab-borrow
        // loop ends (`queue_notify` needs `&mut self`).
        let mut fresh_busy: Vec<usize> = Vec::new();
        for (i, s) in self.app.tabs.iter().enumerate() {
            if i == self.app.active {
                // We're looking at it now: re-baseline, don't flag, and reset any pending nudge.
                self.seen_history[i] = s.history_len();
                self.notified[i] = false;
                self.grew_delta[i] = 0;
                continue;
            }
            let len = s.history_len();
            let grew = self.seen_history[i] != usize::MAX && len > self.seen_history[i];
            self.grew_delta[i] = len.saturating_sub(self.seen_history[i]);
            self.seen_history[i] = len;
            if grew {
                // This is genuine fresh output from a session we're not looking at — a timestamp of
                // "last did something." The focused tab is never stamped (it can't reach this arm),
                // so a tab you're actively reading stays busy-forever, never quiet.
                self.last_output[i] = std::time::Instant::now();
            }
            flags[i] = grew;
            // A muted tab is intentionally ignored: no busy badge, no OS notification. It still
            // feeds its seen-history so it isn't flagged the moment it's unmuted.
            if self.muted[i] {
                continue;
            }
            // Fire a one-shot OS notification the first time a backgrounded agent goes busy, so a
            // diver is nudged without having to watch the badge. Once the tab is looked at (or it
            // settles), `notified` is reset and the next transition nags again.
            if grew && !self.notified[i] {
                self.notified[i] = true;
                fresh_busy.push(i);
            }
        }
        for i in fresh_busy {
            self.queue_notify("busy", i);
        }
        // Every caller of this pass (render badge + next_busy + tab-bar) expects the queued busy and
        // bell events to have been delivered; flush the merged batch into one notification per kind.
        self.flush_notifications();
        flags
    }

    /// Focus tab `i` and remember it (so a relaunch opens on the same tab).
    fn set_active(&mut self, i: usize) {
        if i != self.app.active && self.app.active < self.app.tabs.len() {
            self.last_active = Some(self.app.active);
        }
        self.app.active = i.min(self.app.tabs.len().saturating_sub(1));
        crate::restore::save_active(self.app.active);
    }

    /// `prefix+l`: flip to the tab that was active just before this one (tmux last-window).
    /// Repeated presses ping-pong, since swapping focus swaps `last_active`.
    fn last_window(&mut self) {
        if let Some(prev) = self.last_active.take() {
            if prev < self.app.tabs.len() {
                self.set_active(prev);
            }
        }
    }

    /// `prefix+p`: paste the system clipboard into the active session via the bracketed-paste
    /// sequence. Works uniformly for local and remote/tunnel sessions, so it's the reliable paste
    /// when Ctrl+V is taken by a window manager.
    fn paste_clipboard(&mut self) {
        let Some(s) = self.app.active_session() else {
            return;
        };
        if let Ok(mut cb) = arboard::Clipboard::new() {
            if let Ok(text) = cb.get_text() {
                let seq = format!("\x1b[200~{}\x1b[201~", text);
                s.write(seq.as_bytes());
            }
        }
    }

    /// Middle-click: paste the system clipboard into the active session WITHOUT the bracketed-paste
    /// wrapper. Terminal middle-click paste has always been raw bytes; bracketing assumes the app
    /// requested it, and most CLIs ignore the delimiter sequence but echo it, corrupting the line.
    /// Raw write is what every X11 terminal does.
    fn paste_raw(&mut self) {
        let Some(s) = self.app.active_session() else {
            return;
        };
        if let Ok(mut cb) = arboard::Clipboard::new() {
            if let Ok(text) = cb.get_text() {
                s.write(text.as_bytes());
            }
        }
    }

    /// `Ctrl+D`: copy the active session's entire scrollback (history + screen) to the system
    /// clipboard as plain text. Handy for dumping an agent's whole log for pasting into an issue or
    /// a summary. Builds the text with the same capture path used for restart persistence.
    fn copy_whole_scrollback(&mut self) {
        let Some(s) = self.app.active_session() else {
            return;
        };
        let text = s.capture_scrollback();
        if text.trim().is_empty() {
            return;
        }
        if let Ok(mut cb) = arboard::Clipboard::new() {
            let _ = cb.set_text(text);
        }
    }

    /// `prefix+j`: copy the active session's identity to the system clipboard — `<engine|name>@host`
    /// for a local PTY, `pane@host` for a pane-backed tab (the exact target `prefix+r` attach
    /// accepts). Quick way to share or reference a session without hand-typing `user@host:session`.
    fn copy_identity(&mut self) {
        let Some(s) = self.app.active_session() else {
            return;
        };
        let head = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
        let id = format!("{}@{}", head, s.meta.host);
        if let Ok(mut cb) = arboard::Clipboard::new() {
            let _ = cb.set_text(id);
        }
        self.flash = Some((
            format!("copied {}@{}", head, s.meta.host),
            std::time::Instant::now(),
        ));
    }

    /// `prefix+E`: copy a one-line summary of every open tab to the clipboard — the "what's my
    /// fleet doing right now" gesture. Each row is `●/○ state · engine@host · name · live title`,
    /// so the clipboard reads as a grep-friendly status dump (ready to paste into a chat or notes).
    fn copy_fleet(&mut self) {
        if self.app.tabs.is_empty() {
            self.flash = Some((
                "no sessions to summarize".to_string(),
                std::time::Instant::now(),
            ));
            return;
        }
        let text = self.fleet_summary_text();
        if let Ok(mut cb) = arboard::Clipboard::new() {
            let _ = cb.set_text(text);
        }
        self.flash = Some((
            format!("copied {} sessions to clipboard", self.app.tabs.len()),
            std::time::Instant::now(),
        ));
    }

    /// One line per open tab for the fleet-summary copy: `●/○ engine (N)@host · name · live · ⏳`.
    fn fleet_summary_text(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        for (i, s) in self.app.tabs.iter().enumerate() {
            let head = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
            let state = if s.alive() { "●" } else { "○" };
            let live = s
                .live_title()
                .map(|t| format!(" · {t}"))
                .unwrap_or_default();
            let where_s = if s.kind() == "pty" {
                String::new()
            } else {
                format!("@{}", s.meta.host)
            };
            let queued = s.pending_bytes();
            let queued_s = if queued > 0 {
                format!(" · ⏳{}B queued", queued)
            } else {
                String::new()
            };
            lines.push(format!(
                "{state} {} ({}){where_s} · {head}{live}{queued_s}",
                s.meta.engine,
                i + 1
            ));
        }
        lines.join("\n")
    }

    /// `prefix+w`: write the active session's whole scrollback to a timestamped text file in the
    /// current directory (or HOME if that fails). Bigger than the clipboard and leaves a lasting
    /// artifact — a diver can dump a long agent log to disk to grep, diff, or share. Shows the path
    /// in the OSC title slot so the user sees where it landed.
    fn export_scrollback(&mut self) {
        let Some(s) = self.app.active_session() else {
            return;
        };
        let text = s.capture_scrollback();
        if text.trim().is_empty() {
            return;
        }
        let slug = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
        let slug: String = slug
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let base = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
        // The timestamp needs to be readable but collision-safe; epoch-ms keeps it unique.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let path = base.join(format!("{}-{}.log", slug, stamp));
        if std::fs::write(&path, &text).is_ok() {
            let shown = path.to_string_lossy();
            self.flash = Some((
                format!("wrote {} bytes → {}", text.len(), shown),
                std::time::Instant::now(),
            ));
        }
    }

    /// Fleet-overlay Enter: jump to an already-open tab running the selected session's engine if one
    /// exists (closest thing to 'diving into' that pane), else open a fresh local tmux pane for it.
    /// Refresh `fleet_filtered` (indices into `app.fleet.fleet` matching `fleet_query`) and clamp the
    /// selection. Mirrors the palette filter, but over fleet sessions (matched by id / pane / engine).
    fn fleet_refresh_filter(&mut self) {
        let q = self.fleet_query.to_lowercase();
        self.fleet_filtered = self
            .app
            .fleet
            .fleet
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                if q.is_empty() {
                    return true;
                }
                s.session_id.to_lowercase().contains(&q)
                    || s.tmux_pane.to_lowercase().contains(&q)
                    || s.engine.to_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect();
        if self.app.selected >= self.fleet_filtered.len() {
            self.app.selected = self.fleet_filtered.len().saturating_sub(1);
        }
    }

    fn fleet_attach_selected(&mut self) {
        // `selected` is an index into `fleet_filtered`; resolve it back to the real session.
        let real = self
            .fleet_filtered
            .get(self.app.selected)
            .copied()
            .unwrap_or(self.app.selected);
        let Some(fs) = self.app.fleet.fleet.get(real) else {
            self.app.overlay = Overlay::None;
            return;
        };
        let eng = fs.engine.clone();
        // Prefer an open tab with the same engine id, so re-running the same session isn't duplicated.
        if let Some(i) = self.app.tabs.iter().position(|t| t.meta.engine == eng) {
            self.set_active(i);
        } else if !eng.is_empty() {
            self.app.spawn_tmux("this-host", &eng);
        }
        self.app.overlay = Overlay::None;
    }

    /// `prefix+o`: jump to the next backgrounded tab that produced output since we last looked,
    /// wrapping around. Makes the activity badge actionable — a key to reach 'the one that just
    /// did something'. If none is flagged, does nothing.
    fn next_busy(&mut self) {
        let flags = self.activity_flags();
        let n = self.app.tabs.len();
        if n == 0 {
            return;
        }
        for step in 1..=n {
            let i = (self.app.active + step) % n;
            if flags[i] {
                self.set_active(i);
                return;
            }
        }
    }

    /// Which live, backgrounded, unprotected tabs have sat silent past the quiet threshold — the
    /// inverse of `activity_flags`'s busy. A session that's been quiet this long is almost certainly
    /// done (or paused at an input prompt) and waiting on you, so it's worth the triage count's
    /// `⌛N`. The focused tab, dead tabs, and pinned/muted tabs (deliberately shielded) are excluded;
    /// a tab is only counted once its history has been sampled AND it has actually sat idle.
    fn quiet_flags(&self) -> (bool, usize, std::time::Duration) {
        let present = self.seen_history.len() == self.app.tabs.len();
        let threshold = std::time::Duration::from_secs(
            crate::config::Config::load()
                .quiet_after_secs
                .unwrap_or(120),
        );
        let now = std::time::Instant::now();
        let mut any = false;
        let mut count = 0;
        let mut max_idle = std::time::Duration::ZERO;
        for (i, s) in self.app.tabs.iter().enumerate() {
            let live = s.alive() && s.kind() != "pty";
            let watched = i == self.app.active;
            let shielded = self.pinned.get(i).copied().unwrap_or(false)
                || self.muted.get(i).copied().unwrap_or(false);
            let sampled = present && self.seen_history[i] != usize::MAX;
            if !(live && !watched && !shielded && sampled) {
                continue;
            }
            let idle = now - self.last_output[i];
            if idle >= threshold {
                any = true;
                count += 1;
                if idle > max_idle {
                    max_idle = idle;
                }
            }
        }
        (any, count, max_idle)
    }

    /// Whether a single tab currently reads as "quiet" (live, backgrounded, unprotected, sampled,
    /// and sat silent past the threshold). Shares the exact condition `next_quiet` uses so the fleet
    /// grid's per-tile `⌛` matches the `prefix+z` jump and the triage count.
    fn quiet_for(&self, i: usize) -> bool {
        let present = self.seen_history.len() == self.app.tabs.len();
        let Some(s) = self.app.tabs.get(i) else {
            return false;
        };
        let live = s.alive() && s.kind() != "pty";
        let watched = i == self.app.active;
        let shielded = self.pinned.get(i).copied().unwrap_or(false)
            || self.muted.get(i).copied().unwrap_or(false);
        let sampled = present && self.seen_history[i] != usize::MAX;
        if !(live && !watched && !shielded && sampled) {
            return false;
        }
        let threshold = std::time::Duration::from_secs(
            crate::config::Config::load()
                .quiet_after_secs
                .unwrap_or(120),
        );
        let idle = std::time::Instant::now() - self.last_output[i];
        idle >= threshold
    }

    /// `prefix+z`: jump to the next tab whose live session has gone quiet (sat silent past the
    /// quiet threshold — likely done, or parked at an input prompt waiting on you), wrapping.
    /// Complements `next_busy`/`next_down`: busy means "just produced output", quiet means
    /// "finished/stalled — needs a look". No-op when no tab is quiet.
    fn next_quiet(&mut self) {
        let present = self.seen_history.len() == self.app.tabs.len();
        let threshold = std::time::Duration::from_secs(
            crate::config::Config::load()
                .quiet_after_secs
                .unwrap_or(120),
        );
        let n = self.app.tabs.len();
        if n == 0 {
            return;
        }
        for step in 1..=n {
            let i = (self.app.active + step) % n;
            let s = &self.app.tabs[i];
            let live = s.alive() && s.kind() != "pty";
            let watched = i == self.app.active;
            let shielded = self.pinned.get(i).copied().unwrap_or(false)
                || self.muted.get(i).copied().unwrap_or(false);
            let sampled = present && self.seen_history[i] != usize::MAX;
            if !(live && !watched && !shielded && sampled) {
                continue;
            }
            let idle = std::time::Instant::now() - self.last_output[i];
            if idle >= threshold {
                self.set_active(i);
                return;
            }
        }
    }

    /// `prefix+Q`: jump to the next tab whose session is down (disconnected / in reconnect backoff),
    /// wrapping. Complements `next_busy` (which finds tabs that produced output): when a host dies
    /// and several panes go dark, this cycles just the dead ones so a diver sees what's reconnecting
    /// instead of typing into a blank pane. No-op when every tab is alive.
    fn next_down(&mut self) {
        let n = self.app.tabs.len();
        if n == 0 {
            return;
        }
        for step in 1..=n {
            let i = (self.app.active + step) % n;
            let down = !self.app.tabs[i].alive() && self.app.tabs[i].kind() != "pty"; // live local PTY is never "down"
            if down {
                self.set_active(i);
                return;
            }
        }
    }

    /// `prefix+P`: jump to the next pinned tab (wrapping). Pinned tabs are the long-running agents
    /// a diver deliberately protects, so this is a fast way to cycle just the protected ones
    /// without scrolling the whole bar. No-op when there are no other pinned tabs.
    fn next_pinned(&mut self) {
        let n = self.app.tabs.len();
        if n == 0 {
            return;
        }
        for step in 1..=n {
            let i = (self.app.active + step) % n;
            if self.pinned.get(i).copied().unwrap_or(false) {
                self.set_active(i);
                return;
            }
        }
    }

    /// `prefix+H`: jump to the next unique host in the fleet, wrapping. For a fleet spread across
    /// machines this pages by machine instead of by tab — a three-host farm becomes three stops
    /// rather than a trip through every pane. It lands on the first tab of the next distinct host
    /// after the active one; ties in the linear tab order are resolved by cycling order. No-op when
    /// every tab is on one host (or there are no tabs).
    fn next_host(&mut self) {
        let hosts: Vec<&str> = self.app.tabs.iter().map(|s| s.meta.host.as_str()).collect();
        if let Some(i) = next_host_index(&hosts, self.app.active) {
            let target = self.app.tabs[i].meta.host.clone();
            self.flash = Some((format!("host {target}"), std::time::Instant::now()));
            self.set_active(i);
        }
    }

    /// `prefix+m`: toggle mute on the active tab. A muted tab stops firing the busy OS notification
    /// and its `!` badge (see `activity_flags`), so a noisy pane a diver doesn't care about stops
    /// nagging — while still showing its own live tail in the tab bar. Toggle again to unmute.
    fn toggle_mute_active(&mut self) {
        if self.app.active < self.app.tabs.len() {
            let on = !self.muted[self.app.active];
            self.muted[self.app.active] = on;
            let state = if on { "MUTED" } else { "unmuted" };
            let head = self
                .app
                .active_session()
                .map(|s| s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone()))
                .unwrap_or_default();
            self.flash = Some((format!("{head} {state}"), std::time::Instant::now()));
        }
    }

    /// `prefix+C`: close every tab whose live session is "quiet" — done or parked waiting on you
    /// (the exact `quiet_for` predicate behind prefix+z and the `⌛N` triage). A fleet-cleanup
    /// gesture: after a long night of agent runs, one key sweeps the finished ones off the bar.
    /// Pinned tabs and the active tab are never closed (unprotect via `A`, or hop away first);
    /// mutes do NOT shield, since a muted-but-done agent is exactly the cruft you'd sweep.
    /// Reopenable via prefix+u for the last one removed.
    fn close_quiet_tabs(&mut self) {
        // Collect indices (high→low) of quiet, unpinned, non-active tabs.
        let mut doomed: Vec<usize> = Vec::new();
        for i in 0..self.app.tabs.len() {
            if i == self.app.active {
                continue;
            }
            if self.pinned.get(i).copied().unwrap_or(false) {
                continue; // protected — never batch-closed
            }
            if self.quiet_for(i) {
                doomed.push(i);
            }
        }
        if doomed.is_empty() {
            self.flash = Some((
                "no quiet tabs to close".to_string(),
                std::time::Instant::now(),
            ));
            return;
        }
        // Remove high→low so indices stay valid; remember the first (lowest) for undo.
        doomed.sort_unstable();
        let first = doomed[0];
        if let Some(s) = self.app.tabs.get(first) {
            self.app.last_closed = Some(crate::restore::TabSpec {
                kind: s.kind().to_string(),
                host: s.meta.host.clone(),
                engine: s.meta.engine.clone(),
                port: None,
                name: s.meta.name.clone(),
            });
        }
        for &i in doomed.iter().rev() {
            self.app.tabs.remove(i);
        }
        if self.app.active >= self.app.tabs.len() {
            self.app.active = self.app.tabs.len().saturating_sub(1);
        }
        crate::restore::save(&self.app.tab_specs());
        self.save_pin_state();
        self.flash = Some((
            format!(
                "closed {} quiet tab{}",
                doomed.len(),
                if doomed.len() == 1 { "" } else { "s" }
            ),
            std::time::Instant::now(),
        ));
    }

    /// `prefix+M`: toggle global Do-Not-Disturb. When on, no OS notifications fire fleet-wide — the
    /// backgrounded-busy nag and the terminal-bell popup are both swallowed at `queue_notify`. In-bar
    /// busy/bell badges still render, so a diver who mutes popups can still see, later, that a tab
    /// rang. Toggle again to restore notifications. The status line shows `🔕` while on.
    fn toggle_dnd(&mut self) {
        self.dnd = !self.dnd;
        let state = if self.dnd {
            "do-not-disturb ON"
        } else {
            "notifications restored"
        };
        self.flash = Some((state.to_string(), std::time::Instant::now()));
    }

    /// `prefix+!`: send an interrupt (Ctrl-C) to the active session so a diver can stop a runaway agent
    /// run without dropping into its raw terminal. `Session::write` handles the typing — buffered
    /// into `pending` if the transport is momentarily down, flushed on reconnect.
    fn interrupt_active(&mut self) {
        let Some(head) = self
            .app
            .active_session()
            .map(|s| s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone()))
        else {
            return;
        };
        if let Some(s) = self.app.active_session() {
            s.write(b"\x03");
            self.flash = Some((
                format!("{head} interrupted (Ctrl-C)"),
                std::time::Instant::now(),
            ));
        }
    }

    /// Force a live-again attempt on the active tab, ignoring its auto-reconnect backoff. A no-op for
    /// local PTYs (nothing to re-attach) and alive tabs, so it only nudges dead remote panes.
    fn reconnect_active(&mut self) {
        let Some(s) = self.app.active_session_mut() else {
            return;
        };
        if s.kind() == "pty" || s.alive() {
            self.flash = Some((
                format!("{} is fine", s.meta.engine),
                std::time::Instant::now(),
            ));
            return;
        }
        match s.reconnect_now() {
            Ok(()) => {
                self.flash = Some((
                    format!("{} reconnected", s.meta.engine),
                    std::time::Instant::now(),
                ))
            }
            Err(_) => {
                self.flash = Some((
                    format!("{} still unreachable — will keep retrying", s.meta.engine),
                    std::time::Instant::now(),
                ))
            }
        }
    }

    /// `prefix+T`: force a live-again attempt on EVERY down remote pane at once, ignoring each
    /// transport's auto-reconnect backoff. The fleet-shaped cousin of `reconnect_active`: when
    /// several hosts dropped together (a network blip, a box reboot), one key re-runs the connect
    /// path for all of them instead of tab-hopping and pressing `R` per pane. PTYs are skipped
    /// (nothing to re-attach); the focused tab's reconnect, when down, also fires so the dive
    /// session isn't left behind. Flashes how many were attempted vs. actually reached.
    fn reconnect_all_down(&mut self) {
        let down: Vec<usize> = (0..self.app.tabs.len())
            .filter(|&i| {
                let s = &self.app.tabs[i];
                s.kind() != "pty" && !s.alive()
            })
            .collect();
        if down.is_empty() {
            self.flash = Some((
                "no down panes — fleet is healthy".to_string(),
                std::time::Instant::now(),
            ));
            return;
        }
        let mut ok = 0usize;
        for i in &down {
            if self.app.tabs[*i].reconnect_now().is_ok() {
                ok += 1;
            }
        }
        self.flash = Some((
            format!(
                "reconnect-all: {} reached, {} still down",
                ok,
                down.len() - ok
            ),
            std::time::Instant::now(),
        ));
    }

    /// Kill the active tab's pane (remote tmux/ssh/tunnel), then close the tab so the watchdog
    /// doesn't reconnect it. The one way a fleet diver reclaims a runaway agent's resources on a
    /// remote host from here. Local PTYs have no separate session, so it's just a close.
    fn destroy_active(&mut self) {
        let Some(head) = self
            .app
            .active_session()
            .map(|s| s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone()))
        else {
            return;
        };
        // Destroy first, while the session object still owns the transport. A pinned tab is killed
        // just like an unpinned one — destroy is explicit and removing resources is the point.
        let kind = self.app.active_session().map(|s| s.kind().to_string());
        if let Some(s) = self.app.active_session() {
            s.destroy();
        }
        let note = match kind.as_deref() {
            Some("pty") => format!("closed {head}"),
            Some(k) => format!("killed {k} pane {head}"),
            None => return,
        };
        self.flash = Some((note, std::time::Instant::now()));
        crate::native::close_tab(&mut self.app, false);
    }

    /// Fork the active tab, preserving its pin state: a pinned clone (prefix+k / palette
    /// Duplicate) protects the same agent run the original was protecting. No-op for local PTYs.
    fn duplicate_active_preserving_pin(&mut self) {
        let was_pinned = self.pinned.get(self.app.active).copied().unwrap_or(false);
        let before = self.app.tabs.len();
        self.app.duplicate_active();
        if was_pinned && self.app.tabs.len() > before {
            self.pinned.resize(self.app.tabs.len(), false);
            self.pinned[self.app.tabs.len() - 1] = true;
            self.save_pin_state();
        }
    }

    /// `prefix+a` (default key `A`): toggle pin on the active tab. A pinned tab is protected from
    /// accidental close (`x` / prefix+close_tab refuse it with a flash), so a long-running agent a
    /// diver cares about stays until deliberately unpinned. Persists across restarts like mute.
    fn toggle_pin_active(&mut self) {
        if self.app.active < self.app.tabs.len() {
            let on = !self.pinned[self.app.active];
            self.pinned[self.app.active] = on;
            let state = if on { "PINNED 🔒" } else { "unpinned" };
            let head = self
                .app
                .active_session()
                .map(|s| s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone()))
                .unwrap_or_default();
            self.flash = Some((format!("{head} {state}"), std::time::Instant::now()));
            self.save_pin_state();
        }
    }

    /// `prefix+v`: toggle focus mode — hide the tab bar + status line so the grid fills the whole
    /// window for a distraction-free dive. The resize runs `redraw` which re-sizes the session to the
    /// now-larger grid. Toggle again to bring the chrome back.
    fn toggle_focus(&mut self) {
        self.focus = !self.focus;
        let state = if self.focus { "focus" } else { "chrome" };
        self.flash = Some((state.to_string(), std::time::Instant::now()));
        self.redraw();
    }

    fn redraw(&mut self) {
        let (Some(w), Some(h)) = (
            NonZeroU32::new(self.size.width),
            NonZeroU32::new(self.size.height),
        ) else {
            return;
        };
        let (width, height) = (w.get() as usize, h.get() as usize);
        // 0x00RRGGBB (softbuffer's native format); starts solid black.
        let mut fb = Framebuffer::new(width, height);
        for p in fb.pixels.iter_mut() {
            *p = 0x0000_0000;
        }
        // Render into the CPU framebuffer first (doesn't borrow the surface).
        self.render(&mut fb);

        // Sync the OS window title to the active tab's live OSC title so the fleet Diver sees at a
        // glance what the pane is doing even when the window is minimized/unfocused. Only call
        // set_title when the string actually changed.
        if let Some(w) = &self.window {
            let title = self
                .app
                .active_session()
                .and_then(|s| s.live_title())
                .unwrap_or_else(|| "harness-terminal".to_string());
            let title = format!("{} — harness-terminal", title);
            if title != self.window_title {
                w.set_title(&title);
                self.window_title = title;
            }
        }

        // Then present it via the softbuffer surface.
        let Some(surface) = &mut self.surface else {
            return;
        };
        let Ok(mut buffer) = surface.buffer_mut() else {
            return;
        };
        for (dst, src) in buffer.iter_mut().zip(fb.pixels.iter()) {
            *dst = *src;
        }
        let _ = buffer.present();
    }

    /// Render the whole frame into the framebuffer.
    fn render(&mut self, fb: &mut Framebuffer) {
        // In focus mode both bars collapse to zero so the grid fills the window edge to edge.
        let bar_h = if self.focus { 0 } else { self.cell_h as usize };
        let (tab_h, status_h) = (bar_h, bar_h);
        let term_top = tab_h;
        let term_bottom = fb.height.saturating_sub(status_h);
        let gline_px = self.cell_h as usize;
        let gcol_px = self.cell_w as usize;
        let grid_lines = term_bottom.saturating_sub(term_top).max(1) / gline_px;
        let grid_cols = fb.width.max(1) / gcol_px;

        // Size the active session to the on-screen grid.
        if let Some(active) = self.app.active_session() {
            let g = active.term.lock();
            let (gl, gc) = (g.screen_lines(), g.columns());
            drop(g);
            if gl != grid_lines || gc != grid_cols {
                let size = TermSize {
                    lines: grid_lines.max(1),
                    cols: grid_cols.max(1),
                };
                active.resize(size);
            }
        }

        // Terminal grid.
        if let Some(active) = self.app.active_session() {
            let mut g = active.term.lock();
            // With history, a non-zero display_offset is a live-follow "scroll" (bottom = offset 0).
            // Auto-return to the live view when new content pushes us to the bottom again: if we're
            // not mid-gesture (self.scrolled is false) force the view back to the latest line.
            let at_bottom = g.grid().display_offset() == 0;
            // Only force back to live if THIS session isn't pinned into history by the user.
            if !active.scrolled() && !at_bottom {
                use alacritty_terminal::grid::Scroll;
                g.grid_mut().scroll_display(Scroll::Bottom);
            }
            // Compute the current text-selection range (if any) so draw_grid can highlight it.
            let sel = g.selection.as_ref().and_then(|s| s.to_range(&g));
            let copy = if self.copy_mode {
                Some(self.copy_pos)
            } else {
                None
            };
            draw_grid(
                fb,
                &g,
                self.cell_w,
                self.cell_h,
                self.font_px,
                &mut self.cache,
                &self.colors,
                self.find_hit,
                &self.find_all,
                sel.as_ref(),
                copy,
            );
        } else {
            // No sessions open — draw a short 'how to start' hint so the window isn't a blank void.
            let cy = term_top + (grid_lines / 2) * gline_px;
            let hint = "no sessions ·  Ctrl+Space n  new   Ctrl+Space r  attach remote   Ctrl+Space /  palette ";
            let hw = draw_text(fb, &mut self.cache, hint, 0, cy, self.font_px, CHROME_DIM);
            let cx = fb.width.saturating_sub(hw) / 2;
            // Blank the row so the sizing pass above doesn't leave a left-aligned ghost, then paint
            // a single centered copy. (All pixels are black here to begin; clearing is a no-op but
            // keeps this robust if the grid later draws under the hint.)
            for py in cy.saturating_sub(self.font_px as usize)..(cy + self.font_px as usize) {
                for px in 0..fb.width {
                    if py < fb.height {
                        fb.pixels[py * fb.width + px] = argb(0, 0, 0, 0);
                    }
                }
            }
            draw_text(fb, &mut self.cache, hint, cx, cy, self.font_px, CHROME_DIM);
        }

        // Tab bar (top row). Flag backgrounded tabs that produced output since we last looked.
        // The activity pass (busy/bell detection + coalesced notifications) runs EVERY frame,
        // including focus mode where the bar is hidden — hiding the chrome must not silence the
        // fleet: a backgrounded agent finishing still nudges there.
        let activity = self.activity_flags();
        if self.focus {
            // Focus mode: no tab bar — the grid owns the full height.
        } else {
            let tab_base = self.cell_h as usize / 2;
            let mut x = 6usize;
            for (i, s) in self.app.tabs.iter().enumerate() {
                let active = i == self.app.active;
                // Color the active tab's dot by its host so a fleet diver reads 'which machine' at a
                // glance; matches the host hue used across tabs and it's stable across restarts.
                let dot = if active { "●" } else { "○" };
                // Show the pane's live OSC title (what the agent is doing) when it has announced one.
                let live = s.live_title().unwrap_or_else(|| s.meta.title.clone());
                let mut live = live.replace('\n', " ");
                if live.chars().count() > 18 {
                    live = live.chars().take(18).collect::<String>() + "…";
                }
                // A magnitude badge shows how much a backgrounded tab produced since we last looked
                // (rooted at 1 so one line displays as "!1", capped so it doesn't eat the bar); a muted
                // tab is dimmed and shows M instead so its silence is read at a glance.
                let delta = self.grew_delta.get(i).copied().unwrap_or(0);
                let flag = if activity[i] {
                    format!("!{}", delta.min(999))
                } else {
                    String::new()
                };
                let mute = if self.muted.get(i).copied().unwrap_or(false) {
                    " M "
                } else {
                    " "
                };
                // A pinned tab shows a small 🔒 so its protected status (won't close with x) is
                // readable at a glance, next to the mute marker.
                let pin = if self.pinned.get(i).copied().unwrap_or(false) {
                    "🔒"
                } else {
                    " "
                };
                // Show the user's rename if set; otherwise the plain engine id.
                let head = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
                // For a pane-backed (non-local) tab, append @host so a diver reads WHERE each tab
                // runs without hovering — a local PTY has nothing to say here and stays bare.
                let where_s = if s.kind() == "pty" {
                    String::new()
                } else {
                    format!("@{}", s.meta.host)
                };
                // A 🔔 badge marks a terminal bell (a long agent run finishing) for a few seconds.
                // Drawn before the busy flag stays out of the label's own magnitude space.
                let bell = if self.bell_until.get(i).copied().flatten().is_some() {
                    "🔔 "
                } else {
                    ""
                };
                // A recovery badge marks a pane that just reconnected (down → alive), fading on its
                // own like the bell badge. Reading `↻` at a glance beats silently watching a host
                // come back.
                let recover = if self.recover_until.get(i).copied().flatten().is_some() {
                    "↻ "
                } else {
                    ""
                };
                // A down pane with queued type-ahead shows how much is staged to flush on reconnect,
                // so input parked for a host coming back is visible in the fleet bar, not just the
                // status line. (This is a 5-tier red "queued" marker, drawn dim on non-active tabs.)
                let queued = s.pending_bytes();
                let queued_mark = if queued > 0 {
                    format!("⏳{queued}")
                } else {
                    String::new()
                };
                let label = format!(
                    " {}{}{}{}{} {} {}{}{}{} ",
                    bell, recover, flag, pin, head, live, mute, where_s, queued_mark, dot
                );
                // Active tab: tinted by a stable hash of its host (dive context). Inactive tabs fall back
                // to the engine's own accent color so you can spot the "claude" tab from across the bar.
                let color = if active {
                    host_color(&s.meta.host)
                } else {
                    engine_accent(&s.meta.engine, &self.colors.accents)
                };
                x += draw_text(
                    fb,
                    &mut self.cache,
                    &label,
                    x,
                    tab_base,
                    self.font_px,
                    color,
                ) + 12;
                if x > fb.width.saturating_sub(20) {
                    // Tabs past the window edge get clipped with no hint. Show how many are hidden
                    // so a fleet diver knows the bar is truncating rather than assuming fewer tabs.
                    let hidden = self.app.tabs.len() - i - 1;
                    if hidden > 0 {
                        draw_text(
                            fb,
                            &mut self.cache,
                            &format!("⋯ +{hidden}"),
                            fb.width.saturating_sub(58),
                            tab_base,
                            self.font_px,
                            CHROME_DIM,
                        );
                    }
                    break;
                }
            }
            // Fleet triage count, right edge: `↓N` panes down/reconnecting and `!M` busy, so when
            // the fleet is quiet (no per-tab badges, no notifications firing) a diver still sees at
            // a glance that 2 machines went dark or 3 agents are producing. Only shown when non-zero
            // (a fully-healthy, idle fleet draws nothing — no chrome noise). The active tab is
            // excluded from the busy count as it's what we're looking at.
            let down = self
                .app
                .tabs
                .iter()
                .filter(|s| !s.alive() && s.kind() != "pty")
                .count();
            let busy = activity
                .iter()
                .enumerate()
                .filter(|&(i, &b)| b && i != self.app.active)
                .count();
            // Queued type-ahead across down panes (sum of staged bytes) — a host coming back with
            // parked input deserves the triage's attention too.
            let queued: usize = self.app.tabs.iter().map(|s| s.pending_bytes()).sum();
            // Quiet (not recently-produced-output) live sessions — the inverse of busy: a session
            // that's sat silent past the threshold is likely done / parked waiting on you. Shown as
            // a dim `⌛N` alongside busy's `!M` so the two triage counters read together.
            let (any_quiet, quiet_n, _) = self.quiet_flags();
            if down > 0 || busy > 0 || queued > 0 || any_quiet || self.dnd {
                let mut triage = String::new();
                if down > 0 {
                    triage += &format!("↓{down} ");
                }
                if busy > 0 {
                    triage += &format!("!{busy} ");
                }
                if queued > 0 {
                    triage += &format!("⏳{queued} ");
                }
                if any_quiet {
                    triage += &format!("⌛{quiet_n} ");
                }
                if self.dnd {
                    triage += "🔕 ";
                }
                draw_text(
                    fb,
                    &mut self.cache,
                    &triage,
                    fb.width
                        .saturating_sub((triage.chars().count() * self.cell_w as usize) + 24),
                    tab_base,
                    self.font_px,
                    if down > 0 { CHROME_ERR } else { CHROME_DIM },
                );
            }
        } // end if self.focus (tab bar)

        // Status line (bottom row): left = session info, right = hints.
        // `status_base` lives here (not inside the else) because the copy-mode banner below also
        // anchors to it and must keep rendering in focus mode.
        let status_base = fb.height.saturating_sub(self.cell_h as usize / 2);
        if self.focus {
            // Focus mode: no status line either — just the grid.
        } else {
            let mut info = String::new();
            if let Some(s) = self.app.active_session() {
                let link = if s.alive() {
                    "●".to_string()
                } else {
                    // Show the live retry state (attempts + backoff seconds) so a dropped tunnel is
                    // visibly healing itself rather than silently sitting on a dead pane.
                    // If the diver typed into the dead pane, say how much is queued to flush on
                    // reconnect — otherwise they'd assume the keystrokes were lost.
                    let queued = s.pending_bytes();
                    let base = s
                        .retry_info()
                        .unwrap_or_else(|| "reconnecting…".to_string());
                    if queued > 0 {
                        format!("○ {base} · queued {}B", queued)
                    } else {
                        format!("○ {base}")
                    }
                };
                let live = s.live_title().unwrap_or_else(|| s.meta.title.clone());
                let head = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
                info = format!(
                    " {} · {} · {} · [{} {}]",
                    s.meta.host,
                    head,
                    live,
                    s.kind(),
                    link
                );
                // Show how many scrollback lines this session has accumulated, so a diver monitoring a
                // long agent run sees growth at a glance without entering the tab.
                info += &format!(" · {} ln", s.history_len());
            }
            // Link-health badge for the whole fleet (refreshed on the throttled reconnect sweep): a
            // diver wants to know the e2ee tunnel to the harness daemon is up without opening the panel.
            let tunnel = if self.app.fleet.connected {
                "● tunnel up"
            } else {
                "○ tunnel down"
            };
            info = format!("  {} · {}", tunnel, info);
            // When the viewport is scrolled back from the live bottom, say so — a dead giveaway that
            // keys won't take you to fresh output until you press Escape (or the b key). Also show how
            // far back we are as a percentage so a long agent log stays navigable.
            let scrolled_now = self
                .app
                .active_session()
                .map(|s| s.scrolled())
                .unwrap_or(false);
            if scrolled_now {
                let pct = self
                    .app
                    .active_session()
                    .and_then(|s| {
                        let g = s.term.lock();
                        let hist = g.grid().history_size();
                        if hist == 0 {
                            None
                        } else {
                            let above = g.grid().display_offset().min(hist);
                            Some(((hist - above) * 100 / hist).min(100))
                        }
                    })
                    .unwrap_or(0);
                // A fully-scrolled-back log reads 100% (at the very top), which can look like 'live'; keep
                // it below 100 so '100% = at the top' stays unambiguous vs the live-bottom position.
                let pct = pct.min(99);
                info += &format!("  ▾ {pct}% (Esc/b to bottom)");
            }
            // A transient confirmation toast (e.g. "wrote 12k bytes → /path") overrides the session
            // info for a couple seconds so an export's destination is readable before it fades.
            if let Some((text, at)) = &self.flash {
                if at.elapsed() < std::time::Duration::from_secs(3) {
                    info = format!("  ⚑ {text}");
                } else {
                    self.flash = None;
                }
            }
            draw_text(
                fb,
                &mut self.cache,
                &info,
                6,
                status_base,
                self.font_px,
                CHROME_FG,
            );
            let hints = " prefix+/ palette  prefix+a broadcast  prefix+h search all  prefix+n new  prefix+r remote  prefix+s fleet  prefix+o busy  prefix+[ copy  prefix+p paste  prefix+l last  prefix+? help  prefix+q quit ";
            let hw = draw_text(
                fb,
                &mut self.cache,
                hints,
                6,
                status_base,
                self.font_px,
                CHROME_DIM,
            );
            // Move the hint to the right edge by re-drawing after clearing a wide column is complex;
            // simplest right-align: draw hints over the info end offset. We draw at the right edge:
            let hx = fb.width.saturating_sub(hw + 6);
            // Overwrite: clear the column first via black, then draw.
            for py in status_base.saturating_sub(self.font_px as usize)
                ..(status_base + self.font_px as usize)
            {
                for px in hx.min(fb.width)..fb.width {
                    if py < fb.height {
                        fb.pixels[py * fb.width + px] = 0;
                    }
                }
            }
            draw_text(
                fb,
                &mut self.cache,
                hints,
                hx,
                status_base,
                self.font_px,
                CHROME_DIM,
            );
        } // end if self.focus (status line)

        // Copy mode banner: a prominent green status bar so the user knows keystrokes are captured
        // for navigation, with the current motion hints.
        if self.copy_mode {
            let selecting = if self.copy_anchor.is_some() {
                "[selecting]"
            } else {
                "[v=select]"
            };
            let msg = if self.copy_searching {
                format!(
                    " COPY SEARCH /{} · Enter jump · Esc cancel ",
                    self.copy_query
                )
            } else if !self.copy_query.is_empty() {
                format!(" COPY MODE · h/j/k/l/w/b move {} · n/N search /{} · Enter copy · / search · Esc quit ", selecting, self.copy_query)
            } else {
                format!(
                    " COPY MODE · h/j/k/l/w/b move {} · / search · Enter copy · Esc quit ",
                    selecting
                )
            };
            let cw = draw_text(
                fb,
                &mut self.cache,
                &msg,
                6,
                status_base,
                self.font_px,
                (0x00, 0x00, 0x00),
            );
            // Clear the region background to green behind the message for contrast.
            let green = argb(255, 0x18, 0xe0, 0x8a);
            for py in status_base.saturating_sub(self.font_px as usize)
                ..(status_base + self.font_px as usize)
            {
                for px in 0..cw.min(fb.width) {
                    if py < fb.height {
                        fb.pixels[py * fb.width + px] = green;
                    }
                }
            }
            // Re-draw the message in black on green.
            draw_text(
                fb,
                &mut self.cache,
                &msg,
                6,
                status_base,
                self.font_px,
                (0x00, 0x00, 0x00),
            );
        }

        // Overlays.
        match self.app.overlay {
            Overlay::Palette => self.render_palette(fb),
            Overlay::NewSession => self.render_new_session(fb),
            Overlay::RemoteAttach => self.render_remote(fb),
            Overlay::Find => self.render_find(fb),
            Overlay::FleetSearch => self.render_fleet_search(fb),
            Overlay::Fleet => self.render_fleet(fb),
            Overlay::Help => self.render_help(fb),
            Overlay::Rename => self.render_rename(fb),
            Overlay::Broadcast => self.render_broadcast(fb),
            Overlay::Peek => self.render_peek(fb),
            Overlay::FleetGrid => self.render_fleet_grid(fb),
            Overlay::CommandPalette => self.render_command_palette(fb),
            Overlay::Info => self.render_info(fb),
            Overlay::None => {}
        }

        // Tab-bar hover tooltip draws last so it sits on top of everything (its own overlay-less
        // popover). Only shows in chrome mode (focus mode has no bar to hover).
        if self.hover_tab.is_some() {
            self.render_tooltip(fb);
        }
    }

    fn overlay_base_y(&self) -> (usize, usize) {
        let line_px = self.font_px as usize + 6;
        (self.cell_h as usize + 4, line_px)
    }

    /// Hover tooltip: a small popover under the hovered tab showing that session's live tail, so a
    /// fleet diver can check what a backgrounded agent is doing without switching out of the current
    /// tab. Draws last (on top of overlays). Click-through is deliberately not wired — it's purely a
    /// preview; switching still requires a click or the usual keys, so it never disrupts focus.
    fn render_tooltip(&mut self, fb: &mut Framebuffer) {
        let Some(i) = self.hover_tab else {
            return;
        };
        let Some(s) = self.app.tabs.get(i) else {
            return;
        };
        // Roughly the x center of the hovered tab: approximate by the hover cursor clamped to the
        // bar's width, which lands the popover near the label the pointer is actually over.
        let hx = (self
            .cursor
            .0
            .clamp(6.0, (self.size.width.max(60) - 40) as f64)) as usize;
        let base_y = self.cell_h as usize + 8;

        let mut lines: Vec<String> = s.tail(5);
        // Title / state header so the preview is self-describing.
        let head = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
        let live = s.live_title().unwrap_or_else(|| s.meta.title.clone());
        let alive = if s.alive() { "● live" } else { "○ down" };
        // Protection flags (pin 🔒 / mute M) appended so the peek preview shows shielding at a glance.
        let prot = match (
            self.pinned.get(i).copied().unwrap_or(false),
            self.muted.get(i).copied().unwrap_or(false),
        ) {
            (true, true) => " · pinned🔒 muted",
            (true, false) => " · pinned🔒",
            (false, true) => " · muted",
            (false, false) => "",
        };
        lines.insert(0, format!(" {} · {} · {}{}", head, live, alive, prot));
        if lines.len() > 1 {
            lines.push(" (hover → switch? no: click the tab) ".to_string());
        }

        // Measure the widest line so the panel hugs its content.
        let mut wmax = 0usize;
        let colpad = 18;
        for l in &lines {
            let mut w = 0usize;
            for ch in l.chars() {
                if ch != '\n' {
                    w += self.cache.glyph(ch, self.font_px, false).0 as usize;
                }
            }
            wmax = wmax.max(w);
        }
        let panel_w = wmax + colpad * 2;
        let row_px = self.font_px as usize + 4;
        let panel_h = lines.len() * row_px + 14;
        let px0 = hx.min(fb.width.saturating_sub(panel_w + 8));
        let py0 = base_y;
        // Fill the panel background (dim near-black) then a soft border.
        let bg = argb(255, 0x12, 0x12, 0x16);
        for py in py0..(py0 + panel_h).min(fb.height) {
            for px in px0..(px0 + panel_w).min(fb.width) {
                fb.pixels[py * fb.width + px] = bg;
            }
        }
        let border = argb(255, 0x3a, 0x3a, 0x44);
        for px in px0..(px0 + panel_w).min(fb.width) {
            if py0 < fb.height {
                fb.pixels[py0 * fb.width + px] = border;
            }
            if py0 + panel_h - 1 < fb.height {
                fb.pixels[(py0 + panel_h - 1) * fb.width + px] = border;
            }
        }
        for py in py0..(py0 + panel_h).min(fb.height) {
            if px0 < fb.width {
                fb.pixels[py * fb.width + px0] = border;
            }
            if px0 + panel_w - 1 < fb.width {
                fb.pixels[py * fb.width + px0 + panel_w - 1] = border;
            }
        }
        // Draw the header bright, the tail dim.
        let ty = py0 + 8;
        for (k, l) in lines.iter().enumerate() {
            let color = if k == 0 { WHITE } else { CHROME_DIM };
            draw_text(
                fb,
                &mut self.cache,
                l,
                px0 + colpad,
                ty + k * row_px,
                self.font_px,
                color,
            );
        }
    }

    fn render_palette(&mut self, fb: &mut Framebuffer) {
        // Recompute the filter (mirrors tui::refresh_filter).
        self.app.refresh_filter();
        let (base_y, line_px) = self.overlay_base_y();
        draw_text(
            fb,
            &mut self.cache,
            &format!("🔍 {}", self.app.query),
            32,
            base_y,
            self.font_px,
            WHITE,
        );
        for (row, &i) in self.app.filtered.iter().enumerate().take(12) {
            let s = &self.app.tabs[i];
            let sel = row == self.app.selected;
            let color = if sel { WHITE } else { CHROME_DIM };
            let name = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
            // Compact status flags so a jump carries context: live/pin/mute next to the name.
            let live = if s.alive() { "" } else { "○" };
            let pin = if self.pinned.get(i).copied().unwrap_or(false) {
                "🔒"
            } else {
                " "
            };
            let mute = if self.muted.get(i).copied().unwrap_or(false) {
                "M"
            } else {
                " "
            };
            let flags = format!("{live}{pin}{mute}");
            let line = format!(
                "  {} {} · {} · {}  {}",
                flags,
                s.meta.host,
                name,
                s.meta.title,
                if sel { "◄" } else { "" }
            );
            draw_text(
                fb,
                &mut self.cache,
                &line,
                32,
                base_y + (row + 1) * line_px,
                self.font_px,
                color,
            );
        }
    }

    /// New-session picker: a `dir:` working-directory line on top, then the engine list below
    /// (Up/Down selects the engine, typing edits the directory). Mirrors the RemoteAttach overlay.
    fn render_new_session(&mut self, fb: &mut Framebuffer) {
        let (base_y, line_px) = self.overlay_base_y();
        draw_text(
            fb,
            &mut self.cache,
            "  new session  ",
            32,
            base_y,
            self.font_px,
            WHITE,
        );
        // Rows: header(0), legend(1), dir(2), engines(3+). The engine list offsets below account
        // for the legend + dir slots.
        draw_text(
            fb,
            &mut self.cache,
            "  ✗ = not on PATH  ",
            32,
            base_y + line_px,
            self.font_px,
            CHROME_DIM,
        );
        if self.new_cwd.is_empty() {
            draw_text(
                fb,
                &mut self.cache,
                "  dir:  (blank = start_cwd · pre-filled from last use)  ",
                32,
                base_y + 2 * line_px,
                self.font_px,
                CHROME_FG,
            );
        } else {
            draw_text(
                fb,
                &mut self.cache,
                &format!("  dir: {}", self.new_cwd),
                32,
                base_y + 2 * line_px,
                self.font_px,
                CHROME_FG,
            );
        }
        let ordered = self.app.engine_order();
        // Cache which engines are actually on this machine's PATH so the picker isn't re-scanning
        // disk every frame (a half-dozen stat calls per tab-frame adds up); still a hint, not a
        // promise — a present binary can fail at spawn.
        let installed: Vec<bool> = ordered
            .iter()
            .map(|e| crate::engines::is_installed(e.cmd))
            .collect();
        for (i, e) in ordered.iter().enumerate() {
            let sel = i == self.app.selected;
            let present = installed[i];
            let color = if sel {
                WHITE
            } else if present {
                CHROME_DIM
            } else {
                CHROME_ERR
            };
            let mark = if sel {
                "◄"
            } else if present {
                ""
            } else {
                "✗"
            };
            let line = format!("  {}  {}  {}", e.id, e.label, mark);
            draw_text(
                fb,
                &mut self.cache,
                &line,
                32,
                base_y + (i + 3) * line_px,
                self.font_px,
                color,
            );
        }
        // Under the engine list, a running description of the selected engine so a diver can read
        // what each framework is before committing to a spawn.
        if let Some(e) = ordered.get(self.app.selected) {
            draw_text(
                fb,
                &mut self.cache,
                &format!("      {} — {}", e.label, e.desc),
                32,
                base_y + (ENGINES.len() + 3) * line_px,
                self.font_px,
                CHROME_DIM,
            );
        }
    }

    /// Read-only fleet panel: the machine id + tunnel state, then one line per harness session with
    /// a live/stale marker. Esc (or any key) dismisses; `s` re-fetches. Never writes to a pane.
    fn render_fleet(&mut self, fb: &mut Framebuffer) {
        let (base_y, line_px) = self.overlay_base_y();
        // Recompute the filter each frame so typing filters live (mirrors the palette overlay).
        self.fleet_refresh_filter();
        let f = &self.app.fleet;
        let mid = if f.machine_id.is_empty() {
            "unknown".to_string()
        } else {
            f.machine_id.chars().take(6).collect()
        };
        let tunnel = if f.connected {
            "tunnel up"
        } else {
            "tunnel down"
        };
        let n = f.fleet.len();
        let shown = self.fleet_filtered.len();
        let q = if self.fleet_query.is_empty() {
            String::new()
        } else {
            format!("/{} ", self.fleet_query)
        };
        draw_text(
            fb,
            &mut self.cache,
            &format!(
                "  fleet · {} · {} · {} session{} · {}type to filter · Up/Down+Enter to dive  ",
                mid,
                tunnel,
                n,
                if n == 1 { "" } else { "s" },
                q
            ),
            32,
            base_y,
            self.font_px,
            WHITE,
        );
        if n == 0 {
            draw_text(
                fb,
                &mut self.cache,
                "  no harness sessions (daemon unreachable or nothing joined)  ",
                32,
                base_y + line_px,
                self.font_px,
                CHROME_DIM,
            );
            return;
        }
        if shown == 0 && !self.fleet_query.is_empty() {
            draw_text(
                fb,
                &mut self.cache,
                "  no sessions match  ",
                32,
                base_y + line_px,
                self.font_px,
                CHROME_DIM,
            );
            return;
        }
        for (row, &real) in self.fleet_filtered.iter().enumerate().take(20) {
            let s = &f.fleet[real];
            let live = s.is_live();
            let sel = row == self.app.selected;
            let mark = if live { "●" } else { "○" };
            let color = if sel {
                WHITE
            } else if live {
                (0x4a, 0xe0, 0x8a)
            } else {
                CHROME_DIM
            };
            let eng = if s.engine.is_empty() {
                "?"
            } else {
                s.engine.as_str()
            };
            let id = if s.session_id.is_empty() {
                s.tmux_pane.clone()
            } else {
                s.session_id.chars().take(8).collect()
            };
            let line = format!(
                "  {} {}  {:<9} {}{}",
                mark,
                eng,
                "",
                id,
                if sel { "  ◄" } else { "" }
            );
            draw_text(
                fb,
                &mut self.cache,
                &line,
                32,
                base_y + (row + 1) * line_px,
                self.font_px,
                color,
            );
        }
    }

    /// Render the fleet-search overlay: query prompt, total/selected match counts, the currently
    /// selected match's session + matching line, and a list of the first ~8 hits each prefixed with
    /// its tab label (engine / host). The selected row is highlighted.
    fn render_fleet_search(&mut self, fb: &mut Framebuffer) {
        let (base_y, line_px) = self.overlay_base_y();
        let n = self.fleet_matches.len();
        // Header row: query, live match count, and "no matches" when the query misses everywhere.
        let (hdr, hdr_color) = if self.fleet_q.is_empty() {
            (
                "  search all sessions: (type to match every tab)  ".to_string(),
                CHROME_DIM,
            )
        } else if n == 0 {
            (
                format!("  search all sessions: {}  (no matches)", self.fleet_q),
                CHROME_DIM,
            )
        } else {
            let here = (self.fleet_sel % n) + 1;
            let totals = format!(
                "  search all sessions: {}  · {} match{} across {} tab{} · showing {} of {}  ",
                self.fleet_q,
                n,
                if n == 1 { "" } else { "es" },
                self.app.tabs.len(),
                if self.app.tabs.len() == 1 { "" } else { "s" },
                here,
                n,
            );
            (totals, WHITE)
        };
        draw_text(
            fb,
            &mut self.cache,
            &hdr,
            32,
            base_y,
            self.font_px,
            hdr_color,
        );

        // The list of matches: up to 8 rows, each prefixed with its tab's engine/host label.
        let list_rows = if self.fleet_q.is_empty() || n == 0 {
            0
        } else {
            8.min(n)
        };
        for row in 0..list_rows {
            let m = self.fleet_matches[row];
            let selected = row == self.fleet_sel;
            let color = if selected { WHITE } else { CHROME_DIM };
            // Tab label: user name → engine id @ host.
            let s = &self.app.tabs[m.tab];
            let name = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
            let label = format!("{}@{}", name, s.meta.host);
            // The matched line text, read live from that session's grid at render time.
            let raw: String = {
                let g = s.term.lock();
                let cols = g.columns();
                use alacritty_terminal::index::{Column, Line};
                g.grid()[Line(m.line)][Column(0)..Column(cols)]
                    .iter()
                    .map(|c| c.c)
                    .collect()
            };
            let text = if raw.trim().is_empty() {
                "(blank line)".to_string()
            } else {
                raw.trim_end().to_string()
            };
            let line = format!(
                "  [{}] {}  {}",
                label,
                if selected { "◄" } else { " " },
                text
            );
            draw_text(
                fb,
                &mut self.cache,
                &line,
                32,
                base_y + (row + 1) * line_px,
                self.font_px,
                color,
            );
        }
    }

    /// Keybinding reference overlay. Static list; dismiss on any key.
    /// The live key label for a prefix action, honoring any `[keybindings]` remap: `"prefix X"`.
    /// `key_action` is `key → action` (already resolved through config + defaults), so reverse it to
    /// find the active key for this action.
    fn prefix_label(&self, action: &str) -> String {
        let key = self
            .key_action
            .iter()
            .find(|(_, a)| a.as_str() == action)
            .map(|(k, _)| k.as_str())
            .unwrap_or(action);
        format!("prefix {key}")
    }

    fn render_help(&mut self, fb: &mut Framebuffer) {
        let (base_y, line_px) = self.overlay_base_y();
        draw_text(
            fb,
            &mut self.cache,
            "  harness-terminal keys  ",
            32,
            base_y,
            self.font_px,
            WHITE,
        );
        // Build the rows in display order. Remappable actions resolve their key label live (so a
        // `[keybindings]` remap shows the REAL key, not a stale default); fixed/multi-key rows are
        // literals. Each row is a `&str` key + `&str` description.
        let mut all: Vec<(String, String)> = Vec::new();
        for (action, desc) in [
            ("", "harness-terminal keys"),
            ("palette", "jump to any session"),
            ("command_palette", "command palette (all actions)"),
            ("new_session", "new session (engine + working dir)"),
            ("remote_attach", "attach to a remote pane@host"),
            ("fleet", "fleet status"),
            ("search", "search scrollback"),
            ("search_all", "search all sessions (fleet)"),
            ("copy_mode", "copy mode"),
            ("rename", "rename the active tab"),
            ("broadcast", "broadcast a line to all sessions"),
            ("paste", "paste clipboard (bracketed)"),
            ("next_busy", "jump to next busy tab"),
            ("next_quiet", "jump to next quiet (awaiting-you) tab"),
            ("next_down", "jump to next down/reconnecting tab"),
            ("next_pinned", "jump to next pinned tab"),
            ("next_host", "jump to next host (page the fleet by machine)"),
            ("dnd", "toggle do-not-disturb (mute all OS notifications)"),
            ("reconnect", "force reconnect active tab (bypass backoff)"),
            ("reconnect_all", "force reconnect ALL down panes at once"),
            ("destroy", "kill active tab's remote pane"),
            ("interrupt", "send Ctrl-C to active tab (stop the run)"),
            ("close_quiet", "close all quiet (done) tabs at once"),
            ("mute", "mute/unmute the active tab"),
            ("pin", "pin/unpin the active tab (protect from close)"),
            ("last_window", "flip to the previous tab"),
            ("undo_close", "undo close (reopen last closed tab)"),
            ("duplicate", "duplicate this tab (fork same engine@host)"),
            ("copy_scrollback", "copy whole scrollback to clipboard"),
            ("export_scrollback", "write scrollback to a .log file"),
            (
                "copy_identity",
                "copy session identity (engine@host) to clipboard",
            ),
            ("copy_fleet", "copy whole fleet summary (all tabs)"),
            ("peek", "peek tails of all sessions"),
            ("fleet_grid", "fleet grid: live tails of every session"),
            ("session_info", "show this tab's info (kind/host/task)"),
            ("toggle_focus", "focus mode (hide tab bar + status)"),
            ("help", "this help"),
            ("quit", "quit"),
        ] {
            if action.is_empty() {
                all.push((
                    "Ctrl+Space".to_string(),
                    "prefix (then a command)".to_string(),
                ));
            } else {
                all.push((self.prefix_label(action), desc.to_string()));
            }
        }
        for (k, d) in [
            ("prefix { }", "move tab left / right"),
            ("1-9 / 0 / Tab", "switch tab (0 = last)"),
            ("x / c", "close tab / go to tab 0"),
            ("g / b", "scroll up a page / jump to bottom"),
            ("Ctrl+= / Ctrl+-", "font zoom (Ctrl+0 reset)"),
            ("Ctrl+Enter", "toggle fullscreen"),
            ("PgUp/PgDn", "scrollback"),
            ("Cmd/Ctrl+click", "open URL / file path"),
        ] {
            all.push((k.to_string(), d.to_string()));
        }

        for (row, (k, d)) in all.iter().enumerate() {
            draw_text(
                fb,
                &mut self.cache,
                &format!("  {:<16} {}", k, d),
                32,
                base_y + (row + 1) * line_px,
                self.font_px,
                CHROME_DIM,
            );
        }
    }

    fn render_info(&mut self, fb: &mut Framebuffer) {
        let (base_y, line_px) = self.overlay_base_y();
        let Some(s) = self.app.active_session() else {
            return;
        };
        // Which line draws at which row. Title is the panel header; the rest are data rows.
        let title = format!("  {} · {}  ", s.kind(), s.meta.engine);
        draw_text(fb, &mut self.cache, &title, 32, base_y, self.font_px, WHITE);

        let mut rows: Vec<String> = Vec::new();
        let display = if let Some(n) = &s.meta.name {
            n.clone()
        } else {
            s.meta.engine.clone()
        };
        rows.push(format!("  name       {}", display));
        rows.push(format!("  host       {}", s.meta.host));
        rows.push(format!("  engine     {}", s.meta.engine));
        rows.push(format!("  transport  {}", s.kind()));
        rows.push(format!(
            "  state      {}",
            match s.retry_info() {
                Some(r) => format!("down · {r}"),
                None => "live".to_string(),
            }
        ));
        // Idle age — how long this session has sat without producing output. Negative/zero means
        // it produced output this very frame (or we never sampled it); otherwise the readable time.
        if s.alive() && s.kind() != "pty" {
            let now = std::time::Instant::now();
            let idle = now.saturating_duration_since(self.last_output[self.app.active]);
            let idle_txt = if idle.is_zero() || self.seen_history[self.app.active] == usize::MAX {
                "now".to_string()
            } else {
                format!("quiet {}", fmt_duration(idle))
            };
            rows.push(format!("  silence    {idle_txt}"));
        }
        // Staged type-ahead (visible in the bar too): how much input is parked to flush on reconnect.
        let queued = s.pending_bytes();
        if queued > 0 {
            rows.push(format!(
                "  queued     ⏳{}B staged, flush on reconnect",
                queued
            ));
        }
        rows.push(format!(
            "  view       {}",
            if s.scrolled() {
                "scrolled into history"
            } else {
                "live follow (bottom)"
            }
        ));
        // Pin/mute protection status — same flags the tab bar badges, so the info panel is a
        // one-stop read of a tab's shielding without hunting the bar.
        rows.push(format!(
            "  protec     {}",
            match (
                self.pinned.get(self.app.active).copied().unwrap_or(false),
                self.muted.get(self.app.active).copied().unwrap_or(false),
            ) {
                (true, true) => "pinned 🔒 · muted M",
                (true, false) => "pinned 🔒",
                (false, true) => "muted M",
                (false, false) => "none",
            }
        ));
        // The OSC task title, if the shell/agent has set one — the running task context.
        if let Some(t) = s.live_title() {
            rows.push(format!("  task       {t}"));
        }

        // Terminal grid size = what the session actually render (its own lines × cols), read from
        // the emulator's current size without disturbing it.
        let size = {
            let term = s.term.lock();
            format!("  size       {}×{}", term.screen_lines(), term.columns())
        };
        rows.push(size);

        for (row, line) in rows.iter().enumerate() {
            draw_text(
                fb,
                &mut self.cache,
                line,
                32,
                base_y + (row + 1) * line_px,
                self.font_px,
                CHROME_DIM,
            );
        }
    }

    fn render_remote(&mut self, fb: &mut Framebuffer) {
        let (base_y, line_px) = self.overlay_base_y();
        draw_text(
            fb,
            &mut self.cache,
            "  attach to pane@host  ",
            32,
            base_y,
            self.font_px,
            WHITE,
        );
        draw_text(
            fb,
            &mut self.cache,
            &format!("  host[:port] {}", self.app.remote_host),
            32,
            base_y + line_px,
            self.font_px,
            CHROME_FG,
        );
        for (i, e) in ENGINES.iter().enumerate() {
            let sel = i == self.app.selected;
            let color = if sel { WHITE } else { CHROME_DIM };
            let line = format!("  {}  {}  {}", e.id, e.label, if sel { "◄" } else { "" });
            draw_text(
                fb,
                &mut self.cache,
                &line,
                32,
                base_y + (i + 3) * line_px,
                self.font_px,
                color,
            );
        }
        if let Some(e) = ENGINES.get(self.app.selected) {
            draw_text(
                fb,
                &mut self.cache,
                &format!("      {} — {}", e.label, e.desc),
                32,
                base_y + (ENGINES.len() + 3) * line_px,
                self.font_px,
                CHROME_DIM,
            );
        }
    }

    /// Recompute the focused search match (from the top if none, else continue from it) and scroll
    /// the viewport so the match is visible at the top of the grid area.
    /// Scroll the viewport so the focused match's line is visible (at the top of the screen).
    fn find_scroll_to(
        &self,
        g: &mut alacritty_terminal::term::Term<crate::session::Listener>,
        l: i32,
    ) {
        use alacritty_terminal::grid::Scroll;
        let current = g.grid().display_offset() as i32;
        let desired = (-l as i32).clamp(0, g.grid().history_size() as i32);
        g.grid_mut()
            .scroll_display(Scroll::Delta(desired - current));
    }

    /// Recompute the occurrence list after a query edit; focuses the first match (or the match
    /// nearest the previous focus) so the viewport tracks the user.
    fn find_recompute(&mut self, _start: Option<i32>) {
        let Some(active) = self.app.active_session() else {
            self.find_hit = None;
            self.find_all = Vec::new();
            self.find_index = 0;
            return;
        };
        let mut g = active.term.lock();
        // Remember the previous focus position so edits keep roughly the same match in view.
        let prev_line = self.find_hit.map(|(l, _, _)| l);
        self.find_all = crate::render::all_matches(&g, &self.find_query);
        // Pick the first match at-or-after the old focus; else the very first match.
        let idx = if self.find_all.is_empty() {
            0
        } else {
            prev_line
                .and_then(|pl| self.find_all.iter().position(|&(l, _, _)| l >= pl))
                .unwrap_or(0)
        };
        self.find_index = idx;
        self.find_hit = self.find_all.get(idx).copied();
        if let Some((l, _, _)) = self.find_hit {
            self.find_scroll_to(&mut *g, l);
            active.set_scrolled(true);
        }
    }

    /// Recompute the fleet-matches list across EVERY open session's scrollback for the current
    /// query. Collects each matching (tab, line, col) and sorts by tab then line, then keeps the
    /// selection within range. Called on every query edit while the FleetSearch overlay is open; a
    /// plain loop so each match carries its source tab index (unlike `all_matches`, which has none).
    fn fleet_recompute(&mut self) {
        self.fleet_matches.clear();
        let q = self.fleet_q.to_lowercase();
        if !q.is_empty() {
            // Gather the term locks into a slice so the shared, testable helper can run over them.
            let terms: Vec<_> = self.app.tabs.iter().map(|s| Arc::clone(&s.term)).collect();
            self.fleet_matches = collect_fleet_matches(&terms, &q);
        }
        if self.fleet_sel >= self.fleet_matches.len() {
            self.fleet_sel = self.fleet_matches.len().saturating_sub(1);
        }
    }

    /// Jump the fleet selection by `delta` rows (wrapping around the ends).
    fn fleet_jump(&mut self, delta: isize) -> bool {
        let m = self.fleet_matches.len();
        if m == 0 {
            return false;
        }
        self.fleet_sel = (self.fleet_sel as isize + delta).rem_euclid(m as isize) as usize;
        true
    }

    /// Fleet-search Enter: focus the matching session, scroll its scrollback so the hit line is
    /// visible, and leave the read cursor on the match's start column so it's on-screen. Closes the
    /// overlay. No-op if there's nothing selected.
    fn fleet_jump_to(&mut self) {
        let Some(m) = self.fleet_matches.get(self.fleet_sel) else {
            self.app.overlay = Overlay::None;
            return;
        };
        let tab = m.tab;
        // Focus the session's tab first so the scroll/copy targets the same session the renderer draws.
        self.app.active = tab.min(self.app.tabs.len().saturating_sub(1));
        crate::restore::save_active(self.app.active);
        if let Some(s) = self.app.tabs.get(self.app.active) {
            let mut g = s.term.lock();
            // Scroll so the match line is at the top of the viewport.
            use alacritty_terminal::grid::Scroll;
            let current = g.grid().display_offset() as i32;
            let desired = (-m.line as i32).clamp(0, g.grid().history_size() as i32);
            g.grid_mut()
                .scroll_display(Scroll::Delta(desired - current));
            s.set_scrolled(true);
        }
        // Place the read cursor at the match start so it's clearly visible where the hit landed.
        self.copy_pos = (m.line, m.col);
        self.app.overlay = Overlay::None;
    }

    /// Jump the focus by `delta` through the matches (wrapping around the ends), updating the
    /// focused highlight and the viewport. Returns false when there's nothing to step to.
    fn find_jump(&mut self, delta: isize) -> bool {
        let m = self.find_all.len();
        if m == 0 {
            return false;
        }
        let next = (self.find_index as isize + delta).rem_euclid(m as isize) as usize;
        self.find_index = next;
        self.find_hit = self.find_all.get(next).copied();
        let Some(active) = self.app.active_session() else {
            return false;
        };
        if let Some((l, _, _)) = self.find_hit {
            let mut g = active.term.lock();
            self.find_scroll_to(&mut *g, l);
            active.set_scrolled(true);
        }
        true
    }

    /// Enter copy mode: anchor the read cursor at the top-left visible cell so the user starts
    /// where they can see, and keep the view scrolled (copy mode lives in the scrollback).
    fn start_copy_mode(&mut self) {
        let Some(active) = self.app.active_session() else {
            return;
        };
        let g = active.term.lock();
        if g.grid().history_size() == 0 {
            return; // nothing to scroll/copy yet
        }
        // Place the cursor at the first visible (top of viewport) cell.
        let top = g.grid().display_offset();
        self.copy_pos = ((top as i32 * -1), 0);
        self.copy_anchor = None;
        self.copy_query.clear();
        self.copy_searching = false;
        self.copy_mode = true;
        active.set_scrolled(true);
    }

    /// Copy the current copy-mode selection (anchor→pos inclusive) to the clipboard via
    /// selection_to_string, then leave copy mode. No-op if nothing is selected.
    fn copy_mode_copy(&mut self) {
        if self.copy_anchor.is_none() {
            self.copy_mode = false;
            return;
        }
        let Some(active) = self.app.active_session() else {
            return;
        };
        let mut g = active.term.lock();
        // Build a simple rectangular selection from the two grid points, install it as the live
        // selection, and read it via alacritty's range-to-string so line handling (wrap vs hard
        // newline) is correct. We restore/clear the live selection afterward.
        let (a, b) = (self.copy_anchor.unwrap(), self.copy_pos);
        let (lo, hi) = if (a, b) < (b, a) { (a, b) } else { (b, a) };
        use alacritty_terminal::index::{Column, Line, Point, Side};
        use alacritty_terminal::selection::{Selection, SelectionType};
        let start = Point::new(Line(lo.0), Column(lo.1));
        let end = Point::new(Line(hi.0), Column(hi.1));
        let mut sel = Selection::new(SelectionType::Simple, start, Side::Left);
        sel.update(end, Side::Right);
        g.selection = Some(sel);
        let text = g.selection_to_string().unwrap_or_default();
        g.selection = None;
        self.copy_mode = false;
        self.copy_anchor = None;
        self.copy_query.clear();
        self.copy_searching = false;
        if !text.is_empty() {
            drop(g);
            if let Ok(mut cb) = arboard::Clipboard::new() {
                let _ = cb.set_text(text);
            }
        }
    }

    /// Move the copy-mode read cursor by a grid delta, keeping it in-bounds and extending the
    /// selection if one is active.
    fn copy_move(&mut self, dl: i32, dc: i32) {
        let Some(active) = self.app.active_session() else {
            return;
        };
        let g = active.term.lock();
        let cols = g.columns() as i32;
        let max_line = g.grid().bottommost_line().0;
        let min_line = g.grid().topmost_line().0;
        let (l, c) = self.copy_pos;
        let l = (l + dl).clamp(min_line, max_line);
        let c = (c as i32 + dc).clamp(0, cols - 1) as usize;
        self.copy_pos = (l, c);
    }

    /// Jump the copy-mode read cursor to the next match of `copy_query` at-or-after (or, for
    /// `backwards`, at-or-before) the current position, wrapping around the scrollback. Sets the
    /// cursor to the match's start column so the user can Inspect/select from it. Returns whether a
    /// match was found.
    fn copy_goto(&mut self, backwards: bool) {
        let Some(active) = self.app.active_session() else {
            return;
        };
        if self.copy_query.is_empty() {
            return;
        }
        let query = self.copy_query.clone();
        let (cur_line, _) = self.copy_pos;
        // Start the scan just past the cursor (forwards) or just before it (backwards) so `n`/`N`
        // walk distinct matches rather than re-selecting the current one.
        let search_start = if backwards {
            cur_line - 1
        } else {
            cur_line + 1
        };
        let g = active.term.lock();
        let full = crate::render::all_matches(&g, &query);
        drop(g);
        if full.is_empty() {
            self.copy_pos = (cur_line, 0);
            return;
        }
        // Pick the nearest match at/after `search_start` (forwards) or at/before it (backwards),
        // wrapping to the other end of the scrollback when there's no further hit on that side.
        let chosen = if backwards {
            full.iter()
                .filter(|m| m.0 <= search_start)
                .max_by_key(|m| m.0)
                .or_else(|| full.iter().max_by_key(|m| m.0))
        } else {
            full.iter()
                .filter(|m| m.0 >= search_start)
                .min_by_key(|m| m.0)
                .or_else(|| full.iter().min_by_key(|m| m.0))
        };
        if let Some((l, c, _)) = chosen.copied() {
            self.copy_pos = (l, c);
        }
    }

    /// Render the search overlay: query prompt + current match location in the status area.
    fn render_find(&mut self, fb: &mut Framebuffer) {
        let status_base = fb.height.saturating_sub(self.cell_h as usize / 2);
        // Overlay status line with query and match count info.
        let line = if self.find_query.is_empty() {
            "  find: (type to search)  ".to_string()
        } else {
            let n = self.find_all.len();
            if n > 0 {
                let here = (self.find_index % n) + 1;
                format!(
                    "  find: {}  (match {} of {} · Enter/Tab next, Shift+Enter prev)",
                    self.find_query, here, n
                )
            } else {
                format!("  find: {}  (no match)", self.find_query)
            }
        };
        draw_text(
            fb,
            &mut self.cache,
            &line,
            6,
            status_base,
            self.font_px,
            WHITE,
        );
    }

    /// Render the rename overlay: show what the tab is currently called and the in-progress name.
    fn render_rename(&mut self, fb: &mut Framebuffer) {
        let status_base = fb.height.saturating_sub(self.cell_h as usize / 2);
        let cur = self
            .app
            .active_session()
            .map(|s| s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone()))
            .unwrap_or_default();
        let prompt = if self.rename_query.is_empty() {
            "  rename tab: (type a name, Enter keeps, Esc cancels)  ".to_string()
        } else {
            format!("  rename tab: {} ▏", self.rename_query)
        };
        draw_text(
            fb,
            &mut self.cache,
            &format!("  currently: {}  ", cur),
            6,
            status_base - self.cell_h as usize,
            self.font_px,
            CHROME_DIM,
        );
        draw_text(
            fb,
            &mut self.cache,
            &prompt,
            6,
            status_base,
            self.font_px,
            WHITE,
        );
    }

    /// Render the broadcast overlay: the in-progress line, then a checkbox list of every session
    /// with its target state. Space toggles the focused row; Enter sends only the marked targets.
    fn render_broadcast(&mut self, fb: &mut Framebuffer) {
        let (base_y, line_px) = self.overlay_base_y();
        let n = self.broadcast_targets.iter().filter(|&&t| t).count();
        let prompt = if self.broadcast_query.is_empty() {
            format!("  send line to {n} of {} session{} (↑/↓ focus · Space=toggle · ⇧↑/⇧↓ history · Enter=broadcast · Esc=cancel)  ",
                self.app.tabs.len(), if n == 1 { "" } else { "s" })
        } else {
            format!(
                "  broadcast to {n} session{}: {} ▏",
                if n == 1 { "" } else { "s" },
                self.broadcast_query
            )
        };
        draw_text(
            fb,
            &mut self.cache,
            &prompt,
            32,
            base_y,
            self.font_px,
            WHITE,
        );
        for (row, s) in self.app.tabs.iter().enumerate().take(20) {
            let on = self.broadcast_targets.get(row).copied().unwrap_or(false);
            let mark = if on { "☑" } else { "☐" };
            let name = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
            let line = format!("  {} {} @ {}", mark, name, s.meta.host);
            let color = if row == self.broadcast_sel {
                WHITE
            } else {
                CHROME_DIM
            };
            draw_text(
                fb,
                &mut self.cache,
                &line,
                32,
                base_y + (row + 2) * line_px,
                self.font_px,
                color,
            );
        }
    }

    /// Render the peek overlay: a header, then one compact row per session with either the dimmed
    /// tail of every other session folded in and the highlighted row expanded to a ~4-line preview
    /// of its last scrollback lines (WHITE). The selection index is `self.peek_sel`.
    fn render_fleet_grid(&mut self, fb: &mut Framebuffer) {
        let (_, line_px) = self.overlay_base_y();
        let n = self.app.tabs.len();
        draw_text(
            fb,
            &mut self.cache,
            &format!(
                "  fleet grid · {} session{} · ↑/↓/1-9 select · Enter dive · Esc close  ",
                n,
                if n == 1 { "" } else { "s" }
            ),
            32,
            self.cell_h as usize + 2,
            self.font_px,
            WHITE,
        );
        if n == 0 {
            return;
        }
        // Clamp the selected tile into range (tabs can close underneath the overlay).
        self.grid_sel = self.grid_sel.min(n - 1);
        // Same per-frame output-activity pass the tab bar / triage use, so each tile's status glyphs
        // match the rest of the chrome: busy = produced output, quiet = sat silent past threshold.
        let activity = self.activity_flags();
        let (_, _, _max_idle) = self.quiet_flags();
        // Layout the grid. Tile geometry is a full text cell (the grid rows draw just below the
        // header). Fit as many columns as the window width allows at the active cell width.
        let gcol = self.cell_w as usize;
        let grow = self.cell_h as usize;
        let x0 = 8usize;
        let y0 = (self.cell_h as usize + 2) + line_px;
        let inner_w = fb.width.saturating_sub(x0 + 8);
        // Tile geometry is a full text cell (header + up to 3 tail lines). Fit as many columns as
        // the window allows; deeper rows simply clip at the window bottom.
        let cols = (inner_w / (gcol * 12)).max(1).min(n.max(1));
        let tw = inner_w / cols;
        let th = grow * 4;

        // We draw the tail with the session's own colors so the overview reads like the real panes.
        for (idx, s) in self.app.tabs.iter().enumerate() {
            let (c, r) = (idx % cols, idx / cols);
            let (tx, ty) = (x0 + c * (tw + 8), y0 + r * (th + 8));
            if ty + th > fb.height {
                break;
            }
            // A thin highlight border for the focused tile.
            let selected = idx == self.grid_sel;
            let border = if selected { WHITE } else { CHROME_DIM };
            for (bord_x, color) in [(tx, border), (tx + tw.saturating_sub(1), border)] {
                for yy in ty..ty + th {
                    fb.set(bord_x, yy, argb(0xff, color.0, color.1, color.2));
                }
            }
            for yy in [ty, ty + th.saturating_sub(1)] {
                for xx in tx..tx + tw {
                    fb.set(xx, yy, argb(0xff, border.0, border.1, border.2));
                }
            }
            let head = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
            let host = if s.kind() == "pty" {
                String::new()
            } else {
                format!("@{}", s.meta.host)
            };
            // Status glyph, matching the tab-bar chrome: nothing for a live idle tab; `!N` busy,
            // `⌛` quiet (awaiting you), `○`/`↓` down, `↻` just-reconnected, `⏳N` queued-input,
            // `🔒` pinned, `M` muted.
            // Quiet and busy are mutually exclusive; down wins over both. The focused tile is the
            // one you're actively reading, so it's never flagged busy/quiet (same rule as the bar).
            let is_down = !s.alive() && s.kind() != "pty";
            let busy = activity
                .get(idx)
                .copied()
                .unwrap_or(false)
                .then_some(self.grew_delta.get(idx).copied().unwrap_or(0))
                .unwrap_or(0);
            let clipped = s.pending_bytes();
            let glyph = if is_down {
                "○".to_string()
            } else if idx != self.grid_sel && activity[idx] {
                format!("!{}", busy)
            } else if idx != self.grid_sel && self.quiet_for(idx) {
                "⌛".to_string()
            } else {
                String::new()
            };
            let mut glyph_s = glyph;
            if self.recover_until.get(idx).copied().flatten().is_some() {
                glyph_s += "↻";
            }
            if self.pinned.get(idx).copied().unwrap_or(false) {
                glyph_s.push('🔒');
            }
            if self.muted.get(idx).copied().unwrap_or(false) {
                glyph_s.push('M');
            }
            if clipped > 0 {
                glyph_s.push_str(&format!("⏳{}", clipped));
            }
            let pfx = if glyph_s.is_empty() {
                String::new()
            } else {
                format!("{glyph_s} ")
            };
            let header = format!(
                "  {}{}  {head}{} {}",
                idx + 1,
                host,
                if selected { " ◄" } else { "" },
                pfx
            );
            draw_text(
                fb,
                &mut self.cache,
                &header,
                tx + 2,
                ty + grow,
                self.font_px,
                if selected { WHITE } else { CHROME_DIM },
            );
            // Live near-tail lines, reversed (tail() is newest-first) so the tile reads top-to-bottom
            // as a terminal would — newest sits on the bottom row. Uses the grid's own foreground.
            let mut tail = s.tail(3);
            tail.reverse();
            for (k, tl) in tail.iter().enumerate().take(3) {
                let t = if tl.chars().count() > (tw / gcol).saturating_sub(2) {
                    let cut: String = tl.chars().take((tw / gcol).saturating_sub(2)).collect();
                    format!("{cut}…")
                } else {
                    tl.clone()
                };
                draw_text(
                    fb,
                    &mut self.cache,
                    &t,
                    tx + 2,
                    ty + (k + 2) * grow,
                    self.font_px,
                    self.colors.fg,
                );
            }
        }
    }

    fn render_peek(&mut self, fb: &mut Framebuffer) {
        let (base_y, line_px) = self.overlay_base_y();
        draw_text(
            fb,
            &mut self.cache,
            "  peek · ↑/↓ preview  · Enter jump · Esc close  ",
            32,
            base_y,
            self.font_px,
            WHITE,
        );
        // Cap the visible window (10 rows + preview lines), but scroll through ALL tabs: `peek_scroll`
        // offsets the start so sessions beyond the first window are reachable, matching peek_sel.
        let rows = self.app.tabs.len().min(10);
        for row in 0..rows {
            let i = self.peek_scroll + row;
            if i >= self.app.tabs.len() {
                break;
            }
            let s = &self.app.tabs[i];
            let sel = i == self.peek_sel;
            let color = if sel { WHITE } else { CHROME_DIM };
            let name = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
            let live = s
                .live_title()
                .unwrap_or_else(|| s.meta.title.clone())
                .replace('\n', " ");
            let line = format!(
                "  {} · {} · {}  {}",
                s.meta.host,
                name,
                live,
                if sel { "◄" } else { "" }
            );
            let row_y = base_y + (row + 1) * line_px;
            draw_text(fb, &mut self.cache, &line, 32, row_y, self.font_px, color);
            // Expand the highlighted row: dim preview of the last ~4 scrollback lines underneath.
            if sel {
                let scrollback = s.capture_scrollback();
                let lines: Vec<&str> = scrollback
                    .split('\n')
                    .map(|l| l.trim_end())
                    .filter(|l| !l.is_empty())
                    .collect();
                let start = lines.len().saturating_sub(4);
                for (k, tl) in lines[start..].iter().enumerate().take(4) {
                    let t = if tl.chars().count() > 90 {
                        tl.chars().take(90).collect::<String>() + "…"
                    } else {
                        tl.to_string()
                    };
                    draw_text(
                        fb,
                        &mut self.cache,
                        &format!("      {}", t),
                        32,
                        row_y + (k + 1) * line_px,
                        self.font_px,
                        CHROME_DIM,
                    );
                }
            }
        }
    }

    /// Recompute `palette_filtered` (indices into `palette_rows`) matching `palette_q` by
    /// case-insensitive substring on the label, then clamp the selection into range. Mirrors the
    /// other overlays' per-frame recompute.
    fn palette_refresh_filter(&mut self) {
        let q = self.palette_q.to_lowercase();
        self.palette_filtered = (0..self.palette_rows.len())
            .filter(|&i| q.is_empty() || self.palette_rows[i].0.to_lowercase().contains(&q))
            .collect();
        if self.palette_sel >= self.palette_filtered.len() {
            self.palette_sel = self.palette_filtered.len().saturating_sub(1);
        }
    }

    /// Render the command palette overlay: a filter prompt, then up to 12 matching action rows with
    /// the selected one highlighted. `palette_filtered` is recomputed each frame, mirroring the
    /// palette overlay.
    fn render_command_palette(&mut self, fb: &mut Framebuffer) {
        self.palette_refresh_filter();
        let (base_y, line_px) = self.overlay_base_y();
        let total = self.palette_rows.len();
        let shown = self.palette_filtered.len();
        draw_text(
            fb,
            &mut self.cache,
            &format!(
                "  ⚡ [action] {}  · {} of {}  ",
                self.palette_q, shown, total
            ),
            32,
            base_y,
            self.font_px,
            WHITE,
        );
        if shown == 0 {
            draw_text(
                fb,
                &mut self.cache,
                "  no actions match  ",
                32,
                base_y + line_px,
                self.font_px,
                CHROME_DIM,
            );
            return;
        }
        for (row, &i) in self.palette_filtered.iter().enumerate().take(12) {
            let sel = row == self.palette_sel;
            let color = if sel { WHITE } else { CHROME_DIM };
            let line = format!(
                "  {}  {}",
                self.palette_rows[i].0,
                if sel { "◄" } else { "" }
            );
            draw_text(
                fb,
                &mut self.cache,
                &line,
                32,
                base_y + (row + 1) * line_px,
                self.font_px,
                color,
            );
        }
    }

    /// Run a command-palette action: close the overlay first, then perform the same effect as the
    /// corresponding prefix command so terminal state and overlays are kept consistent.
    fn run_palette_action(&mut self, a: PaletteAction) {
        use PaletteAction::*;
        self.app.overlay = Overlay::None;
        match a {
            NewSession => {
                self.app.overlay = Overlay::NewSession;
                self.app.select_default_engine();
                // Pre-fill the last repo a local tab was spawned in (MRU), so respawning is one Enter.
                self.new_cwd = self.app.last_dirs.first().cloned().unwrap_or_default();
            }
            RemoteAttach => {
                self.app.overlay = Overlay::RemoteAttach;
                self.app.remote_host.clear();
                self.app.selected = 0;
            }
            SessionPalette => {
                self.app.overlay = Overlay::Palette;
                self.app.query.clear();
                self.app.selected = 0;
                self.app.refresh_filter();
            }
            FindInTab => {
                self.app.overlay = Overlay::Find;
                self.find_query.clear();
                self.find_hit = None;
                self.find_all = Vec::new();
            }
            SearchAll => {
                self.app.overlay = Overlay::FleetSearch;
                self.fleet_q.clear();
                self.fleet_matches.clear();
                self.fleet_sel = 0;
            }
            CopyScrollback => self.copy_whole_scrollback(),
            ExportLog => self.export_scrollback(),
            CopyIdentity => self.copy_identity(),
            CopyFleet => self.copy_fleet(),
            Broadcast => {
                // Pre-fill the last-sent line so a repeat command to every host survives; Backspace
                // clears it if a fresh one is wanted. Targets still reset all-on (safety).
                self.broadcast_query = self.last_broadcast.clone();
                self.broadcast_targets.iter_mut().for_each(|t| *t = true);
                self.broadcast_sel = 0;
                self.app.overlay = Overlay::Broadcast;
            }
            Peek => {
                self.app.overlay = Overlay::Peek;
                self.peek_sel = 0;
                self.peek_scroll = 0;
            }
            FleetGrid => {
                self.grid_sel = 0;
                self.app.overlay = Overlay::FleetGrid;
            }
            UndoClose => self.app.reopen_last_closed(),
            Duplicate => {
                self.duplicate_active_preserving_pin();
                self.flash = Some(("duplicated".to_string(), std::time::Instant::now()));
            }
            SessionInfo => {
                self.app.overlay = Overlay::Info;
            }
            ToggleFocus => self.toggle_focus(),
            Pin => self.toggle_pin_active(),
            NextPinned => self.next_pinned(),
            NextDown => self.next_down(),
            NextHost => self.next_host(),
            Dnd => self.toggle_dnd(),
            Reconnect => self.reconnect_active(),
            ReconnectAll => self.reconnect_all_down(),
            Destroy => self.destroy_active(),
            Interrupt => self.interrupt_active(),
            CloseQuiet => self.close_quiet_tabs(),
            Help => {
                self.app.overlay = Overlay::Help;
            }
            Quit => self.quit(),
        }
    }

    /// Handle a key while in copy mode: vim-style motion keys move the read cursor; `v` starts
    /// (or re-anchors) a selection; Enter/Space copies and exits; Esc/Q exits; g/G go to top/bottom.
    fn handle_copy_key(&mut self, key: &Key, _mods: &ModifiersState) {
        // While the search prompt is open, typing builds `copy_query` (live); Enter jumps to the
        // next match and exits the prompt; Esc/Backspace-empty dismisses it back to nav.
        if self.copy_searching {
            match key {
                Key::Character(c) if c == "/" => { /* ignore a second slash */ }
                Key::Character(c) => {
                    self.copy_query.push_str(c);
                    return;
                }
                Key::Named(n) => match n {
                    winit::keyboard::NamedKey::Backspace => {
                        self.copy_query.pop();
                        return;
                    }
                    winit::keyboard::NamedKey::Enter => {
                        self.copy_searching = false;
                        self.copy_goto(false);
                        return;
                    }
                    winit::keyboard::NamedKey::Escape => {
                        self.copy_query.clear();
                        self.copy_searching = false;
                        return;
                    }
                    _ => return,
                },
                _ => return,
            }
        }
        match key {
            Key::Character(c) => match c.as_str() {
                "/" => {
                    self.copy_query.clear();
                    self.copy_searching = true;
                }
                "n" => {
                    if !self.copy_query.is_empty() {
                        self.copy_goto(false);
                    }
                }
                "N" => {
                    if !self.copy_query.is_empty() {
                        self.copy_goto(true);
                    }
                }
                // vim motions
                "h" | "j" | "k" | "l" | "w" | "b" => {
                    let (dl, dc) = match c.as_str() {
                        "h" => (0, -1),
                        "j" => (1, 0),
                        "k" => (-1, 0),
                        "l" | " " => (0, 1),
                        "w" => {
                            self.copy_word(true);
                            (0, 0)
                        }
                        "b" => {
                            self.copy_word(false);
                            (0, 0)
                        }
                        _ => (0, 0),
                    };
                    self.copy_move(dl, dc);
                }
                "v" => {
                    // Start/re-anchor the selection at the read cursor.
                    self.copy_anchor = Some(self.copy_pos);
                }
                "g" => {
                    let Some(active) = self.app.active_session() else {
                        return;
                    };
                    let g = active.term.lock();
                    self.copy_pos = (g.grid().topmost_line().0, 0);
                }
                "G" => {
                    let Some(active) = self.app.active_session() else {
                        return;
                    };
                    let g = active.term.lock();
                    self.copy_pos = (g.grid().bottommost_line().0, 0);
                }
                "q" => {
                    self.copy_mode = false;
                    self.copy_anchor = None;
                    self.copy_query.clear();
                    self.copy_searching = false;
                }
                _ => {}
            },
            Key::Named(n) => match n {
                winit::keyboard::NamedKey::Enter => self.copy_mode_copy(),
                winit::keyboard::NamedKey::Space => self.copy_mode_copy(),
                winit::keyboard::NamedKey::ArrowUp => self.copy_move(-1, 0),
                winit::keyboard::NamedKey::ArrowDown => self.copy_move(1, 0),
                winit::keyboard::NamedKey::ArrowLeft => self.copy_move(0, -1),
                winit::keyboard::NamedKey::ArrowRight => self.copy_move(0, 1),
                winit::keyboard::NamedKey::PageUp => self.copy_move(-20, 0),
                winit::keyboard::NamedKey::PageDown => self.copy_move(20, 0),
                winit::keyboard::NamedKey::Escape => {
                    self.copy_mode = false;
                    self.copy_anchor = None;
                    self.copy_query.clear();
                    self.copy_searching = false;
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// Jump the copy cursor to the next/previous word boundary. `forward` moves right, otherwise
    /// left. Wraps selection extension implicitly because it just moves `copy_pos`.
    fn copy_word(&mut self, forward: bool) {
        let Some(active) = self.app.active_session() else {
            return;
        };
        let g = active.term.lock();
        let cols = g.columns();
        let (mut l, mut cur) = self.copy_pos;
        let max_line = g.grid().bottommost_line().0;
        // Work on the current line's text; map from copy_pos col to a byte index.
        use alacritty_terminal::index::{Column, Line};
        let line_text: String = g.grid()[Line(l)][Column(0)..Column(cols)]
            .iter()
            .map(|c| c.c)
            .collect();
        let ci = cur.min(line_text.len().saturating_sub(1));
        let bytes = line_text.as_bytes();
        let is_space_at = |i: usize| bytes.get(i).map_or(true, |b| b.is_ascii_whitespace());
        if forward {
            // Skip current word (or spaces), land on next word start.
            let mut i = ci;
            // If on a space, skip spaces first.
            while i < bytes.len() && is_space_at(i) {
                i += 1;
            }
            while i < bytes.len() && !is_space_at(i) {
                i += 1;
            }
            // If still within the line, land at the next non-space.
            let mut ni = i;
            while ni < bytes.len() && is_space_at(ni) {
                ni += 1;
            }
            if ni < bytes.len() {
                cur = ni;
            } else if l < max_line {
                l += 1;
                cur = 0;
            }
        } else {
            // Walk back over the current word, then any spaces, to the previous word's start.
            let mut i = ci;
            while i > 0 && !is_space_at(i.saturating_sub(1)) {
                i -= 1;
            }
            // i is now the start of the current word (or 0). Skip spaces before it.
            let mut si = i;
            while si > 0 && is_space_at(si.saturating_sub(1)) {
                si -= 1;
            }
            cur = if si < i { si } else { i };
        }
        self.copy_pos = (l, cur);
    }

    /// Handle a key. Mirrors the TUI's prefix→command→forward logic; prefix here is Ctrl+Space
    /// (tmux-like), so a plain space still types normally.
    fn handle_key(&mut self, key: &Key, mods: &ModifiersState) {
        let is_space = matches!(key, Key::Character(c) if c == " ")
            || matches!(key, Key::Named(winit::keyboard::NamedKey::Space));

        // Enter command mode: Ctrl+Space (or, while we haven't a real prefix, Ctrl+Space).
        if mods.control_key() && is_space && self.app.overlay == Overlay::None {
            self.prefix_down = true;
            return;
        }

        if self.prefix_down && self.app.overlay == Overlay::None {
            self.prefix_down = false;
            if self.command_key(key, mods) {
                self.quit();
            }
            return;
        }

        self.forward_key(key, mods);
    }

    /// Persist which tabs are muted (prefix+m) so a restart brings them back muted instead of the
    /// tab nagging again the moment the window reopens. Shared by `quit` and the close path.
    fn save_muted_state(&self) {
        let keys: Vec<(&str, &str, &str)> = self
            .app
            .tabs
            .iter()
            .enumerate()
            .filter(|(i, _)| self.muted.get(*i).copied().unwrap_or(false))
            .map(|(_, s)| (s.kind(), s.meta.host.as_str(), s.meta.engine.as_str()))
            .collect();
        crate::restore::save_muted(&keys);
    }

    /// Persist which tabs are pinned (prefix+a) so a restart keeps protecting them. Shared by
    /// `quit` and the close path (a pinned tab should survive a relaunch as pinned).
    fn save_pin_state(&self) {
        let keys: Vec<(&str, &str, &str)> = self
            .app
            .tabs
            .iter()
            .enumerate()
            .filter(|(i, _)| self.pinned.get(*i).copied().unwrap_or(false))
            .map(|(_, s)| (s.kind(), s.meta.host.as_str(), s.meta.engine.as_str()))
            .collect();
        crate::restore::save_pinned(&keys);
    }

    /// Apply the same save-then-exit dance as a window CloseRequested: persist open tabs, tab list,
    /// and geometry, then flag the loop to exit at the next `about_to_wait`.
    fn quit(&mut self) {
        self.app.save_all_scrollbacks();
        crate::restore::save(&self.app.tab_specs());
        self.save_muted_state();
        self.save_pin_state();
        crate::restore::save_geometry(self.size.width, self.size.height);
        self.quit_requested = true;
    }

    /// Command-mode (after prefix). Returns true to quit.
    ///
    /// The single key pressed after `Ctrl+Space` looks up `self.key_action` (key -> action name,
    /// resolved from config with the hardcoded defaults as fallback) and dispatches on the action
    /// name. Digit keys 1-9 and Tab are NOT part of the remappable table — they stay fixed, exactly
    /// as before, so a remap can never accidentally break tab switching.
    fn command_key(&mut self, key: &Key, mods: &ModifiersState) -> bool {
        match key {
            Key::Character(c) if c.len() == 1 && c.chars().next().unwrap().is_ascii_digit() => {
                // Numeric tabs: 1-9 jump to that tab, 0 jumps to the LAST tab (kept fixed outside
                // the keybinding table so a remap can't break tab switching).
                let idx = c.chars().next().unwrap() as u8;
                if (b'1'..=b'9').contains(&idx) {
                    let i = (idx - b'1') as usize;
                    if i < self.app.tabs.len() {
                        self.set_active(i);
                    }
                } else if idx == b'0' && !self.app.tabs.is_empty() {
                    self.set_active(self.app.tabs.len() - 1);
                }
            }
            Key::Character(c) => match self.key_action.get(c.as_str()).map(String::as_str) {
                Some("palette") => {
                    self.app.overlay = Overlay::Palette;
                    self.app.query.clear();
                    self.app.selected = 0;
                    self.app.refresh_filter();
                }
                Some("new_session") => {
                    self.app.overlay = Overlay::NewSession;
                    self.app.select_default_engine();
                    self.new_cwd = self.app.last_dirs.first().cloned().unwrap_or_default();
                }
                Some("remote_attach") => {
                    self.app.overlay = Overlay::RemoteAttach;
                    self.app.remote_host.clear();
                    self.app.selected = 0;
                }
                Some("local_shell") => self.app.spawn_tmux("this-host", "shell"),
                Some("quit") => return true,
                Some("fleet") => {
                    // Fleet overlay: fetch status on open so it's fresh, then show it. Filter starts
                    // empty so the full list is visible; typing narrows it live.
                    self.app.selected = 0;
                    self.fleet_query.clear();
                    self.fleet_filtered.clear();
                    if let Ok(st) = crate::harness::HarnessClient::local().status() {
                        self.app.fleet = st;
                    }
                    self.app.overlay = Overlay::Fleet;
                }
                Some("fleet_grid") => {
                    self.grid_sel = 0;
                    self.app.overlay = Overlay::FleetGrid;
                }
                Some("goto_tab0") => {
                    if !self.app.tabs.is_empty() {
                        self.set_active(0);
                    }
                }
                Some("next_busy") => self.next_busy(),
                Some("next_quiet") => self.next_quiet(),
                Some("next_down") => self.next_down(),
                Some("next_host") => self.next_host(),
                Some("dnd") => self.toggle_dnd(),
                Some("mute") => self.toggle_mute_active(),
                Some("interrupt") => self.interrupt_active(),
                Some("pin") => self.toggle_pin_active(),
                Some("next_pinned") => self.next_pinned(),
                Some("reconnect") => self.reconnect_active(),
                Some("reconnect_all") => self.reconnect_all_down(),
                Some("destroy") => self.destroy_active(),
                Some("last_window") => self.last_window(),
                Some("paste") => self.paste_clipboard(),
                Some("broadcast") => {
                    // Broadcast one line to every open session. Pre-fills the last-sent line (see
                    // the palette arm); targets reset all-on on each open — a prior run's
                    // deselections must NOT silently carry over, or a user who meant to exclude one
                    // host could re-fan to it on the next send.
                    self.broadcast_query = self.last_broadcast.clone();
                    self.broadcast_targets.iter_mut().for_each(|t| *t = true);
                    self.broadcast_sel = 0;
                    self.app.overlay = Overlay::Broadcast;
                }
                Some("close_quiet") => self.close_quiet_tabs(),
                Some("close_tab") => {
                    let pin = self.pinned.get(self.app.active).copied().unwrap_or(false);
                    if !close_tab(&mut self.app, pin) && pin {
                        self.flash = Some((
                            "🔒 pinned — prefix A to unpin first".to_string(),
                            std::time::Instant::now(),
                        ));
                    }
                    self.save_pin_state();
                }
                Some("copy_scrollback") => self.copy_whole_scrollback(),
                Some("export_scrollback") => self.export_scrollback(),
                Some("copy_identity") => self.copy_identity(),
                Some("copy_fleet") => self.copy_fleet(),
                Some("peek") => {
                    self.app.overlay = Overlay::Peek;
                    self.peek_sel = 0;
                    self.peek_scroll = 0;
                }
                Some("undo_close") => self.app.reopen_last_closed(),
                Some("duplicate") => {
                    self.duplicate_active_preserving_pin();
                    self.flash = Some(("duplicated".to_string(), std::time::Instant::now()));
                }
                Some("page_up") => {
                    scroll_active(self, 20);
                    if let Some(s) = self.app.active_session() {
                        s.set_scrolled(true);
                    }
                }
                Some("scroll_bottom") => self.scroll_to_bottom(),
                Some("search") => {
                    self.app.overlay = Overlay::Find;
                    self.find_query.clear();
                    self.find_hit = None;
                    self.find_all = Vec::new();
                }
                Some("search_all") => {
                    self.app.overlay = Overlay::FleetSearch;
                    self.fleet_q.clear();
                    self.fleet_matches.clear();
                    self.fleet_sel = 0;
                }
                Some("move_left") => self.app.move_tab(-1),
                Some("move_right") => self.app.move_tab(1),
                Some("copy_mode") => self.start_copy_mode(),
                Some("help") => {
                    self.app.overlay = Overlay::Help;
                }
                Some("command_palette") => {
                    self.palette_q.clear();
                    self.palette_sel = 0;
                    self.palette_rows = PaletteAction::all_rows();
                    self.app.overlay = Overlay::CommandPalette;
                }
                Some("session_info") => {
                    self.app.overlay = Overlay::Info;
                }
                Some("toggle_focus") => self.toggle_focus(),
                Some("rename") => {
                    // Rename the active tab. Pre-fill with the current custom name (if any) so
                    // editing doesn't start from scratch.
                    self.rename_query = self
                        .app
                        .active_session()
                        .map(|s| s.meta.name.clone().unwrap_or_default())
                        .unwrap_or_default();
                    self.app.overlay = Overlay::Rename;
                }
                _ => {}
            },
            Key::Named(n) => match n {
                winit::keyboard::NamedKey::Tab => {
                    if !self.app.tabs.is_empty() {
                        let n = self.app.tabs.len();
                        // Shift+Tab cycles backward (wrapping) through tabs; plain Tab goes forward.
                        if mods.shift_key() {
                            self.set_active((self.app.active + n - 1) % n);
                        } else {
                            self.set_active((self.app.active + 1) % n);
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
        false
    }

    /// Normal-typing + overlay navigation.
    fn forward_key(&mut self, key: &Key, mods: &ModifiersState) {
        match self.app.overlay {
            Overlay::Palette => {
                match key {
                    Key::Character(c) => {
                        self.app.query.push_str(c);
                        self.app.refresh_filter();
                    }
                    Key::Named(n) => match n {
                        winit::keyboard::NamedKey::Enter => self.app.jump_to_selection(),
                        winit::keyboard::NamedKey::Escape => self.app.overlay = Overlay::None,
                        winit::keyboard::NamedKey::ArrowDown => {
                            self.app.selected = self
                                .app
                                .selected
                                .saturating_add(1)
                                .min(self.app.filtered.len().saturating_sub(1))
                        }
                        winit::keyboard::NamedKey::ArrowUp => {
                            self.app.selected = self.app.selected.saturating_sub(1)
                        }
                        winit::keyboard::NamedKey::Backspace => {
                            self.app.query.pop();
                            self.app.refresh_filter();
                        }
                        _ => {}
                    },
                    _ => {}
                }
                return;
            }
            Overlay::NewSession => {
                match key {
                    // Typing builds the per-tab working-directory field; Up/Down still select the
                    // engine; Backspace edits the directory.
                    Key::Character(c) => self.new_cwd.push_str(c),
                    Key::Named(n) => match n {
                        winit::keyboard::NamedKey::Enter => {
                            if let Some(e) = self.app.selected_engine() {
                                let cwd = if self.new_cwd.trim().is_empty() {
                                    None
                                } else {
                                    Some(self.new_cwd.trim().to_string())
                                };
                                self.app.spawn_local("this-host", e, cwd);
                                self.app.overlay = Overlay::None;
                            }
                        }
                        winit::keyboard::NamedKey::Escape => self.app.overlay = Overlay::None,
                        winit::keyboard::NamedKey::ArrowDown => {
                            self.app.selected = (self.app.selected + 1).min(ENGINES.len() - 1)
                        }
                        winit::keyboard::NamedKey::ArrowUp => {
                            self.app.selected = self.app.selected.saturating_sub(1)
                        }
                        winit::keyboard::NamedKey::Backspace => {
                            self.new_cwd.pop();
                        }
                        _ => {}
                    },
                    _ => {}
                }
                return;
            }
            Overlay::RemoteAttach => {
                match key {
                    Key::Character(c) => self.app.remote_host.push_str(c),
                    Key::Named(n) => match n {
                        winit::keyboard::NamedKey::Enter => {
                            if let Some(e) = self.app.selected_engine() {
                                let raw = self.app.remote_host.trim();
                                let (host, port) = if raw.is_empty() {
                                    (
                                        "127.0.0.1".to_string(),
                                        crate::harness::HARNESS_PORT_DEFAULT,
                                    )
                                } else if let Some((h, p)) = raw.split_once(':') {
                                    (
                                        h.to_string(),
                                        p.parse().unwrap_or(crate::harness::HARNESS_PORT_DEFAULT),
                                    )
                                } else {
                                    (raw.to_string(), crate::harness::HARNESS_PORT_DEFAULT)
                                };
                                self.app.spawn_tunnel(&host, port, e);
                                self.app.overlay = Overlay::None;
                            }
                        }
                        winit::keyboard::NamedKey::Escape => self.app.overlay = Overlay::None,
                        winit::keyboard::NamedKey::ArrowDown => {
                            self.app.selected = (self.app.selected + 1).min(ENGINES.len() - 1)
                        }
                        winit::keyboard::NamedKey::ArrowUp => {
                            self.app.selected = self.app.selected.saturating_sub(1)
                        }
                        winit::keyboard::NamedKey::Backspace => {
                            self.app.remote_host.pop();
                        }
                        _ => {}
                    },
                    _ => {}
                }
                return;
            }
            Overlay::Find => {
                match key {
                    Key::Character(c) => {
                        self.find_query.push_str(c);
                        self.find_recompute(None);
                    }
                    Key::Named(n) => match n {
                        winit::keyboard::NamedKey::Enter if mods.shift_key() => {
                            self.find_jump(-1);
                        }
                        winit::keyboard::NamedKey::Enter | winit::keyboard::NamedKey::Tab => {
                            self.find_jump(1);
                        }
                        winit::keyboard::NamedKey::ArrowDown => {
                            self.find_jump(1);
                        }
                        winit::keyboard::NamedKey::ArrowUp => {
                            self.find_jump(-1);
                        }
                        winit::keyboard::NamedKey::Backspace => {
                            self.find_query.pop();
                            self.find_recompute(None);
                        }
                        winit::keyboard::NamedKey::Escape => {
                            self.app.overlay = Overlay::None;
                        }
                        _ => {}
                    },
                    _ => {}
                }
                return;
            }
            Overlay::FleetSearch => {
                match key {
                    Key::Character(c) => {
                        self.fleet_q.push_str(c);
                        self.fleet_recompute();
                    }
                    Key::Named(n) => match n {
                        // Enter closes and jumps to the selected match across tables.
                        winit::keyboard::NamedKey::Enter => self.fleet_jump_to(),
                        // Tab moves the selection down (Shift+Tab up), wrapping.
                        winit::keyboard::NamedKey::Tab if mods.shift_key() => {
                            self.fleet_jump(-1);
                        }
                        winit::keyboard::NamedKey::Tab => {
                            self.fleet_jump(1);
                        }
                        winit::keyboard::NamedKey::ArrowDown => {
                            self.fleet_jump(1);
                        }
                        winit::keyboard::NamedKey::ArrowUp => {
                            self.fleet_jump(-1);
                        }
                        winit::keyboard::NamedKey::Backspace => {
                            self.fleet_q.pop();
                            self.fleet_recompute();
                        }
                        winit::keyboard::NamedKey::Escape => {
                            self.app.overlay = Overlay::None;
                        }
                        _ => {}
                    },
                    _ => {}
                }
                return;
            }
            Overlay::Fleet => {
                match key {
                    Key::Named(n) => match n {
                        // Up/Down move the highlighted row; Enter attaches to it (jump to an open
                        // tab for that engine, else open a fresh local tmux pane). Esc dismisses.
                        winit::keyboard::NamedKey::Escape => {
                            self.app.overlay = Overlay::None;
                        }
                        winit::keyboard::NamedKey::ArrowDown => {
                            if !self.fleet_filtered.is_empty() {
                                self.app.selected =
                                    (self.app.selected + 1).min(self.fleet_filtered.len() - 1);
                            }
                        }
                        winit::keyboard::NamedKey::ArrowUp => {
                            self.app.selected = self.app.selected.saturating_sub(1);
                        }
                        winit::keyboard::NamedKey::Enter => {
                            self.fleet_attach_selected();
                        }
                        // Backspace removes the last filter character (re-filter live).
                        winit::keyboard::NamedKey::Backspace => {
                            self.fleet_query.pop();
                            self.app.selected = 0;
                            self.fleet_refresh_filter();
                        }
                        _ => {}
                    },
                    // `s` re-fetches for a fresh view; any other character filters the list.
                    Key::Character(c) if c == "s" && self.fleet_query.is_empty() => {
                        if let Ok(st) = crate::harness::HarnessClient::local().status() {
                            self.app.fleet = st;
                        }
                    }
                    Key::Character(c) => {
                        self.fleet_query.push_str(c);
                        self.app.selected = 0;
                        self.fleet_refresh_filter();
                    }
                    _ => {}
                }
                return;
            }
            Overlay::Help => {
                self.app.overlay = Overlay::None;
                return;
            }
            Overlay::Info => {
                self.app.overlay = Overlay::None;
                return;
            }
            Overlay::Rename => {
                match key {
                    Key::Character(c) => {
                        self.rename_query.push_str(c);
                    }
                    Key::Named(n) => match n {
                        winit::keyboard::NamedKey::Enter => {
                            // Commit the rename (empty = clear back to the default engine label).
                            let name = if self.rename_query.trim().is_empty() {
                                None
                            } else {
                                Some(self.rename_query.trim().to_string())
                            };
                            if let Some(s) = self.app.active_session_mut() {
                                s.meta.name = name;
                            }
                            self.app.overlay = Overlay::None;
                        }
                        winit::keyboard::NamedKey::Backspace => {
                            self.rename_query.pop();
                        }
                        winit::keyboard::NamedKey::Escape => {
                            self.app.overlay = Overlay::None;
                        }
                        _ => {}
                    },
                    _ => {}
                }
                return;
            }
            Overlay::Broadcast => {
                // Keep the target list sized to the current tab set (a tab may have opened/closed).
                if self.broadcast_targets.len() != self.app.tabs.len() {
                    self.broadcast_targets.resize(self.app.tabs.len(), true);
                    self.broadcast_sel = self
                        .broadcast_sel
                        .min(self.app.tabs.len().saturating_sub(1));
                }
                match key {
                    Key::Character(c) => {
                        if c == " " {
                            // Space toggles the focused session's target.
                            if let Some(on) = self.broadcast_targets.get_mut(self.broadcast_sel) {
                                *on = !*on;
                            }
                        } else {
                            self.broadcast_query.push_str(c);
                        }
                        // Editing a fresh line leaves history recall.
                        self.hist_sel = None;
                    }
                    Key::Named(n) => match n {
                        winit::keyboard::NamedKey::Enter => {
                            // Fan the line out to the MARKED sessions only, then close. Unchecked
                            // sessions are left untouched — the whole point of targeting.
                            let bytes = broadcast_bytes(&self.broadcast_query);
                            if !bytes.is_empty() {
                                for (i, s) in self.app.tabs.iter().enumerate() {
                                    if self.broadcast_targets.get(i).copied().unwrap_or(false) {
                                        s.write(&bytes);
                                    }
                                }
                                // Remember what we sent so the next broadcast pre-fills it (a repeat
                                // command to every host doesn't need retyping).
                                self.last_broadcast = self.broadcast_query.clone();
                                crate::restore::save_last_broadcast(&self.last_broadcast);
                                // Push it onto the MRU history (dedup + front + cap 16).
                                self.broadcast_hist.retain(|h| h != &self.broadcast_query);
                                self.broadcast_hist.insert(0, self.broadcast_query.clone());
                                self.broadcast_hist.truncate(16);
                                crate::restore::save_broadcast_history(&self.broadcast_hist);
                            }
                            self.broadcast_query.clear();
                            self.hist_sel = None;
                            self.app.overlay = Overlay::None;
                        }
                        winit::keyboard::NamedKey::ArrowDown => {
                            if mods.shift_key() {
                                // Shift+Down recalls newer history.
                                self.hist_down();
                            } else {
                                self.broadcast_sel = (self.broadcast_sel + 1)
                                    .min(self.app.tabs.len().saturating_sub(1));
                            }
                        }
                        winit::keyboard::NamedKey::ArrowUp => {
                            if mods.shift_key() {
                                // Shift+Up recalls older history.
                                self.hist_up();
                            } else {
                                self.broadcast_sel = self.broadcast_sel.saturating_sub(1);
                            }
                        }
                        winit::keyboard::NamedKey::Backspace => {
                            self.broadcast_query.pop();
                            self.hist_sel = None;
                        }
                        winit::keyboard::NamedKey::Escape => {
                            self.broadcast_query.clear();
                            self.app.overlay = Overlay::None;
                        }
                        _ => {}
                    },
                    _ => {}
                }
                return;
            }
            Overlay::Peek => {
                match key {
                    // A picker, not a prompt: typing does nothing.
                    Key::Character(_) => {}
                    Key::Named(n) => match n {
                        winit::keyboard::NamedKey::ArrowDown | winit::keyboard::NamedKey::Tab
                            if !mods.shift_key() =>
                        {
                            self.peek_sel =
                                (self.peek_sel + 1).min(self.app.tabs.len().saturating_sub(1));
                            // Keep the selection in the visible window as it walks past the bottom.
                            if self.peek_sel >= self.peek_scroll + 10 {
                                self.peek_scroll = self.peek_sel + 1 - 10;
                            }
                        }
                        winit::keyboard::NamedKey::Tab => {
                            self.peek_sel = self.peek_sel.saturating_sub(1);
                            // Pull the window up when the selection walks above the top.
                            if self.peek_sel < self.peek_scroll {
                                self.peek_scroll = self.peek_sel;
                            }
                        }
                        winit::keyboard::NamedKey::ArrowUp => {
                            self.peek_sel = self.peek_sel.saturating_sub(1);
                            if self.peek_sel < self.peek_scroll {
                                self.peek_scroll = self.peek_sel;
                            }
                        }
                        winit::keyboard::NamedKey::Enter => {
                            if !self.app.tabs.is_empty() {
                                self.app.active = self.peek_sel;
                                crate::restore::save_active(self.app.active);
                                self.app.overlay = Overlay::None;
                            }
                        }
                        winit::keyboard::NamedKey::Escape => {
                            self.app.overlay = Overlay::None;
                        }
                        _ => {}
                    },
                    _ => {}
                }
                return;
            }
            Overlay::FleetGrid => {
                match key {
                    Key::Named(n) => match n {
                        // A re-computed layout isn't needed per keypress: the first three/down/right
                        // edges are enough to clamp the selection without a full re-layout.
                        winit::keyboard::NamedKey::ArrowDown | winit::keyboard::NamedKey::Tab
                            if !mods.shift_key() =>
                        {
                            self.grid_sel =
                                (self.grid_sel + 1).min(self.app.tabs.len().saturating_sub(1));
                        }
                        winit::keyboard::NamedKey::Tab => {
                            self.grid_sel = self.grid_sel.saturating_sub(1);
                        }
                        winit::keyboard::NamedKey::ArrowUp => {
                            self.grid_sel = self.grid_sel.saturating_sub(1);
                        }
                        winit::keyboard::NamedKey::Enter => {
                            if !self.app.tabs.is_empty() {
                                self.app.active = self.grid_sel.min(self.app.tabs.len() - 1);
                                crate::restore::save_active(self.app.active);
                                self.app.overlay = Overlay::None;
                            }
                        }
                        winit::keyboard::NamedKey::Escape => {
                            self.app.overlay = Overlay::None;
                        }
                        _ => {}
                    },
                    // 1-9 jump straight to a tile by session number; a character that maps to a
                    // session index (1..=9) does the same. Everything else is ignored — it's a
                    // viewer, not a prompt.
                    Key::Character(c) => {
                        if let Some(d) = c.chars().next().and_then(|ch| ch.to_digit(10)) {
                            if d >= 1 && d <= 9 {
                                let i = (d - 1) as usize;
                                if i < self.app.tabs.len() {
                                    self.grid_sel = i;
                                }
                            }
                        }
                    }
                    _ => {}
                }
                return;
            }
            Overlay::CommandPalette => {
                match key {
                    Key::Character(c) => {
                        self.palette_q.push_str(c);
                        self.palette_refresh_filter();
                    }
                    Key::Named(n) => match n {
                        winit::keyboard::NamedKey::Enter => {
                            // Resolve the selected row to its action and run it (closes the overlay).
                            if let Some(&i) = self.palette_filtered.get(self.palette_sel) {
                                let a = self.palette_rows[i].1;
                                self.run_palette_action(a);
                            }
                        }
                        // Tab moves down, Shift+Tab up (wraps); arrows move within the filtered list.
                        winit::keyboard::NamedKey::Tab if mods.shift_key() => {
                            if self.palette_filtered.is_empty() {
                                self.palette_sel = 0;
                            } else {
                                self.palette_sel = self.palette_sel.saturating_sub(1);
                            }
                        }
                        winit::keyboard::NamedKey::Tab => {
                            let len = self.palette_filtered.len();
                            if len > 0 {
                                self.palette_sel = (self.palette_sel + 1).min(len - 1);
                            }
                        }
                        winit::keyboard::NamedKey::ArrowDown => {
                            let len = self.palette_filtered.len();
                            if len > 0 {
                                self.palette_sel = (self.palette_sel + 1).min(len - 1);
                            }
                        }
                        winit::keyboard::NamedKey::ArrowUp => {
                            self.palette_sel = self.palette_sel.saturating_sub(1);
                        }
                        winit::keyboard::NamedKey::Backspace => {
                            self.palette_q.pop();
                            self.palette_refresh_filter();
                        }
                        winit::keyboard::NamedKey::Escape => {
                            self.palette_q.clear();
                            self.app.overlay = Overlay::None;
                        }
                        _ => {}
                    },
                    _ => {}
                }
                return;
            }
            Overlay::None => {}
        }

        // Normal mode: send keystrokes to the active session.
        if self.app.overlay == Overlay::None {
            // Font zoom (Ctrl+= / Ctrl+- / Ctrl+0 to reset) — captured before anything reaches the
            // shell, like any terminal's, and a persistent per-window preference.
            if mods.control_key() {
                if let Key::Character(c) = key {
                    match c.as_str() {
                        "=" | "+" => {
                            self.zoom_font(1.1);
                            return;
                        }
                        "-" => {
                            self.zoom_font(1.0 / 1.1);
                            return;
                        }
                        "0" => {
                            self.zoom = 1.0;
                            crate::restore::save_zoom(1.0);
                            self.metrics_from_scale();
                            return;
                        }
                        _ => {}
                    }
                }
            }
            // Fullscreen toggle (Ctrl+Enter). A fleet diver drops a busy pane fullscreen to watch it
            // without the OS chrome; pressing again returns to windowed. Window-size events reflow
            // the grid automatically.
            if mods.control_key() && matches!(key, Key::Named(winit::keyboard::NamedKey::Enter)) {
                if let Some(w) = &self.window {
                    let fs = w.fullscreen().is_some();
                    w.set_fullscreen(if fs {
                        None
                    } else {
                        Some(winit::window::Fullscreen::Borderless(None))
                    });
                    self.flash = Some((
                        if fs { "windowed" } else { "fullscreen" }.to_string(),
                        std::time::Instant::now(),
                    ));
                }
                return;
            }
            // Copy mode intercepts keystrokes (navigation + selection) instead of forwarding.
            if self.copy_mode {
                self.handle_copy_key(key, mods);
                return;
            }
            // Scrollback navigation takes precedence over forwarding to the shell. While scrolled,
            // page/arrow keys move the viewport; Esc returns to the live (bottom) view. PageUp from
            // the live view also enters scroll mode.
            let scrolled_now = self
                .app
                .active_session()
                .map(|s| s.scrolled())
                .unwrap_or(false);
            if scrolled_now || matches!(key, Key::Named(winit::keyboard::NamedKey::PageUp)) {
                match key {
                    Key::Named(n) => match n {
                        winit::keyboard::NamedKey::PageUp => {
                            scroll_active(self, 20);
                            if let Some(s) = self.app.active_session() {
                                s.set_scrolled(true);
                            }
                        }
                        winit::keyboard::NamedKey::PageDown => scroll_active(self, -20),
                        winit::keyboard::NamedKey::ArrowUp => {
                            scroll_active(self, 1);
                            if let Some(s) = self.app.active_session() {
                                s.set_scrolled(true);
                            }
                        }
                        winit::keyboard::NamedKey::ArrowDown => scroll_active(self, -1),
                        winit::keyboard::NamedKey::Escape => self.scroll_to_bottom(),
                        _ => {}
                    },
                    _ => {}
                }
                // Snap scrolled state to reality once we hit bottom (offset no longer moves).
                if let Some(active) = self.app.active_session() {
                    if active.scrolled() {
                        let g = active.term.lock();
                        if g.grid().display_offset() == 0 {
                            active.set_scrolled(false);
                        }
                    }
                }
                return;
            }

            if let Some(s) = self.app.active_session_mut() {
                // Copy selection: Cmd+C (mac convention) copies the current text selection to the
                // system clipboard and clears the highlight. Ctrl+C still goes to the session as
                // the interrupt byte unless a selection exists on mac.
                let is_copy = mods.super_key() && matches!(key, Key::Character(c) if c == "c");
                if is_copy {
                    // Copy without holding the session borrow: read the text, clear it, then store.
                    let (text, selected) = {
                        let g = s.term.lock();
                        (g.selection_to_string(), g.selection.is_some())
                    };
                    if selected {
                        if let Some(t) = text {
                            if !t.is_empty() {
                                if let Ok(mut cb) = arboard::Clipboard::new() {
                                    let _ = cb.set_text(t);
                                }
                            }
                        }
                        s.term.lock().selection = None;
                    }
                    return;
                }
                // Paste clipboard: Ctrl+V (and Cmd+V, the mac convention) reads the system clipboard
                // and writes it to the session, bracketing with bracketed-paste if the app asked.
                let is_paste = (mods.control_key() && matches!(key, Key::Character(c) if c == "v"))
                    || (mods.super_key() && matches!(key, Key::Character(c) if c == "v"));
                if is_paste {
                    if let Ok(mut cb) = arboard::Clipboard::new() {
                        if let Ok(text) = cb.get_text() {
                            // Bracketed paste is negotiated; we always bracket to be safe with
                            // multi-line input (most modern shells/clis accept it).
                            let seq = format!("\x1b[200~{}\x1b[201~", text);
                            s.write(seq.as_bytes());
                        }
                    }
                    return;
                }
                match key {
                    Key::Character(c) => {
                        if mods.control_key() {
                            if let Some(b) = ctrl_byte(c) {
                                s.write(&[b]);
                            }
                        } else {
                            s.write(c.as_bytes());
                        }
                    }
                    Key::Named(n) => match n {
                        winit::keyboard::NamedKey::Enter => s.write(b"\r"),
                        winit::keyboard::NamedKey::Backspace => s.write(b"\x7f"),
                        winit::keyboard::NamedKey::Tab => s.write(b"\t"),
                        winit::keyboard::NamedKey::Escape => s.write(b"\x1b"),
                        winit::keyboard::NamedKey::ArrowUp => s.write(b"\x1b[A"),
                        winit::keyboard::NamedKey::ArrowDown => s.write(b"\x1b[B"),
                        winit::keyboard::NamedKey::ArrowRight => s.write(b"\x1b[C"),
                        winit::keyboard::NamedKey::ArrowLeft => s.write(b"\x1b[D"),
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
    }
}

/// The bytes sent to every session when a broadcast line is committed: the query plus a trailing
/// newline. A blank query sends nothing (never broadcast a bare newline). Shared free function so
/// the fan-out formatting is unit-testable without building real `Session`s.
/// Format a monotonic duration in the most readable compact unit: under a minute as `%ds`,
/// otherwise `%dm`, `%dh`, or `%dd`. Used for a session's quiet ("idle") age — the readable
/// inverse of "produced output just now".
fn fmt_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

fn broadcast_bytes(q: &str) -> Vec<u8> {
    if q.is_empty() {
        Vec::new()
    } else {
        format!("{}\n", q).into_bytes()
    }
}

/// Next broadcast-history slot when walking `delta` (negative = older). Starting cold (`None`),
/// Shift+Up grabs the newest entry; Shift+Down wraps to the oldest. Wraps around the history list.
fn recall_index(n: usize, delta: isize, cur: Option<usize>) -> usize {
    match cur {
        Some(i) => (i as isize + delta).rem_euclid(n as isize) as usize,
        None => {
            if delta < 0 {
                n - 1
            } else {
                0
            }
        }
    }
}

/// Collect every (tab, line, col) match of the lowercase query across ALL sessions' scrollbacks,
/// sorted by tab then line. Used by fleet search. Shared free function so the cross-tab + sort
/// behavior is unit-testable without building real `Session`s (tests pass raw lockable Terms).
fn collect_fleet_matches(
    terms: &[Arc<
        alacritty_terminal::sync::FairMutex<
            alacritty_terminal::term::Term<crate::session::Listener>,
        >,
    >],
    query_lower: &str,
) -> Vec<FleetMatch> {
    let mut out = Vec::new();
    for (tab, term) in terms.iter().enumerate() {
        let g = term.lock();
        // Reuse the public `all_matches` per-tab, tagging each hit with its tab index.
        for (line, col, _) in crate::render::all_matches(&g, query_lower) {
            out.push(FleetMatch { tab, line, col });
        }
    }
    out.sort_by(|a, b| (a.tab, a.line, a.col).cmp(&(b.tab, b.line, b.col)));
    out
}

/// Close the active tab (`x` / prefix+close_tab), unless it's pinned. A pinned tab (prefix+a)
/// refuses the close with a flash and keeps itself in the bar, so a long-running agent can't be
/// fat-fingered away — you must unpin first.
pub(crate) fn close_tab(app: &mut App, pinned: bool) -> bool {
    if app.tabs.is_empty() {
        return false;
    }
    if app.active < app.tabs.len() && pinned {
        return false;
    }
    // Stash the closed tab's spec so prefix+u can undo a mistaken close. Kind + host + engine
    // are enough to re-spawn the same identity (TMUX/etc. re-attach to the same pane@host).
    if let Some(s) = app.tabs.get(app.active) {
        app.last_closed = Some(crate::restore::TabSpec {
            kind: s.kind().to_string(),
            host: s.meta.host.clone(),
            engine: s.meta.engine.clone(),
            port: None,
            name: s.meta.name.clone(),
        });
    }
    app.tabs.remove(app.active);
    if app.active >= app.tabs.len() {
        app.active = app.tabs.len().saturating_sub(1);
    }
    crate::restore::save(&app.tab_specs());
    true
}

/// Scroll the active session's viewport by `delta` lines into history (positive = up/back).
/// Marks the view as user-scrolled so render doesn't snap us back to the live line.
fn scroll_active(app: &Application, delta: i32) {
    use alacritty_terminal::grid::Scroll;
    if let Some(active) = app.app.active_session() {
        let mut g = active.term.lock();
        g.grid_mut().scroll_display(Scroll::Delta(delta));
    }
}

impl Application {
    /// Move the broadcast overlay's recall cursor by `delta` slots (`-1` older, `+1` newer) and set
    /// the query to the recalled line. Wraps around the history; a fresh (unsaved) line sits above the
    /// newest entry and is yielded back to when stepping past the top. No-op with an empty history.
    /// Walk broadcast history with Shift+Up (older).
    fn hist_up(&mut self) {
        self.recall_broadcast(-1)
    }

    /// Walk broadcast history with Shift+Down (newer).
    fn hist_down(&mut self) {
        self.recall_broadcast(1)
    }

    fn recall_broadcast(&mut self, delta: isize) {
        let n = self.broadcast_hist.len();
        if n == 0 {
            return;
        }
        self.hist_sel = Some(recall_index(n, delta, self.hist_sel));
        self.broadcast_query = self.broadcast_hist[self.hist_sel.unwrap()].clone();
    }

    /// Return to the latest (live) view: clear the display offset and end scroll mode.
    fn scroll_to_bottom(&mut self) {
        use alacritty_terminal::grid::Scroll;
        if let Some(active) = self.app.active_session() {
            let mut g = active.term.lock();
            g.grid_mut().scroll_display(Scroll::Bottom);
            active.set_scrolled(false);
        }
    }

    /// Map a framebuffer pixel position to the terminal-cell it lands on (viewing row 0 = the
    /// visually-top line, which with scrollback is history). Returns None if the point is outside
    /// the grid area (tab/status chrome or the right/left gutter).
    fn mouse_to_cell(&self, x: f64, y: f64) -> Option<Point> {
        let x = x as i64;
        let y = y as i64;
        // Grid area origin: below the tab bar (cell_h), above the status bar.
        let top = self.cell_h as i64;
        let bottom = self.size.height as i64 - self.cell_h as i64;
        if y < top || y >= bottom || x < 0 {
            return None;
        }
        let row = ((y - top) / self.cell_h as i64) as i64;
        let col = (x / self.cell_w as i64) as i64;
        if row < 0 || col < 0 {
            return None;
        }
        Some(Point::new(
            Line((row as usize).try_into().unwrap()),
            Column(col as usize),
        ))
    }

    /// Which tab the pointer is over in the tab bar (if any). Mirrors the render loop's label x
    /// positions so the preview tooltip lines up with the painted labels: starts at x=6, each label
    /// advances by its drawn width + 12, and the bar stops at `width - 20` the same way. Only the
    /// top chrome row counts (y within the tab bar and below it); focus mode has no bar.
    fn tab_at(&mut self, x: f64, y: f64) -> Option<usize> {
        if self.focus {
            return None;
        }
        // The tab bar occupies the first cell row, vertically centered in it.
        if y < 0.0 || y >= self.cell_h as f64 || x < 6.0 {
            return None;
        }
        let mut cx = 6i64;
        for (i, s) in self.app.tabs.iter().enumerate() {
            // A stable, cheap label (dot + tail of live title + head + status dot) for hit-testing;
            // same font as the painted bar so the hover zone tracks the visible tab. Busy/bell badges
            // shift a tab's painted edge by a few glyphs, which barely moves where the preview pops,
            // so they're omitted here for speed and stability.
            let live = s.live_title().unwrap_or_else(|| s.meta.title.clone());
            let mut live = live.replace('\n', " ");
            if live.chars().count() > 18 {
                live = live.chars().take(18).collect::<String>() + "…";
            }
            let head = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
            let label = format!(" ○ {} {} ○ ", head, live);
            // Measure the label width with the glyph cache (same font advance as render).
            let mut w = 0i64;
            for ch in label.chars() {
                w += self.cache.glyph(ch, self.font_px, false).0 as i64;
            }
            if x >= cx as f64 && x < (cx + w + 12) as f64 {
                return Some(i);
            }
            cx += w + 12;
            if cx > (self.size.width as i64).saturating_sub(20) {
                break;
            }
        }
        None
    }

    /// Does a URL begin/overlap the cell at framebuffer (x, y)? Used to show the hand cursor over
    /// links, mirroring exactly what `mouse_open` would open so the affordance never lies. Cheap: no
    /// grid write, no selection — just a read of the row under the pointer.
    fn cell_has_link(&self, x: f64, y: f64) -> bool {
        let Some(pt) = self.mouse_to_cell(x, y) else {
            return false;
        };
        if self
            .app
            .active_session()
            .map(|s| s.scrolled())
            .unwrap_or(false)
            || pt.line.0 < 0
        {
            return false;
        }
        let Some(active) = self.app.active_session() else {
            return false;
        };
        let g = active.term.lock();
        let row = pt.line.0 as usize;
        let cols = g.columns();
        if row >= g.screen_lines() || cols == 0 {
            return false;
        }
        let col = (pt.column.0 as usize).min(cols - 1);
        let line_text: String = g.grid()[Line(row as i32)][Column(0)..Column(cols)]
            .iter()
            .map(|c| c.c)
            .collect();
        drop(g);
        crate::links::url_span(&line_text, col)
            .map(|s| !s.as_str(&line_text).is_empty())
            .unwrap_or(false)
    }

    /// Detect how many clicks this press represents. A press within ~250ms and ~4px of the previous
    /// one increments the count (up to 3); anything else resets to 1.
    fn click_count(&mut self, x: f64, y: f64) -> u32 {
        const THRESHOLD_MS: u64 = 250;
        const THRESHOLD_PX: f64 = 4.0;
        let now = std::time::Instant::now();
        let click = if let Some((prev_at, (px, py), prev_count)) = self.last_press {
            let dt = now.duration_since(prev_at).as_millis() as u64;
            let dist = ((px - x).powi(2) + (py - y).powi(2)).sqrt();
            if dt < THRESHOLD_MS && dist < THRESHOLD_PX {
                (prev_count + 1).min(3)
            } else {
                1
            }
        } else {
            1
        };
        self.last_press = Some((now, (x, y), click));
        click
    }

    /// Start a text selection at a pressed cursor position, or clear the selection when clicking.
    /// Single click = simple drag; double click = expand to a word (semantic); triple = whole line.
    /// Alt+click: reposition the shell/readline cursor at the clicked cell. We report the cursor
    /// position with the standard "active position report" sequence (`ESC [ row ; col R`), which
    /// line editors that support click-to-move (zsh, fish, and readline via inputrc) honor.
    fn mouse_alt_click(&mut self, x: f64, y: f64) {
        let Some(pt) = self.mouse_to_cell(x, y) else {
            return;
        };
        // Only meaningful against live (unscrolled) screen coordinates; report 1-based.
        if self
            .app
            .active_session()
            .map(|s| s.scrolled())
            .unwrap_or(false)
            || pt.line.0 < 0
        {
            return;
        }
        let seq = format!("\x1b[{};{}R", pt.line.0 + 1, pt.column.0 + 1);
        if let Some(active) = self.app.active_session() {
            active.write(seq.as_bytes());
        }
    }

    /// Cmd/Ctrl+click: jump to whatever is under the cursor. A URL opens in the default browser; a
    /// relative path opens in a text editor via `open` (macOS). Reads the word containing the
    /// clicked cell by expanding to the nearest whitespace/bracket boundaries on the grid row.
    /// Best-effort — a click on non-text just does nothing.
    fn mouse_open(&mut self, x: f64, y: f64) {
        let Some(pt) = self.mouse_to_cell(x, y) else {
            return;
        };
        if self
            .app
            .active_session()
            .map(|s| s.scrolled())
            .unwrap_or(false)
            || pt.line.0 < 0
        {
            return;
        }
        let Some(active) = self.app.active_session() else {
            return;
        };
        let g = active.term.lock();
        let row = pt.line.0 as usize;
        let cols = g.columns();
        if row >= g.screen_lines() {
            return;
        }
        let col = (pt.column.0 as usize).min(cols - 1);
        // Read the whole visible row and expand left/right from the click to word boundaries.
        let line_text: String = g.grid()[Line(row as i32)][Column(0)..Column(cols)]
            .iter()
            .map(|c| c.c)
            .collect();
        drop(g);
        // Prefer the detected URL at the clicked cell; fall back to the historical "word under the
        // cursor" (which covers relative file paths like `src/main.rs`).
        let target = crate::links::url_span(&line_text, col)
            .map(|s| s.as_str(&line_text).to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| expand_click_word(&line_text, col).to_owned());
        if target.is_empty() {
            return;
        }
        // Shell it through `open`: macOS routes `http(s)://…` to the browser and a relative path to
        // a text editor (XDG on Linux needs a different incantation; we're the mac build today).
        let _ = std::process::Command::new("open").arg(&target).spawn();
    }

    fn mouse_press(&mut self, x: f64, y: f64) {
        let Some(pt) = self.mouse_to_cell(x, y) else {
            // Click outside the grid (on chrome) clears any selection.
            if let Some(active) = self.app.active_session() {
                let mut g = active.term.lock();
                g.selection = None;
            }
            self.mouse_anchor = None;
            self.last_press = None;
            return;
        };
        let clicks = self.click_count(x, y);
        self.mouse_anchor = Some(pt);
        if let Some(active) = self.app.active_session() {
            let mut g = active.term.lock();
            let sel = match clicks {
                // Double-click: word selection drives semantic expansion off a non-empty range (a
                // distinct end point keeps bracket-search from hijacking a single cell).
                2 => {
                    let s = Selection::new(SelectionType::Semantic, pt, Side::Left);
                    let mut s2 = s;
                    s2.update(Point::new(pt.line + Line(1), Column(0)), Side::Right);
                    s2
                }
                3 => Selection::new(SelectionType::Lines, pt, Side::Left),
                // Single click: plain drag-select; on the same cell it just anchors.
                _ => Selection::new(SelectionType::Simple, pt, Side::Left),
            };
            g.selection = Some(sel);
        }
    }

    /// Grow the drag selection to the cursor's current cell while the button is held.
    fn mouse_drag(&mut self, x: f64, y: f64) {
        let Some(pt) = self.mouse_to_cell(x, y) else {
            return;
        };
        if let Some(active) = self.app.active_session() {
            let mut g = active.term.lock();
            if let Some(sel) = g.selection.as_mut() {
                sel.update(pt, Side::Right);
            }
        }
    }

    /// The user released the button: finalize the drag (if any) and copy the selection to the
    /// system clipboard, keeping it highlighted until the next click (standard terminal behavior).
    fn mouse_release(&mut self, x: f64, y: f64) {
        if self.mouse_anchor.is_some() {
            self.mouse_drag(x, y);
            self.mouse_anchor = None;
            self.copy_selection();
        }
    }

    /// Drag the hovered tab: as the pointer crosses a neighbor's midpoint, live-swap the dragged tab
    /// with it so the bar reflows under the cursor (standard tab-drag feel). `drag_tab` holds the
    /// current source slot; each swap keeps the dragged session under the pointer.
    fn tab_drag(&mut self, x: f64) {
        let src = match self.drag_tab {
            Some(i) => i,
            None => return,
        };
        if self.app.tabs.len() < 2 {
            return;
        }
        // Destination is the tab currently hovered (by the same hit-test the press used).
        let Some(dst) = self.tab_at(x, 4.0) else {
            return;
        };
        if dst == src {
            return;
        }
        self.app.move_tab_from_to(src, dst);
        // The dragged session landed at final index `dst`; resume dragging it from there so a
        // further move keeps the same session under the pointer.
        self.drag_tab = Some(dst);
    }

    /// Land a drag: if the tab moved from where it was pressed, leave it reordered; if it didn't
    /// move, treat the release as a click that switches to that tab. Always clears the drag state.
    /// The `from` slot is recovered as the tab the press started on via `drag_tab`; we track the
    /// original so a no-move release still switches.
    fn tab_drop(&mut self, x: f64, y: f64) {
        if let Some(i) = self.drag_tab.take() {
            // A plain click on a tab (press→release over the same tab) switches to it.
            if self.tab_at(x, y) == Some(i) {
                self.set_active(i);
            }
        }
    }

    /// Copy the active session's text selection to the system clipboard. No-op when empty.
    fn copy_selection(&mut self) {
        let Some(active) = self.app.active_session() else {
            return;
        };
        let g = active.term.lock();
        let Some(text) = g.selection_to_string() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        if let Ok(mut cb) = arboard::Clipboard::new() {
            let _ = cb.set_text(text);
        }
    }
}

/// A stable accent color for a host string, so every tab pointing at the same machine (and the
/// details pane) shares a hue and a fleet diver can tell at a glance which host a tab is on.
/// Deterministic — same host always yields the same color, across sessions and restarts.
/// The engine's own accent color as an RGB tuple, for the inactive-tab label. This is the deliberate
/// "which engine is this" signal (a brand color from the engine table), complementing `host_color`
/// which tells you *which machine*. A `[theme.accents]` override for the engine wins; otherwise the
/// built-in brand accent. Unknown engines fall back to the neutral chrome dim.
fn engine_accent(
    engine: &str,
    accents: &std::collections::BTreeMap<String, (u8, u8, u8)>,
) -> (u8, u8, u8) {
    if let Some(c) = accents.get(engine) {
        return *c;
    }
    static CACHE: std::sync::OnceLock<std::collections::HashMap<&'static str, (u8, u8, u8)>> =
        std::sync::OnceLock::new();
    let map = CACHE.get_or_init(|| {
        ENGINES
            .iter()
            .map(|e| (e.id, argb_to_rgb(e.color)))
            .collect()
    });
    map.get(engine).copied().unwrap_or(CHROME_DIM)
}

fn argb_to_rgb(argb: u32) -> (u8, u8, u8) {
    (
        ((argb >> 16) & 0xff) as u8,
        ((argb >> 8) & 0xff) as u8,
        (argb & 0xff) as u8,
    )
}

fn host_color(host: &str) -> (u8, u8, u8) {
    // FNV-1a over the host; pick a hue from the warm-to-cool range and keep it readable on black.
    let h = host.bytes().fold(0x811c_9dc5u32, |acc, b| {
        (acc ^ b as u32).wrapping_mul(0x0100_0193)
    });
    // 32 hues across the spectrum, OSV below is luminance-boosted so text reads on near-black.
    let hue = (h >> 4) % 32;
    const TABLE: [(u8, u8, u8); 32] = [
        (0xe0, 0x5b, 0x5b),
        (0xe0, 0x8b, 0x5b),
        (0xe0, 0xb8, 0x5b),
        (0xbf, 0xe0, 0x5b),
        (0x8b, 0xe0, 0x5b),
        (0x5b, 0xe0, 0x8b),
        (0x5b, 0xe0, 0xbf),
        (0x5b, 0xdd, 0xe0),
        (0x5b, 0xa8, 0xe0),
        (0x5b, 0x74, 0xe0),
        (0x8b, 0x5b, 0xe0),
        (0xbf, 0x5b, 0xe0),
        (0xe0, 0x5b, 0xd0),
        (0xe0, 0x5b, 0x9b),
        (0x9b, 0x7b, 0x5b),
        (0x9b, 0x9b, 0x5b),
        (0x5b, 0x9b, 0x7b),
        (0x5b, 0x7b, 0x9b),
        (0x7b, 0x5b, 0x9b),
        (0x9b, 0x5b, 0x7b),
        (0xf7, 0x9e, 0x8b),
        (0xf7, 0xbe, 0x8b),
        (0xd9, 0xf7, 0x8b),
        (0xa7, 0xf7, 0x8b),
        (0x8b, 0xf7, 0xa7),
        (0x8b, 0xf7, 0xd9),
        (0x8b, 0xdd, 0xf7),
        (0x8b, 0xa7, 0xf7),
        (0xa7, 0x8b, 0xf7),
        (0xd9, 0x8b, 0xf7),
        (0xf7, 0x8b, 0xd9),
        (0xf7, 0x8b, 0xa7),
    ];
    TABLE[(hue % 32) as usize]
}

/// Expand the word containing byte index `col` in a line of text, growing to whitespace/bracket
/// boundaries on both sides. Returns the substring (may be empty if `col` sits on a boundary).
fn expand_click_word(line: &str, col: usize) -> &str {
    let bytes = line.as_bytes();
    let col = col.min(bytes.len());
    let is_boundary = |b: u8| {
        b.is_ascii_whitespace()
            || matches!(b, b'(' | b')' | b'"' | b'\'' | b'<' | b'>' | b'[' | b']')
    };
    let mut start = col;
    while start > 0 && !is_boundary(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = col;
    while end < bytes.len() && !is_boundary(bytes[end]) {
        end += 1;
    }
    &line[start..end]
}

/// Map a typed letter to its control byte (a-z → 1-26), for Ctrl+key.
fn ctrl_byte(c: &str) -> Option<u8> {
    let ch = c.chars().next()?;
    let l = ch.to_ascii_lowercase();
    if ('a'..='z').contains(&l) {
        Some(l as u8 - b'a' + 1)
    } else {
        None
    }
}

impl ApplicationHandler for Application {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let size = crate::restore::load_geometry()
            .map(|(w, h)| Size::Physical(PhysicalSize::new(w, h)))
            .unwrap_or(Size::Logical(LogicalSize::new(110.0, 34.0)));
        let attribs = winit::window::Window::default_attributes()
            .with_title("harness-terminal")
            .with_inner_size(size)
            .with_position({
                let (x, y) = crate::restore::load_position().unwrap_or((200, 120));
                winit::dpi::Position::Physical(winit::dpi::PhysicalPosition::new(x, y))
            });
        match event_loop.create_window(attribs) {
            Ok(w) => {
                let w = Rc::new(w);
                self.window = Some(Rc::clone(&w));
                self.context = Context::new(Rc::clone(&w)).ok();
                self.surface = self
                    .context
                    .as_ref()
                    .and_then(|c| Surface::new(c, Rc::clone(&w)).ok());
                self.size = w.inner_size();
                self.metrics_from_scale();
                w.request_redraw();
            }
            Err(e) => {
                eprintln!("harness-terminal: failed to create window: {e}");
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                // Persist open tabs so they come back on the next launch; keep the window size too.
                self.app.save_all_scrollbacks();
                crate::restore::save(&self.app.tab_specs());
                self.save_muted_state();
                crate::restore::save_geometry(self.size.width, self.size.height);
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                self.size = size;
                if size.width > 0 && size.height > 0 {
                    crate::restore::save_geometry(size.width, size.height);
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            WindowEvent::Moved(pos) => {
                // Persist the window's top-left so a relaunch returns to the same spot on screen
                // (complements the size persistence; both are best-effort).
                crate::restore::save_position(pos.x, pos.y);
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                self.metrics_from_scale();
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.mods = mods.state();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    let mods = self.mods;
                    self.handle_key(&event.logical_key, &mods);
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x, position.y);
                // Track which tab the pointer is over so render can show a tail-preview tooltip.
                // Only request a redraw when it changes (or exits) a tab, not on every mouse move.
                let ht = self.tab_at(position.x, position.y);
                if ht != self.hover_tab {
                    self.hover_tab = ht;
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
                // Hand cursor over clickable links: switch the icon only when the state actually
                // changes so we don't spam the OS every mouse move.
                let link = self.cell_has_link(position.x, position.y);
                if link != self.over_link {
                    self.over_link = link;
                    if let Some(w) = &self.window {
                        use winit::window::CursorIcon;
                        w.set_cursor(if link {
                            CursorIcon::Pointer
                        } else {
                            CursorIcon::Default
                        });
                    }
                }
                // Dragging a tab reorders the bar (live swap as you cross a neighbor's midpoint);
                // dragging in the grid grows a text selection. Both are mutually exclusive.
                if self.drag_tab.is_some() && self.app.overlay == Overlay::None {
                    self.tab_drag(position.x);
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                } else if self.mouse_anchor.is_some() && self.app.overlay == Overlay::None {
                    self.mouse_drag(position.x, position.y);
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // Scroll wheel navigates the scrollback: up = back into history, down = forward.
                // Natural/line/other deltas are normalized to a sign; each notch ~3 lines keeps it
                // snappy without racing the keyboard.
                use winit::event::MouseScrollDelta;
                let mag = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(p) => -p.y / 40.0,
                };
                if mag == 0.0 {
                    return;
                }
                // Positive magnitude = scroll up (into history), negative = down (toward live).
                let lines = (mag * 3.0) as i32;
                scroll_active(self, lines);
                if lines > 0 {
                    if let Some(s) = self.app.active_session() {
                        s.set_scrolled(true);
                    }
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if self.app.overlay != Overlay::None {
                    return;
                }
                let (x, y) = self.cursor;
                // Middle-click = paste the system clipboard (X11 muscle memory). Only the press
                // matters; the release is discarded.
                if button == MouseButton::Middle {
                    if state == ElementState::Pressed {
                        self.paste_raw();
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                    }
                    return;
                }
                if button != MouseButton::Left {
                    return;
                }
                match state {
                    ElementState::Pressed => {
                        if self.mods.alt_key() {
                            // Alt+click moves the shell cursor instead of selecting.
                            self.mouse_alt_click(x, y);
                            return;
                        }
                        if self.mods.control_key() || self.mods.super_key() {
                            // Cmd/Ctrl+click opens the URL/path under the cursor.
                            self.mouse_open(x, y);
                            return;
                        }
                        // Pressing on a tab starts a drag-reorder (or a click-to-switch); pressing
                        // in the grid starts normal text selection. Both are mutually exclusive.
                        if let Some(i) = self.tab_at(x, y) {
                            self.drag_tab = Some(i);
                            if let Some(w) = &self.window {
                                w.request_redraw();
                            }
                            return;
                        }
                        self.drag_tab = None;
                        self.mouse_press(x, y);
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                    }
                    ElementState::Released => {
                        if self.drag_tab.is_some() {
                            self.tab_drop(x, y);
                        } else {
                            self.mouse_release(x, y);
                        }
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                    }
                }
            }
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // A quit request (prefix+q or the palette's Quit action) is honored here, after the key
        // handler's borrows have ended.
        if self.quit_requested {
            event_loop.exit();
            return;
        }
        // Reconnect_sweep is throttled internally, so piggyback a cheap link-health refresh on it:
        // a periodic ping to the local harness daemon keeps the status-line tunnel badge current.
        self.app.reconnect_sweep_refresh();
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}

/// Fire one coalesced macOS notification (title + body). Shells out to `osascript` — no extra crate
/// dependency, and the terminal already assumes macOS. Best-effort: a missing/denied osascript
/// (rare, headless) must not touch the terminal.
fn notify_simple(title: &str, body: &str) {
    let script = format!("display notification \"{body}\" with title \"{title}\"");
    let _ = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Group queued (kind, tab) notifications into coalesced batches: one batch per kind, in queued
/// order, listing every tab of that kind. Pure so it's unit-testable without an Application.
/// `pending` is drained by the caller; a multi-tab `broadcast` (busy in N tabs at once) collapses
/// to one "busy" batch instead of N popups.
pub(crate) fn group_notifications(pending: &[(String, usize)]) -> Vec<(String, Vec<usize>)> {
    // One bucket per kind, in first-seen order, collecting every tab of that kind regardless of how
    // busy/bell interleave — a frame with mixed kinds still yields exactly two popups (one per kind).
    let mut batches: Vec<(String, Vec<usize>)> = Vec::new();
    for (kind, tab) in pending {
        match batches.iter_mut().find(|(k, _)| *k == *kind) {
            Some((_, tabs)) => tabs.push(*tab),
            None => batches.push((kind.clone(), vec![*tab])),
        }
    }
    batches
}

/// Join session labels for a coalesced-notification list. A handful reads as `a, b, c`; a fleet-wide
/// broadcast (every host) collapses to `all N` so the popup stays short.
fn join_labels(labels: &[String]) -> String {
    if labels.len() <= 3 {
        labels.join(", ")
    } else {
        format!("all {} sessions", labels.len())
    }
}

/// Entry point: create the native window and run the event loop.
pub fn run(app: App) -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = EventLoop::new()?;
    let mut application = Application::new(app);
    event_loop.run_app(&mut application)?;
    Ok(())
}

/// The tab `prefix+H` should land on given each tab's host (in tab order): the first tab of the
/// next distinct host after `active`, in host-first-occurrence order, wrapping around. Returns None
/// when `hosts` is empty or has only one distinct host (nothing to page by). Pure so it can be
/// unit-tested without building a window or PTYs.
fn next_host_index(hosts: &[&str], active: usize) -> Option<usize> {
    if hosts.is_empty() {
        return None;
    }
    // Distinct hosts in tab order (a monotonic scan keeps the first tab per host).
    let mut uniq: Vec<&str> = Vec::new();
    for h in hosts {
        if !uniq.contains(h) {
            uniq.push(h);
        }
    }
    if uniq.len() < 2 {
        return None;
    }
    let cur = *hosts.get(active)?;
    let cur_idx = uniq.iter().position(|h| *h == cur).unwrap_or(0);
    let target = uniq[(cur_idx + 1) % uniq.len()];
    // First tab of `target` AFTER `active`, else (wrap) the first tab of `target` overall.
    hosts
        .iter()
        .enumerate()
        .skip(active + 1)
        .find(|&(_, h)| *h == target)
        .map(|(i, _)| i)
        .or_else(|| hosts.iter().position(|h| *h == target))
}

#[cfg(test)]
mod tests {
    use super::{
        argb_to_rgb, broadcast_bytes, collect_fleet_matches, engine_accent, expand_click_word,
        fmt_duration, group_notifications, host_color, join_labels, next_host_index, recall_index,
        FleetMatch,
    };

    use std::sync::Arc;

    /// Fleet search spans multiple sessions and is sorted by tab then line. Build two real emulator
    /// Terms (no PTY needed), seed each with distinct text, and assert the collected matches are
    /// tagged by tab and ordered (tab 1's matches before tab 2's, lines in order within each tab).
    #[test]
    fn fleet_search_crosses_tabs_sorted_by_tab_then_line() {
        use alacritty_terminal::sync::FairMutex;
        use alacritty_terminal::term::{Config, Term};
        use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

        use crate::session::{Listener, TermSize};

        // Two tabs: tab 0 has "fix" on two lines, tab 1 on one line — out of sorted order at the
        // line level so we can prove the primary sort is BY TAB.
        let size = TermSize { lines: 4, cols: 40 };
        let mut terms: Vec<Arc<FairMutex<Term<Listener>>>> = Vec::new();
        let t0 = Arc::new(FairMutex::new(Term::new(
            Config::default(),
            &size,
            Listener::default(),
        )));
        {
            let mut p: Processor<StdSyncHandler> = Processor::default();
            p.advance(&mut *t0.lock(), b"fix first\r\nno match\r\nfix third");
        }
        let t1 = Arc::new(FairMutex::new(Term::new(
            Config::default(),
            &size,
            Listener::default(),
        )));
        {
            let mut p: Processor<StdSyncHandler> = Processor::default();
            p.advance(&mut *t1.lock(), b"only fix here");
        }
        terms.push(t0);
        terms.push(t1);

        let hits = collect_fleet_matches(&terms, "fix");
        // 2 (tab 0) + 1 (tab 1) = 3 matches.
        assert_eq!(hits.len(), 3);
        // Sorted by tab first: both tab-0 hits (lines 0, 2 in line order) then the tab-1 hit.
        assert_eq!(
            hits[0],
            FleetMatch {
                tab: 0,
                line: 0,
                col: 0
            }
        );
        assert_eq!(
            hits[1],
            FleetMatch {
                tab: 0,
                line: 2,
                col: 0
            }
        );
        assert_eq!(
            hits[2],
            FleetMatch {
                tab: 1,
                line: 0,
                col: 5
            }
        );
        // Case-insensitive: uppercase query still matches lowercase text (recompute lowercases it).
        assert_eq!(collect_fleet_matches(&terms, "FIX").len(), 3);
        // A query in no tab matches nothing.
        assert!(collect_fleet_matches(&terms, "zzz").is_empty());
    }

    /// Same-kind queued notifications (a multi-tab broadcast) coalesce into ONE batch, in order,
    /// instead of one popup per tab.
    #[test]
    fn group_notifications_coalesces_same_kind() {
        let pending = vec![
            ("busy".to_string(), 0),
            ("busy".to_string(), 2),
            ("busy".to_string(), 5),
        ];
        let batches = group_notifications(&pending);
        assert_eq!(
            batches.len(),
            1,
            "three simltaneous busy events -> one batch"
        );
        assert_eq!(batches[0].0, "busy");
        assert_eq!(batches[0].1, vec![0, 2, 5]);
    }

    /// Mixed busy and bell events in one frame stay in separate batches so each kind gets its own
    /// (title-accurate) popup, but same-kind events still merge across the alternation.
    #[test]
    fn group_notifications_splits_busy_and_bell() {
        let pending = vec![
            ("busy".to_string(), 1),
            ("bell".to_string(), 2),
            ("busy".to_string(), 3),
        ];
        let batches = group_notifications(&pending);
        assert_eq!(
            batches.len(),
            2,
            "one bucket per kind, even when interleaved"
        );
        assert_eq!(batches[0].0, "busy");
        assert_eq!(batches[0].1, vec![1, 3]);
        assert_eq!(batches[1].0, "bell");
        assert_eq!(batches[1].1, vec![2]);
    }

    /// A single backgrounded tab going busy stays a single-notification event (no spurious merge
    /// or split).
    #[test]
    fn group_notifications_single_stays_single() {
        let pending = vec![("busy".to_string(), 7)];
        let batches = group_notifications(&pending);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0], ("busy".to_string(), vec![7]));
    }

    /// The recover kind is its own bucket (distinct from busy/bell), so several panes reconnecting
    /// in the same frame coalesce into ONE "N sessions reconnected" popup.
    #[test]
    fn group_notifications_keeps_recover_its_own_kind() {
        let pending = vec![
            ("busy".to_string(), 1),
            ("recover".to_string(), 2),
            ("recover".to_string(), 3),
        ];
        let batches = group_notifications(&pending);
        let recover = batches.iter().find(|(k, _)| k == "recover").unwrap();
        assert_eq!(recover.1, vec![2, 3]);
    }

    /// A fleet-wide broadcast produces N busy events; the list should collapse to "all N sessions"
    /// rather than a long comma list.
    #[test]
    fn join_labels_collapses_large_fleet() {
        assert_eq!(
            join_labels(&["a".into(), "b".into(), "c".into()]),
            "a, b, c"
        );
        assert_eq!(
            join_labels(&["w".into(), "x".into(), "y".into(), "z".into()]),
            "all 4 sessions"
        );
    }

    /// Every host gets a stable, in-table color; the same host never changes across calls, and two
    /// different hosts can share a color (fine) but the mapping is deterministic.
    #[test]
    fn host_color_is_deterministic() {
        let a = host_color("build-host");
        assert_eq!(a, host_color("build-host"));
        let b = host_color("10.0.0.4");
        assert_eq!(b, host_color("10.0.0.4"));
        assert_ne!(a, (0, 0, 0), "colors must be visible on black");
    }

    /// ARGB unpacking extracts RGB in the right order, and known engines resolve to their brand
    /// accent while unknown engines fall back to the neutral dim (never black).
    #[test]
    fn engine_accent_unpacks_argb_and_falls_back() {
        assert_eq!(argb_to_rgb(0xff_9a4dff), (0x9a, 0x4d, 0xff));
        // Claude's accent is purple-ish; it must differ from a randomly-picked unknown's fallback.
        let empty = std::collections::BTreeMap::new();
        let cl = engine_accent("claude", &empty);
        assert_eq!(cl, (0x9a, 0x4d, 0xff));
        let unknown = engine_accent("no-such-engine", &empty);
        assert_ne!(
            unknown,
            (0, 0, 0),
            "unknown-engine fallback must still be visible"
        );
        // A `[theme.accents]` override wins over the built-in brand accent.
        let mut accents = std::collections::BTreeMap::new();
        accents.insert("claude".to_string(), (255, 0, 0));
        assert_eq!(engine_accent("claude", &accents), (255, 0, 0));
    }

    /// Cmd+click word expansion picks the whole token, not the shell quoting around it.
    #[test]
    fn click_word_expands_to_token() {
        let line = "see https://example.com/foo in the log (src/main.rs)";
        assert_eq!(expand_click_word(line, 25), "https://example.com/foo");
        // The substring is a byte index into the line; find it by locating the token start.
        let url_start = line.find("https").unwrap();
        assert_eq!(
            expand_click_word(line, url_start + 5),
            "https://example.com/foo"
        );
        // A path inside parens expands to the path, stopping at ')'.
        let p = line.find("src/main.rs").unwrap();
        assert_eq!(expand_click_word(line, p), "src/main.rs");
        // A click on a boundary returns an empty token.
        assert_eq!(expand_click_word("   ", 1), "");
    }

    /// A single word on its own line is returned whole.
    #[test]
    fn click_word_single_token() {
        assert_eq!(expand_click_word("README.md", 3), "README.md");
    }

    /// Broadcast formatting: a non-blank line is fanned out with a trailing newline; a blank line
    /// sends nothing (never broadcast a bare newline).
    #[test]
    fn broadcast_bytes_sends_line_but_not_blank() {
        assert_eq!(broadcast_bytes("git pull"), b"git pull\n");
        assert_eq!(
            broadcast_bytes("  "),
            b"  \n",
            "whitespace-only is still a real line"
        );
        assert!(
            broadcast_bytes("").is_empty(),
            "blank must not broadcast a bare newline"
        );
    }

    /// Broadcast-history recall walks older/newer and wraps around both ends; a cold start lands on
    /// the newest line for Shift+Up and the oldest for Shift+Down.
    #[test]
    fn recall_broadcast_wraps_and_cold_starts() {
        let n = 3;
        // Cold + Up -> newest (index 2), cold + Down -> oldest (index 0).
        assert_eq!(recall_index(n, -1, None), 2);
        assert_eq!(recall_index(n, 1, None), 0);
        // Stepping older from the newest wraps to the oldest.
        assert_eq!(recall_index(n, -1, Some(2)), 1);
        assert_eq!(recall_index(n, -1, Some(0)), 2);
        // Stepping newer from the oldest wraps to the newest.
        assert_eq!(recall_index(n, 1, Some(0)), 1);
        assert_eq!(recall_index(n, 1, Some(2)), 0);
    }

    /// Idle-age formatting stays compact and readable at every scale — seconds, minutes, hours,
    /// days — never a raw millisecond dump.
    #[test]
    fn fmt_duration_readable_units() {
        let d = |s: u64| std::time::Duration::from_secs(s);
        assert_eq!(fmt_duration(d(0)), "0s");
        assert_eq!(fmt_duration(d(59)), "59s");
        assert_eq!(fmt_duration(d(60)), "1m");
        assert_eq!(fmt_duration(d(3599)), "59m");
        assert_eq!(fmt_duration(d(3600)), "1h");
        assert_eq!(fmt_duration(d(23 * 3600)), "23h");
        assert_eq!(fmt_duration(d(24 * 3600)), "1d");
    }

    /// `prefix+H` pages by host: from a three-host fleet it steps to the first tab of the next
    /// distinct host (in first-occurrence order), not the adjacent pane.
    #[test]
    fn next_host_pages_by_distinct_host_in_first_occurrence_order() {
        // Tab order: a0 a1 (host-a), b0 (host-b), c0 c1 (host-c).
        let hosts = ["host-a", "host-a", "host-b", "host-c", "host-c"];
        // Active on host-a -> next host in first-occurrence order is host-b, first tab = index 2.
        assert_eq!(next_host_index(&hosts, 0), Some(2));
        // Active on index 1 (still host-a) -> same next host.
        assert_eq!(next_host_index(&hosts, 1), Some(2));
        // Active on host-b -> next is host-c, first tab = index 3.
        assert_eq!(next_host_index(&hosts, 2), Some(3));
    }

    #[test]
    fn next_host_wraps_to_first_distinct_host() {
        let hosts = ["host-a", "host-b", "host-c"];
        // Active on the LAST distinct host (host-c) -> wrap to the first host, its first tab.
        assert_eq!(next_host_index(&hosts, 2), Some(0));
    }

    #[test]
    fn next_host_from_later_tab_still_picks_next_not_adjacent() {
        let hosts = ["host-a", "host-b", "host-b", "host-c"];
        // Active on host-b's second tab (index 2) -> next distinct host is host-c (index 3),
        // NOT host-b's adjacent behavior blocking on same-host tabs.
        assert_eq!(next_host_index(&hosts, 2), Some(3));
    }

    #[test]
    fn next_host_is_none_for_single_or_empty_fleet() {
        assert_eq!(next_host_index(&[], 0), None);
        assert_eq!(next_host_index(&["only-host", "only-host"], 0), None);
    }
}
