//! Vector drawing of a laid-out frame.
//!
//! Everything here works in logical pixels; the scale factor is applied through a single
//! transform so HiDPI output stays sharp without the layout code knowing about it.

use tiny_skia::{FillRule, Mask, Paint, Path, PathBuilder, PixmapMut, Rect, Transform};

use crate::color::Color;
use crate::config::{Config, Direction, EdgeShape, SeparatorShape};
use crate::layout::{Frame, PlacedSeparator};
use crate::text::TextRenderer;

/// Control-point ratio that turns a cubic into a quarter circle.
const KAPPA: f32 = 0.552_285;

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

fn draw_separator(
    pixmap: &mut PixmapMut<'_>,
    sep: &PlacedSeparator,
    transform: Transform,
    clip: Option<&Mask>,
) {
    if sep.shape.is_none() {
        return;
    }
    // Bleed past both sides so neither antialiased edge leaves a hairline of wallpaper.
    let x0 = sep.x - sep.overlap;
    let x1 = sep.x + sep.width + sep.overlap;
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

    fill(
        pixmap,
        (x0, y0, x1 - x0, y1 - y0),
        0.0,
        under,
        transform,
        clip,
    );

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
    paint.set_color(color.to_skia());
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

/// Draw the bar background, then every group and module, into `pixmap`.
pub fn render(
    pixmap: &mut PixmapMut<'_>,
    cfg: &Config,
    frame: &Frame,
    scale: f32,
    text: &mut TextRenderer,
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

    let line_height = text.line_height();
    for group in &frame.groups {
        let radius = |shape: EdgeShape| match shape {
            EdgeShape::Round => group.edges.radius,
            EdgeShape::None => 0.0,
        };
        let (rl, rr) = (radius(group.edges.left), radius(group.edges.right));
        let Some(outline) = edged_rect(group.x, group.y, group.width, group.height, rl, rr) else {
            continue;
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
            draw_separator(pixmap, separator, transform, clip);
        }

        for module in &group.modules {
            fill(
                pixmap,
                (module.x, module.y, module.width, module.height),
                module.radius,
                module.background,
                transform,
                clip,
            );
            // Center the text in the module box on both axes.
            let text_width = text.measure(&module.text);
            let tx = module.x + (module.width - text_width) / 2.0;
            let ty = module.y + (module.height - line_height) / 2.0;
            text.draw(pixmap, &module.text, tx, ty, module.foreground);
        }
    }
}
