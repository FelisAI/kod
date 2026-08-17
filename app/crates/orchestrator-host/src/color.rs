//! Terminal color resolution (docs/013 §1, render plane).
//!
//! alacritty cells carry `Color = Named | Spec(rgb) | Indexed(u8)`. The GUI
//! needs concrete `0xRRGGBB`. We resolve against a 16-color ANSI palette tuned
//! to sit on Focused Dark (#14161B) — claude/codex TUIs lean on the bright
//! variants, so the palette is chosen to read well against charcoal, not a
//! generic xterm dump.

use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

// Focused Dark surface defaults (match the GUI tokens in main.rs).
pub const DEFAULT_FG: u32 = 0xC7CBD4;
pub const DEFAULT_BG: u32 = 0x14161B;

// 16-color ANSI palette, One-Dark-adjacent, legible on charcoal.
const ANSI16: [u32; 16] = [
    0x14161B, // 0 black (= bg, so "black" text is invisible-by-design like a real term)
    0xE68A8A, // 1 red
    0x7FCBA4, // 2 green
    0xE6C07A, // 3 yellow
    0x7FB0E6, // 4 blue
    0xC8A6E6, // 5 magenta
    0x7ED6D6, // 6 cyan
    0xC7CBD4, // 7 white
    0x5C636F, // 8 bright black (muted gray)
    0xF0A0A0, // 9 bright red
    0x8FE3B6, // 10 bright green
    0xF0D196, // 11 bright yellow
    0x9CC4F5, // 12 bright blue
    0xD7BAF5, // 13 bright magenta
    0x9CE8E8, // 14 bright cyan
    0xE7EAF0, // 15 bright white
];

fn named(c: NamedColor) -> u32 {
    use NamedColor::*;
    match c {
        Black => ANSI16[0],
        Red => ANSI16[1],
        Green => ANSI16[2],
        Yellow => ANSI16[3],
        Blue => ANSI16[4],
        Magenta => ANSI16[5],
        Cyan => ANSI16[6],
        White => ANSI16[7],
        BrightBlack => ANSI16[8],
        BrightRed => ANSI16[9],
        BrightGreen => ANSI16[10],
        BrightYellow => ANSI16[11],
        BrightBlue => ANSI16[12],
        BrightMagenta => ANSI16[13],
        BrightCyan => ANSI16[14],
        BrightWhite => ANSI16[15],
        Foreground | BrightForeground => DEFAULT_FG,
        Background => DEFAULT_BG,
        Cursor => DEFAULT_FG,
        DimBlack => ANSI16[0],
        DimRed => 0xB36B6B,
        DimGreen => 0x639E80,
        DimYellow => 0xB39660,
        DimBlue => 0x6389B3,
        DimMagenta => 0x9C82B3,
        DimCyan => 0x62A8A8,
        DimWhite => 0x9AA0AB,
        DimForeground => 0x9AA0AB,
    }
}

fn rgb(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

/// Resolve a palette index (0..=255) to `0xRRGGBB` — used to answer OSC 4
/// color queries.
pub fn resolve_index(i: u8) -> u32 {
    indexed(i)
}

fn indexed(i: u8) -> u32 {
    match i {
        0..=15 => ANSI16[i as usize],
        16..=231 => {
            // 6×6×6 color cube
            let i = i - 16;
            let steps = [0u8, 95, 135, 175, 215, 255];
            let r = steps[(i / 36) as usize];
            let g = steps[((i / 6) % 6) as usize];
            let b = steps[(i % 6) as usize];
            rgb(r, g, b)
        }
        232..=255 => {
            // grayscale ramp
            let v = 8 + (i - 232) * 10;
            rgb(v, v, v)
        }
    }
}

fn resolve(c: Color, default: u32) -> u32 {
    match c {
        Color::Named(n) => named(n),
        Color::Spec(Rgb { r, g, b }) => rgb(r, g, b),
        Color::Indexed(i) => {
            // alacritty uses 256 = default fg sentinel in some paths; guard
            if i == 0 && default == DEFAULT_BG {
                ANSI16[0]
            } else {
                indexed(i)
            }
        }
    }
}

/// Resolve a cell's (fg, bg) to concrete RGB, honoring INVERSE and DIM.
pub fn resolve_pair(fg: Color, bg: Color, inverse: bool, dim: bool) -> (u32, u32) {
    let mut f = resolve(fg, DEFAULT_FG);
    let mut b = resolve(bg, DEFAULT_BG);
    if dim {
        f = dim_rgb(f);
    }
    if inverse {
        std::mem::swap(&mut f, &mut b);
    }
    (f, b)
}

fn dim_rgb(c: u32) -> u32 {
    let r = ((c >> 16) & 0xff) * 2 / 3;
    let g = ((c >> 8) & 0xff) * 2 / 3;
    let b = (c & 0xff) * 2 / 3;
    (r << 16) | (g << 8) | b
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::vte::ansi::{Color, NamedColor};

    #[test]
    fn default_fg_bg_resolve_to_focused_dark() {
        let (f, b) = resolve_pair(
            Color::Named(NamedColor::Foreground),
            Color::Named(NamedColor::Background),
            false,
            false,
        );
        assert_eq!(f, DEFAULT_FG);
        assert_eq!(b, DEFAULT_BG);
    }

    #[test]
    fn inverse_swaps() {
        let (f, b) = resolve_pair(
            Color::Named(NamedColor::Foreground),
            Color::Named(NamedColor::Background),
            true,
            false,
        );
        assert_eq!(f, DEFAULT_BG);
        assert_eq!(b, DEFAULT_FG);
    }

    #[test]
    fn cube_and_grayscale_indices() {
        assert_eq!(indexed(16), 0x000000); // cube origin
        assert_eq!(indexed(231), 0xFFFFFF); // cube max
        let g = indexed(244); // mid gray
        assert_eq!((g >> 16) & 0xff, (g >> 8) & 0xff); // r==g==b
    }

    #[test]
    fn spec_rgb_passes_through() {
        let (f, _) = resolve_pair(
            Color::Spec(Rgb { r: 0x12, g: 0x34, b: 0x56 }),
            Color::Named(NamedColor::Background),
            false,
            false,
        );
        assert_eq!(f, 0x123456);
    }
}
