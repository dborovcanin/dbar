//! Turns config plus the current status blocks into positioned rectangles.
//!
//! The result is purely geometric: the renderer draws it and the pointer code hit-tests it,
//! neither needs to know about config or the i3bar protocol.

use crate::color::Color;
use crate::config::{
    Config, Direction, EdgeShape, Edges, Group as GroupCfg, Module as ModuleCfg, Separator,
    SeparatorColor, SeparatorShape, Style,
};
use crate::icon::{self, Icon};
use crate::status::Block;

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

struct SizedModule {
    width: f32,
    text_width: f32,
    /// Width of the icon plus its gap, or zero.
    icon_advance: f32,
    icon: Option<(Icon, usize)>,
    text: String,
    style: Style,
    foreground: Color,
    background: Color,
    block: usize,
}

/// Colour used for messages dbar generates itself, matching the i3bar convention.
const FAULT_COLOR: Color = Color::rgba(0xf3, 0x8b, 0xa8, 0xff);

/// Pick the blocks a group shows, in the order the group asks for.
fn select_blocks<'a, 'g>(
    group: &'g GroupCfg,
    blocks: &'a [Block],
) -> Vec<(usize, &'a Block, &'g ModuleCfg)> {
    let mut out = Vec::new();
    if group.wildcard {
        if let Some(module) = group.modules.first() {
            out.extend(blocks.iter().enumerate().map(|(i, b)| (i, b, module)));
        }
        return out;
    }
    for module in &group.modules {
        for (i, block) in blocks.iter().enumerate() {
            if block.selector() == Some(module.name.as_str()) {
                out.push((i, block, module));
            }
        }
    }
    out
}

fn size_group(
    group: &GroupCfg,
    blocks: &[Block],
    height: f32,
    text: &mut dyn Measure,
) -> Option<SizedGroup> {
    let mut modules = Vec::new();
    for (index, block, module) in select_blocks(group, blocks) {
        // The i3bar protocol uses an empty `full_text` to mean "hide this block".
        if block.full_text.is_empty() {
            continue;
        }
        let content = block.display_text().into_owned();
        if content.is_empty() {
            continue;
        }
        // One reading of the text serves both the state rules and the graded icons.
        let value = crate::status::percent(&content);
        let style = module
            .states
            .iter()
            .find(|rule| rule.matches(block.urgent, value))
            .map(|rule| rule.style)
            .unwrap_or(module.style);

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
            icon_advance,
            icon,
            text: content,
            style,
            foreground: block
                .color
                .as_deref()
                .and_then(|c| Color::parse(c).ok())
                .unwrap_or(style.foreground),
            background: block
                .background
                .as_deref()
                .and_then(|c| Color::parse(c).ok())
                .unwrap_or(style.background),
            block: index,
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

fn place(sized: SizedGroup, mut x: f32, height: f32) -> PlacedGroup {
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

        modules.push(PlacedModule {
            x,
            y: inner_y,
            width: m.width,
            height: inner_h,
            icon: placed_icon,
            text: m.text,
            text_x: content_x + m.icon_advance,
            foreground: m.foreground,
            background: m.background,
            radius: m.style.radius,
            block: Some(m.block),
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
    width: f32,
    height: f32,
    text: &mut dyn Measure,
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
                .filter_map(|g| size_group(g, blocks, height, text))
                .collect()
        })
        .collect();

    let run_width = |groups: &Vec<SizedGroup>| -> f32 {
        if groups.is_empty() {
            return 0.0;
        }
        groups.iter().map(|g| g.width).sum::<f32>() + gap * (groups.len() - 1) as f32
    };

    let starts = [
        0.0,
        ((width - run_width(&sized[1])) / 2.0).max(0.0),
        (width - run_width(&sized[2])).max(0.0),
    ];

    for (groups, mut x) in sized.into_iter().zip(starts) {
        for group in groups {
            let w = group.width;
            frame.groups.push(place(group, x, height));
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
