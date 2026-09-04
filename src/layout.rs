//! Turns config plus the current status items into positioned rectangles.
//!
//! The result is purely geometric: the renderer draws it and the pointer code hit-tests it,
//! neither needs to know about config or where the items came from.

use crate::collect::Registry;
use crate::color::Color;
use crate::config::{
    Config, Direction, EdgeShape, Edges, Group as GroupCfg, Module as ModuleCfg, Separator,
    SeparatorColor, SeparatorShape, Source, StateFlags, Style,
};
use crate::format::Format;
use crate::icon::{self, Icon};
use crate::status::{ActionTarget, Fields, StatusItem, Value};
use crate::sway::SwayState;

/// Everything the bar currently knows, whoever it came from.
///
/// One struct rather than a growing argument list, so adding a source does not touch every
/// signature between here and the event loop.
pub struct Inputs<'a> {
    /// Items from an external status provider.
    pub items: &'a [StatusItem],
    /// The latest reading from each collector dbar runs itself.
    pub native: &'a Registry,
    pub sway: &'a SwayState,
    /// Modules currently showing their second wording, by name.
    pub alt: &'a std::collections::HashSet<String>,
}

/// What layout needs from a text backend: how wide a string is, and how tall a line is.
///
/// Layout is otherwise free of rendering concerns, so it can be exercised with a stub
/// measurer and no font system.
pub trait Measure {
    /// Width of `text` in logical pixels.
    fn measure(&mut self, text: &str) -> f32;
}

/// An icon placed inside a module, already resolved to the level it should draw at.
#[derive(Clone, Copy, Debug)]
pub struct PlacedIcon {
    pub icon: Icon,
    pub level: usize,
    pub x: f32,
    pub y: f32,
    pub size: f32,
}

#[derive(Clone, Debug)]
pub struct PlacedModule {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub icon: Option<PlacedIcon>,
    pub text: String,
    /// Left edge of the text, already offset past any icon.
    pub text_x: f32,
    pub foreground: Color,
    pub background: Color,
    pub radius: f32,
    /// What a click here does, if anything.
    pub action: Option<ActionTarget>,
    /// The module's name, when it has a second wording a click can swap to.
    pub alt: Option<String>,
}

/// A transition drawn in the gap between two neighbouring modules.
#[derive(Clone, Debug)]
pub struct PlacedSeparator {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub shape: SeparatorShape,
    pub direction: Direction,
    pub overlap: f32,
    /// Colour of the region on the leading side of the boundary.
    pub fill: Color,
    /// Colour behind it, on the trailing side.
    pub under: Color,
}

#[derive(Clone, Debug)]
pub struct PlacedGroup {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub background: Color,
    pub edges: Edges,
    pub modules: Vec<PlacedModule>,
    pub separators: Vec<PlacedSeparator>,
}

#[derive(Clone, Debug, Default)]
pub struct Frame {
    pub groups: Vec<PlacedGroup>,
}

fn contains(x: f32, y: f32, rx: f32, ry: f32, rw: f32, rh: f32) -> bool {
    x >= rx && x < rx + rw && y >= ry && y < ry + rh
}

impl Frame {
    /// Identity of the module under a point, for spotting a hover change without laying
    /// the bar out again. Motion within one module leaves this unchanged.
    pub fn hover_key(&self, at: Option<(f32, f32)>) -> Option<(u32, u32)> {
        let (x, y) = at?;
        let module = self.module_at(x, y)?;
        Some((module.x.to_bits(), module.width.to_bits()))
    }

    /// Module under a point in surface coordinates.
    pub fn module_at(&self, x: f32, y: f32) -> Option<&PlacedModule> {
        for group in &self.groups {
            if !contains(x, y, group.x, group.y, group.width, group.height) {
                continue;
            }
            // Fall back to a horizontal-only test so clicks in a group's vertical padding
            // still reach the module they are visually over.
            return group
                .modules
                .iter()
                .find(|m| contains(x, y, m.x, m.y, m.width, m.height))
                .or_else(|| group.modules.iter().find(|m| x >= m.x && x < m.x + m.width));
        }
        None
    }
}

/// A group before it has been given an x position.
struct SizedGroup {
    width: f32,
    background: Color,
    edges: Edges,
    padding: f32,
    /// Horizontal space between neighbouring modules.
    advance: f32,
    separator: Separator,
    /// Module widths paired with their content.
    modules: Vec<SizedModule>,
}

/// Stands in for the text a module could not fit.
const ELLIPSIS: &str = "\u{2026}";

/// The longest prefix of `text` that fits in `budget`, with an ellipsis marking the cut.
///
/// Widths come from the text backend, so the search is over character boundaries by
/// bisection rather than by counting bytes, which would cut multi-byte characters in half.
fn truncate(text: &str, budget: f32, measure: &mut dyn Measure) -> String {
    if measure.measure(text) <= budget {
        return text.to_string();
    }
    let ellipsis = measure.measure(ELLIPSIS);
    if ellipsis > budget {
        return String::new();
    }

    let cuts: Vec<usize> = text
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(text.len()))
        .collect();
    let (mut lo, mut hi, mut best) = (0usize, cuts.len() - 1, 0usize);
    while lo <= hi {
        let mid = (lo + hi) / 2;
        if measure.measure(&text[..cuts[mid]]) + ellipsis <= budget {
            best = mid;
            lo = mid + 1;
        } else if mid == 0 {
            break;
        } else {
            hi = mid - 1;
        }
    }
    format!("{}{ELLIPSIS}", text[..cuts[best]].trim_end())
}

/// The wording a module is currently showing.
///
/// A module with a second wording keeps both parsed; which one is drawn is the only thing
/// a click changes, so nothing has to be re-read or re-collected to swap them.
fn wording<'g>(module: &'g ModuleCfg, alt: &std::collections::HashSet<String>) -> &'g Format {
    match module.format_alt.as_ref() {
        Some(second) if alt.contains(&module.name) => second,
        _ => &module.format,
    }
}

/// Space between an icon and the text beside it, as a fraction of the icon size.
const ICON_GAP: f32 = 0.4;

struct SizedModule {
    width: f32,
    text_width: f32,
    /// Paint overrides applied while the pointer is over this module.
    hover_style: Option<Style>,
    /// Width of the icon plus its gap, or zero.
    icon_advance: f32,
    icon: Option<(Icon, usize)>,
    text: String,
    style: Style,
    foreground: Color,
    background: Color,
    action: Option<ActionTarget>,
    alt: Option<String>,
}

/// Colour used for messages dbar generates itself, matching the i3bar convention.
const FAULT_COLOR: Color = Color::rgba(0xf3, 0x8b, 0xa8, 0xff);

/// One thing a group will draw, before it has been measured.
struct Candidate<'g> {
    module: &'g ModuleCfg,
    text: String,
    flags: StateFlags,
    /// What the source published, so a threshold can read the value it names rather than
    /// the one the format happened to show.
    values: Fields,
    /// Colours the source asked for, which win over the style's own.
    foreground: Option<Color>,
    background: Option<Color>,
    action: Option<ActionTarget>,
}

/// Everything a group shows, in the order the group asks for.
///
/// A module drawn from the compositor expands here: `sway:workspaces` becomes one candidate
/// per workspace, so each is its own rectangle with its own state and click target.
fn collect<'g>(group: &'g GroupCfg, inputs: &Inputs<'_>) -> Vec<Candidate<'g>> {
    let mut out = Vec::new();

    let from_item = |module: &'g ModuleCfg, item: &StatusItem| Candidate {
        module,
        text: wording(module, inputs.alt).render(&item.fields),
        flags: StateFlags {
            urgent: item.urgent,
            state: item.state,
            ..StateFlags::default()
        },
        values: item.fields.clone(),
        foreground: item.foreground,
        background: item.background,
        action: item.action.clone(),
    };

    if group.wildcard {
        if let Some(module) = group.modules.first() {
            out.extend(inputs.items.iter().map(|item| from_item(module, item)));
        }
        return out;
    }

    for module in &group.modules {
        match module.source {
            Source::Native(which) => {
                // A collector that has not read yet has nothing to show, which is the same
                // as a provider that has not spoken: the module simply is not there.
                if let Some(reading) = inputs.native.reading(which) {
                    out.push(Candidate {
                        module,
                        text: wording(module, inputs.alt).render(&reading.fields),
                        flags: StateFlags {
                            state: reading.state,
                            ..StateFlags::default()
                        },
                        values: reading.fields.clone(),
                        foreground: None,
                        background: None,
                        action: None,
                    });
                }
            }
            Source::Provider => {
                for item in inputs.items {
                    if item.id.as_deref() == Some(module.name.as_str()) {
                        out.push(from_item(module, item));
                    }
                }
            }
            Source::SwayWindow => {
                if let Some(title) = &inputs.sway.window {
                    let mut fields = Fields::default();
                    fields.set("title", Value::Text(title.clone()));
                    out.push(Candidate {
                        module,
                        text: wording(module, inputs.alt).render(&fields),
                        flags: StateFlags::default(),
                        values: fields,
                        foreground: None,
                        background: None,
                        action: None,
                    });
                }
            }
            Source::SwayWorkspaces => {
                for workspace in &inputs.sway.workspaces {
                    let mut fields = Fields::default();
                    fields.set("name", Value::Text(workspace.name.clone()));
                    out.push(Candidate {
                        module,
                        text: wording(module, inputs.alt).render(&fields),
                        flags: StateFlags {
                            urgent: workspace.urgent,
                            focused: workspace.focused,
                            visible: workspace.visible,
                            ..StateFlags::default()
                        },
                        values: fields.clone(),
                        foreground: None,
                        background: None,
                        // Switching is what clicking a workspace is for.
                        action: Some(ActionTarget::Sway(format!(
                            "workspace {}",
                            quote(&workspace.name)
                        ))),
                    });
                }
            }
        }
    }
    out
}

/// Wrap a workspace name for the compositor's command parser.
fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
}

fn size_group(
    group: &GroupCfg,
    inputs: &Inputs<'_>,
    height: f32,
    text: &mut dyn Measure,
) -> Option<SizedGroup> {
    let mut modules = Vec::new();
    for candidate in collect(group, inputs) {
        let Candidate {
            module,
            text: content,
            flags,
            values,
            foreground,
            background,
            action,
        } = candidate;
        // The i3bar protocol uses an empty `full_text` to mean "hide this block".
        if content.is_empty() {
            continue;
        }
        // The state rules and the graded icons both key on the source's own numbers, not
        // on whatever the text happens to say. A rule naming a field reads that one; the
        // rest read whichever value the source is mainly about.
        let bound = |rule: &crate::config::StateRule| match &rule.field {
            Some(name) => values.get(name).and_then(|v| v.num()),
            None => values.primary().and_then(|v| v.num()),
        };
        let resolve = |hovered: bool, text: &str| {
            module
                .states
                .iter()
                .find(|rule| rule.matches(flags, hovered, bound(rule), text))
                .map(|rule| rule.style)
                .unwrap_or(module.style)
        };
        let value = values.primary().and_then(|v| v.num());
        let style = resolve(false, &content);

        // A provider often has to spell a state into the text for a rule to match on. Once
        // it has been matched the wording has done its job, and the icon says it better.
        let content = match module
            .states
            .iter()
            .find(|rule| rule.strip && rule.matches(flags, false, bound(rule), &content))
        {
            Some(rule) => {
                let needle = rule.contains.as_deref().unwrap_or_default();
                content
                    .replace(needle, "")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            }
            None => content,
        };
        // Stripping can empty the text entirely, which is fine when an icon is left to
        // carry the module: a muted volume is the icon and nothing else.
        if content.is_empty() && style.icon.is_none() {
            continue;
        }

        // Hover is deliberately paint-only. Letting it change padding or the icon would
        // resize the module under the pointer, which can move the pointer off it and
        // oscillate, so the metrics always come from the unhovered style.
        let hovered = resolve(true, &content);
        let hover_style = (hovered != style).then_some(Style {
            padding: style.padding,
            min_width: style.min_width,
            icon_size: style.icon_size,
            icon: style.icon,
            ..hovered
        });

        let icon = style.icon.map(|icon| {
            let level = if icon.is_graded() {
                value.map(icon::level_of).unwrap_or(0)
            } else {
                0
            };
            (icon, level)
        });
        let icon_advance = match icon {
            Some(_) if style.icon_size > 0.0 => style.icon_size * (1.0 + ICON_GAP),
            _ => 0.0,
        };
        // A module that would outgrow max_width loses text rather than pushing its
        // neighbours aside: a window title has no length limit of its own.
        let fixed = icon_advance + style.padding * 2.0;
        let content = if style.max_width > 0.0 {
            truncate(&content, style.max_width - fixed, text)
        } else {
            content
        };
        if content.is_empty() && style.icon.is_none() {
            continue;
        }

        let text_width = text.measure(&content);
        let width = (text_width + fixed).max(style.min_width);
        modules.push(SizedModule {
            width,
            text_width,
            hover_style,
            icon_advance,
            icon,
            text: content,
            style,
            foreground: foreground.unwrap_or(style.foreground),
            background: background.unwrap_or(style.background),
            action,
            alt: module.format_alt.is_some().then(|| module.name.clone()),
        });
    }
    if modules.is_empty() {
        return None;
    }

    // A configured separator owns the space between modules; otherwise `spacing` does.
    let advance = if group.separator.shape.is_none() {
        group.spacing
    } else {
        group.separator.width
    };

    let content: f32 = modules.iter().map(|m| m.width).sum();
    let gaps = advance * (modules.len() - 1) as f32;
    let width = content + gaps + group.padding * 2.0;
    let _ = height;

    Some(SizedGroup {
        width,
        background: group.background,
        edges: group.edges,
        padding: group.padding,
        advance,
        separator: group.separator,
        modules,
    })
}

fn place(sized: SizedGroup, mut x: f32, height: f32, pointer: Option<(f32, f32)>) -> PlacedGroup {
    let group_x = x;
    let inner_y = sized.padding;
    let inner_h = (height - sized.padding * 2.0).max(0.0);
    x += sized.padding;

    let separator = sized.separator;
    let draw_separators = !separator.shape.is_none() && sized.advance > 0.0;

    let mut modules: Vec<PlacedModule> = Vec::with_capacity(sized.modules.len());
    let mut separators = Vec::new();

    for (i, m) in sized.modules.into_iter().enumerate() {
        if i > 0 {
            if draw_separators {
                let previous = &modules[i - 1];
                separators.push(PlacedSeparator {
                    x,
                    y: inner_y,
                    width: sized.advance,
                    height: inner_h,
                    shape: separator.shape,
                    direction: separator.direction,
                    overlap: separator.overlap,
                    fill: match separator.color {
                        SeparatorColor::Previous => previous.background,
                        SeparatorColor::Next => m.background,
                        SeparatorColor::Foreground => previous.foreground,
                        SeparatorColor::Background => sized.background,
                        SeparatorColor::Fixed(c) => c,
                    },
                    // Whatever the boundary leads into shows behind the shape.
                    under: m.background,
                });
            }
            x += sized.advance;
        }
        // Icon and text are centred together inside the module box.
        let content_width = m.icon_advance + m.text_width;
        let content_x = x + (m.width - content_width) / 2.0;
        let placed_icon = m.icon.map(|(icon, level)| PlacedIcon {
            icon,
            level,
            x: content_x,
            y: inner_y + (inner_h - m.style.icon_size) / 2.0,
            size: m.style.icon_size,
        });

        // Hover is resolved here, against the final rectangle, so it is always the module
        // actually under the pointer rather than one from a previous frame.
        let over = pointer.is_some_and(|(px, py)| contains(px, py, x, inner_y, m.width, inner_h));
        let paint = match (over, m.hover_style) {
            (true, Some(hover)) => hover,
            _ => m.style,
        };
        let (foreground, background) = if over && m.hover_style.is_some() {
            (paint.foreground, paint.background)
        } else {
            (m.foreground, m.background)
        };

        modules.push(PlacedModule {
            x,
            y: inner_y,
            width: m.width,
            height: inner_h,
            icon: placed_icon,
            text: m.text,
            text_x: content_x + m.icon_advance,
            foreground,
            background,
            radius: paint.radius,
            action: m.action,
            alt: m.alt,
        });
        x += m.width;
    }

    PlacedGroup {
        x: group_x,
        y: 0.0,
        width: sized.width,
        height,
        background: sized.background,
        edges: sized.edges,
        modules,
        separators,
    }
}

/// Lay the whole bar out for a surface of `width` x `height` logical pixels.
pub fn compute(
    cfg: &Config,
    inputs: &Inputs<'_>,
    width: f32,
    height: f32,
    text: &mut dyn Measure,
    pointer: Option<(f32, f32)>,
) -> Frame {
    let gap = cfg.bar.gap;
    let mut frame = Frame::default();

    // Size every position first; placement needs the totals for center and right.
    let sized: Vec<Vec<SizedGroup>> = cfg
        .positions
        .iter()
        .map(|groups| {
            groups
                .iter()
                .filter_map(|g| size_group(g, inputs, height, text))
                .collect()
        })
        .collect();

    let run_width = |groups: &Vec<SizedGroup>| -> f32 {
        if groups.is_empty() {
            return 0.0;
        }
        groups.iter().map(|g| g.width).sum::<f32>() + gap * (groups.len() - 1) as f32
    };

    // The centre run is centred on the bar, but pushed aside rather than allowed to sit on
    // top of its neighbours: a wide right-hand run would otherwise overlap a centred clock
    // long before the bar is actually full.
    let (left_width, centre_width, right_width) = (
        run_width(&sized[0]),
        run_width(&sized[1]),
        run_width(&sized[2]),
    );
    let right_start = (width - right_width).max(0.0);
    let centre_lower = if left_width > 0.0 {
        left_width + gap
    } else {
        0.0
    };
    let centre_upper = (right_start - gap - centre_width).max(centre_lower);
    let centre_start = ((width - centre_width) / 2.0)
        .max(0.0)
        .clamp(centre_lower, centre_upper);

    let starts = [0.0, centre_start, right_start];

    for (groups, mut x) in sized.into_iter().zip(starts) {
        for group in groups {
            let w = group.width;
            frame.groups.push(place(group, x, height, pointer));
            x += w + gap;
        }
    }

    frame
}

/// A frame showing a single message from dbar itself.
///
/// Provider failures bypass the group configuration entirely: a fault reported as an ordinary
/// block would be dropped by any group that selects modules by name, which is exactly when the
/// message matters most.
pub fn fault(message: &str, width: f32, height: f32, text: &mut dyn Measure) -> Frame {
    let padding = 10.0;
    let module_width = text.measure(message) + padding * 2.0;
    let x = (width - module_width).max(0.0);

    Frame {
        groups: vec![PlacedGroup {
            x,
            y: 0.0,
            width: module_width,
            height,
            background: Color::TRANSPARENT,
            edges: Edges {
                left: EdgeShape::None,
                right: EdgeShape::None,
                radius: 0.0,
            },
            modules: vec![PlacedModule {
                x,
                y: 0.0,
                width: module_width,
                height,
                icon: None,
                action: None,
                text: message.to_string(),
                text_x: x + padding,
                foreground: FAULT_COLOR,
                background: Color::TRANSPARENT,
                radius: 0.0,
                alt: None,
            }],
            separators: Vec::new(),
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::{Reading, Which};
    use crate::status::{ActionTarget, Fields, State, StatusItem, Unit, Value};

    /// A text backend with no fonts: every character is one unit wide.
    ///
    /// Layout only ever asks how wide a string is, so a fixed width makes every expected
    /// number in these tests a character count.
    struct Fixed;

    impl Measure for Fixed {
        fn measure(&mut self, text: &str) -> f32 {
            text.chars().count() as f32
        }
    }

    fn item(id: &str, text: &str) -> StatusItem {
        let mut fields = Fields::default();
        fields.set("text", Value::Text(text.to_string()));
        StatusItem {
            id: Some(id.to_string()),
            fields,
            state: State::Idle,
            urgent: false,
            foreground: None,
            background: None,
            action: None,
        }
    }

    fn with_percent(mut item: StatusItem, percent: f64) -> StatusItem {
        item.fields.set(
            "percent",
            Value::Num {
                v: percent,
                unit: Unit::Percent,
            },
        );
        item.fields.set_primary("percent");
        item
    }

    fn frame_of(config: &str, items: &[StatusItem]) -> Frame {
        frame_with(config, items, Registry::new(&Default::default()))
    }

    fn frame_with(config: &str, items: &[StatusItem], native: Registry) -> Frame {
        frame_showing(config, items, native, &Default::default())
    }

    fn frame_showing(
        config: &str,
        items: &[StatusItem],
        native: Registry,
        alt: &std::collections::HashSet<String>,
    ) -> Frame {
        let cfg = Config::parse(config).expect("test config parses");
        let inputs = Inputs {
            items,
            native: &native,
            sway: &SwayState::default(),
            alt,
        };
        compute(&cfg, &inputs, 200.0, 10.0, &mut Fixed, None)
    }

    const BASIC: &str = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu", "mem"]

[module.cpu]
padding = 0

[module.mem]
padding = 0
"##;

    #[test]
    fn a_native_module_draws_what_its_collector_measured() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
source = "cpu"
padding = 0
format = "$utilization.n(d:0)"
"##;
        let mut fields = Fields::default();
        fields.set(
            "utilization",
            Value::Num {
                v: 42.0,
                unit: crate::status::Unit::Percent,
            },
        );
        fields.set_primary("utilization");
        let native = Registry::fixture(
            Which::Cpu,
            Reading {
                fields,
                state: crate::status::State::Idle,
            },
        );
        let frame = frame_with(config, &[], native);
        assert_eq!(frame.groups[0].modules[0].text, "42%");
    }

    #[test]
    fn a_native_module_that_has_not_read_yet_draws_nothing() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
source = "cpu"
"##;
        let native = Registry::fixture(Which::Cpu, Reading::default());
        // No fields yet, so the format renders empty and the module is not there.
        assert!(frame_with(config, &[], native).groups.is_empty());
    }

    #[test]
    fn modules_select_items_by_name() {
        let frame = frame_of(BASIC, &[item("mem", "50%"), item("cpu", "10%")]);
        let texts: Vec<&str> = frame.groups[0]
            .modules
            .iter()
            .map(|m| m.text.as_str())
            .collect();
        // Group order wins over the order the source sent them in.
        assert_eq!(texts, ["10%", "50%"]);
    }

    #[test]
    fn an_unmatched_item_draws_nothing() {
        let frame = frame_of(BASIC, &[item("disk", "1G")]);
        assert!(frame.groups.is_empty());
    }

    #[test]
    fn an_empty_item_is_hidden() {
        // The i3bar protocol uses empty text to mean "hide this block", and a native
        // source with nothing to say lands in the same place.
        let frame = frame_of(BASIC, &[item("cpu", ""), item("mem", "50%")]);
        assert_eq!(frame.groups[0].modules.len(), 1);
        assert_eq!(frame.groups[0].modules[0].text, "50%");
    }

    #[test]
    fn a_wildcard_group_takes_every_item_in_order() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["*"]
"##;
        let frame = frame_of(config, &[item("mem", "50%"), item("cpu", "10%")]);
        let texts: Vec<&str> = frame.groups[0]
            .modules
            .iter()
            .map(|m| m.text.as_str())
            .collect();
        assert_eq!(texts, ["50%", "10%"]);
    }

    #[test]
    fn source_colours_win_over_the_style() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
foreground = "#00ff00"
"##;
        let mut styled = item("cpu", "10%");
        styled.foreground = Some(Color::rgba(0xff, 0, 0, 0xff));
        let frame = frame_of(config, &[styled]);
        assert_eq!(
            frame.groups[0].modules[0].foreground,
            Color::rgba(0xff, 0, 0, 0xff)
        );
    }

    #[test]
    fn a_module_says_what_its_format_says() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
padding = 0
format = "cpu $percent.n(w:3)"
"##;
        let frame = frame_of(config, &[with_percent(item("cpu", "ignored"), 7.0)]);
        // The width counts the whole number, suffix included, so a column of them lines up.
        assert_eq!(frame.groups[0].modules[0].text, "cpu  7%");
    }

    #[test]
    fn a_format_can_drop_what_the_source_could_not_measure() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
padding = 0
format = "cpu{ $percent}"
"##;
        // No percentage in the text, so the source published none and the group goes.
        let frame = frame_of(config, &[item("cpu", "busy")]);
        assert_eq!(frame.groups[0].modules[0].text, "cpu");
    }

    #[test]
    fn a_click_target_marks_a_module_that_has_a_second_wording() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu", "mem"]

[module.cpu]
padding = 0
format_alt = "$percent"

[module.mem]
padding = 0
"##;
        let frame = frame_of(config, &[item("cpu", "a"), item("mem", "b")]);
        assert_eq!(frame.groups[0].modules[0].alt.as_deref(), Some("cpu"));
        assert_eq!(frame.groups[0].modules[1].alt, None);
    }

    #[test]
    fn the_second_wording_is_what_is_drawn_once_it_is_showing() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
padding = 0
format = "short"
format_alt = "the long way round"
"##;
        let showing = std::collections::HashSet::from(["cpu".to_string()]);
        let plain = frame_of(config, &[item("cpu", "x")]);
        assert_eq!(plain.groups[0].modules[0].text, "short");

        let swapped = frame_showing(
            config,
            &[item("cpu", "x")],
            Registry::new(&Default::default()),
            &showing,
        );
        assert_eq!(swapped.groups[0].modules[0].text, "the long way round");
    }

    #[test]
    fn a_rule_can_key_on_how_the_source_rates_itself() {
        let config = r##"
[colors]
bad = "#ff0000"

[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
source = "cpu"
format = "x"

[module.cpu.states.broken]
state = "error"
foreground = "$bad"
"##;
        let mut fields = Fields::default();
        fields.set(
            "utilization",
            Value::Num {
                v: 1.0,
                unit: crate::status::Unit::Percent,
            },
        );
        fields.set_primary("utilization");

        let working = Registry::fixture(
            Which::Cpu,
            Reading {
                fields: fields.clone(),
                state: State::Idle,
            },
        );
        assert_ne!(
            frame_with(config, &[], working).groups[0].modules[0].foreground,
            Color::rgba(0xff, 0, 0, 0xff)
        );

        let broken = Registry::fixture(
            Which::Cpu,
            Reading {
                fields,
                state: State::Error,
            },
        );
        assert_eq!(
            frame_with(config, &[], broken).groups[0].modules[0].foreground,
            Color::rgba(0xff, 0, 0, 0xff)
        );
    }

    #[test]
    fn a_threshold_can_read_a_field_other_than_the_main_one() {
        let config = r##"
[colors]
warn = "#ffff00"

[left]
groups = ["g"]

[group.g]
modules = ["mem"]

[module.mem]
source = "memory"
format = "x"

[module.mem.states.swapping]
field = "swap_percent"
above = 20
foreground = "$warn"
"##;
        let unit = crate::status::Unit::Percent;
        let mut fields = Fields::default();
        // The main value is calm; the one the rule names is not.
        fields.set("percent", Value::Num { v: 5.0, unit });
        fields.set("swap_percent", Value::Num { v: 90.0, unit });
        fields.set_primary("percent");

        let native = Registry::fixture(
            Which::Memory,
            Reading {
                fields,
                state: State::Idle,
            },
        );
        assert_eq!(
            frame_with(config, &[], native).groups[0].modules[0].foreground,
            Color::rgba(0xff, 0xff, 0, 0xff)
        );
    }

    #[test]
    fn thresholds_key_on_the_published_value_not_the_text() {
        let config = r##"
[colors]
warn = "#ffff00"

[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu.states.high]
above = 80
foreground = "$warn"
"##;
        // The text says nothing a threshold could be scraped from; the field carries it.
        let hot = with_percent(item("cpu", "busy"), 90.0);
        let frame = frame_of(config, &[hot]);
        assert_eq!(
            frame.groups[0].modules[0].foreground,
            Color::rgba(0xff, 0xff, 0, 0xff)
        );

        let cool = with_percent(item("cpu", "busy"), 10.0);
        let frame = frame_of(config, &[cool]);
        assert_ne!(
            frame.groups[0].modules[0].foreground,
            Color::rgba(0xff, 0xff, 0, 0xff)
        );
    }

    #[test]
    fn a_graded_icon_takes_its_level_from_the_published_value() {
        let config = r##"
[bar]
icon_size = 10

[left]
groups = ["g"]

[group.g]
modules = ["bat"]

[module.bat]
icon = "battery"
"##;
        for (percent, level) in [(0.0, 0), (50.0, 2), (100.0, 4)] {
            let frame = frame_of(config, &[with_percent(item("bat", "x"), percent)]);
            assert_eq!(
                frame.groups[0].modules[0].icon.unwrap().level,
                level,
                "at {percent}%"
            );
        }
    }

    #[test]
    fn clicks_route_to_whatever_the_source_asked_for() {
        let mut clickable = item("cpu", "10%");
        clickable.action = Some(ActionTarget::I3Bar {
            name: Some("0".to_string()),
            instance: None,
        });
        let frame = frame_of(BASIC, &[clickable]);
        let action = frame.groups[0].modules[0].action.as_ref();
        assert!(matches!(
            action,
            Some(ActionTarget::I3Bar { name: Some(n), .. }) if n == "0"
        ));
    }

    #[test]
    fn a_module_wider_than_max_width_loses_text_not_its_neighbours() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu", "mem"]

[module.cpu]
padding = 0
max_width = 5

[module.mem]
padding = 0
"##;
        let frame = frame_of(config, &[item("cpu", "0123456789"), item("mem", "ab")]);
        let modules = &frame.groups[0].modules;
        assert_eq!(modules[0].width, 5.0);
        assert_eq!(modules[0].text, "0123\u{2026}");
        assert_eq!(modules[1].x, 5.0);
    }

    #[test]
    fn hit_testing_finds_the_module_under_a_point() {
        let frame = frame_of(BASIC, &[item("cpu", "abc"), item("mem", "de")]);
        assert_eq!(frame.module_at(1.0, 5.0).unwrap().text, "abc");
        assert_eq!(frame.module_at(4.0, 5.0).unwrap().text, "de");
        assert!(frame.module_at(50.0, 5.0).is_none());
    }
}
