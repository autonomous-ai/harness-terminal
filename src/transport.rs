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

use crate::session::{EchoCanceller, Listener, TermSize};

/// A byte transport for one session. Sends keystrokes/resize outward; the concrete transport's own
/// reader thread feeds incoming bytes back into the shared `Term` grid.
pub trait Transport: Send {
    /// Stable transport kind, shown in the status line.
    fn kind(&self) -> &'static str;
    /// Push keystrokes into the transport.
    fn write(&self, bytes: &[u8]);
    /// Resize the underlying pane/PTY to match the TUI's terminal area.
    fn resize(&self, size: TermSize);
    /// Whether the underlying connection/pane is still alive. Default true, so transports that
    /// cannot drop (a local PTY) needn't opt in.
    fn alive(&self) -> bool {
        true
    }
    /// Re-establish a dropped connection. Default no-op for transports that can't drop.
    fn reconnect(&mut self) -> io::Result<()> {
        Ok(())
    }
    /// Kill the session's pane (and its underlying tmux session) so it stops consuming resources on
    /// its host. Default no-op for a local PTY, which already dies with its child.
    fn destroy(&self) {}
    /// The harness control port this transport reaches, if it's a tunnel transport. Lets the app
    /// persist/duplicate a non-default remote port instead of reconnecting to the default. None for
    /// local, tmux and ssh transports.
    fn port(&self) -> Option<u16> {
        None
    }
}

// ── local PTY (alacritty event loop) ────────────────────────────────────────────────────────────

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
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
        env_cwd: Option<String>,
        term: Arc<FairMutex<Term<Listener>>>,
    ) -> io::Result<LocalPtyTransport> {
        let wsize = WindowSize {
            num_lines: size.lines as u16,
            num_cols: size.cols as u16,
            cell_width: 0,
            cell_height: 0,
        };
        // Per-tab working directory: a diver's typed `dir:` in the new-session picker wins; when
        // they leave it blank (None/empty) we fall back to the config's `start_cwd` (when set) so a
        // diver who keeps a repo can hit `prefix+n` and land in it, not wherever the binary launched.
        let tty_cwd: Option<std::path::PathBuf> = match env_cwd {
            Some(cwd) if !cwd.trim().is_empty() => Some(cwd.into()),
            _ => crate::config::Config::load()
                .start_cwd
                .filter(|p| !p.is_empty())
                .map(Into::into),
        };
        let pty = tty::new(
            &tty::Options {
                shell: Some(tty::Shell::new(program.into(), args)),
                working_directory: tty_cwd,
                drain_on_exit: true,
                env: Default::default(),
            },
            wsize,
            /* window_id */ 0,
        )?;
        let event_loop = EventLoop::new(Arc::clone(&term), Listener::default(), pty, true, false)?;
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
    /// Spawn a local pane. No echo cancellation — a local link has no round-trip latency to smooth.
    pub fn spawn(
        program: &str,
        size: TermSize,
        term: Arc<FairMutex<Term<Listener>>>,
    ) -> io::Result<TmuxTransport> {
        let name = format!("auton-{}", program.replace('/', "-"));
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &name])
            .status();
        let pipe = ControlPipe::spawn(
            "tmux".to_string(),
            vec!["-C".to_string()],
            &name,
            program,
            size,
            term,
            None,
        )?;
        Ok(TmuxTransport { pipe })
    }
}

/// A tmux control-mode client split into a reader thread (parses `%output` → grid) and a writer
/// thread (keystrokes/resize → tmux stdin). The client is spawned from an arbitrary command line,
/// so the SAME protocol drives a local pane (`tmux -C`) or a remote pane (`ssh host tmux -C`).
struct ControlPipe {
    /// The control-mode command line (`argv0` + `argv`) this pipe was spawned with, so reconnect can
    /// re-run the same attach. For a local pane it is `tmux -C`; for a remote pane `ssh host tmux -C`.
    argv0: String,
    argv: Vec<String>,
    session: String,
    program: String,
    size: TermSize,
    /// The shared grid the reader thread replays `%output` into; kept so a reconnect can hand a
    /// fresh pipe a live view of the same `Term`.
    term: Arc<FairMutex<Term<Listener>>>,
    /// Echo cancellation for the reader thread (ssh/tunnel). Reconnect re-arms it on the fresh pipe.
    echo: Option<Arc<EchoCanceller>>,
    child: Child,
    /// Set false by the reader thread as soon as the control client's stdout closes — the pane
    /// (and thus the tab) is gone, so the watchdog knows to reconnect.
    alive: Arc<std::sync::atomic::AtomicBool>,
    tx: mpsc::Sender<Vec<u8>>,
}

impl ControlPipe {
    /// Spawn a control-mode client with `argv` (the control-mode command), create `session` running
    /// `program`, and stream its `%output` into `term`. A closure so [`ControlPipe::reconnect`] can
    /// rebuild a fresh pipe against the same `term`.
    fn spawn(
        argv0: String,
        argv: Vec<String>,
        session: &str,
        program: &str,
        size: TermSize,
        term: Arc<FairMutex<Term<Listener>>>,
        echo: Option<Arc<EchoCanceller>>,
    ) -> io::Result<ControlPipe> {
        let (child, tx, alive) = ControlPipe::build(
            &argv0,
            &argv,
            session,
            program,
            size,
            &term,
            echo.as_ref(),
            true,
            false,
        )?;
        Ok(ControlPipe {
            argv0,
            argv,
            session: session.to_string(),
            program: program.to_string(),
            size,
            term,
            echo,
            child,
            alive,
            tx,
        })
    }

    /// Create the session + pane on the control client's own stdin (no separate pre-spawn, so a
    /// remote hop doesn't need a second RTT). `recreate` keeps the fresh-spawn contract: kill any
    /// stale session of the same name first (ignoring failure) so a new attach can't trip tmux's
    /// "duplicate session". When `recreate` is false (reconnect) we DON'T kill — the whole point is
    /// to re-attach to a surviving remote pane after a blip, not to wipe its in-flight agent run.
    fn attach_cmds(session: &str, size: TermSize, program: &str, recreate: bool) -> Vec<String> {
        let mut cmds = Vec::new();
        if recreate {
            cmds.push(format!("kill-session -t {session}\n"));
            cmds.push(format!(
                "new-session -s {} -x {} -y {} {}\n",
                session, size.cols, size.lines, program
            ));
        } else {
            // -A re-attaches if the session exists, else creates it fresh; no kill, so an agent run
            // still alive on the host survives a dropped client link.
            cmds.push(format!(
                "new-session -A -s {} -x {} -y {} {}\n",
                session, size.cols, size.lines, program
            ));
        }
        cmds
    }

    /// Build a fresh control client + reader/writer threads against `term`, without owning the
    /// respawn identity. Shared by initial [`ControlPipe::spawn`] and [`ControlPipe::reconnect`].
    fn build(
        argv0: &str,
        argv: &[String],
        session: &str,
        program: &str,
        size: TermSize,
        term: &Arc<FairMutex<Term<Listener>>>,
        echo: Option<&Arc<EchoCanceller>>,
        recreate: bool,
        capture: bool,
    ) -> io::Result<(
        Child,
        mpsc::Sender<Vec<u8>>,
        Arc<std::sync::atomic::AtomicBool>,
    )> {
        let mut cmd = Command::new(argv0);
        cmd.args(argv)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd.spawn()?;
        let child_stdin = child.stdin.take().expect("control client stdin piped");
        let mut child_stdout = child.stdout.take().expect("control client stdout piped");

        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        for cmd in Self::attach_cmds(session, size, program, recreate) {
            tx.send(cmd.into_bytes()).ok();
        }
        // After a reconnect the grid is stale: request a full `capture-pane -ep` so the (surviving)
        // pane's current contents are replayed into the grid, instead of starting blank. The reply
        // arrives as raw escaped text inside a %begin/%end block (not %output lines), so the reader
        // accumulates it below and replays on `%end`.
        if capture {
            tx.send(b"capture-pane -ep\n".to_vec()).ok();
        }

        // Reader: parse control-mode notification lines; %output carries the pane's byte payload.
        // When the transport is remote, filter the returned bytes through echo cancellation before
        // advancing into the grid so typed text we optimistically echoed isn't double-rendered.
        let a = Arc::clone(&alive);
        let t = Arc::clone(term);
        let e = echo.cloned();
        thread::Builder::new()
            .name("tmux-read".into())
            .spawn(move || {
                let mut parser: Processor<StdSyncHandler> = Processor::default();
                let mut out = BufReader::new(&mut child_stdout);
                let mut line = String::new();
                // Raw text buffered inside a %begin/%end block (a capture-pane reply). Replayed once
                // at %end — ONCE because we clear it so live `%output` keeps flowing separately.
                let mut block: Vec<u8> = Vec::new();
                loop {
                    line.clear();
                    match out.read_line(&mut line) {
                        Ok(0) | Err(_) => {
                            a.store(false, std::sync::atomic::Ordering::Relaxed);
                            break;
                        }
                        Ok(_) => {
                            if line.starts_with("%begin") {
                                block = Vec::new();
                                continue;
                            }
                            if line.starts_with("%end") {
                                if !block.is_empty() {
                                    let payload = match &e {
                                        Some(c) => c.filter_echo(&block),
                                        None => block.clone(),
                                    };
                                    let mut term = t.lock();
                                    parser.advance(&mut *term, &payload);
                                }
                                block = Vec::new();
                                continue;
                            }
                            if let Some(payload) = parse_output(&line) {
                                let payload = match &e {
                                    Some(c) => c.filter_echo(&payload),
                                    None => payload,
                                };
                                let mut term = t.lock();
                                parser.advance(&mut *term, &payload);
                            } else if !block.is_empty() {
                                // Raw content inside a capture block: escaped, not %output-prefixed.
                                if let Some(decoded) = parse_escapes(line.trim_end_matches('\n')) {
                                    block.extend_from_slice(&decoded);
                                    block.push(b'\n');
                                }
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
        Ok((child, tx, alive))
    }

    /// Encode a keystroke buffer as `send-keys` commands (see [`encode_keys`]).
    fn write(&self, bytes: &[u8]) {
        let _ = self.tx.send(encode_keys(bytes).into_bytes());
    }

    /// Destroy the owned tmux session (killing whatever runs in it) and detach the local client.
    /// Sends `kill-session` directly to the pipe — for ssh these bytes go to the remote tmux, killing
    /// the pane there even though our client itself dies on drop.
    fn destroy(&self) {
        let _ = self
            .tx
            .send(format!("kill-session -t {}\n", self.session).into_bytes());
    }

    /// Re-attach after the client died: kill the old local/ssh client, spawn a fresh control client
    /// with the same identity, and stream it into the same grid. Unlike an initial spawn, no
    /// `kill-session` runs first — `new-session -A` re-attaches to the pane if the server session
    /// survived the blip (only creating one if it's truly gone), so a long agent run isn't wiped by a
    /// dropped link.
    fn reconnect(&mut self) -> io::Result<()> {
        let _ = self.child.kill();
        let (child, tx, alive) = ControlPipe::build(
            &self.argv0,
            &self.argv,
            &self.session,
            &self.program,
            self.size,
            &self.term,
            self.echo.as_ref(),
            false,
            true,
        )?;
        self.child = child;
        self.tx = tx;
        self.alive = alive;
        Ok(())
    }
}

/// Encode a keystroke buffer as tmux control-mode `send-keys` commands.
///
/// Typed bytes go to the pane as `send-keys -l '<text>'`. The pane gets a raw key press on CR/LF,
/// which we submit as a separate `send-keys Enter` command (the `-l` form types literally, so it
/// cannot carry key names). Tab becomes `send-keys Tab`. A trailing partial literal line is flushed
/// without an Enter so backspace-then-type still works. Shared by the local control-mode pipe and the
/// harness tunnel (the relay expects commands, not raw bytes).
fn encode_keys(bytes: &[u8]) -> String {
    let mut cmd = String::new();
    let mut literal = String::new();
    let flush = |cmd: &mut String, literal: &mut String, enter: bool| {
        if !literal.is_empty() {
            cmd.push_str("send-keys -l '");
            cmd.push_str(&escape_single_quote(&std::mem::take(literal)));
            cmd.push_str("'\n");
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
    cmd
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

    fn destroy(&self) {
        self.pipe.destroy();
    }

    fn resize(&self, size: TermSize) {
        let cmd = format!("resize-window -x {} -y {}\n", size.cols, size.lines);
        let _ = self.pipe.tx.send(cmd.into_bytes());
    }

    fn alive(&self) -> bool {
        self.pipe.alive.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn reconnect(&mut self) -> io::Result<()> {
        self.pipe.reconnect()
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
        echo: Arc<EchoCanceller>,
    ) -> io::Result<RemoteTransport> {
        let name = format!("auton-{}", program.replace('/', "-"));
        // The control-mode client runs ON the remote host via ssh; its stdin/stdout carry the
        // control-mode byte stream back through the (already-secured) ssh channel.
        let pipe = ControlPipe::spawn(
            "ssh".to_string(),
            vec![
                "-tt".to_string(), // force a tty so the remote tmux runs as an attached client
                "-o".to_string(),
                "StrictHostKeyChecking=accept-new".to_string(),
                host.to_string(),
                "tmux".to_string(),
                "-C".to_string(),
            ],
            &name,
            program,
            size,
            term,
            Some(echo),
        )?;
        Ok(RemoteTransport { pipe })
    }
}

impl Transport for RemoteTransport {
    fn kind(&self) -> &'static str {
        "ssh"
    }

    fn write(&self, bytes: &[u8]) {
        self.pipe.write(bytes);
    }

    fn destroy(&self) {
        self.pipe.destroy();
    }

    fn resize(&self, size: TermSize) {
        let cmd = format!("resize-window -x {} -y {}\n", size.cols, size.lines);
        let _ = self.pipe.tx.send(cmd.into_bytes());
    }

    fn alive(&self) -> bool {
        self.pipe.alive.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn reconnect(&mut self) -> io::Result<()> {
        self.pipe.reconnect()
    }
}

/// Tunnel-backed remote transport — ARCHITECTURE §10 path 1, now real.
///
/// The on-machine harness daemon (`harness`) exposes a raw pane-byte relay at
/// `ws://localhost:<port>/api/pane-ws` (added to `autonomous-harness` hookServer.ts): it owns a
/// `tmux -C` control-mode client and relays the pane's `%output` byte stream over the machine-ws
/// WebSocket. This transport drives that SAME control-mode protocol through the tunnel — a pane on a
/// joined machine is reached over the e2ee harness fabric instead of raw ssh. The `%output` decode →
/// `advance` path is byte-identical to the local tmux transport, proven against the live relay.
pub struct TunnelTransport {
    /// Where the pane-relay lives (the `@host` + its harness control port), so reconnect can find it.
    host: String,
    port: u16,
    program: String,
    /// The tmux session name this pane maps to on the remote host (`auton-<program>` for a fresh
    /// spawn, or the session the user asked to attach to). Kept so destroy/reconnect target the
    /// exact same session instead of re-deriving it.
    session: String,
    size: TermSize,
    /// The shared grid; a reconnect hands the fresh connection the same `Term`.
    term: Arc<FairMutex<Term<Listener>>>,
    /// Echo cancellation for the connection thread; reconnect re-arms it on the fresh connection.
    echo: Arc<EchoCanceller>,
    /// Set false by the connection thread the moment the WebSocket closes or errors.
    alive: Arc<std::sync::atomic::AtomicBool>,
    tx: mpsc::Sender<Vec<u8>>,
}

impl TunnelTransport {
    /// Attach to a fresh pane on the harness daemon at `host:port`, running `program`.
    pub fn spawn(
        host: &str,
        port: u16,
        program: &str,
        size: TermSize,
        term: Arc<FairMutex<Term<Listener>>>,
        echo: Arc<EchoCanceller>,
    ) -> io::Result<TunnelTransport> {
        let session = format!("auton-{}", program.replace('/', "-"));
        let (tx, alive) = TunnelTransport::build_connection(
            host, port, &session, program, size, &term, &echo, true, false,
        )?;
        Ok(TunnelTransport {
            host: host.to_string(),
            port,
            program: program.to_string(),
            session,
            size,
            term,
            echo,
            alive,
            tx,
        })
    }

    /// Attach to an EXISTING named tmux session on the harness daemon at `host:port`, without
    /// creating or killing anything. Uses `new-session -A` (attach-or-create) against the exact
    /// session the user named and a plain shell, and replays the pane's current screen so the grid
    /// isn't blank. This is how a diver resumes a session that's already running on a server.
    pub fn spawn_attach(
        host: &str,
        port: u16,
        session: &str,
        size: TermSize,
        term: Arc<FairMutex<Term<Listener>>>,
        echo: Arc<EchoCanceller>,
    ) -> io::Result<TunnelTransport> {
        let (tx, alive) = TunnelTransport::build_connection(
            host, port, session, "bash", size, &term, &echo, false, true,
        )?;
        Ok(TunnelTransport {
            host: host.to_string(),
            port,
            program: "bash".to_string(),
            session: session.to_string(),
            size,
            term,
            echo,
            alive,
            tx,
        })
    }

    /// Open one WebSocket to `/api/pane-ws`, create the pane, and spawn the connection thread.
    /// Shared by initial [`TunnelTransport::spawn`] and [`TunnelTransport::reconnect`]. `recreate`
    /// mirrors [`ControlPipe`] semantics: a fresh spawn kills any stale session first, a reconnect
    /// re-attaches (`new-session -A`) so a pane that survived the blip keeps its agent run. `capture`
    /// requests a `capture-pane -ep` on connect so a reconnect replays the pane's current screen
    /// (the grid is stale after a link drop).
    fn build_connection(
        host: &str,
        port: u16,
        session: &str,
        program: &str,
        size: TermSize,
        term: &Arc<FairMutex<Term<Listener>>>,
        echo: &Arc<EchoCanceller>,
        recreate: bool,
        capture: bool,
    ) -> io::Result<(mpsc::Sender<Vec<u8>>, Arc<std::sync::atomic::AtomicBool>)> {
        let name = session;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));

        // Build the connection by hand so the underlying socket can be set nonblocking before the
        // upgrade (tungstenite's `connect` owns the socket and gives no way in).
        //
        // NOTE on no-freeze: spawn/attach run synchronously on the UI thread, and a plain
        // `TcpStream::connect` has NO timeout — a remote address that silently drops packets (not
        // refusing) would block here for the OS's default multi-minute connect timeout, freezing
        // the whole terminal during a Remote-Attach. `connect_timeout` caps that at 4s while still
        // returning the error (so the in-app toast fires), instead of freezing the UI.
        let addr = format!("{host}:{port}");
        let tcp = std::net::ToSocketAddrs::to_socket_addrs(addr.as_str())
            .map_err(|e| io::Error::other(format!("tunnel resolve {addr}: {e}")))?
            .next()
            .ok_or_else(|| io::Error::other(format!("tunnel resolve {addr}: no address")))
            .and_then(|sa| {
                std::net::TcpStream::connect_timeout(&sa, std::time::Duration::from_secs(4))
            })
            .map_err(|e| io::Error::other(format!("tunnel connect {addr}: {e}")))?;
        let url = format!("ws://{host}:{port}/api/pane-ws");
        let request = tungstenite::client::IntoClientRequest::into_client_request(url)
            .map_err(|e| io::Error::other(e.to_string()))?;
        // Handshake BLOCKING first (a nonblocking socket makes the upgrade itself fail), then flip the
        // underlying stream to nonblocking so one connection can both read output and drain keystrokes.
        let (mut ws, _) = tungstenite::client::client(request, tcp)
            .map_err(|e| io::Error::other(format!("tunnel upgrade: {e}")))?;
        // With a raw TcpStream the upgrade produces a MaybeTlsStream wrapping it; set the underlying
        // socket nonblocking so the read/write loop below can service both directions on one connection.
        let _ = ws.get_ref().set_nonblocking(true);
        // Create the pane + start the program, mirroring ControlPipe's first on-stdin command. A
        // fresh spawn kills any stale session of the same name first so the pane starts clean; a
        // reconnect does NOT — it re-attaches to a pane that survived the blip.
        if recreate {
            ws.send(tungstenite::Message::Text(format!(
                "kill-session -t {name}\n"
            )))
            .ok();
            ws.send(tungstenite::Message::Text(format!(
                "new-session -s {} -x {} -y {} {}\n",
                name, size.cols, size.lines, program,
            )))
            .map_err(|e| io::Error::other(e.to_string()))?;
        } else {
            ws.send(tungstenite::Message::Text(format!(
                "new-session -A -s {} -x {} -y {} {}\n",
                name, size.cols, size.lines, program,
            )))
            .map_err(|e| io::Error::other(e.to_string()))?;
        }
        // Replay the existing pane's screen into the grid after a reconnect (the grid is stale).
        if capture {
            ws.send(tungstenite::Message::Text("capture-pane -ep\n".to_string()))
                .ok();
        }

        // One thread owns the single connection: read `%output` → grid, drain tx → send-keys. The
        // returned bytes are echo-cancelled (see EchoCanceller) so the optimistic echo isn't doubled.
        let a = Arc::clone(&alive);
        let t = Arc::clone(term);
        let e = Arc::clone(echo);
        thread::Builder::new()
            .name("tunnel".into())
            .spawn(move || {
                let mut parser: Processor<StdSyncHandler> = Processor::default();
                // Raw text buffered inside a %begin/%end block (a capture-pane reply), replayed once
                // at %end; cleared so live `%output` keeps flowing separately. Same shape as the
                // ControlPipe reader, so a re-attached tunnel repaints a stale grid too.
                let mut block: Vec<u8> = Vec::new();
                let mut feed = |line: &str, parser: &mut Processor<StdSyncHandler>| {
                    if line.starts_with("%begin") {
                        block = Vec::new();
                    } else if line.starts_with("%end") {
                        if !block.is_empty() {
                            let mut term = t.lock();
                            parser.advance(&mut *term, &e.filter_echo(&block));
                        }
                        block = Vec::new();
                    } else if let Some(payload) = parse_output(line) {
                        let mut term = t.lock();
                        parser.advance(&mut *term, &e.filter_echo(&payload));
                    } else if !block.is_empty() {
                        if let Some(decoded) = parse_escapes(line.trim_end_matches('\n')) {
                            block.extend_from_slice(&decoded);
                            block.push(b'\n');
                        }
                    }
                };
                loop {
                    match ws.read() {
                        Ok(tungstenite::Message::Text(s)) => {
                            for line in s.lines() {
                                feed(line, &mut parser);
                            }
                        }
                        Ok(tungstenite::Message::Binary(b)) => {
                            for line in String::from_utf8_lossy(&b).lines() {
                                feed(line, &mut parser);
                            }
                        }
                        Ok(_) => {}
                        // Nonblocking: nothing to read yet is not an error — keep draining keystrokes.
                        Err(tungstenite::Error::Io(ref e))
                            if e.kind() == io::ErrorKind::WouldBlock => {}
                        Err(_) => {
                            a.store(false, std::sync::atomic::Ordering::Relaxed);
                            break; // closed
                        }
                    }
                    while let Ok(bytes) = rx.try_recv() {
                        if let Ok(text) = String::from_utf8(bytes) {
                            let _ = ws.send(tungstenite::Message::Text(text));
                        }
                    }
                    thread::sleep(std::time::Duration::from_millis(2));
                }
            })?;

        Ok((tx, alive))
    }

    /// Re-attach after the WebSocket died: open a fresh connection to the same daemon/pane identity
    /// and stream it into the same grid. Re-attaches (`new-session -A`) so a pane that survived the
    /// blip keeps its agent run; only creates a fresh pane if the session is truly gone.
    fn reconnect(&mut self) -> io::Result<()> {
        let (tx, alive) = TunnelTransport::build_connection(
            &self.host,
            self.port,
            &self.session,
            &self.program,
            self.size,
            &self.term,
            &self.echo,
            false,
            true,
        )?;
        self.tx = tx;
        self.alive = alive;
        Ok(())
    }
}

impl Transport for TunnelTransport {
    fn kind(&self) -> &'static str {
        "tunnel"
    }
    fn port(&self) -> Option<u16> {
        Some(self.port)
    }

    fn write(&self, bytes: &[u8]) {
        // The relay forwards messages verbatim to its `tmux -C` client, which expects control-mode
        // commands — so keystrokes must be encoded as `send-keys` first (same as the local pipe).
        let _ = self.tx.send(encode_keys(bytes).into_bytes());
    }

    fn resize(&self, size: TermSize) {
        let _ = self
            .tx
            .send(format!("resize-window -x {} -y {}\n", size.cols, size.lines).into_bytes());
    }

    fn alive(&self) -> bool {
        self.alive.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn reconnect(&mut self) -> io::Result<()> {
        self.reconnect()
    }

    fn destroy(&self) {
        // Same kill-session the connection thread sends on spawn, targeted at this pane's session so
        // the relay's tmux tears the pane down. The daemon holds the session, not us, so this must go
        // over the wire rather than die with the socket.
        let name = &self.session;
        let _ = self
            .tx
            .send(format!("kill-session -t {name}\n").into_bytes());
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
    let rest = rest.split_once(' ')?.1; // pane id
    let data = rest.split_once(' ')?.1;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh spawn kills any stale session and creates a new one; a reconnect neither kills nor
    /// recreates a surviving pane — it re-attaches so the in-flight agent run isn't wiped by a blip.
    #[test]
    fn attach_cmds_recreate_vs_preserve() {
        let size = TermSize {
            cols: 80,
            lines: 24,
        };
        let fresh = ControlPipe::attach_cmds("auton-claude", size, "claude", true);
        assert_eq!(fresh.len(), 2, "fresh spawn kills + creates");
        assert!(fresh[0].starts_with("kill-session -t auton-claude"));
        assert!(fresh[1].starts_with("new-session -s auton-claude -x 80 -y 24 claude"));

        let resume = ControlPipe::attach_cmds("auton-claude", size, "claude", false);
        assert_eq!(resume.len(), 1, "reconnect only re-attaches");
        assert!(
            resume[0].starts_with("new-session -A -s auton-claude"),
            "must use -A to attach-if-exists: {}",
            resume[0]
        );
        assert!(
            !resume[0].contains("kill-session"),
            "never kill on reconnect"
        );
    }
}
