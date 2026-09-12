//! Built-in vector icons.
//!
//! Icons are drawn as geometry a unit tall, so they scale with `icon_size` instead of
//! riding on a font. Most are a unit wide as well; a battery is longer than it is tall and
//! says so with `width`, which is the only thing that varies. Graded icons take a level
//! rather than being five separate drawings: a battery is one outline with a fill of
//! varying width, wifi is a dot plus a count of arcs, and so on.

/// Number of steps a graded icon has.
pub const LEVELS: usize = 5;

/// How a path in an icon is painted. Widths are in unit space, so they scale with the icon.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ink {
    Fill,
    /// Filled with the even-odd rule, so overlapping subpaths cut holes in each other.
    FillEvenOdd,
    Stroke(f32),
}

/// A point in the unit square an icon is drawn inside.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

/// One step of an outline, in the unit square.
///
/// Icons describe themselves this way rather than in the rasteriser's own path type, so
/// the library says what the shape is and the backend decides how to draw it. A GPU
/// backend tessellates the same commands a CPU one hands to `tiny-skia`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PathCmd {
    MoveTo(Point),
    LineTo(Point),
    CubicTo(Point, Point, Point),
    Close,
}

pub struct IconPath {
    pub cmds: Vec<PathCmd>,
    pub ink: Ink,
}

/// Artwork for one icon. Built-ins are vector; the raster arm is what an application's own
/// icon arrives as, already premultiplied RGBA at a size.
pub enum IconArt {
    Paths(Vec<IconPath>),
    #[allow(dead_code)]
    Raster(std::sync::Arc<Raster>),
}

/// An icon that is already pixels: premultiplied RGBA, row-major.
///
/// What a tray item hands over is a picture rather than an outline, and this is where it
/// stops being anything else. Nothing here knows where the pixels came from, which is what
/// keeps the protocol above `Frame` and out of the renderer.
#[derive(Debug, PartialEq, Eq)]
pub struct Raster {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Collects the commands that make up one outline.
///
/// Stands in for the rasteriser's path builder, with the few shapes the icon library
/// actually draws: everything here is a line, a cubic, a rectangle or a circle.
#[derive(Default)]
pub struct Outline {
    cmds: Vec<PathCmd>,
}

impl Outline {
    fn new() -> Outline {
        Outline::default()
    }

    fn move_to(&mut self, x: f32, y: f32) {
        self.cmds.push(PathCmd::MoveTo(Point { x, y }));
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.cmds.push(PathCmd::LineTo(Point { x, y }));
    }

    #[allow(clippy::too_many_arguments)]
    fn cubic_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.cmds.push(PathCmd::CubicTo(
            Point { x: x1, y: y1 },
            Point { x: x2, y: y2 },
            Point { x, y },
        ));
    }

    fn close(&mut self) {
        self.cmds.push(PathCmd::Close);
    }

    fn push_rect(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        self.move_to(x0, y0);
        self.line_to(x1, y0);
        self.line_to(x1, y1);
        self.line_to(x0, y1);
        self.close();
    }

    /// A circle as four cubics, which is what every rasteriser does with one anyway.
    fn push_circle(&mut self, cx: f32, cy: f32, r: f32) {
        let c = r * KAPPA;
        self.move_to(cx + r, cy);
        self.cubic_to(cx + r, cy + c, cx + c, cy + r, cx, cy + r);
        self.cubic_to(cx - c, cy + r, cx - r, cy + c, cx - r, cy);
        self.cubic_to(cx - r, cy - c, cx - c, cy - r, cx, cy - r);
        self.cubic_to(cx + c, cy - r, cx + r, cy - c, cx + r, cy);
        self.close();
    }

    fn is_empty(&self) -> bool {
        self.cmds.is_empty()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Icon {
    /// An icon that is a picture rather than an outline, whose pixels travel beside it.
    ///
    /// It has a variant of its own so that layout can size and place it like any other
    /// icon without knowing what it is, and so the cache of rasterised outlines is never
    /// asked to hold something that is already pixels.
    Raster,
    Cpu,
    Tux,
    Arch,
    Slack,
    Code,
    Chrome,
    Memory,
    Disk,
    Clock,
    Ethernet,
    /// Graded by a percentage found in the module's text.
    Battery,
    BatteryCharging,
    Wifi,
    Volume,
    Brightness,
    /// Graded by the temperature itself, read as a share of a hundred degrees.
    Temperature,
    VolumeMuted,
    WifiOff,
    Headphones,
    HeadphonesMuted,
    Play,
    Pause,
    Media,
    Keyboard,
    /// A module is waiting on something. Graded by the animation frame rather than by a
    /// reading, which is why it has more steps than the rest and no name in the config.
    Spinner,
}

impl Icon {
    pub fn parse(name: &str) -> Option<Icon> {
        Some(match name {
            "cpu" => Icon::Cpu,
            "tux" => Icon::Tux,
            "arch" | "arch-linux" => Icon::Arch,
            "slack" => Icon::Slack,
            "code" => Icon::Code,
            "chrome" | "chromium" => Icon::Chrome,
            "memory" | "ram" => Icon::Memory,
            "disk" => Icon::Disk,
            "clock" | "time" => Icon::Clock,
            "ethernet" => Icon::Ethernet,
            "battery" => Icon::Battery,
            "battery-charging" => Icon::BatteryCharging,
            "wifi" | "network" => Icon::Wifi,
            "volume" => Icon::Volume,
            "brightness" => Icon::Brightness,
            "temperature" | "temp" => Icon::Temperature,
            "volume-muted" => Icon::VolumeMuted,
            "wifi-off" => Icon::WifiOff,
            "headphones" => Icon::Headphones,
            "headphones-muted" => Icon::HeadphonesMuted,
            "play" => Icon::Play,
            "pause" => Icon::Pause,
            "media" | "music" => Icon::Media,
            "keyboard" | "language" => Icon::Keyboard,
            _ => return None,
        })
    }

    /// How many steps this icon has, which is the animation's length for a spinner and
    /// the grading's for everything else.
    pub fn frames(self) -> usize {
        match self {
            Icon::Spinner => SPINNER_FRAMES,
            _ => LEVELS,
        }
    }

    /// How wide this icon is drawn, as a multiple of its height.
    ///
    /// Icons are square unless they have a reason not to be. A battery does: the thing it
    /// is a picture of is long, and a square one reads as a box with a pip on the end.
    pub fn width(self) -> f32 {
        match self {
            Icon::Battery | Icon::BatteryCharging => BATTERY_WIDTH,
            _ => 1.0,
        }
    }

    /// Whether this icon changes with a percentage in the module's text.
    pub fn is_graded(self) -> bool {
        matches!(
            self,
            Icon::Battery
                | Icon::BatteryCharging
                | Icon::Wifi
                | Icon::Volume
                | Icon::Brightness
                | Icon::Temperature
        )
    }
}

/// Which step of a graded icon a percentage falls in.
pub fn level_of(percent: f64) -> usize {
    let fraction = (percent / 100.0).clamp(0.0, 1.0);
    ((fraction * LEVELS as f64) as usize).min(LEVELS - 1)
}

// ---------------------------------------------------------------------------
// Geometry, a unit tall and `Icon::width` wide
// ---------------------------------------------------------------------------

/// Control-point ratio that turns a cubic into a quarter circle.
const KAPPA: f32 = 0.552_285;

fn rounded(pb: &mut Outline, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) {
    let r = r.clamp(0.0, ((x1 - x0) / 2.0).min((y1 - y0) / 2.0));
    if r <= 0.0 {
        pb.push_rect(x0, y0, x1, y1);
        return;
    }
    let c = r * KAPPA;
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
}

/// Append a circular arc, approximated with one cubic per quadrant-ish span.
fn arc(pb: &mut Outline, cx: f32, cy: f32, r: f32, from: f32, to: f32) {
    let steps = ((to - from).abs() / std::f32::consts::FRAC_PI_2)
        .ceil()
        .max(1.0) as usize;
    let step = (to - from) / steps as f32;
    let k = 4.0 / 3.0 * (step / 4.0).tan();
    let (mut a, mut started) = (from, false);
    for _ in 0..steps {
        let b = a + step;
        let (x0, y0) = (cx + r * a.cos(), cy + r * a.sin());
        let (x1, y1) = (cx + r * b.cos(), cy + r * b.sin());
        if !started {
            pb.move_to(x0, y0);
            started = true;
        }
        pb.cubic_to(
            x0 - k * r * a.sin(),
            y0 + k * r * a.cos(),
            x1 + k * r * b.sin(),
            y1 - k * r * b.cos(),
            x1,
            y1,
        );
        a = b;
    }
}

fn line(pb: &mut Outline, x0: f32, y0: f32, x1: f32, y1: f32) {
    pb.move_to(x0, y0);
    pb.line_to(x1, y1);
}

fn finish(pb: Outline, ink: Ink, out: &mut Vec<IconPath>) {
    if !pb.is_empty() {
        out.push(IconPath { cmds: pb.cmds, ink });
    }
}

/// Build `icon` at `level`, as paths inside the unit square.
pub fn art(icon: Icon, level: usize) -> IconArt {
    let level = level.min(icon.frames() - 1);
    let mut out = Vec::new();
    match icon {
        // Its pixels are carried on the placed icon, so there is no outline to build.
        Icon::Raster => {}
        Icon::Cpu => cpu(&mut out),
        Icon::Tux => tux(&mut out),
        Icon::Arch => arch(&mut out),
        Icon::Slack => slack(&mut out),
        Icon::Code => code(&mut out),
        Icon::Chrome => chrome(&mut out),
        Icon::Memory => memory(&mut out),
        Icon::Disk => disk(&mut out),
        Icon::Clock => clock(&mut out),
        Icon::Ethernet => ethernet(&mut out),
        Icon::Battery => battery(&mut out, level),
        Icon::BatteryCharging => battery_charging(&mut out, level),
        Icon::Wifi => wifi(&mut out, level),
        Icon::WifiOff => {
            // Strike through a full-strength wifi: a slash over an empty one reads as
            // nothing at all at bar sizes.
            wifi(&mut out, LEVELS - 2);
            let mut pb = Outline::new();
            line(&mut pb, 0.20, 0.22, 0.80, 0.82);
            finish(pb, Ink::Stroke(0.09), &mut out);
        }
        Icon::Volume => volume(&mut out, level),
        Icon::VolumeMuted => {
            volume(&mut out, 0);
            let mut pb = Outline::new();
            line(&mut pb, 0.62, 0.36, 0.88, 0.64);
            line(&mut pb, 0.88, 0.36, 0.62, 0.64);
            finish(pb, Ink::Stroke(0.08), &mut out);
        }
        Icon::Spinner => spinner(&mut out, level),
        Icon::Brightness => brightness(&mut out, level),
        Icon::Temperature => temperature(&mut out, level),
        Icon::Headphones => headphones(&mut out),
        Icon::HeadphonesMuted => {
            headphones(&mut out);
            // Struck through the way `wifi-off` is, so the two read as the same kind of
            // statement: the thing is there, and it is not carrying anything.
            let mut pb = Outline::new();
            line(&mut pb, 0.20, 0.22, 0.80, 0.82);
            finish(pb, Ink::Stroke(0.09), &mut out);
        }
        Icon::Play => play(&mut out),
        Icon::Pause => pause(&mut out),
        Icon::Media => media(&mut out),
        Icon::Keyboard => keyboard(&mut out),
    }
    IconArt::Paths(out)
}

/// The Arch Linux mark: a peak with the underside cut into an arch, standing on two feet.
///
/// One filled contour, traced the way the official artwork is: down the left flank, out
/// along the ground, up and over the arch between the feet, then back out to the right.
/// The two nicks in the flanks are turns in that same edge, not separate shapes, which is
/// why nothing here is stroked - a single stroke width could describe neither the flare at
/// the feet nor the taper of a nick.
fn arch(out: &mut Vec<IconPath>) {
    let mut mark = Outline::new();
    mark.move_to(0.5000, 0.0700);
    mark.cubic_to(0.4599, 0.1650, 0.4357, 0.2271, 0.3911, 0.3192);
    mark.cubic_to(0.4185, 0.3472, 0.4521, 0.3798, 0.5066, 0.4167);
    mark.cubic_to(0.4480, 0.3934, 0.4080, 0.3700, 0.3781, 0.3457);
    mark.cubic_to(0.3210, 0.4608, 0.2316, 0.6248, 0.0500, 0.9400);
    mark.cubic_to(0.1928, 0.8604, 0.3033, 0.8113, 0.4064, 0.7926);
    mark.cubic_to(0.4019, 0.7742, 0.3994, 0.7543, 0.3996, 0.7335);
    mark.line_to(0.3998, 0.7291);
    mark.cubic_to(0.4020, 0.6408, 0.4496, 0.5729, 0.5059, 0.5775);
    mark.cubic_to(0.5622, 0.5821, 0.6060, 0.6575, 0.6037, 0.7458);
    mark.cubic_to(0.6033, 0.7624, 0.6013, 0.7784, 0.5980, 0.7933);
    mark.cubic_to(0.6999, 0.8125, 0.8093, 0.8615, 0.9500, 0.9400);
    mark.cubic_to(0.9222, 0.8906, 0.8975, 0.8461, 0.8738, 0.8037);
    mark.cubic_to(0.8366, 0.7758, 0.7977, 0.7395, 0.7185, 0.7002);
    mark.cubic_to(0.7729, 0.7138, 0.8119, 0.7297, 0.8423, 0.7473);
    mark.cubic_to(0.6019, 0.3146, 0.5824, 0.2570, 0.5000, 0.0700);
    mark.close();
    finish(mark, Ink::Fill, out);
}

/// Slack's four interlocking pairs of pills, reduced to one colour for a status bar.
///
/// These are the proportions of the application mark rather than a generic hash: each
/// arm has a short cap beside the longer stroke and the four pairs rotate around the
/// empty centre.
fn slack(out: &mut Vec<IconPath>) {
    let mut mark = Outline::new();
    rounded(&mut mark, 0.304, 0.08, 0.472, 0.248, 0.084);
    rounded(&mut mark, 0.08, 0.304, 0.472, 0.472, 0.084);
    rounded(&mut mark, 0.752, 0.304, 0.92, 0.472, 0.084);
    rounded(&mut mark, 0.528, 0.08, 0.696, 0.472, 0.084);
    rounded(&mut mark, 0.528, 0.752, 0.696, 0.92, 0.084);
    rounded(&mut mark, 0.528, 0.528, 0.92, 0.696, 0.084);
    rounded(&mut mark, 0.08, 0.528, 0.248, 0.696, 0.084);
    rounded(&mut mark, 0.304, 0.528, 0.472, 0.92, 0.084);
    finish(mark, Ink::Fill, out);
}

/// A native version of Nerd Fonts' `󰘦`: square-ended braces around three low dots.
///
/// Inset to the same 0.06 the other application marks keep, so a row of workspaces does
/// not draw this one heavier than the icon beside it.
fn code(out: &mut Vec<IconPath>) {
    let mut braces = Outline::new();
    // Left brace, traced as a filled band so its ends remain square at small sizes.
    braces.move_to(0.308, 0.09);
    braces.line_to(0.225, 0.09);
    braces.cubic_to(0.152, 0.09, 0.152, 0.16, 0.152, 0.23);
    braces.line_to(0.152, 0.34);
    braces.cubic_to(0.152, 0.42, 0.115, 0.46, 0.06, 0.46);
    braces.line_to(0.06, 0.54);
    braces.cubic_to(0.115, 0.54, 0.152, 0.58, 0.152, 0.66);
    braces.line_to(0.152, 0.77);
    braces.cubic_to(0.152, 0.84, 0.152, 0.91, 0.225, 0.91);
    braces.line_to(0.308, 0.91);
    braces.line_to(0.308, 0.83);
    braces.line_to(0.234, 0.83);
    braces.line_to(0.234, 0.66);
    braces.cubic_to(0.234, 0.58, 0.207, 0.53, 0.152, 0.50);
    braces.cubic_to(0.207, 0.47, 0.234, 0.42, 0.234, 0.34);
    braces.line_to(0.234, 0.17);
    braces.line_to(0.308, 0.17);
    braces.close();

    // The other brace is the same contour reflected horizontally.
    braces.move_to(0.692, 0.09);
    braces.line_to(0.775, 0.09);
    braces.cubic_to(0.848, 0.09, 0.848, 0.16, 0.848, 0.23);
    braces.line_to(0.848, 0.34);
    braces.cubic_to(0.848, 0.42, 0.885, 0.46, 0.94, 0.46);
    braces.line_to(0.94, 0.54);
    braces.cubic_to(0.885, 0.54, 0.848, 0.58, 0.848, 0.66);
    braces.line_to(0.848, 0.77);
    braces.cubic_to(0.848, 0.84, 0.848, 0.91, 0.775, 0.91);
    braces.line_to(0.692, 0.91);
    braces.line_to(0.692, 0.83);
    braces.line_to(0.766, 0.83);
    braces.line_to(0.766, 0.66);
    braces.cubic_to(0.766, 0.58, 0.793, 0.53, 0.848, 0.50);
    braces.cubic_to(0.793, 0.47, 0.766, 0.42, 0.766, 0.34);
    braces.line_to(0.766, 0.17);
    braces.line_to(0.692, 0.17);
    braces.close();
    finish(braces, Ink::Fill, out);

    let mut dots = Outline::new();
    for x in [0.353, 0.50, 0.647] {
        dots.push_circle(x, 0.63, 0.043);
    }
    finish(dots, Ink::Fill, out);
}

/// Chrome's three asymmetric blades and detached centre, inset slightly so its circular
/// footprint carries the same visual weight as dbar's other native icons.
fn chrome(out: &mut Vec<IconPath>) {
    let mut blades = Outline::new();

    // Red blade in the full-colour mark.
    blades.move_to(0.157, 0.207);
    blades.cubic_to(0.356, 0.005, 0.662, 0.005, 0.833, 0.172);
    blades.cubic_to(0.860, 0.199, 0.905, 0.248, 0.901, 0.298);
    blades.line_to(0.500, 0.298);
    blades.cubic_to(0.416, 0.298, 0.343, 0.358, 0.299, 0.456);
    blades.close();

    // Yellow blade.
    blades.move_to(0.637, 0.349);
    blades.line_to(0.926, 0.349);
    blades.cubic_to(0.950, 0.437, 0.950, 0.572, 0.905, 0.662);
    blades.cubic_to(0.815, 0.842, 0.653, 0.950, 0.472, 0.950);
    blades.line_to(0.672, 0.616);
    blades.cubic_to(0.702, 0.563, 0.712, 0.482, 0.671, 0.401);
    blades.close();

    // Green blade.
    blades.move_to(0.122, 0.253);
    blades.line_to(0.338, 0.613);
    blades.cubic_to(0.383, 0.698, 0.464, 0.716, 0.560, 0.640);
    blades.line_to(0.413, 0.939);
    blades.cubic_to(0.212, 0.896, 0.068, 0.734, 0.050, 0.527);
    blades.cubic_to(0.041, 0.428, 0.068, 0.329, 0.122, 0.253);
    blades.close();
    finish(blades, Ink::Fill, out);

    let mut hub = Outline::new();
    hub.push_circle(0.50, 0.50, 0.151);
    finish(hub, Ink::Fill, out);
}

/// A seated penguin in the same single foreground colour as the other icons.
/// The belly, eyes and beak are cutouts so they work on any group background.
fn tux(out: &mut Vec<IconPath>) {
    let mut body = Outline::new();
    body.move_to(0.50, 0.06);
    body.cubic_to(0.34, 0.06, 0.31, 0.18, 0.31, 0.32);
    body.cubic_to(0.30, 0.43, 0.14, 0.51, 0.10, 0.72);
    body.cubic_to(0.08, 0.80, 0.17, 0.81, 0.25, 0.70);
    body.cubic_to(0.23, 0.88, 0.35, 0.91, 0.50, 0.91);
    body.cubic_to(0.65, 0.91, 0.77, 0.88, 0.75, 0.70);
    body.cubic_to(0.83, 0.81, 0.92, 0.80, 0.90, 0.72);
    body.cubic_to(0.86, 0.51, 0.70, 0.43, 0.69, 0.32);
    body.cubic_to(0.69, 0.18, 0.66, 0.06, 0.50, 0.06);
    body.close();
    // Broad belly, narrowing below the beak.
    body.move_to(0.50, 0.47);
    body.cubic_to(0.39, 0.43, 0.31, 0.60, 0.32, 0.75);
    body.cubic_to(0.33, 0.86, 0.67, 0.86, 0.68, 0.75);
    body.cubic_to(0.69, 0.60, 0.61, 0.43, 0.50, 0.47);
    body.close();
    rounded(&mut body, 0.365, 0.225, 0.475, 0.35, 0.055);
    rounded(&mut body, 0.525, 0.225, 0.635, 0.35, 0.055);
    body.move_to(0.39, 0.38);
    body.cubic_to(0.44, 0.34, 0.56, 0.34, 0.61, 0.38);
    body.line_to(0.50, 0.445);
    body.close();
    finish(body, Ink::FillEvenOdd, out);

    let mut details = Outline::new();
    details.push_circle(0.435, 0.29, 0.023);
    details.push_circle(0.565, 0.29, 0.023);
    // Splayed feet, kept apart to preserve the seated silhouette at bar sizes.
    details.move_to(0.26, 0.80);
    details.cubic_to(0.18, 0.79, 0.15, 0.87, 0.08, 0.91);
    details.cubic_to(0.08, 0.97, 0.29, 0.98, 0.43, 0.94);
    details.cubic_to(0.46, 0.91, 0.34, 0.80, 0.26, 0.80);
    details.close();
    details.move_to(0.74, 0.80);
    details.cubic_to(0.82, 0.79, 0.85, 0.87, 0.92, 0.91);
    details.cubic_to(0.92, 0.97, 0.71, 0.98, 0.57, 0.94);
    details.cubic_to(0.54, 0.91, 0.66, 0.80, 0.74, 0.80);
    details.close();
    finish(details, Ink::Fill, out);
}

fn cpu(out: &mut Vec<IconPath>) {
    let mut body = Outline::new();
    rounded(&mut body, 0.24, 0.24, 0.76, 0.76, 0.08);
    finish(body, Ink::Stroke(0.08), out);

    let mut core = Outline::new();
    rounded(&mut core, 0.40, 0.40, 0.60, 0.60, 0.03);
    finish(core, Ink::Fill, out);

    // Three pins on each side.
    let mut pins = Outline::new();
    for i in 0..3 {
        let t = 0.35 + i as f32 * 0.15;
        line(&mut pins, t, 0.10, t, 0.24);
        line(&mut pins, t, 0.76, t, 0.90);
        line(&mut pins, 0.10, t, 0.24, t);
        line(&mut pins, 0.76, t, 0.90, t);
    }
    finish(pins, Ink::Stroke(0.07), out);
}

fn memory(out: &mut Vec<IconPath>) {
    let mut body = Outline::new();
    rounded(&mut body, 0.12, 0.30, 0.88, 0.66, 0.06);
    finish(body, Ink::Stroke(0.08), out);

    let mut inner = Outline::new();
    for i in 0..3 {
        let x = 0.30 + i as f32 * 0.20;
        line(&mut inner, x, 0.40, x, 0.56);
    }
    finish(inner, Ink::Stroke(0.08), out);

    let mut legs = Outline::new();
    line(&mut legs, 0.28, 0.66, 0.28, 0.78);
    line(&mut legs, 0.72, 0.66, 0.72, 0.78);
    finish(legs, Ink::Stroke(0.08), out);
}

/// A hard disk: enclosure, platter, hub and actuator arm.
///
/// The enclosure is portrait, as a drive is. Corner screws are in the real thing but vanish
/// at bar sizes, so they are left out.
fn disk(out: &mut Vec<IconPath>) {
    let mut shell = Outline::new();
    rounded(&mut shell, 0.22, 0.09, 0.78, 0.91, 0.06);
    finish(shell, Ink::Stroke(0.08), out);

    let mut platter = Outline::new();
    platter.push_circle(0.50, 0.42, 0.20);
    finish(platter, Ink::Stroke(0.07), out);

    let mut hub = Outline::new();
    hub.push_circle(0.50, 0.42, 0.062);
    finish(hub, Ink::Fill, out);

    let mut arm = Outline::new();
    line(&mut arm, 0.31, 0.71, 0.57, 0.47);
    finish(arm, Ink::Stroke(0.07), out);
}

fn clock(out: &mut Vec<IconPath>) {
    let mut face = Outline::new();
    face.push_circle(0.50, 0.50, 0.34);
    finish(face, Ink::Stroke(0.08), out);

    let mut hands = Outline::new();
    line(&mut hands, 0.50, 0.50, 0.50, 0.28);
    line(&mut hands, 0.50, 0.50, 0.66, 0.58);
    finish(hands, Ink::Stroke(0.08), out);
}

fn ethernet(out: &mut Vec<IconPath>) {
    // An RJ45 plug: body above, contacts below, cable out of the top.
    let mut cable = Outline::new();
    line(&mut cable, 0.50, 0.14, 0.50, 0.28);
    finish(cable, Ink::Stroke(0.09), out);

    let mut body = Outline::new();
    rounded(&mut body, 0.22, 0.28, 0.78, 0.62, 0.06);
    finish(body, Ink::Fill, out);

    let mut pins = Outline::new();
    for i in 0..3 {
        let x = 0.33 + i as f32 * 0.17;
        line(&mut pins, x, 0.62, x, 0.80);
    }
    finish(pins, Ink::Stroke(0.09), out);
}

fn keyboard(out: &mut Vec<IconPath>) {
    let mut body = Outline::new();
    rounded(&mut body, 0.06, 0.28, 0.94, 0.72, 0.08);
    finish(body, Ink::Stroke(0.07), out);

    // Two rows of keys and a spacebar. Round caps make the short strokes read as keys at
    // the size a bar draws this.
    let mut keys = Outline::new();
    for row in 0..2 {
        let y = 0.40 + row as f32 * 0.12;
        for column in 0..4 {
            let x = 0.19 + column as f32 * 0.18;
            line(&mut keys, x, y, x + 0.08, y);
        }
    }
    line(&mut keys, 0.32, 0.62, 0.68, 0.62);
    finish(keys, Ink::Stroke(0.07), out);
}

/// How much longer than tall a battery is drawn.
///
/// A quarter over, which is what it takes to stop reading as a box, and which lands on a
/// whole pixel at every icon size a whole-pixel font gives: 16 becomes 20, and the scaled
/// sizes an output asks for follow.
const BATTERY_WIDTH: f32 = 1.25;

/// The battery's own geometry, shared by both of its drawings.
///
/// A battery is a wide, shallow thing, so it is drawn as tall as the box allows and as
/// long as it: at the sizes a bar uses the body is eleven or twelve pixels of it, and
/// every one of them is the difference between a battery and a dash. It runs to the edges
/// of its box, which is why the box is wider than the others to begin with.
const BODY: (f32, f32, f32, f32) = (0.04, 0.188, 1.07, 0.812);
/// How far the charge sits inside the shell, which is the stroke plus a hair of daylight.
const INSET: f32 = 0.06;

/// The charge bar for a battery at this level, and the room left inside the shell.
fn charge(level: usize) -> (f32, f32, f32, f32) {
    let (x0, y0, x1, y1) = BODY;
    let (fx0, fx1) = (x0 + INSET, x1 - INSET);
    let filled = fx0 + (fx1 - fx0) * (level + 1) as f32 / LEVELS as f32;
    (fx0, y0 + INSET, filled, y1 - INSET)
}

fn battery(out: &mut Vec<IconPath>, level: usize) {
    shell(out);
    let (fx0, fy0, filled, fy1) = charge(level);
    let mut fill = Outline::new();
    rounded(&mut fill, fx0, fy0, filled, fy1, 0.03);
    finish(fill, Ink::Fill, out);
}

/// The shell and its cap, which are what say "battery" before the charge says anything.
fn shell(out: &mut Vec<IconPath>) {
    let (x0, y0, x1, y1) = BODY;
    let mut shell = Outline::new();
    rounded(&mut shell, x0, y0, x1, y1, 0.08);
    finish(shell, Ink::Stroke(0.08), out);

    let mut cap = Outline::new();
    rounded(&mut cap, x1 + 0.05, 0.40, x1 + 0.16, 0.60, 0.03);
    finish(cap, Ink::Fill, out);
}

/// A charging battery: the same charge bar as `battery`, with a bolt through it.
///
/// The bolt and the bar share one even-odd path, so the bolt reads as solid where the
/// battery is empty and as a cut-out where it is full. Drawing it on top in the same colour
/// would make it vanish over the bar.
///
/// It is drawn well inside the shell rather than across it. A bolt that reaches the walls
/// has no daylight left to read against once the charge is behind it, which is what made a
/// half-charged battery look like a smudge at bar sizes.
fn battery_charging(out: &mut Vec<IconPath>, level: usize) {
    shell(out);
    let (fx0, fy0, filled, fy1) = charge(level);

    let mut combined = Outline::new();
    rounded(&mut combined, fx0, fy0, filled, fy1, 0.03);
    bolt(&mut combined, (fy0 + fy1) / 2.0);
    finish(combined, Ink::FillEvenOdd, out);
}

/// A lightning bolt, centred on `y` and standing clear of the shell either side of it.
///
/// It grows sideways rather than up: the shell is only eight pixels deep at bar sizes, and
/// a bolt that takes the last of that has nothing left to read against once the charge is
/// behind it. Across the battery there is room to spare, so that is where the weight goes.
fn bolt(out: &mut Outline, y: f32) {
    // Half the height and half the width of the bolt, in unit space.
    const H: f32 = 0.222;
    const W: f32 = 0.21;
    let (cx, cy) = (0.555, y);
    let at = |x: f32, y: f32| (cx + x * W, cy + y * H);
    let (sx, sy) = at(0.55, -1.0);
    out.move_to(sx, sy);
    for (x, y) in [
        (-1.0, 0.1),
        (-0.2, 0.1),
        (-0.45, 1.0),
        (1.0, -0.15),
        (0.2, -0.15),
    ] {
        let (px, py) = at(x, y);
        out.line_to(px, py);
    }
    out.close();
}

/// Headphones: a headband over two earcups.
fn headphones(out: &mut Vec<IconPath>) {
    let mut band = Outline::new();
    arc(
        &mut band,
        0.50,
        0.56,
        0.32,
        std::f32::consts::PI,
        2.0 * std::f32::consts::PI,
    );
    finish(band, Ink::Stroke(0.10), out);

    let mut cups = Outline::new();
    rounded(&mut cups, 0.10, 0.53, 0.29, 0.84, 0.075);
    rounded(&mut cups, 0.71, 0.53, 0.90, 0.84, 0.075);
    finish(cups, Ink::Fill, out);
}

fn wifi(out: &mut Vec<IconPath>, level: usize) {
    let (cx, cy) = (0.50, 0.74);
    let mut dot = Outline::new();
    dot.push_circle(cx, cy, 0.07);
    finish(dot, Ink::Fill, out);

    // Level 0 is the dot alone; each further level adds an arc.
    //
    // The fan is a quarter turn either side of straight up, and the outermost arc stops
    // just inside the box: a wider sweep at this radius would put the ends of the top arc
    // past the edges, where the rasteriser cuts them off flat.
    let mut arcs = Outline::new();
    let (from, to) = (-std::f32::consts::PI * 0.75, -std::f32::consts::PI * 0.25);
    for i in 0..level {
        arc(&mut arcs, cx, cy, 0.20 + i as f32 * 0.15, from, to);
    }
    finish(arcs, Ink::Stroke(0.08), out);
}

fn volume(out: &mut Vec<IconPath>, level: usize) {
    let mut body = Outline::new();
    body.move_to(0.12, 0.38);
    body.line_to(0.28, 0.38);
    body.line_to(0.46, 0.20);
    body.line_to(0.46, 0.80);
    body.line_to(0.28, 0.62);
    body.line_to(0.12, 0.62);
    body.close();
    finish(body, Ink::Fill, out);

    // One wave per level, so all five steps stay distinct; level 0 is the speaker alone.
    let mut arcs = Outline::new();
    let (from, to) = (-std::f32::consts::FRAC_PI_3, std::f32::consts::FRAC_PI_3);
    for i in 0..level {
        arc(&mut arcs, 0.46, 0.50, 0.15 + i as f32 * 0.11, from, to);
    }
    finish(arcs, Ink::Stroke(0.08), out);
}

/// A triangle pointing the way a track runs.
fn play(out: &mut Vec<IconPath>) {
    let mut pb = Outline::new();
    pb.move_to(0.32, 0.22);
    pb.line_to(0.78, 0.50);
    pb.line_to(0.32, 0.78);
    pb.close();
    finish(pb, Ink::Fill, out);
}

/// Two bars, the width of the gap between them, which is what makes it read as pause
/// rather than as a pair of unrelated marks.
fn pause(out: &mut Vec<IconPath>) {
    let mut pb = Outline::new();
    rounded(&mut pb, 0.32, 0.22, 0.45, 0.78, 0.03);
    rounded(&mut pb, 0.55, 0.22, 0.68, 0.78, 0.03);
    finish(pb, Ink::Fill, out);
}

/// Two beamed notes: a mark that says "a player lives here" and nothing about clicking.
///
/// Constructed rather than traced, so every number is geometry of dbar's own and the icon
/// carries nobody else's artwork: circles for the heads, capsules for the stems, one bar
/// across their tops for the beam. Filled as one set, which unions them - they overlap on
/// purpose, and a stem that stopped at the head would show the seam.
///
/// Play, pause and headphones are all buttons: each says what pressing it does. A folded
/// module is the one thing left to click and pressing it only unfolds, so it needs a mark
/// that names the module instead of promising something it will not do.
fn media(out: &mut Vec<IconPath>) {
    // One baseline and a level beam. Notation slants the beam and staggers the heads, and
    // at the twenty-odd pixels a bar draws an icon in, both read as a mistake.
    const R: f32 = 0.125;
    const STEM: f32 = 0.085;
    const BEAM: f32 = 0.11;
    const LEFT: f32 = 0.315;
    const RIGHT: f32 = 0.685;
    const BASE: f32 = 0.685;
    const TOP: f32 = 0.19;

    let mut pb = Outline::new();
    pb.push_circle(LEFT, BASE, R);
    pb.push_circle(RIGHT, BASE, R);
    // Up the right of each head, which is the side a stem rises from.
    rounded(&mut pb, LEFT + R - STEM, TOP, LEFT + R, BASE, STEM / 2.0);
    rounded(&mut pb, RIGHT + R - STEM, TOP, RIGHT + R, BASE, STEM / 2.0);
    // Across both, which is what makes the pair read as one mark rather than two notes.
    rounded(
        &mut pb,
        LEFT + R - STEM,
        TOP,
        RIGHT + R,
        TOP + BEAM,
        STEM / 2.0,
    );
    finish(pb, Ink::Fill, out);
}

/// A thermometer whose column rises with the level.
///
/// The bulb is always full, because a thermometer with an empty bulb reads as broken
/// rather than as cold, and the column above it is what carries the level.
fn temperature(out: &mut Vec<IconPath>, level: usize) {
    const TOP: f32 = 0.16;
    const NECK: f32 = 0.66;
    const BULB: f32 = 0.76;

    // The tube: an outline the column then rises inside.
    let mut tube = Outline::new();
    rounded(&mut tube, 0.41, TOP, 0.59, NECK, 0.09);
    finish(tube, Ink::Stroke(0.07), out);

    let mut bulb = Outline::new();
    bulb.push_circle(0.50, BULB, 0.145);
    finish(bulb, Ink::Stroke(0.07), out);

    // The mercury: the bulb, and a column standing on it.
    let mut mercury = Outline::new();
    mercury.push_circle(0.50, BULB, 0.085);
    let t = level as f32 / (LEVELS - 1) as f32;
    // Level zero still shows a little in the neck, or a cold module looks like a module
    // whose sensor has stopped.
    let top = NECK - 0.06 - t * (NECK - TOP - 0.16);
    rounded(&mut mercury, 0.455, top, 0.545, BULB, 0.045);
    finish(mercury, Ink::Fill, out);

    // Two graduations, which say thermometer rather than test tube.
    let mut marks = Outline::new();
    for i in 0..2 {
        let y = TOP + 0.12 + i as f32 * 0.14;
        line(&mut marks, 0.63, y, 0.73, y);
    }
    finish(marks, Ink::Stroke(0.06), out);
}

fn brightness(out: &mut Vec<IconPath>, level: usize) {
    // The core grows and the rays lengthen with the level.
    let t = level as f32 / (LEVELS - 1) as f32;
    let mut core = Outline::new();
    core.push_circle(0.50, 0.50, 0.14 + 0.06 * t);
    finish(core, Ink::Fill, out);

    let inner = 0.26 + 0.06 * t;
    let outer = inner + 0.08 + 0.06 * t;
    let mut rays = Outline::new();
    for i in 0..8 {
        let a = i as f32 * std::f32::consts::FRAC_PI_4;
        let (c, s) = (a.cos(), a.sin());
        line(
            &mut rays,
            0.50 + inner * c,
            0.50 + inner * s,
            0.50 + outer * c,
            0.50 + outer * s,
        );
    }
    finish(rays, Ink::Stroke(0.08), out);
}

/// How many steps a spinner turns through before it is back where it started.
///
/// Twenty-four at the rate the bar animates is a turn and a half a second, and a step of
/// fifteen degrees, which is small enough that the head reads as sweeping rather than as
/// jumping between positions. It is not `LEVELS` because it is not a grading: nothing
/// measures a spinner, it just goes round.
pub const SPINNER_FRAMES: usize = 24;

/// How far round the circle the comet reaches, from its head back to the end of its tail.
const SPINNER_SPAN: f32 = std::f32::consts::TAU * 0.38;
/// How far the comet sits from the middle of its box.
const SPINNER_RADIUS: f32 = 0.35;
/// How thick the comet is at its head and at the end of its tail.
const SPINNER_HEAD: f32 = 0.135;
const SPINNER_TAIL: f32 = 0.03;
/// How many pieces the taper is drawn in. Enough that the steps between them are smaller
/// than the round caps that cover them, and few enough to stay a handful of paths.
const SPINNER_SEGMENTS: usize = 7;

/// A spinner, as a comet at `frame` of its turn: thick at the head, tapering to nothing
/// behind it, so the direction it is going is visible in the shape and not only in the
/// movement.
///
/// The taper is several stroked arcs rather than one, because an icon path carries a
/// single stroke width. The renderer caps every stroke round, so each piece ends in a
/// half-disc that the next piece's own cap covers, and what a cache of them draws is a
/// continuous shape rather than seven of them. A fade would have been the other way to
/// say the same thing, and an icon is painted in one colour.
fn spinner(out: &mut Vec<IconPath>, frame: usize) {
    let head = std::f32::consts::TAU * frame as f32 / SPINNER_FRAMES as f32;
    for piece in 0..SPINNER_SEGMENTS {
        // How far back along the tail this piece runs, as a share of the whole comet.
        let (near, far) = (
            piece as f32 / SPINNER_SEGMENTS as f32,
            (piece + 1) as f32 / SPINNER_SEGMENTS as f32,
        );
        let mut pb = Outline::new();
        arc(
            &mut pb,
            0.5,
            0.5,
            SPINNER_RADIUS,
            head - SPINNER_SPAN * far,
            head - SPINNER_SPAN * near,
        );
        // Thickness is taken in the middle of the piece, so the two ends of the comet are
        // its true head and tail rather than the widths of the pieces that reach them.
        let along = (near + far) / 2.0;
        finish(
            pb,
            Ink::Stroke(SPINNER_HEAD + (SPINNER_TAIL - SPINNER_HEAD) * along),
            out,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Icon; 26] = [
        Icon::Cpu,
        Icon::Tux,
        Icon::Arch,
        Icon::Slack,
        Icon::Code,
        Icon::Chrome,
        Icon::Memory,
        Icon::Disk,
        Icon::Clock,
        Icon::Ethernet,
        Icon::Battery,
        Icon::BatteryCharging,
        Icon::Wifi,
        Icon::Volume,
        Icon::Brightness,
        Icon::Temperature,
        Icon::VolumeMuted,
        Icon::WifiOff,
        Icon::Headphones,
        Icon::HeadphonesMuted,
        Icon::Play,
        Icon::Pause,
        Icon::Media,
        Icon::Keyboard,
        Icon::Spinner,
        Icon::Raster,
    ];

    /// Every name `Icon::parse` accepts, which is the whole written vocabulary.
    const NAMES: &[&str] = &[
        "cpu",
        "tux",
        "arch",
        "arch-linux",
        "slack",
        "code",
        "chrome",
        "chromium",
        "memory",
        "ram",
        "disk",
        "clock",
        "time",
        "ethernet",
        "battery",
        "battery-charging",
        "wifi",
        "network",
        "volume",
        "brightness",
        "temperature",
        "temp",
        "volume-muted",
        "wifi-off",
        "headphones",
        "headphones-muted",
        "play",
        "pause",
        "media",
        "music",
        "keyboard",
        "language",
    ];

    /// The README names every icon a config can ask for, and names nothing else.
    ///
    /// It drifted once already, in both directions at once: icons were added and the list
    /// kept the old set, and the change that made `$name` the way to write one left every
    /// bare example meaning a glyph instead. A test is the only thing that notices, since
    /// nothing else reads the README.
    #[test]
    fn the_readme_names_every_icon_a_config_can_ask_for() {
        const README: &str = include_str!("../README.md");
        // Spelled with the sigil, so this cannot pass on a bare word the config would now
        // read as text, and in backticks, so prose about a module never stands in for it.
        for name in NAMES {
            assert!(
                Icon::parse(name).is_some(),
                "the README vocabulary has {name:?}, which no longer parses"
            );
            assert!(
                README.contains(&format!("`${name}`")),
                "the README does not name `${name}`"
            );
        }
        // Everything drawable is reachable by one of those names. Raster is a tray item's
        // own artwork and Spinner is drawn while a command is out; neither is written.
        for icon in ALL {
            if matches!(icon, Icon::Raster | Icon::Spinner) {
                continue;
            }
            assert!(
                NAMES.iter().any(|name| Icon::parse(name) == Some(icon)),
                "{icon:?} can be drawn but not written, so the README cannot name it"
            );
        }
    }

    /// Extent of one icon's ink, stroke width included.
    fn ink_bounds(icon: Icon, level: usize) -> Option<(f32, f32, f32, f32)> {
        let IconArt::Paths(paths) = art(icon, level) else {
            return None;
        };
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for item in &paths {
            // A stroke is centred on the path, and a round cap or join reaches half its
            // width past the geometry in every direction.
            let pad = match item.ink {
                Ink::Stroke(w) => w / 2.0,
                _ => 0.0,
            };
            let mut at = |p: &Point| {
                x0 = x0.min(p.x - pad);
                y0 = y0.min(p.y - pad);
                x1 = x1.max(p.x + pad);
                y1 = y1.max(p.y + pad);
            };
            let mut cur = Point { x: 0.0, y: 0.0 };
            for cmd in &item.cmds {
                match *cmd {
                    PathCmd::MoveTo(p) | PathCmd::LineTo(p) => {
                        at(&p);
                        cur = p;
                    }
                    PathCmd::CubicTo(c1, c2, p) => {
                        // Sample the curve rather than taking its hull: the control points
                        // of a circular arc sit outside the circle, and a bound taken from
                        // them would fail an icon that is really inside the square.
                        for i in 0..=32 {
                            let t = i as f32 / 32.0;
                            let u = 1.0 - t;
                            at(&Point {
                                x: u * u * u * cur.x
                                    + 3.0 * u * u * t * c1.x
                                    + 3.0 * u * t * t * c2.x
                                    + t * t * t * p.x,
                                y: u * u * u * cur.y
                                    + 3.0 * u * u * t * c1.y
                                    + 3.0 * u * t * t * c2.y
                                    + t * t * t * p.y,
                            });
                        }
                        cur = p;
                    }
                    PathCmd::Close => {}
                }
            }
        }
        (x0 <= x1).then_some((x0, y0, x1, y1))
    }

    /// Every icon has to be drawn inside the box layout reserves for it, or the rasteriser
    /// cuts whatever hangs over the edge.
    #[test]
    fn icons_stay_inside_their_box() {
        for icon in ALL {
            for level in 0..icon.frames() {
                let Some((x0, y0, x1, y1)) = ink_bounds(icon, level) else {
                    continue;
                };
                let w = icon.width();
                assert!(
                    x0 >= -0.001 && y0 >= -0.001 && x1 <= w + 0.001 && y1 <= 1.001,
                    "{icon:?} level {level} draws outside 0..{w} x 0..1: \
                     ({x0:.3}, {y0:.3})..({x1:.3}, {y1:.3})"
                );
            }
        }
    }

    #[test]
    fn code_ink_has_its_inset_and_is_centred() {
        let (x0, y0, x1, y1) = ink_bounds(Icon::Code, 0).expect("code has visible ink");
        assert!((y0 - 0.09).abs() < 0.001, "{y0}");
        assert!((y1 - 0.91).abs() < 0.001, "{y1}");
        assert!(((y0 + y1) / 2.0 - 0.5).abs() < 0.001);
        // The braces reach further than anything else in this icon, so the horizontal
        // inset is the one that decides whether it sits heavier than the mark beside it.
        assert!((x0 - 0.06).abs() < 0.001, "{x0}");
        assert!((x1 - 0.94).abs() < 0.001, "{x1}");
        assert!(((x0 + x1) / 2.0 - 0.5).abs() < 0.001);
    }
}
