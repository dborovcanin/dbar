//! Turns config plus the current status blocks into positioned rectangles.
//!
//! The result is purely geometric: the renderer draws it and the pointer code hit-tests it,
//! neither needs to know about config or the i3bar protocol.

use crate::color::Color;
use crate::config::{Config, Group as GroupCfg, Style};
use crate::status::Block;
use crate::text::TextRenderer;

#[derive(Clone, Debug)]
pub struct PlacedModule {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub text: String,
    pub foreground: Color,
    pub background: Color,
    pub radius: f32,
    /// Index into the block list this module was built from, for click routing.
    pub block: usize,
}

#[derive(Clone, Debug)]
pub struct PlacedGroup {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub background: Color,
    pub radius: f32,
    pub modules: Vec<PlacedModule>,
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
    radius: f32,
    padding: f32,
    spacing: f32,
    /// Module widths paired with their content.
    modules: Vec<SizedModule>,
}

struct SizedModule {
    width: f32,
    text: String,
    style: Style,
    foreground: Color,
    background: Color,
    block: usize,
}

/// Pick the blocks a group shows, in the order the group asks for.
fn select_blocks<'a>(group: &GroupCfg, blocks: &'a [Block]) -> Vec<(usize, &'a Block, Style)> {
    let mut out = Vec::new();
    if group.wildcard {
        let style = group.modules.first().map(|m| m.style).unwrap_or_default();
        out.extend(blocks.iter().enumerate().map(|(i, b)| (i, b, style)));
        return out;
    }
    for module in &group.modules {
        for (i, block) in blocks.iter().enumerate() {
            if block.name.as_deref() == Some(module.name.as_str()) {
                out.push((i, block, module.style));
            }
        }
    }
    out
}

fn size_group(
    group: &GroupCfg,
    blocks: &[Block],
    height: f32,
    text: &mut TextRenderer,
) -> Option<SizedGroup> {
    let mut modules = Vec::new();
    for (index, block, style) in select_blocks(group, blocks) {
        // The i3bar protocol uses an empty `full_text` to mean "hide this block".
        if block.full_text.is_empty() {
            continue;
        }
        let content = block.display_text().into_owned();
        if content.is_empty() {
            continue;
        }
        let width = (text.measure(&content) + style.padding * 2.0).max(style.min_width);
        modules.push(SizedModule {
            width,
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

    let content: f32 = modules.iter().map(|m| m.width).sum();
    let gaps = group.spacing * (modules.len() - 1) as f32;
    let width = content + gaps + group.padding * 2.0;
    let _ = height;

    Some(SizedGroup {
        width,
        background: group.background,
        radius: group.radius,
        padding: group.padding,
        spacing: group.spacing,
        modules,
    })
}

fn place(sized: SizedGroup, mut x: f32, height: f32) -> PlacedGroup {
    let group_x = x;
    let inner_y = sized.padding;
    let inner_h = (height - sized.padding * 2.0).max(0.0);
    x += sized.padding;

    let mut modules = Vec::with_capacity(sized.modules.len());
    for (i, m) in sized.modules.into_iter().enumerate() {
        if i > 0 {
            x += sized.spacing;
        }
        modules.push(PlacedModule {
            x,
            y: inner_y,
            width: m.width,
            height: inner_h,
            text: m.text,
            foreground: m.foreground,
            background: m.background,
            radius: m.style.radius,
            block: m.block,
        });
        x += m.width;
    }

    PlacedGroup {
        x: group_x,
        y: 0.0,
        width: sized.width,
        height,
        background: sized.background,
        radius: sized.radius,
        modules,
    }
}

/// Lay the whole bar out for a surface of `width` x `height` logical pixels.
pub fn compute(
    cfg: &Config,
    blocks: &[Block],
    width: f32,
    height: f32,
    text: &mut TextRenderer,
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
