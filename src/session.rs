//! A session — the atomic unit of the terminal. One `Term` emulator surface fed by one transport.
//!
//! `TAB = SESSION = PANE@HOST`. Each session owns its own emulator (grid + parser) and its own
//! PTY/transport. The same `Session` struct handles a local shell and (later) a remote tmux pane —
//! only the backing source of bytes differs.

use std::io;
use std::sync::Arc;

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
/// are currently no-ops; a future dirty-region/GPU client can hook `Event::Wakeup` here.
#[derive(Clone, Default)]
pub struct Listener;

impl EventListener for Listener {
    fn send_event(&self, _event: Event) {}
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
        Ok(Session { meta, term, transport: Box::new(transport) })
    }

    /// Create a session backed by a real tmux pane (control mode).
    pub fn tmux(
        meta: SessionMeta,
        program: &str,
        size: TermSize,
    ) -> io::Result<Session> {
        let term = Arc::new(FairMutex::new(Term::new(Config::default(), &size, Listener)));
        let transport = crate::transport::TmuxTransport::spawn(program, size, Arc::clone(&term))?;
        Ok(Session { meta, term, transport: Box::new(transport) })
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
        let transport = crate::transport::TunnelTransport::spawn(host, port, program, size, Arc::clone(&term))?;
        Ok(Session { meta, term, transport: Box::new(transport) })
    }

    /// Create a session whose pane lives on REMOTE host `host` (via ssh + tmux control mode).
    /// `meta.host` carries the `@host` half of `pane@host`.
    pub fn remote(
        meta: SessionMeta,
        program: &str,
        size: TermSize,
    ) -> io::Result<Session> {
        let term = Arc::new(FairMutex::new(Term::new(Config::default(), &size, Listener)));
        let transport = crate::transport::RemoteTransport::spawn(&meta.host, program, size, Arc::clone(&term))?;
        Ok(Session { meta, term, transport: Box::new(transport) })
    }

    /// Transport kind: "pty" or "tmux" (shown in the status line).
    pub fn kind(&self) -> &'static str {
        self.transport.kind()
    }

    /// Push keystrokes into the session's transport.
    pub fn write(&self, bytes: &[u8]) {
        self.transport.write(bytes);
        // Local echo: for a remote session the pane's own bytes travel back with round-trip latency,
        // so mirror the keystrokes into the grid here to make typing feel instant. The real echo from
        // the pane overwrites these (they are the same chars + any app-driven prompt echo). Purely an
        // optimistic render — never affects the actual transport bytes.
        if self.transport.kind() == "ssh" || self.transport.kind() == "tunnel" {
            let mut term = self.term.lock();
            let mut parser: Processor<StdSyncHandler> = Processor::default();
            parser.advance(&mut *term, bytes);
        }
    }

    /// Resize the session's screen + underlying transport.
    pub fn resize(&self, size: TermSize) {
        self.term.lock().resize(size);
        self.transport.resize(size);
    }
}
