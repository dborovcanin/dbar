//! Turns config plus the current status items into positioned rectangles.
//!
//! The result is purely geometric: the renderer draws it and the pointer code hit-tests it,
//! neither needs to know about config or where the items came from.

use crate::collect::{Registry, Which};
use crate::color::Color;
use crate::config::{
    Config, Direction, EdgeShape, Edges, Ends, Group as GroupCfg, Module as ModuleCfg, Separator,
    SeparatorColor, SeparatorShape, Source, StateFlags, Style,
};
use crate::format::Format;
use crate::icon::{self, Icon};
use std::sync::Arc;

use crate::config::{Button, ClickActions};
use crate::status::{ActionTarget, Fields, StatusItem, Unit, Value};
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
    pub alt: &'a std::collections::HashMap<String, usize>,
    /// Which page each module is scrolled to, by name, for a source that published
    /// several readings at once.
    pub pages: &'a std::collections::HashMap<String, usize>,
    /// Modules folded down to their icon.
    pub collapsed: &'a std::collections::HashSet<String>,
    /// Command sources with a run on its way that has been out long enough to say so.
    ///
    /// Which run it is does not matter here, only that one is happening: a module waiting
    /// on its program shows a spinner where its icon goes.
    pub waiting: &'a std::collections::HashSet<Which>,
    /// Which step of its turn a spinner is on. One step for the whole bar, so two waiting
    /// modules turn together rather than beating against each other.
    pub spin: usize,
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
    /// The module's name, when a gesture on it has to name it.
    ///
    /// Further wordings, folding, paging and refreshing are all remembered against the
    /// module rather than against the frame, which is built again from nothing on every
    /// redraw. One name serves all of them, and a module that answers to none carries
    /// none.
    pub name: Option<String>,
    /// How many wordings there are to move through, when there is more than one.
    pub alt: Option<usize>,
    /// Which button moves through those wordings.
    pub alt_button: Button,
    /// Which button reads the module's source again, when the config gives one that job.
    pub refresh: Option<Button>,
    /// Which button mutes, for a module showing the volume. None on everything else.
    pub mute: Option<Button>,
    /// How many readings there are to scroll between, when the source published several.
    pub paged: Option<usize>,
    /// Whether a click folds this module down to its icon.
    pub collapsible: bool,
    /// Which button does that folding.
    pub collapse_button: Button,
    /// The programs this module's buttons run, shared with the config rather than copied
    /// into every frame.
    pub on_click: Option<Arc<ClickActions>>,
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
    /// How much of the finished island reaches the screen, 0.0 to 1.0.
    pub opacity: f32,
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
    opacity: f32,
    edges: Edges,
    padding: f32,
    /// Horizontal space between neighbouring modules.
    advance: f32,
    separator: Separator,
    ends: Ends,
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
/// Which of a module's wordings is showing.
///
/// Zero is what it says by default, and a click moves on to the next: a module with two
/// further wordings goes round three views rather than toggling between two.
fn wording<'g>(
    module: &'g ModuleCfg,
    alt: &std::collections::HashMap<String, usize>,
) -> &'g Format {
    match alt.get(&module.name) {
        Some(&showing) if showing > 0 => {
            module.format_alt.get(showing - 1).unwrap_or(&module.format)
        }
        _ => &module.format,
    }
}

struct SizedModule {
    width: f32,
    text_width: f32,
    /// The module's name, when a gesture on it has to name it.
    name: Option<String>,
    collapsible: bool,
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
    alt: Option<usize>,
    alt_button: Button,
    collapse_button: Button,
    refresh: Option<Button>,
    mute: Option<Button>,
    paged: Option<usize>,
    on_click: Option<Arc<ClickActions>>,
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
    /// How many readings the source published, when the module scrolls between them.
    pages: usize,
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
        pages: 1,
    };

    if group.wildcard {
        if let Some(module) = group.modules.first() {
            out.extend(inputs.items.iter().map(|item| from_item(module, item)));
        }
        return out;
    }

    for module in &group.modules {
        match &module.source {
            Source::Native(which) => {
                // Which of the readings this module is scrolled to. A source that
                // published one has one, and the page is always that one.
                let page = inputs.pages.get(&module.name).copied().unwrap_or(0);
                // A collector that has not read yet has nothing to show, which is the same
                // as a provider that has not spoken: the module simply is not there. A
                // command with its first run still out is the exception, because it has a
                // reason to be on the bar early: the spinner stands in until the reading
                // lands, in the place the reading will land in.
                let Some((reading, pages)) = inputs.native.showing(which, page) else {
                    if inputs.waiting.contains(which) {
                        out.push(Candidate {
                            module,
                            text: String::new(),
                            flags: StateFlags::default(),
                            values: Fields::default(),
                            foreground: None,
                            background: None,
                            action: None,
                            pages: 1,
                        });
                    }
                    continue;
                };
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
                    pages,
                    // A module the config lets be operated carries what its buttons
                    // do; one that does not is drawn exactly as before.
                    action: module
                        .control
                        .map(|(what, step)| ActionTarget::Control { what, step }),
                });
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
                        pages: 1,
                        action: None,
                    });
                }
            }
            Source::SwayLanguage(layouts) => {
                if let Some(layout) = &inputs.sway.layout {
                    let mut fields = Fields::default();
                    // What the module calls this layout if it says, and an abbreviation of
                    // xkb's own name if it does not.
                    let short = layouts
                        .get(&layout.name)
                        .cloned()
                        .unwrap_or_else(|| crate::sway::abbreviate(&layout.name));
                    fields.set("layout", Value::Text(layout.name.clone()));
                    fields.set("short", Value::Text(short));
                    fields.set(
                        "index",
                        Value::Num {
                            v: layout.index as f64,
                            unit: Unit::None,
                        },
                    );
                    // Which layout it is, rather than what it is called: a rule keyed on
                    // the index survives xkb renaming anything.
                    fields.set_primary("index");
                    out.push(Candidate {
                        module,
                        text: wording(module, inputs.alt).render(&fields),
                        flags: StateFlags::default(),
                        values: fields,
                        foreground: None,
                        background: None,
                        pages: 1,
                        action: None,
                    });
                }
            }
            Source::SwayMode => {
                // Only while the compositor is in a mode worth mentioning: the default one
                // is what a keyboard does anyway, so the module disappears rather than
                // saying so, the way i3 and sway's own bars do.
                if let Some(mode) = &inputs.sway.mode
                    && mode != crate::sway::DEFAULT_MODE
                {
                    let mut fields = Fields::default();
                    fields.set("mode", Value::Text(mode.clone()));
                    out.push(Candidate {
                        module,
                        text: wording(module, inputs.alt).render(&fields),
                        flags: StateFlags::default(),
                        values: fields,
                        foreground: None,
                        background: None,
                        pages: 1,
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
                        pages: 1,
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
    budget: f32,
) -> Option<SizedGroup> {
    // What is left for the modules once the group's own padding is paid for. A run with
    // nothing else beside it gets infinity, and nothing below has to think about it.
    let mut left = budget - group.padding * 2.0;
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
            pages,
        } = candidate;
        // Whether this module's program is out, which the spinner is drawn for. The check
        // is skipped outright while nothing is waiting, which is nearly always.
        let waiting = !inputs.waiting.is_empty()
            && matches!(&module.source, Source::Native(which) if inputs.waiting.contains(which));
        // The i3bar protocol uses an empty `full_text` to mean "hide this block". A module
        // folded down is empty on purpose and stays, because its icon is still there, and
        // so does one whose spinner is the only thing it has to show.
        let folded = module.collapsible && inputs.collapsed.contains(&module.name);
        if content.is_empty() && !folded && !waiting {
            continue;
        }
        // The state rules and the graded icons both key on what the source published, not
        // on whatever the text ended up saying; a rule reads the field it names, or the
        // value the source is mainly about.
        let resolve = |hovered: bool, text: &str| {
            module
                .states
                .iter()
                .find(|rule| rule.matches(flags, hovered, &values, text))
                .map(|rule| rule.style)
                .unwrap_or(module.style)
        };
        let value = values.primary().and_then(|v| v.num());
        let style = resolve(false, &content);

        // A provider often has to spell a state into the text for a rule to match on. Once
        // it has been matched the wording has done its job, and the icon says it better.
        let mut content = match module
            .states
            .iter()
            .find(|rule| rule.strip && rule.matches(flags, false, &values, &content))
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
        // carry the module: a muted volume is the icon and nothing else, and so is a
        // command that has not answered yet.
        if content.is_empty() && style.icon.is_none() && !waiting {
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

        // The rules and the icon read what the module would have said, so folding changes
        // what is drawn without changing what the module is: a paused player keeps its
        // paused styling while it is a single icon.
        if folded {
            content.clear();
        }

        // A command with a run on its way says so where its icon goes, so the reading
        // that is coming lands in the place the spinner was and nothing else moves. A
        // module with no icon of its own grows one for as long as it is waiting.
        let icon = match waiting {
            true => Some((Icon::Spinner, inputs.spin)),
            false => style.icon.map(|icon| {
                let level = if icon.is_graded() {
                    value.map(icon::level_of).unwrap_or(0)
                } else {
                    0
                };
                (icon, level)
            }),
        };
        // The icon and the space after it, which is what the text starts behind. An icon
        // is as tall as `icon_size` and as wide as its own shape asks for, which is the
        // same thing for everything but the battery.
        //
        // The gap belongs to the text rather than to the icon, so a module with nothing
        // written on it does not get one. Keeping it would pad the far side of a module
        // folded down to its icon and leave the icon sitting half a gap off centre, which
        // is exactly the case the gap was never for.
        let advance = |content: &str| match icon {
            Some((icon, _)) if style.icon_size > 0.0 => {
                let gap = match content.is_empty() {
                    true => 0.0,
                    false => style.gap(),
                };
                style.icon_size * icon.width() + gap
            }
            _ => 0.0,
        };
        // A module that would outgrow max_width, or the room its run has left, loses text
        // rather than pushing its neighbours aside: a window title has no length limit of
        // its own, and a bar can run out of width whatever the config says.
        let fixed = advance(&content) + style.padding * 2.0;
        let cap = match style.max_width > 0.0 {
            true => style.max_width.min(left),
            false => left,
        };
        let content = if cap.is_finite() {
            truncate(&content, cap - fixed, text)
        } else {
            content
        };
        // Truncating to nothing takes the gap with it, the same as folding does. Measured
        // again rather than kept, because only the text that survived says whether there
        // is anything for a gap to separate.
        let icon_advance = advance(&content);
        let fixed = icon_advance + style.padding * 2.0;
        // Truncation can take the last of it, which an icon still carries - a spinner
        // included, since a module waiting on its first answer has nothing else.
        if content.is_empty() && style.icon.is_none() && !waiting {
            continue;
        }

        let text_width = text.measure(&content);
        let width = (text_width + fixed).max(style.min_width);
        // A module with nothing left to draw in is left out entirely, rather than drawn
        // over whatever the run was making room for.
        if width > left {
            continue;
        }
        left -= width + group.spacing;
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
            // How many views this module has in all, so a click knows where it wraps.
            alt: (!module.format_alt.is_empty()).then(|| module.format_alt.len() + 1),
            alt_button: module.alt_button,
            collapsible: module.collapsible,
            collapse_button: module.collapse_button,
            refresh: module.refresh_button,
            mute: module.mute_button,
            // Only where there is somewhere to scroll to: a command reporting on one
            // thing leaves the wheel alone.
            paged: (pages > 1).then_some(pages),
            // Named only where something on the module answers to a gesture, so an
            // ordinary module costs no allocation on the path that runs every frame.
            name: (!module.format_alt.is_empty()
                || module.collapsible
                || module.refresh_button.is_some()
                || pages > 1)
                .then(|| module.name.clone()),
            on_click: module.on_click.clone(),
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
    // A shaped end needs room of its own: it is drawn beside the modules, not over them.
    let ends = group.ends.left_width() + group.ends.right_width();
    let width = content + gaps + ends + group.padding * 2.0;
    let _ = height;

    Some(SizedGroup {
        width,
        background: group.background,
        opacity: group.opacity,
        edges: group.edges,
        padding: group.padding,
        advance,
        separator: group.separator,
        ends: group.ends,
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

    // The left end is drawn before the first module, in space reserved for it.
    let ends = sized.ends;
    let lead = ends.left_width();
    if lead > 0.0
        && let Some(first) = sized.modules.first()
    {
        separators.push(end_separator(
            ends.left,
            x,
            inner_y,
            lead,
            inner_h,
            &ends,
            separator.direction,
            first.background,
        ));
    }
    x += lead;

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
                    // The ground the shape is drawn over: the neighbour whose colour the
                    // shape did not take. Taking the same one twice would paint the gap in
                    // a single colour and leave the boundary invisible.
                    under: match separator.color {
                        SeparatorColor::Next => previous.background,
                        _ => m.background,
                    },
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
            name: m.name,
            alt: m.alt,
            alt_button: m.alt_button,
            collapsible: m.collapsible,
            collapse_button: m.collapse_button,
            refresh: m.refresh,
            mute: m.mute,
            paged: m.paged,
            on_click: m.on_click,
        });
        x += m.width;
    }

    let trail = ends.right_width();
    if trail > 0.0
        && let Some(last) = modules.last()
    {
        separators.push(end_separator(
            ends.right,
            x,
            inner_y,
            trail,
            inner_h,
            &ends,
            separator.direction,
            last.background,
        ));
    }

    PlacedGroup {
        x: group_x,
        y: 0.0,
        width: sized.width,
        height,
        background: sized.background,
        opacity: sized.opacity,
        edges: sized.edges,
        modules,
        separators,
    }
}

/// The transition between a module at the edge of a group and the bar behind it.
///
/// The shape is filled with the module's colour and the rest of the space is left alone,
/// so the ribbon appears to come to a point over whatever is behind the bar. Which of the
/// two colours the drawing code treats as the shape depends on the direction, because a
/// mirrored separator swaps them.
#[allow(clippy::too_many_arguments)]
fn end_separator(
    shape: SeparatorShape,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    ends: &Ends,
    direction: Direction,
    module: Color,
) -> PlacedSeparator {
    let (fill, under) = match direction {
        Direction::Left => (Color::TRANSPARENT, module),
        Direction::Right => (module, Color::TRANSPARENT),
    };
    PlacedSeparator {
        x,
        y,
        width,
        height,
        shape,
        direction,
        overlap: ends.overlap,
        fill,
        under,
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

    let run_width = |groups: &Vec<SizedGroup>| -> f32 {
        if groups.is_empty() {
            return 0.0;
        }
        groups.iter().map(|g| g.width).sum::<f32>() + gap * (groups.len() - 1) as f32
    };

    // Sized in the order they get to keep their width: the right run says what it needs,
    // the left run takes what is left, and the centre lives in the gap between them. A run
    // that runs out of room truncates the module it is in the middle of and drops the rest,
    // rather than drawing over its neighbour.
    let size_run = |groups: &[GroupCfg], budget: f32, text: &mut dyn Measure| -> Vec<SizedGroup> {
        let mut left = budget;
        let mut out = Vec::new();
        for group in groups {
            let Some(sized) = size_group(group, inputs, height, text, left) else {
                continue;
            };
            left -= sized.width + gap;
            out.push(sized);
        }
        out
    };

    let right = size_run(&cfg.positions[2], width, text);
    let right_width = run_width(&right);
    let left = size_run(
        &cfg.positions[0],
        (width - right_width - gap).max(0.0),
        text,
    );
    let left_width = run_width(&left);
    let between = (width - right_width - left_width - gap * 2.0).max(0.0);
    let centre = size_run(&cfg.positions[1], between, text);
    let centre_width = run_width(&centre);
    let sized = [left, centre, right];

    // The centre run is centred on the bar, but pushed aside rather than allowed to sit on
    // top of its neighbours: a wide right-hand run would otherwise overlap a centred clock
    // long before the bar is actually full.
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
            opacity: 1.0,
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
                name: None,
                alt_button: Button::Left,
                collapsible: false,
                collapse_button: Button::Right,
                refresh: None,
                mute: None,
                paged: None,
                on_click: None,
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
        alt: &std::collections::HashMap<String, usize>,
    ) -> Frame {
        frame_folded(config, items, native, alt, &Default::default())
    }

    fn frame_folded(
        config: &str,
        items: &[StatusItem],
        native: Registry,
        alt: &std::collections::HashMap<String, usize>,
        collapsed: &std::collections::HashSet<String>,
    ) -> Frame {
        frame_paged(config, items, native, alt, collapsed, &Default::default())
    }

    fn frame_paged(
        config: &str,
        items: &[StatusItem],
        native: Registry,
        alt: &std::collections::HashMap<String, usize>,
        collapsed: &std::collections::HashSet<String>,
        pages: &std::collections::HashMap<String, usize>,
    ) -> Frame {
        frame_waiting(
            config,
            items,
            native,
            alt,
            collapsed,
            pages,
            &Default::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn frame_waiting(
        config: &str,
        items: &[StatusItem],
        native: Registry,
        alt: &std::collections::HashMap<String, usize>,
        collapsed: &std::collections::HashSet<String>,
        pages: &std::collections::HashMap<String, usize>,
        waiting: &std::collections::HashSet<Which>,
    ) -> Frame {
        let cfg = Config::parse(config).expect("test config parses");
        let inputs = Inputs {
            items,
            native: &native,
            sway: &SwayState::default(),
            alt,
            pages,
            collapsed,
            waiting,
            spin: 3,
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
    fn a_language_module_says_what_the_config_calls_the_layout() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["lang"]

[module.lang]
source = "sway:language"
padding = 0

[module.lang.layouts]
"English (US)" = "EN"
"##;
        let cfg = Config::parse(config).expect("test config parses");
        let mut sway = SwayState::default();
        let render = |sway: &SwayState| {
            let inputs = Inputs {
                items: &[],
                native: &Registry::new(&Default::default()),
                sway,
                alt: &Default::default(),
                pages: &Default::default(),
                collapsed: &Default::default(),
                waiting: &Default::default(),
                spin: 0,
            };
            let frame = compute(&cfg, &inputs, 200.0, 10.0, &mut Fixed, None);
            frame.groups.first().map(|g| g.modules[0].text.clone())
        };

        // Nothing to show before the compositor has said anything, the same as a collector
        // that has not read yet.
        assert_eq!(render(&sway), None);

        sway.layout = Some(crate::sway::Layout {
            name: "English (US)".to_string(),
            index: 0,
        });
        assert_eq!(render(&sway).as_deref(), Some(" EN "));

        // A layout the config does not name is abbreviated rather than left out.
        sway.layout = Some(crate::sway::Layout {
            name: "Serbian".to_string(),
            index: 1,
        });
        assert_eq!(render(&sway).as_deref(), Some(" SE "));
    }

    /// The mode indicator is on the bar exactly while a mode is held. `default` is what a
    /// keyboard does anyway, so it is drawn as nothing at all rather than as the word.
    #[test]
    fn a_binding_mode_is_shown_only_while_one_is_held() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["mode"]

[module.mode]
source = "sway:mode"
padding = 0
"##;
        let cfg = Config::parse(config).expect("test config parses");
        let mut sway = SwayState::default();
        let render = |sway: &SwayState| {
            let inputs = Inputs {
                items: &[],
                native: &Registry::new(&Default::default()),
                sway,
                alt: &Default::default(),
                pages: &Default::default(),
                collapsed: &Default::default(),
                waiting: &Default::default(),
                spin: 0,
            };
            let frame = compute(&cfg, &inputs, 200.0, 10.0, &mut Fixed, None);
            frame.groups.first().map(|g| g.modules[0].text.clone())
        };

        // Before the compositor has answered, and while it is in the default mode, the
        // module draws nothing and takes the group with it.
        assert_eq!(render(&sway), None);
        sway.mode = Some(crate::sway::DEFAULT_MODE.to_string());
        assert_eq!(render(&sway), None);

        sway.mode = Some("resize".to_string());
        assert_eq!(render(&sway).as_deref(), Some(" resize "));

        // And it goes away again when the mode is left.
        sway.mode = Some(crate::sway::DEFAULT_MODE.to_string());
        assert_eq!(render(&sway), None);
    }

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

    /// One command reporting on three cities is three readings, and the module shows the
    /// one it is scrolled to. The wording is the module's either way: a page is a reading
    /// like any other, and nothing here knows it came from the same fetch as its
    /// neighbours.
    #[test]
    fn a_source_that_said_several_things_shows_the_page_it_is_scrolled_to() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["weather"]

[module.weather]
source = "command"
command = ["weather"]
interval = "once"
pages = true
padding = 0
format = "$text"
"##;
        let said = |text: &str| {
            let mut fields = Fields::default();
            fields.set("text", Value::Text(text.to_string()));
            fields.set_primary("text");
            Reading {
                fields,
                state: crate::status::State::Idle,
            }
        };
        let which = Which::Command(crate::collect::CommandSpec {
            argv: vec!["weather".to_string()],
            run: crate::collect::command::Run::Once,
            pages: true,
            fields: crate::collect::command::PLAIN,
        });
        let page = |showing: usize| {
            let native = Registry::fixture_pages(
                which.clone(),
                vec![said("Novi Sad"), said("Beograd"), said("Sokolac")],
            );
            let pages = std::collections::HashMap::from([("weather".to_string(), showing)]);
            frame_paged(
                config,
                &[],
                native,
                &Default::default(),
                &Default::default(),
                &pages,
            )
            .groups[0]
                .modules[0]
                .text
                .clone()
        };
        assert_eq!(page(0), "Novi Sad");
        assert_eq!(page(1), "Beograd");
        // A fetch that came back with fewer places than the last one leaves the module
        // pointing past the end, and it wraps rather than showing nothing.
        assert_eq!(page(4), "Beograd");
    }

    #[test]
    fn a_command_with_a_run_out_shows_a_spinner_where_its_icon_goes() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["weather"]

[module.weather]
source = "command"
command = ["weather"]
interval = "30m"
icon = "clock"
"##;
        let which = Which::Command(crate::collect::CommandSpec {
            argv: vec!["weather".to_string()],
            run: crate::collect::command::Run::Every(std::time::Duration::from_secs(1800)),
            pages: false,
            fields: crate::collect::command::PLAIN,
        });
        let mut fields = Fields::default();
        fields.set("text", Value::Text("18C".to_string()));
        fields.set_primary("text");
        let reading = Reading {
            fields,
            state: crate::status::State::Idle,
        };
        let drawn = |waiting: bool| {
            let waiting = match waiting {
                true => std::collections::HashSet::from([which.clone()]),
                false => std::collections::HashSet::new(),
            };
            let module = frame_waiting(
                config,
                &[],
                Registry::fixture(which.clone(), reading.clone()),
                &Default::default(),
                &Default::default(),
                &Default::default(),
                &waiting,
            )
            .groups[0]
                .modules[0]
                .clone();
            (module.text.clone(), module.icon.map(|i| (i.icon, i.level)))
        };
        // Nothing is out, so the module is its own icon and its last reading.
        assert_eq!(drawn(false), (" 18C ".to_string(), Some((Icon::Clock, 0))));
        // A run is out. The reading stays put - it is still the truth until the next one
        // lands - and the icon says that another is on its way.
        assert_eq!(drawn(true), (" 18C ".to_string(), Some((Icon::Spinner, 3))));
    }

    #[test]
    fn a_command_waiting_on_its_first_answer_is_drawn_anyway() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["weather"]

[module.weather]
source = "command"
command = ["weather"]
interval = "once"
"##;
        let which = Which::Command(crate::collect::CommandSpec {
            argv: vec!["weather".to_string()],
            run: crate::collect::command::Run::Once,
            pages: false,
            fields: crate::collect::command::PLAIN,
        });
        let frame = |waiting: std::collections::HashSet<Which>| {
            frame_waiting(
                config,
                &[],
                Registry::fixture(which.clone(), Reading::default()),
                &Default::default(),
                &Default::default(),
                &Default::default(),
                &waiting,
            )
        };
        // Nothing said yet and nothing on its way: the module is not there at all, the
        // same as any other source that has not read.
        assert!(frame(Default::default()).groups.is_empty());
        // The first run is out. There is no wording to show and no icon in the config,
        // and the spinner is enough to carry the module on its own.
        let module =
            frame(std::collections::HashSet::from([which.clone()])).groups[0].modules[0].clone();
        assert_eq!(module.text, "");
        assert_eq!(module.icon.map(|i| i.icon), Some(Icon::Spinner));
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

    const TWO_TONE: &str = r##"
[colors]
a = "#ff0000"
b = "#0000ff"

[left]
groups = ["g"]

[group.g]
modules = ["one", "two"]

[module.one]
padding = 0
background = "$a"

[module.two]
padding = 0
background = "$b"
"##;

    #[test]
    fn a_separator_is_never_the_same_colour_on_both_sides() {
        // Whichever neighbour the shape takes its colour from, the ground behind it has to
        // be the other one, or the boundary is invisible.
        for mode in ["previous", "next"] {
            let config = format!(
                "{TWO_TONE}\n[group.g.separator]\nshape = \"chevron\"\nwidth = 4\ncolor = \"{mode}\"\n"
            );
            let frame = frame_of(&config, &[item("one", "a"), item("two", "b")]);
            let sep = &frame.groups[0].separators[0];
            assert_ne!(sep.fill, sep.under, "with color = {mode:?}");
        }
    }

    #[test]
    fn an_end_comes_to_a_point_over_whatever_is_behind_the_bar() {
        let config = format!(
            "{TWO_TONE}\n[group.g.ends]\nleft = \"chevron\"\nright = \"chevron\"\nwidth = 6\n"
        );
        let frame = frame_of(&config, &[item("one", "a"), item("two", "b")]);
        let group = &frame.groups[0];
        assert_eq!(group.separators.len(), 2, "one end at each side");

        // Each end pairs its module's colour with nothing, so the shape reads against the
        // bar rather than against another module.
        for end in &group.separators {
            let colours = [end.fill, end.under];
            assert!(
                colours.contains(&Color::TRANSPARENT),
                "an end must leave one side clear: {colours:?}"
            );
        }

        // The ends are drawn beside the modules, not over them.
        let modules_width: f32 = group.modules.iter().map(|m| m.width).sum();
        assert_eq!(group.width, modules_width + 12.0);
        assert_eq!(group.modules[0].x, group.x + 6.0);
    }

    #[test]
    fn a_group_without_ends_reserves_no_room_for_them() {
        let frame = frame_of(TWO_TONE, &[item("one", "a"), item("two", "b")]);
        let group = &frame.groups[0];
        assert!(group.separators.is_empty());
        assert_eq!(group.modules[0].x, group.x);
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
        assert_eq!(
            frame.groups[0].modules[0].alt,
            Some(2),
            "one further wording is two views to go round"
        );
        assert_eq!(
            frame.groups[0].modules[0].name.as_deref(),
            Some("cpu"),
            "a click has to know which module it is turning"
        );
        assert_eq!(frame.groups[0].modules[1].alt, None);
        assert_eq!(
            frame.groups[0].modules[1].name, None,
            "a module no gesture names carries no name"
        );
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
        let showing = std::collections::HashMap::from([("cpu".to_string(), 1)]);
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
    fn a_module_can_have_several_further_wordings_and_goes_round_them() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
padding = 0
format = "first"
format_alt = ["second", "third"]
"##;
        let views = |showing: usize| {
            frame_showing(
                config,
                &[item("cpu", "x")],
                Registry::new(&Default::default()),
                &std::collections::HashMap::from([("cpu".to_string(), showing)]),
            )
            .groups[0]
                .modules[0]
                .text
                .clone()
        };
        assert_eq!(views(0), "first");
        assert_eq!(views(1), "second");
        assert_eq!(views(2), "third");

        let frame = frame_of(config, &[item("cpu", "x")]);
        assert_eq!(
            frame.groups[0].modules[0].alt,
            Some(3),
            "three views, so a click wraps after the third"
        );
    }

    /// The width of a one-module bar built from these style keys, with the stub measurer
    /// making every character one unit wide.
    fn width_of(keys: &str) -> f32 {
        let config = format!(
            r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]
padding = 0
spacing = 0

[module.cpu]
{keys}
"##
        );
        let frame = frame_of(&config, &[item("cpu", "abc")]);
        frame.groups[0].modules[0].width
    }

    #[test]
    fn padding_is_added_on_both_sides_and_nowhere_else() {
        // Three characters, no icon: the module is the text plus a padding each side.
        assert_eq!(width_of("padding = 0"), 3.0);
        assert_eq!(width_of("padding = 5"), 13.0);
        assert_eq!(width_of("padding = 5.5"), 14.0);
    }

    #[test]
    fn the_gap_between_an_icon_and_its_text_is_the_configured_one() {
        let keys = |gap: &str| format!("padding = 0\nicon = \"cpu\"\nicon_size = 10\n{gap}");
        // Icon, gap, then the text.
        assert_eq!(width_of(&keys("icon_gap = 0")), 13.0);
        assert_eq!(width_of(&keys("icon_gap = 4")), 17.0);
        // Without one, the gap is a quarter of the icon.
        assert_eq!(width_of(&keys("")), 15.5);
    }

    #[test]
    fn a_bigger_icon_keeps_its_breathing_room_without_being_told() {
        let width =
            |size: f32| width_of(&format!("padding = 0\nicon = \"cpu\"\nicon_size = {size}"));
        // Twice the icon is twice the gap, so the proportions hold as the bar grows.
        assert_eq!(width(10.0) - 3.0, 12.5);
        assert_eq!(width(20.0) - 3.0, 25.0);
    }

    #[test]
    fn a_battery_is_given_the_room_a_long_icon_needs() {
        let width = |icon: &str| {
            width_of(&format!(
                "padding = 0\nicon_gap = 0\nicon_size = 20\nicon = \"{icon}\""
            ))
        };
        // Three characters of text either way, and a square icon takes its size.
        assert_eq!(width("cpu"), 23.0);
        // The battery asks for a quarter more, and the text starts behind all of it.
        assert_eq!(width("battery"), 28.0);
        assert_eq!(width("battery-charging"), 28.0);
    }

    #[test]
    fn a_full_bar_truncates_rather_than_drawing_over_itself() {
        // The stub measurer makes every character a unit wide, and the bar is 200 of them.
        let config = r##"
[left]
groups = ["l"]

[center]
groups = ["c"]

[right]
groups = ["r"]

[group.l]
modules = ["title"]
padding = 0

[group.c]
modules = ["media"]
padding = 0

[group.r]
modules = ["clock"]
padding = 0

[module.title]
padding = 0

[module.media]
padding = 0

[module.clock]
padding = 0
"##;
        let long = "x".repeat(150);
        let frame = frame_of(
            config,
            &[
                item("title", &long),
                item("media", &long),
                item("clock", "12:00"),
            ],
        );

        let mut edges: Vec<(f32, f32)> = frame
            .groups
            .iter()
            .flat_map(|g| g.modules.iter())
            .map(|m| (m.x, m.x + m.width))
            .collect();
        edges.sort_by(|a, b| a.0.partial_cmp(&b.0).expect("finite"));
        for pair in edges.windows(2) {
            assert!(
                pair[0].1 <= pair[1].0,
                "modules overlap: {:?} then {:?}",
                pair[0],
                pair[1]
            );
        }
        let last = edges.last().expect("something was drawn");
        assert!(
            last.1 <= 200.0,
            "the bar draws past its own width: {last:?}"
        );
    }

    #[test]
    fn a_folded_module_keeps_its_icon_and_loses_its_text() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
padding = 0
icon = "cpu"
icon_size = 10
collapsible = true
"##;
        let open = frame_of(config, &[item("cpu", "a long wording")]);
        assert_eq!(open.groups[0].modules[0].text, "a long wording");
        assert!(
            open.groups[0].modules[0].collapsible,
            "the frame has to say a right click can fold it"
        );
        assert_eq!(open.groups[0].modules[0].name.as_deref(), Some("cpu"));

        let folded = frame_folded(
            config,
            &[item("cpu", "a long wording")],
            Registry::new(&Default::default()),
            &Default::default(),
            &std::collections::HashSet::from(["cpu".to_string()]),
        );
        assert_eq!(folded.groups[0].modules[0].text, "");
        assert!(
            folded.groups[0].modules[0].icon.is_some(),
            "the icon is what is left to click on"
        );
        assert!(
            folded.groups[0].modules[0].width < open.groups[0].modules[0].width,
            "folding is only worth doing if it takes less room"
        );
    }

    #[test]
    fn a_folded_module_is_its_icon_centred_and_nothing_else() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
padding = 6
icon = "cpu"
icon_size = 12
collapsible = true
"##;
        let folded = frame_folded(
            config,
            &[item("cpu", "50%")],
            Registry::new(&Default::default()),
            &Default::default(),
            &std::collections::HashSet::from(["cpu".to_string()]),
        );
        let module = &folded.groups[0].modules[0];
        // The icon and its padding, and not the gap that would have separated it from
        // text there is none of: 12 + 6 + 6.
        assert_eq!(module.width, 24.0);
        let icon = module.icon.expect("the icon is what is left");
        // Which is what leaves the same room either side of it.
        assert_eq!(icon.x - module.x, 6.0);
        assert_eq!((module.x + module.width) - (icon.x + icon.size), 6.0);
    }

    #[test]
    fn a_module_with_nothing_to_say_still_disappears_when_it_is_not_folded() {
        // An empty wording means "hide this", and that has to keep working next to a
        // module that is empty on purpose.
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
padding = 0
icon = "cpu"
collapsible = true
"##;
        let frame = frame_of(config, &[item("cpu", "")]);
        assert!(frame.groups.is_empty() || frame.groups[0].modules.is_empty());
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
    fn a_rule_can_match_a_word_the_source_published() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["bat"]

[module.bat]
source = "battery"
format = "x"
icon = "battery"

[module.bat.states.charging]
field = "status"
equals = "charging"
icon = "battery-charging"
"##;
        let charging = |status: &str| {
            let mut fields = Fields::default();
            fields.set(
                "percent",
                Value::Num {
                    v: 50.0,
                    unit: crate::status::Unit::Percent,
                },
            );
            fields.set("status", Value::Text(status.to_string()));
            fields.set_primary("percent");
            Registry::fixture(
                Which::Battery,
                Reading {
                    fields,
                    state: State::Idle,
                },
            )
        };

        let on = frame_with(config, &[], charging("charging"));
        assert_eq!(
            on.groups[0].modules[0].icon.unwrap().icon,
            Icon::BatteryCharging
        );

        let off = frame_with(config, &[], charging("discharging"));
        assert_eq!(off.groups[0].modules[0].icon.unwrap().icon, Icon::Battery);
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
