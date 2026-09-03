//! Vector drawing of a laid-out frame.
//!
//! Everything here works in logical pixels; the scale factor is applied through a single
//! transform so HiDPI output stays sharp without the layout code knowing about it.

use tiny_skia::{FillRule, Paint, Path, PathBuilder, PixmapMut, Rect, Transform};

use crate::color::Color;
use crate::config::Config;
use crate::layout::Frame;
use crate::text::TextRenderer;

/// Control-point ratio that turns a cubic into a quarter circle.
const KAPPA: f32 = 0.552_285;

/// A rectangle with uniformly rounded corners. Returns `None` for degenerate sizes.
pub fn rounded_rect(x: f32, y: f32, w: f32, h: f32, radius: f32) -> Option<Path> {
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let r = radius.clamp(0.0, (w / 2.0).min(h / 2.0));
    let mut pb = PathBuilder::new();
    if r <= 0.0 {
        pb.push_rect(Rect::from_xywh(x, y, w, h)?);
        return pb.finish();
    }

    let c = r * KAPPA;
    let (x0, y0, x1, y1) = (x, y, x + w, y + h);

    pb.move_to(x0 + r, y0);
    pb.line_to(x1 - r, y0);
    pb.cubic_to(x1 - r + c, y0, x1, y0 + r - c, x1, y0 + r);
    pb.line_to(x1, y1 - r);
    pb.cubic_to(x1, y1 - r + c, x1 - r + c, y1, x1 - r, y1);
    pb.line_to(x0 + r, y1);
    pb.cubic_to(x0 + r - c, y1, x0, y1 - r + c, x0, y1 - r);
    pb.line_to(x0, y0 + r);
    pb.cubic_to(x0, y0 + r - c, x0 + r - c, y0, x0 + r, y0);
    pb.close();
    pb.finish()
}

/// Fill one rounded box given as `(x, y, width, height)` in logical pixels.
fn fill(
    pixmap: &mut PixmapMut<'_>,
    bounds: (f32, f32, f32, f32),
    radius: f32,
    color: Color,
    transform: Transform,
) {
    if color.is_transparent() {
        return;
    }
    let (x, y, w, h) = bounds;
    let Some(path) = rounded_rect(x, y, w, h, radius) else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color(color.to_skia());
    paint.anti_alias = true;
    pixmap.fill_path(&path, &paint, FillRule::Winding, transform, None);
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
    );

    let line_height = text.line_height();
    for group in &frame.groups {
        fill(
            pixmap,
            (group.x, group.y, group.width, group.height),
            group.radius,
            group.background,
            transform,
        );
        for module in &group.modules {
            fill(
                pixmap,
                (module.x, module.y, module.width, module.height),
                module.radius,
                module.background,
                transform,
            );
            // Center the text in the module box on both axes.
            let text_width = text.measure(&module.text);
            let tx = module.x + (module.width - text_width) / 2.0;
            let ty = module.y + (module.height - line_height) / 2.0;
            text.draw(pixmap, &module.text, tx, ty, module.foreground);
        }
    }
}
