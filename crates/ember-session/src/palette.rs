//! The default color palette (design §4 gotcha: `alacritty_terminal` ships no
//! default theme — `term.colors()` is all-`None` until a program sets colors via
//! OSC — so the projection must own the defaults and resolve every cell to RGB).

use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb as AlacRgb};
use ember_core::Rgb;

/// A resolved 256-entry palette plus the fg/bg/cursor defaults.
#[derive(Clone, Debug)]
pub struct Palette {
    colors: [Rgb; 256],
    pub default_fg: Rgb,
    pub default_bg: Rgb,
    pub cursor: Rgb,
}

impl Default for Palette {
    fn default() -> Self {
        Self::dark()
    }
}

impl Palette {
    /// The standard xterm 256-color palette with a dark default theme.
    pub fn dark() -> Self {
        Self {
            colors: build_xterm_256(),
            default_fg: Rgb::new(0xcc, 0xcc, 0xcc),
            default_bg: Rgb::new(0x10, 0x10, 0x10),
            cursor: Rgb::new(0xcc, 0xcc, 0xcc),
        }
    }

    /// Resolve an alacritty cell color to concrete RGB, applying our defaults
    /// and palette where the engine carries no spec.
    pub fn resolve(&self, color: Color) -> Rgb {
        match color {
            Color::Spec(AlacRgb { r, g, b }) => Rgb::new(r, g, b),
            Color::Indexed(i) => self.colors[i as usize],
            Color::Named(named) => self.named(named),
        }
    }

    fn named(&self, named: NamedColor) -> Rgb {
        match named {
            NamedColor::Foreground | NamedColor::BrightForeground => self.default_fg,
            NamedColor::Background => self.default_bg,
            NamedColor::Cursor => self.cursor,
            NamedColor::DimForeground => self.default_fg,
            NamedColor::Black => self.colors[0],
            NamedColor::Red => self.colors[1],
            NamedColor::Green => self.colors[2],
            NamedColor::Yellow => self.colors[3],
            NamedColor::Blue => self.colors[4],
            NamedColor::Magenta => self.colors[5],
            NamedColor::Cyan => self.colors[6],
            NamedColor::White => self.colors[7],
            NamedColor::BrightBlack => self.colors[8],
            NamedColor::BrightRed => self.colors[9],
            NamedColor::BrightGreen => self.colors[10],
            NamedColor::BrightYellow => self.colors[11],
            NamedColor::BrightBlue => self.colors[12],
            NamedColor::BrightMagenta => self.colors[13],
            NamedColor::BrightCyan => self.colors[14],
            NamedColor::BrightWhite => self.colors[15],
            // Dim variants fall back to their normal counterpart.
            NamedColor::DimBlack => self.colors[0],
            NamedColor::DimRed => self.colors[1],
            NamedColor::DimGreen => self.colors[2],
            NamedColor::DimYellow => self.colors[3],
            NamedColor::DimBlue => self.colors[4],
            NamedColor::DimMagenta => self.colors[5],
            NamedColor::DimCyan => self.colors[6],
            NamedColor::DimWhite => self.colors[7],
        }
    }
}

/// Build the canonical xterm 256-color table: 16 base + 6×6×6 cube + 24 grays.
fn build_xterm_256() -> [Rgb; 256] {
    // A modern 16-color set tuned for a dark background — the classic VGA
    // palette (blue = #000080) is unreadably dark, so the normals are brightened.
    const BASE: [(u8, u8, u8); 16] = [
        (0x1a, 0x1a, 0x1a), // black
        (0xd9, 0x4a, 0x3d), // red
        (0x4e, 0xb8, 0x3a), // green
        (0xd9, 0xb0, 0x2c), // yellow
        (0x3b, 0x8e, 0xea), // blue
        (0xb0, 0x56, 0xd4), // magenta
        (0x3a, 0xb8, 0xb8), // cyan
        (0xcc, 0xcc, 0xcc), // white
        (0x66, 0x66, 0x66), // bright black
        (0xff, 0x6b, 0x5e), // bright red
        (0x6b, 0xe5, 0x52), // bright green
        (0xff, 0xd5, 0x4a), // bright yellow
        (0x5c, 0xa8, 0xff), // bright blue
        (0xc9, 0x7c, 0xf0), // bright magenta
        (0x5a, 0xd8, 0xd8), // bright cyan
        (0xff, 0xff, 0xff), // bright white
    ];

    let mut colors = [Rgb::new(0, 0, 0); 256];
    for (i, (r, g, b)) in BASE.iter().enumerate() {
        colors[i] = Rgb::new(*r, *g, *b);
    }
    // 216-color cube: indices 16..232.
    let step = |c: u8| -> u8 { if c == 0 { 0 } else { 55 + c * 40 } };
    let mut idx = 16;
    for r in 0..6u8 {
        for g in 0..6u8 {
            for b in 0..6u8 {
                colors[idx] = Rgb::new(step(r), step(g), step(b));
                idx += 1;
            }
        }
    }
    // 24-step grayscale ramp: indices 232..256.
    for i in 0..24u8 {
        let v = 8 + i * 10;
        colors[232 + i as usize] = Rgb::new(v, v, v);
    }
    colors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cube_and_grays_are_populated() {
        let p = Palette::dark();
        assert_eq!(p.resolve(Color::Indexed(0)), Rgb::new(0x1a, 0x1a, 0x1a));
        assert_eq!(p.resolve(Color::Indexed(15)), Rgb::new(0xff, 0xff, 0xff));
        // 196 = pure red in the cube (r=5,g=0,b=0).
        assert_eq!(p.resolve(Color::Indexed(196)), Rgb::new(255, 0, 0));
        // 231 = white corner of the cube.
        assert_eq!(p.resolve(Color::Indexed(231)), Rgb::new(255, 255, 255));
        // 232 = darkest gray.
        assert_eq!(p.resolve(Color::Indexed(232)), Rgb::new(8, 8, 8));
    }

    #[test]
    fn named_defaults_resolve() {
        let p = Palette::dark();
        assert_eq!(
            p.resolve(Color::Named(NamedColor::Foreground)),
            p.default_fg
        );
        assert_eq!(
            p.resolve(Color::Named(NamedColor::Background)),
            p.default_bg
        );
        assert_eq!(
            p.resolve(Color::Named(NamedColor::Red)),
            Rgb::new(0xd9, 0x4a, 0x3d)
        );
    }
}
