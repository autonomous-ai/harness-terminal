//! A session — the atomic unit of the terminal. One `Term` emulator surface fed by one transport.
//!
//! `TAB = SESSION = PANE@HOST`. Each session owns its own emulator (grid + parser) and its own
//! PTY/transport. The same `Session` struct handles a local shell and (later) a remote tmux pane —
//! only the backing source of bytes differs.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line as GridLine};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{cell::Flags, Term};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

use crate::transport::{LocalPtyTransport, Transport};

/// Build the alacritty term config every session starts from, applying the configured in-memory
/// scrollback line limit (`config.scrollback_lines`) instead of the alacritty default. Absent
/// keeps the default 10000 lines. Session creation is user-triggered (never hot-path), so a config
/// read here is free; a `0` explicitly disables scrollback history.
#[cfg(test)]
use alacritty_terminal::term::Config;

/// Upper bound on the in-memory scrollback a single session holds, so a mis-typed config value
/// (e.g. `scrollback_lines = 1000000000`) can never balloon a pane into exhausting RAM — the grid
/// pre-allocates history up front. 1M lines is far beyond real agent runs but still bounded.
pub(crate) const MAX_SCROLLBACK_LINES: usize = 1_000_000;

/// Clamp a configured scrollback-line request into the safe range. `0` stays 0 (no history);
/// enormous values are pinned to [`MAX_SCROLLBACK_LINES`]. Pure so the guard is unit-testable.
fn clamp_scrollback_lines(n: usize) -> usize {
    n.min(MAX_SCROLLBACK_LINES)
}

fn term_config() -> alacritty_terminal::term::Config {
    let mut cfg = alacritty_terminal::term::Config::default();
    if let Some(n) = crate::config::Config::load().scrollback_lines {
        cfg.scrolling_history = clamp_scrollback_lines(n);
    }
    cfg
}

/// Reusable pane geometry.
#[derive(Clone, Copy, Debug)]
pub struct TermSize {
    pub lines: usize,
    pub cols: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// Sink for terminal render events. Rendering is drawn synchronously from the grid, so wakeups
/// are currently no-ops; a future dirty-region/GPU client can hook `Event::Wakeup` here. We act on
/// these events:
/// - `ClipboardStore`, which the alacritty emulator issues for an OSC 52 write (e.g. an agent's
///   `pbcopy`/`wl-copy` over the pane) — the data is copied to the system clipboard.
/// - `Title`, which the emulator issues for an OSC 0/2 window-title write (e.g. an agent announcing
///   what it's working on). The text is stored in a shared slot so the tab/status can show it.
/// - `Bell`, which the emulator issues on a terminal bell (BEL / OSC 14-#). Many agent CLIs and
///   shells ring it when a long run finishes, so the bell is surfaced as a tab badge + OS
///   notification to tell a diver "done" without watching.
#[derive(Clone, Default)]
pub struct Listener {
    title: Arc<Mutex<Option<String>>>,
    /// Set when this pane rings the terminal bell. Read + cleared by `Session::take_bell`.
    bell: Arc<AtomicBool>,
}

impl Listener {
    /// Create a listener that records OSC window/task titles into `title`.
    pub fn with_title(title: Arc<Mutex<Option<String>>>) -> Self {
        Listener {
            title,
            bell: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The shared bell flag, for `Session` to clone while keeping the listener for the emulator.
    pub fn bell_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.bell)
    }
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            Event::ClipboardStore(_, text) => {
                // Best-effort: a missing/wrong clipboard backend must not break terminal I/O.
                if let Ok(mut cb) = arboard::Clipboard::new() {
                    let _ = cb.set_text(text);
                }
            }
            Event::Title(title) => {
                if let Ok(mut t) = self.title.lock() {
                    *t = Some(title);
                }
            }
            Event::ResetTitle => {
                if let Ok(mut t) = self.title.lock() {
                    *t = None;
                }
            }
            Event::Bell => {
                self.bell.store(true, Ordering::SeqCst);
            }
            _ => {}
        }
    }
}

/// Latency smoothing for remote-byte transports.
///
/// When a session is reached over a link with round-trip latency (ssh or the harness tunnel), the
/// typed keystrokes are optimistically echoed into the grid immediately, so typing feels instant
/// (`Session::write`). That same text then comes back from the remote pane ~RTT later — without
/// cancellation it would double-render. `EchoCanceller` suppresses that returned copy (it is the
/// identical bytes, plus any prompt echo the app printed), while still passing genuine program
/// output through untouched.
///
/// It is byte-oriented and conservative: a byte is only dropped while it matches the front of the
/// pending outgoing buffer AND the remote echo is still "expected" (within the smoothing window).
/// Program output that doesn't match pending input is never touched.
///
/// The canceller is deliberately `Arc<Mutex<…>>` — `Session::write` (the UI thread) records
/// outgoing bytes while each transport reader thread (which runs independently and bypasses the
/// `Session`) filters the returned stream with the `feed()` free function.
pub struct EchoCanceller {
    /// Bytes typed locally but not yet confirmed returned (the optimistic-echo protection window).
    pending: Mutex<VecDeque<(u8, Instant)>>,
    /// How long a pending echo byte is expected before it is treated as never-coming-back.
    window: Duration,
}

impl Default for EchoCanceller {
    fn default() -> Self {
        // Typical human idle between keystrokes is far larger than this; an RTT is far smaller. Long
        // enough to carry the optimistic echo across the link, short enough that a pane that never
        // echoes (password prompt, fullscreen app) doesn't poison later genuine output.
        EchoCanceller {
            pending: Mutex::new(VecDeque::new()),
            window: Duration::from_millis(1500),
        }
    }
}

impl EchoCanceller {
    /// Record locally-typed bytes that are about to go out and be optimistically echoed.
    pub fn note_echo(&self, bytes: &[u8]) {
        let mut p = self.pending.lock().unwrap();
        let now = Instant::now();
        for &b in bytes {
            p.push_back((b, now));
        }
    }

    /// Filter a chunk of bytes returned by the transport: return only the bytes that are NOT the
    /// just-typed echo (i.e. genuine program output), while dropping cancelled echo bytes. Bytes
    /// that could be echo but are no longer "expected" fall through as genuine output.
    pub fn filter_echo(&self, bytes: &[u8]) -> Vec<u8> {
        let mut p = self.pending.lock().unwrap();
        // Drop any pending echo bytes that outlived the window (pane never echoed them back).
        let cutoff = Instant::now() - self.window;
        while let Some(&(_, at)) = p.front() {
            if at < cutoff {
                p.pop_front();
            } else {
                break;
            }
        }
        let mut out = Vec::with_capacity(bytes.len());
        for &b in bytes {
            if let Some(&(pb, _)) = p.front() {
                if pb == b {
                    p.pop_front();
                    continue; // cancelled — this is the returned echo
                }
            }
            out.push(b);
        }
        out
    }
}

/// Who owns this session (host · engine · title). `name` is an optional user-assigned tab label
/// set via rename; None means "no custom name" and chrome falls back to the engine id.
#[derive(Clone, Debug)]
pub struct SessionMeta {
    pub host: String,
    pub engine: String,
    pub title: String,
    pub name: Option<String>,
}

/// A single terminal session: emulator surface + transport for I/O.
pub struct Session {
    pub meta: SessionMeta,
    /// The remote tmux session name this pane attaches to, when it was opened via remote-attach as
    /// "attach an existing session" (rather than a fresh engine spawn). `None` for every other kind.
    /// Persisted in the tab spec so a relaunch re-attaches to the same named session.
    pub attach_session: Option<String>,
    pub term: Arc<FairMutex<Term<Listener>>>,
    transport: Box<dyn Transport>,
    /// Echo cancellation for remote-byte transports (ssh/tunnel); None for local ones.
    echo: Option<Arc<EchoCanceller>>,
    /// Live OSC title (what the shell/agent in the pane is currently doing), written by the
    /// emulator's Listener and read by the tab/status chrome.
    title: Arc<Mutex<Option<String>>>,
    /// Set when the pane rang the terminal bell (a long agent run finishing, e.g.). Read + cleared
    /// by `take_bell` so the chrome can show one bell badge + notification per ring.
    bell: Arc<AtomicBool>,
    /// Reconnect bookkeeping for dropped remote transports (tmux/ssh/tunnel): how many consecutive
    /// attempts have failed and when the next attempt may fire (exponential backoff so a dead daemon
    /// isn't hammered and the status line can show *how long* it's been down).
    retry: Mutex<RetryState>,
    /// Whether THIS session's view is scrolled into history (live-follow suspended). Per-session so
    /// switching tabs doesn't lose where you left each pane — flag A's scroll survives a switch to
    /// B and back, unlike a single app-wide "scrolled".
    scrolled: Arc<std::sync::atomic::AtomicBool>,
    /// Keystrokes typed while the transport is dead, flushed into the pane on the next successful
    /// reconnect. Lets a diver queue a command for a host that's coming back instead of typing into a
    /// black hole (and losing the input when the pane re-attaches).
    pending: Mutex<Vec<u8>>,
    /// Monotonic instant the session was constructed — its age/uptime, shown in the `prefix+i`
    /// info panel so a diver can tell a long-running agent from a just-spawned one at a glance.
    born: Instant,
}

/// Per-session auto-reconnect policy: exponential backoff with a visible attempt count.
struct RetryState {
    /// Consecutive failed reconnect attempts since the transport last came up.
    attempts: u32,
    /// Monotonic instant before which we won't try again.
    next_attempt: Instant,
}

impl RetryState {
    fn new() -> Self {
        RetryState {
            attempts: 0,
            next_attempt: Instant::now(),
        }
    }
    /// Exponential backoff seconds for the *next* retry, capped: 5s, 10s, 20s, … 60s.
    fn backoff_seconds(attempts: u32) -> u64 {
        let exp = 5u64 << attempts.min(4); // 5,10,20,40 -> clamp at 60
        exp.min(60)
    }
}

impl Session {
    /// Current OSC window-title (from the pane's `\x1b]0;…\x07`), if one has been set. Used by the
    /// chrome to show live task context per tab.
    pub fn live_title(&self) -> Option<String> {
        self.title.lock().ok().and_then(|t| t.clone())
    }

    /// How long this session has been alive (since construction). Drives the age/uptime row in the
    /// `prefix+i` info panel.
    pub fn age(&self) -> Duration {
        Instant::now().saturating_duration_since(self.born)
    }

    /// True if the pane has rung the terminal bell since it was last checked, clearing the flag.
    /// Used by the chrome to badge a bell (a long agent run finishing) once, then let it fade.
    pub fn take_bell(&self) -> bool {
        self.bell.swap(false, Ordering::SeqCst)
    }

    /// Current scrollback line count (rows that have scrolled off the top and into history). Grows
    /// monotonically as the pane produces output; used to badge tabs that produced unseen output
    /// while we were looking at another one. Read-only; never locks for long.
    pub fn history_len(&self) -> usize {
        self.term.lock().grid().history_size()
    }

    /// Create a session running a LOCAL program (shell or an engine CLI) in a fresh PTY.
    /// `working_dir` is an optional per-tab working directory (None falls back to config `start_cwd`
    /// / the binary's cwd inside the transport).
    pub fn local(
        meta: SessionMeta,
        program: &str,
        args: Vec<String>,
        size: TermSize,
        working_dir: Option<String>,
    ) -> io::Result<Session> {
        let title = Arc::new(Mutex::new(None));
        let listener = Listener::with_title(Arc::clone(&title));
        let bell = listener.bell_flag();
        let term = Arc::new(FairMutex::new(Term::new(term_config(), &size, listener)));
        let transport =
            LocalPtyTransport::spawn(program, args, size, working_dir, Arc::clone(&term))?;
        Ok(Session {
            meta,
            attach_session: None,
            term,
            transport: Box::new(transport),
            echo: None,
            title,
            bell,
            retry: Mutex::new(RetryState::new()),
            scrolled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending: Mutex::new(Vec::new()),
            born: Instant::now(),
        })
    }

    /// Create a session backed by a real tmux pane (control mode).
    pub fn tmux(meta: SessionMeta, program: &str, size: TermSize) -> io::Result<Session> {
        let title = Arc::new(Mutex::new(None));
        let listener = Listener::with_title(Arc::clone(&title));
        let bell = listener.bell_flag();
        let term = Arc::new(FairMutex::new(Term::new(term_config(), &size, listener)));
        let transport = crate::transport::TmuxTransport::spawn(program, size, Arc::clone(&term))?;
        Ok(Session {
            meta,
            attach_session: None,
            term,
            transport: Box::new(transport),
            echo: None,
            title,
            bell,
            retry: Mutex::new(RetryState::new()),
            scrolled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending: Mutex::new(Vec::new()),
            born: Instant::now(),
        })
    }

    /// Create a session whose pane is reached through the harness pane-relay tunnel at `host:port`.
    /// The pane and its bytes live on `host`; our client only talks to the local harness daemon.
    pub fn tunnel(
        meta: SessionMeta,
        host: &str,
        port: u16,
        program: &str,
        size: TermSize,
    ) -> io::Result<Session> {
        let title = Arc::new(Mutex::new(None));
        let listener = Listener::with_title(Arc::clone(&title));
        let bell = listener.bell_flag();
        let term = Arc::new(FairMutex::new(Term::new(term_config(), &size, listener)));
        // The tunnel crosses a latency link, so the session owns an echo canceller (Session::write
        // notes keystrokes; the transport's reader thread cancels the returned copy).
        let echo = Arc::new(EchoCanceller::default());
        let transport = crate::transport::TunnelTransport::spawn(
            host,
            port,
            program,
            size,
            Arc::clone(&term),
            Arc::clone(&echo),
        )?;
        Ok(Session {
            meta,
            attach_session: None,
            term,
            transport: Box::new(transport),
            echo: Some(echo),
            title,
            bell,
            retry: Mutex::new(RetryState::new()),
            scrolled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending: Mutex::new(Vec::new()),
            born: Instant::now(),
        })
    }

    /// Attach to an EXISTING named tmux session on `host` through the harness pane-relay tunnel.
    /// Unlike [`Session::tunnel`] this does NOT spawn/restart an engine — it resumes whatever is
    /// already running in that session (attach-or-create, no kill) and replays the pane's screen.
    pub fn tunnel_attach(
        meta: SessionMeta,
        host: &str,
        port: u16,
        session: &str,
        size: TermSize,
    ) -> io::Result<Session> {
        let title = Arc::new(Mutex::new(None));
        let listener = Listener::with_title(Arc::clone(&title));
        let bell = listener.bell_flag();
        let term = Arc::new(FairMutex::new(Term::new(term_config(), &size, listener)));
        // Attaching to a live pane is a latency cross too — same echo-cancellation setup as tunnel.
        let echo = Arc::new(EchoCanceller::default());
        let transport = crate::transport::TunnelTransport::spawn_attach(
            host,
            port,
            session,
            size,
            Arc::clone(&term),
            Arc::clone(&echo),
        )?;
        Ok(Session {
            meta,
            attach_session: Some(session.to_string()),
            term,
            transport: Box::new(transport),
            echo: Some(echo),
            title,
            bell,
            retry: Mutex::new(RetryState::new()),
            scrolled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending: Mutex::new(Vec::new()),
            born: Instant::now(),
        })
    }

    /// Create a session whose pane lives on REMOTE host `host` (via ssh + tmux control mode).
    /// `meta.host` carries the `@host` half of `pane@host`.
    pub fn remote(meta: SessionMeta, program: &str, size: TermSize) -> io::Result<Session> {
        let title = Arc::new(Mutex::new(None));
        let listener = Listener::with_title(Arc::clone(&title));
        let bell = listener.bell_flag();
        let term = Arc::new(FairMutex::new(Term::new(term_config(), &size, listener)));
        // Remote ssh crosses a latency link — same echo-cancellation setup as the tunnel.
        let echo = Arc::new(EchoCanceller::default());
        let transport = crate::transport::RemoteTransport::spawn(
            &meta.host,
            program,
            size,
            Arc::clone(&term),
            Arc::clone(&echo),
        )?;
        Ok(Session {
            meta,
            attach_session: None,
            term,
            transport: Box::new(transport),
            echo: Some(echo),
            title,
            bell,
            retry: Mutex::new(RetryState::new()),
            scrolled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending: Mutex::new(Vec::new()),
            born: Instant::now(),
        })
    }

    /// Transport kind: "pty" / "tmux" / "ssh" / "tunnel" (shown in the status line).
    pub fn kind(&self) -> &'static str {
        self.transport.kind()
    }

    /// The harness control port this session reaches over, if it's a tunnel transport (used to keep
    /// a non-default remote port when persisting, duplicating, or undoing-close). None otherwise.
    pub fn port(&self) -> Option<u16> {
        self.transport.port()
    }

    /// Whether the session's transport is still live. Local PTYs are always alive; tmux/ssh/tunnel
    /// transports report false once their connection or pane dies.
    pub fn alive(&self) -> bool {
        self.transport.alive()
    }

    /// Re-attach after a dropped connection. Local PTYs are a no-op. Returns an error only if the
    /// immediate re-spawn fails (e.g. tunnel daemon unreachable again) — callers retry later.
    ///
    /// Encapsulates the per-session exponential backoff: each failure pushes the next attempt out by
    /// 5s → 10s → 20s → … (capped at 60s), and success resets the counter. The reconnect sweep calls
    /// this every few seconds but only actually attempts when the session's backoff window has passed,
    /// so a permanently-dead daemon gets probed on a sane schedule, not every sweep tick.
    pub fn reconnect(&mut self) -> io::Result<()> {
        let due = {
            let r = self.retry.lock().unwrap();
            Instant::now() >= r.next_attempt
        };
        if !due {
            // Not yet due — skip. Return Ok so the sweep treats it as "handled, nothing to do",
            // keeping the session dead but not failing the sweep.
            return Ok(());
        }
        self.reconnect_now()
    }

    /// Force an immediate reconnect attempt, ignoring the exponential-backoff timer so a diver can
    /// nudge a dropped pane/daemon back right away instead of waiting out the retry ladder. On
    /// failure the failure still lands on the backoff ladder (so the next auto-try is still sane).
    pub fn reconnect_now(&mut self) -> io::Result<()> {
        match self.transport.reconnect() {
            Ok(()) => {
                // Back up: the pane came through, reset the retry ladder.
                self.retry.lock().unwrap().attempts = 0;
                // Replay whatever was typed while the pane was dead so a queued command actually
                // lands in the re-attached pane.
                let buffered: Vec<u8> = self.pending.lock().unwrap().drain(..).collect();
                if !buffered.is_empty() {
                    self.transport.write(&buffered);
                }
                Ok(())
            }
            Err(e) => {
                let mut r = self.retry.lock().unwrap();
                r.attempts = r.attempts.saturating_add(1);
                let backoff = Duration::from_secs(RetryState::backoff_seconds(r.attempts));
                r.next_attempt = Instant::now() + backoff;
                Err(e)
            }
        }
    }

    /// Human-readable reconnect status, for the status line and tab chrome. None while the transport
    /// is alive or a local PTY; otherwise describes the current backoff state.
    pub fn retry_info(&self) -> Option<String> {
        if self.transport.alive() {
            return None;
        }
        let r = self.retry.lock().unwrap();
        if r.attempts == 0 {
            Some("reconnecting…".to_string())
        } else {
            let secs = RetryState::backoff_seconds(r.attempts);
            Some(format!("reconnect {} · retry in {}s", r.attempts, secs))
        }
    }

    /// Number of bytes buffered while the transport was down (type-ahead awaiting a reconnect).
    /// Zero for live tabs. Lets the tab/status show how much queued input will land when the pane
    /// comes back.
    pub fn pending_bytes(&self) -> usize {
        self.pending.lock().unwrap().len()
    }

    /// Kill the pane's underlying session so it stops consuming resources on its host. A no-op for a
    /// local PTY (already tied to the child process). The tab should be removed right after; the
    /// transport's alive flag flips false and the watchdog would otherwise try to reconnect.
    pub fn destroy(&self) {
        self.transport.destroy();
    }

    /// Push keystrokes into the session's transport. If the transport is down the keystrokes are
    /// buffered in `pending` (see [`Session::pending`]) and flushed on the next successful reconnect —
    /// so typing into a dead pane queues the command rather than dropping it.
    pub fn write(&self, bytes: &[u8]) {
        if !self.transport.alive() {
            self.pending.lock().unwrap().extend_from_slice(bytes);
            return;
        }
        self.transport.write(bytes);
        if let Some(echo) = &self.echo {
            // Latency smoothing: optimistically render the keystrokes locally so typing feels
            // instant, and record them so the transport reader thread can cancel the identical copy
            // that comes back ~RTT later (no double-render).
            let mut term = self.term.lock();
            let mut parser: Processor<StdSyncHandler> = Processor::default();
            parser.advance(&mut *term, bytes);
            echo.note_echo(bytes);
        }
    }

    /// Resize the session's screen + underlying transport.
    pub fn resize(&self, size: TermSize) {
        self.term.lock().resize(size);
        self.transport.resize(size);
    }

    /// Capture the session's full scrollback + visible screen as plain text, most-recent line last,
    /// with hard line boundaries (wrapped rows rejoined by removing their newlines). Used to persist
    /// history across a restart so a restored session lands with its scrollback intact.
    ///
    /// We capture *text*, not cells: replaying text through the parser on restore reconstructs
    /// wrapping, color, and cursor state exactly as the original byte stream would have, and text is
    /// cheap to serialize/version. (alacritty's `Grid` serde is internal-only; the `Term` wrapper
    /// isn't serializable, so a faithful cell-level snapshot isn't reachable through the public API.)
    /// Whether this session ceases live-follow (view pinned into history).
    pub fn scrolled(&self) -> bool {
        self.scrolled.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Pin / release this session view into / from history scroll.
    pub fn set_scrolled(&self, v: bool) {
        self.scrolled.store(v, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn capture_scrollback(&self) -> String {
        let term = self.term.lock();
        capture_grid_to_string(term.grid())
    }

    /// Capture just the tail of the *visible screen* (the live rows currently on screen, newest
    /// last) as plain text split into logical lines. Cheap — it never walks history — so it can run
    /// every frame while a hover preview is showing. Used by the tab-bar hover tooltip to show what
    /// a session is doing right now without switching to it.
    pub fn tail(&self, n: usize) -> Vec<String> {
        let term = self.term.lock();
        tail_to_string(term.grid(), n)
    }

    /// The newest rows that have scrolled off the visible screen into scrollback HISTORY, oldest
    /// first, up to `n`. Unlike [`tail`] (which reads the live screen where new chars land every
    /// frame), these rows are frozen in history and no longer reflow, so they read stably while an
    /// agent streams. Used by the hover tooltip on a busy tab to show the freshly-printed, settled
    /// output instead of a moving blur. Empty when nothing has scrolled off yet.
    pub fn history_slice(&self, n: usize) -> Vec<String> {
        let term = self.term.lock();
        let grid = term.grid();
        let cols = grid.columns();
        let hist = grid.history_size();
        let take = hist.min(n) as i64;
        // History rows are indexed GridLine(-k), k=1..=hist, where -1 is the newest (just above the
        // screen). Walk newest-first then reverse so the returned slice is oldest-first (natural
        // reading order), matching `tail`.
        let mut out = Vec::with_capacity(take as usize);
        for k in 1..=take {
            out.push(row_text(grid, -k as i32, cols));
        }
        out.reverse();
        out
    }

    /// Replay a previously-captured scrollback (from [`capture_scrollback`]) into a fresh session,
    /// reconstructing wrapping/color/cursor as if the pane had produced those bytes. Call right after
    /// construction, before the transport's live bytes arrive; the reconnect sweep then appends live
    /// output on top. The trailing newline writes a hard break so later captured output doesn't weld
    /// onto the last restored line.
    pub fn restore_history(&self, captured: &str) {
        if captured.is_empty() {
            return;
        }
        let mut term = self.term.lock();
        let mut parser: Processor<StdSyncHandler> = Processor::default();
        // Replay with CRLF, not bare LF: a bare `\n` moves down a line WITHOUT resetting the column,
        // so each replayed line would pick up a leading-space run. Real pane output is CRLF-safe;
        // this mirrors it so restored lines land left-aligned exactly as they were captured.
        let mut bytes = Vec::with_capacity(captured.len() + 8);
        for line in captured.split('\n') {
            bytes.extend_from_slice(line.as_bytes());
            bytes.extend_from_slice(b"\r\n");
        }
        parser.advance(&mut *term, &bytes);
    }
}

/// Capture the whole grid (scrollback + visible screen) as plain text, most-recent line last, with
/// hard logical-line boundaries: wrapped rows (last cell has WRAPLINE) rejoin with no newline, so
/// the output reads exactly as the user saw it scroll by. Trailing cell padding is stripped.
fn capture_grid_to_string(grid: &Grid<alacritty_terminal::term::cell::Cell>) -> String {
    let cols = grid.columns();
    let top = grid.topmost_line().0;
    let bottom = grid.bottommost_line().0;
    let mut out = String::new();
    let mut line = top;
    while line <= bottom {
        // WRAPLINE on a row means THIS row continues onto the next one (auto-wrapped): suppress the
        // newline after it so both rows rejoin into one logical line. A row without WRAPLINE ends a
        // logical line, so emit a hard newline after it (harmless even for the final row).
        let wrapped = grid[GridLine(line)][Column(cols.saturating_sub(1))]
            .flags
            .contains(Flags::WRAPLINE);
        let text = row_text(grid, line, cols);
        out.push_str(&text);
        if !wrapped {
            out.push('\n');
        }
        line += 1;
    }
    out
}

/// Newest visible screen rows first, up to `n`, as plain logical lines. Never walks history, so it
/// is cheap enough for a hover preview running while the pane is live. Each row keeps the terminal
/// width's leading spaces (from a partially-cleared line, say) but drops the grid's right-pad.
fn tail_to_string(grid: &Grid<alacritty_terminal::term::cell::Cell>, n: usize) -> Vec<String> {
    let cols = grid.columns();
    let mut rows = grid.screen_lines() as i64;
    let mut out = Vec::with_capacity(n);
    while rows > 0 && out.len() < n {
        out.push(row_text(grid, rows as i32 - 1, cols));
        rows -= 1;
    }
    out
}

/// Text of a single grid row. `Column(0)..cols` takes a full-cell slice; we strip trailing
/// whitespace so persisted lines don't carry the grid's right-pad.
fn row_text(grid: &Grid<alacritty_terminal::term::cell::Cell>, line: i32, cols: usize) -> String {
    let out = &grid[GridLine(line)][Column(0)..Column(cols)];
    let mut s = String::with_capacity(out.len());
    for cell in out {
        s.push(cell.c);
    }
    s.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A configured scrollback line limit is clamped to the safe ceiling (a typo can't balloon a
    /// pane into exhausting RAM), `0` stays 0, and normal values pass through unchanged.
    #[test]
    fn scrollback_lines_are_clamped_to_safe_ceiling() {
        assert_eq!(clamp_scrollback_lines(0), 0);
        assert_eq!(clamp_scrollback_lines(50000), 50000);
        assert_eq!(
            clamp_scrollback_lines(MAX_SCROLLBACK_LINES),
            MAX_SCROLLBACK_LINES
        );
        assert_eq!(clamp_scrollback_lines(1_000_000_000), MAX_SCROLLBACK_LINES);
        assert_eq!(clamp_scrollback_lines(usize::MAX), MAX_SCROLLBACK_LINES);
    }

    /// Typed bytes are echoed locally, then the identical copy returns over the link — the canceller
    /// must drop that returned copy while keeping genuine program output.
    #[test]
    fn cancels_returned_echo_and_keeps_output() {
        let c = EchoCanceller::default();
        // User types a command line; the transport will echo the exact same bytes back.
        c.note_echo(b"ls -la\r");
        // The link returns the pane's echo of what was typed (identical bytes)…
        let returned = c.filter_echo(b"ls -la\r");
        assert!(returned.is_empty(), "returned echo should be cancelled");
        // …then the program's own output follows — it must NOT be dropped just because its first
        // chars coincidentally match a pending byte.
        let out = c.filter_echo(b"total 8\ndrwxr-xr-x\n");
        assert_eq!(out, b"total 8\ndrwxr-xr-x\n");
    }

    /// A fragment split across chunks still cancels once reassembled in order.
    #[test]
    fn cancels_echo_split_across_chunks() {
        let c = EchoCanceller::default();
        c.note_echo(b"hello");
        assert!(c.filter_echo(b"he").is_empty());
        assert!(c.filter_echo(b"ll").is_empty());
        assert!(c.filter_echo(b"o").is_empty());
    }

    /// Bytes that were never echoed back stop being treated as pending once the window passes.
    #[test]
    fn pending_echo_expires_so_future_output_is_kept() {
        // Use a 0ms window via manual Default construction not possible (fields private, same module —
        // tests are a child module, so construct directly).
        let c = EchoCanceller {
            pending: Mutex::new(VecDeque::new()),
            window: Duration::from_millis(0),
        };
        c.note_echo(b"xxxxx");
        std::thread::sleep(Duration::from_millis(2));
        // The very same bytes returning now are no longer "expected echo" — they're real output.
        let out = c.filter_echo(b"xxxxx");
        assert_eq!(
            out, b"xxxxx",
            "expired pending echo must pass through as real output"
        );
    }

    /// A controllable fake transport whose writes are recorded and whose liveness the test toggles.
    /// Lives behind shared cells so the test can flip liveness and read what was written through the
    /// same `Box<dyn Transport>` the session holds.
    struct FakeTransport {
        alive: Arc<std::sync::atomic::AtomicBool>,
        writes: Arc<Mutex<Vec<u8>>>,
    }
    impl FakeTransport {
        fn new() -> (
            FakeTransport,
            Arc<std::sync::atomic::AtomicBool>,
            Arc<Mutex<Vec<u8>>>,
        ) {
            let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
            let writes = Arc::new(Mutex::new(Vec::new()));
            (
                FakeTransport {
                    alive: Arc::clone(&alive),
                    writes: Arc::clone(&writes),
                },
                alive,
                writes,
            )
        }
    }
    impl Transport for FakeTransport {
        fn kind(&self) -> &'static str {
            "fake"
        }
        fn write(&self, bytes: &[u8]) {
            self.writes.lock().unwrap().extend_from_slice(bytes);
        }
        fn resize(&self, _size: TermSize) {}
        fn alive(&self) -> bool {
            self.alive.load(Ordering::Relaxed)
        }
        fn reconnect(&mut self) -> io::Result<()> {
            self.alive.store(true, Ordering::Relaxed);
            Ok(())
        }
        fn destroy(&self) {}
    }

    fn fake_session(fake: FakeTransport) -> Session {
        let title = Arc::new(Mutex::new(None));
        let listener = Listener::with_title(Arc::clone(&title));
        let size = TermSize {
            lines: 24,
            cols: 80,
        };
        let term = Arc::new(FairMutex::new(Term::new(
            Config::default(),
            &size,
            listener,
        )));
        Session {
            meta: SessionMeta {
                host: "h".into(),
                engine: "e".into(),
                title: "e @ h".into(),
                name: None,
            },
            attach_session: None,
            term,
            transport: Box::new(fake),
            echo: None,
            title,
            bell: Arc::new(AtomicBool::new(false)),
            retry: Mutex::new(RetryState::new()),
            scrolled: Arc::new(AtomicBool::new(false)),
            pending: Mutex::new(Vec::new()),
            born: Instant::now(),
        }
    }

    /// Keystrokes typed while a pane is dead are buffered (not dropped) and flushed into the pane on
    /// the next successful reconnect.
    #[test]
    fn type_ahead_buffers_while_down_and_flushes_on_reconnect() {
        let (fake, alive, writes) = FakeTransport::new();
        let mut s = fake_session(fake);
        // Live tab: writes go straight through.
        s.write(b"ls\r");
        assert_eq!(*writes.lock().unwrap(), b"ls\r");
        assert_eq!(s.pending_bytes(), 0);

        // Kill the transport; further input buffers instead of vanishing.
        alive.store(false, Ordering::Relaxed);
        s.write(b"git pull\r ");
        s.write(b"&& make\r");
        assert_eq!(s.pending_bytes(), 18, "dead pane input buffers, not drops");
        assert_eq!(
            *writes.lock().unwrap(),
            b"ls\r",
            "nothing new reaches a dead pane"
        );

        // Bring the pane back; the queued command replays into it and the buffer clears.
        s.reconnect_now().unwrap();
        assert_eq!(
            *writes.lock().unwrap(),
            b"ls\rgit pull\r && make\r",
            "buffered keystrokes flush on reconnect"
        );
        assert_eq!(s.pending_bytes(), 0);
    }

    /// OSC window titles are captured into the shared slot and exposed via live_title; ResetTitle
    /// clears them again.
    #[test]
    fn captures_osc_title_and_reset() {
        use alacritty_terminal::event::Event as AEvent;

        let title = Arc::new(Mutex::new(None));
        let listener = Listener::with_title(Arc::clone(&title));
        listener.send_event(AEvent::Title("fixing auth".to_string()));
        assert_eq!(*title.lock().unwrap(), Some("fixing auth".to_string()));
        listener.send_event(AEvent::ResetTitle);
        assert!(
            title.lock().unwrap().is_none(),
            "ResetTitle should clear the slot"
        );
    }

    /// Scrollback capture returns the emulated lines in order, hard-wrapping preserved, so a
    /// captured snapshot can be replayed to a fresh emulator and reconstruct the same history.
    #[test]
    fn capture_returns_scrollback_text_in_order() {
        let size = TermSize {
            lines: 24,
            cols: 40,
        };
        let term = FairMutex::new(Term::new(Config::default(), &size, Listener::default()));
        {
            let mut p: Processor<StdSyncHandler> = Processor::default();
            // Three logical lines, the middle one long enough to wrap past the 40-col grid, so we
            // verify captured output still exposes the two wrapped rows as one logical line.
            p.advance(&mut *term.lock(), b"alpha\r\n");
            p.advance(&mut *term.lock(), b"beta-".repeat(10).as_slice());
            p.advance(&mut *term.lock(), b"gamma\r\nomega");
        }
        let captured = capture_grid_to_string(term.lock().grid());
        // The middle logical line (wrapped across two + rows) reappears as a single line.
        let lines: Vec<&str> = captured.lines().collect();
        assert!(
            lines.iter().any(|l| *l == "alpha"),
            "alpha present: {captured:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("beta-") && l.contains("gamma")),
            "wrapped beta+gamma merged: {captured:?}"
        );
        assert!(
            lines.iter().any(|l| *l == "omega"),
            "trailing line omega present: {captured:?}"
        );
    }

    /// Capture→restore round-trip: text captured from one emulator, replayed into a fresh one,
    /// yields the same visible text — proving a persisted snapshot re-hydrates history intact.
    #[test]
    fn captured_scrollback_restores_into_fresh_term() {
        let size = TermSize {
            lines: 24,
            cols: 40,
        };
        let make = || Term::new(Config::default(), &size, Listener::default());
        let t1 = FairMutex::new(make());
        {
            let mut p: Processor<StdSyncHandler> = Processor::default();
            p.advance(&mut *t1.lock(), b"line one\r\nline two\r\nline three");
        }
        let captured = capture_grid_to_string(t1.lock().grid());

        // A brand-new term (as a restored session would have) replays the captured text.
        let t2 = FairMutex::new(make());
        {
            use alacritty_terminal::vte::ansi::{Processor as P2, StdSyncHandler as S2};
            let mut p: P2<S2> = P2::default();
            // Same normalization `Session::restore_history` applies: CRLF, so bare-LF captured text
            // replays left-aligned rather than accumulating leading spaces.
            let mut bytes = Vec::with_capacity(captured.len() + 8);
            for line in captured.split('\n') {
                bytes.extend_from_slice(line.as_bytes());
                bytes.extend_from_slice(b"\r\n");
            }
            p.advance(&mut *t2.lock(), &bytes);
        }
        let recaptured = capture_grid_to_string(t2.lock().grid());
        // The replayed text survives a fresh capture (scroll may shift, but the three lines remain).
        let ls: Vec<&str> = recaptured.lines().collect();
        assert!(ls.iter().any(|l| *l == "line one"), "restored line one");
        assert!(ls.iter().any(|l| *l == "line two"), "restored line two");
        assert!(
            ls.iter().any(|l| l.starts_with("line three")),
            "restored line three: {recaptured:?}"
        );
    }

    /// The retry backoff ladder grows exponentially and caps at 60s, so a dead daemon is probed on
    /// a sane schedule (5s, 10s, 20s, 40s, 60s, 60s, …) rather than hammered every sweep tick.
    #[test]
    fn retry_backoff_ladder_caps_at_60() {
        assert_eq!(RetryState::backoff_seconds(0), 5);
        assert_eq!(RetryState::backoff_seconds(1), 10);
        assert_eq!(RetryState::backoff_seconds(2), 20);
        assert_eq!(RetryState::backoff_seconds(3), 40);
        assert_eq!(RetryState::backoff_seconds(4), 60);
        assert_eq!(RetryState::backoff_seconds(9), 60, "must cap, not overflow");
    }

    /// `tail` returns the newest visible rows first, never walking history, and strips the grid's
    /// right-padding so the preview reads as plain lines (not a fixed-width block).
    #[test]
    fn tail_returns_newest_screen_rows_first() {
        use alacritty_terminal::vte::ansi::Processor;

        let size = TermSize {
            lines: 24,
            cols: 40,
        };
        let term = FairMutex::new(Term::new(Config::default(), &size, Listener::default()));
        // Fill enough rows that some scroll into history, then leave three distinct screen lines.
        {
            let mut p: Processor<StdSyncHandler> = Processor::default();
            for i in 0..30 {
                p.advance(&mut *term.lock(), format!("row{i}\r\n").as_bytes());
            }
            p.advance(&mut *term.lock(), b"last-a\r\n");
            p.advance(&mut *term.lock(), b"last-b\r\n");
            p.advance(&mut *term.lock(), b"last-c");
        }
        let tail = tail_to_string(term.lock().grid(), 3);
        assert_eq!(tail, vec!["last-c", "last-b", "last-a"]);
    }

    /// The bell flag is set when the emulator fires the bell event, and `take_bell` is reset-on-read
    /// so a single ring badges exactly once (a second read must come back false).
    #[test]
    fn bell_flag_sets_on_bell_and_takes_once() {
        let listener = Listener::default();
        let bell = listener.bell_flag();
        assert!(!bell.load(Ordering::SeqCst), "starts clear");
        listener.send_event(Event::Bell);
        assert!(bell.load(Ordering::SeqCst), "bell event sets the flag");
        assert!(listener.bell_flag().load(Ordering::SeqCst), "clone sees it");

        // Simulate what a Session would do: take -> true once, then false.
        let take = || listener.bell_flag().swap(false, Ordering::SeqCst);
        assert!(take(), "first take returns true");
        assert!(!take(), "second take is false (reset-on-read)");
    }
}
