//! Built-in vector icons.
//!
//! Icons are drawn as geometry in a unit square, so they scale with `icon_size` instead of
//! riding on a font. Graded icons take a level rather than being five separate drawings:
//! a battery is one outline with a fill of varying width, wifi is a dot plus a count of
//! arcs, and so on.

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

/// Artwork for one icon. Built-ins are vector; the raster arm is what SVG and
/// application icons will arrive as later, as premultiplied RGBA at a size.
pub enum IconArt {
    Paths(Vec<IconPath>),
    #[allow(dead_code)]
    Raster {
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
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
    Cpu,
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
    Keyboard,
}

impl Icon {
    pub fn parse(name: &str) -> Option<Icon> {
        Some(match name {
            "cpu" => Icon::Cpu,
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
            "keyboard" | "language" => Icon::Keyboard,
            _ => return None,
        })
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
// Geometry, all inside the unit square
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
    let level = level.min(LEVELS - 1);
    let mut out = Vec::new();
    match icon {
        Icon::Cpu => cpu(&mut out),
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
        Icon::Keyboard => keyboard(&mut out),
    }
    IconArt::Paths(out)
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

fn battery(out: &mut Vec<IconPath>, level: usize) {
    let (x0, x1) = (0.10, 0.80);
    let mut shell = Outline::new();
    rounded(&mut shell, x0, 0.30, x1, 0.70, 0.07);
    finish(shell, Ink::Stroke(0.08), out);

    let mut cap = Outline::new();
    rounded(&mut cap, 0.84, 0.42, 0.92, 0.58, 0.03);
    finish(cap, Ink::Fill, out);

    // The charge fills the shell from the left, one fifth per level.
    let inset = 0.055;
    let (fx0, fx1) = (x0 + inset, x1 - inset);
    let filled = fx0 + (fx1 - fx0) * (level + 1) as f32 / LEVELS as f32;
    let mut fill = Outline::new();
    rounded(&mut fill, fx0, 0.30 + inset, filled, 0.70 - inset, 0.03);
    finish(fill, Ink::Fill, out);
}

/// A charging battery: the same charge bar as `battery`, with a bolt through it.
///
/// The bolt and the bar share one even-odd path, so the bolt reads as solid where the
/// battery is empty and as a cut-out where it is full. Drawing it on top in the same colour
/// would make it vanish over the bar.
fn battery_charging(out: &mut Vec<IconPath>, level: usize) {
    let (x0, x1) = (0.10, 0.80);
    let mut shell = Outline::new();
    rounded(&mut shell, x0, 0.30, x1, 0.70, 0.07);
    finish(shell, Ink::Stroke(0.08), out);

    let mut cap = Outline::new();
    rounded(&mut cap, 0.84, 0.42, 0.92, 0.58, 0.03);
    finish(cap, Ink::Fill, out);

    let inset = 0.055;
    let (fx0, fx1) = (x0 + inset, x1 - inset);
    let filled = fx0 + (fx1 - fx0) * (level + 1) as f32 / LEVELS as f32;

    let mut combined = Outline::new();
    rounded(&mut combined, fx0, 0.30 + inset, filled, 0.70 - inset, 0.03);
    combined.move_to(0.49, 0.31);
    combined.line_to(0.30, 0.52);
    combined.line_to(0.41, 0.52);
    combined.line_to(0.38, 0.69);
    combined.line_to(0.57, 0.48);
    combined.line_to(0.46, 0.48);
    combined.close();
    finish(combined, Ink::FillEvenOdd, out);
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
    finish(band, Ink::Stroke(0.08), out);

    let mut cups = Outline::new();
    rounded(&mut cups, 0.12, 0.54, 0.28, 0.82, 0.07);
    rounded(&mut cups, 0.72, 0.54, 0.88, 0.82, 0.07);
    finish(cups, Ink::Fill, out);
}

fn wifi(out: &mut Vec<IconPath>, level: usize) {
    let (cx, cy) = (0.50, 0.74);
    let mut dot = Outline::new();
    dot.push_circle(cx, cy, 0.07);
    finish(dot, Ink::Fill, out);

    // Level 0 is the dot alone; each further level adds an arc.
    let mut arcs = Outline::new();
    let (from, to) = (-std::f32::consts::PI * 0.80, -std::f32::consts::PI * 0.20);
    for i in 0..level {
        arc(&mut arcs, cx, cy, 0.20 + i as f32 * 0.16, from, to);
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
