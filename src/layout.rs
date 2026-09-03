//! Turns config plus the current status blocks into positioned rectangles.
//!
//! The result is purely geometric: the renderer draws it and the pointer code hit-tests it,
//! neither needs to know about config or the i3bar protocol.

use crate::color::Color;
use crate::config::{
    Config, Direction, EdgeShape, Edges, Group as GroupCfg, Module as ModuleCfg, Separator,
    SeparatorColor, SeparatorShape, Source, StateFlags, Style,
};
use crate::icon::{self, Icon};
use crate::status::Block;
use crate::sway::SwayState;

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
    /// Index into the block list this module was built from, for click routing.
    /// `None` for synthetic modules that no block produced.
    pub block: Option<usize>,
    /// A compositor command to run instead of forwarding the click to the provider.
    pub command: Option<String>,
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

/// Space between an icon and the text beside it, as a fraction of the icon size.
const ICON_GAP: f32 = 0.4;

/// A colour the provider set on a block, if it set one.
fn block_color(
    blocks: &[Block],
    index: Option<usize>,
    pick: impl Fn(&Block) -> Option<&str>,
) -> Option<Color> {
    let block = blocks.get(index?)?;
    Color::parse(pick(block)?).ok()
}

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
    block: Option<usize>,
    command: Option<String>,
}

/// Colour used for messages dbar generates itself, matching the i3bar convention.
const FAULT_COLOR: Color = Color::rgba(0xf3, 0x8b, 0xa8, 0xff);

/// One thing a group will draw, before it has been measured.
struct Candidate<'g> {
    module: &'g ModuleCfg,
    text: String,
    flags: StateFlags,
    /// Index of the block behind it, for routing clicks back to the provider.
    block: Option<usize>,
    /// A compositor command to run on click instead.
    command: Option<String>,
}

/// Everything a group shows, in the order the group asks for.
///
/// A module drawn from the compositor expands here: `sway:workspaces` becomes one candidate
/// per workspace, so each is its own rectangle with its own state and click target.
fn collect<'g>(group: &'g GroupCfg, blocks: &[Block], sway: &SwayState) -> Vec<Candidate<'g>> {
    let mut out = Vec::new();

    if group.wildcard {
        if let Some(module) = group.modules.first() {
            out.extend(blocks.iter().enumerate().map(|(i, b)| Candidate {
                module,
                text: b.display_text().into_owned(),
                flags: StateFlags {
                    urgent: b.urgent,
                    ..StateFlags::default()
                },
                block: Some(i),
                command: None,
            }));
        }
        return out;
    }

    for module in &group.modules {
        match module.source {
            Source::Provider => {
                for (i, block) in blocks.iter().enumerate() {
                    if block.selector() == Some(module.name.as_str()) {
                        out.push(Candidate {
                            module,
                            text: block.display_text().into_owned(),
                            flags: StateFlags {
                                urgent: block.urgent,
                                ..StateFlags::default()
                            },
                            block: Some(i),
                            command: None,
                        });
                    }
                }
            }
            Source::SwayWindow => {
                if let Some(title) = &sway.window {
                    out.push(Candidate {
                        module,
                        text: title.clone(),
                        flags: StateFlags::default(),
                        block: None,
                        command: None,
                    });
                }
            }
            Source::SwayWorkspaces => {
                for workspace in &sway.workspaces {
                    out.push(Candidate {
                        module,
                        text: workspace.name.clone(),
                        flags: StateFlags {
                            urgent: workspace.urgent,
                            focused: workspace.focused,
                            visible: workspace.visible,
                        },
                        block: None,
                        // Switching is what clicking a workspace is for.
                        command: Some(format!("workspace {}", quote(&workspace.name))),
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
    blocks: &[Block],
    sway: &SwayState,
    height: f32,
    text: &mut dyn Measure,
) -> Option<SizedGroup> {
    let mut modules = Vec::new();
    for candidate in collect(group, blocks, sway) {
        let Candidate {
            module,
            text: content,
            flags,
            block: index,
            command,
        } = candidate;
        // The i3bar protocol uses an empty `full_text` to mean "hide this block".
        if content.is_empty() {
            continue;
        }
        // One reading of the text serves both the state rules and the graded icons.
        let value = crate::status::percent(&content);
        let resolve = |hovered: bool| {
            module
                .states
                .iter()
                .find(|rule| rule.matches(flags, hovered, value, &content))
                .map(|rule| rule.style)
                .unwrap_or(module.style)
        };
        let style = resolve(false);

        // Hover is deliberately paint-only. Letting it change padding or the icon would
        // resize the module under the pointer, which can move the pointer off it and
        // oscillate, so the metrics always come from the unhovered style.
        let hovered = resolve(true);
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
        let text_width = text.measure(&content);
        let width = (text_width + icon_advance + style.padding * 2.0).max(style.min_width);
        modules.push(SizedModule {
            width,
            text_width,
            hover_style,
            icon_advance,
            icon,
            text: content,
            style,
            foreground: block_color(blocks, index, |b| b.color.as_deref())
                .unwrap_or(style.foreground),
            background: block_color(blocks, index, |b| b.background.as_deref())
                .unwrap_or(style.background),
            block: index,
            command,
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
            block: m.block,
            command: m.command,
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
    blocks: &[Block],
    sway: &SwayState,
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
                .filter_map(|g| size_group(g, blocks, sway, height, text))
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
                command: None,
                text: message.to_string(),
                text_x: x + padding,
                foreground: FAULT_COLOR,
                background: Color::TRANSPARENT,
                radius: 0.0,
                block: None,
            }],
            separators: Vec::new(),
        }],
    }
}
