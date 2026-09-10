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
fn separator_path(shape: SeparatorShape, x0: f32, y0: f32, x1: f32, y1: f32) -> Option<Path> {
    let w = x1 - x0;
    let h = y1 - y0;
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let ymid = (y0 + y1) / 2.0;
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

    let Some(path) = separator_path(sep.shape, x0, y0, x1, y1) else {
        return;
    };
    // Outer caps occupy the side of the boundary adjacent to their module. Build
    // a single even-odd path for the complement, preserving transparent bar backgrounds.
    let path = if sep.inverted {
        let mut builder = PathBuilder::new();
        let Some(rect) = Rect::from_xywh(x0, y0, x1 - x0, y1 - y0) else {
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
        // Reflect about the gap's vertical centre line.
        match path.transform(Transform::from_row(-1.0, 0.0, 0.0, 1.0, x0 + x1, 0.0)) {
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
    line_height: f32,
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

/// Put a string on the pixmap with its left edge at `x` and its top at `y`.
///
/// The backend does the placing and the colouring: the text side hands back pixels and
/// where they sit relative to the origin, and knows nothing about what they land on.
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
    let (ox, oy) = ((at.0 * scale).round() as i32, (at.1 * scale).round() as i32);
    let Some(run) = text.run(what) else {
        return;
    };
    let (rx, ry) = (ox + run.left, oy + run.top);
    let (rw, rh) = (run.width, run.height);
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
    let shape = separator_path(edge.shape, x0, y0, x1, y1)?;
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
            (row.text_x, row.text_y),
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
        for px in canvas.chunks_exact_mut(4) {
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
    let line_height = text.line_height();
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
                &mut Tools {
                    mask,
                    icons,
                    text,
                    line_height,
                },
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
                &mut Tools {
                    mask,
                    icons,
                    text,
                    line_height,
                },
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
                line_height,
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
    // when it was open, so something has to stop it as the edge travels over it. Not the
    // island's own outline, though - the trailing cap sits in room of its own inside that,
    // the way it does when nothing is folding, and modules run through to the island's
    // edge would fill the open side of the cap in. Contents stop where contents stop.
    let mask_path = match group.content_right {
        Some(right) => {
            // Module fills and joins share snapped device columns. The moving clip
            // must use the same columns or its partially covered last pixel exposes
            // the bar background beside an otherwise solid ribbon.
            let left = snap(group.x, scale);
            let stopped = edged_rect(
                left,
                group.y,
                snap(right, scale) - left,
                group.height,
                rl,
                rr,
            );
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
        // A module part way between two wordings is the one thing cut at its own edge
        // rather than at the island's: what is written on it was fitted to the width it
        // lands at, and until it lands that is not the width it is drawn in. Rounded up,
        // because the column is where the wording stops rather than the last one it
        // reaches, and a wording that exactly fills its box must not lose its last pixel
        // to it on the frame before it arrives.
        let wording = match module.text_right {
            Some(right) => wording.stopped(((right + offset.0) * scale).ceil() as i32),
            None => wording,
        };
        // Layout already placed the text; only the vertical centring is ours.
        let ty = module.y + (module.height - tools.line_height) / 2.0;
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
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::geometry::Edges;
    use crate::icon::Icon;
    use crate::layout::{Frame, PlacedGroup, PlacedModule, PlacedSeparator};
    use tiny_skia::Pixmap;

    /// A text backend with no fonts, which paints a solid block per character.
    ///
    /// Layout's stub answers how wide text is; this one has to put ink on the page, so a
    /// test can see whether text landed where it belongs. Every character is one unit wide
    /// and the block is the full line height, which makes a drawn string a rectangle at a
    /// position the test can predict.
    struct Blocks {
        /// The output scale, held by the backend rather than taken from the renderer -
        /// which is exactly why text has to be moved by hand when the target moves.
        scale: f32,
        run: Option<TextRun>,
    }

    const BLOCK: f32 = 8.0;

    impl DrawText for Blocks {
        fn line_height(&self) -> f32 {
            BLOCK
        }

        /// A solid block per character, rasterised at the backend's own scale and with no
        /// reference to the renderer's transform - which is how the real one behaves.
        fn run(&mut self, text: &str) -> Option<&TextRun> {
            if text.is_empty() {
                return None;
            }
            let s = self.scale;
            let width = (text.chars().count() as f32 * BLOCK * s) as usize;
            let height = (BLOCK * s) as usize;
            self.run = Some(TextRun {
                left: 0,
                top: 0,
                width,
                height,
                pixels: RunPixels::Coverage(vec![0xff; width * height]),
            });
            self.run.as_ref()
        }
    }

    impl crate::layout::Measure for Blocks {
        fn measure(&mut self, text: &str) -> f32 {
            text.chars().count() as f32 * BLOCK
        }
    }

    /// A bar of the shape the rules care about: an island per position, a Powerline run of
    /// eight modules with filled separators, and one faded group, which is the frame that
    /// goes down the slowest path the renderer has.
    const BUSY: &str = r##"
[bar]
height = 34
font = "sans-serif 11"

[colors]
ink = "#cdd6f4"
one = "#313244"
two = "#45475a"

[style.default]
foreground = "$ink"
background = "$one"
padding = 8

[left]
groups = ["l"]

[center]
groups = ["c"]

[right]
groups = ["r"]

[group.l]
modules = ["a", "b", "c"]

[group.c]
modules = ["d"]
opacity = 0.85

[group.r]
modules = ["e", "f", "g", "h", "i", "j", "k", "m"]

[group.r.separator]
shape = "slant"
width = 12
direction = "right"
color = "previous"

[group.r.ends]
left = "slant"
right = "slant"

[module.a]
format = "$text"
[module.b]
format = "$text"
[module.c]
format = "$text"
[module.d]
format = "$text"
[module.e]
format = "$text"
[module.f]
format = "$text"
[module.g]
format = "$text"
[module.h]
format = "$text"
[module.i]
format = "$text"
[module.j]
format = "$text"
[module.k]
format = "$text"
[module.m]
format = "$text"
"##;

    /// The two layouts are the same picture, told to the compositor two ways: ABGR8888 is
    /// what the rasteriser already writes, ARGB8888 is that with red and blue changed
    /// over. Getting this the wrong way round is a bar drawn in the wrong colours, which
    /// is why it is pinned here rather than left to a screenshot.
    #[test]
    fn the_two_buffer_formats_differ_only_in_red_and_blue() {
        let frame = island(1.0);
        let (w, h) = (500u32, 20u32);
        let paint = |pixels: Pixels| {
            let mut canvas = vec![0u8; w as usize * h as usize * 4];
            let mut painter = Painter::new(Blocks {
                scale: 1.0,
                run: None,
            });
            render_to_buffer(
                Target {
                    canvas: &mut canvas,
                    width: w,
                    height: h,
                    clip: &mut Clip::default(),
                    pixels,
                },
                &frame,
                1.0,
                &mut painter,
            )
            .expect("painting");
            canvas
        };

        let written = paint(Pixels::AsWritten);
        let swapped = paint(Pixels::Swapped);
        assert_ne!(
            written, swapped,
            "the island is not grey, so the two must differ"
        );
        for (a, b) in written.chunks_exact(4).zip(swapped.chunks_exact(4)) {
            assert_eq!(
                [a[2], a[1], a[0], a[3]],
                [b[0], b[1], b[2], b[3]],
                "the swap has to move red and blue and leave green and alpha alone"
            );
        }
    }

    /// What a whole frame costs to paint, which is the number that decides whether
    /// partial repainting is worth its buffer-age machinery.
    ///
    /// Ignored: it measures the machine it runs on, and a test that fails on a slow one
    /// would be a test about hardware. Run it deliberately:
    ///
    /// ```text
    /// cargo test --release -- --ignored --nocapture paint_costs
    /// ```
    #[test]
    #[ignore]
    fn paint_costs_this_much_per_frame() {
        use crate::status::{Fields, State, StatusItem, Value};
        use std::time::Instant;

        let names = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "m"];
        let items: Vec<StatusItem> = names
            .iter()
            .map(|name| {
                let mut fields = Fields::default();
                fields.set("text", Value::Text(format!("{name} 100%")));
                StatusItem {
                    id: Some((*name).to_string()),
                    fields,
                    state: State::Idle,
                    urgent: false,
                    foreground: None,
                    background: None,
                    action: None,
                }
            })
            .collect();

        let cfg = Config::parse(BUSY).expect("the bench config parses");
        println!("width scale  pixels        as written    swapped");
        for width in [1920.0f32, 3840.0] {
            for scale in [1.0f32, 2.0] {
                let mut painter = Painter::new(Blocks { scale, run: None });
                let frame = {
                    let native = crate::collect::Registry::new(&Default::default());
                    let inputs = crate::layout::Inputs {
                        items: &items,
                        native: &native,
                        sway: &Default::default(),
                        alt: &Default::default(),
                        pages: &Default::default(),
                        collapsed_groups: &Default::default(),
                        switching: &Default::default(),
                        folding: &Default::default(),
                        collapsed: &Default::default(),
                        waiting: &Default::default(),
                        spin: 0,
                        tray: &Default::default(),
                        output: None,
                    };
                    crate::layout::compute(
                        &cfg,
                        &inputs,
                        width,
                        cfg.bar.height as f32,
                        &mut painter.text,
                        None,
                    )
                };

                let (pw, ph) = (
                    (width * scale) as u32,
                    (cfg.bar.height as f32 * scale) as u32,
                );
                let mut canvas = vec![0u8; pw as usize * ph as usize * 4];
                let mut clip = Clip::default();

                let time = |canvas: &mut Vec<u8>,
                            clip: &mut Clip,
                            painter: &mut Painter<Blocks>,
                            pixels: Pixels| {
                    // One frame first, so a cache filling up is not counted as painting.
                    let mut once = |canvas: &mut Vec<u8>, clip: &mut Clip| {
                        render_to_buffer(
                            Target {
                                canvas,
                                width: pw,
                                height: ph,
                                clip,
                                pixels,
                            },
                            &frame,
                            scale,
                            painter,
                        )
                        .expect("painting a frame");
                    };
                    once(canvas, clip);
                    let runs = 50;
                    let start = Instant::now();
                    for _ in 0..runs {
                        once(canvas, clip);
                    }
                    start.elapsed().as_secs_f64() * 1e6 / runs as f64
                };

                let written = time(&mut canvas, &mut clip, &mut painter, Pixels::AsWritten);
                let swapped = time(&mut canvas, &mut clip, &mut painter, Pixels::Swapped);
                println!(
                    "{width:>5.0} {scale:>5.0} {:>9}  {written:>9.0} us {swapped:>9.0} us",
                    pw * ph
                );
            }
        }
    }

    const A: Color = Color::rgba(0x3c, 0x38, 0x36, 0xff);
    const B: Color = Color::rgba(0x50, 0x49, 0x45, 0xff);
    const INK: Color = Color::rgba(0xff, 0xff, 0xff, 0xff);
    const TILE: Color = A;
    const TILE_ALT: Color = B;

    /// Paint two tiles with a curve between them, the way a group does, and report the
    /// alpha of every column across the join.
    ///
    /// With an `opacity` the island is drawn on a layer and composited, which is what the
    /// renderer does for a group that asks to be faded.
    fn alphas_across_a_join(scale: f32, module_edge: f32, opacity: f32) -> Vec<u8> {
        let (w, h) = (60.0f32, 10.0f32);
        let (dw, dh) = ((w * scale) as u32, (h * scale) as u32);
        let mut pixmap = Pixmap::new(dw, dh).unwrap();
        let mut layer = Pixmap::new(dw, dh).unwrap();
        let faded = opacity < 1.0;
        let mut canvas = if faded {
            layer.as_mut()
        } else {
            pixmap.as_mut()
        };
        let transform = Transform::from_scale(scale, scale);

        let sep = PlacedSeparator {
            x: module_edge,
            y: 0.0,
            width: 20.0,
            height: h,
            shape: SeparatorShape::Curve,
            direction: Direction::Right,
            overlap: 0.0,
            inverted: false,
            cap: false,
            fill: A,
            under: B,
        };
        draw_separator(&mut canvas, &sep, scale, transform, None);

        for (x0, x1, color) in [(0.0, module_edge, A), (module_edge + sep.width, w, B)] {
            let (sx0, sx1) = (snap(x0, scale), snap(x1, scale));
            fill(
                &mut canvas,
                (sx0, 0.0, sx1 - sx0, h),
                0.0,
                color,
                transform,
                None,
            );
        }

        if faded {
            composite(
                &mut pixmap.as_mut(),
                layer.as_ref(),
                (0, 0, dw, dh),
                opacity,
            );
        }

        // One row through the middle, where the curve's own boundary is not in play.
        let row = (h * scale) as usize / 2;
        let stride = (w * scale) as usize;
        pixmap.pixels()[row * stride..(row + 1) * stride]
            .iter()
            .map(|p| p.alpha())
            .collect()
    }

    /// A filled separator and the modules on either side of it have to cover the gap
    /// between them completely, at any scale and wherever layout happened to put the
    /// boundary. A column that is not fully opaque is a line of wallpaper showing through
    /// the middle of an island.
    #[test]
    fn a_filled_separator_leaves_no_seam_between_its_two_tiles() {
        for scale in [1.0, 2.0] {
            for edge in [17.0, 17.5, 17.3, 17.87] {
                let alphas = alphas_across_a_join(scale, edge, 1.0);
                let dip = alphas.iter().copied().min().unwrap();
                assert_eq!(
                    dip, 0xff,
                    "scale {scale}, edge {edge}: a column fell to {dip:#x}, so two fills \
                     that share an edge each took part of it and neither covered the pixel"
                );
            }
        }
    }

    /// The point of drawing an island on a layer: its alpha lands once, on the finished
    /// island, so a filled separator inside it is neither heavier than its neighbours
    /// where the two overlap nor lighter where they meet.
    #[test]
    fn a_faded_island_is_the_same_alpha_the_whole_way_across() {
        for scale in [1.0, 2.0] {
            for edge in [17.0, 17.5, 17.3, 17.87] {
                let alphas = alphas_across_a_join(scale, edge, 0.8);
                let (lo, hi) = (
                    alphas.iter().copied().min().unwrap(),
                    alphas.iter().copied().max().unwrap(),
                );
                assert_eq!(
                    (lo, hi),
                    (0xcc, 0xcc),
                    "scale {scale}, edge {edge}: alpha ran {lo:#x}..={hi:#x} across the \
                     island, so something inside it was composited more than once"
                );
            }
        }
    }

    /// The middle row of a separator drawn on its own, as `(r, g, b, a)` per column.
    fn separator_row(direction: Direction, fill: Color, under: Color) -> Vec<(u8, u8, u8, u8)> {
        let (w, h) = (40.0f32, 8.0f32);
        let mut pixmap = Pixmap::new(w as u32, h as u32).unwrap();
        let sep = PlacedSeparator {
            x: 4.0,
            y: 0.0,
            width: 32.0,
            height: h,
            shape: SeparatorShape::Slant,
            direction,
            overlap: 0.0,
            inverted: false,
            cap: false,
            fill,
            under,
        };
        draw_separator(&mut pixmap.as_mut(), &sep, 1.0, Transform::identity(), None);

        let row = h as usize / 2;
        pixmap.pixels()[row * w as usize..(row + 1) * w as usize]
            .iter()
            .map(|p| (p.red(), p.green(), p.blue(), p.alpha()))
            .collect()
    }

    /// `direction` mirrors the boundary and nothing else: the two colours stay on their
    /// own sides. So pointing a separator the other way is the same picture reflected,
    /// with the colours named the other way round - which is what lets one set of shapes
    /// serve a bar that reads right-to-left, and why only the path is reflected rather
    /// than a concave complement being built for every shape.
    #[test]
    fn a_separator_pointing_left_is_one_pointing_right_reflected() {
        let left = separator_row(Direction::Left, TILE, TILE_ALT);
        let mut right = separator_row(Direction::Right, TILE_ALT, TILE);
        right.reverse();
        assert_eq!(
            left, right,
            "mirroring moved the colours, not just the boundary"
        );
    }

    /// A hairline is symmetric, so it is the one shape `direction` must not touch.
    #[test]
    fn a_hairline_reads_the_same_way_round() {
        let row = |direction| {
            let (w, h) = (40.0f32, 8.0f32);
            let mut pixmap = Pixmap::new(w as u32, h as u32).unwrap();
            let sep = PlacedSeparator {
                x: 4.0,
                y: 0.0,
                width: 32.0,
                height: h,
                shape: SeparatorShape::Line,
                direction,
                overlap: 0.0,
                inverted: false,
                cap: false,
                fill: TILE,
                under: TILE_ALT,
            };
            draw_separator(&mut pixmap.as_mut(), &sep, 1.0, Transform::identity(), None);
            pixmap
                .pixels()
                .iter()
                .map(|p| p.alpha())
                .collect::<Vec<_>>()
        };
        assert_eq!(row(Direction::Left), row(Direction::Right));
    }

    // -----------------------------------------------------------------------
    // Whole frames
    // -----------------------------------------------------------------------

    fn module(x: f32, width: f32, background: Color, text: &str, icon: bool) -> PlacedModule {
        PlacedModule {
            x,
            y: 0.0,
            width,
            height: 20.0,
            icon: icon.then_some(PlacedIcon {
                art: None,
                icon: Icon::Cpu,
                level: 0,
                x: x + 2.0,
                y: 4.0,
                size: 12.0,
            }),
            text: text.to_string(),
            text_x: x + 16.0,
            text_right: None,
            foreground: INK,
            background,
            radius: 0.0,
            action: None,
            name: None,
            alt: None,
            alt_button: crate::config::Button::Left,
            collapsible: false,
            collapse_button: crate::config::Button::Right,
            refresh: None,
            mute: None,
            paged: None,
            on_click: None,
        }
    }

    /// One island a long way along the bar, with two tiles, a curve between them, an icon
    /// and text - everything the renderer places through a transform, plus the one thing
    /// it does not.
    fn island(opacity: f32) -> Frame {
        let x = 300.0;
        let (first, gap, second) = (60.0, 12.0, 60.0);
        Frame {
            groups: vec![PlacedGroup {
                collapse: None,
                content_right: None,
                content_edge: None,
                text_right: None,
                x,
                y: 0.0,
                width: first + gap + second,
                height: 20.0,
                background: Color::TRANSPARENT,
                opacity,
                edges: Edges {
                    left: EdgeShape::Round,
                    right: EdgeShape::Round,
                    radius: 6.0,
                },
                modules: vec![
                    module(x, first, TILE, "ab", true),
                    module(x + first + gap, second, TILE_ALT, "cd", true),
                ],
                separators: vec![PlacedSeparator {
                    x: x + first,
                    y: 0.0,
                    width: gap,
                    height: 20.0,
                    shape: SeparatorShape::Curve,
                    direction: Direction::Right,
                    overlap: 0.0,
                    inverted: false,
                    cap: false,
                    fill: TILE,
                    under: TILE_ALT,
                }],
            }],
            ..Frame::default()
        }
    }

    fn shot(frame: &Frame, scale: f32) -> Pixmap {
        let mut pixmap = Pixmap::new((480.0 * scale) as u32, (20.0 * scale) as u32).unwrap();
        let mut painter = Painter::new(Blocks { scale, run: None });
        render(
            &mut pixmap.as_mut(),
            frame,
            scale,
            &mut painter,
            &mut Clip::default(),
        );
        pixmap
    }

    /// The bar's own ground is the frame's, not the config's: the renderer is handed
    /// positioned geometry and colour and reads nothing else, which is the whole of what
    /// makes it replaceable.
    #[test]
    fn the_bar_draws_the_ground_the_frame_carries() {
        let ground = Color::rgba(0x28, 0x28, 0x28, 0xff);
        let frame = Frame {
            background: ground,
            radius: 6.0,
            ..island(1.0)
        };
        let painted = shot(&frame, 1.0);
        let at = |x: u32, y: u32| {
            let p = painted.pixel(x, y).expect("inside the pixmap");
            (p.red(), p.green(), p.blue(), p.alpha())
        };

        // Everywhere the islands are not, down to the rounded corner.
        assert_eq!(at(200, 10), (ground.r, ground.g, ground.b, 0xff));
        assert_eq!(at(0, 0).3, 0, "the radius rounds the corner off the ground");

        // And nothing at all where the frame says the bar is transparent.
        let clear = shot(
            &Frame {
                background: Color::TRANSPARENT,
                ..frame
            },
            1.0,
        );
        assert_eq!(clear.pixel(200, 10).expect("inside").alpha(), 0);
    }

    /// Text is the one thing the renderer's transform cannot move: the backend places
    /// glyphs at its own scale and never sees it, so drawing an island onto a layer has to
    /// move the text by hand. Miss that and the island keeps its shapes and loses its
    /// words, which is invisible to any test that only counts covered pixels - the tile
    /// behind the text is covered either way.
    #[test]
    fn text_on_a_faded_island_lands_where_it_would_on_the_bar() {
        let alpha = 0.8;
        for scale in [1.0, 2.0] {
            let faded = shot(&island(alpha), scale);

            // The middle of the first module's text, which the fixture puts well along the
            // bar - far enough that a layer-local coordinate misses the surface entirely.
            let module = &island(alpha).groups[0].modules[0];
            let ty = module.y + (module.height - BLOCK) / 2.0;
            let at = |x: f32, y: f32| {
                let i = (y * scale) as usize * faded.width() as usize + (x * scale) as usize;
                faded.pixels()[i]
            };
            let ink = at(module.text_x + BLOCK / 2.0, ty + BLOCK / 2.0);

            let want = ((0xff * 204 + 127) / 255) as u8;
            assert_eq!(
                (ink.red(), ink.alpha()),
                (want, want),
                "scale {scale}: the middle of the text is {:#x} on {:#x}, not the ink \
                 colour faded to {want:#x} - the words did not reach the bar",
                ink.red(),
                ink.alpha()
            );
        }
    }

    /// And in the same colours: fading is one multiply over the finished island, so every
    /// pixel of it is the opaque pixel scaled, and nothing inside was composited twice.
    #[test]
    fn fading_an_island_only_scales_what_was_already_there() {
        let alpha = 0.8;
        let scaled = |v: u8| ((v as u32 * (alpha * 255.0) as u32 + 127) / 255) as u8;

        for scale in [1.0, 2.0] {
            let plain = shot(&island(1.0), scale);
            let faded = shot(&island(alpha), scale);

            for (i, (want, got)) in plain.pixels().iter().zip(faded.pixels()).enumerate() {
                let expected = (
                    scaled(want.red()),
                    scaled(want.green()),
                    scaled(want.blue()),
                    scaled(want.alpha()),
                );
                let actual = (got.red(), got.green(), got.blue(), got.alpha());
                assert_eq!(
                    actual,
                    expected,
                    "scale {scale}, pixel {} of {}: {actual:?} where the opaque island \
                     scaled to {expected:?}",
                    i,
                    plain.pixels().len()
                );
            }
        }
    }

    /// A module with a background of its own sits right in the group's rounded corner, and
    /// must stop where the group's outline does. It gets the group's radius rather than a
    /// clip mask, so this is what proves the geometry replaced the mask correctly: a square
    /// corner here would be an opaque pixel out past the curve.
    #[test]
    fn a_filled_module_does_not_square_off_a_rounded_group() {
        for scale in [1.0, 2.0] {
            let frame = island(1.0);
            let group = &frame.groups[0];
            let (gx, gy) = (group.x, group.y);
            let shot = shot(&frame, scale);
            let at = |x: f32, y: f32| {
                let i = (y * scale) as usize * shot.width() as usize + (x * scale) as usize;
                shot.pixels()[i]
            };

            // The very corner of the group's bounding box, which the curve cuts away.
            let corner = at(gx, gy);
            assert!(
                corner.alpha() < 40,
                "scale {scale}: the group's top-left corner is {:#x} opaque, so a module \
                 painted straight through the rounded edge",
                corner.alpha()
            );
            // Well inside the same module, to be sure it drew at all.
            let inside = at(gx + 20.0, gy + 10.0);
            assert_eq!(
                inside.alpha(),
                0xff,
                "scale {scale}: the module itself did not draw"
            );
        }
    }

    /// A cached icon has to look like the one drawn straight. It is blended by hand rather
    /// than by tiny-skia, so the two round differently in the last bit; anything more than
    /// that would be the art moving, which is what this guards against.
    #[test]
    fn a_cached_icon_matches_the_one_drawn_straight() {
        use crate::icon::Icon;
        for size in [16.0f32, 20.0, 24.5] {
            for (fx, fy) in [(0.0f32, 0.0f32), (0.5, 0.25), (0.37, 0.81)] {
                // The battery is here because it is the one icon wider than its height,
                // so a rasterised box sized as a square would lose its cap.
                for what in [
                    Icon::Cpu,
                    Icon::Headphones,
                    Icon::Wifi,
                    Icon::Clock,
                    Icon::Battery,
                    Icon::BatteryCharging,
                ] {
                    let placed = PlacedIcon {
                        art: None,
                        icon: what,
                        level: 2,
                        x: 4.0 + fx,
                        y: 3.0 + fy,
                        size,
                    };
                    let colour = Color::rgba(0xeb, 0xdb, 0xb2, 0xff);
                    let mut direct = Pixmap::new(64, 64).unwrap();
                    let mut cached = Pixmap::new(64, 64).unwrap();
                    draw_icon(
                        &mut direct.as_mut(),
                        &placed,
                        colour,
                        Transform::identity(),
                        None,
                    );
                    let mut cache = IconCache::new();
                    draw_icon_cached(
                        &mut cached.as_mut(),
                        &placed,
                        colour,
                        Transform::identity(),
                        Cut::default(),
                        &mut cache,
                    );
                    // Again, onto its own ground, so a hit is checked as well as a miss.
                    let mut again = Pixmap::new(64, 64).unwrap();
                    draw_icon_cached(
                        &mut again.as_mut(),
                        &placed,
                        colour,
                        Transform::identity(),
                        Cut::default(),
                        &mut cache,
                    );
                    assert_eq!(cached.data(), again.data(), "a cache hit drew differently");

                    let worst = direct
                        .data()
                        .iter()
                        .zip(cached.data())
                        .map(|(a, b)| a.abs_diff(*b))
                        .max()
                        .unwrap_or(0);
                    assert!(
                        worst <= 1,
                        "{what:?} at size {size}, offset ({fx}, {fy}): a channel differed \
                         by {worst}, which is more than rounding"
                    );
                }
            }
        }
    }

    /// A fold lands its icons on a different fraction of a pixel every frame, so the keys
    /// it makes are used once and never asked for again. The bar's own icons are asked for
    /// on every redraw, and emptying the cache to make room took them out with the rest.
    #[test]
    fn a_folds_worth_of_throwaway_icons_does_not_cost_the_bar_its_own() {
        use crate::icon::Icon;
        let draw = || rasterise_icon(Icon::Cpu, 0, 16.0, 0.0, 0.0);
        let settled = IconKey {
            icon: Icon::Cpu,
            level: 0,
            size: 16f32.to_bits(),
            offset: (0, 0),
        };
        // Whether a key was already here, which is what the cache never says out loud: it
        // hands back a run either way, so a test asking after eviction has to watch for the
        // rasteriser being called instead.
        let mut cache = IconCache::new();
        let took = |cache: &mut IconCache, key: IconKey| {
            let mut drawn = false;
            let got = cache
                .run(key, || {
                    drawn = true;
                    draw()
                })
                .is_some();
            assert!(got, "the rasteriser refused an icon it has already drawn");
            drawn
        };
        assert!(took(&mut cache, settled), "the first ask is a miss");

        // Three folds' worth of positions nothing will ask for twice, with the bar drawing
        // its own icon on every frame in between.
        for frame in 0..(ICONS_KEPT * 3) {
            assert!(
                !took(&mut cache, settled),
                "the bar's icon went at frame {frame}"
            );
            took(
                &mut cache,
                IconKey {
                    offset: (frame as u32 + 1, 0),
                    ..settled
                },
            );
        }
        assert!(!took(&mut cache, settled), "the bar's icon went at the end");
        assert!(cache.runs.len() <= ICONS_KEPT, "the bound stopped holding");

        // And what nobody has drawn for a while really is the thing that goes: the very
        // first throwaway key cannot have survived three times its own capacity.
        let first = IconKey {
            offset: (1, 0),
            ..settled
        };
        assert!(took(&mut cache, first), "nothing was ever evicted");
    }

    /// The icon cache used to be skipped whenever a mask was present, which meant a
    /// folding island - the only thing on the bar that animates - re-rasterised every icon
    /// on every frame. It goes through the cache now, so the cache has to honour the mask
    /// itself rather than leaning on tiny-skia's clip.
    #[test]
    fn a_cached_icon_is_cut_by_the_mask_it_is_given() {
        use crate::icon::Icon;
        let placed = PlacedIcon {
            icon: Icon::Cpu,
            level: 0,
            x: 8.0,
            y: 8.0,
            size: 24.0,
            art: None,
        };
        let colour = Color::parse("#ebdbb2").unwrap();
        let draw = |cut: Cut<'_>| {
            let mut pixmap = Pixmap::new(64, 64).unwrap();
            let mut cache = IconCache::new();
            draw_icon_cached(
                &mut pixmap.as_mut(),
                &placed,
                colour,
                Transform::identity(),
                cut,
                &mut cache,
            );
            pixmap
        };
        let bounds = (0, 0, 64, 64);
        let mut slot = None;
        let open = draw(Cut::default());
        assert!(open.data().iter().any(|&v| v != 0), "nothing was drawn");

        let all = PathBuilder::from_rect(Rect::from_xywh(0.0, 0.0, 64.0, 64.0).unwrap());
        let mask = clip_mask(
            &mut slot,
            (64, 64),
            bounds,
            &all,
            FillRule::Winding,
            None,
            Transform::identity(),
        )
        .unwrap();
        let covered = draw(Cut {
            mask: Some(mask),
            bounds,
            stop: None,
            shift: 0,
        });
        assert_eq!(open.data(), covered.data(), "a mask over everything cut it");

        let none = PathBuilder::from_rect(Rect::from_xywh(0.0, 0.0, 1.0, 1.0).unwrap());
        let mut slot = None;
        let mask = clip_mask(
            &mut slot,
            (64, 64),
            bounds,
            &none,
            FillRule::Winding,
            None,
            Transform::identity(),
        )
        .unwrap();
        let cut = draw(Cut {
            mask: Some(mask),
            bounds,
            stop: None,
            shift: 0,
        });
        assert!(
            cut.data().iter().all(|&v| v == 0),
            "a mask covering nothing let the icon through"
        );
    }

    /// An island that runs off the end of the bar keeps the part that fits. The layer is
    /// clamped to the surface, so this is where an off-by-one turns into a panic.
    #[test]
    fn an_island_hanging_off_the_edge_still_draws() {
        for x in [-40.0, 440.0, 479.0] {
            let mut frame = island(0.8);
            let shift = x - frame.groups[0].x;
            let group = &mut frame.groups[0];
            group.x += shift;
            for m in &mut group.modules {
                m.x += shift;
                m.text_x += shift;
                if let Some(icon) = &mut m.icon {
                    icon.x += shift;
                }
            }
            group.separators[0].x += shift;
            shot(&frame, 1.0);
        }
    }

    /// A fold is a travel between two settled frames, so the frame it leaves from has to
    /// be one of them. It is not enough for the width to match: the island's contents are
    /// cut to its outline while a fold is on, and a square module corner inset into a
    /// round island sits outside that outline. Cutting on the frame the click lands on
    /// takes those corners off for one frame and puts them back on the next.
    #[test]
    fn the_frame_a_fold_leaves_from_is_the_open_one_pixel_for_pixel() {
        use crate::{
            collect::Registry,
            layout::Inputs,
            status::{Fields, StatusItem, Value},
        };
        let config = "\
[bar]
height = 34
icon_size = 17
[right]
groups = ['s']
[group.s]
modules = ['cpu', 'memory']
collapsible = true
collapse_button = 'right'
radius = 16
padding = 2
spacing = 2
background = '#313244'
collapsed = { icon = 'cpu', padding = 6 }
edges = { left = 'round', right = 'round' }
[module.cpu]
format = '$text'
padding = 6
icon = 'cpu'
background = '#cc241d'
[module.memory]
format = '$text'
padding = 6
icon = 'memory'
background = '#458588'
";
        let cfg = Config::parse(config).unwrap();
        let items: Vec<_> = [("cpu", "13%"), ("memory", "66%")]
            .into_iter()
            .map(|(name, text)| {
                let mut fields = Fields::default();
                fields.set("text", Value::Text(text.into()));
                StatusItem {
                    id: Some(name.into()),
                    fields,
                    state: Default::default(),
                    urgent: false,
                    foreground: None,
                    background: None,
                    action: None,
                }
            })
            .collect();
        let native = Registry::new(&Default::default());
        let frame = |folding: &std::collections::HashMap<String, f32>| {
            let inputs = Inputs {
                items: &items,
                native: &native,
                sway: &Default::default(),
                alt: &Default::default(),
                pages: &Default::default(),
                collapsed_groups: &Default::default(),
                collapsed: &Default::default(),
                switching: &Default::default(),
                folding,
                waiting: &Default::default(),
                spin: 0,
                tray: &Default::default(),
                output: None,
            };
            crate::layout::compute(
                &cfg,
                &inputs,
                480.0,
                34.0,
                &mut Blocks {
                    scale: 1.0,
                    run: None,
                },
                None,
            )
        };
        let open = frame(&Default::default());
        let leaving = frame(&[("s".to_string(), 0.0)].into());
        for scale in [1.0, 2.0] {
            assert_eq!(
                shot(&open, scale).data(),
                shot(&leaving, scale).data(),
                "the first frame of a fold is not the open one at scale {scale}"
            );
        }
        assert!(leaving.groups[0].content_right.is_none());

        // And once it is moving the contents really are cut, which is the other half of
        // the same rule: a test that only ever saw them uncut would pass on a fold that
        // had stopped folding.
        let moving = frame(&[("s".to_string(), 0.4)].into());
        assert!(moving.groups[0].content_right.is_some());
        assert_ne!(shot(&open, 1.0).data(), shot(&moving, 1.0).data());
    }

    /// And the frame it arrives on is the shut one, by the same measure. Geometry alone
    /// does not say so: the island's own rectangle converges long before what is inside it
    /// does, so a test comparing widths and an icon position passes on a frame that still
    /// holds a module box of the wrong size and the first characters of a reading.
    ///
    /// The fixture is deliberately awkward on both counts. The collapsed padding is wider
    /// than the module's icon gap, which is what leaves text standing in room the settled
    /// frame draws nothing in; and it is wider than the module's own padding, which is what
    /// leaves the ground short of where the settled frame fills from. The group's own
    /// background is see-through so the second one shows as a hole rather than a colour.
    ///
    /// It names one icon for both, because that is the one difference the fold is allowed:
    /// `examples/gruvbox-islands.toml` folds three readings down to Tux on purpose, and a
    /// glyph cannot blend into another glyph. Everything except the icon has to agree.
    #[test]
    fn the_frame_a_fold_arrives_on_is_the_shut_one_pixel_for_pixel() {
        use crate::{
            collect::Registry,
            layout::Inputs,
            status::{Fields, StatusItem, Value},
        };
        let config = "\
[bar]
height = 24
icon_size = 12
[left]
groups = ['s']
[group.s]
modules = ['cpu', 'memory']
collapsible = true
collapse_button = 'right'
radius = 8
padding = 0
spacing = 2
background = '#00000000'
collapsed = { icon = 'cpu', padding = 12, background = '#83a598', foreground = '#282828' }
edges = { left = 'round', right = 'round' }
ends = { left = 'none', right = 'slant', width = 12 }
[module.cpu]
format = '$text'
padding = 0
icon_gap = 3
icon = 'cpu'
background = '#cc241d'
foreground = '#ebdbb2'
[module.memory]
format = '$text'
padding = 0
icon_gap = 3
icon = 'memory'
background = '#458588'
";
        let cfg = Config::parse(config).unwrap();
        // Long and short, because they fail differently. A long reading fills the shut
        // island and over-runs it; a short one does not reach its far side, and the second
        // module used to be sitting inside the edge on the frame the fold arrives, showing
        // its own colour where the settled frame has the collapsed ground.
        for reading in ["88888888", "1"] {
            let items: Vec<_> = [("cpu", reading), ("memory", "66%")]
                .into_iter()
                .map(|(name, text)| {
                    let mut fields = Fields::default();
                    fields.set("text", Value::Text(text.into()));
                    StatusItem {
                        id: Some(name.into()),
                        fields,
                        state: Default::default(),
                        urgent: false,
                        foreground: None,
                        background: None,
                        action: None,
                    }
                })
                .collect();
            let native = Registry::new(&Default::default());
            let frame = |folding: &std::collections::HashMap<String, f32>,
                         shut: &std::collections::HashSet<String>| {
                let inputs = Inputs {
                    items: &items,
                    native: &native,
                    sway: &Default::default(),
                    alt: &Default::default(),
                    pages: &Default::default(),
                    collapsed_groups: shut,
                    collapsed: &Default::default(),
                    switching: &Default::default(),
                    folding,
                    waiting: &Default::default(),
                    spin: 0,
                    tray: &Default::default(),
                    output: None,
                };
                crate::layout::compute(
                    &cfg,
                    &inputs,
                    200.0,
                    24.0,
                    &mut Blocks {
                        scale: 1.0,
                        run: None,
                    },
                    None,
                )
            };
            let settled = frame(&Default::default(), &["s".to_string()].into());
            let arriving = frame(&[("s".to_string(), 1.0)].into(), &Default::default());

            // The fixture has to be one the old geometry-only check would have waved through,
            // or this proves nothing: same island, and the icon landed, but the insides are
            // still the open ones.
            let (a, b) = (&settled.groups[0], &arriving.groups[0]);
            assert!((a.x - b.x).abs() < 0.001 && (a.width - b.width).abs() < 0.001);
            assert!(
                b.modules.len() > a.modules.len(),
                "the fixture must arrive still holding what the shut island does not"
            );

            for scale in [1.0, 2.0] {
                assert_eq!(
                    shot(&settled, scale).data(),
                    shot(&arriving, scale).data(),
                    "the last frame of a fold is not the shut one, \
                 reading {reading:?} at scale {scale}"
                );
            }
        }
    }

    /// A module at the island's edge wears the island's corner. Which module that is comes
    /// of comparing where the fill stops against where the island does, and a fill's own
    /// edge is put on a whole device pixel: an island ending a fraction of one further
    /// along has no module reaching it at all, and every rounded right corner comes out
    /// square.
    #[test]
    fn a_rounded_corner_survives_an_island_that_ends_between_pixels() {
        use crate::{
            collect::Registry,
            layout::Inputs,
            status::{Fields, StatusItem, Value},
        };
        let cfg = Config::parse(
            "[bar]\nheight = 20\n[left]\ngroups = ['g']\n[group.g]\nmodules = ['a']\n\
             radius = 6\npadding = 0\nbackground = '#00000000'\n\
             [module.a]\nbackground = '#cc241d'\npadding = 2.1\nformat = '$text'\n",
        )
        .unwrap();
        let mut fields = Fields::default();
        fields.set("text", Value::Text("reading".into()));
        let items = [StatusItem {
            id: Some("a".into()),
            fields,
            state: Default::default(),
            urgent: false,
            foreground: None,
            background: None,
            action: None,
        }];
        let native = Registry::new(&Default::default());
        let inputs = Inputs {
            items: &items,
            native: &native,
            sway: &Default::default(),
            alt: &Default::default(),
            pages: &Default::default(),
            collapsed_groups: &Default::default(),
            collapsed: &Default::default(),
            switching: &Default::default(),
            folding: &Default::default(),
            waiting: &Default::default(),
            spin: 0,
            tray: &Default::default(),
            output: None,
        };
        let frame = crate::layout::compute(
            &cfg,
            &inputs,
            480.0,
            20.0,
            &mut Blocks {
                scale: 1.0,
                run: None,
            },
            None,
        );
        let island = &frame.groups[0];
        let right = island.x + island.width;
        assert!(
            (right - right.round()).abs() > 0.1,
            "the fixture must end between pixels, not at {right}"
        );
        for scale in [1.0, 1.5, 2.0] {
            let shot = shot(&frame, scale);
            // The corner's own pixel: solid means the arc was never cut.
            let x = (right * scale).floor() as u32 - 1;
            let pixel = shot.pixels()[x as usize];
            assert!(
                pixel.alpha() < 200,
                "a square corner at scale {scale}: alpha {}",
                pixel.alpha()
            );
        }
    }

    /// The end a fold cuts with leans, and the frame it arrives on cuts nothing at all: the
    /// island it lands on holds one module, and the wording and marks behind that one have
    /// to be gone rather than standing in the lean of the cap.
    ///
    /// The fixture gives the cap more width than the collapsed style gives its icon room,
    /// because the room is what the wording is already held back by. Where the two are
    /// equal the lean is paid for by accident, and a cut that never straightened would pass.
    #[test]
    fn a_fold_arrives_with_nothing_standing_in_the_lean_of_its_cap() {
        use crate::{
            collect::Registry,
            layout::Inputs,
            status::{Fields, StatusItem, Value},
        };
        let cfg = Config::parse(
            "[bar]\nheight = 24\nicon_size = 12\n[left]\ngroups = ['s']\n[group.s]\n\
             modules = ['cpu', 'memory']\ncollapsible = true\ncollapse_button = 'right'\n\
             radius = 0\npadding = 0\nspacing = 2\nbackground = '#00000000'\n\
             ends = { left = 'none', right = 'slant', width = 12 }\n\
             collapsed = { icon = 'cpu', padding = 0, background = '#83a598' }\n\
             [module.cpu]\nformat = '$text'\npadding = 0\nicon = 'cpu'\n\
             background = '#cc241d'\nforeground = '#ebdbb2'\n\
             [module.memory]\nformat = '$text'\npadding = 0\nicon = 'memory'\n\
             background = '#458588'\n",
        )
        .unwrap();
        let items: Vec<_> = [("cpu", "88888888"), ("memory", "66%")]
            .into_iter()
            .map(|(name, text)| {
                let mut fields = Fields::default();
                fields.set("text", Value::Text(text.into()));
                StatusItem {
                    id: Some(name.into()),
                    fields,
                    state: Default::default(),
                    urgent: false,
                    foreground: None,
                    background: None,
                    action: None,
                }
            })
            .collect();
        let native = Registry::new(&Default::default());
        let frame = |folding: &std::collections::HashMap<String, f32>,
                     shut: &std::collections::HashSet<String>| {
            let inputs = Inputs {
                items: &items,
                native: &native,
                sway: &Default::default(),
                alt: &Default::default(),
                pages: &Default::default(),
                collapsed_groups: shut,
                collapsed: &Default::default(),
                switching: &Default::default(),
                folding,
                waiting: &Default::default(),
                spin: 0,
                tray: &Default::default(),
                output: None,
            };
            crate::layout::compute(
                &cfg,
                &inputs,
                200.0,
                24.0,
                &mut Blocks {
                    scale: 1.0,
                    run: None,
                },
                None,
            )
        };
        let settled = frame(&Default::default(), &["s".to_string()].into());
        let arriving = frame(&[("s".to_string(), 1.0)].into(), &Default::default());
        let (a, b) = (&settled.groups[0], &arriving.groups[0]);
        assert!((a.x - b.x).abs() < 0.001 && (a.width - b.width).abs() < 0.001);
        assert!(
            b.modules.len() > a.modules.len(),
            "the fixture must arrive still holding what the shut island does not"
        );
        let cap = (b.separators.iter())
            .find(|s| s.cap)
            .expect("a trailing cap");
        let room = (b.content_right.zip(b.text_right)).map_or(0.0, |(right, text)| right - text);
        assert!(
            cap.width > room,
            "the cap must lean further than the room the wording is already held back by"
        );
        for scale in [1.0, 2.0] {
            assert_eq!(
                shot(&settled, scale).data(),
                shot(&arriving, scale).data(),
                "the last frame of a fold stood in the lean of its cap at scale {scale}"
            );
        }
    }

    /// An end cap bleeds past itself to hide the seam between two antialiased edges, and
    /// it is drawn at the very edge of its island. A group with square edges has no clip
    /// mask to catch that - one is only built for a rounded edge - so with an overlap wider
    /// than the group's padding the cap really does paint outside the group's rectangle.
    /// Damage that named the rectangle left those pixels stale on every colour change.
    #[test]
    fn a_square_group_damages_the_pixels_its_end_caps_reach() {
        use crate::{
            collect::Registry,
            layout::{Damage, Inputs},
            status::{Fields, StatusItem, Value},
        };
        let config = "\
[left]
groups = ['a']
[group.a]
modules = ['a']
padding = 0
background = '#3c3836'
ends = { left = 'slant', right = 'slant', width = 6, overlap = 6 }
[module.a]
padding = 12
format = '$text'
";
        let cfg = Config::parse(config).unwrap();
        let item = |color: &str| {
            let mut fields = Fields::default();
            fields.set("text", Value::Text("example".into()));
            vec![StatusItem {
                id: Some("a".into()),
                fields,
                state: Default::default(),
                urgent: false,
                foreground: None,
                background: Some(Color::parse(color).unwrap()),
                action: None,
            }]
        };
        let native = Registry::new(&Default::default());
        let frame = |items: &[StatusItem]| {
            let inputs = Inputs {
                items,
                native: &native,
                sway: &Default::default(),
                alt: &Default::default(),
                pages: &Default::default(),
                collapsed: &Default::default(),
                collapsed_groups: &Default::default(),
                switching: &Default::default(),
                folding: &Default::default(),
                waiting: &Default::default(),
                spin: 0,
                tray: &Default::default(),
                output: None,
            };
            crate::layout::compute(
                &cfg,
                &inputs,
                480.0,
                20.0,
                &mut Blocks {
                    scale: 1.0,
                    run: None,
                },
                None,
            )
        };
        let red = frame(&item("#cc241d"));
        let blue = frame(&item("#458588"));
        // The caps are outside the rectangle, which is the whole point of the test.
        let group = &red.groups[0];
        let (bx, _, bw, _) = group.paint_bounds();
        assert!(bx < group.x && bx + bw > group.x + group.width);

        for scale in [1.0, 2.0] {
            let before = shot(&red, scale);
            let after = shot(&blue, scale);
            assert_ne!(before.data(), after.data());
            let Damage::Rects(rects) = blue.damage(&red) else {
                panic!("only the colour changed")
            };
            for (n, (a, b)) in before.pixels().iter().zip(after.pixels()).enumerate() {
                if a == b {
                    continue;
                }
                let (x, y) = (
                    (n as u32 % before.width()) as f32,
                    (n as u32 / before.width()) as f32,
                );
                assert!(
                    rects
                        .iter()
                        .any(|(rx, ry, rw, rh)| x >= (rx * scale).floor()
                            && x < ((rx + rw) * scale).ceil()
                            && y >= (ry * scale).floor()
                            && y < ((ry + rh) * scale).ceil()),
                    "undamaged pixel {x},{y} at scale {scale}"
                );
            }
        }
    }

    /// Twelve blocks, either one ribbon or four independently configured groups.
    fn ribbon_config(joined: bool, shape: &str, direction: &str, color: &str) -> Config {
        let separator = format!(
            "{{ shape = '{shape}', width = 6, direction = '{direction}', color = '{color}', overlap = 1 }}"
        );
        let names: Vec<_> = (0..12).map(|n| format!("\"m{n}\"")).collect();
        let mut config =
            "[bar]\nheight = 20\ngap = 19\nbackground = { color = '#282828' }\n".to_string();
        if joined {
            config +=
                &format!("[right]\ngroups = ['g0','g1','g2','g3']\nseparator = {separator}\n");
            for (n, chunk) in names.chunks(3).enumerate() {
                config += &format!(
                    "[group.g{n}]\nmodules = [{}]\nbackground = '#282828'\nradius = 5\nseparator = {separator}\n",
                    chunk.join(",")
                );
            }
        } else {
            config += &format!(
                "[right]\ngroups = ['g']\n[group.g]\nmodules = [{}]\nbackground = '#282828'\nradius = 5\nseparator = {separator}\n",
                names.join(",")
            );
        }
        for n in 0..12 {
            let background = if n % 2 == 0 { "#3c3836" } else { "#504945" };
            config += &format!(
                "[module.m{n}]\nformat = '$text'\npadding = 3\nbackground = '{background}'\nforeground = '#ebdbb2'\n"
            );
        }
        config += "[module.m3.states.hover]\nhover = true\nbackground = '#ffee00'\nforeground = '#000000'\n";
        config += "[module.m8.states.warning]\nstate = 'warning'\nbackground = '#ff7700'\nforeground = '#000000'\n";
        Config::parse(&config).unwrap()
    }

    fn ribbon_items() -> Vec<crate::status::StatusItem> {
        use crate::status::{Fields, StatusItem, Value};
        (0..12)
            .map(|n| {
                let mut fields = Fields::default();
                fields.set("text", Value::Text(format!("{n:02}")));
                StatusItem {
                    id: Some(format!("m{n}")),
                    fields,
                    state: Default::default(),
                    urgent: false,
                    foreground: None,
                    background: None,
                    action: None,
                }
            })
            .collect()
    }

    fn ribbon_frame(
        cfg: &Config,
        items: &[crate::status::StatusItem],
        text: &mut impl crate::layout::Measure,
        width: f32,
        pointer: Option<(f32, f32)>,
    ) -> Frame {
        let inputs = crate::layout::Inputs {
            items,
            native: &crate::collect::Registry::new(&Default::default()),
            sway: &Default::default(),
            alt: &Default::default(),
            pages: &Default::default(),
            collapsed_groups: &Default::default(),
            switching: &Default::default(),
            folding: &Default::default(),
            collapsed: &Default::default(),
            waiting: &Default::default(),
            spin: 0,
            tray: &Default::default(),
            output: None,
        };
        crate::layout::compute(cfg, &inputs, width, 20.0, text, pointer)
    }

    #[test]
    fn joined_groups_paint_exactly_like_one_ribbon() {
        for shape in ["line", "slant", "chevron", "notch", "round", "curve"] {
            for direction in ["left", "right"] {
                for color in ["previous", "next", "foreground", "background", "#83a598"] {
                    let merged = ribbon_config(false, shape, direction, color);
                    let joined = ribbon_config(true, shape, direction, color);
                    for mode in 0..3 {
                        let mut items = ribbon_items();
                        items[8].state = crate::status::State::Warning;
                        if mode == 2 {
                            for item in &mut items[3..6] {
                                item.fields
                                    .set("text", crate::status::Value::Text(String::new()));
                            }
                        }
                        let mut text = Blocks {
                            scale: 1.0,
                            run: None,
                        };
                        let reference = ribbon_frame(&merged, &items, &mut text, 480.0, None);
                        let pointer = if mode == 1 {
                            Some((reference.groups[0].modules[3].x + 1.0, 10.0))
                        } else {
                            None
                        };
                        let reference = ribbon_frame(&merged, &items, &mut text, 480.0, pointer);
                        let actual = ribbon_frame(&joined, &items, &mut text, 480.0, pointer);
                        assert_eq!(actual.groups.len(), if mode == 2 { 3 } else { 4 });
                        for scale in [1.0, 1.5, 2.0] {
                            assert!(
                                shot(&reference, scale).data() == shot(&actual, scale).data(),
                                "{shape} {direction} {color}, mode {mode}, scale {scale}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    #[ignore = "release timing probe; run with --release --ignored --nocapture"]
    fn benchmark_joined_ribbon() {
        use crate::status::Value;
        use std::time::{Duration, Instant};
        let mut painter =
            Painter::new(crate::text::TextRenderer::new("sans-serif", 13.0, &[]).unwrap());
        let mut items = ribbon_items();
        for (case, radius, background) in [
            ("square", 0.0, Color::TRANSPARENT),
            ("rounded", 5.0, Color::parse("#282828").unwrap()),
        ] {
            let mut configs = [
                ribbon_config(false, "slant", "right", "previous"),
                ribbon_config(true, "slant", "right", "previous"),
            ];
            for cfg in &mut configs {
                for group in cfg.positions.iter_mut().flat_map(|p| &mut p.groups) {
                    group.edges.radius = radius;
                    group.background = background;
                }
            }
            for width in [1920u32, 3840] {
                for scale in [1.0, 2.0] {
                    painter.text.set_scale(scale);
                    let (pw, ph) = ((width as f32 * scale) as u32, (20.0 * scale) as u32);
                    let mut pixels = vec![0; pw as usize * ph as usize * 4];
                    let mut clip = Clip::default();
                    let mut times = [(Duration::ZERO, Duration::ZERO); 2];
                    // Interleave both paths to reduce drift from cache warming and CPU frequency.
                    for n in 0..1200 {
                        items[3]
                            .fields
                            .set("text", Value::Text(format!("CPU {}%", n % 100)));
                        for index in [n % 2, 1 - n % 2] {
                            let start = Instant::now();
                            let frame = ribbon_frame(
                                &configs[index],
                                &items,
                                &mut painter.text,
                                width as f32,
                                None,
                            );
                            let layout = start.elapsed();
                            let start = Instant::now();
                            render_to_buffer(
                                Target {
                                    canvas: &mut pixels,
                                    width: pw,
                                    height: ph,
                                    clip: &mut clip,
                                    pixels: Pixels::AsWritten,
                                },
                                &frame,
                                scale,
                                &mut painter,
                            )
                            .unwrap();
                            let paint = start.elapsed();
                            if n >= 200 {
                                times[index].0 += layout;
                                times[index].1 += paint;
                            }
                        }
                    }
                    for (index, (layout, paint)) in times.into_iter().enumerate() {
                        println!(
                            "{case} width={width} scale={scale} {} layout={:.2}us paint={:.2}us",
                            if index == 0 { "merged" } else { "joined" },
                            layout.as_secs_f64() * 1000.0,
                            paint.as_secs_f64() * 1000.0
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn slanted_caps_follow_the_requested_slope_and_stay_attached() {
        for direction in ["left", "right"] {
            for leading in [false, true] {
                let (left, right) = if leading {
                    ("slant", "none")
                } else {
                    ("none", "slant")
                };
                let cfg = Config::parse(&format!(
                    r##"
[bar]
height = 20
[right]
groups = ["g"]
[group.g]
modules = ["m0"]
ends = {{ left = "{left}", right = "{right}", direction = "{direction}", width = 12 }}
[module.m0]
format = "$text"
padding = 0
min_width = 24
background = "#83a598"
"##
                ))
                .unwrap();
                let frame = ribbon_frame(
                    &cfg,
                    &ribbon_items(),
                    &mut Blocks {
                        scale: 1.0,
                        run: None,
                    },
                    480.0,
                    None,
                );
                let cap = &frame.groups[0].separators[0];
                for scale in [1.0, 2.0] {
                    let image = shot(&frame, scale);
                    let alpha = |x: f32, y: f32| {
                        image
                            .pixel(((cap.x + x) * scale) as u32, (y * scale) as u32)
                            .unwrap()
                            .alpha()
                    };
                    let top_filled = leading == (direction == "left");
                    assert_eq!(
                        alpha(3.0, 3.0),
                        if top_filled { 255 } else { 0 },
                        "{leading} {direction}"
                    );
                    assert_eq!(
                        alpha(3.0, 16.0),
                        if top_filled { 0 } else { 255 },
                        "{leading} {direction}"
                    );
                    // Both ends of the edge touching the module are solid: no detached wedge.
                    let touching = if leading { 11.0 } else { 0.0 };
                    assert_eq!(alpha(touching, 5.0), 255);
                    assert_eq!(alpha(touching, 14.0), 255);
                }
            }
        }
    }
    fn fold_ribbon(at: f32, joined: bool) -> Frame {
        use crate::{
            collect::Registry,
            config::Source,
            layout::Inputs,
            status::{Fields, State, StatusItem, Value},
        };
        let mut cfg = Config::parse(include_str!("../examples/gruvbox-ribbon.toml")).unwrap();
        cfg.positions[0].groups.clear();
        cfg.positions[1].groups.clear();
        cfg.positions[2]
            .groups
            .retain(|group| group.name == "system" || (joined && group.name == "connections"));
        if !joined {
            cfg.positions[2].separator = None;
        }
        // This fixture folds the real example, so it is worth saying what it expected of
        // it: an edit that renames one of these would otherwise show up as an index out
        // of range somewhere further down, with nothing to say which file moved.
        assert_eq!(
            cfg.positions[2].groups.len(),
            1 + usize::from(joined),
            "examples/gruvbox-ribbon.toml no longer has the groups this folds"
        );
        let system: Vec<_> = (cfg.positions[2].groups[0].modules.iter())
            .map(|module| module.name.as_str())
            .collect();
        assert_eq!(
            system,
            ["cpu", "memory", "temperature"],
            "examples/gruvbox-ribbon.toml's system group is not the one this folds"
        );
        for group in &mut cfg.positions[2].groups {
            if group.name == "connections" {
                group.modules.retain(|module| module.name == "language");
            }
            for module in &mut group.modules {
                module.source = Source::Provider;
                module.format = crate::format::Format::parse("$text").unwrap();
            }
        }
        let items: Vec<_> = [
            ("cpu", "13%"),
            ("memory", "66%"),
            ("temperature", "76°C"),
            ("language", "EN"),
        ]
        .into_iter()
        .map(|(name, text)| {
            let mut fields = Fields::default();
            fields.set("text", Value::Text(text.into()));
            StatusItem {
                id: Some(name.into()),
                fields,
                state: if name == "temperature" {
                    State::Warning
                } else {
                    State::Idle
                },
                urgent: false,
                foreground: None,
                background: None,
                action: None,
            }
        })
        .collect();
        let native = Registry::new(&Default::default());
        let folding = [("system".to_string(), at)].into();
        let inputs = Inputs {
            items: &items,
            native: &native,
            sway: &Default::default(),
            alt: &Default::default(),
            pages: &Default::default(),
            collapsed_groups: &Default::default(),
            collapsed: &Default::default(),
            switching: &Default::default(),
            folding: &folding,
            waiting: &Default::default(),
            spin: 0,
            tray: &Default::default(),
            output: None,
        };
        crate::layout::compute(
            &cfg,
            &inputs,
            480.0,
            30.0,
            &mut Blocks {
                scale: 1.0,
                run: None,
            },
            None,
        )
    }

    #[test]
    fn a_fold_colors_its_trailing_transition_from_visible_content() {
        for joined in [false, true] {
            let frame = fold_ribbon(0.85, joined);
            let group = &frame.groups[0];
            let cpu = &group.modules[0];
            let temperature = group.modules.last().unwrap();
            let right = group.content_right.unwrap();
            assert!(right > cpu.x && right < cpu.x + cpu.width);
            assert_ne!(cpu.background, temperature.background);
            let transition = if joined {
                &frame.group_separators[0]
            } else {
                group.separators.last().unwrap()
            };
            assert_eq!(
                transition.fill, cpu.background,
                "hidden temperature colored the edge; joined={joined}"
            );
        }
    }

    /// The transition at a folding island's end is one colour, and the fold sweeps it over
    /// modules and over the gaps between them alike. Taking it from the module box rather
    /// than from the ground actually under the edge swaps that colour a whole gap early,
    /// which is a band arriving and leaving in one step on a bar that is otherwise
    /// travelling - the edge looks swallowed rather than covered.
    #[test]
    fn a_folding_edge_is_the_colour_of_the_ground_it_stands_on() {
        let scale = 2.0;
        let mut gaps = 0;
        for step in 1..=200 {
            let frame = fold_ribbon(step as f32 / 200.0, true);
            let group = &frame.groups[0];
            let right = group.content_right.expect("the island is folding");
            let join = &frame.group_separators[0];
            gaps += usize::from(
                (group.modules.windows(2))
                    .any(|pair| right > pair[0].x + pair[0].width && right <= pair[1].x),
            );
            // Within a pixel of a boundary the last column rounds to either side of it,
            // and which one it lands on says nothing about the rule under test.
            let boundaries = (group.modules.iter()).flat_map(|m| [m.x, m.x + m.width]);
            if boundaries
                .map(|edge| (right - edge).abs())
                .fold(f32::MAX, f32::min)
                < 1.0
            {
                continue;
            }
            let (dw, dh) = ((480.0 * scale) as u32, (30.0 * scale) as u32);
            let mut pixmap = Pixmap::new(dw, dh).unwrap();
            let mut painter = Painter::new(Blocks { scale, run: None });
            render(
                &mut pixmap.as_mut(),
                &frame,
                scale,
                &mut painter,
                &mut Clip::default(),
            );
            // The bottom row is where the island reaches least far, so the last column
            // before the edge there is island rather than transition however it leans.
            let x = (right * scale).round() as usize - 1;
            let pixel = pixmap.pixels()[(dh as usize - 1) * dw as usize + x];
            assert_eq!(
                (pixel.red(), pixel.green(), pixel.blue()),
                (join.fill.r, join.fill.g, join.fill.b),
                "the edge was coloured from ground it is not standing on at step {step}"
            );
        }
        assert!(
            gaps > 0,
            "no frame put the edge inside a gap, so nothing was tested"
        );
    }

    /// Fills stop where the fold's edge is and the transition drawn there covers them, but
    /// glyphs and icons are cut instead, and a column cut inside a slanted end is the one
    /// straight line left on an angled ribbon.
    #[test]
    fn a_fold_cuts_its_wording_with_the_shape_it_ends_with() {
        let scale = 2.0;
        let mut leaned = 0;
        for step in 1..=99 {
            let frame = fold_ribbon(step as f32 / 100.0, true);
            let group = &frame.groups[0];
            let (dw, dh) = ((480.0 * scale) as u32, (30.0 * scale) as u32);
            let mut pixmap = Pixmap::new(dw, dh).unwrap();
            let mut painter = Painter::new(Blocks { scale, run: None });
            render(
                &mut pixmap.as_mut(),
                &frame,
                scale,
                &mut painter,
                &mut Clip::default(),
            );
            let px = pixmap.pixels();
            let w = dw as usize;
            for module in &group.modules {
                let ink = module.foreground;
                let span = ((module.text_x * scale) as usize)..dw as usize;
                // Icons are drawn in the same colour and are taller than the wording, so
                // the rows are the ones the text backend writes and no others.
                let top = ((module.y + (module.height - BLOCK) / 2.0) * scale).round() as usize;
                let rows = top..(top + (BLOCK * scale) as usize).min(dh as usize);
                // The rightmost written column of this module's wording, row by row. A
                // straight cut leaves every row the same; the island's own end does not.
                let edge = |y: usize| {
                    span.clone().rev().find(|&x| {
                        let p = px[y * w + x];
                        (p.red(), p.green(), p.blue(), p.alpha()) == (ink.r, ink.g, ink.b, 255)
                    })
                };
                let written: Vec<_> = rows.filter_map(edge).collect();
                let (Some(top), Some(bottom)) = (written.first(), written.last()) else {
                    continue;
                };
                assert!(
                    top >= bottom,
                    "the wording leaned against the island's end at step {step}"
                );
                leaned += usize::from(top - bottom >= 3);
            }
        }
        assert!(
            leaned > 0,
            "no frame cut a reading on the slant, so nothing was tested"
        );
    }

    /// What an island keeps past the column its fills stop at is its own end, and the mask
    /// admits it so glyphs and marks can lean with it. A divider out there is furniture of
    /// content the fold has already covered - the mask used to be what stopped it - so it
    /// now paints a gap that is no longer on the bar into the island's own end. Taking one
    /// out of the frame has to change nothing.
    #[test]
    fn a_fold_draws_no_divider_it_has_gone_past() {
        let scale = 2.0;
        let mut past = 0;
        for step in 1..=400 {
            let frame = fold_ribbon(step as f32 / 400.0, true);
            let right = frame.groups[0]
                .content_right
                .expect("the island is folding");
            let covered = |separator: &PlacedSeparator| !separator.cap && separator.x >= right;
            if !frame.groups[0].separators.iter().any(covered) {
                continue;
            }
            past += 1;
            let mut without = frame.clone();
            without.groups[0].separators.retain(|s| !covered(s));
            let shot = |frame: &Frame| {
                let (dw, dh) = ((480.0 * scale) as u32, (30.0 * scale) as u32);
                let mut pixmap = Pixmap::new(dw, dh).unwrap();
                let mut painter = Painter::new(Blocks { scale, run: None });
                render(
                    &mut pixmap.as_mut(),
                    frame,
                    scale,
                    &mut painter,
                    &mut Clip::default(),
                );
                pixmap
            };
            assert_eq!(
                shot(&frame).data(),
                shot(&without).data(),
                "a divider the fold had gone past was drawn at step {step}"
            );
        }
        assert!(past > 0, "no frame of the fold went past a divider");
    }

    #[test]
    fn a_folding_ribbon_meets_its_join_without_a_pixel_seam() {
        for step in 85..=95 {
            let frame = fold_ribbon(step as f32 / 100.0, true);
            let cpu = &frame.groups[0].modules[0];
            let join = &frame.group_separators[0];
            assert!(join.x > cpu.x && join.x < cpu.x + cpu.width);
            for scale in [1.0, 1.5, 2.0] {
                let mut pixmap =
                    Pixmap::new((480.0 * scale) as u32, (30.0 * scale) as u32).unwrap();
                let mut painter = Painter::new(Blocks { scale, run: None });
                render(
                    &mut pixmap.as_mut(),
                    &frame,
                    scale,
                    &mut painter,
                    &mut Clip::default(),
                );
                // The last module column before the join is solid at the top of the
                // ribbon, away from its text and icon. Fractional clipping must not
                // expose a column of bar background here.
                let column = (join.x * scale).round() as usize;
                let x = column
                    .checked_sub(1)
                    .expect("the join has a ribbon to its left");
                let pixel = pixmap.pixels()[pixmap.width() as usize + x];
                assert_eq!(
                    (pixel.red(), pixel.green(), pixel.blue()),
                    (cpu.background.r, cpu.background.g, cpu.background.b),
                    "seam before join at step {step} scale {scale}"
                );
            }
        }
    }

    /// A trailing cap is drawn in room of its own at the island's end, and half of a slant
    /// is the bar showing through. A fold moves that room in over the content, so the
    /// modules have to stop before it: they are drawn after the separators and would
    /// otherwise fill the open side of the cap in, turning a slant into a wall.
    #[test]
    fn a_folding_island_leaves_the_open_side_of_its_cap_alone() {
        use crate::{
            collect::Registry,
            layout::Inputs,
            status::{Fields, StatusItem, Value},
        };
        let cfg = Config::parse(
            "[left]\ngroups = ['a']\n[group.a]\nmodules = ['a', 'a', 'a']\ncollapsible = true\n\
             collapse_button = 'right'\ncollapse_animation = '150ms'\nbackground = '#00000000'\n\
             radius = 0\npadding = 0\nseparator = { shape = 'slant', width = 5, overlap = 1 }\n\
             ends = { left = 'slant', right = 'slant', width = 8, \
             direction = 'right' }\ncollapsed = { icon = 'cpu', icon_size = 8, padding = 2, \
             background = '#83a598' }\n[module.a]\nbackground = '#cc241d'\npadding = 2\n\
             format = '$text'\n",
        )
        .unwrap();
        let mut fields = Fields::default();
        fields.set("text", Value::Text("reading".into()));
        let items = [StatusItem {
            id: Some("a".into()),
            fields,
            state: Default::default(),
            urgent: false,
            foreground: None,
            background: None,
            action: None,
        }];
        let native = Registry::new(&Default::default());
        let frame = |folding: &std::collections::HashMap<String, f32>| {
            let inputs = Inputs {
                items: &items,
                native: &native,
                sway: &Default::default(),
                alt: &Default::default(),
                pages: &Default::default(),
                collapsed: &Default::default(),
                collapsed_groups: &Default::default(),
                switching: &Default::default(),
                folding,
                waiting: &Default::default(),
                spin: 0,
                tray: &Default::default(),
                output: None,
            };
            crate::layout::compute(
                &cfg,
                &inputs,
                480.0,
                12.0,
                &mut Blocks {
                    scale: 1.0,
                    run: None,
                },
                None,
            )
        };
        // What the cap's own strip is made of: how much of it the bar is still showing
        // through, and how much of it the cap actually painted. A slant needs both - one
        // says it did not fill in, the other says it is still there at all.
        let strip = |frame: &Frame| {
            let group = &frame.groups[0];
            let cap = group.separators.last().unwrap();
            let shot = shot(frame, 1.0);
            let width = shot.width() as usize;
            let (x0, x1) = (cap.x.ceil() as usize, (cap.x + cap.width).floor() as usize);
            let rows = group.y as usize..(group.y + group.height) as usize;
            let mut clear = 0;
            let mut painted = 0;
            for y in rows {
                for x in x0..x1 {
                    match shot.pixels()[y * width + x].alpha() {
                        0 => clear += 1,
                        250.. => painted += 1,
                        _ => {}
                    }
                }
            }
            (clear, painted)
        };

        // Open, the slant leaves half its strip to the bar behind it.
        let open = frame(&Default::default());
        assert!(open.groups[0].separators.last().unwrap().width > 0.0);
        let (was_clear, was_painted) = strip(&open);
        assert!(was_clear > 0, "the cap was solid before anything folded");
        assert!(
            was_painted > 0,
            "the cap drew nothing before anything folded"
        );

        // Folding, the cap moves in over the content and has to stay just as open.
        for at in [0.25, 0.5, 0.75] {
            let travelling = frame(&[("a".to_string(), at)].into());
            let group = &travelling.groups[0];
            let cap = group.separators.last().unwrap();
            let last = group.modules.last().unwrap();
            assert!(cap.cap, "the last separator must be the trailing cap");
            if at >= 0.5 {
                assert!(
                    group
                        .separators
                        .iter()
                        .any(|separator| { !separator.cap && separator.x > cap.x + cap.width }),
                    "the fixture must also hide an internal divider at {at}"
                );
            }
            assert!(
                cap.x < last.x + last.width,
                "the cap is not over the content at {at}"
            );
            let (clear, painted) = strip(&travelling);
            assert!(
                clear * 4 >= was_clear * 3,
                "the cap filled in at {at}: {clear} clear against {was_clear} open"
            );
            assert!(
                painted * 4 >= was_painted * 3,
                "the cap went missing at {at}: {painted} painted against {was_painted} open"
            );
        }
    }

    /// A module the travelling edge is part way across is cut to a sliver, and a sliver has
    /// no room for the corner it stands in: `edged_rect` clamps a radius to half the box,
    /// so two pixels of a module at a twelve pixel corner round by one and paint over the
    /// arc they belong inside. Cutting the fills by geometry is what makes the two ends of
    /// a fold agree with the settled frames, and this is the case geometry cannot state -
    /// where it cannot, the mask still can.
    ///
    /// The island has no ends here, on purpose. With a cap the contents stop a cap's width
    /// short of the island and the corner is the cap's business; without one the travelling
    /// edge arrives at the corner itself, which is the only way to reach this.
    #[test]
    fn a_sliver_of_a_module_at_a_rounded_corner_stays_inside_it() {
        use crate::{
            collect::Registry,
            layout::Inputs,
            status::{Fields, StatusItem, Value},
        };
        let radius = 12.0f32;
        let cfg = Config::parse(
            "[bar]\nheight = 24\nicon_size = 12\n[left]\ngroups = ['a']\n[group.a]\n\
             modules = ['a', 'b']\ncollapsible = true\ncollapse_button = 'right'\n\
             collapse_animation = '150ms'\nbackground = '#3c3836'\nradius = 12\npadding = 0\n\
             spacing = 0\ncollapsed = { icon = 'cpu', padding = 4 }\n\
             edges = { left = 'round', right = 'round' }\n\
             [module.a]\nbackground = '#cc241d'\npadding = 4\nformat = '$text'\nicon = 'cpu'\n\
             [module.b]\nbackground = '#458588'\npadding = 4\nformat = '$text'\nicon = 'memory'\n",
        )
        .unwrap();
        let items: Vec<_> = [("a", "aaaaaaaa"), ("b", "bbbbbbbb")]
            .into_iter()
            .map(|(id, text)| {
                let mut fields = Fields::default();
                fields.set("text", Value::Text(text.into()));
                StatusItem {
                    id: Some(id.into()),
                    fields,
                    state: Default::default(),
                    urgent: false,
                    foreground: None,
                    background: None,
                    action: None,
                }
            })
            .collect();
        let native = Registry::new(&Default::default());
        let frame = |folding: &std::collections::HashMap<String, f32>| {
            let inputs = Inputs {
                items: &items,
                native: &native,
                sway: &Default::default(),
                alt: &Default::default(),
                pages: &Default::default(),
                collapsed: &Default::default(),
                collapsed_groups: &Default::default(),
                switching: &Default::default(),
                folding,
                waiting: &Default::default(),
                spin: 0,
                tray: &Default::default(),
                output: None,
            };
            crate::layout::compute(
                &cfg,
                &inputs,
                200.0,
                24.0,
                &mut Blocks {
                    scale: 1.0,
                    run: None,
                },
                None,
            )
        };
        // How far past the island's arc anything visible lies. Not solid pixels only: a
        // fill that escapes through a mask it should have been cut by arrives at part
        // strength, and reading `alpha == 255` would call four columns of a module leaking
        // at forty-five per cent a clean frame. Every outline here is antialiased, so a
        // pixel on the boundary says nothing either way - a leak is measured in whole
        // pixels, and the two this test was written for reached two and a half.
        let mut worst = (0.0f32, 0.0f32);
        for step in 1..100 {
            let at = step as f32 / 100.0;
            let frame = frame(&[("a".to_string(), at)].into());
            let group = &frame.groups[0];
            // The island rounds by what it has room for, the same clamp `edged_rect`
            // applies: near the end of its travel it is narrower than two radii and its
            // ends are semicircles. Measuring against the configured radius there would
            // report the island's own edge as a leak.
            let radius = radius.min(group.width / 2.0).min(group.height / 2.0);
            let shot = shot(&frame, 1.0);
            let width = shot.width();
            for (n, pixel) in shot.pixels().iter().enumerate() {
                if pixel.alpha() < 8 {
                    continue;
                }
                let (px, py) = ((n as u32 % width) as f32, (n as u32 / width) as f32);
                let (x0, x1) = (group.x, group.x + group.width);
                let (y0, y1) = (group.y, group.y + group.height);
                let beyond = px < x0 - 0.5 || px > x1 + 0.5 || py < y0 - 0.5 || py > y1 + 0.5;
                assert!(!beyond, "paint outside the island at {at}");
                let cx = px.clamp(x0 + radius, (x1 - radius).max(x0 + radius));
                let cy = py.clamp(y0 + radius, (y1 - radius).max(y0 + radius));
                let (dx, dy) = (px - cx, py - cy);
                let over = (dx * dx + dy * dy).sqrt() - radius - 0.5;
                if over > worst.1 {
                    worst = (at, over);
                }
            }
        }
        assert!(
            worst.1 < 1.0,
            "a sliver painted {:.2}px outside the corner at {}",
            worst.1,
            worst.0
        );
    }

    /// A folding island holds content measured for the width it had when it was open, and
    /// the edge travels over it. Everything that reaches the screen has to stop at that
    /// edge, corners included - the fills through the mask, and the glyphs through it too,
    /// since the backend rasterises and places those itself and a straight column can stop
    /// text at an edge but not at a rounded corner.
    #[test]
    fn a_folding_island_paints_nothing_outside_its_own_outline() {
        use crate::{
            collect::Registry,
            layout::Inputs,
            status::{Fields, StatusItem, Value},
        };
        let cfg = Config::parse(
            "[left]\ngroups = ['a']\n[group.a]\nmodules = ['a', 'b', 'c']\n\
             collapsible = true\ncollapse_button = 'right'\ncollapse_animation = '150ms'\n\
             background = '#3c3836'\nradius = 6\npadding = 0\n\
             separator = { shape = 'slant', width = 5, overlap = 1 }\n\
             ends = { left = 'slant', right = 'slant', width = 6, direction = 'right' }\n\
             collapsed = { icon = 'cpu', icon_size = 6, padding = 3, \
             background = '#83a598' }\n[module.a]\nbackground = '#cc241d'\npadding = 2\n\
             format = '$text'\n[module.b]\nbackground = '#98971a'\npadding = 2\n\
             format = '$text'\n[module.c]\nbackground = '#458588'\npadding = 2\n\
             format = '$text'\n",
        )
        .unwrap();
        // Three of them, so the island holds dividers as well as content. A divider the
        // fold has already covered belongs to the contents and is cut where they are;
        // only the island's own cap is placed out beyond them.
        let items: Vec<_> = ["a", "b", "c"]
            .into_iter()
            .map(|id| {
                let mut fields = Fields::default();
                fields.set(
                    "text",
                    Value::Text(format!("{id} very long wording indeed")),
                );
                StatusItem {
                    id: Some(id.into()),
                    fields,
                    state: Default::default(),
                    urgent: false,
                    foreground: None,
                    background: None,
                    action: None,
                }
            })
            .collect();
        let native = Registry::new(&Default::default());
        let frame = |folding: &std::collections::HashMap<String, f32>| {
            let inputs = Inputs {
                items: &items,
                native: &native,
                sway: &Default::default(),
                alt: &Default::default(),
                pages: &Default::default(),
                collapsed: &Default::default(),
                collapsed_groups: &Default::default(),
                switching: &Default::default(),
                folding,
                waiting: &Default::default(),
                spin: 0,
                tray: &Default::default(),
                output: None,
            };
            crate::layout::compute(
                &cfg,
                &inputs,
                480.0,
                12.0,
                &mut Blocks {
                    scale: 1.0,
                    run: None,
                },
                None,
            )
        };
        let open = frame(&Default::default());
        // Swept rather than sampled: the corner is only reachable while the edge is in the
        // few pixels of travel that put the island's end inside it, which three points of
        // a fold walk straight past.
        let mut clipped = 0;
        for step in 1..80 {
            let at = step as f32 / 100.0;
            let travelling = frame(&[("a".to_string(), at)].into());
            let island = &travelling.groups[0];
            assert!(island.width < open.groups[0].width);
            let last = island.modules.last().unwrap();
            if last.x + last.width <= island.x + island.width {
                continue;
            }
            clipped += usize::from(
                (island.separators.iter())
                    .any(|separator| !separator.cap && separator.x > island.x + island.width),
            );
            // Inside the island's own rounded rectangle. A straight column can stop text
            // at an edge but not at a corner, and the content of a folding island runs
            // right into its rounded ones.
            let radius = 6.0f32;
            let outside = |px: f32, py: f32| {
                let (x0, x1) = (island.x, island.x + island.width);
                let (y0, y1) = (island.y, island.y + island.height);
                if px < x0 - 0.5 || px > x1 + 0.5 || py < y0 - 0.5 || py > y1 + 0.5 {
                    return true;
                }
                let cx = px.clamp(x0 + radius, (x1 - radius).max(x0 + radius));
                let cy = py.clamp(y0 + radius, (y1 - radius).max(y0 + radius));
                let (dx, dy) = (px - cx, py - cy);
                (dx * dx + dy * dy).sqrt() > radius + 0.5
            };
            for scale in [1.0, 1.5, 2.0] {
                let shot = shot(&travelling, scale);
                let width = shot.width();
                let edge = ((island.x + island.width) * scale).ceil() as u32;
                let mut drew = false;
                for (n, pixel) in shot.pixels().iter().enumerate() {
                    if n as u32 % width >= edge {
                        assert_eq!(
                            pixel.alpha(),
                            0,
                            "paint past the island at {at} scale {scale}"
                        );
                    }
                    // Only what is solidly painted: every outline in the frame is
                    // antialiased, so a partly covered pixel on a boundary says nothing
                    // about whether the island kept its contents in.
                    if pixel.alpha() < 250 {
                        continue;
                    }
                    drew = true;
                    let px = (n as u32 % width) as f32 / scale;
                    let py = (n as u32 / width) as f32 / scale;
                    assert!(
                        !outside(px, py),
                        "painted at {px},{py} at {at} scale {scale}"
                    );
                }
                assert!(drew, "nothing was drawn at all");
            }
        }
        assert!(clipped > 0, "the fixture never hid an internal divider");
    }

    #[test]
    fn collapsed_groups_keep_islands_caps_and_damage_every_changed_pixel() {
        use crate::{
            collect::Registry,
            layout::{Damage, Inputs},
            status::{Fields, StatusItem, Value},
        };
        for joined in [false, true] {
            for direction in ["left", "right"] {
                let sep = "{ shape = 'slant', width = 6, overlap = 1 }";
                let mut config = format!(
                    "[left]\ngroups = ['a', 'b', 'c']\n{}",
                    if joined {
                        format!("separator = {sep}\n")
                    } else {
                        String::new()
                    }
                );
                for (name, color) in [("a", "#cc241d"), ("b", "#98971a"), ("c", "#458588")] {
                    config += &format!(
                        "[group.{name}]\nmodules = ['{name}']\ncollapsible = true\ncollapse_button = 'right'\nbackground = '#3c3836'\nradius = 8\npadding = {}\nopacity = {}\nends = {{ left = 'slant', right = 'slant', direction = '{direction}', width = 6 }}\ncollapsed = {{ icon = 'cpu', icon_size = 10, padding = 3, background = '#83a598' }}\n[module.{name}]\nbackground = '{color}'\npadding = 12\nformat = '$text'\n",
                        if joined { 0 } else { 2 },
                        if joined { 1.0 } else { 0.8 }
                    );
                }
                let cfg = Config::parse(&config).unwrap();
                let items: Vec<_> = ["a", "b", "c"]
                    .into_iter()
                    .map(|name| {
                        let mut fields = Fields::default();
                        fields.set("text", Value::Text("example".into()));
                        StatusItem {
                            id: Some(name.into()),
                            fields,
                            state: Default::default(),
                            urgent: false,
                            foreground: None,
                            background: None,
                            action: None,
                        }
                    })
                    .collect();
                let native = Registry::new(&Default::default());
                for name in ["a", "b", "c"] {
                    let empty = Default::default();
                    let folded = [name.to_string()].into();
                    let frame = |groups| {
                        let inputs = Inputs {
                            items: &items,
                            native: &native,
                            sway: &Default::default(),
                            alt: &Default::default(),
                            pages: &Default::default(),
                            collapsed: &Default::default(),
                            collapsed_groups: groups,
                            switching: &Default::default(),
                            folding: &Default::default(),
                            waiting: &Default::default(),
                            spin: 0,
                            tray: &Default::default(),
                            output: None,
                        };
                        crate::layout::compute(
                            &cfg,
                            &inputs,
                            480.0,
                            20.0,
                            &mut Blocks {
                                scale: 1.0,
                                run: None,
                            },
                            None,
                        )
                    };
                    let open = frame(&empty);
                    let closed = frame(&folded);
                    assert_eq!(closed.groups.len(), 3);
                    for group in &closed.groups {
                        assert_eq!(group.opacity, if joined { 1.0 } else { 0.8 });
                    }
                    for scale in [1.0, 1.5, 2.0] {
                        let before = shot(&open, scale);
                        let after = shot(&closed, scale);
                        assert_ne!(before.data(), after.data());
                        for (old, new) in [(&open, &closed), (&closed, &open)] {
                            let Damage::Rects(rects) = new.damage(old) else {
                                panic!("same groups")
                            };
                            for (n, (a, b)) in
                                before.pixels().iter().zip(after.pixels()).enumerate()
                            {
                                if a == b {
                                    continue;
                                }
                                let (x, y) = (
                                    (n as u32 % before.width()) as f32,
                                    (n as u32 / before.width()) as f32,
                                );
                                assert!(
                                    rects
                                        .iter()
                                        .any(|(rx, ry, rw, rh)| x >= (rx * scale).floor()
                                            && x < ((rx + rw) * scale).ceil()
                                            && y >= (ry * scale).floor()
                                            && y < ((ry + rh) * scale).ceil()),
                                    "undamaged pixel {x},{y}; joined={joined} name={name} direction={direction} scale={scale}"
                                );
                            }
                        }
                    }
                    assert_eq!(shot(&frame(&empty), 1.0).data(), shot(&open, 1.0).data());
                }
            }
        }
    }
    /// A module part way between two wordings is drawn at a width the wording it is going
    /// to was never fitted to, so what is written on it has to stop at the module's own
    /// edge. Left to run on it would land on the module beside it, or outside the island
    /// altogether - the same artefact a fold's clip exists for, one level down.
    #[test]
    fn a_travelling_wording_stops_at_the_module_it_is_drawn_in() {
        let config = r##"
[bar]
height = 20
background = { color = "#00000000" }
[left]
groups = ["g"]
[group.g]
modules = ["a"]
padding = 0
background = "#00000000"
[module.a]
format = "$text"
format_alt = "$text spelled out at considerable length"
padding = 4
background = "#00000000"
foreground = "#ffffffff"
"##;
        let cfg = crate::config::Config::parse(config).unwrap();
        let mut fields = crate::status::Fields::default();
        fields.set("text", crate::status::Value::Text("42%".to_string()));
        let items = [crate::status::StatusItem {
            id: Some("a".to_string()),
            fields,
            state: Default::default(),
            urgent: false,
            foreground: None,
            background: None,
            action: None,
        }];
        let native = crate::collect::Registry::new(&Default::default());
        let showing: std::collections::HashMap<String, usize> = [("a".to_string(), 1)].into();
        let frame = |switching: &std::collections::HashMap<String, crate::layout::Leaving>| {
            let inputs = crate::layout::Inputs {
                items: &items,
                native: &native,
                sway: &Default::default(),
                alt: &showing,
                pages: &Default::default(),
                collapsed: &Default::default(),
                collapsed_groups: &Default::default(),
                switching,
                folding: &Default::default(),
                waiting: &Default::default(),
                spin: 0,
                tray: &Default::default(),
                output: None,
            };
            crate::layout::compute(
                &cfg,
                &inputs,
                480.0,
                20.0,
                &mut Blocks {
                    scale: 1.0,
                    run: None,
                },
                None,
            )
        };

        let settled = frame(&Default::default());
        let mut overflowed = false;
        for step in 0..20 {
            let at = step as f32 / 20.0;
            let travelling =
                frame(&[("a".to_string(), crate::layout::Leaving { from: 0, at })].into());
            let module = &travelling.groups[0].modules[0];
            assert!(module.width <= settled.groups[0].modules[0].width + 0.001);
            overflowed |= module.text.chars().count() as f32 * BLOCK
                > module.text_right.expect("a travelling module is cut") - module.text_x;
            for scale in [1.0, 2.0] {
                let shot = shot(&travelling, scale);
                let width = shot.width();
                let edge = ((module.x + module.width) * scale).ceil() as u32;
                for (n, pixel) in shot.pixels().iter().enumerate() {
                    assert!(
                        n as u32 % width < edge || pixel.alpha() == 0,
                        "the wording ran past its module at {at} scale {scale}"
                    );
                }
            }
        }
        assert!(
            overflowed,
            "the travel has to hold more than it shows for the cut to mean anything"
        );
    }
}
