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
use crate::icon::{self, IconArt, Ink};
use crate::layout::{Frame, PlacedGroup, PlacedIcon, PlacedSeparator};
use crate::text::TextRenderer;

/// What the renderer needs from a text backend.
///
/// Drawing sits behind this the way measuring sits behind `layout::Measure`, so `render.rs`
/// can be tested without fonts and so a future backend can bring its own rasteriser. The
/// coordinates are logical pixels; an implementation applies the output scale itself.
pub trait DrawText {
    /// Height of one line, in logical pixels.
    fn line_height(&self) -> f32;

    /// Draw `text` with its left edge at `x` and its top at `y`.
    fn draw(&mut self, pixmap: &mut PixmapMut<'_>, text: &str, x: f32, y: f32, color: Color);
}

impl DrawText for TextRenderer {
    fn line_height(&self) -> f32 {
        TextRenderer::line_height(self)
    }

    fn draw(&mut self, pixmap: &mut PixmapMut<'_>, text: &str, x: f32, y: f32, color: Color) {
        TextRenderer::draw(self, pixmap, text, x, y, color)
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
        match item.ink {
            Ink::Fill => {
                pixmap.fill_path(&item.path, &paint, FillRule::Winding, ts, clip);
            }
            Ink::FillEvenOdd => {
                pixmap.fill_path(&item.path, &paint, FillRule::EvenOdd, ts, clip);
            }
            Ink::Stroke(width) => {
                let stroke = Stroke {
                    width,
                    line_cap: LineCap::Round,
                    ..Stroke::default()
                };
                pixmap.stroke_path(&item.path, &paint, &stroke, ts, clip);
            }
        }
    }
}

/// A coverage mask of `path`, used to clip a group's contents to its outline.
fn clip_mask(pixmap: &PixmapMut<'_>, path: &Path, transform: Transform) -> Option<Mask> {
    let mut mask = Mask::new(pixmap.width(), pixmap.height())?;
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
    canvas: &mut [u8],
    width: u32,
    height: u32,
    cfg: &Config,
    frame: &Frame,
    scale: f32,
    painter: &mut Painter<impl DrawText>,
) -> Result<()> {
    {
        let mut pixmap =
            PixmapMut::from_bytes(canvas, width, height).context("wrapping the shm buffer")?;
        render(&mut pixmap, cfg, frame, scale, painter);
    }
    // tiny-skia writes premultiplied RGBA; wl_shm ARGB8888 is BGRA in memory order.
    for px in canvas.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    Ok(())
}

/// Draw the bar background, then every group and module, into `pixmap`.
fn render(
    pixmap: &mut PixmapMut<'_>,
    cfg: &Config,
    frame: &Frame,
    scale: f32,
    painter: &mut Painter<impl DrawText>,
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
    let Painter { text, scratch } = painter;
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
            draw_group(pixmap, group, scale, transform, text, line_height);
            continue;
        };
        let Some(layer) = layer(scratch, bw, bh) else {
            // Nowhere to draw: an island at full strength is a worse bar than one at the
            // asked-for alpha, but it is still a readable one.
            draw_group(pixmap, group, scale, transform, text, line_height);
            continue;
        };

        // The island is drawn into the layer's own corner, so the layer only ever has to
        // be as big as the widest island rather than as big as the bar.
        clear(layer, bw, bh);
        let local = transform.post_translate(-(bx as f32), -(by as f32));
        draw_group(&mut layer.as_mut(), group, scale, local, text, line_height);
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
    text: &mut dyn DrawText,
    line_height: f32,
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

    // Square module corners and separator overlap would otherwise spill past a
    // rounded group edge, so clip the group's contents to its own outline.
    let clip = if rl > 0.0 || rr > 0.0 {
        clip_mask(pixmap, &outline, transform)
    } else {
        None
    };
    let clip = clip.as_ref();

    // Separators go down before the modules, so any overlap is covered by them.
    for separator in &group.separators {
        draw_separator(pixmap, separator, scale, transform, clip);
    }

    for module in &group.modules {
        // Snapped against the same grid as the separators, so the edge a module shares
        // with the gap beside it is one edge rather than two.
        let (mx0, mx1) = (snap(module.x, scale), snap(module.x + module.width, scale));
        fill(
            pixmap,
            (mx0, module.y, mx1 - mx0, module.height),
            module.radius,
            module.background,
            transform,
            clip,
        );
        if let Some(icon) = &module.icon {
            draw_icon(pixmap, icon, module.foreground, transform, clip);
        }
        // Layout already placed the text; only the vertical centring is ours.
        let ty = module.y + (module.height - line_height) / 2.0;
        text.draw(
            pixmap,
            &module.text,
            module.text_x + offset.0,
            ty + offset.1,
            module.foreground,
        );
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
}

impl<T: DrawText> Painter<T> {
    pub fn new(text: T) -> Painter<T> {
        Painter {
            text,
            scratch: None,
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
    }

    const BLOCK: f32 = 8.0;

    impl DrawText for Blocks {
        fn line_height(&self) -> f32 {
            BLOCK
        }

        fn draw(&mut self, pixmap: &mut PixmapMut<'_>, text: &str, x: f32, y: f32, color: Color) {
            let w = text.chars().count() as f32 * BLOCK;
            // Logical coordinates in, device pixels out, with no reference to the
            // renderer's transform. The real backend rasterises glyphs the same way.
            let s = self.scale;
            let bounds = (x * s, y * s, w * s, BLOCK * s);
            fill(pixmap, bounds, 0.0, color, Transform::identity(), None);
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
            alt: None,
            collapsible: None,
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
        let mut painter = Painter::new(Blocks { scale });
        render(&mut pixmap.as_mut(), &bar(), frame, scale, &mut painter);
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
