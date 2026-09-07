//! Turns config plus the current status items into positioned rectangles.
//!
//! The result is purely geometric: the renderer draws it and the pointer code hit-tests it,
//! neither needs to know about config or where the items came from.

use crate::collect::{Registry, Which};
use crate::color::Color;
use crate::config::{
    Config, Direction, EdgeShape, Edges, Ends, Group as GroupCfg, Module as ModuleCfg, Scope,
    Separator, SeparatorColor, SeparatorShape, Source, StateFlags, Style,
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
    /// What the system tray is showing, when a module asks for one.
    pub tray: &'a crate::tray::TrayState,
    /// The screen this bar is on, as the compositor names it, when it is known.
    ///
    /// Everything drawn from the compositor is about a screen: the workspaces on it and
    /// the window it is showing. Nothing else in layout has any use for it.
    pub output: Option<&'a str>,
}

impl<'a> Inputs<'a> {
    /// Whether a screen's worth of the compositor belongs on this bar.
    ///
    /// A bar that does not know which screen it is on shows everything: half a workspace
    /// list is worse than a whole one, and an unnamed output is not something a config can
    /// have asked for either.
    fn on_this_screen(&self, scope: Scope, output: &str) -> bool {
        match (scope, self.output) {
            (Scope::Session, _) | (_, None) => true,
            (Scope::Output, Some(mine)) => mine == output,
        }
    }

    /// The window this bar is about: the one on its own screen, or whichever has the
    /// session's focus.
    fn window(&self, scope: Scope) -> Option<&crate::sway::Window> {
        let sway = self.sway;
        let output = match scope {
            Scope::Output => self.output.or(sway.focused_output.as_deref()),
            Scope::Session => sway.focused_output.as_deref(),
        }?;
        sway.windows.get(output)
    }
}

/// A menu laid out: where every row sits, and what is drawn on it.
///
/// Geometry and colour like `Frame`, and for the same reason - the renderer draws this
/// without knowing a bus exists.
#[derive(Clone, Debug)]
pub struct MenuFrame {
    pub width: f32,
    pub height: f32,
    pub background: Color,
    pub radius: f32,
    pub rows: Vec<PlacedRow>,
}

#[derive(Clone, Debug)]
pub struct PlacedRow {
    pub y: f32,
    pub height: f32,
    /// A rule rather than a row: drawn as a line and never highlighted.
    pub separator: bool,
    /// Whether the pointer is on this row.
    pub highlight: bool,
    /// What that highlight is painted in.
    pub highlight_color: Color,
    pub text: String,
    pub text_x: f32,
    pub text_y: f32,
    pub foreground: Color,
    pub icon: Option<PlacedIcon>,
    /// Where a tick goes, for a row that carries one and is on.
    pub mark: Option<(f32, f32)>,
    /// Where the arrow saying "there is more" goes.
    pub arrow: Option<(f32, f32)>,
}

impl Default for MenuFrame {
    fn default() -> MenuFrame {
        MenuFrame {
            width: 0.0,
            height: 0.0,
            background: Color::TRANSPARENT,
            radius: 0.0,
            rows: Vec::new(),
        }
    }
}

impl MenuFrame {
    /// Which row is at this point, if it is one that can be chosen.
    pub fn row_at(&self, y: f32) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| !row.separator && y >= row.y && y < row.y + row.height)
    }
}

/// Lay out a menu: one row per entry, wide enough for the longest of them.
///
/// The width is the menu's own business rather than the bar's - a menu is a separate
/// surface and has no column to fit into - so it is measured from what it has to say and
/// capped where a label would otherwise run off the screen.
pub fn menu(
    rows: &[crate::tray::menu::Row],
    style: &crate::config::Menu,
    icon_size: f32,
    line: f32,
    hover: Option<usize>,
    text: &mut dyn Measure,
) -> MenuFrame {
    let row_height = (line + style.padding).max(icon_size + 4.0);
    let rule_height = (style.padding * 0.75).max(3.0);
    // Room on the left for a picture or a tick, and on the right for the arrow that says a
    // row opens another menu. Both columns exist whether or not every row uses them, so
    // the labels line up instead of stepping in and out.
    let gutter = icon_size + style.padding * 0.5;
    let arrow = line * 0.4;

    // Cut to what the menu is allowed to be before anything is measured. A label is
    // written by the application, not by dbar, and a long one used to be shaped whole and
    // then drawn straight off the edge of the popup.
    let room = style.max_width - (style.padding * 3.0 + gutter + arrow);
    let labels: Vec<String> = rows
        .iter()
        .map(|row| match row.separator {
            true => String::new(),
            false => truncate(&row.label, room, text),
        })
        .collect();

    let widest = labels
        .iter()
        .map(|label| text.measure(label))
        .fold(0.0f32, f32::max);
    let width = (style.padding * 2.0 + gutter + widest + style.padding + arrow)
        .min(style.max_width)
        .max(icon_size * 3.0);

    let mut placed = Vec::with_capacity(rows.len());
    let mut y = style.padding;
    for (index, row) in rows.iter().enumerate() {
        let height = match row.separator {
            true => rule_height,
            false => row_height,
        };
        let highlight = hover == Some(index) && row.selectable();
        let foreground = match (row.separator, row.enabled, highlight) {
            (true, _, _) => style.separator,
            (false, false, _) => style.disabled,
            (false, true, true) => style.highlight_foreground,
            (false, true, false) => style.foreground,
        };
        let text_x = style.padding + gutter;
        placed.push(PlacedRow {
            y,
            height,
            separator: row.separator,
            highlight,
            highlight_color: style.highlight,
            text: labels[index].clone(),
            text_x,
            text_y: y + (height - line) / 2.0,
            foreground,
            icon: row.icon.as_ref().map(|art| PlacedIcon {
                icon: Icon::Raster,
                level: 0,
                x: style.padding,
                y: y + (height - icon_size) / 2.0,
                size: icon_size,
                art: Some(art.clone()),
            }),
            // A tick shares the left column with a picture, and a row with both is not a
            // thing any menu sends.
            mark: (row.toggle == Some(true) && row.icon.is_none())
                .then(|| (style.padding, y + height / 2.0)),
            arrow: row
                .submenu
                .then(|| (width - style.padding - arrow, y + height / 2.0)),
        });
        y += height;
    }

    MenuFrame {
        width,
        height: y + style.padding,
        background: style.background,
        radius: style.radius,
        rows: placed,
    }
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
#[derive(Clone, Debug)]
pub struct PlacedIcon {
    pub icon: Icon,
    pub level: usize,
    pub x: f32,
    pub y: f32,
    pub size: f32,
    /// Pixels to draw instead of the built-in outline, for an icon that arrived as a
    /// picture. Shared rather than copied: it outlives the frames that draw it.
    pub art: Option<Arc<crate::icon::Raster>>,
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
    /// Fill the complement of the shape, keeping an outer cap attached to its module.
    pub inverted: bool,
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

#[derive(Clone, Debug)]
pub struct Frame {
    pub groups: Vec<PlacedGroup>,
    /// Shared transitions between groups; their colours depend on both neighbours.
    pub group_separators: Vec<PlacedSeparator>,
    /// The bar's own ground, under everything the groups draw.
    ///
    /// Here rather than read from the config while painting, so that nothing below this
    /// point knows a config exists - the same deal `MenuFrame` already had. A renderer
    /// takes positioned geometry and colour and needs nothing else; that is what makes it
    /// replaceable.
    pub background: Color,
    pub radius: f32,
}

impl Default for Frame {
    fn default() -> Frame {
        Frame {
            groups: Vec::new(),
            group_separators: Vec::new(),
            background: Color::TRANSPARENT,
            radius: 0.0,
        }
    }
}

fn contains(x: f32, y: f32, rx: f32, ry: f32, rw: f32, rh: f32) -> bool {
    x >= rx && x < rx + rw && y >= ry && y < ry + rh
}

/// What of the bar has changed since the last frame was drawn.
///
/// This is what the compositor is told, not what dbar repaints: the whole surface is
/// painted every time, and saying so needlessly makes the compositor copy and composite a
/// strip it already has. Being wrong the other way - claiming less than really changed -
/// leaves the old pixels on screen, so everything here errs towards saying more.
#[derive(Clone, Debug, PartialEq)]
pub enum Damage {
    /// Something structural moved, or this is the first frame. Say the lot.
    All,
    /// Only these rectangles, in logical pixels.
    Rects(Vec<(f32, f32, f32, f32)>),
}

/// The surface a frame is drawn on: its logical size, and the scale it is painted at.
///
/// Damage is worked out in logical pixels, so two frames can lay out identically on
/// surfaces that do not share a single pixel - a monitor whose scale changed being the
/// plain case.
pub type Surface = (u32, u32, i32);

/// Whether two of the same thing would be drawn identically.
///
/// Deliberately not `PartialEq`: a placed module carries what a click on it does and what
/// it is called, and none of that reaches the screen. Comparing those as well would report
/// a change nobody can see.
trait SamePaint {
    fn same_paint(&self, other: &Self) -> bool;
}

impl SamePaint for PlacedIcon {
    fn same_paint(&self, other: &Self) -> bool {
        self.icon == other.icon
            && self.level == other.level
            && self.x == other.x
            && self.y == other.y
            && self.size == other.size
            // Pictures are compared by identity rather than by pixel: the same artwork
            // arrives as the same `Arc` until the application replaces it, and a copy that
            // happens to match would only be reported as a change, which is safe.
            && match (&self.art, &other.art) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
    }
}

impl SamePaint for PlacedModule {
    fn same_paint(&self, other: &Self) -> bool {
        self.x == other.x
            && self.y == other.y
            && self.width == other.width
            && self.height == other.height
            && self.text == other.text
            && self.text_x == other.text_x
            && self.foreground == other.foreground
            && self.background == other.background
            && self.radius == other.radius
            && match (&self.icon, &other.icon) {
                (Some(a), Some(b)) => a.same_paint(b),
                (None, None) => true,
                _ => false,
            }
    }
}

impl SamePaint for PlacedSeparator {
    fn same_paint(&self, other: &Self) -> bool {
        self.x == other.x
            && self.y == other.y
            && self.width == other.width
            && self.height == other.height
            && self.shape == other.shape
            && self.direction == other.direction
            && self.overlap == other.overlap
            && self.inverted == other.inverted
            && self.fill == other.fill
            && self.under == other.under
    }
}

impl SamePaint for PlacedGroup {
    fn same_paint(&self, other: &Self) -> bool {
        self.x == other.x
            && self.y == other.y
            && self.width == other.width
            && self.height == other.height
            && self.background == other.background
            && self.opacity == other.opacity
            && self.edges == other.edges
            && self.modules.len() == other.modules.len()
            && self.separators.len() == other.separators.len()
            && (self.modules.iter())
                .zip(&other.modules)
                .all(|(a, b)| a.same_paint(b))
            && (self.separators.iter())
                .zip(&other.separators)
                .all(|(a, b)| a.same_paint(b))
    }
}

impl Frame {
    /// What changed between the frame that is on screen and this one.
    ///
    /// The island is the unit rather than the module, and on purpose: a separator bleeds
    /// under the modules it runs between, a group's ends are rounded over its corners, and
    /// a translucent one is composited as a single object. Naming a module's own rectangle
    /// would cut through all three. An island is a few hundred pixels of a bar that is
    /// thousands wide, so there is nothing to gain by being cleverer.
    /// What has to be repainted, given the surface the frame on screen was presented on.
    ///
    /// A frame that lays out the same way on a different surface is not the same picture:
    /// the buffer is a new size, or the same size at another scale, and nothing in it has
    /// been painted yet. Comparing the two layouts would find nothing to say and leave the
    /// compositor with a buffer it was never told to look at.
    ///
    /// `presented` is `None` until a frame has actually reached the screen, which covers
    /// the first draw and every draw after one that failed.
    pub fn damage_since(
        &self,
        on_screen: &Frame,
        presented: Option<Surface>,
        now: Surface,
    ) -> Damage {
        match presented == Some(now) {
            true => self.damage(on_screen),
            false => Damage::All,
        }
    }

    pub fn damage(&self, on_screen: &Frame) -> Damage {
        if self.groups.len() != on_screen.groups.len()
            || self.group_separators.len() != on_screen.group_separators.len()
            || self.background != on_screen.background
            || self.radius != on_screen.radius
        {
            return Damage::All;
        }
        let mut rects = Vec::new();
        for (new, old) in self.groups.iter().zip(&on_screen.groups) {
            if new.same_paint(old) {
                continue;
            }
            // Both rectangles: a group that moved or shrank has to repair where it was as
            // well as cover where it is now.
            rects.push((old.x, old.y, old.width, old.height));
            rects.push((new.x, new.y, new.width, new.height));
        }
        for (new, old) in self
            .group_separators
            .iter()
            .zip(&on_screen.group_separators)
        {
            if !new.same_paint(old) {
                for sep in [old, new] {
                    rects.push((
                        sep.x - sep.overlap,
                        sep.y,
                        sep.width + sep.overlap * 2.0,
                        sep.height,
                    ));
                }
            }
        }
        Damage::Rects(rects)
    }
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
///
/// Only a window of the text is ever measured. Measuring means shaping, which is charged
/// per character and cached by string, and what arrives here is somebody else's: a window
/// title, a line from a script, a track name. A module a few hundred pixels wide can show
/// a few hundred characters, so shaping a hundred thousand of them to find that out would
/// be the sender deciding how much work the bar does, and how much memory the cache holds.
fn truncate(text: &str, budget: f32, measure: &mut dyn Measure) -> String {
    let (text, whole) = window(text, budget, measure);
    if whole && measure.measure(text) <= budget {
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

/// The most characters that are ever shaped to lay out one string.
///
/// The window below grows until the text is too wide to draw, which is the right question
/// to ask of text that takes up room. Not everything does: a run of zero-width spaces is
/// as wide as the empty string however much of it there is, and the window would grow to
/// the length of whatever arrived. This is the backstop, and it is in characters because
/// that is what shaping is charged in.
///
/// Four thousand is far past anything a bar can show - a 4K screen at the smallest
/// readable size holds a few hundred - and small enough that the pathological case costs
/// a millisecond rather than a second.
const MOST_SHAPED: usize = 4096;

/// As much of `text` as could possibly be drawn in `budget`, and whether that is all of it.
///
/// Doubled from a small window until it overflows, so the work is proportional to what
/// fits rather than to what was sent: at most twice the characters that can be shown are
/// ever measured, whatever the font and whatever arrives.
fn window<'a>(text: &'a str, budget: f32, measure: &mut dyn Measure) -> (&'a str, bool) {
    const FIRST: usize = 32;
    let mut take = FIRST;
    loop {
        let end = text
            .char_indices()
            .nth(take)
            .map(|(i, _)| i)
            .unwrap_or(text.len());
        let head = &text[..end];
        if end == text.len() {
            return (head, true);
        }
        if take >= MOST_SHAPED || measure.measure(head) > budget {
            return (head, false);
        }
        take *= 2;
    }
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
    /// Pixels for an icon that arrived as a picture, carried beside `icon`.
    art: Option<Arc<crate::icon::Raster>>,
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
    /// An icon the source brought with it, as pixels, for a source whose artwork is not
    /// dbar's to choose.
    art: Option<Arc<crate::icon::Raster>>,
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
        art: None,
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
                            art: None,
                        });
                    }
                    continue;
                };
                out.push(Candidate {
                    art: None,
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
            Source::SwayWindow(scope) => {
                if let Some(window) = inputs.window(*scope) {
                    let mut fields = Fields::default();
                    fields.set("title", Value::Text(window.title.clone()));
                    // One or the other, never both: what a Wayland client calls itself, or
                    // what an X11 one does. The one the window does not have is absent
                    // rather than empty, so `{$app_id}` disappears instead of drawing a
                    // gap, and `$app_id|$class` names whichever it is.
                    for (name, value) in [("app_id", &window.app_id), ("class", &window.class)] {
                        match value.is_empty() {
                            true => fields.set(name, Value::Absent),
                            false => fields.set(name, Value::Text(value.clone())),
                        }
                    }
                    out.push(Candidate {
                        art: None,
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
                        art: None,
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
                        art: None,
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
            // One application, one rectangle - the same expansion a workspace list gets,
            // and for the same reason: each needs its own icon, state and click target.
            Source::Tray(view) => {
                // Registration order unless the config named one, and filtered to what it
                // asked to see. A handful of items either way: an application registers an
                // icon, not a thousand of them.
                let mut items: Vec<&crate::tray::Item> = inputs
                    .tray
                    .items
                    .iter()
                    .filter(|item| view.shows(item.status))
                    .collect();
                if !view.order.is_empty() {
                    // Stable, so everything the config did not name keeps the order it
                    // arrived in rather than shuffling when an application restarts.
                    items.sort_by_key(|item| view.rank(&item.id));
                }
                for item in items {
                    let mut fields = Fields::default();
                    fields.set("title", Value::Text(item.title.clone()));
                    fields.set("id", Value::Text(item.id.clone()));
                    fields.set("status", Value::Text(item.status.name().to_string()));
                    out.push(Candidate {
                        art: item.icon.clone(),
                        module,
                        text: wording(module, inputs.alt).render(&fields),
                        flags: StateFlags {
                            urgent: item.status == crate::tray::Status::NeedsAttention,
                            ..StateFlags::default()
                        },
                        values: fields,
                        foreground: None,
                        background: None,
                        pages: 1,
                        action: Some(ActionTarget::Tray {
                            key: item.key.clone(),
                        }),
                    });
                }
            }
            Source::SwayWorkspaces(scope) => {
                for workspace in &inputs.sway.workspaces {
                    if !inputs.on_this_screen(*scope, &workspace.output) {
                        continue;
                    }
                    let mut fields = Fields::default();
                    fields.set("name", Value::Text(workspace.name.clone()));
                    out.push(Candidate {
                        art: None,
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
    joined_ends: Option<Ends>,
) -> Option<SizedGroup> {
    let ends = joined_ends.unwrap_or(group.ends);
    let advance = if group.separator.shape.is_none() {
        group.spacing
    } else {
        group.separator.width
    };
    // What is left for the modules once the group's own padding is paid for. A run with
    // nothing else beside it gets infinity, and nothing below has to think about it.
    let mut left = budget - group.padding * 2.0;
    if joined_ends.is_some() {
        left -= ends.left_width() + ends.right_width();
    }
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
            art,
        } = candidate;
        // Whether this module's program is out, which the spinner is drawn for. The check
        // is skipped outright while nothing is waiting, which is nearly always.
        let waiting = !inputs.waiting.is_empty()
            && matches!(&module.source, Source::Native(which) if inputs.waiting.contains(which));
        // The i3bar protocol uses an empty `full_text` to mean "hide this block". A module
        // folded down is empty on purpose and stays, because its icon is still there, and
        // so does one whose spinner is the only thing it has to show.
        let folded = module.collapsible && inputs.collapsed.contains(&module.name);
        // A module with a picture to show is not empty, whatever its wording says: a tray
        // item is its icon, and most of them have nothing written on them at all.
        if content.is_empty() && !folded && !waiting && art.is_none() {
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
        if content.is_empty() && style.icon.is_none() && !waiting && art.is_none() {
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
        // A picture the source handed over stands in for whatever icon the style names:
        // an application's own artwork is the thing a tray module exists to show.
        let icon = match (waiting, art.is_some()) {
            (true, _) => Some((Icon::Spinner, inputs.spin)),
            (false, true) => Some((Icon::Raster, 0)),
            (false, false) => style.icon.map(|icon| {
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
        let available = if joined_ends.is_some() && !modules.is_empty() {
            left - if group.separator.shape.is_none() {
                group.spacing
            } else {
                group.separator.width
            }
        } else {
            left
        };
        let fixed = advance(&content) + style.padding * 2.0;
        let cap = match style.max_width > 0.0 {
            true => style.max_width.min(available),
            false => available,
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
        if content.is_empty() && style.icon.is_none() && !waiting && art.is_none() {
            continue;
        }

        let text_width = text.measure(&content);
        let width = (text_width + fixed).max(style.min_width);
        // A module with nothing left to draw in is left out entirely, rather than drawn
        // over whatever the run was making room for.
        if width > available {
            continue;
        }
        left = if joined_ends.is_some() {
            available - width
        } else {
            left - width - group.spacing
        };
        modules.push(SizedModule {
            width,
            text_width,
            hover_style,
            icon_advance,
            icon,
            art,
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

    let content: f32 = modules.iter().map(|m| m.width).sum();
    let gaps = advance * (modules.len() - 1) as f32;
    // A shaped end needs room of its own: it is drawn beside the modules, not over them.
    let width = content + gaps + ends.left_width() + ends.right_width() + group.padding * 2.0;
    let _ = height;

    Some(SizedGroup {
        width,
        background: group.background,
        opacity: group.opacity,
        edges: group.edges,
        padding: group.padding,
        advance,
        separator: group.separator,
        ends,
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

    let ends = sized.ends;
    let lead = ends.left_width();
    x += lead;

    for (i, m) in sized.modules.into_iter().enumerate() {
        if i > 0 {
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
            art: m.art.clone(),
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

    if lead > 0.0
        && let Some(first) = modules.first()
    {
        separators.push(end_separator(
            ends.left,
            group_x + sized.padding,
            inner_y,
            lead,
            inner_h,
            &ends,
            separator.direction,
            first.background,
            true,
        ));
    }
    if draw_separators {
        for pair in modules.windows(2) {
            separators.push(separator_between(
                separator,
                &pair[0],
                &pair[1],
                sized.background,
            ));
        }
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
            false,
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

/// The same colour rule serves internal separators and joins between groups.
fn separator_between(
    separator: Separator,
    previous: &PlacedModule,
    next: &PlacedModule,
    ground: Color,
) -> PlacedSeparator {
    PlacedSeparator {
        x: previous.x + previous.width,
        y: previous.y,
        width: separator.width,
        height: previous.height,
        shape: separator.shape,
        direction: separator.direction,
        overlap: separator.overlap,
        inverted: false,
        fill: match separator.color {
            SeparatorColor::Previous => previous.background,
            SeparatorColor::Next => next.background,
            SeparatorColor::Foreground => previous.foreground,
            SeparatorColor::Background => ground,
            SeparatorColor::Fixed(c) => c,
        },
        under: match separator.color {
            SeparatorColor::Next => previous.background,
            _ => next.background,
        },
    }
}

/// Fit connected groups in one pass. The last accepted group provisionally owns its
/// trailing cap; accepting another group replaces that cap with a shared separator.
/// Empty or truncated-away groups never consume a join or change the outer edges.
fn size_joined_run(
    groups: &[GroupCfg],
    separator: Separator,
    inputs: &Inputs<'_>,
    height: f32,
    text: &mut dyn Measure,
    budget: f32,
) -> Vec<SizedGroup> {
    let mut out: Vec<SizedGroup> = Vec::new();
    let mut left = budget;
    for group in groups {
        let mut ends = group.ends;
        let (reclaimed, gap) = match out.last() {
            Some(previous) => {
                ends.left = SeparatorShape::None;
                (previous.ends.right_width(), separator.width)
            }
            None => (0.0, 0.0),
        };
        let room = left + reclaimed - gap;
        let Some(mut sized) = size_group(group, inputs, height, text, room, Some(ends)) else {
            continue;
        };
        if let Some(previous) = out.last_mut() {
            previous.width -= reclaimed;
            previous.ends.right = SeparatorShape::None;
            previous.edges.right = EdgeShape::None;
            sized.edges.left = EdgeShape::None;
        }
        left = room - sized.width;
        out.push(sized);
    }
    out
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
    leading: bool,
) -> PlacedSeparator {
    let direction = ends.direction.unwrap_or(direction);
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
        // Only slants need a complementary triangle to change slope while staying
        // attached. Other cap shapes retain their established pointing behavior.
        inverted: shape == SeparatorShape::Slant && leading == (direction == Direction::Right),
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
    let mut frame = Frame {
        background: cfg.bar.background,
        radius: cfg.bar.radius,
        ..Frame::default()
    };

    let run_width = |groups: &Vec<SizedGroup>, separator: Option<Separator>| -> f32 {
        if groups.is_empty() {
            return 0.0;
        }
        groups.iter().map(|g| g.width).sum::<f32>()
            + separator.map_or(gap, |s| s.width) * (groups.len() - 1) as f32
    };

    // Sized in the order they get to keep their width: the right run says what it needs,
    // the left run takes what is left, and the centre lives in the gap between them. A run
    // that runs out of room truncates the module it is in the middle of and drops the rest,
    // rather than drawing over its neighbour.
    let size_run = |position: &crate::config::Position,
                    budget: f32,
                    text: &mut dyn Measure|
     -> Vec<SizedGroup> {
        if let Some(separator) = position.separator {
            return size_joined_run(&position.groups, separator, inputs, height, text, budget);
        }
        let mut left = budget;
        let mut out = Vec::new();
        for group in &position.groups {
            let Some(sized) = size_group(group, inputs, height, text, left, None) else {
                continue;
            };
            left -= sized.width + gap;
            out.push(sized);
        }
        out
    };

    let right = size_run(&cfg.positions[2], width, text);
    let right_width = run_width(&right, cfg.positions[2].separator);
    let left = size_run(
        &cfg.positions[0],
        (width - right_width - gap).max(0.0),
        text,
    );
    let left_width = run_width(&left, cfg.positions[0].separator);
    let between = (width - right_width - left_width - gap * 2.0).max(0.0);
    let centre = size_run(&cfg.positions[1], between, text);
    let centre_width = run_width(&centre, cfg.positions[1].separator);
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

    for ((groups, mut x), position) in sized.into_iter().zip(starts).zip(&cfg.positions) {
        let first = frame.groups.len();
        for group in groups {
            let w = group.width;
            let placed = place(group, x, height, pointer);
            if let Some(separator) = position.separator
                && frame.groups.len() > first
                && let Some(previous) = frame.groups.last().and_then(|g| g.modules.last())
                && let Some(next) = placed.modules.first()
            {
                frame.group_separators.push(separator_between(
                    separator,
                    previous,
                    next,
                    frame.background,
                ));
            }
            frame.groups.push(placed);
            x += w + position.separator.map_or(gap, |s| s.width);
        }
    }

    frame
}

/// A frame showing a single message from dbar itself.
///
/// Provider failures bypass the group configuration entirely: a fault reported as an ordinary
/// block would be dropped by any group that selects modules by name, which is exactly when the
/// message matters most.
pub fn fault(
    cfg: &Config,
    message: &str,
    width: f32,
    height: f32,
    text: &mut dyn Measure,
) -> Frame {
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
        // The bar keeps its own ground while it is saying what went wrong: a message that
        // needs reading is not the moment to lose the surface it is read on.
        background: cfg.bar.background,
        radius: cfg.bar.radius,
        ..Frame::default()
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
            tray: &Default::default(),
            output: None,
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
                tray: &Default::default(),
                output: None,
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

    /// A bar exists once per screen, so its workspace list is about that screen. Listing
    /// all of them would put the other monitor's workspaces on this one, which is the whole
    /// thing multiple bars are for.
    #[test]
    fn a_workspace_list_is_about_the_screen_its_bar_is_on() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["ws"]

[module.ws]
source = "sway:workspaces"
padding = 0
"##;
        let cfg = Config::parse(config).expect("test config parses");
        let sway = two_screens();
        let on = |output: Option<&str>| {
            let inputs = Inputs {
                items: &[],
                native: &Registry::new(&Default::default()),
                sway: &sway,
                alt: &Default::default(),
                pages: &Default::default(),
                collapsed: &Default::default(),
                waiting: &Default::default(),
                spin: 0,
                tray: &Default::default(),
                output,
            };
            let frame = compute(&cfg, &inputs, 200.0, 10.0, &mut Fixed, None);
            let modules = frame.groups.first().map(|g| g.modules.clone());
            modules
                .unwrap_or_default()
                .iter()
                .map(|m| m.text.clone())
                .collect::<Vec<_>>()
        };

        assert_eq!(on(Some("DP-1")), ["1", "3"]);
        assert_eq!(on(Some("HDMI-A-1")), ["2"]);
        // A bar that has not been told which screen it is on shows the lot, because half a
        // list is worse than a whole one.
        assert_eq!(on(None), ["1", "2", "3"]);
    }

    /// `scope = "session"` is what a single-screen configuration always had, and what
    /// someone who wants every workspace on every bar asks for.
    #[test]
    fn a_workspace_list_can_be_asked_for_the_whole_session() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["ws"]

[module.ws]
source = "sway:workspaces"
scope = "session"
padding = 0
"##;
        let cfg = Config::parse(config).expect("test config parses");
        let sway = two_screens();
        let inputs = Inputs {
            items: &[],
            native: &Registry::new(&Default::default()),
            sway: &sway,
            alt: &Default::default(),
            pages: &Default::default(),
            collapsed: &Default::default(),
            waiting: &Default::default(),
            spin: 0,
            tray: &Default::default(),
            output: Some("HDMI-A-1"),
        };
        let frame = compute(&cfg, &inputs, 200.0, 10.0, &mut Fixed, None);
        let texts: Vec<String> = frame.groups[0]
            .modules
            .iter()
            .map(|m| m.text.clone())
            .collect();
        assert_eq!(texts, ["1", "2", "3"]);
    }

    /// The same for the title above it: only one window in the session has focus, and the
    /// other screen is still showing something.
    #[test]
    fn a_window_module_says_what_its_own_screen_is_showing() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["win"]

[module.win]
source = "sway:window"
padding = 0
"##;
        let cfg = Config::parse(config).expect("test config parses");
        let sway = two_screens();
        let on = |output: Option<&str>| {
            let inputs = Inputs {
                items: &[],
                native: &Registry::new(&Default::default()),
                sway: &sway,
                alt: &Default::default(),
                pages: &Default::default(),
                collapsed: &Default::default(),
                waiting: &Default::default(),
                spin: 0,
                tray: &Default::default(),
                output,
            };
            let frame = compute(&cfg, &inputs, 200.0, 10.0, &mut Fixed, None);
            frame.groups.first().map(|g| g.modules[0].text.clone())
        };

        assert_eq!(on(Some("DP-1")).as_deref(), Some("vim"));
        assert_eq!(on(Some("HDMI-A-1")).as_deref(), Some("a page"));
        // Nothing said about the screen: the one with the focus is the best guess there is.
        assert_eq!(on(None).as_deref(), Some("vim"));
    }

    /// Two screens, the keyboard on the first, and a workspace on each plus one more that
    /// is open but not on screen.
    fn two_screens() -> SwayState {
        let workspace =
            |name: &str, output: &str, focused: bool, visible: bool| crate::sway::Workspace {
                name: name.to_string(),
                output: output.to_string(),
                focused,
                visible,
                urgent: false,
            };
        SwayState {
            workspaces: vec![
                workspace("1", "DP-1", true, true),
                workspace("2", "HDMI-A-1", false, true),
                workspace("3", "DP-1", false, false),
            ],
            windows: [("DP-1", "vim", "foot"), ("HDMI-A-1", "a page", "firefox")]
                .into_iter()
                .map(|(output, title, app_id)| {
                    (
                        output.to_string(),
                        crate::sway::Window {
                            title: title.to_string(),
                            app_id: app_id.to_string(),
                            class: String::new(),
                        },
                    )
                })
                .collect(),
            focused_output: Some("DP-1".to_string()),
            ..SwayState::default()
        }
    }

    /// A window module can be written against what the window is, not only what it is
    /// showing: a rule that gives one program its own colour must not key on a title that
    /// changes with every tab.
    #[test]
    fn a_window_module_can_name_what_the_window_is() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["title"]

[module.title]
source = "sway:window"
format = "$app_id|$class|'?': $title"
"##;
        let cfg = Config::parse(config).expect("test config parses");
        let sway = two_screens();
        let inputs = Inputs {
            items: &[],
            native: &Registry::new(&Default::default()),
            sway: &sway,
            alt: &Default::default(),
            pages: &Default::default(),
            collapsed: &Default::default(),
            waiting: &Default::default(),
            spin: 0,
            tray: &Default::default(),
            output: Some("HDMI-A-1"),
        };
        let frame = compute(&cfg, &inputs, 200.0, 10.0, &mut Fixed, None);
        assert_eq!(frame.groups[0].modules[0].text, "firefox: a page");
    }

    /// One module, one icon per application - the same expansion a workspace list gets.
    #[test]
    fn a_tray_module_becomes_one_module_per_application() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["tray"]

[module.tray]
source = "tray"
padding = 0
"##;
        let cfg = Config::parse(config).expect("test config parses");
        let tray = crate::tray::TrayState {
            items: vec![tray_item("a", "One"), tray_item("b", "Two")],
        };
        let frame = render_with(&cfg, &tray);
        assert_eq!(frame.groups[0].modules.len(), 2);
        // Each carries its own artwork and its own click target rather than the module's.
        for module in &frame.groups[0].modules {
            assert!(module.icon.as_ref().is_some_and(|i| i.art.is_some()));
        }
        assert!(matches!(
            frame.groups[0].modules[0].action,
            Some(ActionTarget::Tray { .. })
        ));
    }

    /// A tray item says nothing by default - the picture is the whole module - and the
    /// check that drops a module with no text would otherwise drop every one of them.
    #[test]
    fn an_item_with_no_wording_is_still_drawn() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["tray"]

[module.tray]
source = "tray"
padding = 0
"##;
        let cfg = Config::parse(config).expect("test config parses");
        let tray = crate::tray::TrayState {
            items: vec![tray_item("a", "One")],
        };
        let frame = render_with(&cfg, &tray);
        assert_eq!(frame.groups[0].modules[0].text, "");
        assert!(frame.groups[0].modules[0].icon.is_some());
    }

    /// An item that brought no icon at all and has nothing written on it is nothing to
    /// draw, and a rectangle of empty bar is worse than no rectangle.
    #[test]
    fn an_item_with_neither_icon_nor_wording_is_left_out() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["tray"]

[module.tray]
source = "tray"
padding = 0
"##;
        let cfg = Config::parse(config).expect("test config parses");
        let mut item = tray_item("a", "One");
        item.icon = None;
        let tray = crate::tray::TrayState { items: vec![item] };
        assert!(render_with(&cfg, &tray).groups.is_empty());
    }

    /// Items arrive as their applications start, so a bar that has been up since morning
    /// is in one order and the same bar restarted at lunch is in another. Naming the ones
    /// that matter pins them; the rest keep arriving after them.
    #[test]
    fn a_tray_can_be_told_what_to_show_and_in_what_order() {
        let config = |body: &str| {
            format!(
                r##"
[left]
groups = ["g"]

[group.g]
modules = ["tray"]

[module.tray]
source = "tray"
format = "$id"
padding = 0
{body}
"##
            )
        };
        let mut quiet = tray_item("k3", "Quiet");
        quiet.status = crate::tray::Status::Passive;
        let tray = crate::tray::TrayState {
            items: vec![tray_item("k1", "Volume"), tray_item("k2", "Network"), quiet],
        };
        let drawn = |body: &str| {
            let cfg = Config::parse(&config(body)).expect("test config parses");
            render_with(&cfg, &tray)
                .groups
                .first()
                .map(|g| g.modules.iter().map(|m| m.text.clone()).collect::<Vec<_>>())
                .unwrap_or_default()
        };

        // As they arrived, passive ones included, which is what a bar without either key
        // has always drawn.
        assert_eq!(drawn(""), ["volume", "network", "quiet"]);
        assert_eq!(drawn("show_passive = false"), ["volume", "network"]);
        // Named first, in the order named; everything else after, in arrival order.
        assert_eq!(
            drawn("order = [\"network\"]"),
            ["network", "volume", "quiet"]
        );
        // An id nothing in the tray has costs nothing.
        assert_eq!(
            drawn("order = [\"nothing\", \"quiet\"]"),
            ["quiet", "volume", "network"]
        );
    }

    fn tray_item(key: &str, title: &str) -> crate::tray::Item {
        crate::tray::Item {
            key: key.to_string(),
            is_menu: false,
            has_menu: false,
            id: title.to_lowercase(),
            title: title.to_string(),
            status: crate::tray::Status::Active,
            icon: Some(Arc::new(crate::icon::Raster {
                width: 1,
                height: 1,
                pixels: vec![255, 255, 255, 255],
            })),
        }
    }

    fn render_with(cfg: &Config, tray: &crate::tray::TrayState) -> Frame {
        let inputs = Inputs {
            items: &[],
            native: &Registry::new(&Default::default()),
            sway: &SwayState::default(),
            alt: &Default::default(),
            pages: &Default::default(),
            collapsed: &Default::default(),
            waiting: &Default::default(),
            spin: 0,
            tray,
            output: None,
        };
        compute(cfg, &inputs, 200.0, 10.0, &mut Fixed, None)
    }

    fn menu_row(label: &str) -> crate::tray::menu::Row {
        crate::tray::menu::Row {
            id: 1,
            label: label.to_string(),
            enabled: true,
            ..Default::default()
        }
    }

    fn menu_style() -> crate::config::Menu {
        Config::parse("[bar]\nheight = 20\n")
            .expect("a bar with no modules is a config")
            .menu
    }

    /// A menu row's label comes from the application, and the popup has a width the
    /// config decided. A label longer than that used to be shaped whole and then drawn
    /// straight off the edge.
    #[test]
    fn a_menu_label_longer_than_the_menu_is_cut_to_fit() {
        let style = menu_style();
        let rows = vec![menu_row(&"label ".repeat(4096))];
        let frame = menu(&rows, &style, 16.0, 10.0, None, &mut Fixed);
        assert!(
            frame.width <= style.max_width,
            "the menu is {} wide against a limit of {}",
            frame.width,
            style.max_width
        );
        let drawn = &frame.rows[0].text;
        assert!(drawn.ends_with(ELLIPSIS), "{drawn:?} was not cut");
        assert!(
            (drawn.chars().count() as f32) < style.max_width,
            "the label is still {} characters against a menu {} wide",
            drawn.chars().count(),
            style.max_width
        );
    }

    /// A menu is as wide as the longest thing it has to say, and every row is the same
    /// height so the pointer does not have to hunt for them.
    #[test]
    fn a_menu_is_sized_by_what_it_has_to_say() {
        let rows = vec![menu_row("Short"), menu_row("A much longer label")];
        let frame = menu(&rows, &menu_style(), 16.0, 10.0, None, &mut Fixed);
        // The stub measurer makes every character one unit wide.
        assert!(frame.width > "A much longer label".len() as f32);
        assert_eq!(frame.rows.len(), 2);
        assert_eq!(frame.rows[0].height, frame.rows[1].height);
        // Rows follow one another with no gap, and the whole is as tall as they are plus
        // the padding at each end.
        assert_eq!(frame.rows[1].y, frame.rows[0].y + frame.rows[0].height);
        assert!(frame.height > frame.rows[1].y + frame.rows[1].height);
    }

    /// A rule is shorter than a row and cannot be pointed at: hunting for a row and
    /// landing on the line between two of them is how a menu feels broken.
    #[test]
    fn a_separator_is_not_something_the_pointer_can_land_on() {
        let rows = vec![
            menu_row("One"),
            crate::tray::menu::Row {
                id: 2,
                separator: true,
                ..Default::default()
            },
            menu_row("Two"),
        ];
        let frame = menu(&rows, &menu_style(), 16.0, 10.0, None, &mut Fixed);
        assert!(frame.rows[1].height < frame.rows[0].height);

        let middle = |index: usize| frame.rows[index].y + frame.rows[index].height / 2.0;
        assert_eq!(frame.row_at(middle(0)), Some(0));
        assert_eq!(frame.row_at(middle(1)), None, "a rule is not a row");
        assert_eq!(frame.row_at(middle(2)), Some(2));
    }

    /// The highlight follows the pointer, and never onto a row that does nothing.
    #[test]
    fn only_a_row_that_can_be_chosen_is_highlighted() {
        let rows = vec![
            menu_row("Enabled"),
            crate::tray::menu::Row {
                id: 2,
                label: "Disabled".to_string(),
                enabled: false,
                ..Default::default()
            },
        ];
        let style = menu_style();
        let frame = menu(&rows, &style, 16.0, 10.0, Some(0), &mut Fixed);
        assert!(frame.rows[0].highlight);
        assert!(!frame.rows[1].highlight);

        // Pointing at the disabled row highlights nothing, and it keeps its quieter ink.
        let frame = menu(&rows, &style, 16.0, 10.0, Some(1), &mut Fixed);
        assert!(!frame.rows[0].highlight);
        assert!(!frame.rows[1].highlight);
        assert_eq!(frame.rows[1].foreground, style.disabled);
    }

    /// Both columns exist on every row so the labels line up, whether or not each row has
    /// something to put in them.
    #[test]
    fn a_mark_and_an_arrow_are_placed_only_where_they_belong() {
        let rows = vec![
            crate::tray::menu::Row {
                id: 1,
                label: "Ticked".to_string(),
                enabled: true,
                toggle: Some(true),
                ..Default::default()
            },
            crate::tray::menu::Row {
                id: 2,
                label: "Unticked".to_string(),
                enabled: true,
                toggle: Some(false),
                ..Default::default()
            },
            crate::tray::menu::Row {
                id: 3,
                label: "More".to_string(),
                enabled: true,
                submenu: true,
                ..Default::default()
            },
        ];
        let frame = menu(&rows, &menu_style(), 16.0, 10.0, None, &mut Fixed);
        assert!(frame.rows[0].mark.is_some());
        assert!(
            frame.rows[1].mark.is_none(),
            "an unticked row wears no tick"
        );
        assert!(frame.rows[2].arrow.is_some());
        assert!(frame.rows[0].arrow.is_none());
        // The labels start in the same place regardless.
        assert_eq!(frame.rows[0].text_x, frame.rows[2].text_x);
    }

    /// What a module is asked to draw is somebody else's: a window title, a line from a
    /// script. Finding the few characters that fit must not cost what the sender sent, in
    /// shaping or in what the cache then holds on to.
    #[test]
    fn a_very_long_line_is_not_measured_end_to_end() {
        /// Records the longest string it was ever asked about.
        struct Longest(usize);
        impl Measure for Longest {
            fn measure(&mut self, text: &str) -> f32 {
                self.0 = self.0.max(text.chars().count());
                text.chars().count() as f32
            }
        }

        let long = "x".repeat(256 * 1024);
        let mut measure = Longest(0);
        let shown = truncate(&long, 20.0, &mut measure);
        assert_eq!(shown.chars().count(), 20, "19 characters and an ellipsis");
        assert!(
            measure.0 <= 128,
            "measured a string of {} characters to draw 20 of them",
            measure.0
        );

        // Text that takes up no room at all never overflows a budget, so the growing
        // window would grow to whatever arrived. Zero-width spaces are the plain case,
        // and a joined emoji sequence is the one that turns up by accident.
        struct Weightless(usize);
        impl Measure for Weightless {
            fn measure(&mut self, text: &str) -> f32 {
                self.0 = self.0.max(text.chars().count());
                0.0
            }
        }
        let empty_looking = "\u{200b}".repeat(64 * 1024);
        let mut measure = Weightless(0);
        truncate(&empty_looking, 20.0, &mut measure);
        assert!(
            measure.0 <= MOST_SHAPED,
            "shaped {} characters of text that is no width at all",
            measure.0
        );

        // A line that fits is still handed back whole, however close to the budget it is.
        let mut measure = Longest(0);
        assert_eq!(truncate("short", 20.0, &mut measure), "short");
        assert_eq!(
            truncate("exactly twenty chars", 20.0, &mut measure),
            "exactly twenty chars"
        );
    }

    /// The bar keeps its ground while it is saying what went wrong. The renderer paints
    /// what the frame carries and nothing else, so a fault frame that carried no ground
    /// put its message straight onto the wallpaper.
    #[test]
    fn a_fault_keeps_the_bar_it_is_drawn_on() {
        let cfg = Config::parse(
            "[colors]\nink = \"#1e1e2e\"\n\n[bar]\nheight = 20\n\n\
             [bar.background]\ncolor = \"$ink\"\nradius = 8\n",
        )
        .expect("a bar with a ground");
        let frame = fault(&cfg, "the provider stopped", 200.0, 20.0, &mut Fixed);
        assert_eq!(frame.background, cfg.bar.background);
        assert_eq!(frame.radius, cfg.bar.radius);
        assert_eq!(frame.groups[0].modules[0].text, "the provider stopped");

        // A bar configured with no ground of its own still has none here.
        let plain = Config::parse("[bar]\nheight = 20\n").expect("a bar with nothing on it");
        let frame = fault(&plain, "the provider stopped", 200.0, 20.0, &mut Fixed);
        assert_eq!(frame.background, Color::TRANSPARENT);
    }

    /// The first frame has nothing on screen to compare against, so all of it is new.
    #[test]
    fn the_first_frame_damages_everything() {
        let frame = frame_of(BASIC, &[item("cpu", "1%"), item("mem", "2%")]);
        assert_eq!(frame.damage(&Frame::default()), Damage::All);
    }

    /// A frame that draws the same thing damages nothing. This is the case that matters:
    /// a collector that read the same value again must not make the compositor take the
    /// bar back.
    #[test]
    fn a_frame_that_changed_nothing_damages_nothing() {
        let items = [item("cpu", "1%"), item("mem", "2%")];
        let one = frame_of(BASIC, &items);
        let two = frame_of(BASIC, &items);
        assert_eq!(two.damage(&one), Damage::Rects(Vec::new()));
    }

    /// The same layout on a different surface is not the same picture: the buffer is a new
    /// size, or the same size at another scale, and none of it has been painted. Comparing
    /// the layouts alone would attach a buffer and tell the compositor to look at none of
    /// it, which is a bar that stops updating until something moves.
    #[test]
    fn a_frame_on_a_surface_that_changed_damages_everything() {
        let items = [item("cpu", "1%"), item("mem", "2%")];
        let one = frame_of(BASIC, &items);
        let two = frame_of(BASIC, &items);
        let surface = (1920, 30, 1);

        assert_eq!(
            two.damage_since(&one, Some(surface), surface),
            Damage::Rects(Vec::new()),
            "the same frame on the same surface is still nothing to repaint"
        );
        // A monitor whose scale changed lays the bar out identically and shares not one
        // pixel with what was there.
        assert_eq!(
            two.damage_since(&one, Some(surface), (1920, 30, 2)),
            Damage::All
        );
        assert_eq!(
            two.damage_since(&one, Some(surface), (3840, 30, 1)),
            Damage::All
        );
        // Nothing has reached the screen yet: the first frame, and every frame after one
        // that failed on its way there.
        assert_eq!(two.damage_since(&one, None, surface), Damage::All);
    }

    /// A module whose wording changed damages the island holding it, and nothing else.
    #[test]
    fn a_changed_module_damages_its_island() {
        let before = frame_of(BASIC, &[item("cpu", "1%"), item("mem", "2%")]);
        let after = frame_of(BASIC, &[item("cpu", "99%"), item("mem", "2%")]);
        let Damage::Rects(rects) = after.damage(&before) else {
            panic!("a wording change is not the whole bar");
        };
        assert!(!rects.is_empty(), "something did change");
        // Every rectangle is one of the group's, before or after.
        let group = &after.groups[0];
        for (x, _, w, _) in &rects {
            assert!(
                *x >= group.x - 1.0 && x + w <= group.x + group.width + 1.0,
                "damage {x}+{w} reaches outside the island at {}+{}",
                group.x,
                group.width
            );
        }
    }

    /// A module that grows moves everything after it, and what moved has to be repaired
    /// where it used to be as well as drawn where it is now.
    #[test]
    fn a_module_that_grows_damages_what_it_pushed_along() {
        let config = r##"
[left]
groups = ["one", "two"]

[group.one]
modules = ["cpu"]

[group.two]
modules = ["mem"]

[module.cpu]
padding = 0

[module.mem]
padding = 0
"##;
        let narrow = frame_of(config, &[item("cpu", "1%"), item("mem", "2%")]);
        let wide = frame_of(config, &[item("cpu", "1000000%"), item("mem", "2%")]);
        let Damage::Rects(rects) = wide.damage(&narrow) else {
            panic!("two islands are not the whole bar");
        };
        // The island that grew and the one it shoved along, each named twice - where it
        // was and where it is.
        assert_eq!(rects.len(), 4, "both islands, before and after: {rects:?}");
        let widest = rects.iter().map(|(_, _, w, _)| *w).fold(0.0f32, f32::max);
        assert!(
            widest >= wide.groups[0].width,
            "the grown island is covered"
        );
    }

    /// Colour is drawn, so a state rule that recolours a module without changing a letter
    /// of it still has to be reported.
    #[test]
    fn a_recoloured_module_is_damaged_even_with_the_same_words() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
padding = 0

[module.cpu.states.hot]
above = 50
background = "#ff0000"
"##;
        let cool = frame_of(config, &[with_percent(item("cpu", "10%"), 10.0)]);
        let hot = frame_of(config, &[with_percent(item("cpu", "90%"), 90.0)]);
        let Damage::Rects(rects) = hot.damage(&cool) else {
            panic!("one module is not the whole bar");
        };
        assert!(!rects.is_empty(), "a colour change is a change");
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
                tray: &Default::default(),
                output: None,
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
            timeout: std::time::Duration::from_secs(30),
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
            timeout: std::time::Duration::from_secs(30),
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
            timeout: std::time::Duration::from_secs(30),
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
        let icon = module.icon.as_ref().expect("the icon is what is left");
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
            on.groups[0].modules[0].icon.as_ref().unwrap().icon,
            Icon::BatteryCharging
        );

        let off = frame_with(config, &[], charging("discharging"));
        assert_eq!(
            off.groups[0].modules[0].icon.as_ref().unwrap().icon,
            Icon::Battery
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
                frame.groups[0].modules[0].icon.as_ref().unwrap().level,
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
    const JOINED: &str = r##"
[bar]
height = 20
gap = 17
[right]
groups = ["a", "b", "c"]
[right.separator]
shape = "slant"
width = 5
overlap = 1
[group.a]
modules = ["a"]
radius = 4
ends = { left = "slant", right = "slant", width = 2 }
[group.b]
modules = ["b"]
radius = 4
ends = { left = "slant", right = "slant", width = 2 }
[group.c]
modules = ["c"]
radius = 4
ends = { left = "slant", right = "slant", width = 2 }
[module.a]
format = "$text"
padding = 0
background = "#aa0000"
[module.b]
format = "$text"
padding = 0
background = "#00aa00"
[module.b.states.hover]
hover = true
background = "#ffff00"
[module.c]
format = "$text"
padding = 0
background = "#0000aa"
"##;

    fn joined_frame(
        config: &str,
        items: &[StatusItem],
        width: f32,
        pointer: Option<(f32, f32)>,
    ) -> Frame {
        let cfg = Config::parse(config).unwrap();
        let inputs = Inputs {
            items,
            native: &Registry::new(&Default::default()),
            sway: &SwayState::default(),
            alt: &Default::default(),
            pages: &Default::default(),
            collapsed: &Default::default(),
            waiting: &Default::default(),
            spin: 0,
            tray: &Default::default(),
            output: None,
        };
        compute(&cfg, &inputs, width, 20.0, &mut Fixed, pointer)
    }

    #[test]
    fn joined_groups_skip_empty_neighbors_and_keep_only_outer_caps() {
        let items = [item("a", "aaa"), item("b", ""), item("c", "ccc")];
        let frame = joined_frame(JOINED, &items, 100.0, None);
        assert_eq!(frame.groups.len(), 2);
        assert_eq!(frame.group_separators.len(), 1);
        let (a, c) = (&frame.groups[0], &frame.groups[1]);
        assert_eq!((a.width, c.width), (5.0, 5.0)); // three letters, one outer cap
        assert_eq!(c.x - (a.x + a.width), 5.0); // join, not bar.gap or facing caps
        assert_eq!(
            (a.edges.left, a.edges.right),
            (EdgeShape::Round, EdgeShape::None)
        );
        assert_eq!(
            (c.edges.left, c.edges.right),
            (EdgeShape::None, EdgeShape::Round)
        );
        assert_eq!((a.separators.len(), c.separators.len()), (1, 1));
        let join = &frame.group_separators[0];
        assert_eq!(join.fill, a.modules[0].background);
        assert_eq!(join.under, c.modules[0].background);
        assert!(frame.module_at(join.x + 2.0, 10.0).is_none());
        let single = joined_frame(JOINED, &[item("b", "bbb")], 100.0, None);
        assert!(single.group_separators.is_empty());
        assert_eq!(single.groups[0].width, 7.0);
        assert_eq!(single.groups[0].separators.len(), 2);
        let empty = joined_frame(JOINED, &[], 100.0, None);
        assert!(empty.groups.is_empty() && empty.group_separators.is_empty());
    }

    #[test]
    fn joined_groups_fit_caps_and_internal_separators_in_the_width_budget() {
        let config = JOINED.replace(
            "modules = [\"a\"]",
            "modules = [\"a\", \"b\"]\nseparator = { shape = 'slant', width = 9 }",
        );
        let items = [
            item("a", "aaaaaaaaaa"),
            item("b", "bbbbbbbbbb"),
            item("c", "cccccccccc"),
        ];
        for width in 1..90 {
            let frame = joined_frame(&config, &items, width as f32, None);
            for group in &frame.groups {
                assert!(
                    group.x >= 0.0 && group.x + group.width <= width as f32,
                    "width {width}: {group:?}"
                );
                for m in &group.modules {
                    assert!(m.x >= group.x && m.x + m.width <= group.x + group.width);
                }
            }
            assert_eq!(
                frame.group_separators.len(),
                frame.groups.len().saturating_sub(1)
            );
        }
    }

    #[test]
    fn joins_use_hover_and_source_colors_and_damage_both_old_and_new_bounds() {
        let mut items = [item("a", "aaa"), item("b", "bbb"), item("c", "ccc")];
        let old = joined_frame(JOINED, &items, 100.0, None);
        let b = &old.groups[1].modules[0];
        let hovered = joined_frame(JOINED, &items, 100.0, Some((b.x + 1.0, 10.0)));
        let yellow = Color::parse("#ffff00").unwrap();
        assert_eq!(hovered.group_separators[0].under, yellow);
        assert_eq!(hovered.group_separators[1].fill, yellow);
        let Damage::Rects(rects) = hovered.damage(&old) else {
            panic!("hover keeps geometry");
        };
        for join in &hovered.group_separators {
            assert!(rects.contains(&(join.x - 1.0, 0.0, 7.0, 20.0)));
        }
        items[1].background = Some(Color::parse("#ff7700").unwrap());
        let changed = joined_frame(JOINED, &items, 100.0, None);
        assert_eq!(
            changed.group_separators[0].under,
            items[1].background.unwrap()
        );
        assert_eq!(
            changed.group_separators[1].fill,
            items[1].background.unwrap()
        );
        assert!(matches!(changed.damage(&changed),Damage::Rects(r) if r.is_empty()));
        let resized = joined_frame(JOINED, &items, 110.0, None);
        let Damage::Rects(rects) = resized.damage(&changed) else {
            panic!("same groups");
        };
        for join in changed
            .group_separators
            .iter()
            .chain(&resized.group_separators)
        {
            assert!(rects.contains(&(join.x - 1.0, 0.0, 7.0, 20.0)));
        }
        assert!(matches!(
            joined_frame(JOINED, &items[..1], 100.0, None).damage(&old),
            Damage::All
        ));
    }

    #[test]
    fn absent_or_disabled_group_separator_preserves_independent_layout() {
        let disabled = JOINED.replace(
            "shape = \"slant\"\nwidth = 5",
            "shape = \"none\"\nwidth = 5",
        );
        let omitted = disabled.replace(
            "[right.separator]\nshape = \"none\"\nwidth = 5\noverlap = 1\n",
            "",
        );
        let items = [item("a", "aaa"), item("b", "bbb"), item("c", "ccc")];
        let a = joined_frame(&disabled, &items, 100.0, None);
        let b = joined_frame(&omitted, &items, 100.0, None);
        assert!(a.group_separators.is_empty());
        assert!(matches!(a.damage(&b),Damage::Rects(r) if r.is_empty()));
        assert_eq!(a.groups[1].x - a.groups[0].x - a.groups[0].width, 17.0);
    }
    #[test]
    fn outer_caps_can_face_left_without_reversing_group_joins() {
        let config = JOINED.replacen(
            "ends = { left = \"slant\", right = \"slant\", width = 2 }",
            "ends = { left = \"slant\", right = \"slant\", width = 2, direction = \"left\" }",
            1,
        );
        let items = [item("a", "aaa"), item("b", "bbb")];
        let frame = joined_frame(&config, &items, 100.0, None);
        let leading = &frame.groups[0].separators[0];
        assert_eq!(leading.direction, Direction::Left);
        assert_eq!(leading.fill, Color::TRANSPARENT);
        assert_eq!(leading.under, frame.groups[0].modules[0].background);
        assert_eq!(frame.group_separators[0].direction, Direction::Right);
        // The next group's outer cap still inherits its own separator direction.
        assert_eq!(frame.groups[1].separators[0].direction, Direction::Right);
        let original = joined_frame(JOINED, &items, 100.0, None);
        assert_eq!(original.groups[0].separators[0].direction, Direction::Right);
    }
}
