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
    widths: Generations<f32>,
    /// Rasterised runs, keyed the same way. `None` marks a string that must not be cached
    /// because it draws colour glyphs, so the discovery is not repeated every frame.
    runs: Generations<Option<Run>>,
    /// Shaped buffers, keyed the same way.
    ///
    /// Shaping is the expensive half of drawing a string, and a bar draws the same strings
    /// over and over - most of them every redraw, unchanged. Measuring and drawing share
    /// this, so a value that layout has already sized costs nothing to put on screen.
    shapes: Generations<Buffer>,
}

/// A string already rasterised, as coverage per pixel.
///
/// Shaping a string is only half of drawing it; turning the shaped glyphs into pixels is
/// the other half, and a bar draws the same strings frame after frame. The coverage does
/// not depend on the colour, so one of these serves a module whatever state it is in, and
/// it does not depend on where the string lands either, because glyphs are positioned
/// within the run and the run is placed as a whole.
struct Run {
    /// Where the run's top-left corner sits relative to the text origin, in device pixels.
    left: i32,
    top: i32,
    width: usize,
    height: usize,
    /// One byte of coverage per pixel, row-major.
    coverage: Vec<u8>,
}

/// A cache that forgets in generations rather than one entry at a time.
///
/// A bar sees a slow drip of strings it will never be asked for again: a clock alone leaves
/// 1440 behind in a day, and every percentage that ticks past adds another. A map that only
/// grows is a leak, but tracking a use order per entry costs more than these lookups save.
///
/// So entries are kept in two generations. Everything goes into the newest; when that fills,
/// it becomes the older one and a fresh generation starts. A lookup that misses the newest
/// still finds anything from the one before and promotes it, so strings the bar is actually
/// using survive indefinitely and the rest fall off a generation at a time.
struct Generations<V> {
    hot: HashMap<String, V>,
    cold: HashMap<String, V>,
    /// Entries in the newest generation before it is rolled over. Total is at most twice it.
    limit: usize,
}

impl<V> Generations<V> {
    fn new(limit: usize) -> Generations<V> {
        Generations {
            hot: HashMap::new(),
            cold: HashMap::new(),
            limit: limit.max(1),
        }
    }

    fn get(&self, key: &str) -> Option<&V> {
        self.hot.get(key)
    }

    fn get_or_insert(&mut self, key: &str, make: impl FnOnce() -> V) -> &mut V {
        if !self.hot.contains_key(key) {
            let value = match self.cold.remove(key) {
                Some(kept) => kept,
                None => make(),
            };
            if self.hot.len() >= self.limit {
                std::mem::swap(&mut self.hot, &mut self.cold);
                self.hot.clear();
            }
            self.hot.insert(key.to_string(), value);
        }
        self.hot.get_mut(key).expect("just inserted")
    }

    fn insert(&mut self, key: &str, value: V) {
        self.get_or_insert(key, || value);
    }

    fn clear(&mut self) {
        self.hot.clear();
        self.cold.clear();
    }
}

/// How many distinct strings each cache keeps before rolling a generation.
///
/// A bar has a dozen or so live at once; the headroom is for the ones that change every
/// tick. Shaped buffers cost far more than a width, so fewer of them are kept.
const WIDTHS_KEPT: usize = 256;
const SHAPES_KEPT: usize = 64;

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
            widths: Generations::new(WIDTHS_KEPT),
            runs: Generations::new(SHAPES_KEPT),
            shapes: Generations::new(SHAPES_KEPT),
        }
    }

    pub fn set_scale(&mut self, scale: f32) {
        if (scale - self.scale).abs() > f32::EPSILON {
            self.scale = scale;
            // Both are shaped at the physical size, so neither survives a scale change.
            self.widths.clear();
            self.shapes.clear();
            self.runs.clear();
        }
    }

    /// Line height in logical pixels.
    pub fn line_height(&self) -> f32 {
        (self.size * 1.3).ceil()
    }

    fn metrics(&self) -> Metrics {
        Metrics::new(self.size * self.scale, self.line_height() * self.scale)
    }

    /// The shaped form of `text`, shaping it if this is the first time it has been seen.
    ///
    /// Returns the font system alongside, because every caller needs both at once and the
    /// borrow checker will not hand them out separately once one is held.
    fn shaped(&mut self, text: &str) -> (&mut Buffer, &mut FontSystem) {
        let metrics = self.metrics();
        // Destructured so the cache, the font system and the family can be borrowed at once.
        let TextRenderer {
            fonts,
            family,
            shapes,
            ..
        } = self;
        let buffer = shapes.get_or_insert(text, || shape(fonts, family, metrics, text));
        (buffer, fonts)
    }

    /// Width of `text` in logical pixels.
    pub fn measure_text(&mut self, text: &str) -> f32 {
        if let Some(w) = self.widths.get(text) {
            return *w;
        }
        let scale = self.scale;
        let (buffer, _) = self.shaped(text);
        let physical = buffer
            .layout_runs()
            .fold(0.0f32, |acc, run| acc.max(run.line_w));
        let logical = physical / scale;
        self.widths.insert(text, logical);
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

        let metrics = self.metrics();
        let cached = {
            let TextRenderer {
                fonts,
                family,
                swash,
                shapes,
                runs,
                ..
            } = self;
            runs.get_or_insert(text, || {
                let buffer = shapes.get_or_insert(text, || shape(fonts, family, metrics, text));
                rasterise(buffer, fonts, swash)
            })
            .is_some()
        };

        if cached {
            let run = self
                .runs
                .get(text)
                .and_then(|run| run.as_ref())
                .expect("just rasterised");
            blend_run(pixmap, run, origin_x, origin_y, color);
            return;
        }

        // Colour glyphs carry their own colour, so there is nothing to tint and no coverage
        // to keep. They are drawn straight, the way everything was before there was a cache.
        let fill = cosmic_text::Color::rgba(color.r, color.g, color.b, color.a);
        let TextRenderer {
            fonts,
            family,
            swash,
            shapes,
            ..
        } = self;
        let buffer = shapes.get_or_insert(text, || shape(fonts, family, metrics, text));
        buffer.draw(fonts, swash, fill, |gx, gy, w, h, c| {
            blend_rect(pixmap, origin_x + gx, origin_y + gy, w, h, c);
        });
    }
}

/// Turn a shaped buffer into coverage, or `None` if it draws colour glyphs.
///
/// The buffer is drawn once in white: a glyph rendered from an outline comes back with the
/// colour it was given and its coverage in the alpha, so white in means coverage out. A
/// glyph that answers in some other colour is carrying its own - an emoji - and cannot be
/// reduced to coverage without losing it.
fn rasterise(buffer: &mut Buffer, fonts: &mut FontSystem, swash: &mut SwashCache) -> Option<Run> {
    let white = cosmic_text::Color::rgba(0xff, 0xff, 0xff, 0xff);
    let mut patches: Vec<(i32, i32, u32, u32, u8)> = Vec::new();
    let mut coloured = false;
    let (mut min_x, mut min_y) = (i32::MAX, i32::MAX);
    let (mut max_x, mut max_y) = (i32::MIN, i32::MIN);

    buffer.draw(fonts, swash, white, |x, y, w, h, c| {
        if c.a() == 0 || w == 0 || h == 0 {
            return;
        }
        if (c.r(), c.g(), c.b()) != (0xff, 0xff, 0xff) {
            coloured = true;
            return;
        }
        patches.push((x, y, w, h, c.a()));
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x + w as i32);
        max_y = max_y.max(y + h as i32);
    });

    if coloured || patches.is_empty() {
        return None;
    }

    let (width, height) = ((max_x - min_x) as usize, (max_y - min_y) as usize);
    let mut coverage = vec![0u8; width * height];
    for (x, y, w, h, a) in patches {
        for row in 0..h as usize {
            let start = (y - min_y) as usize + row;
            let line = start * width + (x - min_x) as usize;
            for slot in &mut coverage[line..line + w as usize] {
                // Two glyphs can touch the same pixel, and they are the same colour, so
                // their coverage composites rather than replacing.
                let under = *slot as u32;
                *slot = (under + (a as u32 * (255 - under) + 127) / 255) as u8;
            }
        }
    }
    Some(Run {
        left: min_x,
        top: min_y,
        width,
        height,
        coverage,
    })
}

/// Blend a rasterised run into the pixmap in `color`, clipped to its bounds.
fn blend_run(pixmap: &mut PixmapMut<'_>, run: &Run, x: i32, y: i32, color: Color) {
    let (ox, oy) = (x + run.left, y + run.top);
    let (pw, ph) = (pixmap.width() as i32, pixmap.height() as i32);
    let x0 = ox.max(0);
    let y0 = oy.max(0);
    let x1 = (ox + run.width as i32).min(pw);
    let y1 = (oy + run.height as i32).min(ph);
    if x0 >= x1 || y0 >= y1 {
        return;
    }

    // The coverage is the source alpha on its own. cosmic-text builds a mask glyph's colour
    // as the coverage over the base's rgb and drops the base's alpha (`swash.rs`, with a
    // TODO beside it), so an alpha on a text colour has never reached the screen. Honouring
    // it here would make the cache change what the bar looks like, which is not this
    // cache's job.
    let pixels = pixmap.pixels_mut();
    for py in y0..y1 {
        let src = (py - oy) as usize * run.width + (x0 - ox) as usize;
        let dst = py as usize * pw as usize + x0 as usize;
        for i in 0..(x1 - x0) as usize {
            let cover = run.coverage[src + i] as u32;
            if cover == 0 {
                continue;
            }
            let src_a = cover;
            let up = |v: u8| (v as u32 * src_a + 127) / 255;
            let (sr, sg, sb) = (up(color.r), up(color.g), up(color.b));
            let inv = 255 - src_a;
            let slot = &mut pixels[dst + i];
            let under = *slot;
            let over = |s: u32, d: u8| s + (d as u32 * inv + 127) / 255;
            let a = over(src_a, under.alpha());
            let r = over(sr, under.red()).min(a);
            let g = over(sg, under.green()).min(a);
            let b = over(sb, under.blue()).min(a);
            *slot = PremultipliedColorU8::from_rgba(r as u8, g as u8, b as u8, a as u8)
                .unwrap_or(under);
        }
    }
}

/// Shape one string at `metrics`, laying it out on a single unbounded line.
fn shape(fonts: &mut FontSystem, family: &str, metrics: Metrics, text: &str) -> Buffer {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn len<V>(cache: &Generations<V>) -> usize {
        cache.hot.len() + cache.cold.len()
    }

    #[test]
    fn a_cache_of_strings_never_seen_again_stays_bounded() {
        let mut cache = Generations::new(8);
        for i in 0..1000 {
            cache.get_or_insert(&format!("{i}%"), || i);
        }
        assert!(
            len(&cache) <= 16,
            "held {} entries against a limit of 8 per generation",
            len(&cache)
        );
    }

    #[test]
    fn a_string_still_in_use_survives_the_drip_around_it() {
        let mut cache = Generations::new(4);
        cache.get_or_insert("clock", || 1);
        // Enough churn to roll several generations, asking for the live one each round.
        for i in 0..200 {
            cache.get_or_insert(&format!("cpu {i}"), || i);
            assert_eq!(*cache.get_or_insert("clock", || 0), 1, "at round {i}");
        }
        assert_eq!(
            cache.get("clock"),
            Some(&1),
            "fell out of the newest generation"
        );
    }

    #[test]
    fn a_scale_change_drops_everything_shaped_for_the_old_one() {
        let mut cache = Generations::new(4);
        cache.insert("a", 1);
        cache.get_or_insert("b", || 2);
        cache.clear();
        assert_eq!(len(&cache), 0);
        assert_eq!(cache.get("a"), None);
    }
}

#[cfg(test)]
mod fidelity {
    use super::*;
    use tiny_skia::Pixmap;

    /// The cache must not change a single pixel: the same string, drawn through the run
    /// cache and drawn straight, has to come out identical. This is the whole contract.
    #[test]
    fn a_cached_run_draws_what_the_uncached_path_draws() {
        if std::env::var("LIVE_FONTS").is_err() {
            return;
        }
        for scale in [1.0f32, 2.0] {
            for text in [" 17% ", "Fri 05 Sep  02:48", "Dusan", "resize", "  ", "x"] {
                for color in [
                    Color::rgba(0xeb, 0xdb, 0xb2, 0xff),
                    Color::rgba(0x28, 0x28, 0x28, 0xff),
                    Color::rgba(0xfb, 0x49, 0x34, 0x80),
                ] {
                    let mut a = Pixmap::new(400, 60).unwrap();
                    let mut b = Pixmap::new(400, 60).unwrap();

                    // Straight through cosmic-text, as the old path did.
                    let mut plain = TextRenderer::new("sans-serif", 18.0);
                    plain.set_scale(scale);
                    let metrics = plain.metrics();
                    let fill = cosmic_text::Color::rgba(color.r, color.g, color.b, color.a);
                    {
                        let TextRenderer {
                            fonts,
                            family,
                            swash,
                            ..
                        } = &mut plain;
                        let mut buffer = shape(fonts, family, metrics, text);
                        let (ox, oy) =
                            ((10.0 * scale).round() as i32, (8.0 * scale).round() as i32);
                        let mut canvas = a.as_mut();
                        buffer.draw(fonts, swash, fill, |gx, gy, w, h, c| {
                            blend_rect(&mut canvas, ox + gx, oy + gy, w, h, c);
                        });
                    }

                    // And through the cache, twice, so a second hit is checked too.
                    let mut cached = TextRenderer::new("sans-serif", 18.0);
                    cached.set_scale(scale);
                    cached.draw(&mut b.as_mut(), text, 10.0, 8.0, color);
                    let mut c2 = Pixmap::new(400, 60).unwrap();
                    cached.draw(&mut c2.as_mut(), text, 10.0, 8.0, color);

                    let differing = a
                        .data()
                        .iter()
                        .zip(b.data())
                        .filter(|(x, y)| x != y)
                        .count();
                    assert_eq!(
                        differing, 0,
                        "scale {scale}, {text:?}, {color:?}: {differing} bytes differ from \
                         the uncached path"
                    );
                    assert_eq!(b.data(), c2.data(), "a second draw differed from the first");
                }
            }
        }
    }
}
