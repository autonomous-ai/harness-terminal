//! A session — the atomic unit of the terminal. One `Term` emulator surface fed by one transport.
//!
//! `TAB = SESSION = PANE@HOST`. Each session owns its own emulator (grid + parser) and its own
//! PTY/transport. The same `Session` struct handles a local shell and (later) a remote tmux pane —
//! only the backing source of bytes differs.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

use crate::transport::{LocalPtyTransport, Transport};

/// Reusable pane geometry.
#[derive(Clone, Copy, Debug)]
pub struct TermSize {
    pub lines: usize,
    pub cols: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize { self.lines }
    fn screen_lines(&self) -> usize { self.lines }
    fn columns(&self) -> usize { self.cols }
}

/// Sink for terminal render events. Rendering is drawn synchronously from the grid, so wakeups
/// are currently no-ops; a future dirty-region/GPU client can hook `Event::Wakeup` here. We do act
/// on `ClipboardStore`, which the alacritty emulator issues for an OSC 52 write (e.g. an agent's
/// `pbcopy`/`wl-copy` over the pane) — the data is copied to the system clipboard.
#[derive(Clone, Default)]
pub struct Listener;

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        if let Event::ClipboardStore(_, text) = event {
            // Best-effort: a missing/wrong clipboard backend must not break terminal I/O.
            if let Ok(mut cb) = arboard::Clipboard::new() {
                let _ = cb.set_text(text);
            }
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
        EchoCanceller { pending: Mutex::new(VecDeque::new()), window: Duration::from_millis(1500) }
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

/// Who owns this session (host · engine · title).
#[derive(Clone, Debug)]
pub struct SessionMeta {
    pub host: String,
    pub engine: String,
    pub title: String,
}

/// A single terminal session: emulator surface + transport for I/O.
pub struct Session {
    pub meta: SessionMeta,
    pub term: Arc<FairMutex<Term<Listener>>>,
    transport: Box<dyn Transport>,
    /// Echo cancellation for remote-byte transports (ssh/tunnel); None for local ones.
    echo: Option<Arc<EchoCanceller>>,
}

impl Session {
    /// Create a session running a LOCAL program (shell or an engine CLI) in a fresh PTY.
    pub fn local(
        meta: SessionMeta,
        program: &str,
        args: Vec<String>,
        size: TermSize,
    ) -> io::Result<Session> {
        let term = Arc::new(FairMutex::new(Term::new(Config::default(), &size, Listener)));
        let transport = LocalPtyTransport::spawn(program, args, size, Arc::clone(&term))?;
        Ok(Session { meta, term, transport: Box::new(transport), echo: None })
    }

    /// Create a session backed by a real tmux pane (control mode).
    pub fn tmux(
        meta: SessionMeta,
        program: &str,
        size: TermSize,
    ) -> io::Result<Session> {
        let term = Arc::new(FairMutex::new(Term::new(Config::default(), &size, Listener)));
        let transport = crate::transport::TmuxTransport::spawn(program, size, Arc::clone(&term))?;
        Ok(Session { meta, term, transport: Box::new(transport), echo: None })
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
        let term = Arc::new(FairMutex::new(Term::new(Config::default(), &size, Listener)));
        // The tunnel crosses a latency link, so the session owns an echo canceller (Session::write
        // notes keystrokes; the transport's reader thread cancels the returned copy).
        let echo = Arc::new(EchoCanceller::default());
        let transport = crate::transport::TunnelTransport::spawn(
            host, port, program, size, Arc::clone(&term), Arc::clone(&echo),
        )?;
        Ok(Session { meta, term, transport: Box::new(transport), echo: Some(echo) })
    }

    /// Create a session whose pane lives on REMOTE host `host` (via ssh + tmux control mode).
    /// `meta.host` carries the `@host` half of `pane@host`.
    pub fn remote(
        meta: SessionMeta,
        program: &str,
        size: TermSize,
    ) -> io::Result<Session> {
        let term = Arc::new(FairMutex::new(Term::new(Config::default(), &size, Listener)));
        // Remote ssh crosses a latency link — same echo-cancellation setup as the tunnel.
        let echo = Arc::new(EchoCanceller::default());
        let transport = crate::transport::RemoteTransport::spawn(
            &meta.host, program, size, Arc::clone(&term), Arc::clone(&echo),
        )?;
        Ok(Session { meta, term, transport: Box::new(transport), echo: Some(echo) })
    }

    /// Transport kind: "pty" / "tmux" / "ssh" / "tunnel" (shown in the status line).
    pub fn kind(&self) -> &'static str {
        self.transport.kind()
    }

    /// Whether the session's transport is still live. Local PTYs are always alive; tmux/ssh/tunnel
    /// transports report false once their connection or pane dies.
    pub fn alive(&self) -> bool {
        self.transport.alive()
    }

    /// Re-attach after a dropped connection. Local PTYs are a no-op. Returns an error only if the
    /// immediate re-spawn fails (e.g. tunnel daemon unreachable again) — callers retry later.
    pub fn reconnect(&mut self) -> io::Result<()> {
        self.transport.reconnect()
    }

    /// Push keystrokes into the session's transport.
    pub fn write(&self, bytes: &[u8]) {
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(out, b"xxxxx", "expired pending echo must pass through as real output");
    }
}
