//! Standalone native window (our terminal — no host emulator).
//!
//! winit provides the window + event loop; softbuffer provides a CPU framebuffer we draw the
//! alacritty grid into; ab_glyph rasterizes glyphs. This replaces the ratatui/crossterm TUI as the
//! default shell — the fleet/tunnel/reconnect machinery in `session.rs`/`transport.rs` is untouched
//! and shared. Chrome (tab bar, palette, status) is drawn natively with `draw_text`.

use std::num::NonZeroU32;
use std::rc::Rc;

use alacritty_terminal::grid::Dimensions;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalSize, Size};
use winit::event::{ElementState, WindowEvent};
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
            let g = active.term.lock();
            draw_grid(fb, &g, self.cell_w, self.cell_h, self.font_px, &mut self.cache);
        }

        // Tab bar (top row).
        let tab_base = self.cell_h as usize / 2;
        let mut x = 6usize;
        for (i, s) in self.app.tabs.iter().enumerate() {
            let active = i == self.app.active;
            let dot = if active { "●" } else { "○" };
            let label = format!(" {} {} ", s.meta.engine, dot);
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
            info = format!(" {} · {} · {} · [{} {}]", s.meta.host, s.meta.engine, s.meta.title, s.kind(), link);
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
            Overlay::None => {}
        }

        // Normal mode: send keystrokes to the active session.
        if self.app.overlay == Overlay::None {
            if let Some(s) = self.app.active_session_mut() {
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
