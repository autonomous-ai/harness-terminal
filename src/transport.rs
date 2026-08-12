//! Transports — the byte source backing a `Session`'s terminal grid.
//!
//! `TAB = SESSION = PANE@HOST`. Every transport drains raw bytes into the same alacritty `Term`
//! grid (via `vte::ansi::Processor::advance`) and accepts keystrokes back out. All three share one
//! control-mode protocol (`ControlPipe`); only the spawned command differs:
//!
//! - `LocalPtyTransport`: a real PTY running a shell/engine CLI, using alacritty's own event loop.
//! - `TmuxTransport`: a real LOCAL tmux pane driven through control mode (`tmux -C`). This is the
//!   literal `pane@host` for the current machine.
//! - `RemoteTransport`: a pane on ANOTHER machine, reached via `ssh <host> tmux -C`. Same `%output`
//!   decode → `advance`; only the byte source crosses the network. This is `pane@host` across the
//!   fleet (machines reachable by ssh; the harness `machine-ws` tunnel can back the same hop later).

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
/// stdin. Drop kills the tmux client.
pub struct TmuxTransport {
    pipe: ControlPipe,
}

impl TmuxTransport {
    /// Spawn a control-mode tmux pane running `program` in a fresh, uniquely-named LOCAL session.
    pub fn spawn(program: &str, size: TermSize, term: Arc<FairMutex<Term<Listener>>>) -> io::Result<TmuxTransport> {
        let name = format!("auton-{}", program.replace('/', "-"));
        let _ = Command::new("tmux").args(["kill-session", "-t", &name]).status();
        let pipe = ControlPipe::spawn(
            "tmux".to_string(),
            vec!["-C".to_string()],
            &name,
            program,
            size,
            term,
        )?;
        Ok(TmuxTransport { pipe })
    }
}

/// A tmux control-mode client split into a reader thread (parses `%output` → grid) and a writer
/// thread (keystrokes/resize → tmux stdin). The client is spawned from an arbitrary command line,
/// so the SAME protocol drives a local pane (`tmux -C`) or a remote pane (`ssh host tmux -C`).
struct ControlPipe {
    child: Child,
    tx: mpsc::Sender<Vec<u8>>,
}

impl ControlPipe {
    /// Spawn a control-mode client with `argv` (the control-mode command), create `session` running
    /// `program`, and stream its `%output` into `term`.
    fn spawn(
        argv0: String,
        argv: Vec<String>,
        session: &str,
        program: &str,
        size: TermSize,
        term: Arc<FairMutex<Term<Listener>>>,
    ) -> io::Result<ControlPipe> {
        let mut cmd = Command::new(&argv0);
        cmd.args(&argv)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd.spawn()?;
        let child_stdin = child.stdin.take().expect("control client stdin piped");
        let mut child_stdout = child.stdout.take().expect("control client stdout piped");

        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        // Create the session + pane on the control client's own stdin (no separate pre-spawn, so a
        // remote hop doesn't need a second RTT).
        tx.send(format!(
            "new-session -s {} -x {} -y {} {}\n",
            session, size.cols, size.lines, program
        ).into_bytes()).ok();

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

        // Writer thread: drain keystrokes (and resize commands) into the client's stdin.
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

        Ok(ControlPipe { child, tx })
    }

    /// Encode a keystroke buffer as `send-keys` commands (see [`ControlPipe::write`]).
    fn write(&self, bytes: &[u8]) {
        // In control mode, typed bytes go to the pane as `send-keys -l '<text>'`. The pane gets a
        // raw key press on CR/LF, which we submit as a separate `send-keys Enter` command (the -l
        // form types literally, so it cannot carry key names). A trailing partial literal line is
        // flushed without an Enter so backspace-then-type still works.
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
                    flush(&mut cmd, &mut literal, false);
                    cmd.push_str("send-keys Tab\n");
                }
                _ => literal.push(b as char),
            }
        }
        flush(&mut cmd, &mut literal, false);
        let _ = self.tx.send(cmd.into_bytes());
    }
}

impl Drop for ControlPipe {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

impl Transport for TmuxTransport {
    fn kind(&self) -> &'static str {
        "tmux"
    }

    fn write(&self, bytes: &[u8]) {
        self.pipe.write(bytes);
    }

    fn resize(&self, size: TermSize) {
        let cmd = format!("resize-window -x {} -y {}\n", size.cols, size.lines);
        let _ = self.pipe.tx.send(cmd.into_bytes());
    }
}

/// A remote `PANE@HOST`: the same control-mode protocol, but the client runs over ssh so the pane
/// lives (and is attachable) on another joined machine. `host` is the `@host` half of `pane@host`.
pub struct RemoteTransport {
    pipe: ControlPipe,
}

impl RemoteTransport {
    /// Create a fresh remote session running `program` on `host`, then attach to it.
    pub fn spawn(
        host: &str,
        program: &str,
        size: TermSize,
        term: Arc<FairMutex<Term<Listener>>>,
    ) -> io::Result<RemoteTransport> {
        let name = format!("auton-{}", program.replace('/', "-"));
        // The control-mode client runs ON the remote host via ssh; its stdin/stdout carry the
        // control-mode byte stream back through the (already-secured) ssh channel.
        let pipe = ControlPipe::spawn(
            "ssh".to_string(),
            vec![
                "-tt".to_string(), // force a tty so the remote tmux runs as an attached client
                "-o".to_string(), "StrictHostKeyChecking=accept-new".to_string(),
                host.to_string(),
                "tmux".to_string(),
                "-C".to_string(),
            ],
            &name,
            program,
            size,
            term,
        )?;
        Ok(RemoteTransport { pipe })
    }
}

impl Transport for RemoteTransport {
    fn kind(&self) -> &'static str {
        "remote"
    }

    fn write(&self, bytes: &[u8]) {
        self.pipe.write(bytes);
    }

    fn resize(&self, size: TermSize) {
        let cmd = format!("resize-window -x {} -y {}\n", size.cols, size.lines);
        let _ = self.pipe.tx.send(cmd.into_bytes());
    }
}

/// Escape a single quote for embedding inside a tmux `send-keys -l '...'` value.
fn escape_single_quote(s: &str) -> String {
    s.replace('\'', "\\'")
}

/// Candidate hosts for remote attach, in display order. Reads `~/.ssh/config` `Host` entries
/// (excluding wildcard/`*` stanzas) plus the literal aliases localhost/this-host. This is the
/// fleet's ssh-reachable surface; the harness `machine-ws` tunnel can add more later.
pub fn discover_hosts() -> Vec<String> {
    use std::fs;
    let mut hosts: Vec<String> = Vec::new();
    if let Ok(contents) = fs::read_to_string(dirs_home().join(".ssh/config")) {
        for line in contents.lines() {
            let line = line.trim();
            // "Host foo bar" — take each whitespace-separated token.
            if let Some(rest) = line.strip_prefix("Host") {
                for tok in rest.split_whitespace() {
                    if tok.contains('*') || tok.contains('?') {
                        continue;
                    }
                    if !hosts.contains(&tok.to_string()) {
                        hosts.push(tok.to_string());
                    }
                }
            }
        }
    }
    for local in ["localhost", "this-host"] {
        if !hosts.contains(&local.to_string()) {
            hosts.push(local.to_string());
        }
    }
    hosts
}

fn dirs_home() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
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

