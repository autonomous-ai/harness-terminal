//! Transports — the byte source backing a `Session`'s terminal grid.
//!
//! `TAB = SESSION = PANE@HOST`. Every transport drains raw bytes into the same alacritty `Term`
//! grid (via `vte::ansi::Processor::advance`) and accepts keystrokes back out. Two shapes today:
//!
//! - `LocalPtyTransport`: a real PTY running a shell/engine CLI, using alacritty's own event loop.
//! - `TmuxTransport`: a real tmux pane driven through tmux control mode (`tmux -C`). The pane
//!   emits `%output` notifications, which we replay into the grid; keystrokes go to the control
//!   client's stdin. This is the target: one pane per session, visible and attachable on the host.
//!
//! A future `HarnessTransport` will source bytes from the harness e2ee tunnel instead of a local
//! pane — same trait, same grid.

use std::io;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

use crate::session::{Listener, TermSize};

/// A byte transport for one session. Sends keystrokes/resize outward; the concrete transport's own
/// reader thread feeds incoming bytes back into the shared `Term` grid.
pub trait Transport: Send {
    /// Stable transport kind, shown in the status line.
    fn kind(&self) -> &'static str;
    /// Push keystrokes into the transport.
    fn write(&self, bytes: &[u8]);
    /// Resize the underlying pane/PTY to match the TUI's terminal area.
    fn resize(&self, size: TermSize);
}

// ── local PTY (alacritty event loop) ────────────────────────────────────────────────────────────

use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::event::WindowSize;
use alacritty_terminal::tty;

/// A real local PTY running a shell or engine CLI. Backed by alacritty's own event-loop thread,
/// which owns the PTY read side and parses bytes directly into the grid.
pub struct LocalPtyTransport {
    sender: EventLoopSender,
}

impl LocalPtyTransport {
    pub fn spawn(
        program: &str,
        args: Vec<String>,
        size: TermSize,
        term: Arc<FairMutex<Term<Listener>>>,
    ) -> io::Result<LocalPtyTransport> {
        let wsize = WindowSize {
            num_lines: size.lines as u16,
            num_cols: size.cols as u16,
            cell_width: 0,
            cell_height: 0,
        };
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
        let event_loop = EventLoop::new(Arc::clone(&term), Listener, pty, true, false)?;
        let sender = event_loop.channel();
        let _handle = event_loop.spawn();
        Ok(LocalPtyTransport { sender })
    }
}

impl Transport for LocalPtyTransport {
    fn kind(&self) -> &'static str {
        "pty"
    }

    fn write(&self, bytes: &[u8]) {
        let _ = self.sender.send(Msg::Input(bytes.to_vec().into()));
    }

    fn resize(&self, size: TermSize) {
        let _ = self.sender.send(Msg::Resize(WindowSize {
            num_lines: size.lines as u16,
            num_cols: size.cols as u16,
            cell_width: 0,
            cell_height: 0,
        }));
    }
}

// ── tmux control-mode pane ─────────────────────────────────────────────────────────────────────

use std::io::{BufRead, BufReader, Write};
use std::sync::mpsc;
use std::thread;

/// A real tmux pane driven through control mode. A reader thread consumes tmux's `%output`
/// notifications and replays them into the shared grid; a channel carries keystrokes to tmux's
/// stdin. `close_tab` is expressed by sending `/` no—by killing the tmux client.
pub struct TmuxTransport {
    child: Child,
    tx: mpsc::Sender<Vec<u8>>,
}

impl TmuxTransport {
    /// Spawn a dedicated tmux session + pane running `program`, then control-mode into it.
    pub fn spawn(program: &str, size: TermSize, term: Arc<FairMutex<Term<Listener>>>) -> io::Result<TmuxTransport> {
        // A fresh, uniquely-named tmux session with one pane. We run a bare `tmux -C` (control
        // mode) and issue `new-session` on its stdin: that creates the session, makes this client
        // the attached pane, and streams `%output` notifications back on stdout. No separate
        // pre-creation — a second `-t` target would spawn a duplicate session.
        let name = format!("auton-{}", program.replace('/', "-"));
        let _ = Command::new("tmux").args(["kill-session", "-t", &name]).status();

        let mut child = Command::new("tmux")
            .arg("-C")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let child_stdin = child.stdin.take().expect("tmux stdin piped");
        let mut child_stdout = child.stdout.take().expect("tmux stdout piped");

        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        tx.send(format!("new-session -s {} -x {} -y {} {}\n", name, size.cols, size.lines, program).into_bytes()).ok();

        // Reader: parse control-mode notification lines; %output carries the pane's byte payload.
        let t = Arc::clone(&term);
        thread::Builder::new()
            .name("tmux-read".into())
            .spawn(move || {
                let mut parser: Processor<StdSyncHandler> = Processor::default();
                let mut out = BufReader::new(&mut child_stdout);
                let mut line = String::new();
                loop {
                    line.clear();
                    match out.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            if let Some(payload) = parse_output(&line) {
                                let mut term = t.lock();
                                parser.advance(&mut *term, &payload);
                            }
                        }
                    }
                }
            })?;

        // Writer thread: drain keystrokes (and resize commands) into tmux's control-client stdin.
        // The stdin handle is moved in here; no other writer exists.
        let mut w = child_stdin;
        thread::Builder::new()
            .name("tmux-write".into())
            .spawn(move || {
                while let Ok(bytes) = rx.recv() {
                    if w.write_all(&bytes).is_err() {
                        break;
                    }
                    let _ = w.flush();
                }
            })?;

        Ok(TmuxTransport { child, tx })
    }
}

/// Escape a single quote for embedding inside a tmux `send-keys -l '...'` value.
fn escape_single_quote(s: &str) -> String {
    s.replace('\'', "\\'")
}

/// Extract the byte payload from a `%output` control-notification line, or None if it isn't one.
fn parse_output(line: &str) -> Option<Vec<u8>> {
    // Format:  %output <pane> <data>
    // Data is space-separated escapes: \e for ESC, \n for LF, \t for TAB, \uXXXX for others.
    let rest = line.strip_prefix("%output")?;
    let rest = rest.splitn(2, ' ').nth(1)?; // pane id
    let data = rest.splitn(2, ' ').nth(1)?;
    parse_escapes(data.trim_end())
}

/// Decode tmux's control-mode escape encoding. tmux escapes non-printable bytes as octal (`\015`),
/// plus `\\` for a literal backslash. Printable bytes pass through unchanged.
fn parse_escapes(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            if b[i + 1] == b'\\' {
                out.push(b'\\');
                i += 2;
            } else if i + 3 < b.len() && b[i + 1].is_ascii_digit() {
                // \NNN — three octal digits → one byte.
                let oct = &s[i + 1..i + 4];
                if let Ok(v) = u8::from_str_radix(oct, 8) {
                    out.push(v);
                    i += 4;
                } else {
                    out.push(b'\\');
                    i += 1;
                }
            } else {
                // Unknown escape — keep the backslash literally.
                out.push(b'\\');
                i += 1;
            }
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(out)
}

impl Drop for TmuxTransport {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

impl Transport for TmuxTransport {
    fn kind(&self) -> &'static str {
        "tmux"
    }

    fn write(&self, bytes: &[u8]) {
        // In control mode, typed bytes go to the pane as `send-keys -l '<text>'`. The pane gets a
        // raw key press on CR/LF, which we submit as a separate `send-keys Enter` command (the -l
        // form types literally, so it cannot carry key names). We flush an incomplete trailing
        // literal line without an Enter so backspace-then-type still works.
        let mut cmd = String::new();
        let mut literal = String::new();
        let flush = |cmd: &mut String, literal: &mut String, enter: bool| {
            if !literal.is_empty() {
                cmd.push_str("send-keys -l '");
                cmd.push_str(&escape_single_quote(literal));
                cmd.push_str("'\n");
                literal.clear();
            }
            if enter {
                cmd.push_str("send-keys Enter\n");
            }
        };
        for &b in bytes {
            match b {
                b'\r' | b'\n' => flush(&mut cmd, &mut literal, true),
                b'\t' => {
                    // A raw Tab press: flush current literal, send Tab key.
                    flush(&mut cmd, &mut literal, false);
                    cmd.push_str("send-keys Tab\n");
                }
                _ => literal.push(b as char),
            }
        }
        // Trailing partial literal (no Enter) — sends an incomplete command line so tmux holds it.
        flush(&mut cmd, &mut literal, false);
        let _ = self.tx.send(cmd.into_bytes());
    }

    fn resize(&self, size: TermSize) {
        // Control-mode clients accept tmux commands as text on their stdin; resize the window's
        // pane to match the TUI area. Rounded to tmux's character grid.
        let cmd = format!("resize-window -x {} -y {}\n", size.cols, size.lines);
        let _ = self.tx.send(cmd.into_bytes());
    }
}
