//! Text shaping and rasterization on top of cosmic-text.
//!
//! Layout works in logical pixels while glyphs are rasterized at the surface's physical size,
//! so the renderer keeps the output scale here and converts on the way in and out. Measuring
//! and drawing therefore always agree, whatever the scale factor is.

use std::collections::HashMap;

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache};
use tiny_skia::{PixmapMut, PremultipliedColorU8};

use crate::color::Color;
use crate::layout::Measure;

pub struct TextRenderer {
    fonts: FontSystem,
    swash: SwashCache,
    family: String,
    /// Font size in logical pixels.
    size: f32,
    scale: f32,
    /// Logical widths, keyed by scale-independent text. Cleared when the scale changes.
    widths: HashMap<String, f32>,
}

/// Families to try when the configured one is generic or missing.
const FALLBACK_FAMILIES: &[&str] = &[
    "Liberation Sans",
    "DejaVu Sans",
    "Noto Sans",
    "Fira Sans",
    "Cantarell",
    "Inter",
];

/// Names that mean "just give me the default UI font".
fn is_generic(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "sans" | "sans-serif" | "sansserif" | "system-ui" | "ui-sans-serif" | "default"
    )
}

/// Resolve a configured family name to one the font database actually has.
///
/// cosmic-text will not match generic CSS names such as `sans-serif`, and silently renders
/// nothing when a family is missing, so the name is pinned to a real family up front.
fn resolve_family(fonts: &mut FontSystem, requested: &str) -> String {
    let available: Vec<String> = fonts
        .db()
        .faces()
        .flat_map(|face| face.families.iter().map(|(name, _)| name.clone()))
        .collect();
    let find = |wanted: &str| {
        available
            .iter()
            .find(|name| name.eq_ignore_ascii_case(wanted))
            .cloned()
    };

    if !is_generic(requested) {
        if let Some(found) = find(requested) {
            return found;
        }
        log::warn!("font family {requested:?} is not installed; falling back");
    }

    let chosen = FALLBACK_FAMILIES
        .iter()
        .find_map(|name| find(name))
        .or_else(|| available.first().cloned())
        .unwrap_or_else(|| "sans-serif".to_string());

    // Point the generic families at something real so glyph fallback works too.
    fonts.db_mut().set_sans_serif_family(chosen.clone());
    chosen
}

impl TextRenderer {
    pub fn new(family: &str, size: f32) -> TextRenderer {
        let mut fonts = FontSystem::new();
        let family = resolve_family(&mut fonts, family);
        log::info!("using font family {family:?} at {size}px");
        TextRenderer {
            fonts,
            swash: SwashCache::new(),
            family,
            size,
            scale: 1.0,
            widths: HashMap::new(),
        }
    }

    pub fn set_scale(&mut self, scale: f32) {
        if (scale - self.scale).abs() > f32::EPSILON {
            self.scale = scale;
            self.widths.clear();
        }
    }

    /// Line height in logical pixels.
    pub fn line_height(&self) -> f32 {
        (self.size * 1.3).ceil()
    }

    fn metrics(&self) -> Metrics {
        Metrics::new(self.size * self.scale, self.line_height() * self.scale)
    }

    fn shaped(&mut self, text: &str) -> Buffer {
        let metrics = self.metrics();
        // Destructure so the font system and the family string can be borrowed at once.
        let TextRenderer { fonts, family, .. } = self;
        let mut buffer = Buffer::new(fonts, metrics);
        buffer.set_size(None, None);
        buffer.set_text(
            text,
            &Attrs::new().family(Family::Name(family)),
            Shaping::Advanced,
            None,
        );
        buffer.shape_until_scroll(fonts, false);
        buffer
    }

    /// Width of `text` in logical pixels.
    pub fn measure_text(&mut self, text: &str) -> f32 {
        if let Some(w) = self.widths.get(text) {
            return *w;
        }
        let buffer = self.shaped(text);
        let physical = buffer
            .layout_runs()
            .fold(0.0f32, |acc, run| acc.max(run.line_w));
        let logical = physical / self.scale;
        self.widths.insert(text.to_string(), logical);
        logical
    }

    /// Draw `text` with its top-left corner at logical `(x, y)`.
    pub fn draw(&mut self, pixmap: &mut PixmapMut<'_>, text: &str, x: f32, y: f32, color: Color) {
        if text.is_empty() || color.is_transparent() {
            return;
        }
        let scale = self.scale;
        let origin_x = (x * scale).round() as i32;
        let origin_y = (y * scale).round() as i32;

        let mut buffer = self.shaped(text);
        let fill = cosmic_text::Color::rgba(color.r, color.g, color.b, color.a);
        let TextRenderer { fonts, swash, .. } = self;
        buffer.draw(fonts, swash, fill, |gx, gy, w, h, c| {
            blend_rect(pixmap, origin_x + gx, origin_y + gy, w, h, c);
        });
    }
}

/// Source-over blend of a solid rect into a premultiplied pixmap, clipped to its bounds.
fn blend_rect(pixmap: &mut PixmapMut<'_>, x: i32, y: i32, w: u32, h: u32, c: cosmic_text::Color) {
    let src_a = c.a() as u32;
    if src_a == 0 || w == 0 || h == 0 {
        return;
    }
    let pw = pixmap.width() as i32;
    let ph = pixmap.height() as i32;

    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + w as i32).min(pw);
    let y1 = (y + h as i32).min(ph);
    if x0 >= x1 || y0 >= y1 {
        return;
    }

    let up = |v: u8| (v as u32 * src_a + 127) / 255;
    let (sr, sg, sb) = (up(c.r()), up(c.g()), up(c.b()));
    let inv = 255 - src_a;
    let pixels = pixmap.pixels_mut();

    for py in y0..y1 {
        let row = (py * pw) as usize;
        for px in x0..x1 {
            let slot = &mut pixels[row + px as usize];
            let dst = *slot;
            let over = |s: u32, d: u8| s + (d as u32 * inv + 127) / 255;
            let a = over(src_a, dst.alpha());
            let r = over(sr, dst.red()).min(a);
            let g = over(sg, dst.green()).min(a);
            let b = over(sb, dst.blue()).min(a);
            *slot =
                PremultipliedColorU8::from_rgba(r as u8, g as u8, b as u8, a as u8).unwrap_or(dst);
        }
    }
}

impl Measure for TextRenderer {
    fn measure(&mut self, text: &str) -> f32 {
        self.measure_text(text)
    }
}
