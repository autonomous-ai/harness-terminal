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
use crate::render::{argb, draw_grid, draw_text, Framebuffer, GlyphCache};
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
    /// The tab that was active before the current one, so prefix+l can flip back to it (tmux
    /// last-window muscle memory). Cleared to None when tabs get rearranged out from under it.
    last_active: Option<usize>,
    /// True while the view is scrolled into history (live follow suspended). Set when the user
    /// scrolls up; cleared when they return to the bottom (scroll command / Esc) or new output
    /// resets the display offset in `render`.
    scrolled: bool,
    /// Active search query ("" when the Find overlay is closed).
    find_query: String,
    /// In-progress rename for the active tab ("" when the Rename overlay is closed).
    rename_query: String,
    /// The currently-focused search match (absolute line, col, width); recomputed on each query
    /// change / Enter and passed to draw_grid for highlighting.
    find_hit: Option<crate::render::Find>,
    /// Every match of the active query (line, col, width), so draw_grid can highlight all of them
    /// in yellow while the focused one shows orange.
    find_all: Vec<crate::render::Find>,
    /// Index into `find_all` of the currently-focused match (the "N of M" cursor).
    find_index: usize,
    /// Whether we're in tmux-style copy mode (prefix+[). While active, keystrokes navigate a read
    /// cursor instead of reaching the shell, and `v` starts/extends a selection to copy.
    copy_mode: bool,
    /// Copy-mode read cursor: (line, col) grid coordinates in the scrollback.
    copy_pos: (i32, usize),
    /// Copy-mode anchor: where the block selection started (Some while selecting), in grid coords.
    copy_anchor: Option<(i32, usize)>,
    /// Mouse state: the cell anchor where a drag-selection started (Some while left button held).
    /// With winit 0.30 we track presses/releases ourselves; dragging updates the selection end.
    mouse_anchor: Option<Point>,
    /// Latest cursor position in framebuffer px (winit's MouseInput has no position; we read this).
    cursor: (f64, f64),
    /// Last (press-time, press-position, accumulated-click-count) to detect double/triple clicks.
    /// winit 0.30 doesn't hand us a click count, so we time consecutive presses ourselves.
    last_press: Option<(std::time::Instant, (f64, f64), u32)>,
    /// Last OS window title we set, so we only call set_title when it changes (each call is a
    /// platform round-trip).
    window_title: String,
    /// Base font scale (1.0 = 14px). Zoom multiplies font/margins; persisted in restore.
    zoom: f32,
    /// Base font size in px from config (the size a fresh window opens at, before display-scale
    /// and zoom). `zoom` still scales on top; Ctrl+0 resets zoom but not this.
    base_font: f32,
    /// Last-seen scrollback line count per tab index, used to flag tabs that produced output while
    /// not focused. `None` until a tab has been sampled once (so a freshly-spawned tab doesn't
    /// immediately badge).
    seen_history: Vec<usize>,
}

impl Application {
    fn new(app: App) -> Self {
        let base_font = crate::config::Config::load().font_px as f32;
        let seen_history = vec![usize::MAX; app.tabs.len()];
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
            last_active: None,
            scrolled: false,
            find_query: String::new(),
            rename_query: String::new(),
            find_hit: None,
            find_all: Vec::new(),
            find_index: 0,
            copy_mode: false,
            copy_pos: (0, 0),
            copy_anchor: None,
            mouse_anchor: None,
            cursor: (0.0, 0.0),
            last_press: None,
            window_title: String::new(),
            zoom: crate::restore::load_zoom(),
            base_font,
            seen_history,
        }
    }

    fn metrics_from_scale(&mut self) {
        if let Some(w) = &self.window {
            let s = w.scale_factor() as f32 * self.zoom;
            self.font_px = (self.base_font * s).round().clamp(8.0, 40.0) as u32;
            self.cell_w = (8.0 * s).round().max(2.0) as u32;
            self.cell_h = (18.0 * s).round() as u32;
        }
    }

    /// Zoom the terminal font by a multiplicative factor, applied on top of the display scale.
    /// Clamps so the grid never becomes unusable, then re-derives cell metrics.
    fn zoom_font(&mut self, delta: f32) {
        self.zoom = (self.zoom * delta).clamp(0.5, 3.0);
        crate::restore::save_zoom(self.zoom);
        self.metrics_from_scale();
        if let Some(active) = self.app.active_session() {
            let lines = (self.size.height - self.cell_h * 2) as usize / self.cell_h as usize;
            let cols = self.size.width as usize / self.cell_w as usize;
            active.resize(crate::session::TermSize { lines, cols });
        }
    }

    /// Return which tab indices have produced output since we last looked at them (are
    /// backgrounded AND have grown scrollback since our last sample). The focused tab is never
    /// flagged. Unknown/gone tabs (never sampled) are not flagged.
    fn activity_flags(&mut self) -> Vec<bool> {
        let n = self.app.tabs.len();
        self.seen_history.resize(n, usize::MAX);
        let mut flags = vec![false; n];
        for (i, s) in self.app.tabs.iter().enumerate() {
            if i == self.app.active {
                // We're looking at it now: re-baseline and don't flag.
                self.seen_history[i] = s.history_len();
                continue;
            }
            let len = s.history_len();
            let grew = self.seen_history[i] != usize::MAX && len > self.seen_history[i];
            self.seen_history[i] = len;
            flags[i] = grew;
        }
        flags
    }

    /// Focus tab `i` and remember it (so a relaunch opens on the same tab).
    fn set_active(&mut self, i: usize) {
        if i != self.app.active && self.app.active < self.app.tabs.len() {
            self.last_active = Some(self.app.active);
        }
        self.app.active = i.min(self.app.tabs.len().saturating_sub(1));
        crate::restore::save_active(self.app.active);
    }

    /// `prefix+l`: flip to the tab that was active just before this one (tmux last-window).
    /// Repeated presses ping-pong, since swapping focus swaps `last_active`.
    fn last_window(&mut self) {
        if let Some(prev) = self.last_active.take() {
            if prev < self.app.tabs.len() {
                self.set_active(prev);
            }
        }
    }

    /// `prefix+o`: jump to the next backgrounded tab that produced output since we last looked,
    /// wrapping around. Makes the activity badge actionable — a key to reach 'the one that just
    /// did something'. If none is flagged, does nothing.
    fn next_busy(&mut self) {
        let flags = self.activity_flags();
        let n = self.app.tabs.len();
        if n == 0 {
            return;
        }
        for step in 1..=n {
            let i = (self.app.active + step) % n;
            if flags[i] {
                self.set_active(i);
                return;
            }
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

        // Sync the OS window title to the active tab's live OSC title so the fleet Diver sees at a
        // glance what the pane is doing even when the window is minimized/unfocused. Only call
        // set_title when the string actually changed.
        if let Some(w) = &self.window {
            let title = self
                .app
                .active_session()
                .and_then(|s| s.live_title())
                .unwrap_or_else(|| "harness-terminal".to_string());
            let title = format!("{} — harness-terminal", title);
            if title != self.window_title {
                w.set_title(&title);
                self.window_title = title;
            }
        }

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
            let copy = if self.copy_mode { Some(self.copy_pos) } else { None };
            draw_grid(fb, &g, self.cell_w, self.cell_h, self.font_px, &mut self.cache, self.find_hit, &self.find_all, sel.as_ref(), copy);
        } else {
            // No sessions open — draw a short 'how to start' hint so the window isn't a blank void.
            let cy = term_top + (grid_lines / 2) * gline_px;
            let hint = "no sessions ·  Ctrl+Space n  new   Ctrl+Space r  attach remote   Ctrl+Space /  palette ";
            let hw = draw_text(fb, &mut self.cache, hint, 0, cy, self.font_px, CHROME_DIM);
            let cx = fb.width.saturating_sub(hw) / 2;
            // Blank the row so the sizing pass above doesn't leave a left-aligned ghost, then paint
            // a single centered copy. (All pixels are black here to begin; clearing is a no-op but
            // keeps this robust if the grid later draws under the hint.)
            for py in cy.saturating_sub(self.font_px as usize)..(cy + self.font_px as usize) {
                for px in 0..fb.width {
                    if py < fb.height {
                        fb.pixels[py * fb.width + px] = argb(0, 0, 0, 0);
                    }
                }
            }
            draw_text(fb, &mut self.cache, hint, cx, cy, self.font_px, CHROME_DIM);
        }

        // Tab bar (top row). Flag backgrounded tabs that produced output since we last looked.
        let activity = self.activity_flags();
        let tab_base = self.cell_h as usize / 2;
        let mut x = 6usize;
        for (i, s) in self.app.tabs.iter().enumerate() {
            let active = i == self.app.active;
            // Color the active tab's dot by its host so a fleet diver reads 'which machine' at a
            // glance; matches the host hue used across tabs and it's stable across restarts.
            let dot = if active { "●" } else { "○" };
            // Show the pane's live OSC title (what the agent is doing) when it has announced one.
            let live = s.live_title().unwrap_or_else(|| s.meta.title.clone());
            let mut live = live.replace('\n', " ");
            if live.chars().count() > 18 {
                live = live.chars().take(18).collect::<String>() + "…";
            }
            // An exclamation marks a backgrounded tab that has scrolled since we last sampled it.
            let flag = if activity[i] { "!" } else { "" };
            // Show the user's rename if set; otherwise the plain engine id.
            let head = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
            let label = format!(" {}{} {} {} ", flag, head, live, dot);
            let color = if active { host_color(&s.meta.host) } else { CHROME_DIM };
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
            let head = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
            info = format!(" {} · {} · {} · [{} {}]", s.meta.host, head, live, s.kind(), link);
        }
        // When the viewport is scrolled back from the live bottom, say so — a dead giveaway that
        // keys won't take you to fresh output until you press Escape (or the b key).
        if self.scrolled {
            info += "  ▾ scrolled (Esc/b to bottom)";
        }
        draw_text(fb, &mut self.cache, &info, 6, status_base, self.font_px, CHROME_FG);
        let hints = " prefix+/ palette  prefix+n new  prefix+r remote  prefix+s fleet  prefix+o busy  prefix+[ copy  prefix+l last  prefix+? help  prefix+q quit ";
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

        // Copy mode banner: a prominent green status bar so the user knows keystrokes are captured
        // for navigation, with the current motion hints.
        if self.copy_mode {
            let selecting = if self.copy_anchor.is_some() { "[selecting]" } else { "[v=select]" };
            let msg = format!(" COPY MODE · h/j/k/l/w/b move {} · Enter copy · Esc quit ", selecting);
            let cw = draw_text(fb, &mut self.cache, &msg, 6, status_base, self.font_px, (0x00, 0x00, 0x00));
            // Clear the region background to green behind the message for contrast.
            let green = argb(255, 0x18, 0xe0, 0x8a);
            for py in status_base.saturating_sub(self.font_px as usize)..(status_base + self.font_px as usize) {
                for px in 0..cw.min(fb.width) {
                    if py < fb.height {
                        fb.pixels[py * fb.width + px] = green;
                    }
                }
            }
            // Re-draw the message in black on green.
            draw_text(fb, &mut self.cache, &msg, 6, status_base, self.font_px, (0x00, 0x00, 0x00));
        }

        // Overlays.
        match self.app.overlay {
            Overlay::Palette => self.render_palette(fb),
            Overlay::NewSession => self.render_list(fb, "  new session  ", true),
            Overlay::RemoteAttach => self.render_remote(fb),
            Overlay::Find => self.render_find(fb),
            Overlay::Fleet => self.render_fleet(fb),
            Overlay::Help => self.render_help(fb),
            Overlay::Rename => self.render_rename(fb),
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
            let name = s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone());
            let line = format!("  {} · {} · {}  {}", s.meta.host, name, s.meta.title, if sel { "◄" } else { "" });
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

    /// Read-only fleet panel: the machine id + tunnel state, then one line per harness session with
    /// a live/stale marker. Esc (or any key) dismisses; `s` re-fetches. Never writes to a pane.
    fn render_fleet(&mut self, fb: &mut Framebuffer) {
        let (base_y, line_px) = self.overlay_base_y();
        let f = &self.app.fleet;
        let mid = if f.machine_id.is_empty() { "unknown".to_string() } else { f.machine_id.chars().take(6).collect() };
        let tunnel = if f.connected { "tunnel up" } else { "tunnel down" };
        let n = f.fleet.len();
        draw_text(fb, &mut self.cache, &format!("  fleet · {} · {} · {} session{}  ", mid, tunnel, n, if n == 1 { "" } else { "s" }), 32, base_y, self.font_px, WHITE);
        if n == 0 {
            draw_text(fb, &mut self.cache, "  no harness sessions (daemon unreachable or nothing joined)  ", 32, base_y + line_px, self.font_px, CHROME_DIM);
            return;
        }
        for (i, s) in f.fleet.iter().enumerate().take(20) {
            let live = s.is_live();
            let mark = if live { "●" } else { "○" };
            let color = if live { (0x4a, 0xe0, 0x8a) } else { CHROME_DIM };
            let eng = if s.engine.is_empty() { "?" } else { s.engine.as_str() };
            let id = if s.session_id.is_empty() { s.tmux_pane.clone() } else { s.session_id.chars().take(8).collect() };
            let line = format!("  {} {}  {:<9} {}", mark, eng, "", id);
            draw_text(fb, &mut self.cache, &line, 32, base_y + (i + 1) * line_px, self.font_px, color);
        }
    }

    /// Keybinding reference overlay. Static list; dismiss on any key.
    fn render_help(&mut self, fb: &mut Framebuffer) {
        let (base_y, line_px) = self.overlay_base_y();
        draw_text(fb, &mut self.cache, "  harness-terminal keys  ", 32, base_y, self.font_px, WHITE);
        let bindings: [(&str, &str); 18] = [
            ("Ctrl+Space", "prefix (then a command)"),
            ("prefix /", "palette: jump to any session"),
            ("prefix n", "new session (engine picker)"),
            ("prefix r", "attach to a remote pane@host"),
            ("prefix s", "fleet status"),
            ("prefix f", "search scrollback"),
            ("prefix [", "copy mode"),
            ("prefix ,", "rename the active tab"),
            ("prefix ?", "this help"),
            ("1-9 / Tab", "switch tab"),
            ("prefix o", "jump to next busy tab"),
            ("prefix l", "flip to the previous tab"),
            ("x / c", "close tab / go to tab 0"),
            ("g / b", "scroll up a page / jump to bottom"),
            ("Ctrl+= / Ctrl+-", "font zoom (Ctrl+0 reset)"),
            ("PgUp/PgDn", "scrollback"),
            ("Cmd/Ctrl+click", "open URL / file path"),
            ("prefix q", "quit"),
        ];
        for (row, (k, d)) in bindings.iter().enumerate() {
            draw_text(fb, &mut self.cache, &format!("  {:<14} {}", k, d), 32, base_y + (row + 1) * line_px, self.font_px, CHROME_DIM);
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

    /// Enter copy mode: anchor the read cursor at the top-left visible cell so the user starts
    /// where they can see, and keep the view scrolled (copy mode lives in the scrollback).
    fn start_copy_mode(&mut self) {
        let Some(active) = self.app.active_session() else { return };
        let g = active.term.lock();
        if g.grid().history_size() == 0 {
            return; // nothing to scroll/copy yet
        }
        // Place the cursor at the first visible (top of viewport) cell.
        let top = g.grid().display_offset();
        self.copy_pos = ((top as i32 * -1), 0);
        self.copy_anchor = None;
        self.copy_mode = true;
        self.scrolled = true;
    }

    /// Copy the current copy-mode selection (anchor→pos inclusive) to the clipboard via
    /// selection_to_string, then leave copy mode. No-op if nothing is selected.
    fn copy_mode_copy(&mut self) {
        if self.copy_anchor.is_none() {
            self.copy_mode = false;
            return;
        }
        let Some(active) = self.app.active_session() else { return };
        let mut g = active.term.lock();
        // Build a simple rectangular selection from the two grid points, install it as the live
        // selection, and read it via alacritty's range-to-string so line handling (wrap vs hard
        // newline) is correct. We restore/clear the live selection afterward.
        let (a, b) = (self.copy_anchor.unwrap(), self.copy_pos);
        let (lo, hi) = if (a, b) < (b, a) { (a, b) } else { (b, a) };
        use alacritty_terminal::index::{Column, Line, Point, Side};
        use alacritty_terminal::selection::{Selection, SelectionType};
        let start = Point::new(Line(lo.0), Column(lo.1));
        let end = Point::new(Line(hi.0), Column(hi.1));
        let mut sel = Selection::new(SelectionType::Simple, start, Side::Left);
        sel.update(end, Side::Right);
        g.selection = Some(sel);
        let text = g.selection_to_string().unwrap_or_default();
        g.selection = None;
        self.copy_mode = false;
        self.copy_anchor = None;
        if !text.is_empty() {
            drop(g);
            if let Ok(mut cb) = arboard::Clipboard::new() {
                let _ = cb.set_text(text);
            }
        }
    }

    /// Move the copy-mode read cursor by a grid delta, keeping it in-bounds and extending the
    /// selection if one is active.
    fn copy_move(&mut self, dl: i32, dc: i32) {
        let Some(active) = self.app.active_session() else { return };
        let g = active.term.lock();
        let cols = g.columns() as i32;
        let max_line = g.grid().bottommost_line().0;
        let min_line = g.grid().topmost_line().0;
        let (l, c) = self.copy_pos;
        let l = (l + dl).clamp(min_line, max_line);
        let c = (c as i32 + dc).clamp(0, cols - 1) as usize;
        self.copy_pos = (l, c);
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

    /// Render the rename overlay: show what the tab is currently called and the in-progress name.
    fn render_rename(&mut self, fb: &mut Framebuffer) {
        let status_base = fb.height.saturating_sub(self.cell_h as usize / 2);
        let cur = self
            .app
            .active_session()
            .map(|s| s.meta.name.clone().unwrap_or_else(|| s.meta.engine.clone()))
            .unwrap_or_default();
        let prompt = if self.rename_query.is_empty() {
            "  rename tab: (type a name, Enter keeps, Esc cancels)  ".to_string()
        } else {
            format!("  rename tab: {} ▏", self.rename_query)
        };
        draw_text(fb, &mut self.cache, &format!("  currently: {}  ", cur), 6, status_base - self.cell_h as usize, self.font_px, CHROME_DIM);
        draw_text(fb, &mut self.cache, &prompt, 6, status_base, self.font_px, WHITE);
    }

    /// Handle a key while in copy mode: vim-style motion keys move the read cursor; `v` starts
    /// (or re-anchors) a selection; Enter/Space copies and exits; Esc/Q exits; g/G go to top/bottom.
    fn handle_copy_key(&mut self, key: &Key, _mods: &ModifiersState) {
        match key {
            Key::Character(c) => match c.as_str() {
                // vim motions
                "h" | "j" | "k" | "l" | "w" | "b" => {
                    let (dl, dc) = match c.as_str() {
                        "h" => (0, -1),
                        "j" => (1, 0),
                        "k" => (-1, 0),
                        "l" | " " => (0, 1),
                        "w" => { self.copy_word(true); (0, 0) }
                        "b" => { self.copy_word(false); (0, 0) }
                        _ => (0, 0),
                    };
                    self.copy_move(dl, dc);
                }
                "v" => {
                    // Start/re-anchor the selection at the read cursor.
                    self.copy_anchor = Some(self.copy_pos);
                }
                "g" => {
                    let Some(active) = self.app.active_session() else { return };
                    let g = active.term.lock();
                    self.copy_pos = (g.grid().topmost_line().0, 0);
                }
                "G" => {
                    let Some(active) = self.app.active_session() else { return };
                    let g = active.term.lock();
                    self.copy_pos = (g.grid().bottommost_line().0, 0);
                }
                "q" => { self.copy_mode = false; self.copy_anchor = None; }
                _ => {}
            },
            Key::Named(n) => match n {
                winit::keyboard::NamedKey::Enter => self.copy_mode_copy(),
                winit::keyboard::NamedKey::Space => self.copy_mode_copy(),
                winit::keyboard::NamedKey::ArrowUp => self.copy_move(-1, 0),
                winit::keyboard::NamedKey::ArrowDown => self.copy_move(1, 0),
                winit::keyboard::NamedKey::ArrowLeft => self.copy_move(0, -1),
                winit::keyboard::NamedKey::ArrowRight => self.copy_move(0, 1),
                winit::keyboard::NamedKey::PageUp => self.copy_move(-20, 0),
                winit::keyboard::NamedKey::PageDown => self.copy_move(20, 0),
                winit::keyboard::NamedKey::Escape => { self.copy_mode = false; self.copy_anchor = None; }
                _ => {}
            },
            _ => {}
        }
    }

    /// Jump the copy cursor to the next/previous word boundary. `forward` moves right, otherwise
    /// left. Wraps selection extension implicitly because it just moves `copy_pos`.
    fn copy_word(&mut self, forward: bool) {
        let Some(active) = self.app.active_session() else { return };
        let g = active.term.lock();
        let cols = g.columns();
        let (mut l, mut cur) = self.copy_pos;
        let max_line = g.grid().bottommost_line().0;
        // Work on the current line's text; map from copy_pos col to a byte index.
        use alacritty_terminal::index::{Column, Line};
        let line_text: String = g.grid()[Line(l)][Column(0)..Column(cols)].iter().map(|c| c.c).collect();
        let ci = cur.min(line_text.len().saturating_sub(1));
        let bytes = line_text.as_bytes();
        let is_space_at = |i: usize| bytes.get(i).map_or(true, |b| b.is_ascii_whitespace());
        if forward {
            // Skip current word (or spaces), land on next word start.
            let mut i = ci;
            // If on a space, skip spaces first.
            while i < bytes.len() && is_space_at(i) {
                i += 1;
            }
            while i < bytes.len() && !is_space_at(i) {
                i += 1;
            }
            // If still within the line, land at the next non-space.
            let mut ni = i;
            while ni < bytes.len() && is_space_at(ni) {
                ni += 1;
            }
            if ni < bytes.len() {
                cur = ni;
            } else if l < max_line {
                l += 1;
                cur = 0;
            }
        } else {
            // Walk back over the current word, then any spaces, to the previous word's start.
            let mut i = ci;
            while i > 0 && !is_space_at(i.saturating_sub(1)) {
                i -= 1;
            }
            // i is now the start of the current word (or 0). Skip spaces before it.
            let mut si = i;
            while si > 0 && is_space_at(si.saturating_sub(1)) {
                si -= 1;
            }
            cur = if si < i { si } else { i };
        }
        self.copy_pos = (l, cur);
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
                "n" => { self.app.overlay = Overlay::NewSession; self.app.select_default_engine(); }
                "r" => { self.app.overlay = Overlay::RemoteAttach; self.app.remote_host.clear(); self.app.selected = 0; }
                "t" => self.app.spawn_tmux("this-host", "shell"),
                "q" => return true,
                "s" => {
                    // Read-only fleet overlay: fetch status on open so it's fresh, then show it.
                    if let Ok(st) = crate::harness::HarnessClient::local().status() {
                        self.app.fleet = st;
                    }
                    self.app.overlay = Overlay::Fleet;
                }
                "c" => { if !self.app.tabs.is_empty() { self.set_active(0); } }
                "o" => self.next_busy(),
                "l" => self.last_window(),
                "x" => { close_tab(&mut self.app); }
                "g" => { scroll_active(self, 20); self.scrolled = true; }
                "b" => self.scroll_to_bottom(),
                "f" => { self.app.overlay = Overlay::Find; self.find_query.clear(); self.find_hit = None; self.find_all = Vec::new(); },
                "[" => self.start_copy_mode(),
                "?" => { self.app.overlay = Overlay::Help; }
                "," => {
                    // Rename the active tab. Pre-fill with the current custom name (if any) so
                    // editing doesn't start from scratch.
                    self.rename_query = self
                        .app
                        .active_session()
                        .map(|s| s.meta.name.clone().unwrap_or_default())
                        .unwrap_or_default();
                    self.app.overlay = Overlay::Rename;
                }
                // Numeric tabs 1-9.
                _ if c.len() == 1 && c.chars().next().unwrap().is_ascii_digit() => {
                    let idx = c.chars().next().unwrap() as u8;
                    if (b'1'..=b'9').contains(&idx) {
                        let i = (idx - b'1') as usize;
                        if i < self.app.tabs.len() { self.set_active(i); }
                    }
                }
                _ => {}
            },
            Key::Named(n) => match n {
                winit::keyboard::NamedKey::Tab => {
                    if !self.app.tabs.is_empty() {
                        self.set_active((self.app.active + 1) % self.app.tabs.len());
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
            Overlay::Fleet => {
                // Read-only: any key closes it. `s` re-fetches for a fresh view.
                if matches!(key, Key::Named(winit::keyboard::NamedKey::Escape)) || matches!(key, Key::Character(c) if c != "s") {
                    self.app.overlay = Overlay::None;
                } else if let Ok(st) = crate::harness::HarnessClient::local().status() {
                    self.app.fleet = st;
                }
                return;
            }
            Overlay::Help => { self.app.overlay = Overlay::None; return; }
            Overlay::Rename => {
                match key {
                    Key::Character(c) => { self.rename_query.push_str(c); }
                    Key::Named(n) => match n {
                        winit::keyboard::NamedKey::Enter => {
                            // Commit the rename (empty = clear back to the default engine label).
                            let name = if self.rename_query.trim().is_empty() { None } else { Some(self.rename_query.trim().to_string()) };
                            if let Some(s) = self.app.active_session_mut() {
                                s.meta.name = name;
                            }
                            self.app.overlay = Overlay::None;
                        }
                        winit::keyboard::NamedKey::Backspace => { self.rename_query.pop(); }
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
            // Font zoom (Ctrl+= / Ctrl+- / Ctrl+0 to reset) — captured before anything reaches the
            // shell, like any terminal's, and a persistent per-window preference.
            if mods.control_key() {
                if let Key::Character(c) = key {
                    match c.as_str() {
                        "=" | "+" => { self.zoom_font(1.1); return; }
                        "-" => { self.zoom_font(1.0 / 1.1); return; }
                        "0" => { self.zoom = 1.0; crate::restore::save_zoom(1.0); self.metrics_from_scale(); return; }
                        _ => {}
                    }
                }
            }
            // Copy mode intercepts keystrokes (navigation + selection) instead of forwarding.
            if self.copy_mode {
                self.handle_copy_key(key, mods);
                return;
            }
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
        crate::restore::save(&app.tab_specs());
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

    /// Cmd/Ctrl+click: jump to whatever is under the cursor. A URL opens in the default browser; a
    /// relative path opens in a text editor via `open` (macOS). Reads the word containing the
    /// clicked cell by expanding to the nearest whitespace/bracket boundaries on the grid row.
    /// Best-effort — a click on non-text just does nothing.
    fn mouse_open(&mut self, x: f64, y: f64) {
        let Some(pt) = self.mouse_to_cell(x, y) else { return };
        if self.scrolled || pt.line.0 < 0 {
            return;
        }
        let Some(active) = self.app.active_session() else { return };
        let g = active.term.lock();
        let row = pt.line.0 as usize;
        let cols = g.columns();
        if row >= g.screen_lines() {
            return;
        }
        let col = (pt.column.0 as usize).min(cols - 1);
        // Read the whole visible row and expand left/right from the click to word boundaries.
        let line_text: String = g.grid()[Line(row as i32)][Column(0)..Column(cols)]
            .iter().map(|c| c.c).collect();
        drop(g);
        let word = expand_click_word(&line_text, col);
        if word.is_empty() {
            return;
        }
        // Shell it through `open`: macOS routes `http(s)://…` to the browser and a relative path to
        // a text editor (XDG on Linux needs a different incantation; we're the mac build today).
        let _ = std::process::Command::new("open").arg(word).spawn();
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

/// A stable accent color for a host string, so every tab pointing at the same machine (and the
/// details pane) shares a hue and a fleet diver can tell at a glance which host a tab is on.
/// Deterministic — same host always yields the same color, across sessions and restarts.
fn host_color(host: &str) -> (u8, u8, u8) {
    // FNV-1a over the host; pick a hue from the warm-to-cool range and keep it readable on black.
    let h = host.bytes().fold(0x811c_9dc5u32, |acc, b| (acc ^ b as u32).wrapping_mul(0x0100_0193));
    // 32 hues across the spectrum, OSV below is luminance-boosted so text reads on near-black.
    let hue = (h >> 4) % 32;
    const TABLE: [(u8, u8, u8); 32] = [
        (0xe0, 0x5b, 0x5b), (0xe0, 0x8b, 0x5b), (0xe0, 0xb8, 0x5b), (0xbf, 0xe0, 0x5b),
        (0x8b, 0xe0, 0x5b), (0x5b, 0xe0, 0x8b), (0x5b, 0xe0, 0xbf), (0x5b, 0xdd, 0xe0),
        (0x5b, 0xa8, 0xe0), (0x5b, 0x74, 0xe0), (0x8b, 0x5b, 0xe0), (0xbf, 0x5b, 0xe0),
        (0xe0, 0x5b, 0xd0), (0xe0, 0x5b, 0x9b), (0x9b, 0x7b, 0x5b), (0x9b, 0x9b, 0x5b),
        (0x5b, 0x9b, 0x7b), (0x5b, 0x7b, 0x9b), (0x7b, 0x5b, 0x9b), (0x9b, 0x5b, 0x7b),
        (0xf7, 0x9e, 0x8b), (0xf7, 0xbe, 0x8b), (0xd9, 0xf7, 0x8b), (0xa7, 0xf7, 0x8b),
        (0x8b, 0xf7, 0xa7), (0x8b, 0xf7, 0xd9), (0x8b, 0xdd, 0xf7), (0x8b, 0xa7, 0xf7),
        (0xa7, 0x8b, 0xf7), (0xd9, 0x8b, 0xf7), (0xf7, 0x8b, 0xd9), (0xf7, 0x8b, 0xa7),
    ];
    TABLE[(hue % 32) as usize]
}

/// Expand the word containing byte index `col` in a line of text, growing to whitespace/bracket
/// boundaries on both sides. Returns the substring (may be empty if `col` sits on a boundary).
fn expand_click_word(line: &str, col: usize) -> &str {
    let bytes = line.as_bytes();
    let col = col.min(bytes.len());
    let is_boundary = |b: u8| b.is_ascii_whitespace() || matches!(b, b'(' | b')' | b'"' | b'\'' | b'<' | b'>' | b'[' | b']');
    let mut start = col;
    while start > 0 && !is_boundary(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = col;
    while end < bytes.len() && !is_boundary(bytes[end]) {
        end += 1;
    }
    &line[start..end]
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
        let size = crate::restore::load_geometry()
            .map(|(w, h)| Size::Physical(PhysicalSize::new(w, h)))
            .unwrap_or(Size::Logical(LogicalSize::new(110.0, 34.0)));
        let attribs = winit::window::Window::default_attributes()
            .with_title("harness-terminal")
            .with_inner_size(size);
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
            WindowEvent::CloseRequested => {
                // Persist open tabs so they come back on the next launch; keep the window size too.
                crate::restore::save(&self.app.tab_specs());
                crate::restore::save_geometry(self.size.width, self.size.height);
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                self.size = size;
                if size.width > 0 && size.height > 0 {
                    crate::restore::save_geometry(size.width, size.height);
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
                            if self.mods.control_key() || self.mods.super_key() {
                                // Cmd/Ctrl+click opens the URL/path under the cursor.
                                self.mouse_open(x, y);
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

#[cfg(test)]
mod tests {
    use super::{expand_click_word, host_color};

    /// Every host gets a stable, in-table color; the same host never changes across calls, and two
    /// different hosts can share a color (fine) but the mapping is deterministic.
    #[test]
    fn host_color_is_deterministic() {
        let a = host_color("build-host");
        assert_eq!(a, host_color("build-host"));
        let b = host_color("10.0.0.4");
        assert_eq!(b, host_color("10.0.0.4"));
        assert_ne!(a, (0, 0, 0), "colors must be visible on black");
    }

    /// Cmd+click word expansion picks the whole token, not the shell quoting around it.
    #[test]
    fn click_word_expands_to_token() {
        let line = "see https://example.com/foo in the log (src/main.rs)";
        assert_eq!(expand_click_word(line, 25), "https://example.com/foo");
        // The substring is a byte index into the line; find it by locating the token start.
        let url_start = line.find("https").unwrap();
        assert_eq!(expand_click_word(line, url_start + 5), "https://example.com/foo");
        // A path inside parens expands to the path, stopping at ')'.
        let p = line.find("src/main.rs").unwrap();
        assert_eq!(expand_click_word(line, p), "src/main.rs");
        // A click on a boundary returns an empty token.
        assert_eq!(expand_click_word("   ", 1), "");
    }

    /// A single word on its own line is returned whole.
    #[test]
    fn click_word_single_token() {
        assert_eq!(expand_click_word("README.md", 3), "README.md");
    }
}
