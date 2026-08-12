//! The ratatui TUI shell: tab bar, session palette, engine picker, and a renderer that draws the
//! active session's alacritty `Term` grid as the main surface.

use std::io::Stdout;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, Overlay};
use crate::engines::ENGINES;
use crate::session::Listener;
use alacritty_terminal::grid::Dimensions;

/// Map an alacritty cell color to a ratatui color (best-effort approximation).
fn map_color(c: &alacritty_terminal::vte::ansi::Color) -> Color {
    use alacritty_terminal::vte::ansi::{Color as ACell, NamedColor};
    match c {
        ACell::Named(n) => match n {
            NamedColor::Black | NamedColor::DimBlack => Color::Black,
            NamedColor::Red | NamedColor::DimRed => Color::Red,
            NamedColor::Green | NamedColor::DimGreen => Color::Green,
            NamedColor::Yellow | NamedColor::DimYellow => Color::Yellow,
            NamedColor::Blue | NamedColor::DimBlue => Color::Blue,
            NamedColor::Magenta | NamedColor::DimMagenta => Color::Magenta,
            NamedColor::Cyan | NamedColor::DimCyan => Color::Cyan,
            NamedColor::White | NamedColor::DimWhite => Color::White,
            _ => Color::Reset,
        },
        ACell::Indexed(i) => {
            // Map 16-color cube to an approximate ANSI/ratatui color.
            Color::Indexed(*i)
        }
        ACell::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
    }
}

/// Render the whole interface.
pub fn draw(term: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<Stdout>>, app: &mut App) {
    let _ = term.draw(|frame| draw_frame(frame, app));
}

/// Render one frame.
fn draw_frame(frame: &mut Frame, app: &mut App) {
    let screen = frame.area();

    // Split: tab bar (top), terminal (main), status (bottom).
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(screen);

    draw_tab_bar(frame, chunks[0], app);
    draw_terminal(frame, chunks[1], app);
    draw_status(frame, chunks[2], app);

    // Overlays on top of the terminal area.
    match app.overlay {
        Overlay::Palette => draw_palette(frame, chunks[1], app),
        Overlay::NewSession => draw_picker(frame, chunks[1], app),
        Overlay::None => {}
    }
}

/// The top tab bar — one tab per session (pane@host).
fn draw_tab_bar(frame: &mut Frame, area: Rect, app: &App) {
    let mut spans: Vec<Span> = Vec::new();
    for (i, s) in app.tabs.iter().enumerate() {
        let active = i == app.active;
        let label = format!(" {} ", s.meta.title);
        let style = if active {
            Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White).bg(Color::DarkGray)
        };
        spans.push(Span::styled(label, style));
        spans.push(Span::raw(" "));
    }
    if spans.is_empty() {
        spans.push(Span::styled(" (no sessions) ", Style::default().fg(Color::DarkGray)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Render the active session's terminal grid into the main area.
fn draw_terminal(frame: &mut Frame, area: Rect, app: &App) {
    // Resize the active session to match the available area (capped by u16).
    if let Some(session) = app.active_session() {
        let size = crate::session::TermSize {
            lines: area.height.max(1) as usize,
            cols: area.width.max(1) as usize,
        };
        // Only resize when the terminal grid differs, to avoid churn.
        let g = session.term.lock();
        let changed = g.screen_lines() as u16 != area.height || g.columns() as u16 != area.width;
        drop(g);
        if changed {
            session.resize(size);
        }
    }

    // Snapshot the active grid's cells into a (lines x cols) buffer.
    let (lines, cols) = (area.height as usize, area.width as usize);
    let mut grid = vec![vec![(' ', Color::Reset, Color::Reset); cols]; lines];

    if let Some(session) = app.active_session() {
        let g = session.term.lock();
        use alacritty_terminal::grid::Dimensions;
        let glines = g.screen_lines();
        let gcols = g.columns();
        // Align the terminal's top-left line to area top-left (ignore scrollback for now).
        let take = lines.min(glines);
        let take_cols = cols.min(gcols);
        for row in 0..take {
            for col in 0..take_cols {
                // Fetch the cell at (row, col) via the public grid indexing.
                if let Some(cell) = cell_at(&g, row, col) {
                    grid[row][col] = (cell.c, map_color(&cell.fg), map_color(&cell.bg));
                }
            }
        }
    }

    // Render each row as a single styled line.
    for (r, row) in grid.iter().enumerate() {
        let line = Line::from(
            row.iter()
                .filter_map(|(c, fg, bg)| {
                    if *c == ' ' && *bg == Color::Reset {
                        // still render as blank to keep geometry
                        Some(Span::styled(" ", Style::default()))
                    } else {
                        Some(Span::styled(c.to_string(), Style::default().fg(*fg).bg(*bg)))
                    }
                })
                .collect::<Vec<_>>(),
        );
        if line.spans.is_empty() {
            continue;
        }
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(area.x, area.y + r as u16, area.width, 1),
        );
    }
}

/// Fetch the cell at visible row/col (no scrollback offset yet).
fn cell_at(g: &alacritty_terminal::term::Term<Listener>, row: usize, col: usize) -> Option<&alacritty_terminal::term::cell::Cell> {
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::Column;
    if row >= g.screen_lines() {
        return None;
    }
    Some(&g.grid()[alacritty_terminal::index::Line(row as i32)][Column(col)])
}

/// Bottom status line: active session's host · engine · title + key hints.
fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let mut text = String::new();
    if let Some(s) = app.active_session() {
        text = format!(" {} · {} · {} · [{}]", s.meta.host, s.meta.engine, s.meta.title, s.kind());
    }
    let hints = "  [prefix+/] palette  [prefix+n] new  [prefix+q] quit  [1..9] jump";
    let line = Line::from(vec![
        Span::styled(text, Style::default().fg(Color::Cyan)),
        Span::styled(hints, Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// The session palette overlay: fuzzy-find any session, jump with Enter.
fn draw_palette(frame: &mut Frame, area: Rect, app: &mut App) {
    app.refresh_filter();
    let height = (app.filtered.len() as u16).min(12);
    let pop = Rect::new(
        area.x + 4,
        area.y + 2,
        area.width.saturating_sub(8).max(10),
        height + 2,
    );
    let mut items: Vec<Line> = Vec::new();
    for (row, &i) in app.filtered.iter().enumerate() {
        let s = &app.tabs[i];
        let sel = row == app.selected;
        let sty = if sel {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default()
        };
        items.push(Line::from(Span::styled(
            format!(" {} · {} · {} ", s.meta.host, s.meta.engine, s.meta.title),
            sty,
        )));
    }
    if items.is_empty() {
        items.push(Line::from(" (no matches)"));
    }
    let query_line = Line::from(vec![
        Span::styled("  🔍 ", Style::default().fg(Color::Cyan)),
        Span::raw(format!("{} ", app.query)),
    ]);
    let mut body = vec![query_line, Line::from("")];
    body.extend(items);
    let block = Block::default()
        .title(" sessions ")
        .borders(Borders::ALL)
        .style(Style::default().fg(Color::Cyan).bg(Color::Black));
    frame.render_widget(Paragraph::new(body).block(block).wrap(Wrap { trim: false }), pop);
}

/// New-session picker: choose a host + engine, then create a tab.
fn draw_picker(frame: &mut Frame, area: Rect, app: &mut App) {
    let _ = app; // picker body computed inline below
    let pop = Rect::new(
        area.x + 4,
        area.y + 2,
        area.width.saturating_sub(8).max(20),
        16,
    );
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        "  new session ",
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
    ))];
    for (i, e) in ENGINES.iter().enumerate() {
        let sel = i == app.selected;
        let sty = if sel {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(
            format!("  {}  {}", e.id, e.label),
            sty,
        )));
    }
    let block = Block::default()
        .title(" engines ")
        .borders(Borders::ALL)
        .style(Style::default().fg(Color::Cyan));
    frame.render_widget(Paragraph::new(lines).block(block), pop);
}
