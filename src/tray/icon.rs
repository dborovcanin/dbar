//! Turning what a tray item says about its icon into pixels the bar can draw.
//!
//! An item offers its icon one of two ways: as pixmaps it hands over on the bus, or as a
//! name to be found in the icon theme. Both end up here as premultiplied RGBA at the size
//! the bar asked for, which is what the renderer blends and the only form anything below
//! `Frame` ever sees.

use std::path::{Path, PathBuf};

use crate::dbus::Value;
use crate::icon::Raster;

/// Refuse anything larger than this on a side before scaling.
///
/// A tray icon is drawn at bar height and arrives over a bus dbar does not control, so the
/// only sane thing to do with an absurd one is to decline it rather than allocate for it.
const LARGEST: i64 = 512;

/// The best of the pixmaps an item handed over, scaled to `target`.
///
/// The property is `a(iiay)`: width, height and the pixels, once per size the application
/// offers. The smallest one that is still big enough is the best starting point, because
/// scaling down keeps detail that scaling up invents.
pub fn from_pixmaps(value: &Value, target: u32) -> Option<Raster> {
    let mut best: Option<(u32, u32, &[u8])> = None;
    for entry in value.items() {
        let parts = entry.items();
        let (width, height) = (parts.first()?.as_int()?, parts.get(1)?.as_int()?);
        let pixels = parts.get(2)?.as_bytes()?;
        if width <= 0 || height <= 0 || width > LARGEST || height > LARGEST {
            continue;
        }
        let (width, height) = (width as u32, height as u32);
        if pixels.len() < (width as usize * height as usize * 4) {
            continue;
        }
        if best.is_none_or(|(w, _, _)| better_size(width, w, target)) {
            best = Some((width, height, pixels));
        }
    }
    let (width, height, pixels) = best?;
    Some(scale(
        &argb_to_rgba(pixels, width, height),
        width,
        height,
        target,
    ))
}

/// Whether `candidate` is a better size to scale from than `current`.
///
/// Anything at or above the target beats anything below it, and among those the smallest
/// wins; below the target, the largest wins.
fn better_size(candidate: u32, current: u32, target: u32) -> bool {
    match (candidate >= target, current >= target) {
        (true, true) => candidate < current,
        (true, false) => true,
        (false, true) => false,
        (false, false) => candidate > current,
    }
}

/// The pixels of an icon as the spec puts them on the bus: ARGB32, most significant byte
/// first, and not premultiplied - which is neither the order nor the form the renderer
/// blends, so both are fixed here once rather than per frame.
fn argb_to_rgba(pixels: &[u8], width: u32, height: u32) -> Vec<u8> {
    let count = width as usize * height as usize;
    let mut out = Vec::with_capacity(count * 4);
    for pixel in pixels.chunks_exact(4).take(count) {
        let (a, r, g, b) = (pixel[0], pixel[1], pixel[2], pixel[3]);
        let premultiply = |c: u8| ((u32::from(c) * u32::from(a) + 127) / 255) as u8;
        out.extend_from_slice(&[premultiply(r), premultiply(g), premultiply(b), a]);
    }
    out
}

/// Resample to a square of `target` on a side.
///
/// A box filter: each destination pixel averages the source pixels it covers. It is what a
/// downscale of an icon wants and it is cheap, and this runs once per icon rather than per
/// frame, so nothing better is worth the code.
fn scale(rgba: &[u8], width: u32, height: u32, target: u32) -> Raster {
    if width == target && height == target {
        return Raster {
            width,
            height,
            pixels: rgba.to_vec(),
        };
    }
    let mut pixels = vec![0u8; target as usize * target as usize * 4];
    for y in 0..target {
        let (y0, y1) = span(y, target, height);
        for x in 0..target {
            let (x0, x1) = span(x, target, width);
            let mut sum = [0u32; 4];
            let mut count = 0u32;
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let at = (sy as usize * width as usize + sx as usize) * 4;
                    for (channel, total) in sum.iter_mut().enumerate() {
                        *total += u32::from(rgba[at + channel]);
                    }
                    count += 1;
                }
            }
            let at = (y as usize * target as usize + x as usize) * 4;
            for (channel, total) in sum.iter().enumerate() {
                pixels[at + channel] = (total / count.max(1)) as u8;
            }
        }
    }
    Raster {
        width: target,
        height: target,
        pixels,
    }
}

/// Which source pixels one destination pixel covers, always at least one.
fn span(index: u32, target: u32, source: u32) -> (u32, u32) {
    let start = index * source / target;
    let end = ((index + 1) * source).div_ceil(target).min(source);
    (start, end.max(start + 1).min(source))
}

/// An icon found by name in the icon theme, scaled to `target`.
///
/// `extra` is the directory an item may name for itself, which is how an application that
/// ships its own artwork points at it without installing a theme.
pub fn from_name(name: &str, extra: Option<&str>, theme: &str, target: u32) -> Option<Raster> {
    let file = find(name, extra, theme, target)?;
    let bytes = std::fs::read(&file).ok()?;
    // tiny-skia already decodes PNG for its own loading, so a tray icon costs no
    // dependency at all - only the code that was already there becoming reachable.
    let decoded = tiny_skia::Pixmap::decode_png(&bytes).ok()?;
    let (width, height) = (decoded.width(), decoded.height());
    if width == 0 || height == 0 || width as i64 > LARGEST || height as i64 > LARGEST {
        return None;
    }
    // tiny-skia hands back premultiplied RGBA, which is what the renderer blends.
    Some(scale(decoded.data(), width, height, target))
}

/// The file holding `name`: the configured theme's if it has one, then the fallback theme
/// every application is required to install into, and at the size closest to what will be
/// drawn.
fn find(name: &str, extra: Option<&str>, theme: &str, target: u32) -> Option<PathBuf> {
    // An absolute name is a file, and some applications send one.
    if name.starts_with('/') {
        let path = PathBuf::from(name);
        return path.is_file().then_some(path);
    }

    let mut candidates = Vec::new();
    for root in roots(extra) {
        let from_item = extra.is_some_and(|dir| root == Path::new(dir));
        for path in search(&root, name) {
            let size = size_of(&path, &root);
            candidates.push((rank(&path, theme, from_item), size, path));
        }
    }
    choose(candidates, target)
}

/// How much a file is wanted before its size is considered at all.
///
/// Themes inherit from one another through `index.theme`, which dbar does not read; the
/// order here is what that inheritance almost always works out to, without parsing
/// anything: what the application shipped, then what the user chose, then the theme every
/// application is required to install into, then whatever else turned up.
fn rank(path: &Path, theme: &str, from_item: bool) -> u8 {
    if from_item {
        return 0;
    }
    let names = || path.components().filter_map(|c| c.as_os_str().to_str());
    if names().any(|part| part.eq_ignore_ascii_case(theme)) {
        return 1;
    }
    if names().any(|part| part.eq_ignore_ascii_case("hicolor")) {
        return 2;
    }
    3
}

/// The best of what was found: the most wanted theme first, and within it the size that
/// needs the least resampling.
fn choose(candidates: Vec<(u8, u32, PathBuf)>, target: u32) -> Option<PathBuf> {
    let mut best: Option<(u8, u32, PathBuf)> = None;
    for (rank, size, path) in candidates {
        let wins = match &best {
            None => true,
            Some((have_rank, have_size, _)) => match rank.cmp(have_rank) {
                std::cmp::Ordering::Less => true,
                std::cmp::Ordering::Greater => false,
                std::cmp::Ordering::Equal => better_size(size, *have_size, target),
            },
        };
        if wins {
            best = Some((rank, size, path));
        }
    }
    best.map(|(_, _, path)| path)
}

/// Where to look for icons, in the order they should win.
fn roots(extra: Option<&str>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = extra.map(PathBuf::from).into_iter().collect();
    if let Some(home) = std::env::var_os("HOME") {
        out.push(PathBuf::from(&home).join(".local/share/icons"));
        out.push(PathBuf::from(&home).join(".icons"));
    }
    let dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    for dir in dirs.split(':').filter(|d| !d.is_empty()) {
        out.push(PathBuf::from(dir).join("icons"));
        out.push(PathBuf::from(dir).join("pixmaps"));
    }
    out
}

/// How deep to walk an icon theme looking for one name.
///
/// A theme is laid out as `<size>/<context>/<name>.png`, so three levels reaches every
/// icon in one and stops the walk from wandering into anything larger.
const DEPTH: usize = 4;

/// Every `<name>.png` under `root`, however the theme arranges its directories.
fn search(root: &Path, name: &str) -> Vec<PathBuf> {
    let wanted = format!("{name}.png");
    let mut found = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if depth < DEPTH {
                    stack.push((path, depth + 1));
                }
            } else if path.file_name().is_some_and(|f| f == wanted.as_str()) {
                found.push(path);
            }
        }
    }
    found
}

/// The size a themed icon's path claims it is.
///
/// Themes say so in a directory name - `48x48`, or `scalable` - which is enough to choose
/// between two files without reading either. A path that says nothing is treated as small,
/// so a named size is always preferred to a guess.
fn size_of(path: &Path, root: &Path) -> u32 {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .find_map(|part| {
            let text = part.as_os_str().to_str()?;
            let head = text.split_once('x').map_or(text, |(head, _)| head);
            head.parse::<u32>()
                .ok()
                .filter(|size| *size <= LARGEST as u32)
        })
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One pixmap entry as the bus delivers it: width, height, then ARGB bytes.
    fn pixmap(width: i64, height: i64, fill: [u8; 4]) -> Value {
        let pixels = std::iter::repeat_n(fill, (width * height) as usize)
            .flatten()
            .collect();
        Value::Seq(vec![Value::Seq(vec![
            Value::Int(width),
            Value::Int(height),
            Value::Bytes(pixels),
        ])])
    }

    /// The bus sends ARGB, most significant byte first, and not premultiplied. The
    /// renderer blends premultiplied RGBA. Getting this backwards paints an icon in the
    /// wrong colour, which no test that only counts pixels would catch.
    #[test]
    fn a_pixmap_is_reordered_and_premultiplied() {
        // Half-transparent pure red: A=128, R=255, G=0, B=0.
        let value = pixmap(1, 1, [128, 255, 0, 0]);
        let raster = from_pixmaps(&value, 1).expect("one pixmap is enough");
        assert_eq!(raster.width, 1);
        // Red scaled by the alpha, then the alpha itself, in RGBA order.
        assert_eq!(raster.pixels, vec![128, 0, 0, 128]);
    }

    #[test]
    fn an_opaque_pixmap_keeps_its_colour() {
        let value = pixmap(1, 1, [255, 10, 20, 30]);
        let raster = from_pixmaps(&value, 1).expect("one pixmap is enough");
        assert_eq!(raster.pixels, vec![10, 20, 30, 255]);
    }

    /// Scaling down keeps detail that scaling up invents, so the smallest size at or above
    /// what will be drawn is the one to start from.
    #[test]
    fn the_size_closest_above_the_target_is_chosen() {
        for target in [16, 22, 32] {
            let sizes = [16u32, 22, 32, 64];
            let mut best = sizes[0];
            for size in sizes.into_iter().skip(1) {
                if better_size(size, best, target) {
                    best = size;
                }
            }
            assert_eq!(best, target, "for a target of {target}");
        }
        // Nothing big enough: the largest available is the least bad.
        let mut best = 16u32;
        for size in [22u32, 24] {
            if better_size(size, best, 48) {
                best = size;
            }
        }
        assert_eq!(best, 24);
    }

    #[test]
    fn a_pixmap_is_scaled_to_the_size_it_will_be_drawn() {
        let value = pixmap(8, 8, [255, 40, 50, 60]);
        let raster = from_pixmaps(&value, 4).expect("a pixmap scales");
        assert_eq!((raster.width, raster.height), (4, 4));
        assert_eq!(raster.pixels.len(), 4 * 4 * 4);
        // A flat colour survives a box filter unchanged.
        assert_eq!(&raster.pixels[..4], &[40, 50, 60, 255]);
    }

    /// An icon whose side is longer than the bar could ever draw is declined rather than
    /// allocated for: the size comes from a bus dbar does not control.
    #[test]
    fn an_absurd_pixmap_is_refused() {
        let value = Value::Seq(vec![Value::Seq(vec![
            Value::Int(4096),
            Value::Int(4096),
            Value::Bytes(vec![0; 16]),
        ])]);
        assert_eq!(from_pixmaps(&value, 20), None);
    }

    /// A pixmap that says it is larger than the bytes it brought would be read past the
    /// end of.
    #[test]
    fn a_pixmap_shorter_than_it_claims_is_refused() {
        let value = Value::Seq(vec![Value::Seq(vec![
            Value::Int(16),
            Value::Int(16),
            Value::Bytes(vec![0; 16]),
        ])]);
        assert_eq!(from_pixmaps(&value, 16), None);
    }

    #[test]
    fn a_themed_path_says_what_size_it_holds() {
        let root = Path::new("/usr/share/icons/Adwaita");
        assert_eq!(
            size_of(
                Path::new("/usr/share/icons/Adwaita/48x48/status/nm-signal-75.png"),
                root
            ),
            48
        );
        assert_eq!(
            size_of(
                Path::new("/usr/share/icons/Adwaita/22x22/apps/thing.png"),
                root
            ),
            22
        );
        // A layout that names no size is worse than one that does.
        assert_eq!(
            size_of(
                Path::new("/usr/share/pixmaps/thing.png"),
                Path::new("/usr/share/pixmaps")
            ),
            1
        );
    }

    /// dbar does not read `index.theme`, so the order themes are preferred in has to hold
    /// up on its own: what the application shipped, the theme the user chose, then the one
    /// every application installs into.
    #[test]
    fn the_configured_theme_wins_over_the_fallback_one() {
        let chosen = choose(
            vec![
                (
                    rank(
                        Path::new("/usr/share/icons/hicolor/22x22/apps/n.png"),
                        "Papirus",
                        false,
                    ),
                    22,
                    PathBuf::from("hicolor"),
                ),
                (
                    rank(
                        Path::new("/usr/share/icons/Papirus/24x24/panel/n.png"),
                        "Papirus",
                        false,
                    ),
                    24,
                    PathBuf::from("papirus"),
                ),
            ],
            22,
        );
        assert_eq!(chosen, Some(PathBuf::from("papirus")));

        // With no themed copy, the fallback theme still beats a stray one elsewhere.
        assert_eq!(
            rank(
                Path::new("/usr/share/icons/hicolor/22x22/apps/n.png"),
                "Adwaita",
                false
            ),
            2
        );
        assert_eq!(
            rank(Path::new("/usr/share/pixmaps/n.png"), "Adwaita", false),
            3
        );
        // What the item shipped for itself beats every theme.
        assert_eq!(rank(Path::new("/opt/app/icons/n.png"), "Adwaita", true), 0);
    }

    /// Within one theme the size closest to what will be drawn wins, but a worse size in
    /// the chosen theme still beats a perfect one somewhere else.
    #[test]
    fn a_worse_size_in_the_right_theme_beats_a_better_size_in_the_wrong_one() {
        let chosen = choose(
            vec![
                (1, 48, PathBuf::from("chosen-theme-48")),
                (2, 22, PathBuf::from("fallback-22")),
            ],
            22,
        );
        assert_eq!(chosen, Some(PathBuf::from("chosen-theme-48")));
    }

    #[test]
    fn every_destination_pixel_covers_at_least_one_source_pixel() {
        // An upscale asks for more pixels than there are, and a zero-wide span would be a
        // division by zero rather than a colour.
        for (target, source) in [(20u32, 8u32), (8, 20), (1, 7), (7, 1)] {
            for index in 0..target {
                let (start, end) = span(index, target, source);
                assert!(end > start, "{index} of {target} over {source}");
                assert!(end <= source);
            }
        }
    }
}
