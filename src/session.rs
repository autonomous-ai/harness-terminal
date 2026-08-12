//! A session — the atomic unit of the terminal. One `Term` emulator surface fed by one transport.
//!
//! `TAB = SESSION = PANE@HOST`. Each session owns its own emulator (grid + parser) and its own
//! PTY/transport. The same `Session` struct handles a local shell and (later) a remote tmux pane —
//! only the backing source of bytes differs.

use std::io;
use std::sync::Arc;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty;

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

/// A single terminal session: emulator surface + event-loop sender for I/O.
pub struct Session {
    pub meta: SessionMeta,
    pub term: Arc<FairMutex<Term<Listener>>>,
    input: EventLoopSender,
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

        let wsize = WindowSize {
            num_lines: size.lines as u16,
            num_cols: size.cols as u16,
            cell_width: 0,
            cell_height: 0,
        };
        // Local PTY. (Later: a remote transport swaps this for the tunnel-backed PTY.)
        let pty = tty::new(
            &tty::Options {
                shell: Some(tty::Shell::new(program.into(), args)),
                working_directory: None,
                drain_on_exit: true,
                env: Default::default(),
            },
            wsize,
            /* window_id */ 0,
        )?;

        // Event loop owns the PTY; we keep the sender to push input + resize.
        let event_loop = EventLoop::new(Arc::clone(&term), Listener, pty, true, false)?;
        let input = event_loop.channel();
        let _handle = event_loop.spawn();

        Ok(Session { meta, term, input })
    }

    /// Push keystrokes into the session's transport.
    pub fn write(&self, bytes: &[u8]) {
        let _ = self.input.send(Msg::Input(bytes.to_vec().into()));
    }

    /// Resize the session's screen + underlying PTY.
    pub fn resize(&self, size: TermSize) {
        self.term.lock().resize(size);
        let _ = self.input.send(Msg::Resize(WindowSize {
            num_lines: size.lines as u16,
            num_cols: size.cols as u16,
            cell_width: 0,
            cell_height: 0,
        }));
    }
}
