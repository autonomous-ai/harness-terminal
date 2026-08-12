//! Standalone native window (our terminal — no host emulator).
//!
//! winit provides the window + event loop; softbuffer provides a CPU framebuffer we draw the
//! alacritty grid into; ab_glyph rasterizes glyphs. This replaces the ratatui/crossterm TUI as the
//! default shell — the fleet/tunnel/reconnect machinery in `session.rs`/`transport.rs` is untouched
//! and shared. Chrome (tab bar, palette, status) is drawn natively with `draw_text`.

use std::num::NonZeroU32;
use std::rc::Rc;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalSize, Size};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, ModifiersState};
use winit::window::{Window, WindowId};

use softbuffer::{Context, Surface};

use crate::app::{App, Overlay};
use crate::engines::ENGINES;
use crate::render::{draw_grid, draw_text, Framebuffer, GlyphCache};
use crate::session::TermSize;

/// Chromeless text colors (macOS style).
const CHROME_FG: (u8, u8, u8) = (0xcc, 0xcc, 0xcc);
const CHROME_DIM: (u8, u8, u8) = (0x66, 0x66, 0x66);
const WHITE: (u8, u8, u8) = (0xff, 0xff, 0xff);

/// The native application: window + framebuffer surface + app state + glyph cache.
struct Application {
    window: Option<Rc<Window>>,
    context: Option<Context<Rc<Window>>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,
    size: PhysicalSize<u32>,
    font_px: u32,
    cell_w: u32,
    cell_h: u32,
    app: App,
    cache: GlyphCache,
    prefix_down: bool,
    mods: ModifiersState,
    /// True while the view is scrolled into history (live follow suspended). Set when the user
    /// scrolls up; cleared when they return to the bottom (scroll command / Esc) or new output
    /// resets the display offset in `render`.
    scrolled: bool,
    /// Active search query ("" when the Find overlay is closed).
    find_query: String,
    /// The currently-focused search match (absolute line, col, width); recomputed on each query
    /// change / Enter and passed to draw_grid for highlighting.
    find_hit: Option<crate::render::Find>,
    /// Every match of the active query (line, col, width), so draw_grid can highlight all of them
    /// in yellow while the focused one shows orange.
    find_all: Vec<crate::render::Find>,
    /// Index into `find_all` of the currently-focused match (the "N of M" cursor).
    find_index: usize,
    /// Mouse state: the cell anchor where a drag-selection started (Some while left button held).
    /// With winit 0.30 we track presses/releases ourselves; dragging updates the selection end.
    mouse_anchor: Option<Point>,
    /// Latest cursor position in framebuffer px (winit's MouseInput has no position; we read this).
    cursor: (f64, f64),
    /// Last (press-time, press-position, accumulated-click-count) to detect double/triple clicks.
    /// winit 0.30 doesn't hand us a click count, so we time consecutive presses ourselves.
    last_press: Option<(std::time::Instant, (f64, f64), u32)>,
}

impl Application {
    fn new(app: App) -> Self {
        Application {
            window: None,
            context: None,
            surface: None,
            size: PhysicalSize::new(800, 600),
            font_px: 14,
            cell_w: 8,
            cell_h: 18,
            app,
            cache: GlyphCache::load(),
            prefix_down: false,
            mods: ModifiersState::default(),
            scrolled: false,
            find_query: String::new(),
            find_hit: None,
            find_all: Vec::new(),
            find_index: 0,
            mouse_anchor: None,
            cursor: (0.0, 0.0),
            last_press: None,
        }
    }

    fn metrics_from_scale(&mut self) {
        if let Some(w) = &self.window {
            let s = w.scale_factor() as f32;
            self.font_px = (14.0 * s).round() as u32;
            self.cell_w = (8.0 * s).round() as u32;
            self.cell_h = (18.0 * s).round() as u32;
        }
    }

    fn redraw(&mut self) {
        let (Some(w), Some(h)) = (NonZeroU32::new(self.size.width), NonZeroU32::new(self.size.height)) else {
            return;
        };
        let (width, height) = (w.get() as usize, h.get() as usize);
        // 0x00RRGGBB (softbuffer's native format); starts solid black.
        let mut fb = Framebuffer::new(width, height);
        for p in fb.pixels.iter_mut() {
            *p = 0x0000_0000;
        }
        // Render into the CPU framebuffer first (doesn't borrow the surface).
        self.render(&mut fb);

        // Then present it via the softbuffer surface.
        let Some(surface) = &mut self.surface else { return };
        let Ok(mut buffer) = surface.buffer_mut() else { return };
        for (dst, src) in buffer.iter_mut().zip(fb.pixels.iter()) {
            *dst = *src;
        }
        let _ = buffer.present();
    }

    /// Render the whole frame into the framebuffer.
    fn render(&mut self, fb: &mut Framebuffer) {
        let (tab_h, status_h) = (self.cell_h as usize, self.cell_h as usize);
        let term_top = tab_h;
        let term_bottom = fb.height.saturating_sub(status_h);
        let gline_px = self.cell_h as usize;
        let gcol_px = self.cell_w as usize;
        let grid_lines = term_bottom.saturating_sub(term_top).max(1) / gline_px;
        let grid_cols = fb.width.max(1) / gcol_px;

        // Size the active session to the on-screen grid.
        if let Some(active) = self.app.active_session() {
            let g = active.term.lock();
            let (gl, gc) = (g.screen_lines(), g.columns());
            drop(g);
            if gl != grid_lines || gc != grid_cols {
                let size = TermSize { lines: grid_lines.max(1), cols: grid_cols.max(1) };
                active.resize(size);
            }
        }

        // Terminal grid.
        if let Some(active) = self.app.active_session() {
            let mut g = active.term.lock();
            // With history, a non-zero display_offset is a live-follow "scroll" (bottom = offset 0).
            // Auto-return to the live view when new content pushes us to the bottom again: if we're
            // not mid-gesture (self.scrolled is false) force the view back to the latest line.
            let at_bottom = g.grid().display_offset() == 0;
            if !self.scrolled && !at_bottom {
                use alacritty_terminal::grid::Scroll;
                g.grid_mut().scroll_display(Scroll::Bottom);
            }
            // Compute the current text-selection range (if any) so draw_grid can highlight it.
            let sel = g.selection.as_ref().and_then(|s| s.to_range(&g));
            draw_grid(fb, &g, self.cell_w, self.cell_h, self.font_px, &mut self.cache, self.find_hit, &self.find_all, sel.as_ref());
        }

        // Tab bar (top row).
        let tab_base = self.cell_h as usize / 2;
        let mut x = 6usize;
        for (i, s) in self.app.tabs.iter().enumerate() {
            let active = i == self.app.active;
            let dot = if active { "●" } else { "○" };
            // Show the pane's live OSC title (what the agent is doing) when it has announced one.
            let live = s.live_title().unwrap_or_else(|| s.meta.title.clone());
            let mut live = live.replace('\n', " ");
            if live.chars().count() > 18 {
                live = live.chars().take(18).collect::<String>() + "…";
            }
            let label = format!(" {} {} {} ", s.meta.engine, live, dot);
            let color = if active { WHITE } else { CHROME_DIM };
            x += draw_text(fb, &mut self.cache, &label, x, tab_base, self.font_px, color) + 12;
            if x > fb.width.saturating_sub(20) {
                break;
            }
        }

        // Status line (bottom row): left = session info, right = hints.
        let status_base = fb.height.saturating_sub(self.cell_h as usize / 2);
        let mut info = String::new();
        if let Some(s) = self.app.active_session() {
            let link = if s.alive() { "●" } else { "○ reconnecting" };
            let live = s.live_title().unwrap_or_else(|| s.meta.title.clone());
            info = format!(" {} · {} · {} · [{} {}]", s.meta.host, s.meta.engine, live, s.kind(), link);
        }
        draw_text(fb, &mut self.cache, &info, 6, status_base, self.font_px, CHROME_FG);
        let hints = " prefix+/ palette  prefix+n new  prefix+r remote  prefix+q quit ";
        let hw = draw_text(fb, &mut self.cache, hints, 6, status_base, self.font_px, CHROME_DIM);
        // Move the hint to the right edge by re-drawing after clearing a wide column is complex;
        // simplest right-align: draw hints over the info end offset. We draw at the right edge:
        let hx = fb.width.saturating_sub(hw + 6);
        // Overwrite: clear the column first via black, then draw.
        for py in status_base.saturating_sub(self.font_px as usize)..(status_base + self.font_px as usize) {
            for px in hx.min(fb.width)..fb.width {
                if py < fb.height {
                    fb.pixels[py * fb.width + px] = 0;
                }
            }
        }
        draw_text(fb, &mut self.cache, hints, hx, status_base, self.font_px, CHROME_DIM);

        // Overlays.
        match self.app.overlay {
            Overlay::Palette => self.render_palette(fb),
            Overlay::NewSession => self.render_list(fb, "  new session  ", true),
            Overlay::RemoteAttach => self.render_remote(fb),
            Overlay::Find => self.render_find(fb),
            Overlay::None => {}
        }
    }

    fn overlay_base_y(&self) -> (usize, usize) {
        let line_px = self.font_px as usize + 6;
        (self.cell_h as usize + 4, line_px)
    }

    fn render_palette(&mut self, fb: &mut Framebuffer) {
        // Recompute the filter (mirrors tui::refresh_filter).
        self.app.refresh_filter();
        let (base_y, line_px) = self.overlay_base_y();
        draw_text(fb, &mut self.cache, &format!("🔍 {}", self.app.query), 32, base_y, self.font_px, WHITE);
        for (row, &i) in self.app.filtered.iter().enumerate().take(12) {
            let s = &self.app.tabs[i];
            let sel = row == self.app.selected;
            let color = if sel { WHITE } else { CHROME_DIM };
            let line = format!("  {} · {} · {}  {}", s.meta.host, s.meta.engine, s.meta.title, if sel { "◄" } else { "" });
            draw_text(fb, &mut self.cache, &line, 32, base_y + (row + 1) * line_px, self.font_px, color);
        }
    }

    /// Engine list overlay (new-session picker). `_is_picker` reserved for remote mode extras.
    fn render_list(&mut self, fb: &mut Framebuffer, header: &str, _is_picker: bool) {
        let (base_y, line_px) = self.overlay_base_y();
        draw_text(fb, &mut self.cache, header, 32, base_y, self.font_px, WHITE);
        for (i, e) in ENGINES.iter().enumerate() {
            let sel = i == self.app.selected;
            let color = if sel { WHITE } else { CHROME_DIM };
            let line = format!("  {}  {}  {}", e.id, e.label, if sel { "◄" } else { "" });
            draw_text(fb, &mut self.cache, &line, 32, base_y + (i + 2) * line_px, self.font_px, color);
        }
    }

    fn render_remote(&mut self, fb: &mut Framebuffer) {
        let (base_y, line_px) = self.overlay_base_y();
        draw_text(fb, &mut self.cache, "  attach to pane@host  ", 32, base_y, self.font_px, WHITE);
        draw_text(fb, &mut self.cache, &format!("  host: {}", self.app.remote_host), 32, base_y + line_px, self.font_px, CHROME_FG);
        for (i, e) in ENGINES.iter().enumerate() {
            let sel = i == self.app.selected;
            let color = if sel { WHITE } else { CHROME_DIM };
            let line = format!("  {}  {}  {}", e.id, e.label, if sel { "◄" } else { "" });
            draw_text(fb, &mut self.cache, &line, 32, base_y + (i + 3) * line_px, self.font_px, color);
        }
    }

    /// Recompute the focused search match (from the top if none, else continue from it) and scroll
    /// the viewport so the match is visible at the top of the grid area.
    /// Scroll the viewport so the focused match's line is visible (at the top of the screen).
    fn find_scroll_to(&self, g: &mut alacritty_terminal::term::Term<crate::session::Listener>, l: i32) {
        use alacritty_terminal::grid::Scroll;
        let current = g.grid().display_offset() as i32;
        let desired = (-l as i32).clamp(0, g.grid().history_size() as i32);
        g.grid_mut().scroll_display(Scroll::Delta(desired - current));
    }

    /// Recompute the occurrence list after a query edit; focuses the first match (or the match
    /// nearest the previous focus) so the viewport tracks the user.
    fn find_recompute(&mut self, _start: Option<i32>) {
        let Some(active) = self.app.active_session() else { self.find_hit = None; self.find_all = Vec::new(); self.find_index = 0; return };
        let mut g = active.term.lock();
        // Remember the previous focus position so edits keep roughly the same match in view.
        let prev_line = self.find_hit.map(|(l, _, _)| l);
        self.find_all = crate::render::all_matches(&g, &self.find_query);
        // Pick the first match at-or-after the old focus; else the very first match.
        let idx = if self.find_all.is_empty() {
            0
        } else {
            prev_line
                .and_then(|pl| self.find_all.iter().position(|&(l, _, _)| l >= pl))
                .unwrap_or(0)
        };
        self.find_index = idx;
        self.find_hit = self.find_all.get(idx).copied();
        if let Some((l, _, _)) = self.find_hit {
            self.find_scroll_to(&mut *g, l);
            self.scrolled = true;
        }
    }

    /// Jump the focus by `delta` through the matches (wrapping around the ends), updating the
    /// focused highlight and the viewport. Returns false when there's nothing to step to.
    fn find_jump(&mut self, delta: isize) -> bool {
        let m = self.find_all.len();
        if m == 0 {
            return false;
        }
        let next = (self.find_index as isize + delta).rem_euclid(m as isize) as usize;
        self.find_index = next;
        self.find_hit = self.find_all.get(next).copied();
        let Some(active) = self.app.active_session() else { return false };
        if let Some((l, _, _)) = self.find_hit {
            let mut g = active.term.lock();
            self.find_scroll_to(&mut *g, l);
            self.scrolled = true;
        }
        true
    }

    /// Render the search overlay: query prompt + current match location in the status area.
    fn render_find(&mut self, fb: &mut Framebuffer) {
        let status_base = fb.height.saturating_sub(self.cell_h as usize / 2);
        // Overlay status line with query and match count info.
        let line = if self.find_query.is_empty() {
            "  find: (type to search)  ".to_string()
        } else {
            let n = self.find_all.len();
            if n > 0 {
                let here = (self.find_index % n) + 1;
                format!("  find: {}  (match {} of {} · Enter/Tab next, Shift+Enter prev)", self.find_query, here, n)
            } else {
                format!("  find: {}  (no match)", self.find_query)
            }
        };
        draw_text(fb, &mut self.cache, &line, 6, status_base, self.font_px, WHITE);
    }

    /// Handle a key. Mirrors the TUI's prefix→command→forward logic; prefix here is Ctrl+Space
    /// (tmux-like), so a plain space still types normally.
    fn handle_key(&mut self, key: &Key, mods: &ModifiersState) {
        let is_space = matches!(key, Key::Character(c) if c == " ")
            || matches!(key, Key::Named(winit::keyboard::NamedKey::Space));

        // Enter command mode: Ctrl+Space (or, while we haven't a real prefix, Ctrl+Space).
        if mods.control_key() && is_space && self.app.overlay == Overlay::None {
            self.prefix_down = true;
            return;
        }

        if self.prefix_down && self.app.overlay == Overlay::None {
            self.prefix_down = false;
            if self.command_key(key) {
                return; // quit handled by caller checking a flag — simplified: ignore for now
            }
            return;
        }

        self.forward_key(key, mods);
    }

    /// Command-mode (after prefix). Returns true to quit.
    fn command_key(&mut self, key: &Key) -> bool {
        match key {
            Key::Character(c) => match c.as_str() {
                "/" => { self.app.overlay = Overlay::Palette; self.app.query.clear(); self.app.selected = 0; self.app.refresh_filter(); }
                "n" => { self.app.overlay = Overlay::NewSession; self.app.selected = 0; }
                "r" => { self.app.overlay = Overlay::RemoteAttach; self.app.remote_host.clear(); self.app.selected = 0; }
                "t" => self.app.spawn_tmux("this-host", "shell"),
                "q" => return true,
                "s" => {
                    match crate::harness::HarnessClient::local().status() {
                        Ok(st) => {
                            self.app.fleet = st;
                            let line = format!("\r\n[fleet] {}\r\n", self.app.fleet.summary());
                            if let Some(s) = self.app.active_session_mut() { s.write(line.as_bytes()); }
                        }
                        Err(_) => {
                            if let Some(s) = self.app.active_session_mut() {
                                s.write(b"\r\n[fleet] harness daemon unreachable (is it joined?)\r\n");
                            }
                        }
                    }
                }
                "c" => { if !self.app.tabs.is_empty() { self.app.active = 0; } }
                "x" => { close_tab(&mut self.app); }
                "g" => { scroll_active(self, 20); self.scrolled = true; }
                "b" => self.scroll_to_bottom(),
                "f" => { self.app.overlay = Overlay::Find; self.find_query.clear(); self.find_hit = None; self.find_all = Vec::new(); },
                // Numeric tabs 1-9.
                _ if c.len() == 1 && c.chars().next().unwrap().is_ascii_digit() => {
                    let idx = c.chars().next().unwrap() as u8;
                    if (b'1'..=b'9').contains(&idx) {
                        let i = (idx - b'1') as usize;
                        if i < self.app.tabs.len() { self.app.active = i; }
                    }
                }
                _ => {}
            },
            Key::Named(n) => match n {
                winit::keyboard::NamedKey::Tab => {
                    if !self.app.tabs.is_empty() {
                        self.app.active = (self.app.active + 1) % self.app.tabs.len();
                    }
                }
                _ => {}
            },
            _ => {}
        }
        false
    }

    /// Normal-typing + overlay navigation.
    fn forward_key(&mut self, key: &Key, mods: &ModifiersState) {
        match self.app.overlay {
            Overlay::Palette => {
                match key {
                    Key::Character(c) => { self.app.query.push_str(c); self.app.refresh_filter(); }
                    Key::Named(n) => match n {
                        winit::keyboard::NamedKey::Enter => self.app.jump_to_selection(),
                        winit::keyboard::NamedKey::Escape => self.app.overlay = Overlay::None,
                        winit::keyboard::NamedKey::ArrowDown => self.app.selected = self.app.selected.saturating_add(1).min(self.app.filtered.len().saturating_sub(1)),
                        winit::keyboard::NamedKey::ArrowUp => self.app.selected = self.app.selected.saturating_sub(1),
                        winit::keyboard::NamedKey::Backspace => { self.app.query.pop(); self.app.refresh_filter(); }
                        _ => {}
                    },
                    _ => {}
                }
                return;
            }
            Overlay::NewSession => {
                match key {
                    Key::Named(n) => match n {
                        winit::keyboard::NamedKey::Enter => if let Some(e) = self.app.selected_engine() {
                            self.app.spawn_local("this-host", e); self.app.overlay = Overlay::None;
                        },
                        winit::keyboard::NamedKey::Escape => self.app.overlay = Overlay::None,
                        winit::keyboard::NamedKey::ArrowDown => self.app.selected = (self.app.selected + 1).min(ENGINES.len() - 1),
                        winit::keyboard::NamedKey::ArrowUp => self.app.selected = self.app.selected.saturating_sub(1),
                        _ => {}
                    },
                    _ => {}
                }
                return;
            }
            Overlay::RemoteAttach => {
                match key {
                    Key::Character(c) => self.app.remote_host.push_str(c),
                    Key::Named(n) => match n {
                        winit::keyboard::NamedKey::Enter => if let Some(e) = self.app.selected_engine() {
                            let host = if self.app.remote_host.trim().is_empty() { "127.0.0.1".to_string() } else { self.app.remote_host.trim().to_string() };
                            self.app.spawn_tunnel(&host, crate::harness::HARNESS_PORT_DEFAULT, e);
                            self.app.overlay = Overlay::None;
                        },
                        winit::keyboard::NamedKey::Escape => self.app.overlay = Overlay::None,
                        winit::keyboard::NamedKey::ArrowDown => self.app.selected = (self.app.selected + 1).min(ENGINES.len() - 1),
                        winit::keyboard::NamedKey::ArrowUp => self.app.selected = self.app.selected.saturating_sub(1),
                        winit::keyboard::NamedKey::Backspace => { self.app.remote_host.pop(); }
                        _ => {}
                    },
                    _ => {}
                }
                return;
            }
            Overlay::Find => {
                match key {
                    Key::Character(c) => {
                        self.find_query.push_str(c);
                        self.find_recompute(None);
                    }
                    Key::Named(n) => match n {
                        winit::keyboard::NamedKey::Enter if mods.shift_key() => { self.find_jump(-1); }
                        winit::keyboard::NamedKey::Enter | winit::keyboard::NamedKey::Tab => { self.find_jump(1); }
                        winit::keyboard::NamedKey::ArrowDown => { self.find_jump(1); }
                        winit::keyboard::NamedKey::ArrowUp => { self.find_jump(-1); }
                        winit::keyboard::NamedKey::Backspace => { self.find_query.pop(); self.find_recompute(None); }
                        winit::keyboard::NamedKey::Escape => { self.app.overlay = Overlay::None; }
                        _ => {}
                    },
                    _ => {}
                }
                return;
            }
            Overlay::None => {}
        }

        // Normal mode: send keystrokes to the active session.
        if self.app.overlay == Overlay::None {
            // Scrollback navigation takes precedence over forwarding to the shell. While scrolled,
            // page/arrow keys move the viewport; Esc returns to the live (bottom) view. PageUp from
            // the live view also enters scroll mode.
            if self.scrolled || matches!(key, Key::Named(winit::keyboard::NamedKey::PageUp)) {
                match key {
                    Key::Named(n) => match n {
                        winit::keyboard::NamedKey::PageUp => { scroll_active(self, 20); self.scrolled = true; }
                        winit::keyboard::NamedKey::PageDown => scroll_active(self, -20),
                        winit::keyboard::NamedKey::ArrowUp => { scroll_active(self, 1); self.scrolled = true; }
                        winit::keyboard::NamedKey::ArrowDown => scroll_active(self, -1),
                        winit::keyboard::NamedKey::Escape => self.scroll_to_bottom(),
                        _ => {}
                    },
                    _ => {}
                }
                // Snap scrolled state to reality once we hit bottom (offset no longer moves).
                if self.scrolled {
                    if let Some(active) = self.app.active_session() {
                        let g = active.term.lock();
                        if g.grid().display_offset() == 0 {
                            self.scrolled = false;
                        }
                    }
                }
                return;
            }

            if let Some(s) = self.app.active_session_mut() {
                // Copy selection: Cmd+C (mac convention) copies the current text selection to the
                // system clipboard and clears the highlight. Ctrl+C still goes to the session as
                // the interrupt byte unless a selection exists on mac.
                let is_copy = mods.super_key() && matches!(key, Key::Character(c) if c == "c");
                if is_copy {
                    // Copy without holding the session borrow: read the text, clear it, then store.
                    let (text, selected) = {
                        let g = s.term.lock();
                        (g.selection_to_string(), g.selection.is_some())
                    };
                    if selected {
                        if let Some(t) = text {
                            if !t.is_empty() {
                                if let Ok(mut cb) = arboard::Clipboard::new() {
                                    let _ = cb.set_text(t);
                                }
                            }
                        }
                        s.term.lock().selection = None;
                    }
                    return;
                }
                // Paste clipboard: Ctrl+V (and Cmd+V, the mac convention) reads the system clipboard
                // and writes it to the session, bracketing with bracketed-paste if the app asked.
                let is_paste = (mods.control_key() && matches!(key, Key::Character(c) if c == "v"))
                    || (mods.super_key() && matches!(key, Key::Character(c) if c == "v"));
                if is_paste {
                    if let Ok(mut cb) = arboard::Clipboard::new() {
                        if let Ok(text) = cb.get_text() {
                            // Bracketed paste is negotiated; we always bracket to be safe with
                            // multi-line input (most modern shells/clis accept it).
                            let seq = format!("\x1b[200~{}\x1b[201~", text);
                            s.write(seq.as_bytes());
                        }
                    }
                    return;
                }
                match key {
                    Key::Character(c) => {
                        if mods.control_key() {
                            if let Some(b) = ctrl_byte(c) {
                                s.write(&[b]);
                            }
                        } else {
                            s.write(c.as_bytes());
                        }
                    }
                    Key::Named(n) => match n {
                        winit::keyboard::NamedKey::Enter => s.write(b"\r"),
                        winit::keyboard::NamedKey::Backspace => s.write(b"\x7f"),
                        winit::keyboard::NamedKey::Tab => s.write(b"\t"),
                        winit::keyboard::NamedKey::Escape => s.write(b"\x1b"),
                        winit::keyboard::NamedKey::ArrowUp => s.write(b"\x1b[A"),
                        winit::keyboard::NamedKey::ArrowDown => s.write(b"\x1b[B"),
                        winit::keyboard::NamedKey::ArrowRight => s.write(b"\x1b[C"),
                        winit::keyboard::NamedKey::ArrowLeft => s.write(b"\x1b[D"),
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
    }
}

fn close_tab(app: &mut App) {
    if !app.tabs.is_empty() {
        app.tabs.remove(app.active);
        if app.active >= app.tabs.len() {
            app.active = app.tabs.len().saturating_sub(1);
        }
    }
}

/// Scroll the active session's viewport by `delta` lines into history (positive = up/back).
/// Marks the view as user-scrolled so render doesn't snap us back to the live line.
fn scroll_active(app: &Application, delta: i32) {
    use alacritty_terminal::grid::Scroll;
    if let Some(active) = app.app.active_session() {
        let mut g = active.term.lock();
        g.grid_mut().scroll_display(Scroll::Delta(delta));
    }
}

impl Application {
    /// Return to the latest (live) view: clear the display offset and end scroll mode.
    fn scroll_to_bottom(&mut self) {
        use alacritty_terminal::grid::Scroll;
        if let Some(active) = self.app.active_session() {
            let mut g = active.term.lock();
            g.grid_mut().scroll_display(Scroll::Bottom);
        }
        self.scrolled = false;
    }

    /// Map a framebuffer pixel position to the terminal-cell it lands on (viewing row 0 = the
    /// visually-top line, which with scrollback is history). Returns None if the point is outside
    /// the grid area (tab/status chrome or the right/left gutter).
    fn mouse_to_cell(&self, x: f64, y: f64) -> Option<Point> {
        let x = x as i64;
        let y = y as i64;
        // Grid area origin: below the tab bar (cell_h), above the status bar.
        let top = self.cell_h as i64;
        let bottom = self.size.height as i64 - self.cell_h as i64;
        if y < top || y >= bottom || x < 0 {
            return None;
        }
        let row = ((y - top) / self.cell_h as i64) as i64;
        let col = (x / self.cell_w as i64) as i64;
        if row < 0 || col < 0 {
            return None;
        }
        Some(Point::new(Line((row as usize).try_into().unwrap()), Column(col as usize)))
    }

    /// Detect how many clicks this press represents. A press within ~250ms and ~4px of the previous
    /// one increments the count (up to 3); anything else resets to 1.
    fn click_count(&mut self, x: f64, y: f64) -> u32 {
        const THRESHOLD_MS: u64 = 250;
        const THRESHOLD_PX: f64 = 4.0;
        let now = std::time::Instant::now();
        let click = if let Some((prev_at, (px, py), prev_count)) = self.last_press {
            let dt = now.duration_since(prev_at).as_millis() as u64;
            let dist = ((px - x).powi(2) + (py - y).powi(2)).sqrt();
            if dt < THRESHOLD_MS && dist < THRESHOLD_PX {
                (prev_count + 1).min(3)
            } else {
                1
            }
        } else {
            1
        };
        self.last_press = Some((now, (x, y), click));
        click
    }

    /// Start a text selection at a pressed cursor position, or clear the selection when clicking.
    /// Single click = simple drag; double click = expand to a word (semantic); triple = whole line.
    /// Alt+click: reposition the shell/readline cursor at the clicked cell. We report the cursor
    /// position with the standard "active position report" sequence (`ESC [ row ; col R`), which
    /// line editors that support click-to-move (zsh, fish, and readline via inputrc) honor.
    fn mouse_alt_click(&mut self, x: f64, y: f64) {
        let Some(pt) = self.mouse_to_cell(x, y) else { return };
        // Only meaningful against live (unscrolled) screen coordinates; report 1-based.
        if self.scrolled || pt.line.0 < 0 {
            return;
        }
        let seq = format!("\x1b[{};{}R", pt.line.0 + 1, pt.column.0 + 1);
        if let Some(active) = self.app.active_session() {
            active.write(seq.as_bytes());
        }
    }

    fn mouse_press(&mut self, x: f64, y: f64) {
        let Some(pt) = self.mouse_to_cell(x, y) else {
            // Click outside the grid (on chrome) clears any selection.
            if let Some(active) = self.app.active_session() {
                let mut g = active.term.lock();
                g.selection = None;
            }
            self.mouse_anchor = None;
            self.last_press = None;
            return;
        };
        let clicks = self.click_count(x, y);
        self.mouse_anchor = Some(pt);
        if let Some(active) = self.app.active_session() {
            let mut g = active.term.lock();
            let sel = match clicks {
                // Double-click: word selection drives semantic expansion off a non-empty range (a
                // distinct end point keeps bracket-search from hijacking a single cell).
                2 => {
                    let s = Selection::new(SelectionType::Semantic, pt, Side::Left);
                    let mut s2 = s;
                    s2.update(Point::new(pt.line + Line(1), Column(0)), Side::Right);
                    s2
                }
                3 => Selection::new(SelectionType::Lines, pt, Side::Left),
                // Single click: plain drag-select; on the same cell it just anchors.
                _ => Selection::new(SelectionType::Simple, pt, Side::Left),
            };
            g.selection = Some(sel);
        }
    }

    /// Grow the drag selection to the cursor's current cell while the button is held.
    fn mouse_drag(&mut self, x: f64, y: f64) {
        let Some(pt) = self.mouse_to_cell(x, y) else { return };
        if let Some(active) = self.app.active_session() {
            let mut g = active.term.lock();
            if let Some(sel) = g.selection.as_mut() {
                sel.update(pt, Side::Right);
            }
        }
    }

    /// The user released the button: finalize the drag (if any) and copy the selection to the
    /// system clipboard, keeping it highlighted until the next click (standard terminal behavior).
    fn mouse_release(&mut self, x: f64, y: f64) {
        if self.mouse_anchor.is_some() {
            self.mouse_drag(x, y);
            self.mouse_anchor = None;
            self.copy_selection();
        }
    }

    /// Copy the active session's text selection to the system clipboard. No-op when empty.
    fn copy_selection(&mut self) {
        let Some(active) = self.app.active_session() else { return };
        let g = active.term.lock();
        let Some(text) = g.selection_to_string() else { return };
        if text.is_empty() {
            return;
        }
        if let Ok(mut cb) = arboard::Clipboard::new() {
            let _ = cb.set_text(text);
        }
    }
}

/// Map a typed letter to its control byte (a-z → 1-26), for Ctrl+key.
fn ctrl_byte(c: &str) -> Option<u8> {
    let ch = c.chars().next()?;
    let l = ch.to_ascii_lowercase();
    if ('a'..='z').contains(&l) {
        Some(l as u8 - b'a' + 1)
    } else {
        None
    }
}

impl ApplicationHandler for Application {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attribs = winit::window::Window::default_attributes()
            .with_title("harness-terminal")
            .with_inner_size(Size::Logical(LogicalSize::new(110.0, 34.0)));
        match event_loop.create_window(attribs) {
            Ok(w) => {
                let w = Rc::new(w);
                self.window = Some(Rc::clone(&w));
                self.context = Context::new(Rc::clone(&w)).ok();
                self.surface = self.context.as_ref().and_then(|c| {
                    Surface::new(c, Rc::clone(&w)).ok()
                });
                self.size = w.inner_size();
                self.metrics_from_scale();
                w.request_redraw();
            }
            Err(e) => {
                eprintln!("harness-terminal: failed to create window: {e}");
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                self.size = size;
                if size.width > 0 && size.height > 0 {
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                self.metrics_from_scale();
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.mods = mods.state();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    let mods = self.mods;
                    self.handle_key(&event.logical_key, &mods);
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x, position.y);
                if self.mouse_anchor.is_some() && self.app.overlay == Overlay::None {
                    self.mouse_drag(position.x, position.y);
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // Scroll wheel navigates the scrollback: up = back into history, down = forward.
                // Natural/line/other deltas are normalized to a sign; each notch ~3 lines keeps it
                // snappy without racing the keyboard.
                use winit::event::MouseScrollDelta;
                let mag = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(p) => -p.y / 40.0,
                };
                if mag == 0.0 {
                    return;
                }
                // Positive magnitude = scroll up (into history), negative = down (toward live).
                let lines = (mag * 3.0) as i32;
                scroll_active(self, lines);
                if lines > 0 {
                    self.scrolled = true;
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if button == MouseButton::Left && self.app.overlay == Overlay::None {
                    let (x, y) = self.cursor;
                    match state {
                        ElementState::Pressed => {
                            if self.mods.alt_key() {
                                // Alt+click moves the shell cursor instead of selecting.
                                self.mouse_alt_click(x, y);
                                return;
                            }
                            self.mouse_press(x, y);
                            if let Some(w) = &self.window {
                                w.request_redraw();
                            }
                        }
                        ElementState::Released => {
                            self.mouse_release(x, y);
                            if let Some(w) = &self.window {
                                w.request_redraw();
                            }
                        }
                    }
                }
            }
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        self.app.reconnect_sweep();
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}

/// Entry point: create the native window and run the event loop.
pub fn run(app: App) -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = EventLoop::new()?;
    let mut application = Application::new(app);
    event_loop.run_app(&mut application)?;
    Ok(())
}
