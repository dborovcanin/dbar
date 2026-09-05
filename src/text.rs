//! Text shaping and rasterization on top of cosmic-text.
//!
//! Layout works in logical pixels while glyphs are rasterized at the surface's physical size,
//! so the renderer keeps the output scale here and converts on the way in and out. Measuring
//! and drawing therefore always agree, whatever the scale factor is.

use std::collections::HashMap;

use anyhow::{Result, bail};
use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache};

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
    runs: Generations<Option<TextRun>>,
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
pub struct TextRun {
    /// Where the run's top-left corner sits relative to the text origin, in device pixels.
    pub left: i32,
    pub top: i32,
    pub width: usize,
    pub height: usize,
    pub pixels: RunPixels,
}

/// What a rasterised string is made of.
///
/// Almost all text is an outline, which comes back as coverage and is tinted with whatever
/// colour the module asks for. An emoji carries its own colour and cannot be reduced to
/// coverage without losing it, so it arrives already painted.
pub enum RunPixels {
    /// One byte of coverage per pixel, row-major.
    Coverage(Vec<u8>),
    /// Premultiplied RGBA, row-major.
    Colour(Vec<u8>),
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

/// What a bar keeps for characters its own font does not have, when the config names
/// nothing: one family per job, most likely first.
///
/// A status bar draws whatever the compositor and the running programs hand it - a
/// workspace named with a Nerd Font glyph, a window title in Japanese, a track with an
/// emoji in it - so the font it was configured with is regularly not enough. The rows below
/// are the jobs worth covering; dbar keeps the first family of each row that is installed.
///
/// The set is deliberately short. Keeping a family costs nothing until a glyph sends the
/// shaper looking, but a character *no* font can draw is looked for in every family that
/// is left, and each one tried is a font file read and parsed. On a machine with a wall of
/// programming fonts installed that is the difference between ten files and four hundred.
const FALLBACK_ROLES: &[&[&str]] = &[
    // Powerline and Nerd Font glyphs, which is what workspace names are usually made of.
    // The dedicated symbol font is worth finding first: every patched programming font
    // carries the same glyphs, so without it the search wanders through all of them.
    &["Symbols Nerd Font", "Symbols Nerd Font Mono"],
    // Latin, Greek and Cyrillic beyond whatever the configured face happens to have.
    &["Noto Sans", "DejaVu Sans", "Liberation Sans", "FreeSans"],
    // Arrows, box drawing, dingbats: the punctuation of a window title.
    &["Noto Sans Symbols 2", "Noto Sans Symbols"],
    &[
        "Noto Color Emoji",
        "Noto Emoji",
        "Twemoji",
        "JoyPixels",
        "OpenMoji",
    ],
    // One CJK family covers Chinese, Japanese and Korean; the regional names differ only
    // in which glyph shapes they prefer.
    &[
        "Noto Sans CJK JP",
        "Noto Sans CJK SC",
        "Source Han Sans",
        "WenQuanYi Zen Hei",
    ],
    &["Noto Sans Arabic", "Noto Naskh Arabic"],
    &["Noto Sans Hebrew"],
    &["Noto Sans Thai"],
    &["Noto Sans Devanagari"],
];

/// Names that mean "just give me the default UI font".
fn is_generic(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "sans" | "sans-serif" | "sansserif" | "system-ui" | "ui-sans-serif" | "default"
    )
}

/// Every family name the font database knows, in database order.
fn installed(fonts: &FontSystem) -> Vec<String> {
    fonts
        .db()
        .faces()
        .flat_map(|face| face.families.iter().map(|(name, _)| name.clone()))
        .collect()
}

/// The installed family called `wanted`, whatever case the config spelled it in.
fn find<'a>(available: &'a [String], wanted: &str) -> Option<&'a String> {
    available
        .iter()
        .find(|name| name.eq_ignore_ascii_case(wanted))
}

/// Resolve a configured family name to one the font database actually has.
///
/// cosmic-text will not match generic CSS names such as `sans-serif`, and silently renders
/// nothing when a family is missing, so the name is pinned to a real family up front.
fn resolve_family(fonts: &mut FontSystem, requested: &str, available: &[String]) -> String {
    let find = |wanted: &str| find(available, wanted).cloned();

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

/// The families worth keeping, given what the config asked for.
///
/// A named list is taken as written, because a config that says which fonts to fall back on
/// means it. Naming one that is not installed is a mistake worth stopping for: the bar would
/// otherwise start and quietly draw empty boxes wherever that font was needed.
fn keep_set(available: &[String], primary: &str, configured: &[String]) -> Result<Vec<String>> {
    let mut keep = vec![primary.to_string()];
    if configured.is_empty() {
        keep.extend(
            FALLBACK_ROLES
                .iter()
                .filter_map(|row| row.iter().find_map(|name| find(available, name)).cloned()),
        );
        return Ok(keep);
    }
    for name in configured {
        match find(available, name) {
            Some(found) => keep.push(found.clone()),
            None => bail!("[bar] fallback names the font family {name:?}, which is not installed"),
        }
    }
    Ok(keep)
}

/// Drop every family outside `keep` from the font database.
///
/// cosmic-text answers a character its primary font lacks by trying the fonts it has, one
/// after another, and every font it tries is read off disk and parsed. The database it
/// builds is every font on the machine, so on a system with a few hundred installed, one
/// unlucky character - a Nerd Font glyph in a workspace name is the ordinary case - reads
/// all of them, taking tens of milliseconds and leaving tens of megabytes mapped for as
/// long as the bar runs. Cutting the database to the families that are actually wanted
/// bounds both.
fn prune(fonts: &mut FontSystem, keep: &[String]) {
    let db = fonts.db_mut();
    let doomed: Vec<_> = db
        .faces()
        .filter(|face| {
            !face
                .families
                .iter()
                .any(|(name, _)| keep.iter().any(|wanted| name.eq_ignore_ascii_case(wanted)))
        })
        .map(|face| face.id)
        .collect();
    // Everything matched means there is nothing to gain, and nothing matching would leave
    // no font at all; either way the database is better left as it is.
    if doomed.is_empty() || doomed.len() == db.len() {
        return;
    }
    for id in doomed {
        db.remove_face(id);
    }
}

impl TextRenderer {
    pub fn new(family: &str, size: f32, fallback: &[String]) -> Result<TextRenderer> {
        let mut fonts = FontSystem::new();
        let available = installed(&fonts);
        let family = resolve_family(&mut fonts, family, &available);
        let keep = keep_set(&available, &family, fallback)?;
        prune(&mut fonts, &keep);
        log::info!(
            "using font family {family:?} at {size}px, falling back to {:?} ({} faces kept)",
            &keep[1..],
            fonts.db().len()
        );
        Ok(TextRenderer {
            fonts,
            swash: SwashCache::new(),
            family,
            size,
            scale: 1.0,
            widths: Generations::new(WIDTHS_KEPT),
            runs: Generations::new(SHAPES_KEPT),
            shapes: Generations::new(SHAPES_KEPT),
        })
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

    /// The rasterised form of `text` at the current scale.
    ///
    /// The renderer places and colours it; nothing here knows what it is drawing onto. That
    /// is the whole of the text seam: a backend that keeps glyphs in an atlas uploads these
    /// same bytes instead of blending them.
    pub fn run(&mut self, text: &str) -> Option<&TextRun> {
        if text.is_empty() {
            return None;
        }
        let metrics = self.metrics();
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
        .as_ref()
    }
}

/// Turn a shaped buffer into pixels.
///
/// The buffer is drawn once in white: a glyph rendered from an outline comes back with the
/// colour it was given and its coverage in the alpha, so white in means coverage out. A
/// glyph that answers in some other colour is carrying its own, and the whole run is kept
/// as painted pixels instead.
fn rasterise(
    buffer: &mut Buffer,
    fonts: &mut FontSystem,
    swash: &mut SwashCache,
) -> Option<TextRun> {
    let white = cosmic_text::Color::rgba(0xff, 0xff, 0xff, 0xff);
    let mut patches: Vec<(i32, i32, u32, u32, cosmic_text::Color)> = Vec::new();
    let (mut min_x, mut min_y) = (i32::MAX, i32::MAX);
    let (mut max_x, mut max_y) = (i32::MIN, i32::MIN);
    let mut coloured = false;

    buffer.draw(fonts, swash, white, |x, y, w, h, c| {
        if c.a() == 0 || w == 0 || h == 0 {
            return;
        }
        coloured |= (c.r(), c.g(), c.b()) != (0xff, 0xff, 0xff);
        patches.push((x, y, w, h, c));
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x + w as i32);
        max_y = max_y.max(y + h as i32);
    });

    if patches.is_empty() {
        return None;
    }
    let (width, height) = ((max_x - min_x) as usize, (max_y - min_y) as usize);

    // Source-over of one patch onto what is already there. Whole runs are built this way
    // because two glyphs can touch the same pixel.
    let over = |under: u32, src: u32, alpha: u32| under + (src * (255 - alpha) + 127) / 255;

    if !coloured {
        let mut coverage = vec![0u8; width * height];
        for (x, y, w, h, c) in patches {
            for row in 0..h as usize {
                let line = ((y - min_y) as usize + row) * width + (x - min_x) as usize;
                for slot in &mut coverage[line..line + w as usize] {
                    *slot = over(u32::from(c.a()), u32::from(*slot), u32::from(c.a())) as u8;
                }
            }
        }
        return Some(TextRun {
            left: min_x,
            top: min_y,
            width,
            height,
            pixels: RunPixels::Coverage(coverage),
        });
    }

    // Something in the run carries its own colour, so the whole run is kept painted.
    let mut rgba = vec![0u8; width * height * 4];
    for (x, y, w, h, c) in patches {
        let a = u32::from(c.a());
        let up = |v: u8| (u32::from(v) * a + 127) / 255;
        let (sr, sg, sb) = (up(c.r()), up(c.g()), up(c.b()));
        for row in 0..h as usize {
            let line = (((y - min_y) as usize + row) * width + (x - min_x) as usize) * 4;
            for px in rgba[line..line + w as usize * 4].chunks_exact_mut(4) {
                px[0] = over(sr, u32::from(px[0]), a) as u8;
                px[1] = over(sg, u32::from(px[1]), a) as u8;
                px[2] = over(sb, u32::from(px[2]), a) as u8;
                px[3] = over(a, u32::from(px[3]), a) as u8;
            }
        }
    }
    Some(TextRun {
        left: min_x,
        top: min_y,
        width,
        height,
        pixels: RunPixels::Colour(rgba),
    })
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

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_bar_that_names_no_fallback_gets_one_family_per_job() {
        let available = names(&[
            "Noto Sans",
            "Symbols Nerd Font",
            "Noto Sans Symbols 2",
            "Noto Color Emoji",
            "Noto Sans CJK JP",
            // Neither of these has a job in the table, so neither is kept.
            "JetBrainsMono Nerd Font",
            "Comic Neue",
        ]);
        let keep = keep_set(&available, "Noto Sans", &[]).expect("nothing was named");
        assert_eq!(keep[0], "Noto Sans", "the bar's own font comes first");
        assert!(keep.contains(&"Symbols Nerd Font".to_string()));
        assert!(keep.contains(&"Noto Color Emoji".to_string()));
        assert!(keep.contains(&"Noto Sans CJK JP".to_string()));
        assert!(
            !keep.contains(&"Comic Neue".to_string()),
            "a family with no job to do is not worth the search it lengthens"
        );
    }

    #[test]
    fn a_job_nothing_installed_can_do_is_left_undone() {
        // No emoji font and no CJK font: those rows contribute nothing rather than
        // holding a name the database does not have.
        let available = names(&["DejaVu Sans", "Noto Sans Symbols"]);
        let keep = keep_set(&available, "DejaVu Sans", &[]).expect("nothing was named");
        for name in &keep {
            assert!(
                available.iter().any(|have| have == name),
                "{name:?} is not installed"
            );
        }
        assert!(keep.contains(&"Noto Sans Symbols".to_string()), "{keep:?}");
    }

    #[test]
    fn the_second_choice_is_taken_when_the_first_is_not_there() {
        let available = names(&["Noto Sans", "Symbols Nerd Font Mono", "Source Han Sans"]);
        let keep = keep_set(&available, "Noto Sans", &[]).expect("nothing was named");
        assert!(
            keep.contains(&"Symbols Nerd Font Mono".to_string()),
            "{keep:?}"
        );
        assert!(keep.contains(&"Source Han Sans".to_string()), "{keep:?}");
    }

    #[test]
    fn a_named_list_is_taken_as_written_rather_than_added_to() {
        let available = names(&["Noto Sans", "Noto Color Emoji", "Symbols Nerd Font"]);
        let keep = keep_set(&available, "Noto Sans", &names(&["Symbols Nerd Font"]))
            .expect("both families are installed");
        assert_eq!(keep, names(&["Noto Sans", "Symbols Nerd Font"]));
        assert!(
            !keep.contains(&"Noto Color Emoji".to_string()),
            "a config that lists its fallbacks says which fonts the bar may load"
        );
    }

    #[test]
    fn a_family_is_matched_however_the_config_spelled_it() {
        let available = names(&["Noto Sans", "Symbols Nerd Font"]);
        let keep = keep_set(&available, "Noto Sans", &names(&["symbols nerd font"]))
            .expect("case is not part of a font's name");
        assert_eq!(
            keep[1], "Symbols Nerd Font",
            "the installed spelling is kept"
        );
    }

    #[test]
    fn a_fallback_that_is_not_installed_is_a_startup_error() {
        let available = names(&["Noto Sans"]);
        let e = keep_set(&available, "Noto Sans", &names(&["Nonesuch Sans"]))
            .expect_err("a font that is not there cannot be fallen back on");
        let message = format!("{e:#}");
        assert!(message.contains("Nonesuch Sans"), "{message}");
        assert!(message.contains("fallback"), "{message}");
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
