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

    /// This colour `t` of the way towards `other`.
    ///
    /// Mixed premultiplied. Blending the straight channels would drag a transparent
    /// colour's rgb into the middle of the travel, so a fade from a module's fill to no
    /// fill at all would go out through black rather than through its own colour.
    pub fn mix(self, other: Color, t: f32) -> Color {
        let t = t.clamp(0.0, 1.0);
        let lerp = |a: f32, b: f32| a + (b - a) * t;
        let alpha = lerp(self.a as f32, other.a as f32);
        if alpha <= 0.0 {
            return Color::TRANSPARENT;
        }
        let channel = |a: u8, b: u8| {
            let mixed = lerp(a as f32 * self.a as f32, b as f32 * other.a as f32);
            (mixed / alpha).round().clamp(0.0, 255.0) as u8
        };
        Color {
            r: channel(self.r, other.r),
            g: channel(self.g, other.g),
            b: channel(self.b, other.b),
            a: alpha.round() as u8,
        }
    }

    /// The same colour at a fraction of its opacity.
    ///
    /// What a menu wants for a row it will not let you choose and for the rule between two
    /// groups of rows: the same ink, quieter, rather than a second colour the config has
    /// to be told about.
    pub fn faded(self, factor: f32) -> Color {
        Color {
            a: (self.a as f32 * factor.clamp(0.0, 1.0)).round() as u8,
            ..self
        }
    }
}
