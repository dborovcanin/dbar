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
    for (a, b) in written
        .as_chunks::<4>()
        .0
        .iter()
        .zip(swapped.as_chunks::<4>().0)
    {
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
                    module_folding: &Default::default(),
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

/// What one travelling module costs to lay out and paint.
///
/// Ignored for the same reason as the other timing probes: this reports the machine it
/// runs on rather than passing or failing. Run old and new code back to back and compare
/// medians instead of treating one noisy sample as a result.
#[test]
#[ignore = "release module-fold timing probe; run with --release --ignored --nocapture"]
fn benchmark_module_collapse() {
    use crate::{
        collect::Registry,
        layout::Inputs,
        status::{Fields, State, StatusItem, Value},
    };
    use std::{hint::black_box, time::Instant};

    let cfg = Config::parse(
        r##"
[bar]
height = 30
[left]
groups = ["system"]
[group.system]
modules = ["cpu", "memory", "clock"]
padding = 2
spacing = 4
background = "#313244"
[module.cpu]
format = "$text"
padding = 8
icon = "$cpu"
collapsible = true
collapse_animation = "150ms"
background = "#45475a"
collapsed = { padding = 4, background = "#585b70" }
[module.memory]
format = "$text"
padding = 8
icon = "$memory"
[module.clock]
format = "$text"
padding = 8
icon = "$clock"
"##,
    )
    .expect("the module-fold bench config parses");
    let items: Vec<_> = [
        ("cpu", "CPU 12%"),
        ("memory", "Memory 34%"),
        ("clock", "12:34"),
    ]
    .into_iter()
    .map(|(name, value)| {
        let mut fields = Fields::default();
        fields.set("text", Value::Text(value.to_string()));
        StatusItem {
            id: Some(name.to_string()),
            fields,
            state: State::Idle,
            urgent: false,
            foreground: None,
            background: None,
            action: None,
        }
    })
    .collect();
    let native = Registry::new(&Default::default());
    let collapsed = ["cpu".to_string()].into();
    let module_folding = [("cpu".to_string(), 0.5)].into();
    let inputs = Inputs {
        items: &items,
        native: &native,
        sway: &Default::default(),
        alt: &Default::default(),
        pages: &Default::default(),
        collapsed: &collapsed,
        collapsed_groups: &Default::default(),
        switching: &Default::default(),
        folding: &Default::default(),
        module_folding: &module_folding,
        waiting: &Default::default(),
        spin: 0,
        tray: &Default::default(),
        output: None,
    };
    let mut painter = Painter::new(Blocks {
        scale: 1.0,
        run: None,
    });

    for _ in 0..100 {
        black_box(crate::layout::compute(
            &cfg,
            &inputs,
            1920.0,
            30.0,
            &mut painter.text,
            None,
        ));
    }
    let start = Instant::now();
    for _ in 0..20_000 {
        black_box(crate::layout::compute(
            &cfg,
            &inputs,
            1920.0,
            30.0,
            &mut painter.text,
            None,
        ));
    }
    let layout = start.elapsed().as_secs_f64() * 1e6 / 20_000.0;

    let frame = crate::layout::compute(&cfg, &inputs, 1920.0, 30.0, &mut painter.text, None);
    let (pw, ph) = (1920, 30);
    let mut canvas = vec![0; pw as usize * ph as usize * 4];
    let mut clip = Clip::default();
    let mut paint = || {
        render_to_buffer(
            Target {
                canvas: &mut canvas,
                width: pw,
                height: ph,
                clip: &mut clip,
                pixels: Pixels::AsWritten,
            },
            &frame,
            1.0,
            &mut painter,
        )
        .expect("painting the module-fold bench frame");
    };
    for _ in 0..20 {
        paint();
    }
    let start = Instant::now();
    for _ in 0..1_000 {
        paint();
    }
    let paint = start.elapsed().as_secs_f64() * 1e6 / 1_000.0;
    println!("module fold layout={layout:.2} us/frame paint={paint:.2} us/frame");
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

/// A chevron comes to a point, and a point is one row of pixels.
///
/// Two ways to lose it, and a bar the height of an even number of pixels finds both.
/// The tip is built into the bleed the separator lays past its own gap, and the module
/// on that side is drawn afterwards and over the top of it: the point is cut off at
/// the module's edge and left as flat as the bleed is wide. And halfway down an even
/// number of rows is the boundary between two of them, so a tip left sitting there is
/// shared out equally between both. Either one reads as a chevron that did not render.
#[test]
fn a_chevron_comes_to_a_point_rather_than_to_a_flat() {
    for scale in [1.0, 1.5, 2.0] {
        for height in [32.0f32, 30.0, 24.0] {
            for direction in [Direction::Left, Direction::Right] {
                let (w, h) = (64.0f32, height);
                let mut pixmap = Pixmap::new((w * scale) as u32, (h * scale) as u32).unwrap();
                let sep = PlacedSeparator {
                    x: 24.484_375,
                    y: 0.0,
                    width: 14.0,
                    height: h,
                    shape: SeparatorShape::Chevron,
                    direction,
                    overlap: 1.0,
                    inverted: false,
                    cap: false,
                    fill: TILE,
                    under: TILE_ALT,
                };
                draw_separator(
                    &mut pixmap.as_mut(),
                    &sep,
                    scale,
                    Transform::from_scale(scale, scale),
                    None,
                );
                // The modules either side, drawn after the separator the way a group
                // draws them, which is what cuts a tip built out past the gap.
                let (edge, side) = (snap(sep.x, scale), snap(sep.x + sep.width, scale));
                for (x0, x1, color) in [(0.0, edge, TILE), (side, w, TILE_ALT)] {
                    fill(
                        &mut pixmap.as_mut(),
                        (x0, 0.0, x1 - x0, h),
                        0.0,
                        color,
                        Transform::from_scale(scale, scale),
                        None,
                    );
                }

                // The wedge points into the module whose colour it is not, so the row
                // it reaches furthest along is the row holding the point - and a point
                // is one row. Counting the colour it points with says so whichever way
                // round the separator is drawn.
                let point = match direction {
                    Direction::Left => TILE_ALT,
                    Direction::Right => TILE,
                };
                let point = skia_color(point).to_color_u8();
                let reach: Vec<_> = (0..(h * scale) as u32)
                    .map(|y| {
                        (0..(w * scale) as u32)
                            .filter(|&x| pixmap.pixel(x, y).map(|p| p.demultiply()) == Some(point))
                            .count()
                    })
                    .collect();
                let tip = reach.iter().copied().max().unwrap();
                let rows = reach.iter().filter(|&&n| n == tip).count();
                assert_eq!(
                    rows, 1,
                    "scale {scale}, height {height}, {direction:?}: {rows} rows share \
                         the point"
                );
            }
        }
    }
}

/// A transition leans from the first row it is drawn in.
///
/// The modules either side of a separator are drawn after it and over the top, so a
/// shape built wider than the gap has its own antialiased edge replaced by a module's
/// hard one for as long as the two overlap. On a tall bar that is most of a column's
/// worth of rows at each end, and the lean the shape is there to draw starts a step
/// late: the transition stands straight out of the top of the bar, then bends.
#[test]
fn a_transition_leans_from_the_first_row_it_is_drawn_in() {
    for (direction, cap) in [
        (Direction::Left, false),
        (Direction::Right, false),
        (Direction::Left, true),
        (Direction::Right, true),
    ] {
        let (w, h) = (64.0f32, 320.0f32);
        let mut pixmap = Pixmap::new(w as u32, h as u32).unwrap();
        let sep = PlacedSeparator {
            x: 24.484_375,
            y: 0.0,
            width: 14.0,
            height: h,
            shape: SeparatorShape::Chevron,
            direction,
            overlap: 1.0,
            inverted: false,
            cap,
            fill: TILE,
            under: TILE_ALT,
        };
        draw_separator(&mut pixmap.as_mut(), &sep, 1.0, Transform::identity(), None);
        // A cap is drawn where the island ends, so only the side its own module is on
        // is filled in behind it. That side is the one the shape has its base on, which
        // is the side `direction` does not point at.
        let (edge, side) = (snap(sep.x, 1.0), snap(sep.x + sep.width, 1.0));
        let behind = [
            (0.0, edge, TILE, direction == Direction::Right || !cap),
            (side, w, TILE_ALT, direction == Direction::Left || !cap),
        ];
        for (x0, x1, color, _) in behind.into_iter().filter(|&(.., drawn)| drawn) {
            fill(
                &mut pixmap.as_mut(),
                (x0, 0.0, x1 - x0, h),
                0.0,
                color,
                Transform::identity(),
                None,
            );
        }

        // How many rows in a row read the same. A shape this tall and this narrow moves
        // its boundary one column every several of them, so the picture is a staircase
        // and the question is only whether the step it starts on is the size of the
        // rest.
        let point = match direction {
            Direction::Left => TILE_ALT,
            Direction::Right => TILE,
        };
        let point = skia_color(point).to_color_u8();
        let reach = (0..h as u32).map(|y| {
            (0..w as u32)
                .filter(|&x| pixmap.pixel(x, y).map(|p| p.demultiply()) == Some(point))
                .count()
        });
        let mut steps = vec![];
        for n in reach {
            match steps.last_mut() {
                Some((seen, count)) if *seen == n => *count += 1,
                _ => steps.push((n, 1usize)),
            }
        }
        // Against the step beside it rather than against the tallest anywhere: a
        // staircase is even, so an end that is drawn the way the middle is has an end
        // step no deeper than its neighbour, give or take the row rounding puts there.
        let ends = [
            ("first", steps[0].1, steps[1].1),
            ("last", steps[steps.len() - 1].1, steps[steps.len() - 2].1),
        ];
        for (which, end, next) in ends {
            assert!(
                end <= next + 1,
                "{direction:?} cap {cap}: the staircase runs {end} rows into its \
                     {which} step \
                     against {next} for the one beside it, so the transition stood \
                     straight before it began to lean"
            );
        }
    }
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
        content_right: None,
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

/// A font's line box is not centred on its own ink: the ascent leaves more room above
/// the letters than the descent leaves below, so centring the box puts every wording a
/// little high. What a module centres is where the ink is.
#[test]
fn a_wording_is_centred_on_its_ink_rather_than_on_its_line_box() {
    /// A backend whose ink sits high in its line, the way a real font's does.
    struct Lopsided {
        run: Option<TextRun>,
    }

    const LINE: f32 = 12.0;
    const INK_TOP: f32 = 2.0;
    const INK: f32 = 6.0;

    impl DrawText for Lopsided {
        fn line_height(&self) -> f32 {
            LINE
        }

        fn middle(&mut self) -> f32 {
            INK_TOP + INK / 2.0
        }

        fn run(&mut self, text: &str) -> Option<&TextRun> {
            let width = text.chars().count() * INK as usize;
            self.run = Some(TextRun {
                left: 0,
                top: INK_TOP as i32,
                width,
                height: INK as usize,
                pixels: RunPixels::Coverage(vec![0xff; width * INK as usize]),
            });
            self.run.as_ref()
        }
    }

    let frame = Frame {
        groups: vec![PlacedGroup {
            modules: vec![module(0.0, 40.0, TILE, "ab", false)],
            ..island(1.0).groups.remove(0)
        }],
        ..Frame::default()
    };
    let mut pixmap = Pixmap::new(40, 20).unwrap();
    render(
        &mut pixmap.as_mut(),
        &frame,
        1.0,
        &mut Painter::new(Lopsided { run: None }),
        &mut Clip::default(),
    );

    let inked =
        |y: usize| (0..40).any(|x| pixmap.pixels()[y * 40 + x].red() > TILE.r.max(TILE_ALT.r));
    let rows: Vec<usize> = (0..20).filter(|&y| inked(y)).collect();
    let module = &frame.groups[0].modules[0];
    let (top, bottom) = (rows[0] as f32, rows[rows.len() - 1] as f32 + 1.0);
    assert_eq!(
        (top - module.y, module.y + module.height - bottom),
        (7.0, 7.0),
        "ink in rows {top}..{bottom} of a module {} tall - it is not centred",
        module.height
    );
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
collapsed = { icon = '$cpu', padding = 6 }
edges = { left = 'round', right = 'round' }
[module.cpu]
format = '$text'
padding = 6
icon = '$cpu'
background = '#cc241d'
[module.memory]
format = '$text'
padding = 6
icon = '$memory'
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
            module_folding: &Default::default(),
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
collapsed = { icon = '$cpu', icon_size = 8, padding = 12, background = '#83a598', foreground = '#282828' }
edges = { left = 'round', right = 'round' }
ends = { left = 'none', right = 'slant', width = 12 }
[module.cpu]
format = '$text'
padding = 0
icon_gap = 3
icon = '$cpu'
background = '#cc241d'
foreground = '#ebdbb2'
[module.memory]
format = '$text'
padding = 0
icon_gap = 3
icon = '$memory'
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
                module_folding: &Default::default(),
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
        module_folding: &Default::default(),
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
             collapsed = { icon = '$cpu', padding = 0, background = '#83a598' }\n\
             [module.cpu]\nformat = '$text'\npadding = 0\nicon = '$cpu'\n\
             background = '#cc241d'\nforeground = '#ebdbb2'\n\
             [module.memory]\nformat = '$text'\npadding = 0\nicon = '$memory'\n\
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
            module_folding: &Default::default(),
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
            module_folding: &Default::default(),
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
        config += &format!("[right]\ngroups = ['g0','g1','g2','g3']\nseparator = {separator}\n");
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
    config +=
        "[module.m3.states.hover]\nhover = true\nbackground = '#ffee00'\nforeground = '#000000'\n";
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
        module_folding: &Default::default(),
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
    let mut cfg = Config::parse(include_str!("../../examples/gruvbox-ribbon.toml")).unwrap();
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
        module_folding: &Default::default(),
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
            let mut pixmap = Pixmap::new((480.0 * scale) as u32, (30.0 * scale) as u32).unwrap();
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
             direction = 'right' }\ncollapsed = { icon = '$cpu', icon_size = 8, padding = 2, \
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
            module_folding: &Default::default(),
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
             spacing = 0\ncollapsed = { icon = '$cpu', padding = 4 }\n\
             edges = { left = 'round', right = 'round' }\n\
             [module.a]\nbackground = '#cc241d'\npadding = 4\nformat = '$text'\nicon = '$cpu'\n\
             [module.b]\nbackground = '#458588'\npadding = 4\nformat = '$text'\nicon = '$memory'\n",
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
            module_folding: &Default::default(),
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
             collapsed = { icon = '$cpu', icon_size = 6, padding = 3, \
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
            module_folding: &Default::default(),
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
                    "[group.{name}]\nmodules = ['{name}']\ncollapsible = true\ncollapse_button = 'right'\nbackground = '#3c3836'\nradius = 8\npadding = {}\nopacity = {}\nends = {{ left = 'slant', right = 'slant', direction = '{direction}', width = 6 }}\ncollapsed = {{ icon = '$cpu', icon_size = 10, padding = 3, background = '#83a598' }}\n[module.{name}]\nbackground = '{color}'\npadding = 12\nformat = '$text'\n",
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
                        module_folding: &Default::default(),
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
            module_folding: &Default::default(),
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
        let travelling = frame(&[("a".to_string(), crate::layout::Leaving { from: 0, at })].into());
        let module = &travelling.groups[0].modules[0];
        assert!(module.width <= settled.groups[0].modules[0].width + 0.001);
        overflowed |= module.text.chars().count() as f32 * BLOCK
            > module.content_right.expect("a travelling module is cut") - module.text_x;
        for scale in [1.0, 1.5, 2.0] {
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

/// An icon can belong only to the wording a module is growing towards. It is already
/// full-sized then, so the moving module edge must cut it the same way it cuts text;
/// otherwise the whole icon appears on the first frame while its box is still a sliver.
#[test]
fn a_travelling_icon_stops_at_the_module_it_is_drawn_in() {
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
format = ""
format_alt = "SHOW"
padding = 0
background = "#00000000"
foreground = "#ffffffff"
[module.a.states.show]
contains = "SHOW"
strip = true
icon = "$cpu"
"##;
    let cfg = crate::config::Config::parse(config).unwrap();
    let items = [crate::status::StatusItem {
        id: Some("a".to_string()),
        fields: crate::status::Fields::default(),
        state: Default::default(),
        urgent: false,
        foreground: None,
        background: None,
        action: None,
    }];
    let native = crate::collect::Registry::new(&Default::default());
    let showing: std::collections::HashMap<String, usize> = [("a".to_string(), 1)].into();
    for at in [0.1, 0.5, 0.9] {
        let switching = [("a".to_string(), crate::layout::Leaving { from: 0, at })].into();
        let inputs = crate::layout::Inputs {
            items: &items,
            native: &native,
            sway: &Default::default(),
            alt: &showing,
            pages: &Default::default(),
            collapsed: &Default::default(),
            collapsed_groups: &Default::default(),
            switching: &switching,
            folding: &Default::default(),
            module_folding: &Default::default(),
            waiting: &Default::default(),
            spin: 0,
            tray: &Default::default(),
            output: None,
        };
        let frame = crate::layout::compute(
            &cfg,
            &inputs,
            100.0,
            20.0,
            &mut Blocks {
                scale: 1.0,
                run: None,
            },
            None,
        );
        let module = &frame.groups[0].modules[0];
        for scale in [1.0, 1.5, 2.0] {
            let image = shot(&frame, scale);
            let edge = ((module.x + module.width) * scale).ceil() as u32;
            assert!(
                image
                    .pixels()
                    .iter()
                    .enumerate()
                    .all(|(n, pixel)| { n as u32 % image.width() < edge || pixel.alpha() == 0 }),
                "the icon ran past its module at {at} scale {scale}"
            );
        }
    }
}

/// A module fold has the same endpoint contract as a group fold: progress zero is the
/// open frame and progress one is the settled collapsed frame. The expanded contents stay
/// in place between them while the module box, icon centre, wording cut and collapsed
/// paint travel over those contents.
#[test]
fn module_folds_match_both_endpoints_at_fractional_scales() {
    use crate::{
        collect::Registry,
        layout::Inputs,
        status::{Fields, StatusItem, Value},
    };

    let cases = [
        ("smaller padding", "$cpu", "padding = 1", false),
        ("larger padding", "$cpu", "padding = 8", false),
        (
            "smaller collapsed icon",
            "$cpu",
            "icon_size = 3\npadding = 4",
            false,
        ),
        (
            "larger collapsed icon",
            "$cpu",
            "icon_size = 10\npadding = 4",
            false,
        ),
        (
            "collapsed minimum width",
            "$cpu",
            "padding = 0\nmin_width = 24",
            false,
        ),
        (
            "changed icon",
            "$cpu",
            "icon = '$memory'\npadding = 4",
            true,
        ),
        ("written icon", "µ", "padding = 5", false),
    ];
    for (case, icon, collapsed_style, swaps_icon) in cases {
        let config = format!(
            r##"
[bar]
height = 20
background = {{ color = "#00000000" }}
[left]
groups = ["g"]
[group.g]
modules = ["m", "next"]
padding = 0
spacing = 2
background = "#00000000"
[module.m]
format = "$text"
padding = 4
icon = "{icon}"
icon_size = 6
icon_gap = 2
background = "#cc241d"
foreground = "#ffffffff"
radius = 1
collapsible = true
collapse_animation = "150ms"
[module.m.collapsed]
{collapsed_style}
background = "#458588"
radius = 6
[module.next]
format = "$text"
padding = 2
background = "#98971a"
foreground = "#ffffffff"
"##
        );
        let cfg = Config::parse(&config).unwrap_or_else(|error| panic!("{case}: {error:#}"));
        let mut fields = Fields::default();
        fields.set("text", Value::Text("TEXT".to_string()));
        let mut next_fields = Fields::default();
        next_fields.set("text", Value::Text("N".to_string()));
        let items = [
            StatusItem {
                id: Some("m".to_string()),
                fields,
                state: Default::default(),
                urgent: false,
                foreground: None,
                background: None,
                action: None,
            },
            StatusItem {
                id: Some("next".to_string()),
                fields: next_fields,
                state: Default::default(),
                urgent: false,
                foreground: None,
                background: None,
                action: None,
            },
        ];
        let native = Registry::new(&Default::default());
        let empty = Default::default();
        let collapsed = ["m".to_string()].into();
        let frame = |at: Option<f32>, shut: &std::collections::HashSet<String>| {
            let module_folding = at
                .map(|at| [("m".to_string(), at)].into())
                .unwrap_or_default();
            let inputs = Inputs {
                items: &items,
                native: &native,
                sway: &Default::default(),
                alt: &Default::default(),
                pages: &Default::default(),
                collapsed: shut,
                collapsed_groups: &Default::default(),
                switching: &Default::default(),
                folding: &Default::default(),
                module_folding: &module_folding,
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
        let open = frame(None, &empty);
        let settled = frame(None, &collapsed);

        for direction in [&empty, &collapsed] {
            let leaving = frame(Some(0.0), direction);
            let near = frame(Some(0.999), direction);
            let arriving = frame(Some(1.0), direction);
            let module = &near.groups[0].modules[0];
            if let (false, Some(a), Some(b)) = (
                swaps_icon,
                arriving.groups[0].modules[0].icon.as_ref(),
                settled.groups[0].modules[0].icon.as_ref(),
            ) {
                assert_eq!((a.x, a.y, a.size), (b.x, b.y, b.size), "{case}");
            }
            assert!(module.content_right.is_some(), "{case}");
            assert!(module.text_right < module.content_right, "{case}");

            for scale in [1.0, 1.5, 2.0] {
                assert_eq!(
                    shot(&leaving, scale).data(),
                    shot(&open, scale).data(),
                    "{case}: progress zero differs at scale {scale}"
                );

                // The space between the wording cut and the travelling edge is the
                // collapsed padding. Neither the old wording nor a clipped icon may leave
                // foreground pixels standing in it near arrival.
                let image = shot(&near, scale);
                let from = (module.text_right.unwrap() * scale).ceil() as u32;
                let to = (module.content_right.unwrap() * scale).floor() as u32;
                let y0 = (module.y * scale) as u32;
                let y1 = ((module.y + module.height) * scale) as u32;
                for y in y0..y1 {
                    for x in from..to {
                        let pixel = image.pixel(x, y).unwrap();
                        assert_ne!(
                            (pixel.red(), pixel.green(), pixel.blue()),
                            (255, 255, 255),
                            "{case}: foreground remained in collapsed padding at {x},{y}, scale {scale}"
                        );
                    }
                }

                let mut comparable = arriving.clone();
                if swaps_icon {
                    comparable.groups[0].modules[0].icon.as_mut().unwrap().icon =
                        settled.groups[0].modules[0].icon.as_ref().unwrap().icon;
                }
                assert_eq!(
                    shot(&comparable, scale).data(),
                    shot(&settled, scale).data(),
                    "{case}: progress one differs from settled at scale {scale}"
                );
                if swaps_icon {
                    assert_ne!(
                        shot(&arriving, scale).data(),
                        shot(&settled, scale).data(),
                        "{case}: the fixture did not exercise the documented icon swap"
                    );
                }
            }
        }
    }
}

/// The two independent cuts can cross while a module and its island are folding together.
/// Their geometry composes in layout, and the renderer has to intersect both at the same
/// snapped device columns so the combined hand-off is still exact at fractional scales.
#[test]
fn overlapping_module_and_group_folds_match_their_endpoints_at_fractional_scales() {
    use crate::{
        collect::Registry,
        layout::Inputs,
        status::{Fields, StatusItem, Value},
    };

    let cfg = Config::parse(
        r##"
[bar]
height = 24
background = { color = "#00000000" }
[left]
groups = ["g"]
[group.g]
modules = ["m", "next"]
padding = 2
spacing = 3
background = "#3c3836"
radius = 7
collapsible = true
collapse_button = "left"
collapse_animation = "150ms"
collapsed = { icon = "$cpu", icon_size = 8, padding = 5, background = "#83a598" }
[module.m]
format = "$text"
padding = 2
icon = "$cpu"
icon_size = 10
icon_gap = 2
background = "#cc241d"
foreground = "#ffffff"
collapsible = true
collapse_animation = "150ms"
collapsed = { icon_size = 6, padding = 4, background = "#458588" }
[module.next]
format = "$text"
padding = 2
background = "#98971a"
foreground = "#ffffff"
"##,
    )
    .unwrap();
    let item = |name: &str, value: &str| {
        let mut fields = Fields::default();
        fields.set("text", Value::Text(value.to_string()));
        StatusItem {
            id: Some(name.to_string()),
            fields,
            state: Default::default(),
            urgent: false,
            foreground: None,
            background: None,
            action: None,
        }
    };
    let items = [item("m", "TEXT"), item("next", "N")];
    let native = Registry::new(&Default::default());
    let empty = Default::default();
    let collapsed_modules = ["m".to_string()].into();
    let collapsed_groups = ["g".to_string()].into();
    let frame = |at: Option<f32>, modules, groups| {
        let progress: std::collections::HashMap<String, f32> = at
            .map(|at| [("m".to_string(), at), ("g".to_string(), at)].into())
            .unwrap_or_default();
        let inputs = Inputs {
            items: &items,
            native: &native,
            sway: &Default::default(),
            alt: &Default::default(),
            pages: &Default::default(),
            collapsed: modules,
            collapsed_groups: groups,
            switching: &Default::default(),
            folding: &progress,
            module_folding: &progress,
            waiting: &Default::default(),
            spin: 0,
            tray: &Default::default(),
            output: None,
        };
        crate::layout::compute(
            &cfg,
            &inputs,
            480.0,
            24.0,
            &mut Blocks {
                scale: 1.0,
                run: None,
            },
            None,
        )
    };

    let open = frame(None, &empty, &empty);
    let leaving = frame(Some(0.0), &collapsed_modules, &collapsed_groups);
    let moving = frame(Some(0.63), &collapsed_modules, &collapsed_groups);
    let arriving = frame(Some(1.0), &collapsed_modules, &collapsed_groups);
    let settled = frame(None, &collapsed_modules, &collapsed_groups);
    assert!(moving.groups[0].content_right.is_some());
    assert!(moving.groups[0].modules[0].content_right.is_some());

    for scale in [1.0, 1.5, 2.0] {
        assert_eq!(
            shot(&leaving, scale).data(),
            shot(&open, scale).data(),
            "combined progress zero differs at scale {scale}"
        );
        assert_eq!(
            shot(&arriving, scale).data(),
            shot(&settled, scale).data(),
            "combined progress one differs at scale {scale}"
        );
    }
}
