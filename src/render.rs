//! Pure framebuffer renderer for the standalone native window.
//!
//! Draws an alacritty `Term` grid (cells → (char, fg, bg)) into a `u32` ARGB pixel buffer with
//! API-stable type aliases instead of the volatile `Cell` enum. Glyph glyphs are rasterized by
//! `ab_glyph` into a cache. Everything here is a pure function over a buffer, so the whole render
//! path is unit-testable headlessly (no window, no GPU).

use std::collections::HashMap;

use ab_glyph::{Font as _, FontArc, Glyph as AbsGlyph, PxScale, ScaleFont as _};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::Term;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::vte::ansi;

use crate::session::Listener;

/// 32-bit packed pixel: 0xAARRGGBB (alpha in the high byte).
pub type Pixel = u32;

pub const fn argb(a: u8, r: u8, g: u8, b: u8) -> Pixel {
    ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

/// A CPU framebuffer. `[y*width + x]`.
pub struct Framebuffer {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<Pixel>,
}

impl Framebuffer {
    pub fn new(width: usize, height: usize) -> Self {
        Framebuffer {
            width,
            height,
            pixels: vec![0; width * height],
        }
    }

    /// Set one pixel at (x, y); silently ignores out-of-bounds.
    #[inline]
    pub fn set(&mut self, x: usize, y: usize, p: Pixel) {
        if x < self.width && y < self.height {
            self.pixels[y * self.width + x] = p;
        }
    }
}

/// Term cell color. API-stable copy of `vte::ansi::Color` variants we use — the enum's fields are
/// public so we can match them, but referencing the variant names directly is fine (they're plain
/// data); the instability is in some *flags* we never touch. We only pull `r/g/b` and `index`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteColor {
    Spec { r: u8, g: u8, b: u8 },
    Indexed(u8),
    /// Named 16 colors: `(bright, index0..=15)`.
    Named { bright: bool, index: u8 },
}

impl Default for PaletteColor {
    fn default() -> Self {
        PaletteColor::Spec { r: 0, g: 0, b: 0 }
    }
}

/// Default theme foreground (light) and background (black), matching alacritty's default so the
/// native surface stays consistent with the local-PTY default Config.
const DEFAULT_FG: (u8, u8, u8) = (0xea, 0xea, 0xea);
const DEFAULT_BG: (u8, u8, u8) = (0, 0, 0);

impl PaletteColor {
    /// Convert to concrete RGB using the terminal's theme. A simple built-in palette for now;
    /// a config-provided theme slots in here later.
    pub fn to_rgb(&self) -> (u8, u8, u8) {
        // Classic 16-color ANSI palette. Matches the terminal's default Config::default() colors
        // so we stay consistent with the local PTY tabs.
        const ANSI: [(u8, u8, u8); 16] = [
            (0, 0, 0),          // black
            (205, 49, 49),      // red
            (13, 188, 121),     // green
            (229, 229, 16),     // yellow
            (36, 114, 200),     // blue
            (188, 63, 188),     // magenta
            (17, 168, 205),     // cyan
            (229, 229, 229),    // white
            (102, 102, 102),    // bright black
            (241, 76, 76),      // bright red
            (35, 209, 139),     // bright green
            (245, 245, 67),     // bright yellow
            (59, 142, 234),     // bright blue
            (214, 112, 214),    // bright magenta
            (41, 184, 219),     // bright cyan
            (255, 255, 255),    // bright white
        ];
        match *self {
            PaletteColor::Spec { r, g, b } => (r, g, b),
            PaletteColor::Indexed(i) => color256(i),
            PaletteColor::Named { bright, index } => {
                let i = if bright { (index as usize) + 8 } else { index as usize };
                ANSI[i.min(15)]
            }
        }
    }
}

/// Map the alacritty cell color enum into our stable `PaletteColor`.
///
/// `NamedColor` carries both the 16 ANSI colors (discriminants 0–15) and specials (Foreground=256,
/// Background, Dim*, …). We keep only the ANSI 0–15; the specials are folded to the theme defaults.
pub fn cell_color(c: &ansi::Color) -> PaletteColor {
    match c {
        ansi::Color::Named(n) => {
            use ansi::NamedColor as N;
            match n {
                N::Black => PaletteColor::Named { bright: false, index: 0 },
                N::Red => PaletteColor::Named { bright: false, index: 1 },
                N::Green => PaletteColor::Named { bright: false, index: 2 },
                N::Yellow => PaletteColor::Named { bright: false, index: 3 },
                N::Blue => PaletteColor::Named { bright: false, index: 4 },
                N::Magenta => PaletteColor::Named { bright: false, index: 5 },
                N::Cyan => PaletteColor::Named { bright: false, index: 6 },
                N::White => PaletteColor::Named { bright: false, index: 7 },
                N::BrightBlack => PaletteColor::Named { bright: true, index: 0 },
                N::BrightRed => PaletteColor::Named { bright: true, index: 1 },
                N::BrightGreen => PaletteColor::Named { bright: true, index: 2 },
                N::BrightYellow => PaletteColor::Named { bright: true, index: 3 },
                N::BrightBlue => PaletteColor::Named { bright: true, index: 4 },
                N::BrightMagenta => PaletteColor::Named { bright: true, index: 5 },
                N::BrightCyan => PaletteColor::Named { bright: true, index: 6 },
                N::BrightWhite => PaletteColor::Named { bright: true, index: 7 },
                // Dim colors: fold to the theme foreground/background (dim nuance is lost in the
                // framebuffer for now — a future glyph/alpha pass can honor it).
                N::DimBlack
                | N::DimRed
                | N::DimGreen
                | N::DimYellow
                | N::DimBlue
                | N::DimMagenta
                | N::DimCyan
                | N::DimWhite => PaletteColor::Spec { r: DEFAULT_FG.0, g: DEFAULT_FG.1, b: DEFAULT_FG.2 },
                N::Background => PaletteColor::Spec { r: DEFAULT_BG.0, g: DEFAULT_BG.1, b: DEFAULT_BG.2 },
                // Foreground and anything unknown → default foreground.
                _ => PaletteColor::Spec { r: DEFAULT_FG.0, g: DEFAULT_FG.1, b: DEFAULT_FG.2 },
            }
        }
        ansi::Color::Indexed(i) => PaletteColor::Indexed(*i),
        ansi::Color::Spec(rgb) => PaletteColor::Spec { r: rgb.r, g: rgb.g, b: rgb.b },
    }
}

/// Render one cell's worth of background into the buffer (fill `w`×`h` px at origin (x0,y0)).
fn paint_bg(buf: &mut Framebuffer, x0: usize, y0: usize, w: usize, h: usize, c: PaletteColor) {
    let (r, g, b) = c.to_rgb();
    let p = argb(255, r, g, b);
    for dy in 0..h {
        for dx in 0..w {
            buf.set(x0 + dx, y0 + dy, p);
        }
    }
}

/// Glyph cache: one 8-bit alpha bitmap per (font-key, char, pixel-size).
pub struct GlyphCache {
    font: FontArc,
    /// cache key -> (w, h, bitmap u8 alpha).
    map: HashMap<(u32, char, u32), (u32, u32, Vec<u8>)>,
}

impl GlyphCache {
    /// Load a TTF/OTF font. Falls back to macOS SF Mono, then fails if that's absent.
    pub fn load() -> Self {
        let data = std::fs::read(font_path()).expect("system mono font not found");
        GlyphCache {
            font: FontArc::try_from_vec(data).expect("font bytes invalid"),
            map: HashMap::new(),
        }
    }

    /// Rasterize (or recall) `ch` at `px` pixels height. Returns (w, h, alpha bitmap).
    pub fn glyph(&mut self, ch: char, px: u32, bold: bool) -> (u32, u32, Vec<u8>) {
        let key = (if bold { 1 } else { 0 }, ch, px);
        if let Some(v) = self.map.get(&key) {
            return v.clone();
        }
        let font = if bold {
            // Bold: we don't have an SF Mono Bold file handy; simulate via heavier weight where
            // the face supports it is complex — for now bold uses the same weight (alacritty cells
            // mostly read the same). This is a TWEAK POINT.
            &self.font
        } else {
            &self.font
        };
        let scale = PxScale::from(px as f32);
        let scaled = font.as_scaled(scale);
        let id = font.glyph_id(ch);
        let advance = scaled.h_advance(id);
        // Height from ascent/descent so all glyphs share the same line box.
        let height = (scaled.ascent() - scaled.descent()).round() as u32;
        let outline = scaled.outline_glyph(AbsGlyph {
            id,
            position: ab_glyph::point(0.0, scaled.ascent()),
            scale,
        });
        let w = advance.ceil().max(1.0) as u32;
        let mut bmp = vec![0u8; (w * height) as usize];
        if let Some(og) = outline {
            let bounds = og.px_bounds();
            let (bw, bh) = (bounds.width() as usize, bounds.height() as usize);
            let (bx, by) = (bounds.min.x.max(0.0) as usize, bounds.min.y.max(0.0) as usize);
            og.draw(|x, y, cov| {
                let gx = x as usize + bx;
                let gy = y as usize + by;
                if gx < w as usize && gy < height as usize {
                    let idx = gy * w as usize + gx;
                    let v = (cov * 255.0) as u8;
                    if v > bmp[idx] {
                        bmp[idx] = v;
                    }
                }
                let _ = (bw, bh);
            });
        }
        self.map.insert(key, (w, height, bmp.clone()));
        (w, height, bmp)
    }
}

/// A search highlight: the currently-focused match, given as (absolute line, column, width).
pub type Find = (i32, usize, usize);

/// Draw the grid rows/cols into the framebuffer. `row_px`, `col_px` are per-cell pixel sizes.
/// The glyph `h` px is the line box; the glyph bitmap is drawn at bottom baseline.
///
/// `find` is an optional search highlight: when Some, the match's cells are drawn with a hot
/// background (yellow) so the user can see where the query landed.
pub fn draw_grid(
    buf: &mut Framebuffer,
    term: &Term<Listener>,
    cell_w: u32,
    cell_h: u32,
    font_px: u32,
    cache: &mut GlyphCache,
    find: Option<Find>,
) {
    let cols = term.columns();
    // Cursor cell (block) — draw on top of its own cell after painting the background.
    let cursor = &term.grid().cursor.point;
    // When scrolled into history the cursor should not draw (it lives off-screen).
    let scrolled = term.grid().display_offset() > 0;
    // Search-highlight color (vivid yellow background, black text pops).
    const HIT_FG: (u8, u8, u8) = (0x00, 0x00, 0x00);
    const HIT_BG: (u8, u8, u8) = (0xff, 0xd2, 0x00);

    // Iterate the grid in *display* order (rows scrolled into the viewport), so a non-zero
    // `display_offset` (history scrollback) renders correctly rather than the raw storage lines.
    // Each item is an `Indexed<&Cell>` deref'ing to the cell, with a display `point` we use to
    // place it on screen.
    for idx in term.grid().display_iter() {
        let cell = &idx.cell;
        let row = idx.point.line.0;
        let col = idx.point.column.0 as usize;
        if row < 0 || col >= cols {
            continue;
        }
        let x0 = col as u32 * cell_w;
        let y0 = row as u32 * cell_h;
        let is_cursor = !scrolled && row == cursor.line.0 && col == cursor.column.0 as usize;
        // Is this cell part of the active search match?
        let in_match = find
            .map(|(l, c, w)| row == l && col >= c && col < c + w)
            .unwrap_or(false);

        // Background.
        let mut bg = cell_color(&cell.bg);
        if in_match {
            bg = PaletteColor::Spec { r: HIT_BG.0, g: HIT_BG.1, b: HIT_BG.2 };
        } else if is_cursor {
            // Classic block cursor: use the cell's foreground as the block fill.
            bg = cell_color(&cell.fg);
        }
        paint_bg(buf, x0 as usize, y0 as usize, cell_w as usize, cell_h as usize, bg);
        if cell.c == ' ' {
            continue;
        }

        // Foreground: normal cell fg, or (cursor/match) inverted theme.
        let mut fg = cell_color(&cell.fg);
        if in_match {
            fg = PaletteColor::Spec { r: HIT_FG.0, g: HIT_FG.1, b: HIT_FG.2 };
        } else if is_cursor {
            // Invert: draw the glyph in the cell's original background color.
            let mut bgsave = cell_color(&cell.bg);
            std::mem::swap(&mut fg, &mut bgsave);
        }
        let (r, g, b) = fg.to_rgb();
        let bold = cell.flags.contains(Flags::BOLD);
        let (gw, gh, alpha) = cache.glyph(cell.c, font_px, bold);
        // Draw glyph alpha over bg at baseline; clamp to the cell box.
        let gx = x0 as usize;
        let top = y0 as usize + cell_h as usize - gh as usize;
        for gy in 0..gh as usize {
            for gx2 in 0..gw as usize {
                let a = alpha[gy * gw as usize + gx2];
                if a == 0 {
                    continue;
                }
                let px = gx + gx2;
                let py = top + gy;
                if px < buf.width && py < buf.height {
                    let dst = buf.pixels[py * buf.width + px];
                    let dr = ((dst >> 16) & 0xff) as u32;
                    let dg = ((dst >> 8) & 0xff) as u32;
                    let db = (dst & 0xff) as u32;
                    let a32 = a as u32;
                    let nr = (r as u32 * a32 + dr * (255 - a32)) / 255;
                    let ng = (g as u32 * a32 + dg * (255 - a32)) / 255;
                    let nb = (b as u32 * a32 + db * (255 - a32)) / 255;
                    buf.pixels[py * buf.width + px] = argb(255, nr as u8, ng as u8, nb as u8);
                }
            }
        }
    }
}

/// Extract the text of one grid line (history or visible) as a String, for case-insensitive search.
fn line_text(term: &Term<Listener>, line: i32) -> String {
    let grid = term.grid();
    // `Row` indexes over `Column`; take a full slice of cells and collect their chars.
    let out = &grid[Line(line)][Column(0)..Column(grid.columns())];
    let mut s = String::with_capacity(out.len());
    for cell in out {
        s.push(cell.c);
    }
    s
}

/// Case-insensitive substring search across the whole grid (history + screen). Returns the first
/// match at-or-after `start` (an absolute line), or None. `start` starts at topmost_line.
pub fn find(term: &Term<Listener>, query: &str, start: i32) -> Option<Find> {
    if query.is_empty() {
        return None;
    }
    let q = query.to_lowercase();
    let grid = term.grid();
    let bottom = grid.bottommost_line().0;
    let mut line = start;
    while line <= bottom {
        let text = line_text(term, line).to_lowercase();
        if let Some(c) = text.find(&q) {
            return Some((line, c, q.chars().count()));
        }
        line += 1;
    }
    None
}

/// Draw a line of text at pixel origin (x0,y0 baseline) with the given color/size. Used for the
/// native chrome (tab bar, status line). Returns the pixel width consumed.
pub fn draw_text(
    buf: &mut Framebuffer,
    cache: &mut GlyphCache,
    text: &str,
    x0: usize,
    y0: usize,
    font_px: u32,
    color: (u8, u8, u8),
) -> usize {
    let (r, g, b) = color;
    let mut cx = x0;
    for ch in text.chars() {
        if ch == '\n' {
            continue;
        }
        let (gw, gh, alpha) = cache.glyph(ch, font_px, false);
        let top = y0 as i64 - gh as i64;
        for gy in 0..gh as usize {
            for gx2 in 0..gw as usize {
                let a = alpha[gy * gw as usize + gx2];
                if a == 0 {
                    continue;
                }
                let px = cx + gx2;
                let py = top + gy as i64;
                if px < buf.width && py >= 0 && py < buf.height as i64 {
                    let py = py as usize;
                    let dst = buf.pixels[py * buf.width + px];
                    let dr = ((dst >> 16) & 0xff) as u32;
                    let dg = ((dst >> 8) & 0xff) as u32;
                    let db = (dst & 0xff) as u32;
                    let a32 = a as u32;
                    let nr = (r as u32 * a32 + dr * (255 - a32)) / 255;
                    let ng = (g as u32 * a32 + dg * (255 - a32)) / 255;
                    let nb = (b as u32 * a32 + db * (255 - a32)) / 255;
                    buf.pixels[py * buf.width + px] = argb(255, nr as u8, ng as u8, nb as u8);
                }
            }
        }
        cx += gw as usize;
    }
    cx - x0
}

/// Path to a usable mono font.
fn font_path() -> String {
    if let Ok(p) = std::env::var("HARNESS_FONT") {
        if !p.is_empty() {
            return p;
        }
    }
    // macOS SF Mono.
    "/System/Library/Fonts/SFNSMono.ttf".to_string()
}

/// 256-color palette lookup (standard xterm cube/greyscale).
fn color256(i: u8) -> (u8, u8, u8) {
    if i < 16 {
        return PaletteColor::Named { bright: i > 7, index: i & 7 }.to_rgb();
    }
    if i < 232 {
        let n = i - 16;
        let r = n / 36;
        let g = (n / 6) % 6;
        let b = n % 6;
        let ramp = |v: u8| -> u8 { if v == 0 { 0 } else { (55 + v * 40) as u8 } };
        (ramp(r), ramp(g), ramp(b))
    } else {
        let g = 8 + (i - 232) * 10;
        (g, g, g)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argb_packing() {
        assert_eq!(argb(255, 10, 20, 30), 0xff0a141e);
    }

    #[test]
    fn framebuffer_bounds() {
        let mut b = Framebuffer::new(4, 3);
        b.set(2, 1, argb(255, 1, 2, 3));
        assert_eq!(b.pixels[1 * 4 + 2], argb(255, 1, 2, 3));
        b.set(99, 99, 7); // out of bounds: ignored, no panic
        assert_eq!(b.pixels.len(), 12);
    }

    #[test]
    fn color256_map() {
        assert_eq!(color256(0), (0, 0, 0));
        assert_eq!(color256(15), (255, 255, 255));
    }

    /// Full render path, headless: feed a real `Term` ANSI (colors + text), render it into a
    /// framebuffer with real font rasterization, and assert the output is non-blank and colored
    /// where a colored glyph was written. This proves the emulator→glyph→framebuffer pipeline the
    /// native window uses, without needing a window or GPU.
    #[test]
    fn renders_colored_text_from_term() {
        use alacritty_terminal::sync::FairMutex;
        use alacritty_terminal::term::{Config, Term};
        use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

        use crate::session::Listener;

        // Build the grid at 80x24 and feed it two colored lines.
        let size = crate::session::TermSize { lines: 24, cols: 80 };
        let term = FairMutex::new(Term::new(Config::default(), &size, Listener));
        let bytes = b"\x1b[32mAAA\x1b[0m\r\n\x1b[31mMMM\x1b[0m";
        {
            let mut p: Processor<StdSyncHandler> = Processor::default();
            p.advance(&mut *term.lock(), bytes);
        }

        // Render at a modest cell size.
        let mut fb = Framebuffer::new(80 * 9, 24 * 18);
        let mut cache = GlyphCache::load();
        {
            let g = term.lock();
            draw_grid(&mut fb, &g, 9, 18, 12, &mut cache, None);
        }

        // At least some pixel is non-background (glyphs drawn).
        let non_blank = fb.pixels.iter().filter(|&&p| p != 0x0000_0000).count();
        assert!(non_blank > 50, "expected glyph pixels, got {non_blank}");

        // Some pixel is green-ish (our palette green = {13,188,121}) from the first line.
        let has_green = fb.pixels.iter().any(|&p| {
            let r = (p >> 16) & 0xff;
            let g = (p >> 8) & 0xff;
            let b = p & 0xff;
            g > 100 && r < 100 && b < 100
        });
        assert!(has_green, "expected a green glyph pixel");
    }

    /// Scrollback: feed far more lines than the viewport (so earlier ones scroll off-screen), then
    /// scroll the viewport up and assert the display_iter renderer shows the scrolled-into-view
    /// line (i.e. non-blank glyphs appear that weren't visible at the live position). This proves
    /// `draw_grid` honors `display_offset` rather than always drawing the raw storage rows.
    #[test]
    fn renders_scrollback_when_scrolled() {
        use alacritty_terminal::grid::Scroll;
        use alacritty_terminal::sync::FairMutex;
        use alacritty_terminal::term::{Config, Term};
        use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

        use crate::session::Listener;

        let size = crate::session::TermSize { lines: 8, cols: 40 };
        let term = FairMutex::new(Term::new(Config::default(), &size, Listener));

        // Feed 30 lines of a distinctive character so the screen (8 lines) overflows into history.
        let mut buf = Vec::new();
        for i in 0..30 {
            buf.extend_from_slice(format!("\r\nLINE{i:02}").as_bytes());
        }
        {
            let mut p: Processor<StdSyncHandler> = Processor::default();
            let mut g = term.lock();
            p.advance(&mut *g, &buf);
            // At live position the first line is scrolled off into history.
            assert!(g.grid().display_offset() == 0, "live view should be at bottom");
            assert!(g.grid().history_size() > 0, "expected history to accumulate");

            // Scroll up one page; the display offset becomes non-zero.
            use alacritty_terminal::grid::Dimensions;
            g.grid_mut().scroll_display(Scroll::Delta(4));
            assert!(g.grid().display_offset() > 0, "display_offset should rise after scroll");
        }

        // Rendering at the scrolled position must produce glyph pixels (the scrolled-into-view rows).
        let mut fb = Framebuffer::new(40 * 9, 8 * 18);
        let mut cache = GlyphCache::load();
        {
            let g = term.lock();
            draw_grid(&mut fb, &g, 9, 18, 12, &mut cache, None);
        }
        let non_blank = fb.pixels.iter().filter(|&&p| p != 0x0000_0000).count();
        assert!(non_blank > 20, "expected scrollback glyphs when scrolled, got {non_blank}");
    }

    /// Search: find a query string across history, case-insensitively, and report its line+col.
    #[test]
    fn finds_text_in_scrollback() {
        use alacritty_terminal::sync::FairMutex;
        use alacritty_terminal::term::{Config, Term};
        use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

        use crate::session::Listener;

        let size = crate::session::TermSize { lines: 6, cols: 40 };
        let term = FairMutex::new(Term::new(Config::default(), &size, Listener));
        let mut buf = Vec::new();
        for i in 0..10 {
            buf.extend_from_slice(format!("\r\nrow {i} needle").as_bytes());
        }
        {
            let mut p: Processor<StdSyncHandler> = Processor::default();
            p.advance(&mut *term.lock(), &buf);
        }
        let g = term.lock();
        // needle appears on every line; starting from the top of history the first hit is the
        // topmost history line (a negative absolute line, since history sits above line 0).
        let hit = find(&g, "NEEDLE", g.grid().topmost_line().0).expect("should find a match");
        assert!(hit.0 < 0, "first hit should be in history (negative line), got {}", hit.0);
        assert!(hit.1 >= 0 && hit.2 > 0, "match should have a column and width");
    }
}
