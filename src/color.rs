//! Color parsing for the `#rrggbb[aa]` forms used in the config and in i3bar blocks.

use anyhow::{Result, bail};

/// Straight (non-premultiplied) 8-bit RGBA.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const TRANSPARENT: Color = Color::rgba(0, 0, 0, 0);

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color {
        Color { r, g, b, a }
    }

    /// Parse `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa`. The leading `#` is optional.
    pub fn parse(s: &str) -> Result<Color> {
        let h = s.strip_prefix('#').unwrap_or(s);
        if !h.is_ascii() || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("color {s:?} is not hexadecimal");
        }
        let pair = |i: usize| -> Result<u8> { Ok(u8::from_str_radix(&h[i..i + 2], 16)?) };
        // In the short forms each digit is doubled: `c` means `cc`.
        let single = |i: usize| -> Result<u8> { Ok(u8::from_str_radix(&h[i..i + 1], 16)? * 0x11) };
        match h.len() {
            3 => Ok(Color::rgba(single(0)?, single(1)?, single(2)?, 0xff)),
            4 => Ok(Color::rgba(single(0)?, single(1)?, single(2)?, single(3)?)),
            6 => Ok(Color::rgba(pair(0)?, pair(2)?, pair(4)?, 0xff)),
            8 => Ok(Color::rgba(pair(0)?, pair(2)?, pair(4)?, pair(6)?)),
            _ => bail!("color {s:?} must be #rgb, #rgba, #rrggbb or #rrggbbaa"),
        }
    }

    pub fn is_transparent(self) -> bool {
        self.a == 0
    }

    pub fn to_skia(self) -> tiny_skia::Color {
        tiny_skia::Color::from_rgba8(self.r, self.g, self.b, self.a)
    }
}
