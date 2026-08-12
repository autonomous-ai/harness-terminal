//! harness-terminal — a terminal-first dive into a fleet of AI agent sessions.
//!
//! A standalone native window (no host terminal): every tab is one agent session (Claude Code,
//! Codex, OpenCode, PI, …) running in one pane on one host in your fleet — local, via tmux, over
//! ssh, or over the harness e2ee tunnel.
//!
//! `TAB = SESSION = PANE@HOST`. Keyboard-first, tmux-style prefix (`Ctrl+Space` then a command),
//! with the active session rendered by the alacritty emulator directly into our own window.
//!
//! Run `harness-terminal --tui` to use the legacy ratatui backend instead (e.g. when SSH'd into a
//! server with no display).

use std::io;

use harness_terminal::app::App;
use harness_terminal::session::TermSize;

fn main() -> io::Result<()> {
    // Legacy TUI fallback: run inside an existing terminal (ratatui + crossterm).
    if std::env::args().any(|a| a == "--tui") {
        return run_tui();
    }
    run_native().map_err(|e| io::Error::other(e.to_string()))
}

/// Standalone native window (default shell).
fn run_native() -> Result<(), Box<dyn std::error::Error>> {
    let mut app = App::new(TermSize { lines: 24, cols: 80 });
    // Reopen the tabs that were open last time (best-effort; failures drop silently).
    for spec in harness_terminal::restore::load() {
        app.restore_tab(&spec);
    }
    // Open on the tab that was focused last time (clamped to however many restored).
    if !app.tabs.is_empty() {
        app.active = harness_terminal::restore::load_active().min(app.tabs.len() - 1);
    }
    harness_terminal::native::run(app)
}

/// Legacy ratatui/crossterm shell — kept as the `--tui` escape hatch for running on a server that
/// has a terminal but no display.
fn run_tui() -> io::Result<()> {
    use crossterm::event::{self, Event as CTEvent};
    use crossterm::execute;
    use ratatui::backend::CrosstermBackend;
    use ratatui::Terminal;

    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    let mut app = App::new(TermSize { lines: 24, cols: 80 });
    app.spawn_local("this-host", "shell");

    let mut in_command = false;
    loop {
        app.reconnect_sweep();
        harness_terminal::tui::draw(&mut term, &mut app);
        if event::poll(std::time::Duration::from_millis(16))? {
            if let CTEvent::Key(key) = event::read()? {
                if handle_key_tui(&mut app, key, &mut in_command) {
                    break;
                }
            }
        }
    }

    crossterm::terminal::disable_raw_mode()?;
    execute!(term.backend_mut(), crossterm::terminal::LeaveAlternateScreen)?;
    Ok(())
}

/// Handle one key in the legacy TUI (mirrors the native handler's logic).
fn handle_key_tui(app: &mut App, key: crossterm::event::KeyEvent, in_command: &mut bool) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};
    use harness_terminal::app::Overlay;
    use harness_terminal::engines::ENGINES;

    if !*in_command && key.code == KeyCode::Char(' ') && app.overlay == Overlay::None {
        *in_command = true;
        return false;
    }

    match app.overlay {
        Overlay::RemoteAttach => {
            match key.code {
                KeyCode::Esc => app.overlay = Overlay::None,
                KeyCode::Enter => {
                    if let Some(eng) = app.selected_engine() {
                        let host = if app.remote_host.trim().is_empty() { "127.0.0.1".to_string() } else { app.remote_host.trim().to_string() };
                        app.spawn_tunnel(&host, harness_terminal::harness::HARNESS_PORT_DEFAULT, eng);
                        app.overlay = Overlay::None;
                    }
                }
                KeyCode::Down => app.selected = (app.selected + 1).min(ENGINES.len() - 1),
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
                KeyCode::Char(c) => { app.query.push(c); app.refresh_filter(); }
                KeyCode::Backspace => { app.query.pop(); app.refresh_filter(); }
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
                KeyCode::Down => app.selected = (app.selected + 1).min(ENGINES.len() - 1),
                KeyCode::Up => app.selected = app.selected.saturating_sub(1),
                _ => {}
            }
            return false;
        }
        Overlay::Find => {} // native-only feature; the TUI fallback ignores it.
        Overlay::FleetSearch => {} // native-only feature; the TUI fallback ignores it.
        Overlay::Fleet => {}
        Overlay::Help => {}
        Overlay::Rename => {}
        Overlay::Broadcast => {} // native-only feature; the TUI fallback ignores it.
        Overlay::Peek => {} // native-only feature; the TUI fallback ignores it.

        Overlay::None => {}
    }

    if *in_command {
        *in_command = false;
        match key.code {
            KeyCode::Char('/') => { app.overlay = Overlay::Palette; app.query.clear(); app.selected = 0; app.refresh_filter(); }
            KeyCode::Char('n') => { app.overlay = Overlay::NewSession; app.selected = 0; }
            KeyCode::Char('r') => { app.overlay = Overlay::RemoteAttach; app.remote_host.clear(); app.selected = 0; }
            KeyCode::Char('t') => app.spawn_tmux("this-host", "shell"),
            KeyCode::Char('q') => return true,
            KeyCode::Char('s') => {
                match harness_terminal::harness::HarnessClient::local().status() {
                    Ok(st) => {
                        app.fleet = st;
                        let line = format!("\r\n[fleet] {}\r\n", app.fleet.summary());
                        if let Some(s) = app.active_session_mut() { s.write(line.as_bytes()); }
                    }
                    Err(_) => {
                        if let Some(s) = app.active_session_mut() {
                            s.write(b"\r\n[fleet] harness daemon unreachable (is it joined?)\r\n");
                        }
                    }
                }
            }
            KeyCode::Char('c') => { if !app.tabs.is_empty() { app.active = 0; } }
            KeyCode::Tab => { if !app.tabs.is_empty() { app.active = (app.active + 1) % app.tabs.len(); } }
            KeyCode::Char(c @ '1'..='9') => {
                let idx = (c as usize) - ('1' as usize);
                if idx < app.tabs.len() { app.active = idx; }
            }
            KeyCode::Char('x') => { if !app.tabs.is_empty() { app.tabs.remove(app.active); if app.active >= app.tabs.len() { app.active = app.tabs.len().saturating_sub(1); } } }
            _ => {}
        }
        return false;
    }

    if let Some(s) = app.active_session_mut() {
        if let KeyCode::Char(c) = key.code {
            let mut buf = [0u8; 4];
            let s2 = c.encode_utf8(&mut buf);
            s.write(s2.as_bytes());
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            s.write(b"\x03");
        }
    }
    false
}
