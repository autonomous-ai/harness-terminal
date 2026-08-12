//! autonomous-term — a terminal-first dive into a fleet of AI agent sessions.
//!
//! Keyboard-first TUI: `prefix + /` opens the session palette, `prefix + n` opens the engine
//! picker, `prefix + q` quits. The active tab is a live alacritty Term; the tab bar / palette /
//! status are ratatui chrome. `TAB = SESSION = PANE@HOST`.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event as CTEvent, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use autonomous_term::app::{App, Overlay};
use autonomous_term::session::TermSize;
use autonomous_term::tui;

/// The prefix key (like tmux's C-b). Hold it to enter "command mode".
const PREFIX: KeyCode = KeyCode::Char(' ');

fn main() -> io::Result<()> {
    // Raw-mode terminal for crossterm event capture.
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    // Start with a local session so the UI is immediately alive.
    let mut app = App::new(TermSize { lines: 24, cols: 80 });
    app.spawn_local("this-host", "shell");

    // Key state: whether we're currently "inside" after pressing prefix.
    let mut in_command = false;

    loop {
        tui::draw(&mut term, &mut app);
        if event::poll(Duration::from_millis(16))? {
            if let CTEvent::Key(key) = event::read()? {
                if handle_key(&mut app, key, &mut in_command) {
                    break;
                }
            }
        }
    }

    // Teardown.
    crossterm::terminal::disable_raw_mode()?;
    execute!(term.backend_mut(), crossterm::terminal::LeaveAlternateScreen)?;
    Ok(())
}

/// Handle one key event. Returns true when the app should quit.
fn handle_key(app: &mut App, key: KeyEvent, in_command: &mut bool) -> bool {
    // Prefix logic: if not in command mode and this is the prefix, enter command mode.
    if !*in_command && key.code == PREFIX && app.overlay == Overlay::None {
        *in_command = true;
        return false;
    }

    // If we're inside a palette/picker overlay, handle its keys.
    match app.overlay {
        Overlay::RemoteAttach => {
            let host = app.remote_host.clone();
            match key.code {
                KeyCode::Esc => app.overlay = Overlay::None,
                KeyCode::Enter => {
                    if let Some(eng) = app.selected_engine() {
                        let host = if host.trim().is_empty() { "localhost".to_string() } else { host.trim().to_string() };
                        app.spawn_remote(&host, eng);
                        app.overlay = Overlay::None;
                    }
                }
                KeyCode::Down => app.selected = (app.selected + 1).min(engines_len() - 1),
                KeyCode::Up => app.selected = app.selected.saturating_sub(1),
                KeyCode::Char(c) => app.remote_host.push(c),
                KeyCode::Backspace => { app.remote_host.pop(); }
                _ => {}
            }
            return false;
        }
        Overlay::Palette => {
            match key.code {
                KeyCode::Esc => app.overlay = Overlay::None,
                KeyCode::Enter => app.jump_to_selection(),
                KeyCode::Down => app.selected = app.selected.saturating_add(1).min(app.filtered.len().saturating_sub(1)),
                KeyCode::Up => app.selected = app.selected.saturating_sub(1),
                KeyCode::Char(c) => {
                    app.query.push(c);
                    app.refresh_filter();
                }
                KeyCode::Backspace => {
                    app.query.pop();
                    app.refresh_filter();
                }
                _ => {}
            }
            return false;
        }
        Overlay::NewSession => {
            match key.code {
                KeyCode::Esc => app.overlay = Overlay::None,
                KeyCode::Enter => {
                    if let Some(eng) = app.selected_engine() {
                        app.spawn_local("this-host", eng);
                        app.overlay = Overlay::None;
                    }
                }
                KeyCode::Down => app.selected = (app.selected + 1).min(engines_len() - 1),
                KeyCode::Up => app.selected = app.selected.saturating_sub(1),
                _ => {}
            }
            return false;
        }
        Overlay::None => {}
    }

    // Command-mode keys (after prefix).
    if *in_command {
        *in_command = false;
        match key.code {
            KeyCode::Char('/') => {
                app.overlay = Overlay::Palette;
                app.query.clear();
                app.selected = 0;
                app.refresh_filter();
            }
            KeyCode::Char('n') => {
                app.overlay = Overlay::NewSession;
                app.selected = 0;
            }
            KeyCode::Char('r') => {
                // Remote attach: pick host + engine, spawn a pane on another machine.
                app.overlay = Overlay::RemoteAttach;
                app.remote_host.clear();
                app.selected = 0;
            }
            KeyCode::Char('t') => {
                // Spawn a session backed by a real tmux pane (TAB=SESSION=PANE@HOST).
                app.spawn_tmux("this-host", "shell");
            }
            KeyCode::Char('q') => return true,
            KeyCode::Char('s') => {
                // Refresh + peek the local harness fleet (commander-bus status badges).
                match autonomous_term::harness::HarnessClient::local().status() {
                    Ok(st) => {
                        app.fleet = st;
                        // Report the fleet summary into the pane so it's visible for a moment.
                        let line = format!("\r\n[fleet] {}\r\n", app.fleet.summary());
                        if let Some(s) = app.active_session_mut() {
                            s.write(line.as_bytes());
                        }
                    }
                    Err(_) => {
                        if let Some(s) = app.active_session_mut() {
                            s.write(b"\r\n[fleet] harness daemon unreachable (is it joined?)\r\n");
                        }
                    }
                }
            }
            KeyCode::Char('c') => {
                // clear / focus first tab
                if !app.tabs.is_empty() {
                    app.active = 0;
                }
            }
            KeyCode::Tab => {
                if !app.tabs.is_empty() {
                    app.active = (app.active + 1) % app.tabs.len();
                }
            }
            KeyCode::Char(c @ '1'..='9') => {
                let idx = (c as usize) - ('1' as usize);
                if idx < app.tabs.len() {
                    app.active = idx;
                }
            }
            KeyCode::Char('x') => close_tab(app),
            _ => {}
        }
        return false;
    }

    // Normal mode: keystrokes go to the active session.
    if let Some(_s) = app.active_session_mut() {
        // Write the key's bytes to the session. (Simple UTF-8 of typed char for now.)
        if let KeyCode::Char(c) = key.code {
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            app.active_session_mut().unwrap().write(s.as_bytes());
        }
        // Handle Ctrl and Enter minimally via ESC sequences — refine later.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if let KeyCode::Char('c') = key.code {
                app.active_session_mut().unwrap().write(b"\x03");
            }
        }
    }
    false
}

fn close_tab(app: &mut App) {
    if !app.tabs.is_empty() {
        app.tabs.remove(app.active);
        if app.active >= app.tabs.len() {
            app.active = app.tabs.len().saturating_sub(1);
        }
    }
}

fn engines_len() -> usize {
    autonomous_term::engines::ENGINES.len()
}
