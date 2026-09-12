//! Vector drawing of a laid-out frame.
//!
//! Everything here works in logical pixels; the scale factor is applied through a single
//! transform so HiDPI output stays sharp without the layout code knowing about it.

use anyhow::{Context as _, Result};
use tiny_skia::{
    FillRule, LineCap, LineJoin, Mask, Paint, Path, PathBuilder, Pixmap, PixmapMut, PixmapRef,
    PremultipliedColorU8, Rect, Stroke, Transform,
};

use crate::color::Color;
use crate::geometry::{Direction, EdgeShape, SeparatorShape};
use crate::icon::{self, IconArt, Ink, PathCmd};
use std::collections::HashMap;

use crate::layout::{Frame, MenuFrame, PlacedGroup, PlacedIcon, PlacedModule, PlacedSeparator};
use crate::text::{RunPixels, TextRenderer, TextRun};

/// What the renderer needs from a text backend.
///
/// Drawing sits behind this the way measuring sits behind `layout::Measure`, so `render.rs`
/// can be tested without fonts and so a future backend can bring its own rasteriser. The
/// coordinates are logical pixels; an implementation applies the output scale itself.
pub trait DrawText {
    /// Height of one line, in logical pixels.
    fn line_height(&self) -> f32;

    /// How far below the middle of a module the baseline goes, in logical pixels.
    ///
    /// What every wording is placed by. The default suits a backend whose glyphs stand on
    /// the bottom of their line box and fill it; a backend drawing a real font says where
    /// its own letters are instead - see `TextRenderer::baseline_offset`.
    fn baseline_offset(&mut self) -> f32 {
        self.line_height() / 2.0
    }

    /// The rasterised form of `text` at the output scale, or nothing to draw.
    ///
    /// The backend places and colours what comes back. Nothing behind this trait knows what
    /// it is drawing onto, which is what lets the rasteriser be replaced: a GPU backend
    /// uploads these bytes to an atlas where this one blends them.
    fn run(&mut self, text: &str) -> Option<&TextRun>;
}

impl DrawText for TextRenderer {
    fn line_height(&self) -> f32 {
        TextRenderer::line_height(self)
    }

    fn baseline_offset(&mut self) -> f32 {
        TextRenderer::baseline_offset(self)
    }

    fn run(&mut self, text: &str) -> Option<&TextRun> {
        TextRenderer::run(self, text)
    }
}

/// Control-point ratio that turns a cubic into a quarter circle.
const KAPPA: f32 = 0.552_285;

fn skia_color(c: Color) -> tiny_skia::Color {
    tiny_skia::Color::from_rgba8(c.r, c.g, c.b, c.a)
}

/// Width of the hairline drawn for `shape = "line"`, in logical pixels.
const LINE_WIDTH: f32 = 1.0;

/// A rectangle whose left and right corners may round by different amounts.
///
/// Returns `None` for degenerate sizes.
pub fn edged_rect(x: f32, y: f32, w: f32, h: f32, left: f32, right: f32) -> Option<Path> {
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let limit = (w / 2.0).min(h / 2.0);
    let rl = left.clamp(0.0, limit);
    let rr = right.clamp(0.0, limit);

    let mut pb = PathBuilder::new();
    if rl <= 0.0 && rr <= 0.0 {
        pb.push_rect(Rect::from_xywh(x, y, w, h)?);
        return pb.finish();
    }

    let (cl, cr) = (rl * KAPPA, rr * KAPPA);
    let (x0, y0, x1, y1) = (x, y, x + w, y + h);

    pb.move_to(x0 + rl, y0);
    pb.line_to(x1 - rr, y0);
    pb.cubic_to(x1 - rr + cr, y0, x1, y0 + rr - cr, x1, y0 + rr);
    pb.line_to(x1, y1 - rr);
    pb.cubic_to(x1, y1 - rr + cr, x1 - rr + cr, y1, x1 - rr, y1);
    pb.line_to(x0 + rl, y1);
    pb.cubic_to(x0 + rl - cl, y1, x0, y1 - rl + cl, x0, y1 - rl);
    pb.line_to(x0, y0 + rl);
    pb.cubic_to(x0, y0 + rl - cl, x0 + rl - cl, y0, x0 + rl, y0);
    pb.close();
    pb.finish()
}

/// The leading-side region of a separator, drawn right-pointing over the gap rect.
///
/// The boundary between the two neighbouring modules is the far edge of this path; the
/// remaining area of the gap belongs to the module the separator leads into.
fn separator_path(
    shape: SeparatorShape,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    scale: f32,
) -> Option<Path> {
    let w = x1 - x0;
    let h = y1 - y0;
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let ymid = match shape {
        // A corner is only a point if one row of pixels owns it. Halfway down an even
        // number of them is a boundary, and the two rows either side of it come out with
        // the same coverage, so the tip rasterises as a flat two pixels tall. The curved
        // shapes are meant to be blunt there and keep their true middle.
        SeparatorShape::Chevron | SeparatorShape::Notch => centre(y0, y1, scale),
        _ => (y0 + y1) / 2.0,
    };
    let mut pb = PathBuilder::new();

    match shape {
        SeparatorShape::None => return None,
        SeparatorShape::Line => {
            let xc = (x0 + x1) / 2.0;
            pb.push_rect(Rect::from_xywh(xc - LINE_WIDTH / 2.0, y0, LINE_WIDTH, h)?);
        }
        SeparatorShape::Slant => {
            pb.move_to(x0, y0);
            pb.line_to(x1, y0);
            pb.line_to(x0, y1);
            pb.close();
        }
        SeparatorShape::Chevron => {
            pb.move_to(x0, y0);
            pb.line_to(x1, ymid);
            pb.line_to(x0, y1);
            pb.close();
        }
        SeparatorShape::Notch => {
            // The gap, with a V bitten out of its trailing edge.
            let xmid = (x0 + x1) / 2.0;
            pb.move_to(x0, y0);
            pb.line_to(x1, y0);
            pb.line_to(xmid, ymid);
            pb.line_to(x1, y1);
            pb.line_to(x0, y1);
            pb.close();
        }
        SeparatorShape::Round => {
            // Two quarter-ellipses meeting at the midpoint of the trailing edge.
            let (cx, cy) = (w * KAPPA, h / 2.0 * KAPPA);
            pb.move_to(x0, y0);
            pb.cubic_to(x0 + cx, y0, x1, ymid - cy, x1, ymid);
            pb.cubic_to(x1, ymid + cy, x0 + cx, y1, x0, y1);
            pb.close();
        }
        SeparatorShape::Curve => {
            // A sigmoid boundary: horizontal tangents at both ends.
            let cx = w / 2.0;
            pb.move_to(x0, y0);
            pb.line_to(x1, y0);
            pb.cubic_to(x1 - cx, y0, x0 + cx, y1, x0, y1);
            pb.close();
        }
    }
    pb.finish()
}

/// Put a logical-pixel edge on a whole device pixel.
///
/// Two fills that meet on a shared edge are rasterised independently, so at a fractional
/// position each takes part of that pixel's coverage. Opaque fills survive it - the two
/// partial covers still add up to the pixel - but translucent ones are composited over the
/// wallpaper separately, and the pixel ends up lighter than either of them. Snapping the
/// edge both sides were laid out against gives each of them whole pixels to cover.
fn snap(v: f32, scale: f32) -> f32 {
    if scale <= 0.0 {
        return v;
    }
    (v * scale).round() / scale
}

/// The middle of `y0..y1`, moved onto the middle of the device pixel row holding it.
///
/// A vertex on a pixel boundary is shared by the rows above and below it, and both of them
/// end up with the coverage the tip should have had to itself. Half a device pixel is
/// nothing on a slope that runs the height of the bar, and it is the whole difference
/// between a point and a flat.
fn centre(y0: f32, y1: f32, scale: f32) -> f32 {
    let mid = (y0 + y1) / 2.0;
    if scale <= 0.0 {
        return mid;
    }
    ((mid * scale).floor() + 0.5) / scale
}

fn draw_separator(
    pixmap: &mut PixmapMut<'_>,
    sep: &PlacedSeparator,
    scale: f32,
    transform: Transform,
    clip: Option<&Mask>,
) {
    if sep.shape.is_none() {
        return;
    }
    // Bleed past both sides so neither antialiased edge leaves a hairline of wallpaper.
    let x0 = snap(sep.x - sep.overlap, scale);
    let x1 = snap(sep.x + sep.width + sep.overlap, scale);
    let (y0, y1) = (sep.y, sep.y + sep.height);

    // A left-pointing separator is the mirror image of a right-pointing one with its two
    // colours exchanged, which avoids building the concave complement of every shape.
    // A hairline is symmetric, so it is never mirrored.
    let mirrored = sep.direction == Direction::Left && sep.shape != SeparatorShape::Line;
    let (under, over) = if mirrored {
        (sep.fill, sep.under)
    } else {
        (sep.under, sep.fill)
    };

    // A filled shape splits the gap between the two module colours. A hairline only
    // divides, so the gap keeps the group background and the line stays centred in it.
    //
    // The ground goes down across the whole gap and the shape over the top of it, rather
    // than the shape being cut out of it. Two fills meeting on a shared antialiased edge
    // each take part of that edge's pixels, and two partial covers never add back up to
    // one, so cutting the shape out would leave a seam of wallpaper along every boundary.
    // The cost is that the two colours are composited where they overlap, which is why a
    // filled separator wants opaque module colours: see `fill_edged`'s callers.
    if sep.shape != SeparatorShape::Line {
        fill(
            pixmap,
            (x0, y0, x1 - x0, y1 - y0),
            0.0,
            under,
            transform,
            clip,
        );
    }

    // The ground may bleed past the gap because it is a rectangle and the modules either
    // side of it are drawn afterwards over the top, which is all the bleed is for. The
    // shape may not: its ends are boundaries those modules are drawn to, and a shape built
    // wider than the gap puts them in the wrong place. Its point lands inside the module
    // beyond it and is painted flat off at that module's edge; its base falls short of the
    // module behind it, which fills the column the shape's own edge was fading across and
    // leaves the transition standing straight where it should have started to lean. So the
    // shape spans the gap itself, snapped to the same grid the fills are, and every edge of
    // it meets the fill it is continued by. A cap is the same shape drawn where an island
    // ends rather than between two of its modules, and is placed and cut the same way.
    let (sx0, sx1) = (snap(sep.x, scale), snap(sep.x + sep.width, scale));
    let Some(path) = separator_path(sep.shape, sx0, y0, sx1, y1, scale) else {
        return;
    };
    // Outer caps occupy the side of the boundary adjacent to their module. Build
    // a single even-odd path for the complement, preserving transparent bar backgrounds.
    let path = if sep.inverted {
        let mut builder = PathBuilder::new();
        let Some(rect) = Rect::from_xywh(sx0, y0, sx1 - sx0, y1 - y0) else {
            return;
        };
        builder.push_rect(rect);
        builder.push_path(&path);
        let Some(complement) = builder.finish() else {
            return;
        };
        complement
    } else {
        path
    };
    let path = if mirrored {
        // Reflect about the shape's vertical centre line.
        match path.transform(Transform::from_row(-1.0, 0.0, 0.0, 1.0, sx0 + sx1, 0.0)) {
            Some(p) => p,
            None => return,
        }
    } else {
        path
    };

    if sep.inverted {
        let mut paint = Paint::default();
        paint.set_color(skia_color(over));
        paint.anti_alias = true;
        pixmap.fill_path(&path, &paint, FillRule::EvenOdd, transform, clip);
    } else {
        fill_path(pixmap, &path, over, transform, clip);
    }
}

/// Draw one icon, tinted with the module's foreground.
///
/// Icon geometry is authored in a unit square, so a single transform puts it at its placed
/// position and size. Stroke widths ride along with that scale.
/// An icon already rasterised, as coverage per pixel.
///
/// Icons are vector art, and redrawing the same twelve shapes every frame was more than
/// half the cost of one. The coverage depends on the size and on where the icon lands
/// within a pixel, but not on its colour, so one of these serves an icon whatever state
/// its module is in.
struct IconRun {
    width: usize,
    height: usize,
    coverage: Vec<u8>,
}

/// What makes one rasterisation different from another.
///
/// The position is part of it, to the bit: an icon half a pixel further along is a
/// different picture, and rounding it to a whole one would move the art. Positions repeat
/// exactly between frames while nothing around them changes, which is when the cache is
/// wanted, and a module whose text has just grown a digit simply rasterises again.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct IconKey {
    icon: icon::Icon,
    level: usize,
    size: u32,
    offset: (u32, u32),
}

/// How many rasterised icons to keep. A bar draws a dozen or so, and the rest is headroom
/// for the positions that keep arriving: a digit's worth of text growing beside an icon
/// moves it, and so does a fold, which lands every icon in an island on a new fraction of
/// a pixel on every frame of its travel.
const ICONS_KEPT: usize = 96;

/// Rasterised icons, kept between frames and bounded by dropping the one least recently
/// drawn.
///
/// Emptying it instead would be cheaper to write and wrong in the one case that matters:
/// a folding island lands its icons on a different fraction of a pixel every frame, so a
/// long fold is a few hundred keys nothing will ask for again, and a bar that cleared on
/// each of them would pay to rasterise every settled icon it has two or three times per
/// fold. Least-recently-used drops the frames that have gone by and keeps the bar.
struct IconCache {
    /// Each run beside the stamp it was last drawn at.
    runs: HashMap<IconKey, (IconRun, u64)>,
    /// Ticks once per lookup, which is what "least recently" is measured in. A bar drawing
    /// a hundred icons a frame at sixty frames a second takes about a hundred million
    /// years to run out of these.
    used: u64,
}

impl IconCache {
    fn new() -> IconCache {
        IconCache {
            runs: HashMap::new(),
            used: 0,
        }
    }

    /// The run for `key`, rasterising it with `draw` if it is not here yet.
    ///
    /// The whole protocol in one place, so a caller asks for an icon rather than asking
    /// whether one is here, making it if it is not, and asking again. It is the same two
    /// probes on a hit either way - a hit has to be found before it can be stamped, and
    /// `entry` cannot be held across the eviction a miss might need - but only one of them
    /// is anybody else's business.
    fn run(&mut self, key: IconKey, draw: impl FnOnce() -> Option<IconRun>) -> Option<&IconRun> {
        self.used += 1;
        let used = self.used;
        if !self.runs.contains_key(&key) {
            // Room first: the oldest goes to make space for this one, and evicting after
            // inserting could pick the one just made.
            self.evict_if_full();
            self.runs.insert(key, (draw()?, used));
        }
        let (run, stamp) = self.runs.get_mut(&key)?;
        *stamp = used;
        Some(run)
    }

    /// Make room for one more by dropping the run nothing has asked for in longest.
    ///
    /// The search is over the whole map, which is a hundred keys, and only happens on a
    /// miss with the cache already full - never on the hit the cache exists for.
    fn evict_if_full(&mut self) {
        if self.runs.len() < ICONS_KEPT {
            return;
        }
        if let Some(oldest) = (self.runs.iter())
            .min_by_key(|(_, (_, stamp))| *stamp)
            .map(|(key, _)| *key)
        {
            self.runs.remove(&oldest);
        }
    }
}

/// What drawing an island needs besides the frame: the text backend and the two caches.
///
/// Bundled because they travel together and are borrowed together, and because a target is
/// either the surface or a layer, which decides which mask comes along.
struct Tools<'a> {
    mask: &'a mut Option<Mask>,
    icons: &'a mut IconCache,
    text: &'a mut dyn DrawText,
}

/// Draw an icon through the cache, rasterising it the first time it is seen at this size
/// and position.
fn draw_icon_cached(
    pixmap: &mut PixmapMut<'_>,
    placed: &PlacedIcon,
    color: Color,
    transform: Transform,
    cut: Cut<'_>,
    cache: &mut IconCache,
) {
    if color.is_transparent() || placed.size <= 0.0 {
        return;
    }
    // A picture is already rasterised, and the cache is keyed by which built-in icon it is
    // - which every one of these shares. Caching them would draw one tray item's artwork
    // for all of them.
    if placed.art.is_some() {
        draw_icon(pixmap, placed, color, transform, cut.mask);
        return;
    }

    let (dx, dy) = (
        placed.x * transform.sx + transform.tx,
        placed.y * transform.sy + transform.ty,
    );
    let size = placed.size * transform.sx;
    let (ox, oy) = (dx.floor(), dy.floor());
    let (fx, fy) = (dx - ox, dy - oy);
    if size <= 0.0 || !size.is_finite() || !ox.is_finite() || !oy.is_finite() {
        return;
    }

    let key = IconKey {
        icon: placed.icon,
        level: placed.level,
        size: size.to_bits(),
        offset: (fx.to_bits(), fy.to_bits()),
    };
    let Some(run) = cache.run(key, || {
        rasterise_icon(placed.icon, placed.level, size, fx, fy)
    }) else {
        return;
    };
    blend_coverage(
        pixmap,
        Blit {
            pixels: &run.coverage,
            width: run.width,
            height: run.height,
            x: ox as i32,
            y: oy as i32,
        },
        color,
        true,
        cut,
    );
}

/// Where a group's contents stop, for the two things tiny-skia's clip cannot catch.
///
/// `mask` is the island's outline, which is what takes the corners and, while a fold is
/// travelling, the shape of the edge it is cutting with. `stop` is the device column past
/// which nothing is written at all, which is what holds where the outline's coverage does
/// not reach. `bounds` is the box of the mask that was cleared for this group:
/// [`clip_mask`] leaves the rest of the buffer holding the last group's coverage, so
/// anything reaching outside that box is treated as uncovered rather than read.
///
/// `shift` reads the mask that many device columns further along, which is how the wording
/// stops short of the fills without a second mask: the edge it wants is the edge the fills
/// have, moved in. A column would have done it before there was a shape to move.
#[derive(Clone, Copy, Default)]
struct Cut<'a> {
    mask: Option<&'a Mask>,
    bounds: (u32, u32, u32, u32),
    stop: Option<i32>,
    shift: i32,
}

impl Cut<'_> {
    /// The same cut, stopped at `col` as well as wherever it already stopped.
    fn stopped(self, col: i32) -> Self {
        Cut {
            stop: Some(self.stop.map_or(col, |stop| stop.min(col))),
            ..self
        }
    }

    /// `[x0, x1) x [y0, y1)` narrowed to what this cut can answer for.
    ///
    /// Everything the mask was built for is inside its cleared box, so a run reaching past
    /// it is a glyph's overhang outside the island - which the outline would have taken
    /// anyway, had the mask been rasterised that far.
    fn within(&self, x0: i32, y0: i32, x1: i32, y1: i32) -> (i32, i32, i32, i32) {
        let x1 = x1.min(self.stop.unwrap_or(i32::MAX));
        if self.mask.is_none() {
            return (x0, y0, x1, y1);
        }
        let (bx, by, bw, bh) = self.bounds;
        (
            x0.max(bx as i32),
            y0.max(by as i32),
            x1.min((bx + bw) as i32 - self.shift),
            y1.min((by + bh) as i32),
        )
    }
}

/// A rasterised run of pixels and the device position its top left corner goes at.
#[derive(Clone, Copy)]
struct Blit<'a> {
    pixels: &'a [u8],
    width: usize,
    height: usize,
    x: i32,
    y: i32,
}

/// Put a string on the pixmap with its text origin at `x` and its baseline set by `y`.
///
/// The backend does the shaping and colouring: the text side hands back the trimmed ink,
/// where it sits relative to the origin and where the letters stand within it, and knows
/// nothing about what it lands on. `y` is the middle of the module; what is put there is
/// the baseline the backend asks for, not the ink box, so a wording does not move because
/// it gained a descender or because a fallback face brought taller metrics to the line.
///
/// Glyphs are the one thing tiny-skia's clip cannot catch, because the backend rasterises
/// and places them itself rather than filling a path. An island that cuts its contents off
/// at its own edge therefore has to catch them here, which is what `cut` carries.
fn draw_text(
    pixmap: &mut PixmapMut<'_>,
    text: &mut dyn DrawText,
    what: &str,
    at: (f32, f32),
    scale: f32,
    color: Color,
    cut: Cut<'_>,
) {
    if color.is_transparent() {
        return;
    }
    let ox = (at.0 * scale).round() as i32;
    let oy = ((at.1 + text.baseline_offset()) * scale).round() as i32;
    let Some(run) = text.run(what) else {
        return;
    };
    let (rw, rh) = (run.width, run.height);
    let (rx, ry) = (ox + run.left, oy - run.baseline);
    let blit = |pixels| Blit {
        pixels,
        width: rw,
        height: rh,
        x: rx,
        y: ry,
    };
    match &run.pixels {
        // cosmic-text builds a mask glyph's colour as the coverage over the base's rgb and
        // drops the base's alpha, so an alpha on a text colour has never reached the screen
        // and is not honoured here either.
        RunPixels::Coverage(coverage) => blend_coverage(pixmap, blit(coverage), color, false, cut),
        // The text tinted first and what carries its own colour laid over it, which is
        // how `☀ Clear` keeps the sun's colours and the module's own wording.
        RunPixels::Mixed { coverage, rgba } => {
            blend_coverage(pixmap, blit(coverage), color, false, cut);
            blend_rgba(pixmap, blit(rgba), cut);
        }
    }
}

/// Blend premultiplied RGBA into the pixmap, clipped to the surface.
///
/// Only emoji arrive this way: they carry their own colour, so there is nothing to tint.
fn blend_rgba(pixmap: &mut PixmapMut<'_>, blit: Blit<'_>, cut: Cut<'_>) {
    let (pw, ph) = (pixmap.width() as i32, pixmap.height() as i32);
    let (ox, oy) = (blit.x, blit.y);
    let (x0, y0, x1, y1) = cut.within(
        ox.max(0),
        oy.max(0),
        (ox + blit.width as i32).min(pw),
        (oy + blit.height as i32).min(ph),
    );
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let mask = cut.mask.map(|m| m.data());
    let pixels = pixmap.pixels_mut();
    let span = (x1 - x0) as usize;
    for py in y0..y1 {
        let src = ((py - oy) as usize * blit.width + (x0 - ox) as usize) * 4;
        let dst = py as usize * pw as usize + x0 as usize;
        // Read off the row rather than the whole buffer, so the per-pixel work below is
        // an index into a slice instead of a branch on whether there is a mask at all.
        let read = dst + cut.shift as usize;
        let row = mask.map(|mask| &mask[read..read + span]);
        for i in 0..span {
            let px = &blit.pixels[src + i * 4..src + i * 4 + 4];
            // Premultiplied, so the mask scales all four channels together or the colour
            // comes out brighter than its own alpha allows.
            let cover = row.map_or(255, |row| u32::from(row[i]));
            let (r, g, b, a) = match cover {
                255 => (px[0], px[1], px[2], px[3]),
                0 => continue,
                _ => {
                    let faded = |v: u8| ((u32::from(v) * cover + 127) / 255) as u8;
                    (faded(px[0]), faded(px[1]), faded(px[2]), faded(px[3]))
                }
            };
            if a == 0 {
                continue;
            }
            let inv = 255 - u32::from(a);
            let slot = &mut pixels[dst + i];
            let under = *slot;
            let over = |s: u8, d: u8| u32::from(s) + (u32::from(d) * inv + 127) / 255;
            let na = over(a, under.alpha());
            let nr = over(r, under.red()).min(na);
            let ng = over(g, under.green()).min(na);
            let nb = over(b, under.blue()).min(na);
            *slot = PremultipliedColorU8::from_rgba(nr as u8, ng as u8, nb as u8, na as u8)
                .unwrap_or(under);
        }
    }
}

/// Blend a coverage buffer into the pixmap in `color`, clipped to the surface.
///
/// `honour_alpha` scales the coverage by the colour's own alpha. Icons want that - the
/// colour they are given is the colour they are drawn in - which is the one way they differ
/// from text, where the mask path drops it.
fn blend_coverage(
    pixmap: &mut PixmapMut<'_>,
    blit: Blit<'_>,
    color: Color,
    honour_alpha: bool,
    cut: Cut<'_>,
) {
    let (pw, ph) = (pixmap.width() as i32, pixmap.height() as i32);
    let (ox, oy) = (blit.x, blit.y);
    let (x0, y0, x1, y1) = cut.within(
        ox.max(0),
        oy.max(0),
        (ox + blit.width as i32).min(pw),
        (oy + blit.height as i32).min(ph),
    );
    if x0 >= x1 || y0 >= y1 {
        return;
    }

    let alpha = match honour_alpha {
        true => color.a as u32,
        false => 255,
    };
    let mask = cut.mask.map(|m| m.data());
    let pixels = pixmap.pixels_mut();
    let span = (x1 - x0) as usize;
    for py in y0..y1 {
        let src = (py - oy) as usize * blit.width + (x0 - ox) as usize;
        let dst = py as usize * pw as usize + x0 as usize;
        // The island's outline, where one was asked for: a straight column can stop text
        // at an edge but not at a corner, and a folding island's content runs right into
        // its rounded ones. Taken a row at a time so the per-pixel work is an index.
        let read = dst + cut.shift as usize;
        let row = mask.map(|mask| &mask[read..read + span]);
        for i in 0..span {
            let cover = blit.pixels[src + i] as u32;
            if cover == 0 {
                continue;
            }
            let cover = match row {
                Some(row) => (cover * u32::from(row[i]) + 127) / 255,
                None => cover,
            };
            if cover == 0 {
                continue;
            }
            let src_a = (cover * alpha + 127) / 255;
            if src_a == 0 {
                continue;
            }
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

/// Turn an icon's outline into the rasteriser's own path type.
///
/// This is the whole of what the CPU backend has to do with the icon library's geometry,
/// and it happens on a cache miss rather than per frame.
fn path_of(cmds: &[PathCmd]) -> Option<Path> {
    let mut pb = PathBuilder::new();
    for cmd in cmds {
        match *cmd {
            PathCmd::MoveTo(p) => pb.move_to(p.x, p.y),
            PathCmd::LineTo(p) => pb.line_to(p.x, p.y),
            PathCmd::CubicTo(a, b, c) => pb.cubic_to(a.x, a.y, b.x, b.y, c.x, c.y),
            PathCmd::Close => pb.close(),
        }
    }
    pb.finish()
}

/// Lay an icon that is already pixels into the box layout gave it.
///
/// The picture is resampled to the box rather than assumed to fit it: what size it arrived
/// at is the application's business, and the surface it lands on may be at any scale.
fn draw_raster(
    pixmap: &mut PixmapMut<'_>,
    placed: &PlacedIcon,
    art: &icon::Raster,
    transform: Transform,
    clip: Option<&Mask>,
) {
    if art.width == 0 || art.height == 0 {
        return;
    }
    let Some(source) = tiny_skia::PixmapRef::from_bytes(&art.pixels, art.width, art.height) else {
        return;
    };
    // Square, like every other icon: the box is `size` on a side, and the picture is put
    // into it whatever shape it arrived in.
    let scale = (
        placed.size / art.width as f32,
        placed.size / art.height as f32,
    );
    let local = Transform::from_translate(placed.x, placed.y).pre_scale(scale.0, scale.1);
    let paint = tiny_skia::PixmapPaint {
        quality: tiny_skia::FilterQuality::Bilinear,
        ..tiny_skia::PixmapPaint::default()
    };
    pixmap.draw_pixmap(0, 0, source, &paint, transform.pre_concat(local), clip);
}

/// Draw an icon on its own, at one size and offset within a pixel, and keep the coverage.
fn rasterise_icon(what: icon::Icon, level: usize, size: f32, fx: f32, fy: f32) -> Option<IconRun> {
    let width = (size * what.width() + fx).ceil() as usize + 1;
    let height = (size + fy).ceil() as usize + 1;
    let mut pixmap = Pixmap::new(width as u32, height as u32)?;
    let placed = PlacedIcon {
        icon: what,
        level,
        x: fx,
        y: fy,
        size,
        art: None,
    };
    draw_icon(
        &mut pixmap.as_mut(),
        &placed,
        Color::rgba(0xff, 0xff, 0xff, 0xff),
        Transform::identity(),
        None,
    );
    Some(IconRun {
        width,
        height,
        coverage: pixmap.pixels().iter().map(|p| p.alpha()).collect(),
    })
}

fn draw_icon(
    pixmap: &mut PixmapMut<'_>,
    placed: &PlacedIcon,
    color: Color,
    transform: Transform,
    clip: Option<&Mask>,
) {
    if color.is_transparent() || placed.size <= 0.0 {
        return;
    }
    // An icon that is a picture is drawn as one: it carries its own colours, so the
    // module's foreground says nothing about it beyond whether it is drawn at all.
    if let Some(art) = &placed.art {
        draw_raster(pixmap, placed, art, transform, clip);
        return;
    }
    let IconArt::Paths(paths) = icon::art(placed.icon, placed.level) else {
        return;
    };

    let local = Transform::from_translate(placed.x, placed.y).pre_scale(placed.size, placed.size);
    let ts = transform.pre_concat(local);

    let mut paint = Paint::default();
    paint.set_color(skia_color(color));
    paint.anti_alias = true;

    for item in &paths {
        let Some(path) = path_of(&item.cmds) else {
            continue;
        };
        match item.ink {
            Ink::Fill => {
                pixmap.fill_path(&path, &paint, FillRule::Winding, ts, clip);
            }
            Ink::FillEvenOdd => {
                pixmap.fill_path(&path, &paint, FillRule::EvenOdd, ts, clip);
            }
            Ink::Stroke(width) => {
                let stroke = Stroke {
                    width,
                    line_cap: LineCap::Round,
                    ..Stroke::default()
                };
                pixmap.stroke_path(&path, &paint, &stroke, ts, clip);
            }
        }
    }
}

/// The island's own side of the transition drawn where its contents stop.
///
/// A fold cuts glyphs and icons rather than covering them, so the cut has to be the edge
/// the island is actually drawn with. [`draw_separator`] builds one shape and one ground
/// and lets the colours say which of them is the island; mirroring and inversion are the
/// two turns it takes to get there, and this reads them back off the separator rather than
/// working them out a second time. `None` for anything with no side to speak of - a
/// hairline, or a shape too narrow to rasterise - which leaves the caller its column.
fn edge_region(edge: &PlacedSeparator, right: f32, scale: f32) -> Option<(Path, FillRule)> {
    if edge.shape == SeparatorShape::Line || edge.shape.is_none() {
        return None;
    }
    // The contents stop on the same snapped column the fills do, and the edge is measured
    // from there: its own overlap bleeds back over ground the island has already covered.
    let x0 = snap(right, scale);
    let x1 = snap(edge.x + edge.width, scale);
    let (y0, y1) = (edge.y, edge.y + edge.height);
    let shape = separator_path(edge.shape, x0, y0, x1, y1, scale)?;
    let mirrored = edge.direction == Direction::Left;
    let shape = match mirrored {
        true => shape.transform(Transform::from_row(-1.0, 0.0, 0.0, 1.0, x0 + x1, 0.0))?,
        false => shape,
    };
    // The shape is drawn in the island's colour unless it was mirrored, and `inverted`
    // swaps which side of it is drawn. Two swaps put the island back on the shape.
    if mirrored == edge.inverted {
        return Some((shape, FillRule::Winding));
    }
    let mut builder = PathBuilder::new();
    builder.push_rect(Rect::from_xywh(x0, y0, x1 - x0, y1 - y0)?);
    builder.push_path(&shape);
    Some((builder.finish()?, FillRule::EvenOdd))
}

/// A coverage mask of `path`, used to clip a group's contents to its outline.
///
/// `slot` carries the buffer between frames. tiny-skia requires a mask to be exactly the
/// size of what it clips, so a slot serves one target - the surface or the layer - and is
/// rebuilt only when that target changes size. Allocating one per group per redraw is what
/// this exists to avoid: at two hundred kilobytes a group it was the single most expensive
/// thing in a frame.
///
/// Only `bounds` is cleared before the outline goes down. The rest of the mask keeps the
/// last group's coverage, which is harmless because nothing is drawn through the mask
/// outside the group it was built for.
fn clip_mask<'a>(
    slot: &'a mut Option<Mask>,
    size: (u32, u32),
    bounds: (u32, u32, u32, u32),
    path: &Path,
    rule: FillRule,
    inside: Option<&Path>,
    transform: Transform,
) -> Option<&'a Mask> {
    let (width, height) = size;
    if !matches!(&slot, Some(m) if m.width() == width && m.height() == height) {
        *slot = Mask::new(width, height);
    }
    let mask = slot.as_mut()?;

    let (bx, by, bw, bh) = bounds;
    let stride = width as usize;
    let data = mask.data_mut();
    for row in by as usize..(by + bh) as usize {
        let start = row * stride + bx as usize;
        data[start..start + bw as usize].fill(0);
    }

    mask.fill_path(path, rule, true, transform);
    // A shape appended past where the contents stop is the island's own end, and a rounded
    // corner is the one thing that cuts one. The outline is what says so, and holding the
    // mask inside it costs a second rasterisation, so only a group with a corner to escape
    // over asks for it.
    if let Some(inside) = inside {
        mask.intersect_path(inside, FillRule::Winding, true, transform);
    }
    Some(mask)
}

/// Fill one box given as `(x, y, width, height)` in logical pixels, rounding each side
/// by its own radius.
fn fill_edged(
    pixmap: &mut PixmapMut<'_>,
    bounds: (f32, f32, f32, f32),
    left: f32,
    right: f32,
    color: Color,
    transform: Transform,
    clip: Option<&Mask>,
) {
    if color.is_transparent() {
        return;
    }
    let (x, y, w, h) = bounds;
    let Some(path) = edged_rect(x, y, w, h, left, right) else {
        return;
    };
    fill_path(pixmap, &path, color, transform, clip);
}

fn fill_path(
    pixmap: &mut PixmapMut<'_>,
    path: &Path,
    color: Color,
    transform: Transform,
    clip: Option<&Mask>,
) {
    if color.is_transparent() {
        return;
    }
    let mut paint = Paint::default();
    paint.set_color(skia_color(color));
    paint.anti_alias = true;
    pixmap.fill_path(path, &paint, FillRule::Winding, transform, clip);
}

/// Fill one uniformly rounded box.
fn fill(
    pixmap: &mut PixmapMut<'_>,
    bounds: (f32, f32, f32, f32),
    radius: f32,
    color: Color,
    transform: Transform,
    clip: Option<&Mask>,
) {
    fill_edged(pixmap, bounds, radius, radius, color, transform, clip);
}

/// Render `frame` into a `wl_shm` ARGB8888 buffer.
///
/// This owns the pixel format, so the Wayland code never has to know how the renderer
/// lays out its bytes.
pub fn render_to_buffer(
    target: Target<'_>,
    frame: &Frame,
    scale: f32,
    painter: &mut Painter<impl DrawText>,
) -> Result<()> {
    let Target {
        canvas,
        width,
        height,
        clip,
        pixels,
    } = target;
    {
        let mut pixmap =
            PixmapMut::from_bytes(canvas, width, height).context("wrapping the shm buffer")?;
        render(&mut pixmap, frame, scale, painter, clip);
    }
    pixels.apply(canvas);
    Ok(())
}

/// Render a menu into a `wl_shm` ARGB8888 buffer.
///
/// Its own entry point rather than a kind of frame: a menu is a surface of its own, with
/// no groups, no separators between modules and nothing to clip.
pub fn render_menu_to_buffer(
    target: Target<'_>,
    frame: &MenuFrame,
    scale: f32,
    painter: &mut Painter<impl DrawText>,
) -> Result<()> {
    let Target {
        canvas,
        width,
        height,
        pixels,
        ..
    } = target;
    {
        let mut pixmap =
            PixmapMut::from_bytes(canvas, width, height).context("wrapping the shm buffer")?;
        render_menu(&mut pixmap, frame, scale, painter);
    }
    pixels.apply(canvas);
    Ok(())
}

fn render_menu(
    pixmap: &mut PixmapMut<'_>,
    frame: &MenuFrame,
    scale: f32,
    painter: &mut Painter<impl DrawText>,
) {
    pixmap.fill(tiny_skia::Color::TRANSPARENT);
    let transform = Transform::from_scale(scale, scale);
    fill(
        pixmap,
        (0.0, 0.0, frame.width, frame.height),
        frame.radius,
        frame.background,
        transform,
        None,
    );

    let Painter { text, icons, .. } = painter;
    let line = text.line_height();
    for row in &frame.rows {
        if row.separator {
            // A rule sits in the middle of the space it was given, inset from both edges
            // so it reads as a division rather than as an edge of its own.
            let inset = frame.width * 0.06;
            let thickness = (1.0f32).max(1.0 / scale);
            fill(
                pixmap,
                (
                    inset,
                    row.y + (row.height - thickness) / 2.0,
                    frame.width - inset * 2.0,
                    thickness,
                ),
                0.0,
                row.foreground,
                transform,
                None,
            );
            continue;
        }

        if row.highlight {
            fill(
                pixmap,
                (0.0, row.y, frame.width, row.height),
                0.0,
                row.highlight_color,
                transform,
                None,
            );
        }
        if let Some(icon) = &row.icon {
            draw_icon_cached(
                pixmap,
                icon,
                row.foreground,
                transform,
                Cut::default(),
                icons,
            );
        }
        if let Some((x, y)) = row.mark {
            draw_tick(pixmap, x, y, line * 0.5, row.foreground, transform);
        }
        if let Some((x, y)) = row.arrow {
            draw_arrow(pixmap, x, y, line * 0.32, row.foreground, transform);
        }
        draw_text(
            pixmap,
            text,
            &row.text,
            (row.text_x, row.text_middle),
            scale,
            row.foreground,
            Cut::default(),
        );
    }
}

/// The tick on a row that is switched on.
fn draw_tick(
    pixmap: &mut PixmapMut<'_>,
    x: f32,
    y: f32,
    size: f32,
    color: Color,
    transform: Transform,
) {
    let mut builder = PathBuilder::new();
    builder.move_to(x, y);
    builder.line_to(x + size * 0.4, y + size * 0.45);
    builder.line_to(x + size * 1.1, y - size * 0.5);
    let Some(path) = builder.finish() else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color(skia_color(color));
    paint.anti_alias = true;
    let stroke = Stroke {
        width: (size * 0.28).max(1.0),
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Stroke::default()
    };
    pixmap.stroke_path(&path, &paint, &stroke, transform, None);
}

/// The arrow on a row that opens another menu.
fn draw_arrow(
    pixmap: &mut PixmapMut<'_>,
    x: f32,
    y: f32,
    size: f32,
    color: Color,
    transform: Transform,
) {
    let mut builder = PathBuilder::new();
    builder.move_to(x, y - size);
    builder.line_to(x + size * 0.8, y);
    builder.line_to(x, y + size);
    builder.close();
    let Some(path) = builder.finish() else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color(skia_color(color));
    paint.anti_alias = true;
    pixmap.fill_path(&path, &paint, FillRule::Winding, transform, None);
}

/// The surface a frame is drawn into: its pixels, its size in them, and the clip mask it
/// keeps between frames.
///
/// One value because the three are one thing - a screen - while the painter and the config
/// behind them are shared by every screen there is.
pub struct Target<'a> {
    pub canvas: &'a mut [u8],
    pub width: u32,
    pub height: u32,
    pub clip: &'a mut Clip,
    pub pixels: Pixels,
}

/// How the surface's bytes are laid out.
///
/// tiny-skia writes premultiplied RGBA, which is `wl_shm`'s ABGR8888 byte for byte. Where
/// the compositor takes that, a frame is finished the moment it is painted; where it takes
/// only ARGB8888 - one of the two formats every compositor must offer - red and blue change
/// places first, which is a pass over every pixel of the surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pixels {
    /// Exactly what the rasteriser wrote.
    AsWritten,
    /// Red and blue the other way round.
    Swapped,
}

impl Pixels {
    fn apply(self, canvas: &mut [u8]) {
        if self == Pixels::AsWritten {
            return;
        }
        for px in canvas.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
        }
    }
}

/// Draw the bar background, then every group and module, into `pixmap`.
///
/// `clip` is the surface's own, not the painter's, for the reason [`Clip`] gives.
fn render(
    pixmap: &mut PixmapMut<'_>,
    frame: &Frame,
    scale: f32,
    painter: &mut Painter<impl DrawText>,
    clip: &mut Clip,
) {
    pixmap.fill(tiny_skia::Color::TRANSPARENT);
    let transform = Transform::from_scale(scale, scale);

    let width = pixmap.width() as f32 / scale;
    let height = pixmap.height() as f32 / scale;
    fill(
        pixmap,
        (0.0, 0.0, width, height),
        frame.radius,
        frame.background,
        transform,
        None,
    );

    // Joined groups remain independent islands. Draw their shared transitions first,
    // so the neighbouring groups cover the overlap just as modules do inside an island.
    for separator in &frame.group_separators {
        draw_separator(pixmap, separator, scale, transform, None);
    }

    // Split up front: drawing an island needs the text backend and the layer at the same
    // time, and they are two independent halves of the painter.
    let Painter {
        text,
        scratch,
        layer_mask,
        icons,
    } = painter;
    let mask = &mut clip.0;
    let (pw, ph) = (pixmap.width(), pixmap.height());

    for group in &frame.groups {
        // An island that is all there goes straight onto the surface. One that is not is
        // drawn opaque on a layer of its own and composited once, so that the modules and
        // separators inside it meet each other at full opacity however they overlap, and
        // only the finished island is faded. Alpha carried on the colours instead would be
        // applied once per fill, and a filled separator overlaps its neighbours by design.
        let bounds = match group.opacity < 1.0 {
            true => device_bounds(group, scale, pw, ph),
            false => None,
        };

        // No bounds means the group is opaque, or has nothing to cover. Either way it goes
        // straight down.
        let Some((bx, by, bw, bh)) = bounds else {
            draw_group(
                pixmap,
                group,
                scale,
                transform,
                &mut Tools { mask, icons, text },
            );
            continue;
        };
        let Some(layer) = layer(scratch, bw, bh) else {
            // Nowhere to draw: an island at full strength is a worse bar than one at the
            // asked-for alpha, but it is still a readable one.
            draw_group(
                pixmap,
                group,
                scale,
                transform,
                &mut Tools { mask, icons, text },
            );
            continue;
        };

        // The island is drawn into the layer's own corner, so the layer only ever has to
        // be as big as the widest island rather than as big as the bar.
        clear(layer, bw, bh);
        let local = transform.post_translate(-(bx as f32), -(by as f32));
        draw_group(
            &mut layer.as_mut(),
            group,
            scale,
            local,
            &mut Tools {
                mask: layer_mask,
                icons,
                text,
            },
        );
        composite(pixmap, layer.as_ref(), (bx, by, bw, bh), group.opacity);
    }
}

/// Draw one island: its background, then its separators, then its modules.
///
/// Everything vector goes through `transform`, but the text backend rasterises glyphs at
/// its own scale and places them itself, so text is the one thing `transform` cannot move.
/// Its translation is taken off the transform and applied by hand, which keeps the two in
/// step wherever the target is: drawing onto a layer only moves the transform.
fn draw_group(
    pixmap: &mut PixmapMut<'_>,
    group: &PlacedGroup,
    scale: f32,
    transform: Transform,
    tools: &mut Tools<'_>,
) {
    let offset = match scale > 0.0 {
        true => (transform.tx / scale, transform.ty / scale),
        false => (0.0, 0.0),
    };
    let radius = |shape: EdgeShape| match shape {
        EdgeShape::Round => group.edges.radius,
        EdgeShape::None => 0.0,
    };
    let (rl, rr) = (radius(group.edges.left), radius(group.edges.right));
    let Some(outline) = edged_rect(group.x, group.y, group.width, group.height, rl, rr) else {
        return;
    };
    fill_path(pixmap, &outline, group.background, transform, None);

    // Square module corners and separator overlap would otherwise spill past a rounded
    // group edge, so the group's contents are clipped to its own outline. Both halves of
    // that are worth avoiding: building the mask costs a path fill, and every fill drawn
    // through one takes a slower blend, which together are a third of a frame.
    let (pw, ph) = (pixmap.width(), pixmap.height());

    // A cap out past where the contents stop is the island's own trailing furniture: it
    // is placed rather than overrun, so the clip that holds the contents in would take it
    // off the bar. It still has to stay inside the island, and a rounded edge is the one
    // thing that can cut it, so it goes down first and through the island's own outline.
    // The slot holds one mask at a time, and everything below wants the other one.
    let trailing = |separator: &PlacedSeparator| {
        separator.cap
            && group
                .content_right
                .is_some_and(|right| separator.x >= right - 0.01)
    };
    if group.separators.iter().any(trailing) {
        let clip = match rl > 0.0 || rr > 0.0 {
            true => drawn_bounds(group, transform, pw, ph).and_then(|bounds| {
                clip_mask(
                    tools.mask,
                    (pw, ph),
                    bounds,
                    &outline,
                    FillRule::Winding,
                    None,
                    transform,
                )
            }),
            // Nothing to cut it with: a square island has no corner for a cap to escape
            // over, which is why one that is not folding gets no mask either.
            false => None,
        };
        for separator in group.separators.iter().filter(|s| trailing(s)) {
            draw_separator(pixmap, separator, scale, transform, clip);
        }
    }

    // A folding island is the other case: what it holds was measured for the width it had
    // when it was open, so something has to stop it as the edge travels over it. An explicit
    // trailing end owns room past the content edge, so that path stops where contents stop
    // and then includes the end's shape. Without one, the island outline is the right mask:
    // module geometry already stops at `content_right`, while rounding the mask there would
    // cut a second corner inside the island's trailing padding.
    let mask_path = match group.content_right {
        Some(right) => {
            // Module fills and joins share snapped device columns. The moving clip
            // must use the same columns or its partially covered last pixel exposes
            // the bar background beside an otherwise solid ribbon.
            let left = snap(group.x, scale);
            let stopped = match group.content_edge.is_some() {
                true => edged_rect(
                    left,
                    group.y,
                    snap(right, scale) - left,
                    group.height,
                    rl,
                    rr,
                ),
                false => Some(outline.clone()),
            };
            // Past that column the island carries on into its own end, and what is written
            // there belongs inside the shape that end is drawn with. The two abut rather
            // than overlap, so one fill rule serves both halves.
            let region = (group.content_edge.as_ref())
                .and_then(|edge| edge_region(edge, right, scale))
                .zip(stopped.as_ref())
                .and_then(|((shape, rule), rect)| {
                    let mut builder = PathBuilder::new();
                    builder.push_path(rect);
                    builder.push_path(&shape);
                    Some((builder.finish()?, rule))
                });
            let extended = region.is_some();
            (region.or_else(|| stopped.map(|rect| (rect, FillRule::Winding))))
                .map(|(path, rule)| (path, rule, extended))
        }
        None => ((rl > 0.0 || rr > 0.0) && spills(group))
            .then(|| (outline.clone(), FillRule::Winding, false)),
    };
    let masked = mask_path.as_ref().and_then(|(path, rule, extended)| {
        let bounds = drawn_bounds(group, transform, pw, ph)?;
        // Only an extended mask can reach outside the island, and only a rounded corner
        // can cut what does.
        let inside = (*extended && (rl > 0.0 || rr > 0.0)).then_some(&outline);
        Some((
            clip_mask(tools.mask, (pw, ph), bounds, path, *rule, inside, transform)?,
            bounds,
        ))
    });
    let clip = masked.map(|(mask, _)| mask);
    // The mask catches every fill and every icon; text is placed by the backend and has to
    // be told where the contents stop. Only a folding island tells it: everywhere else the
    // mask exists to keep an end separator inside a rounded corner, and glyphs have never
    // been cut there.
    let column = |edge: f32| ((edge + offset.0) * scale).round() as i32;
    // How far the island reaches past where its fills stop, which is the width of the end
    // it is drawn with. Nothing is written past that whatever the mask says.
    let reach = (group.content_edge.as_ref()).map_or(0.0, |edge| edge.width);
    let cut = |edge: Option<f32>, shift: i32| match edge {
        Some(edge) => Cut {
            mask: clip,
            bounds: masked.map(|(_, bounds)| bounds).unwrap_or_default(),
            stop: Some(column(edge + reach)),
            shift,
        },
        None => Cut::default(),
    };
    // Icons stop with the fills, at the island's edge. Text stops short of it, where the
    // shut island's wording would have stopped - which is at its icon, because a collapsed
    // island is an icon and the padding around it. Both stop on the same shape: the wording
    // reads the mask that many columns along instead of being cut by a straight one.
    let inset = match (group.content_right, group.text_right) {
        (Some(right), Some(text)) => (column(right) - column(text)).max(0),
        _ => 0,
    };
    let (marks, wording) = (cut(group.content_right, 0), cut(group.text_right, inset));

    // Separators go down before the modules, so any overlap is covered by them. A divider
    // out past where the contents stop is the opposite case to the cap above: it belongs
    // to content the fold has already covered, and letting it through would leave it
    // standing outside the island, or inside the one beside it.
    // A divider the fold has gone past belongs to content it has covered, and the mask no
    // longer stops it: what the island keeps beyond that column is its own end, and this
    // would paint the colours of two hidden modules into it.
    let covered = |separator: &PlacedSeparator| {
        !separator.cap && (group.content_right).is_some_and(|right| separator.x >= right - 0.01)
    };
    for separator in (group.separators.iter()).filter(|s| !trailing(s) && !covered(s)) {
        draw_separator(pixmap, separator, scale, transform, clip);
    }

    // Where a module's ground stops. A fold moves that in over the contents, and the fills
    // are cut there by geometry rather than by the mask, which is the same trade
    // `outer_radii` makes at a corner: one rasterised edge instead of two multiplied. Doing
    // it through the mask leaves the corner a shade off the settled frame's, so the island
    // changes colour on the frame it arrives, and it charges every fill the slower blend.
    let edge = group.content_right.unwrap_or(group.x + group.width);
    // Where the island's own ground ends, which is not where its contents do. Snapped,
    // because what it is compared against is: a fill's own edge is put on a whole device
    // pixel, and an island ending a fraction of one further along would otherwise never
    // look like the edge its last module reaches.
    let side = snap(group.x + group.width, scale);
    for (index, module) in group.modules.iter().enumerate() {
        // Snapped against the same grid as the separators, so the edge a module shares
        // with the gap beside it is one edge rather than two.
        let (mx0, mx1) = (
            snap(module.x, scale),
            snap(module.x + module.width, scale).min(snap(edge, scale)),
        );
        let (ml, mr) = outer_radii(module, group, index, mx1, side, rl, rr);
        // A fill may be cut by geometry instead of by the mask only where its geometry says
        // the same thing the island's arc does. Two ways it can fail to, and a travelling
        // edge finds both: the module the edge is part way across is a sliver, and
        // `edged_rect` clamps a radius to half the box, so two pixels at a twelve pixel
        // corner round by one; and the module before that sliver ends inside the corner
        // with a square side of its own, because it is no longer the one at the island's
        // edge and takes no radius from it. Either way the fill reaches outside the arc it
        // belongs inside, so it goes back through the mask and gives up its corners to it.
        // Everything clear of both corners - which is most of an island, every frame -
        // keeps the single rasterised edge and the faster blend.
        let fits = |r: f32| r * 2.0 <= (mx1 - mx0).min(module.height) + 0.01;
        let inside_left = mx0 >= group.x + rl - 0.01 || (ml >= rl - 0.01 && fits(ml));
        let inside_right = mx1 <= side - rr + 0.01 || (mr >= rr - 0.01 && fits(mr));
        // Whether geometry can stand in for the mask, which is a question about this fill
        // and the island's arc and nothing else. Asking it only of a folding island left
        // the two frames of a hand-over rasterising the same rectangle two different ways:
        // the settled one carries a mask whenever its caps spill, and a fill drawn through
        // one takes the arc twice - the mask's coverage times its own - which is a shade
        // short of the fold's last frame, drawn once.
        let shaped = inside_left && inside_right;
        let (ml, mr) = match group.content_right.is_some() && !shaped {
            true => (module.radius, module.radius),
            false => (ml, mr),
        };
        fill_edged(
            pixmap,
            (mx0, module.y, mx1 - mx0, module.height),
            ml,
            mr,
            module.background,
            transform,
            match shaped {
                true => None,
                false => clip,
            },
        );
        // A wording travel can grow an icon out of a box narrower than the icon itself,
        // and a module fold carries that box over its expanded contents. Marks stop at
        // the visible box in both cases; the group cut still supplies any island edge.
        let marks_stop = module
            .content_right
            .map(|right| ((right + offset.0) * scale).ceil() as i32);
        let marks = marks_stop.map_or(marks, |stop| marks.stopped(stop));
        if let Some(icon) = &module.icon {
            draw_icon_cached(
                pixmap,
                icon,
                module.foreground,
                transform,
                marks,
                tools.icons,
            );
        }
        // Wording can stop earlier than the icon during a fold, progressively leaving the
        // collapsed style's padding empty. A wording transition sets both module edges to
        // the old cutoff, so it keeps its existing behaviour. Rounded up because this is
        // the first column outside the content rather than its last painted one.
        let wording_stop = module
            .text_right
            .map(|right| ((right + offset.0) * scale).ceil() as i32);
        let wording = wording_stop.map_or(wording, |stop| wording.stopped(stop));
        // Layout already placed the text; only the vertical placing is ours, and every
        // wording is put on the one baseline rather than centred on its own ink.
        let ty = module.y + module.height / 2.0;
        let (tx, ty) = (module.text_x + offset.0, ty + offset.1);
        draw_text(
            pixmap,
            tools.text,
            &module.text,
            (tx, ty),
            scale,
            module.foreground,
            wording,
        );
    }
}

/// Whether anything inside `group` could paint past a rounded edge.
///
/// Only the outer edges are at risk, and only two things reach them. The first and last
/// module's background does, and that is handled by giving it the group's own corner radius
/// rather than a mask - see `outer_radii`. A separator drawn at a group end does too, and
/// that one cannot be rounded away, so it is the only case left that needs clipping.
///
/// Icons and text are not considered. They sit inside their module, and text is not clipped
/// at all in any case, since the backend places glyphs itself.
fn spills(group: &PlacedGroup) -> bool {
    let right = group.x + group.width;
    group
        .separators
        .iter()
        .any(|s| !s.shape.is_none() && (s.x <= group.x + 0.01 || s.x + s.width >= right - 0.01))
}

/// The corner radii a module's background is filled with.
///
/// A module that reaches a group's rounded corner takes the group's radius there, so its
/// fill stops exactly where the group's outline does. That replaces a clip mask with the
/// geometry it would have produced, and produces a cleaner edge than the mask did: coverage
/// is rasterised once instead of being multiplied by a second antialiased edge.
///
/// A module inset by the group's padding does not reach the corner and keeps its own radius.
///
/// `edge` is where the fill actually stops, which is the island's own right-hand side
/// unless a fold has moved the contents' edge in over it. Reaching that is what makes a
/// module the one wearing the corner, whether or not it is the last in the group: a fold
/// cuts whichever module the edge has arrived at.
fn outer_radii(
    module: &PlacedModule,
    group: &PlacedGroup,
    index: usize,
    reach: f32,
    side: f32,
    left: f32,
    right: f32,
) -> (f32, f32) {
    let full_height = module.height >= group.height - 0.01 && module.y <= group.y + 0.01;
    if !full_height {
        return (module.radius, module.radius);
    }
    let at_left = index == 0 && module.x <= group.x + 0.01;
    // Where the fill stops against where the island does, rather than against where its
    // contents do. A trailing cap sits in room of its own beyond the contents, so a fold
    // that has moved the contents in leaves no module at the corner at all - and the shut
    // island it is heading for has none there either, which is the frame it has to match.
    let at_right = reach >= side - 0.01;
    (
        if at_left {
            left.max(module.radius)
        } else {
            module.radius
        },
        if at_right {
            right.max(module.radius)
        } else {
            module.radius
        },
    )
}

/// Where a group lands on the target it is being drawn into, in whole device pixels.
///
/// `device_bounds` answers the same question against the surface; this one goes through the
/// transform, so it is right for a group drawn onto a layer at an offset as well.
fn drawn_bounds(
    group: &PlacedGroup,
    transform: Transform,
    width: u32,
    height: u32,
) -> Option<(u32, u32, u32, u32)> {
    let map = |v: f32, scale: f32, offset: f32| v * scale + offset;
    let (gx, gy, gw, gh) = group.paint_bounds();
    let x0 = map(gx, transform.sx, transform.tx).floor();
    let y0 = map(gy, transform.sy, transform.ty).floor();
    let x1 = map(gx + gw, transform.sx, transform.tx).ceil();
    let y1 = map(gy + gh, transform.sy, transform.ty).ceil();

    let x0 = x0.clamp(0.0, width as f32) as u32;
    let y0 = y0.clamp(0.0, height as f32) as u32;
    let x1 = x1.clamp(0.0, width as f32) as u32;
    let y1 = y1.clamp(0.0, height as f32) as u32;
    match (x1 > x0, y1 > y0) {
        (true, true) => Some((x0, y0, x1 - x0, y1 - y0)),
        _ => None,
    }
}

/// The whole device pixels a group can reach, as `(x, y, width, height)` clamped to the
/// surface. `None` for a group with no pixels to its name.
fn device_bounds(
    group: &PlacedGroup,
    scale: f32,
    width: u32,
    height: u32,
) -> Option<(u32, u32, u32, u32)> {
    let (gx, gy, gw, gh) = group.paint_bounds();
    let x0 = (gx * scale).floor().clamp(0.0, width as f32) as u32;
    let y0 = (gy * scale).floor().clamp(0.0, height as f32) as u32;
    let x1 = ((gx + gw) * scale).ceil().clamp(0.0, width as f32) as u32;
    let y1 = ((gy + gh) * scale).ceil().clamp(0.0, height as f32) as u32;
    match (x1 > x0, y1 > y0) {
        (true, true) => Some((x0, y0, x1 - x0, y1 - y0)),
        _ => None,
    }
}

/// Take the top-left `width` x `height` of `layer` back to nothing.
///
/// Only the corner an island is about to be drawn into, since the layer is sized to the
/// widest island the bar has and most are narrower than that.
fn clear(layer: &mut Pixmap, width: u32, height: u32) {
    let stride = layer.width() as usize;
    let pixels = layer.pixels_mut();
    for row in 0..height as usize {
        let start = row * stride;
        pixels[start..start + width as usize].fill(PremultipliedColorU8::TRANSPARENT);
    }
}

/// Put the island in `layer`'s corner onto the surface at `bounds`, faded to `opacity`.
///
/// Device pixel to device pixel: no transform, no sampling, and only the island's own
/// rectangle is touched. Both sides are premultiplied, so fading is a multiply across all
/// four channels and the blend is the ordinary source-over.
fn composite(
    pixmap: &mut PixmapMut<'_>,
    layer: PixmapRef<'_>,
    bounds: (u32, u32, u32, u32),
    opacity: f32,
) {
    let (bx, by, bw, bh) = bounds;
    let alpha = (opacity.clamp(0.0, 1.0) * 255.0).round() as u32;
    let scale = |v: u8, by: u32| ((v as u32 * by + 127) / 255) as u8;

    let src_stride = layer.width() as usize;
    let dst_stride = pixmap.width() as usize;
    let src = layer.pixels();
    let dst = pixmap.pixels_mut();

    for row in 0..bh as usize {
        let s = row * src_stride;
        let d = (by as usize + row) * dst_stride + bx as usize;
        // Taken a row at a time so the bounds are checked once rather than per pixel.
        let over_row = &src[s..s + bw as usize];
        let under_row = &mut dst[d..d + bw as usize];

        for (over, under) in over_row.iter().zip(under_row.iter_mut()) {
            if over.alpha() == 0 {
                continue;
            }
            // Scaling a premultiplied colour keeps every channel under its own alpha, so
            // the result is still a valid premultiplied colour.
            let (r, g, b, a) = (
                scale(over.red(), alpha),
                scale(over.green(), alpha),
                scale(over.blue(), alpha),
                scale(over.alpha(), alpha),
            );
            // Nothing behind it, which is the whole of an island that sits on a bar with
            // no background of its own.
            if under.alpha() == 0 {
                *under = PremultipliedColorU8::from_rgba(r, g, b, a).unwrap_or(*under);
                continue;
            }
            let rest = 255 - a as u32;
            *under = PremultipliedColorU8::from_rgba(
                r + scale(under.red(), rest),
                g + scale(under.green(), rest),
                b + scale(under.blue(), rest),
                a + scale(under.alpha(), rest),
            )
            .unwrap_or(*under);
        }
    }
}

/// What the renderer keeps between frames.
///
/// Shaped fonts and the spare surface both cost too much to build per redraw, and neither
/// depends on the frame being drawn, so they live here and the draw call borrows them.
pub struct Painter<T = TextRenderer> {
    /// Shapes and draws text, and answers layout's questions about how wide it is.
    pub text: T,
    /// A layer for the groups that are composited rather than drawn straight on. One
    /// buffer serves every such group in a frame, since each is cleared, drawn and put
    /// down before the next is started.
    scratch: Option<Pixmap>,
    /// The clip mask for the layer, kept between frames. tiny-skia wants a mask exactly
    /// the size of what it clips, and a frame can draw groups onto either the layer or the
    /// surface; the surface's own mask therefore belongs to the surface and is passed in.
    layer_mask: Option<Mask>,
    /// Rasterised icons, kept between frames.
    icons: IconCache,
}

/// The clip mask one surface keeps between frames.
///
/// tiny-skia wants a mask exactly the size of what it clips, so this belongs to the surface
/// rather than to the painter, which draws every bar in turn: one shared between two
/// screens of different widths would be reallocated on each redraw. Opaque for the same
/// reason the rest of the renderer's types are - nothing above `Frame` should have to name
/// one.
#[derive(Default)]
pub struct Clip(Option<Mask>);

impl<T: DrawText> Painter<T> {
    pub fn new(text: T) -> Painter<T> {
        Painter {
            text,
            scratch: None,
            layer_mask: None,
            icons: IconCache::new(),
        }
    }
}

/// The layer, at least `width` x `height`.
///
/// Sized to the largest island that has asked for one rather than to the bar, and only
/// ever grown, so a bar with no translucent island never allocates it, the first frame
/// settles the size, and no redraw after that allocates at all.
fn layer(scratch: &mut Option<Pixmap>, width: u32, height: u32) -> Option<&mut Pixmap> {
    let (have_w, have_h) = match &scratch {
        Some(p) => (p.width(), p.height()),
        None => (0, 0),
    };
    if have_w < width || have_h < height {
        *scratch = Pixmap::new(have_w.max(width), have_h.max(height));
    }
    scratch.as_mut()
}

#[cfg(test)]
mod tests;
