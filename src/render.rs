//! Vector drawing of a laid-out frame.
//!
//! Everything here works in logical pixels; the scale factor is applied through a single
//! transform so HiDPI output stays sharp without the layout code knowing about it.

use anyhow::{Context as _, Result};
use tiny_skia::{
    FillRule, LineCap, Mask, Paint, Path, PathBuilder, Pixmap, PixmapMut, PixmapRef,
    PremultipliedColorU8, Rect, Stroke, Transform,
};

use crate::color::Color;
use crate::config::{Config, Direction, EdgeShape, SeparatorShape};
use crate::icon::{self, IconArt, Ink, PathCmd};
use std::collections::HashMap;

use crate::layout::{Frame, PlacedGroup, PlacedIcon, PlacedModule, PlacedSeparator};
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
    let path = if mirrored {
        // Reflect about the gap's vertical centre line.
        match path.transform(Transform::from_row(-1.0, 0.0, 0.0, 1.0, x0 + x1, 0.0)) {
            Some(p) => p,
            None => return,
        }
    } else {
        path
    };

    fill_path(pixmap, &path, over, transform, clip);
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

/// How many rasterised icons to keep. A bar draws a dozen or so; the rest is headroom for
/// the positions that shift as neighbouring text changes width.
const ICONS_KEPT: usize = 96;

type IconCache = HashMap<IconKey, IconRun>;

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
    clip: Option<&Mask>,
    cache: &mut IconCache,
) {
    if color.is_transparent() || placed.size <= 0.0 {
        return;
    }
    // A clipped icon is rare - only a group with a separator at its end still has a mask -
    // and blending coverage through one would mean applying the mask by hand, so those go
    // the direct way.
    if clip.is_some() {
        draw_icon(pixmap, placed, color, transform, clip);
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
    if !cache.contains_key(&key) {
        // Bounded the blunt way: icons are few and the set is stable, so the only growth is
        // positions that have stopped being used.
        if cache.len() >= ICONS_KEPT {
            cache.clear();
        }
        let Some(run) = rasterise_icon(placed.icon, placed.level, size, fx, fy) else {
            return;
        };
        cache.insert(key, run);
    }
    let Some(run) = cache.get(&key) else {
        return;
    };
    blend_coverage(
        pixmap,
        &run.coverage,
        run.width,
        run.height,
        ox as i32,
        oy as i32,
        color,
        true,
    );
}

/// Put a string on the pixmap with its left edge at `x` and its top at `y`.
///
/// The backend does the placing and the colouring: the text side hands back pixels and
/// where they sit relative to the origin, and knows nothing about what they land on.
fn draw_text(
    pixmap: &mut PixmapMut<'_>,
    text: &mut dyn DrawText,
    what: &str,
    x: f32,
    y: f32,
    scale: f32,
    color: Color,
) {
    if color.is_transparent() {
        return;
    }
    let (ox, oy) = ((x * scale).round() as i32, (y * scale).round() as i32);
    let Some(run) = text.run(what) else {
        return;
    };
    let (rx, ry) = (ox + run.left, oy + run.top);
    match &run.pixels {
        // cosmic-text builds a mask glyph's colour as the coverage over the base's rgb and
        // drops the base's alpha, so an alpha on a text colour has never reached the screen
        // and is not honoured here either.
        RunPixels::Coverage(coverage) => blend_coverage(
            pixmap, coverage, run.width, run.height, rx, ry, color, false,
        ),
        // The text tinted first and what carries its own colour laid over it, which is
        // how `☀ Clear` keeps the sun's colours and the module's own wording.
        RunPixels::Mixed { coverage, rgba } => {
            blend_coverage(
                pixmap, coverage, run.width, run.height, rx, ry, color, false,
            );
            blend_rgba(pixmap, rgba, run.width, run.height, rx, ry);
        }
    }
}

/// Blend premultiplied RGBA into the pixmap, clipped to the surface.
///
/// Only emoji arrive this way: they carry their own colour, so there is nothing to tint.
fn blend_rgba(
    pixmap: &mut PixmapMut<'_>,
    rgba: &[u8],
    width: usize,
    height: usize,
    ox: i32,
    oy: i32,
) {
    let (pw, ph) = (pixmap.width() as i32, pixmap.height() as i32);
    let x0 = ox.max(0);
    let y0 = oy.max(0);
    let x1 = (ox + width as i32).min(pw);
    let y1 = (oy + height as i32).min(ph);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let pixels = pixmap.pixels_mut();
    for py in y0..y1 {
        let src = ((py - oy) as usize * width + (x0 - ox) as usize) * 4;
        let dst = py as usize * pw as usize + x0 as usize;
        for i in 0..(x1 - x0) as usize {
            let px = &rgba[src + i * 4..src + i * 4 + 4];
            let a = u32::from(px[3]);
            if a == 0 {
                continue;
            }
            let inv = 255 - a;
            let slot = &mut pixels[dst + i];
            let under = *slot;
            let over = |s: u8, d: u8| u32::from(s) + (u32::from(d) * inv + 127) / 255;
            let na = over(px[3], under.alpha());
            let nr = over(px[0], under.red()).min(na);
            let ng = over(px[1], under.green()).min(na);
            let nb = over(px[2], under.blue()).min(na);
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
#[allow(clippy::too_many_arguments)]
fn blend_coverage(
    pixmap: &mut PixmapMut<'_>,
    coverage: &[u8],
    width: usize,
    height: usize,
    ox: i32,
    oy: i32,
    color: Color,
    honour_alpha: bool,
) {
    let (pw, ph) = (pixmap.width() as i32, pixmap.height() as i32);
    let x0 = ox.max(0);
    let y0 = oy.max(0);
    let x1 = (ox + width as i32).min(pw);
    let y1 = (oy + height as i32).min(ph);
    if x0 >= x1 || y0 >= y1 {
        return;
    }

    let alpha = match honour_alpha {
        true => color.a as u32,
        false => 255,
    };
    let pixels = pixmap.pixels_mut();
    for py in y0..y1 {
        let src = (py - oy) as usize * width + (x0 - ox) as usize;
        let dst = py as usize * pw as usize + x0 as usize;
        for i in 0..(x1 - x0) as usize {
            let cover = coverage[src + i] as u32;
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
    width: u32,
    height: u32,
    bounds: (u32, u32, u32, u32),
    path: &Path,
    transform: Transform,
) -> Option<&'a Mask> {
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

    mask.fill_path(path, FillRule::Winding, true, transform);
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
    cfg: &Config,
    frame: &Frame,
    scale: f32,
    painter: &mut Painter<impl DrawText>,
) -> Result<()> {
    let Target {
        canvas,
        width,
        height,
        clip,
    } = target;
    {
        let mut pixmap =
            PixmapMut::from_bytes(canvas, width, height).context("wrapping the shm buffer")?;
        render(&mut pixmap, cfg, frame, scale, painter, clip);
    }
    // tiny-skia writes premultiplied RGBA; wl_shm ARGB8888 is BGRA in memory order.
    for px in canvas.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    Ok(())
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
}

/// Draw the bar background, then every group and module, into `pixmap`.
///
/// `clip` is the surface's own, not the painter's, for the reason [`Clip`] gives.
fn render(
    pixmap: &mut PixmapMut<'_>,
    cfg: &Config,
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
        cfg.bar.radius,
        cfg.bar.background,
        transform,
        None,
    );

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
    let clip = match (rl > 0.0 || rr > 0.0) && spills(group) {
        true => {
            let bounds = drawn_bounds(group, transform, pw, ph);
            bounds.and_then(|b| clip_mask(tools.mask, pw, ph, b, &outline, transform))
        }
        false => None,
    };

    // Separators go down before the modules, so any overlap is covered by them.
    for separator in &group.separators {
        draw_separator(pixmap, separator, scale, transform, clip);
    }

    let count = group.modules.len();
    for (index, module) in group.modules.iter().enumerate() {
        // Snapped against the same grid as the separators, so the edge a module shares
        // with the gap beside it is one edge rather than two.
        let (mx0, mx1) = (snap(module.x, scale), snap(module.x + module.width, scale));
        let (ml, mr) = outer_radii(module, group, index, count, rl, rr);
        fill_edged(
            pixmap,
            (mx0, module.y, mx1 - mx0, module.height),
            ml,
            mr,
            module.background,
            transform,
            clip,
        );
        if let Some(icon) = &module.icon {
            draw_icon_cached(
                pixmap,
                icon,
                module.foreground,
                transform,
                clip,
                tools.icons,
            );
        }
        // Layout already placed the text; only the vertical centring is ours.
        let ty = module.y + (module.height - tools.line_height) / 2.0;
        let (tx, ty) = (module.text_x + offset.0, ty + offset.1);
        draw_text(
            pixmap,
            tools.text,
            &module.text,
            tx,
            ty,
            scale,
            module.foreground,
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
fn outer_radii(
    module: &PlacedModule,
    group: &PlacedGroup,
    index: usize,
    count: usize,
    left: f32,
    right: f32,
) -> (f32, f32) {
    let full_height = module.height >= group.height - 0.01 && module.y <= group.y + 0.01;
    if !full_height {
        return (module.radius, module.radius);
    }
    let at_left = index == 0 && module.x <= group.x + 0.01;
    let at_right = index + 1 == count && module.x + module.width >= group.x + group.width - 0.01;
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
    let x0 = map(group.x, transform.sx, transform.tx).floor();
    let y0 = map(group.y, transform.sy, transform.ty).floor();
    let x1 = map(group.x + group.width, transform.sx, transform.tx).ceil();
    let y1 = map(group.y + group.height, transform.sy, transform.ty).ceil();

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
    let x0 = (group.x * scale).floor().clamp(0.0, width as f32) as u32;
    let y0 = (group.y * scale).floor().clamp(0.0, height as f32) as u32;
    let x1 = ((group.x + group.width) * scale)
        .ceil()
        .clamp(0.0, width as f32) as u32;
    let y1 = ((group.y + group.height) * scale)
        .ceil()
        .clamp(0.0, height as f32) as u32;
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
    use crate::config::{Config, Direction, EdgeShape, Edges, SeparatorShape};
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
                icon: Icon::Cpu,
                level: 0,
                x: x + 2.0,
                y: 4.0,
                size: 12.0,
            }),
            text: text.to_string(),
            text_x: x + 16.0,
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
                    fill: TILE,
                    under: TILE_ALT,
                }],
            }],
        }
    }

    fn bar() -> Config {
        Config::parse("[bar]\nheight = 20\n").expect("a bar with no modules is a config")
    }

    fn shot(frame: &Frame, scale: f32) -> Pixmap {
        let mut pixmap = Pixmap::new((480.0 * scale) as u32, (20.0 * scale) as u32).unwrap();
        let mut painter = Painter::new(Blocks { scale, run: None });
        render(
            &mut pixmap.as_mut(),
            &bar(),
            frame,
            scale,
            &mut painter,
            &mut Clip::default(),
        );
        pixmap
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
                        None,
                        &mut cache,
                    );
                    // Again, onto its own ground, so a hit is checked as well as a miss.
                    let mut again = Pixmap::new(64, 64).unwrap();
                    draw_icon_cached(
                        &mut again.as_mut(),
                        &placed,
                        colour,
                        Transform::identity(),
                        None,
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
}
