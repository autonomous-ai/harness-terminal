//! Pure framebuffer renderer for the standalone native window.
//!
//! Draws an alacritty `Term` grid (cells → (char, fg, bg)) into a `u32` ARGB pixel buffer with
//! API-stable type aliases instead of the volatile `Cell` enum. Glyph glyphs are rasterized by
//! `ab_glyph` into a cache. Everything here is a pure function over a buffer, so the whole render
//! path is unit-testable headlessly (no window, no GPU).

use std::{collections::HashMap, path::Path};

use crate::links::{self, UrlSpan};
use ab_glyph::{Font as _, FontArc, Glyph as AbsGlyph, PxScale, ScaleFont as _};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::Term;
use alacritty_terminal::vte::ansi::{self, CursorShape};

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

    /// Darken every pixel toward black by `f` (1.0 = unchanged, 0.0 = black). Used behind modal
    /// overlays so the terminal content recedes and the overlay's own text reads clearly instead of
    /// being swallowed by bright agent output beneath it.
    pub fn dim(&mut self, f: f64) {
        for p in self.pixels.iter_mut() {
            let a = (*p >> 24) & 0xff;
            let r = (*p >> 16) & 0xff;
            let g = (*p >> 8) & 0xff;
            let b = *p & 0xff;
            let nr = ((r as f64) * f) as u32 & 0xff;
            let ng = ((g as f64) * f) as u32 & 0xff;
            let nb = ((b as f64) * f) as u32 & 0xff;
            *p = (a << 24) | (nr << 16) | (ng << 8) | nb;
        }
    }
}

/// Term cell color. API-stable copy of `vte::ansi::Color` variants we use — the enum's fields are
/// public so we can match them, but referencing the variant names directly is fine (they're plain
/// data); the instability is in some *flags* we never touch. We only pull `r/g/b` and `index`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteColor {
    Spec {
        r: u8,
        g: u8,
        b: u8,
    },
    Indexed(u8),
    /// Named 16 colors: `(bright, index0..=15)`.
    Named {
        bright: bool,
        index: u8,
    },
}

impl Default for PaletteColor {
    fn default() -> Self {
        PaletteColor::Spec { r: 0, g: 0, b: 0 }
    }
}

/// Default theme foreground (light) and background (black), matching alacritty's default so the
/// native surface stays consistent with the local-PTY default Config.
const DEFAULT_FG: (u8, u8, u8) = (0xc0, 0xca, 0xf5);
const DEFAULT_BG: (u8, u8, u8) = (0x1a, 0x1b, 0x26);

/// Classic 16-color ANSI palette. Matches the terminal's default Config::default() colors so we
/// stay consistent with the local PTY tabs. A config theme can override individual entries.
const ANSI: [(u8, u8, u8); 16] = [
    (0x15, 0x16, 0x1e), // black
    (0xf7, 0x76, 0x8e), // red
    (0x9e, 0xce, 0x6a), // green
    (0xe0, 0xaf, 0x68), // yellow
    (0x7a, 0xa2, 0xf7), // blue
    (0xbb, 0x9a, 0xf7), // magenta
    (0x7d, 0xcf, 0xff), // cyan
    (0xa9, 0xb1, 0xd6), // white
    (0x41, 0x48, 0x68), // bright black
    (0xff, 0x7a, 0x93), // bright red
    (0xb9, 0xf2, 0x7c), // bright green
    (0xff, 0x9e, 0x64), // bright yellow
    (0x7d, 0xa6, 0xff), // bright blue
    (0xbb, 0x9a, 0xf7), // bright magenta
    (0x2a, 0xc3, 0xde), // bright cyan
    (0xc0, 0xca, 0xf5), // bright white
];

/// The resolved runtime palette the renderer actually paints with, built from an optional
/// `config::Theme` at startup. `Default` reproduces the exact built-in colors, so a config with no
/// `[theme]` block renders identically to before theming existed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Colors {
    /// Default foreground (normal text).
    pub fg: (u8, u8, u8),
    /// Default background.
    pub bg: (u8, u8, u8),
    /// Underline/beam cursor color.
    pub cursor: (u8, u8, u8),
    /// Text-selection highlight background.
    pub selection: (u8, u8, u8),
    /// Copy-mode read cursor block color.
    pub copy_cursor: (u8, u8, u8),
    /// The 16-color ANSI palette.
    pub ansi: [(u8, u8, u8); 16],
    /// Per-engine accent tints (inactive-tab label color), overridden by `[theme.accents]`. Empty
    /// means every engine keeps its built-in brand accent.
    pub accents: std::collections::BTreeMap<String, (u8, u8, u8)>,
}

impl Default for Colors {
    fn default() -> Self {
        Colors {
            fg: DEFAULT_FG,
            bg: DEFAULT_BG,
            cursor: DEFAULT_FG,
            selection: (0x33, 0x46, 0x7c),
            copy_cursor: (0x9e, 0xce, 0x6a),
            ansi: ANSI,
            accents: std::collections::BTreeMap::new(),
        }
    }
}

/// Build a palette from its core fields (fg, bg, selection, cursor, copy cursor, 16-color ANSI).
fn pal(
    fg: (u8, u8, u8),
    bg: (u8, u8, u8),
    selection: (u8, u8, u8),
    cursor: (u8, u8, u8),
    copy_cursor: (u8, u8, u8),
    ansi: [(u8, u8, u8); 16],
) -> Colors {
    Colors {
        fg,
        bg,
        cursor,
        selection,
        copy_cursor,
        ansi,
        accents: std::collections::BTreeMap::new(),
    }
}

impl Colors {
    /// A named preset palette. `tokyo-night` is the built-in default; the others are popular
    /// high-contrast dark themes (gruvbox, solarized, nord, dracula, github-dark). Unknown or blank
    /// names fall back to the default palette. Individual `[theme]` fields layer on top afterwards.
    pub fn preset(name: &str) -> Colors {
        match name.trim().to_ascii_lowercase().as_str() {
            "gruvbox-dark" => pal(
                (0xeb, 0xdb, 0xb2),
                (0x28, 0x28, 0x28),
                (0x50, 0x49, 0x45),
                (0xeb, 0xdb, 0xb2),
                (0xb8, 0xbb, 0x26),
                [
                    (0x28, 0x28, 0x28),
                    (0xcc, 0x24, 0x1d),
                    (0x98, 0x97, 0x1a),
                    (0xd7, 0x99, 0x21),
                    (0x45, 0x85, 0x88),
                    (0xb1, 0x62, 0x86),
                    (0x68, 0x9d, 0x6a),
                    (0xa8, 0x99, 0x84),
                    (0x92, 0x83, 0x74),
                    (0xfb, 0x49, 0x34),
                    (0xb8, 0xbb, 0x26),
                    (0xfa, 0xbd, 0x2f),
                    (0x83, 0xa5, 0x98),
                    (0xd3, 0x86, 0x9b),
                    (0x8e, 0xc0, 0x7c),
                    (0xeb, 0xdb, 0xb2),
                ],
            ),
            "solarized-dark" => pal(
                (0x83, 0x94, 0x96),
                (0x00, 0x2b, 0x36),
                (0x07, 0x36, 0x42),
                (0x83, 0x94, 0x96),
                (0x2a, 0xa1, 0x98),
                [
                    (0x07, 0x36, 0x42),
                    (0xdc, 0x32, 0x2f),
                    (0x85, 0x99, 0x00),
                    (0xb5, 0x89, 0x00),
                    (0x26, 0x8b, 0xd2),
                    (0xd3, 0x36, 0x82),
                    (0x2a, 0xa1, 0x98),
                    (0xee, 0xe8, 0xd5),
                    (0x00, 0x2b, 0x36),
                    (0xcb, 0x4b, 0x16),
                    (0x58, 0x6e, 0x75),
                    (0x65, 0x7b, 0x83),
                    (0x83, 0x94, 0x96),
                    (0x6c, 0x71, 0xc4),
                    (0x93, 0xa1, 0xa1),
                    (0xfd, 0xf6, 0xe3),
                ],
            ),
            "nord" => pal(
                (0xd8, 0xde, 0xe9),
                (0x2e, 0x34, 0x40),
                (0x43, 0x48, 0x98),
                (0xd8, 0xde, 0xe9),
                (0x88, 0xc0, 0xd0),
                [
                    (0x3b, 0x42, 0x52),
                    (0xbf, 0x61, 0x6a),
                    (0xa3, 0xbe, 0x8c),
                    (0xeb, 0xcb, 0x8b),
                    (0x81, 0xa1, 0xc1),
                    (0xb4, 0x8e, 0xad),
                    (0x88, 0xc0, 0xd0),
                    (0xe5, 0xe9, 0xf0),
                    (0x4c, 0x56, 0x6a),
                    (0xbf, 0x61, 0x6a),
                    (0xa3, 0xbe, 0x8c),
                    (0xeb, 0xcb, 0x8b),
                    (0x81, 0xa1, 0xc1),
                    (0xb4, 0x8e, 0xad),
                    (0x8f, 0xbc, 0xbb),
                    (0xec, 0xef, 0xf4),
                ],
            ),
            "dracula" => pal(
                (0xf8, 0xf8, 0xf2),
                (0x28, 0x2a, 0x36),
                (0x44, 0x47, 0x5a),
                (0xf8, 0xf8, 0xf2),
                (0x50, 0xfa, 0x7b),
                [
                    (0x21, 0x22, 0x2c),
                    (0xff, 0x55, 0x55),
                    (0x50, 0xfa, 0x7b),
                    (0xf1, 0xfa, 0x8c),
                    (0xbd, 0x93, 0xf9),
                    (0xff, 0x79, 0xc6),
                    (0x8b, 0xe9, 0xfd),
                    (0xf8, 0xf8, 0xf2),
                    (0x62, 0x72, 0xa4),
                    (0xff, 0x6e, 0x6e),
                    (0x69, 0xff, 0x94),
                    (0xff, 0xff, 0xa5),
                    (0xd6, 0xac, 0xff),
                    (0xff, 0x92, 0xdf),
                    (0xa4, 0xff, 0xff),
                    (0xff, 0xff, 0xff),
                ],
            ),
            "github-dark" => pal(
                (0xc9, 0xd1, 0xd9),
                (0x0d, 0x11, 0x17),
                (0x1f, 0x6f, 0xeb),
                (0xc9, 0xd1, 0xd9),
                (0x3f, 0xb9, 0x50),
                [
                    (0x48, 0x4f, 0x58),
                    (0xff, 0x7b, 0x72),
                    (0x3f, 0xb9, 0x50),
                    (0xd2, 0x99, 0x22),
                    (0x58, 0xa6, 0xff),
                    (0xbc, 0x8c, 0xff),
                    (0x39, 0xc5, 0xcf),
                    (0xb1, 0xba, 0xc4),
                    (0x6e, 0x76, 0x81),
                    (0xff, 0xa1, 0x98),
                    (0x56, 0xd3, 0x64),
                    (0xe3, 0xb3, 0x41),
                    (0x79, 0xc0, 0xff),
                    (0xd2, 0xa8, 0xff),
                    (0x56, 0xd4, 0xdd),
                    (0xff, 0xff, 0xff),
                ],
            ),
            // "tokyo-night" and anything unknown/blank → the built-in default.
            _ => Colors::default(),
        }
    }
}

impl From<&crate::config::Theme> for Colors {
    fn from(t: &crate::config::Theme) -> Self {
        // Base = the named preset (or the built-in default when absent/unknown).
        let c = match t.preset.as_deref() {
            Some(p) if !p.trim().is_empty() => Colors::preset(p),
            _ => Colors::default(),
        };
        let set = |v: Option<[u8; 3]>, def: (u8, u8, u8)| match v {
            Some([r, g, b]) => (r, g, b),
            None => def,
        };
        let mut ansi = c.ansi;
        if let Some(overrides) = &t.ansi {
            for (i, v) in overrides.iter().enumerate() {
                if let Some([r, g, b]) = v {
                    ansi[i] = (*r, *g, *b);
                }
            }
        }
        Colors {
            fg: set(t.foreground, c.fg),
            bg: set(t.background, c.bg),
            cursor: set(t.cursor, c.cursor),
            selection: set(t.selection, c.selection),
            copy_cursor: set(t.copy_cursor, c.copy_cursor),
            ansi,
            accents: t
                .accents
                .iter()
                .map(|(k, v)| (k.clone(), (v[0], v[1], v[2])))
                .collect(),
        }
    }
}

impl PaletteColor {
    /// Convert to concrete RGB using the resolved theme palette.
    pub fn to_rgb(&self, colors: &Colors) -> (u8, u8, u8) {
        match *self {
            PaletteColor::Spec { r, g, b } => (r, g, b),
            PaletteColor::Indexed(i) => color256(i, colors),
            PaletteColor::Named { bright, index } => {
                let i = if bright {
                    (index as usize) + 8
                } else {
                    index as usize
                };
                colors.ansi[i.min(15)]
            }
        }
    }
}

/// Map the alacritty cell color enum into our stable `PaletteColor`.
///
/// `NamedColor` carries both the 16 ANSI colors (discriminants 0–15) and specials (Foreground=256,
/// Background, Dim*, …). We keep only the ANSI 0–15; the specials are folded to the theme defaults
/// (fg/bg), so `colors` resolves those.
pub fn cell_color(c: &ansi::Color, colors: &Colors) -> PaletteColor {
    match c {
        ansi::Color::Named(n) => {
            use ansi::NamedColor as N;
            match n {
                N::Black => PaletteColor::Named {
                    bright: false,
                    index: 0,
                },
                N::Red => PaletteColor::Named {
                    bright: false,
                    index: 1,
                },
                N::Green => PaletteColor::Named {
                    bright: false,
                    index: 2,
                },
                N::Yellow => PaletteColor::Named {
                    bright: false,
                    index: 3,
                },
                N::Blue => PaletteColor::Named {
                    bright: false,
                    index: 4,
                },
                N::Magenta => PaletteColor::Named {
                    bright: false,
                    index: 5,
                },
                N::Cyan => PaletteColor::Named {
                    bright: false,
                    index: 6,
                },
                N::White => PaletteColor::Named {
                    bright: false,
                    index: 7,
                },
                N::BrightBlack => PaletteColor::Named {
                    bright: true,
                    index: 0,
                },
                N::BrightRed => PaletteColor::Named {
                    bright: true,
                    index: 1,
                },
                N::BrightGreen => PaletteColor::Named {
                    bright: true,
                    index: 2,
                },
                N::BrightYellow => PaletteColor::Named {
                    bright: true,
                    index: 3,
                },
                N::BrightBlue => PaletteColor::Named {
                    bright: true,
                    index: 4,
                },
                N::BrightMagenta => PaletteColor::Named {
                    bright: true,
                    index: 5,
                },
                N::BrightCyan => PaletteColor::Named {
                    bright: true,
                    index: 6,
                },
                N::BrightWhite => PaletteColor::Named {
                    bright: true,
                    index: 7,
                },
                // Dim colors: fold to the theme foreground/background (dim nuance is lost in the
                // framebuffer for now — a future glyph/alpha pass can honor it).
                N::DimBlack
                | N::DimRed
                | N::DimGreen
                | N::DimYellow
                | N::DimBlue
                | N::DimMagenta
                | N::DimCyan
                | N::DimWhite => PaletteColor::Spec {
                    r: colors.fg.0,
                    g: colors.fg.1,
                    b: colors.fg.2,
                },
                N::Background => PaletteColor::Spec {
                    r: colors.bg.0,
                    g: colors.bg.1,
                    b: colors.bg.2,
                },
                // Foreground and anything unknown → default foreground.
                _ => PaletteColor::Spec {
                    r: colors.fg.0,
                    g: colors.fg.1,
                    b: colors.fg.2,
                },
            }
        }
        ansi::Color::Indexed(i) => PaletteColor::Indexed(*i),
        ansi::Color::Spec(rgb) => PaletteColor::Spec {
            r: rgb.r,
            g: rgb.g,
            b: rgb.b,
        },
    }
}

/// Render one cell's worth of background into the buffer (fill `w`×`h` px at origin (x0,y0)).
fn paint_bg(
    buf: &mut Framebuffer,
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
    c: PaletteColor,
    colors: &Colors,
) {
    let (r, g, b) = c.to_rgb(colors);
    let p = argb(255, r, g, b);
    for dy in 0..h {
        for dx in 0..w {
            buf.set(x0 + dx, y0 + dy, p);
        }
    }
}

/// Fill a solid rectangle of `w`×`h` pixels at origin (x0,y0) with an opaque RGB color. Used by the
/// native chrome (tab-bar / status-line panels, active-tab pill) so panels read as designed surfaces
/// rather than text floating on the grid background.
pub fn fill_rect(
    buf: &mut Framebuffer,
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
    color: (u8, u8, u8),
) {
    let p = argb(255, color.0, color.1, color.2);
    for dy in 0..h {
        for dx in 0..w {
            buf.set(x0 + dx, y0 + dy, p);
        }
    }
}

/// Fill a rectangle whose top corners are rounded by `r` px while the bottom edge stays square —
/// the silhouette of a raised native tab (Safari/Chrome-style) that visually connects to the content
/// beneath it. Only writes pixels that are inside the shape, so the previously-painted chrome panel
/// shows through the cut corners instead of leaving a transparent hole.
pub fn filled_round_top(
    buf: &mut Framebuffer,
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
    r: usize,
    color: (u8, u8, u8),
) {
    if w == 0 || h == 0 {
        return;
    }
    let r = r.min(h).min(w / 2);
    let p = argb(255, color.0, color.1, color.2);
    let rr = (r * r) as i64;
    // Corner centers relative to the shape origin (left and right top corners).
    let (ccxl, ccy) = (r as i64, r as i64);
    let ccxr = (w as i64 - 1) - r as i64;
    for dy in 0..h {
        let y = y0 + dy;
        let col = y as i64 - y0 as i64;
        for dx in 0..w {
            let row = dx as i64;
            let inside = if col < r as i64 && (row < r as i64 || row > ccxr) {
                // Top corner band: keep a pixel only if it sits inside its rounded corner.
                let (cx, cy) = if row < r as i64 {
                    (ccxl, ccy)
                } else {
                    (ccxr, ccy)
                };
                let dxx = row - cx;
                let dyy = col - cy;
                dxx * dxx + dyy * dyy <= rr
            } else {
                true
            };
            if inside {
                buf.set(x0 + dx, y, p);
            }
        }
    }
}

/// Glyph cache: one 8-bit alpha bitmap per (style, char, pixel-size). Rasterizes with `ab_glyph`
/// into an 8-bit coverage bitmap (grayscale anti-aliasing), with a matching italic face when one
/// ships alongside the primary font, and a synthetic 1px bold smear for `BOLD` cells.
pub struct GlyphCache {
    font: FontArc,
    /// Optional matching italic face (SF Mono ships `SFNSMonoItalic.ttf`). SGR italic falls back to
    /// the upright face when no italic file is available.
    italic: Option<FontArc>,
    /// cache key -> (w, h, bitmap u8 alpha).
    map: HashMap<(u32, u32, u32, u32), (u32, u32, Vec<u8>)>,
}

impl GlyphCache {
    /// Load a usable monospace font (config override / `HARNESS_FONT` / macOS SF Mono) plus a
    /// best-effort matching italic face. Degrades gracefully: a missing or corrupt configured font
    /// falls back to a known system mono face instead of crashing the app at launch.
    pub fn load() -> Self {
        let (data, path) = usable_mono_font();
        let font = FontArc::try_from_vec(data).expect("validated monospace font");
        let italic = load_italic_for(&path).and_then(|d| FontArc::try_from_vec(d).ok());
        GlyphCache {
            font,
            italic,
            map: HashMap::new(),
        }
    }

    /// Rasterize (or recall) `ch` at `px` pixels height, upright (used by the chrome text).
    pub fn glyph(&mut self, ch: char, px: u32, bold: bool) -> (u32, u32, Vec<u8>) {
        self.glyph_styled(ch, px, bold, false)
    }

    /// Rasterize (or recall) `ch` at `px` pixels height with the terminal's SGR style flags
    /// (`bold` / `italic`). Returns (w, h, 8-bit alpha bitmap).
    pub fn glyph_styled(
        &mut self,
        ch: char,
        px: u32,
        bold: bool,
        italic: bool,
    ) -> (u32, u32, Vec<u8>) {
        let key = (
            if bold { 1 } else { 0 },
            if italic { 1 } else { 0 },
            ch as u32,
            px,
        );
        if let Some(v) = self.map.get(&key) {
            return v.clone();
        }
        // Upright vs italic face. When the terminal asked for italic but no italic face loaded,
        // silently fall back to upright rather than inventing an oblique.
        let face = if italic {
            self.italic.as_ref().unwrap_or(&self.font)
        } else {
            &self.font
        };
        let scale = PxScale::from(px as f32);
        let scaled = face.as_scaled(scale);
        let id = face.glyph_id(ch);
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
            let (bx, by) = (
                bounds.min.x.max(0.0) as usize,
                bounds.min.y.max(0.0) as usize,
            );
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
        // Synthetic bold: no system bold mono face is guaranteed, so thicken the stroke by piping
        // each column's coverage one pixel to the right (rooted in the original coverage so strokes
        // stay crisp rather than smearing into a solid block).
        if bold && w > 1 && height > 0 {
            let (bw, bh) = (w as usize, height as usize);
            let src = bmp.clone();
            for y in 0..bh {
                for x in (1..bw).rev() {
                    let idx = y * bw + x;
                    bmp[idx] = src[idx].max(src[idx - 1]);
                }
            }
        }
        self.map.insert(key, (w, height, bmp.clone()));
        (w, height, bmp)
    }
}

/// Best-effort path to an italic sibling face for a resolved mono font path. The known macOS default
/// (`SFNSMono.ttf`) has a fixed `SFNSMonoItalic.ttf` neighbor; other fonts try `<stem>-Italic<ext>`
/// and `<stem>Italic<ext>`. Returns None when no candidate exists so italic simply falls back.
fn italic_candidate(font: &str) -> Option<String> {
    if font == "/System/Library/Fonts/SFNSMono.ttf" {
        return Some("/System/Library/Fonts/SFNSMonoItalic.ttf".into());
    }
    let p = Path::new(font);
    let dir = p.parent()?.to_string_lossy().into_owned();
    let stem = p.file_stem()?.to_string_lossy().into_owned();
    let ext = p
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    for cand in [
        format!("{dir}/{stem}-Italic{ext}"),
        format!("{dir}/{stem}Italic{ext}"),
    ] {
        if Path::new(&cand).exists() {
            return Some(cand);
        }
    }
    None
}

/// Read the italic sibling bytes for the *actually loaded* primary font path, or None when no
/// italic face is present/readable. Takes the resolved primary path (which may differ from
/// `font_path()` when the config override was invalid and the fallback chain picked another face).
fn load_italic_for(font: &str) -> Option<Vec<u8>> {
    let cand = italic_candidate(font)?;
    std::fs::read(cand).ok()
}

/// Pick a usable monospace font, degrading gracefully instead of panic: the configured `font_path`
/// or `HARNESS_FONT` is validated first; when it's missing/corrupt (or SF Mono is absent on this
/// machine) the known macOS mono faces are tried in order. Returns (bytes, resolved path). Only
/// when no usable monospace exists at all — nothing could be rendered anyway — does this fail
/// loudly at init rather than mid-frame.
fn usable_mono_font() -> (Vec<u8>, String) {
    let primary = font_path();
    if let Some(data) = read_valid_font(&primary) {
        return (data, primary);
    }
    for cand in [
        "/System/Library/Fonts/SFNSMono.ttf",
        "/System/Library/Fonts/Monaco.ttf",
    ] {
        if cand == primary {
            continue;
        }
        if let Some(data) = read_valid_font(cand) {
            return (data, cand.to_string());
        }
    }
    let data = std::fs::read(&primary).expect("no usable monospace font found");
    FontArc::try_from_vec(data).expect("font bytes invalid");
    unreachable!()
}

/// Read `path` and return its bytes only when they parse as a usable font.
fn read_valid_font(path: &str) -> Option<Vec<u8>> {
    let data = std::fs::read(path).ok()?;
    FontArc::try_from_vec(data.clone()).ok()?;
    Some(data)
}

/// A search highlight: a match given as (absolute line, column, width).
pub type Find = (i32, usize, usize);

/// Range of -chars width (in cells) for a query, so every occurrence can be highlighted.
fn match_width(query: &str) -> usize {
    query.chars().count()
}

/// A cheap rolling signature of a session's VISIBLE grid — the same cells `draw_grid` paints — so
/// the idle-wake detector (`about_to_wait` → `detect_content_change`) can notice output that redraws
/// the screen in place without growing scrollback (a vim cursor move, an htop refresh, a spinner
/// line, a TUI redrawing its pane). `history_len` alone misses those, which would leave the terminal
/// showing stale content. Hashes position + char + SGR flags + fg/bg for every visible cell plus the
/// cursor point (the block/beam is drawn even on an otherwise-empty screen). Returns a stable u64 —
/// practically zero collisions across consecutive frames, and cheap at the 8fps idle tick.
pub(crate) fn visible_signature(term: &Term<Listener>) -> u64 {
    // FNV-1a.
    const P: u64 = 0x0000_0100_0000_01b3;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    fn mix(h: &mut u64, v: u64) {
        *h ^= v;
        *h = h.wrapping_mul(P);
    }
    for idx in term.grid().display_iter() {
        let p = idx.point;
        mix(&mut h, p.line.0 as u64);
        mix(&mut h, p.column.0 as u64);
        mix(&mut h, idx.cell.c as u64);
        mix(&mut h, idx.cell.flags.bits() as u64);
        mix(&mut h, color_key(&idx.cell.fg));
        mix(&mut h, color_key(&idx.cell.bg));
    }
    let c = term.grid().cursor.point;
    mix(&mut h, c.line.0 as u64);
    mix(&mut h, c.column.0 as u64);
    h
}

/// Stable fold of a cell color into a signature contribution. Named colors get a fixed index (not the
/// enum's memory layout, which is unspecified), indexed colors keep their palette index, and spec
/// colors keep their RGB, so any color change flips the signature.
fn color_key(c: &ansi::Color) -> u64 {
    use ansi::Color as C;
    match c {
        C::Named(n) => 0x1000_0000 | named_key(*n),
        C::Indexed(i) => 0x2000_0000 | u64::from(*i),
        C::Spec(rgb) => {
            0x3000_0000 | (u64::from(rgb.r) << 16) | (u64::from(rgb.g) << 8) | u64::from(rgb.b)
        }
    }
}

/// Stable 0-based index for every `NamedColor` variant (the struct itself has no repr).
fn named_key(n: ansi::NamedColor) -> u64 {
    use ansi::NamedColor as N;
    match n {
        N::Black => 0,
        N::Red => 1,
        N::Green => 2,
        N::Yellow => 3,
        N::Blue => 4,
        N::Magenta => 5,
        N::Cyan => 6,
        N::White => 7,
        N::BrightBlack => 8,
        N::BrightRed => 9,
        N::BrightGreen => 10,
        N::BrightYellow => 11,
        N::BrightBlue => 12,
        N::BrightMagenta => 13,
        N::BrightCyan => 14,
        N::BrightWhite => 15,
        N::Foreground => 16,
        N::Background => 17,
        N::Cursor => 18,
        N::DimBlack => 19,
        N::DimRed => 20,
        N::DimGreen => 21,
        N::DimYellow => 22,
        N::DimBlue => 23,
        N::DimMagenta => 24,
        N::DimCyan => 25,
        N::DimWhite => 26,
        N::BrightForeground => 27,
        N::DimForeground => 28,
    }
}

/// Draw the grid rows/cols into the framebuffer. `row_px`, `col_px` are per-cell pixel sizes.
/// The glyph `h` px is the line box; the glyph bitmap is drawn at bottom baseline.
///
/// `find` is the focused search match and `matches` is every occurrence of the active query. When
/// either is present, matching cells are drawn with a yellow background so the user can see where
/// the query landed; the focused match uses a brighter orange so it stands apart from the rest.
pub fn draw_grid(
    buf: &mut Framebuffer,
    term: &Term<Listener>,
    cell_w: u32,
    cell_h: u32,
    font_px: u32,
    cache: &mut GlyphCache,
    colors: &Colors,
    find: Option<Find>,
    matches: &[Find],
    sel: Option<&alacritty_terminal::selection::SelectionRange>,
    copy: Option<(i32, usize)>,
) {
    let cols = term.columns();
    // Cursor cell (block) — draw on top of its own cell after painting the background.
    let cursor = &term.grid().cursor.point;
    // When scrolled into history the cursor should not draw (it lives off-screen).
    let scrolled = term.grid().display_offset() > 0;
    // Search-highlight colors: vivid yellow for ordinary matches, bright orange for the focused one
    // (black text pops on both). Kept as fixed built-ins (not part of the theme).
    const HIT_FG: (u8, u8, u8) = (0x00, 0x00, 0x00);
    const HIT_BG: (u8, u8, u8) = (0xff, 0xd2, 0x00);
    const FOCUS_BG: (u8, u8, u8) = (0xff, 0x99, 0x00);
    // Detected-link tint: a light blue (distinct from the default fg) so clickable URLs read as
    // links. The underline beneath them is drawn in whatever foreground the cell already resolves to.
    const LINK_FG: (u8, u8, u8) = (0x5c, 0xb8, 0xff);

    // Pre-pass: detect the URL span in each *visible* row once, before drawing any cell, so link
    // detection is O(rows) per frame rather than O(cells) and runs only on rows being drawn.
    // Rows are visited in screen order (row 0..screen_lines), each row's text collected once. A URL
    // that wraps across grid rows is rendered as one contiguous span per row — the common case of a
    // single-line URL shows all its cells, and a wrapped one is per-row segments.
    let mut url_for_row: HashMap<i32, Vec<UrlSpan>> = HashMap::new();
    let wanted_rows: Vec<(i32, String)> = term
        .grid()
        .display_iter()
        .filter_map(|idx| {
            let row = idx.point.line.0;
            (row >= 0 && (idx.point.column.0 as usize) < cols).then(|| (row, idx.cell.c))
        })
        .fold(Vec::new(), |mut acc, (row, c)| {
            if let Some((last, last_c)) = acc.last_mut() {
                if *last == row {
                    last_c.push(c);
                    return acc;
                }
            }
            acc.push((row, c.to_string()));
            acc
        });
    for (row, text) in &wanted_rows {
        // Detect every URL span in the row (start-of-line and scheme URLs), deduplicated.
        let mut spans = Vec::new();
        let mut i = 0usize;
        while i < text.len() {
            if let Some(sp) = links::url_span(text, i) {
                if sp.end > i && !spans.iter().any(|p: &UrlSpan| p.start == sp.start) {
                    spans.push(sp);
                }
                i = sp.end.max(i + 1);
            } else {
                i += 1;
            }
        }
        url_for_row.insert(*row, spans);
    }

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
        // Is this cell part of the focused search match?
        let in_focus = find
            .map(|(l, c, w)| row == l && col >= c && col < c + w)
            .unwrap_or(false);
        // Is this cell part of any (other) search match? Every occurrence gets highlighted while
        // the overlay is open so the user sees all landing spots at once.
        let in_match = !in_focus
            && matches
                .iter()
                .any(|&(l, c, w)| row == l && col >= c && col < c + w);
        let in_sel = sel.map(|s| s.contains(idx.point)).unwrap_or(false);
        // Is this the copy-mode read cursor? (line, col) grid coords, drawn as a green block below.
        let is_copy_cursor = copy.is_some_and(|(l, c)| row == l && col == c);
        // Is this cell part of a detected URL? (link spans are byte offsets == column for ASCII URLs.)
        let in_link = url_for_row
            .get(&row)
            .is_some_and(|spans| spans.iter().any(|s| col >= s.start && col < s.end));

        // Resolve effective fg/bg, applying SGR inverse first (so cursor/match still take visual
        // precedence while keeping the right base colors to swap).
        let mut fg = if cell.flags.contains(Flags::INVERSE) {
            cell_color(&cell.bg, colors)
        } else {
            cell_color(&cell.fg, colors)
        };
        let mut bgc = if cell.flags.contains(Flags::INVERSE) {
            cell_color(&cell.fg, colors)
        } else {
            cell_color(&cell.bg, colors)
        };
        // Text selection: themed highlight background, keep the foreground for the glyph text.
        if in_sel {
            bgc = PaletteColor::Spec {
                r: colors.selection.0,
                g: colors.selection.1,
                b: colors.selection.2,
            };
        }
        // Search matches: force the yellow highlight regardless of inverse/selection. The focused
        // match is drawn orange so it reads as "you are here" among the others.
        if in_focus {
            fg = PaletteColor::Spec {
                r: HIT_FG.0,
                g: HIT_FG.1,
                b: HIT_FG.2,
            };
            bgc = PaletteColor::Spec {
                r: FOCUS_BG.0,
                g: FOCUS_BG.1,
                b: FOCUS_BG.2,
            };
        } else if in_match {
            fg = PaletteColor::Spec {
                r: HIT_FG.0,
                g: HIT_FG.1,
                b: HIT_FG.2,
            };
            bgc = PaletteColor::Spec {
                r: HIT_BG.0,
                g: HIT_BG.1,
                b: HIT_BG.2,
            };
        }
        let cursor_shape = if is_cursor {
            term.cursor_style().shape
        } else {
            CursorShape::Hidden
        };
        // Block cursor (and its hollow/hidden variants) fills the whole cell; underline/beam draw
        // a thinner bar and leave the cell background intact, colored by the cursor color.
        let is_bar = matches!(cursor_shape, CursorShape::Underline | CursorShape::Beam);
        if is_cursor && matches!(cursor_shape, CursorShape::Block | CursorShape::HollowBlock) {
            // Block cursor: fill with the effective foreground, draw the glyph in the themed bg.
            bgc = fg;
            fg = PaletteColor::Spec {
                r: colors.bg.0,
                g: colors.bg.1,
                b: colors.bg.2,
            };
        }
        // Copy-mode cursor: fill the cell with the themed read cursor color and draw the glyph in
        // black so it's always legible, regardless of scroll/selection state.
        if is_copy_cursor {
            bgc = PaletteColor::Spec {
                r: colors.copy_cursor.0,
                g: colors.copy_cursor.1,
                b: colors.copy_cursor.2,
            };
            fg = PaletteColor::Spec { r: 0, g: 0, b: 0 };
        }
        // Paint the background (uses the resolved bgc).
        paint_bg(
            buf,
            x0 as usize,
            y0 as usize,
            cell_w as usize,
            cell_h as usize,
            bgc,
            colors,
        );
        if cell.c == ' ' {
            continue;
        }
        // SGR dim: scale the foreground toward black (kept simple: halve toward black).
        let (mut r, mut g, mut b) = fg.to_rgb(colors);
        // Detected link: tint the glyph toward link-blue so it reads as clickable.
        if in_link {
            r = (r as u16 * 2 / 3 + LINK_FG.0 as u16 / 3) as u8;
            g = (g as u16 * 2 / 3 + LINK_FG.1 as u16 / 3) as u8;
            b = (b as u16 * 2 / 3 + LINK_FG.2 as u16 / 3) as u8;
        }
        if cell.flags.contains(Flags::DIM) {
            r = (r as u16 / 2) as u8;
            g = (g as u16 / 2) as u8;
            b = (b as u16 / 2) as u8;
        }
        let bold = cell.flags.contains(Flags::BOLD);
        let italic = cell.flags.contains(Flags::ITALIC);
        let (gw, gh, alpha) = cache.glyph_styled(cell.c, font_px, bold, italic);
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

        // SGR underline / strikeout: draw a horizontal rule across the cell in the fg color.
        // Underline sits near the baseline; strikeout near the vertical middle.
        let line_px = (cell_h as usize / 10).max(1);
        let underline_y = y0 as usize + cell_h as usize - line_px - (cell_h as usize / 12);
        let strike_y = y0 as usize + (cell_h as usize * 5) / 12;
        let rule_color = if in_link
            || cell.flags.contains(Flags::UNDERLINE)
            || cell.flags.contains(Flags::STRIKEOUT)
        {
            Some((r, g, b))
        } else {
            None
        };
        if let Some((rr, gg, bb)) = rule_color {
            let x0u = x0 as usize;
            let mut draw_rule = |yb: usize| {
                for px in 0..cell_w as usize {
                    for dy in 0..line_px {
                        let rpx = x0u + px;
                        let rpy = yb + dy;
                        if rpx < buf.width && rpy < buf.height {
                            buf.pixels[rpy * buf.width + rpx] = argb(255, rr, gg, bb);
                        }
                    }
                }
            };
            if in_link || cell.flags.contains(Flags::UNDERLINE) {
                draw_rule(underline_y);
            }
            if cell.flags.contains(Flags::STRIKEOUT) {
                draw_rule(strike_y);
            }
        }

        // Bar cursors (underline / beam): draw over the cell after its glyph+rules, in the cursor
        // fg color. Underline = a bar at the bottom; beam = a vertical bar on the left edge.
        if is_cursor && is_bar {
            let (cr, cg, cb) = fg.to_rgb(colors);
            match cursor_shape {
                CursorShape::Underline => {
                    let barw = x0 as usize + cell_w as usize;
                    let btop = y0 as usize + cell_h as usize - (cell_h as usize / 5).max(2);
                    for py in btop..y0 as usize + cell_h as usize {
                        for px in x0 as usize..barw {
                            if px < buf.width && py < buf.height {
                                buf.pixels[py * buf.width + px] = argb(255, cr, cg, cb);
                            }
                        }
                    }
                }
                CursorShape::Beam => {
                    let barw = (cell_w as usize / 6).max(2);
                    for py in y0 as usize..y0 as usize + cell_h as usize {
                        for px in x0 as usize..(x0 as usize + barw).min(buf.width) {
                            if py < buf.height {
                                buf.pixels[py * buf.width + px] = argb(255, cr, cg, cb);
                            }
                        }
                    }
                }
                _ => {}
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

/// Every non-overlapping match of `query` across the whole grid (history + screen), as
/// (line, column, width) positions — used to highlight ALL matches while the overlay is open.
pub fn all_matches(term: &Term<Listener>, query: &str) -> Vec<Find> {
    if query.is_empty() {
        return Vec::new();
    }
    let q = query.to_lowercase();
    let grid = term.grid();
    let bottom = grid.bottommost_line().0;
    let mut line = grid.topmost_line().0;
    let mut out = Vec::new();
    while line <= bottom {
        let text = line_text(term, line).to_lowercase();
        let mut rest = text.as_str();
        let mut col = 0usize;
        while let Some(ci) = rest.find(&q) {
            out.push((line, col + ci, match_width(query)));
            let advance = ci + q.len();
            col += advance;
            rest = if advance < rest.len() {
                &rest[advance..]
            } else {
                ""
            };
        }
        line += 1;
    }
    out
}

/// Count every non-overlapping match of `query` across the whole grid (history + screen). Used for
/// the "N matches" indicator in the Find overlay.
pub fn count_matches(term: &Term<Listener>, query: &str) -> usize {
    all_matches(term, query).len()
}

/// Measure the pixel width `draw_text` would consume for `text` at `font_px`, without drawing.
/// Uses the same per-glyph advances as the paint path so chrome hit-testing and layout agree.
pub fn text_width(cache: &mut GlyphCache, text: &str, font_px: u32) -> usize {
    let mut w = 0usize;
    for ch in text.chars() {
        if ch == '\n' {
            continue;
        }
        let (gw, _, _) = cache.glyph(ch, font_px, false);
        w += gw as usize;
    }
    w
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

/// Path to a usable mono font. Precedence: `font_path` from config.toml, then the `HARNESS_FONT`
/// env override (handy for portable/CI setups), then the platform default.
pub fn font_path() -> String {
    if let Some(p) = crate::config::Config::load().font_path {
        return p;
    }
    if let Ok(p) = std::env::var("HARNESS_FONT") {
        if !p.is_empty() {
            return p;
        }
    }
    // macOS SF Mono.
    "/System/Library/Fonts/SFNSMono.ttf".to_string()
}

/// 256-color palette lookup (standard xterm cube/greyscale). The first 16 use the themed palette.
fn color256(i: u8, colors: &Colors) -> (u8, u8, u8) {
    if i < 16 {
        return PaletteColor::Named {
            bright: i > 7,
            index: i & 7,
        }
        .to_rgb(colors);
    }
    if i < 232 {
        let n = i - 16;
        let r = n / 36;
        let g = (n / 6) % 6;
        let b = n % 6;
        let ramp = |v: u8| -> u8 {
            if v == 0 {
                0
            } else {
                55 + v * 40
            }
        };
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

    /// The default theme (a modern Tokyo Night-inspired dark palette) is what renders when no
    /// `[theme]` block is configured; a config theme still overrides individual entries.
    #[test]
    fn default_theme_matches_built_in_colors() {
        let c = Colors::default();
        assert_eq!(c.fg, (0xc0, 0xca, 0xf5));
        assert_eq!(c.bg, (0x1a, 0x1b, 0x26));
        assert_eq!(c.selection, (0x33, 0x46, 0x7c));
        assert_eq!(c.copy_cursor, (0x9e, 0xce, 0x6a));
        assert_eq!(c.ansi[1], (0xf7, 0x76, 0x8e)); // red
        assert_eq!(c.ansi[9], (0xff, 0x7a, 0x93)); // bright red
        assert_eq!(c.ansi[15], (0xc0, 0xca, 0xf5)); // bright white
                                                    // Spec and index lookups yield the exact palette colors.
        let named = PaletteColor::Named {
            bright: false,
            index: 2,
        };
        assert_eq!(named.to_rgb(&c), (0x9e, 0xce, 0x6a)); // green
        let named_bright = PaletteColor::Named {
            bright: true,
            index: 2,
        };
        assert_eq!(named_bright.to_rgb(&c), (0xb9, 0xf2, 0x7c)); // bright green
    }

    /// A configured theme overrides foreground/background and individual ANSI entries; unset ones
    /// keep the built-in defaults, and the unknown/Dim specials fold to the themed fg.
    #[test]
    fn theme_overrides_resolve() {
        use crate::config::Theme;
        let t = Theme {
            foreground: Some([240, 240, 240]),
            background: Some([5, 5, 5]),
            ansi: Some({
                let mut a = [None; 16];
                a[1] = Some([200, 0, 0]); // override red
                Some(a).unwrap()
            }),
            ..Default::default()
        };
        let c = Colors::from(&t);
        assert_eq!(c.fg, (240, 240, 240));
        assert_eq!(c.bg, (5, 5, 5));
        assert_eq!(c.ansi[1], (200, 0, 0)); // overridden
        assert_eq!(c.ansi[2], (0x9e, 0xce, 0x6a)); // untouched built-in green
                                                   // Dim red folds to the themed foreground.
        let dim = cell_color(&ansi::Color::Named(ansi::NamedColor::DimRed), &c);
        assert_eq!(dim.to_rgb(&c), (240, 240, 240));
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
        assert_eq!(color256(0, &Colors::default()), (0x15, 0x16, 0x1e));
        assert_eq!(color256(15, &Colors::default()), (0xc0, 0xca, 0xf5));
    }

    /// Each named preset resolves to a distinct background and first ANSI color; unknown/blank
    /// names fall back to the built-in default.
    #[test]
    fn named_presets_resolve_and_unknown_falls_back() {
        let defaults = Colors::default();
        let gruv = Colors::preset("gruvbox-dark");
        let nord = Colors::preset("nord");
        let solar = Colors::preset("solarized-dark");
        assert_ne!(gruv.bg, defaults.bg);
        assert_ne!(nord.bg, defaults.bg);
        assert_ne!(solar.bg, defaults.bg);
        // Distinct palettes are mutually different (not all silently defaulting).
        assert_ne!(gruv.ansi[1], nord.ansi[1]);
        // Case/whitespace-insensitive, and unknown names fall back to the default.
        assert_eq!(Colors::preset("Nord"), nord);
        assert_eq!(Colors::preset("  "), defaults);
        assert_eq!(Colors::preset("made-up"), defaults);
        // tokyo-night is the built-in default.
        assert_eq!(Colors::preset("tokyo-night"), defaults);
    }

    /// A `[theme]` preset layers: preset supplies the palette, and a field-level override wins for
    /// just that entry (the rest stay from the preset, not the built-in default).
    #[test]
    fn preset_layers_with_field_override() {
        use crate::config::Theme;
        let t = Theme {
            preset: Some("gruvbox-dark".into()),
            background: Some([10, 20, 30]),
            ..Default::default()
        };
        let c = Colors::from(&t);
        // Override wins for bg…
        assert_eq!(c.bg, (10, 20, 30));
        // …but the rest come from the gruvbox preset (fg + red), not the built-in tokyo-night.
        assert_eq!(c.fg, (0xeb, 0xdb, 0xb2));
        assert_eq!(c.ansi[1], (0xcc, 0x24, 0x1d));
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
        let size = crate::session::TermSize {
            lines: 24,
            cols: 80,
        };
        let term = FairMutex::new(Term::new(Config::default(), &size, Listener::default()));
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
            draw_grid(
                &mut fb,
                &g,
                9,
                18,
                12,
                &mut cache,
                &Colors::default(),
                None,
                &[],
                None,
                None,
            );
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
        let term = FairMutex::new(Term::new(Config::default(), &size, Listener::default()));

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
            assert!(
                g.grid().display_offset() == 0,
                "live view should be at bottom"
            );
            assert!(
                g.grid().history_size() > 0,
                "expected history to accumulate"
            );

            // Scroll up one page; the display offset becomes non-zero.
            use alacritty_terminal::grid::Dimensions;
            g.grid_mut().scroll_display(Scroll::Delta(4));
            assert!(
                g.grid().display_offset() > 0,
                "display_offset should rise after scroll"
            );
        }

        // Rendering at the scrolled position must produce glyph pixels (the scrolled-into-view rows).
        let mut fb = Framebuffer::new(40 * 9, 8 * 18);
        let mut cache = GlyphCache::load();
        {
            let g = term.lock();
            draw_grid(
                &mut fb,
                &g,
                9,
                18,
                12,
                &mut cache,
                &Colors::default(),
                None,
                &[],
                None,
                None,
            );
        }
        let non_blank = fb.pixels.iter().filter(|&&p| p != 0x0000_0000).count();
        assert!(
            non_blank > 20,
            "expected scrollback glyphs when scrolled, got {non_blank}"
        );
    }

    /// Search: find a query string across history, case-insensitively, and report its line+col.
    #[test]
    fn finds_text_in_scrollback() {
        use alacritty_terminal::sync::FairMutex;
        use alacritty_terminal::term::{Config, Term};
        use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

        use crate::session::Listener;

        let size = crate::session::TermSize { lines: 6, cols: 40 };
        let term = FairMutex::new(Term::new(Config::default(), &size, Listener::default()));
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
        assert!(
            hit.0 < 0,
            "first hit should be in history (negative line), got {}",
            hit.0
        );
        assert!(hit.2 > 0, "match should have a column and width");
    }

    /// SGR inverse: feed text with the inverse attribute; the cell bg should become the colored fg
    /// (green from the SGR before it), proving inverse swaps bg to the original fg color.
    #[test]
    fn sgr_inverse_swaps_fg_bg() {
        use alacritty_terminal::sync::FairMutex;
        use alacritty_terminal::term::{Config, Term};
        use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

        use crate::session::Listener;

        let size = crate::session::TermSize { lines: 2, cols: 40 };
        let term = FairMutex::new(Term::new(Config::default(), &size, Listener::default()));
        // Green foreground, then inverse on top of it ("A" becomes green-on-fg with inverse).
        let bytes = b"\x1b[32m\x1b[7mX";
        {
            let mut p: Processor<StdSyncHandler> = Processor::default();
            p.advance(&mut *term.lock(), bytes);
        }

        // Render: the inverse cell paints its background with the original fg (green).
        let mut fb = Framebuffer::new(40 * 9, 2 * 18);
        let mut cache = GlyphCache::load();
        {
            let g = term.lock();
            draw_grid(
                &mut fb,
                &g,
                9,
                18,
                12,
                &mut cache,
                &Colors::default(),
                None,
                &[],
                None,
                None,
            );
        }
        // Cell (0,0) bg should be green-ish (the original fg became the bg).
        let cell0 = fb.pixels[0];
        let r = (cell0 >> 16) & 0xff;
        let g = (cell0 >> 8) & 0xff;
        let b = cell0 & 0xff;
        assert!(
            g > 180 && r < 220 && b < 180,
            "inverse cell bg should be green, got rgb({r},{g},{b})"
        );
    }

    /// Cursor shapes: force the emulator to a beam cursor and assert the glyph cell is drawn plus a
    /// vertical bar on the left edge (i.e. the beam is distinct from a full block fill).
    #[test]
    fn beam_cursor_draws_vbar_not_block() {
        use alacritty_terminal::sync::FairMutex;
        use alacritty_terminal::term::{Config, Term};
        use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

        use crate::session::Listener;

        let size = crate::session::TermSize { lines: 1, cols: 5 };
        let term = FairMutex::new(Term::new(Config::default(), &size, Listener::default()));
        // Request a beam cursor (DECSCUSR 5 = beam; alacritty maps it to CursorShape::Beam). The
        // cursor stays at (0,0), so the beam is drawn over cell (0,0). No glyph is typed (space), so
        // the only non-background pixels come from the cursor itself.
        let mut p: Processor<StdSyncHandler> = Processor::default();
        {
            let mut g = term.lock();
            p.advance(&mut *g, b"\x1b[5 q"); // beam cursor
        }
        // Render: the left edge of cell (0,0) should be a non-blank beam bar, while the bottom-right
        // corner of the cell stays background (a full block cursor would fill it).
        let mut fb = Framebuffer::new(5 * 9, 18);
        let mut cache = GlyphCache::load();
        let g = term.lock();
        draw_grid(
            &mut fb,
            &g,
            9,
            18,
            12,
            &mut cache,
            &Colors::default(),
            None,
            &[],
            None,
            None,
        );
        drop(g);

        let cell_x = 0;
        // Left-edge column (x=0) of the cursor cell should be non-black (beam top-to-bottom).
        let left_edge_filled = (0..18).any(|py| fb.pixels[py * fb.width + cell_x] != 0);
        assert!(
            left_edge_filled,
            "beam cursor should draw a vertical bar at cell's left edge"
        );
        // The bottom-right corner must be background — a block cursor would fill it with the cursor
        // fg color. Compare RGB only (alpha is always 255 in the framebuffer). The background is
        // the theme bg, not black.
        let corner = fb.pixels[17 * fb.width + (9 - 1)] & 0x00ff_ffff;
        let bg_rgb = (DEFAULT_BG.0 as u32) << 16 | (DEFAULT_BG.1 as u32) << 8 | DEFAULT_BG.2 as u32;
        assert_eq!(
            corner, bg_rgb,
            "beam cursor must not fill the cell to its bottom-right corner"
        );
    }

    /// Copy-mode read cursor: drawing with a `copy` point fills exactly that cell with the bright
    /// green block cursor color (so the user can see where selection starts).
    #[test]
    fn copy_cursor_draws_green_block() {
        use alacritty_terminal::sync::FairMutex;
        use alacritty_terminal::term::{Config, Term};
        use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

        use crate::session::Listener;

        let size = crate::session::TermSize { lines: 2, cols: 6 };
        let term = FairMutex::new(Term::new(Config::default(), &size, Listener::default()));
        let mut p: Processor<StdSyncHandler> = Processor::default();
        p.advance(&mut *term.lock(), b"abc\r\n123");
        let mut fb = Framebuffer::new(6 * 9, 2 * 18);
        let mut cache = GlyphCache::load();
        let g = term.lock();
        // Copy cursor at grid (line 0, col 1) = the "b" cell.
        draw_grid(
            &mut fb,
            &g,
            9,
            18,
            12,
            &mut cache,
            &Colors::default(),
            None,
            &[],
            None,
            Some((0, 1)),
        );
        drop(g);
        // Center pixel of cell (line 0, col 1) should be the copy-cursor green (0x9ece6a).
        let px = fb.pixels[13]; // center pixel (row 0, col 1) framebuffer index
        let (r, gg, b) = ((px >> 16) & 0xff, (px >> 8) & 0xff, px & 0xff);
        assert!(
            gg > 180 && r < 220 && b < 180,
            "copy cursor should paint the target cell green, got rgb({r},{gg},{b})"
        );
    }

    /// Match counting: a query present multiple times across history counts all non-overlapping
    /// occurrences (used for the "N matches" search indicator).
    #[test]
    fn counts_all_matches_across_history() {
        use alacritty_terminal::sync::FairMutex;
        use alacritty_terminal::term::{Config, Term};
        use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

        use crate::session::Listener;

        let size = crate::session::TermSize { lines: 4, cols: 40 };
        let term = FairMutex::new(Term::new(Config::default(), &size, Listener::default()));
        // Two "fix" on one line, one on another, none on the last.
        let bytes = b"fix foo fix\r\nno match here\r\nfix again";
        {
            let mut p: Processor<StdSyncHandler> = Processor::default();
            p.advance(&mut *term.lock(), bytes);
        }
        let g = term.lock();
        assert_eq!(
            count_matches(&g, "fix"),
            3,
            "expected 3 total non-overlapping matches"
        );
        assert_eq!(count_matches(&g, "zzz"), 0);
        assert_eq!(count_matches(&g, ""), 0);
    }

    /// all_matches returns every occurrence with correct (line, col, width) so draw_grid can
    /// highlight them all; line/col are grid-relative display coordinates.
    #[test]
    fn all_matches_lists_every_occurrence() {
        use alacritty_terminal::sync::FairMutex;
        use alacritty_terminal::term::{Config, Term};
        use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

        use crate::session::Listener;

        let size = crate::session::TermSize { lines: 4, cols: 40 };
        let term = FairMutex::new(Term::new(Config::default(), &size, Listener::default()));
        let bytes = b"fix foo fix\r\nno match here\r\nfix again";
        {
            let mut p: Processor<StdSyncHandler> = Processor::default();
            p.advance(&mut *term.lock(), bytes);
        }
        let g = term.lock();
        // First line (row 0): two matches at cols 0 and 8. Third line (row 2): one at col 0.
        let hits = all_matches(&g, "fix");
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0], (0, 0, 3));
        assert_eq!(hits[1], (0, 8, 3));
        assert_eq!(hits[2], (2, 0, 3));
        assert!(all_matches(&g, "zzz").is_empty());
    }

    /// The font-fallback validator accepts a real, parseable mono font (SF Mono or Monaco, both
    /// bundled on macOS) and never returns empty bytes for it.
    #[test]
    fn read_valid_font_accepts_a_real_mono_font() {
        for cand in [
            "/System/Library/Fonts/SFNSMono.ttf",
            "/System/Library/Fonts/Monaco.ttf",
        ] {
            if std::path::Path::new(cand).exists() {
                let data = read_valid_font(cand).expect("a real font must validate");
                assert!(!data.is_empty());
                return;
            }
        }
        // No system mono face present (non-macOS) — the rejection tests below still run.
    }

    /// A nonexistent path and a corrupt file both fail validation, so a bad configured `font_path`
    /// degrades to the fallback chain instead of panicking on launch.
    #[test]
    fn read_valid_font_rejects_missing_and_corrupt() {
        assert!(read_valid_font("/no/such/font-file.ttf").is_none());
        let path = std::env::temp_dir().join("ht_invalid_font_probe.ttf");
        std::fs::write(&path, b"this is definitely not a valid font").expect("write temp probe");
        let spath = path.to_string_lossy().into_owned();
        assert!(read_valid_font(&spath).is_none());
        let _ = std::fs::remove_file(&path);
    }

    /// The idle-wake signature must change when a pane redraws the screen IN PLACE (no new
    /// scrollback) — e.g. a vim cursor move or homegrown spinner — because `history_len` stays flat
    /// for those and the old detector would leave the terminal frozen on stale output. Also asserts
    /// an identical grid yields an identical signature (no spurious wakes) and that color changes
    /// are seen.
    #[test]
    fn visible_signature_tracks_in_place_redraws_without_scrollback() {
        use alacritty_terminal::sync::FairMutex;
        use alacritty_terminal::term::{Config, Term};
        use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

        let size = crate::session::TermSize { lines: 4, cols: 40 };
        let term = FairMutex::new(Term::new(Config::default(), &size, Listener::default()));
        fn render(t: &FairMutex<Term<Listener>>, bytes: &[u8]) {
            let mut p: Processor<StdSyncHandler> = Processor::default();
            p.advance(&mut *t.lock(), bytes);
        }

        // A prompt, then a carriage-return overwrite of the SAME line (like a spinner/progress row).
        render(&term, b"$ waiting\rwaiting for agent...");
        let g0 = term.lock();
        let s0 = visible_signature(&g0);
        // No scrollback was created — the same-line rewrite didn't push history.
        assert_eq!(g0.grid().history_size(), 0);
        drop(g0);

        // A genuinely different in-place update (the spinner advances) MUST change the signature,
        // even though history still hasn't grown.
        render(&term, b"\rwaiting for agent  .");
        let g2 = term.lock();
        assert_eq!(g2.grid().history_size(), 0);
        let s2 = visible_signature(&g2);
        assert_ne!(s2, s0, "in-place change without scrollback must be seen");
        drop(g2);

        // A color-only change (same chars, SGR color) is also detected.
        render(&term, b"\x1b[31mwaiting for agent  .\x1b[0m");
        let g3 = term.lock();
        assert_ne!(visible_signature(&g3), s2);
    }
}
