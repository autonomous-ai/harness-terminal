//! autonomous-term — terminal-first dive into a fleet of agent sessions.
//!
//! Foundation: a `Session` drives one terminal pane (the raw emulator) through
//! alacritty_terminal. Right now this runs a LOCAL process (bash). Later a remote
//! pane becomes the same `Session` with its PTY transport swapped for tmux control
//! mode over the harness e2ee tunnel — the emulator surface is identical.

use std::io::{self, Read};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, Msg};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty;

/// Terminal geometry — the project's reusable pane size type.
#[derive(Clone, Copy, Debug)]
struct TermSize {
    lines: usize,
    cols: usize,
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

/// Terminal event sink. Headless for now; the real client will drive a native
/// surface from `send_event(Event::Wakeup)` onwards.
#[derive(Clone, Default)]
struct Listener;

impl EventListener for Listener {
    fn send_event(&self, _event: Event) {}
}

fn main() -> io::Result<()> {
    let size = TermSize { lines: 24, cols: 80 };

    // Raw emulator surface — the borrowed layer. A pane's bytes get parsed into
    // this screen, identical whether the pane is local or remote.
    let terminal = std::sync::Arc::new(FairMutex::new(Term::new(
        Config::default(),
        &size,
        Listener,
    )));

    // Local PTY running bash. Later: a remote pane's transport replaces this.
    let pty = tty::new(
        &tty::Options {
            shell: Some(tty::Shell::new("bash".into(), Vec::new())),
            working_directory: None,
            drain_on_exit: true,
            env: Default::default(),
        },
        WindowSize {
            num_lines: size.lines as u16,
            num_cols: size.cols as u16,
            cell_width: 0,
            cell_height: 0,
        },
        0,
    )?;

    // Wire the PTY reader → escape parser → Term surface.
    let event_loop = EventLoop::new(
        std::sync::Arc::clone(&terminal),
        Listener,
        pty,
        true,
        false,
    )?;
    let writer = event_loop.channel();
    let _handle = event_loop.spawn();

    println!("autonomous-term: local session started (bash, {}x{}).", size.lines, size.cols);
    println!("Type `exit` to quit.");

    // Pump stdin → PTY. Textecho to a real surface comes with the client shell.
    let mut stdin = io::stdin();
    let mut buf = [0u8; 256];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => break, // EOF
            Ok(n) => writer.send(Msg::Input(buf[..n].to_vec().into())).expect("send input"),
            Err(_) => break,
        }
    }
    Ok(())
}
