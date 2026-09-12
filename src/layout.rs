//! Turns config plus the current status items into positioned rectangles.
//!
//! The result is purely geometric: the renderer draws it and the pointer code hit-tests it,
//! neither needs to know about config or where the items came from.

use crate::collect::{Registry, Which};
use crate::color::Color;
use crate::config::{
    Config, Ends, Group as GroupCfg, IconSpec, Module as ModuleCfg, Scope, Separator,
    SeparatorColor, Source, StateFlags, Style,
};
use crate::format::Format;
use crate::geometry::{Direction, EdgeShape, Edges, SeparatorShape};
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
    /// Groups folded down to one icon, shared across outputs.
    pub collapsed_groups: &'a std::collections::HashSet<String>,
    /// Modules folded down to their icon, preserved while their group is hidden.
    pub collapsed: &'a std::collections::HashSet<String>,
    /// Modules on their way from one wording to the next, by name.
    ///
    /// A module travelling is laid out in the wording it is going to and cut off at its
    /// own edge, which is the part that moves. One that has arrived is not in here at all,
    /// and `alt` says which wording it arrived at.
    pub switching: &'a std::collections::HashMap<String, Leaving>,
    /// Groups on their way between open and shut, by name: 0.0 open, 1.0 shut.
    ///
    /// A group travelling is laid out open and cut off at its own edge, which is the part
    /// that moves. One that has arrived is not in here at all, and `collapsed_groups` says
    /// which end it arrived at.
    pub folding: &'a std::collections::HashMap<String, f32>,
    /// How far each module's own fold has got, on the same terms as `folding`.
    ///
    /// Kept apart from the islands': a module and the group holding it can be folding at
    /// once, and they are two travels with two ends, not one shared number.
    pub module_folding: &'a std::collections::HashMap<String, f32>,
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

/// A module part way between two of its wordings.
#[derive(Clone, Copy, Debug)]
pub struct Leaving {
    /// The wording it is coming from, numbered the way `alt` numbers them.
    pub from: usize,
    /// How far it has got, 0.0 on the frame the click landed and 1.0 as it arrives.
    pub at: f32,
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
    pub text_middle: f32,
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
///
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
            text_middle: y + height / 2.0,
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
    /// Where the module's contents stop, for one holding more than it shows.
    ///
    /// A module travelling between two wordings is drawn at a width neither was fitted to,
    /// so both are cut short of the box by the fitted padding. A module fold instead cuts
    /// marks at the travelling box itself; its wording has the separate inset below.
    /// `None` on every module that has arrived, which is all of them but the one under a
    /// click.
    pub content_right: Option<f32>,
    /// Where the module's wording stops, when that is short of the rest of its contents.
    ///
    /// A wording travel uses the same edge for both. A fold retires the wording before
    /// the icon, leaving the collapsed style's empty padding around that icon by the time
    /// it arrives. `None` on every settled module.
    pub text_right: Option<f32>,
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
    /// Whether this is an island's own end cap, drawn in room of its own beside the
    /// modules, rather than a divider drawn in the gap between two of them.
    ///
    /// The two are clipped differently while a fold is travelling: a divider belongs to
    /// the contents and is cut where they are, and a cap belongs to the island and is
    /// placed at an edge the contents never reach.
    pub cap: bool,
    /// Colour of the region on the leading side of the boundary.
    pub fill: Color,
    /// Colour behind it, on the trailing side.
    pub under: Color,
}

#[derive(Clone, Debug)]
pub struct PlacedGroup {
    /// Stable group name and the button reserved throughout its bounds.
    pub collapse: Option<(String, Button)>,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub background: Color,
    /// How much of the finished island reaches the screen, 0.0 to 1.0.
    pub opacity: f32,
    pub edges: Edges,
    /// Where the island's contents stop, for one holding more than it shows.
    ///
    /// Not the island's own edge: a trailing cap is drawn in room of its own beyond this,
    /// the way it is when nothing is folding, so the modules have to be cut before it or
    /// they would fill the transparent side of it in. `None` on every group that was
    /// measured to fit, which is all of them but the one a fold is travelling over -
    /// clipping costs a mask and a slower blend for every fill drawn through it.
    pub content_right: Option<f32>,
    /// Where the island's wording stops, which is not where its fills stop.
    ///
    /// A collapsed island is an icon with the collapsed style's padding around it, and
    /// nothing is written in padding. Cutting text at `content_right` would leave the
    /// first characters standing in that padding right up to the last frame of a fold and
    /// take them away in one step, so text is cut short of the fills by however much empty
    /// room the island is closing on. `None` wherever `content_right` is.
    pub text_right: Option<f32>,
    /// The transition drawn where the contents stop, for an island a fold is closing.
    ///
    /// Fills stop at `content_right` and this covers them, so they never needed it. Glyphs
    /// and icons are cut rather than covered, and cutting them on a column inside a slanted
    /// end is the one straight line left on an angled ribbon. This is the shape to cut them
    /// with instead - the very separator drawn there, whether it is this island's own cap
    /// or the join it shares with the next one. `None` wherever `content_right` is.
    pub content_edge: Option<PlacedSeparator>,
    pub modules: Vec<PlacedModule>,
    pub separators: Vec<PlacedSeparator>,
}

impl PlacedGroup {
    /// The module still visible immediately before the trailing cap or join.
    fn trailing_module(&self) -> Option<&PlacedModule> {
        module_before(&self.modules, self.content_right)
    }

    /// Every logical pixel the island can reach, which is not its rectangle.
    ///
    /// A separator drawn at a group's end bleeds `overlap` past each side of itself to hide
    /// the seam between two antialiased edges, and a cap sits at the very edge of the
    /// island, so the bleed lands outside it whenever the overlap is wider than the group's
    /// padding. Rounded edges clip that away; square ones have nothing to clip with, and
    /// then the pixels are really there. Both the damage a frame reports and the layer a
    /// translucent island is composited from are wrong if they use the rectangle instead.
    pub fn paint_bounds(&self) -> (f32, f32, f32, f32) {
        let (mut x0, mut x1) = (self.x, self.x + self.width);
        for separator in &self.separators {
            if separator.shape.is_none() {
                continue;
            }
            x0 = x0.min(separator.x - separator.overlap);
            x1 = x1.max(separator.x + separator.width + separator.overlap);
        }
        // A folding island holds more than it shows. What reaches the screen stops at its
        // edge, but the mask that stops it has to be cleared over everything drawn through
        // it, and the layer a translucent one is composited from has to be big enough to
        // take it, so both are asked about the content rather than the island.
        if self.content_right.is_some()
            && let (Some(first), Some(last)) = (self.modules.first(), self.modules.last())
        {
            x0 = x0.min(first.x);
            x1 = x1.max(last.x + last.width);
        }
        // The edge the contents are cut with reaches past them by its own width, and a join
        // belongs to the frame rather than to either island, so this is the one separator
        // the loop above cannot have seen.
        if let Some(edge) = &self.content_edge {
            x0 = x0.min(edge.x - edge.overlap);
            x1 = x1.max(edge.x + edge.width + edge.overlap);
        }
        (x0, self.y, x1 - x0, self.height)
    }
}

/// The module whose ground the island's trailing end lands on.
///
/// A module's ground starts where the one before it stops, not where its own box does: the
/// gap between them is painted across its whole width in the colour arriving on the far
/// side, and the shape drawn over that only covers part of it. Asking for the box instead
/// hands the transition the colour of the module behind while the edge is still standing on
/// the one in front, and the fold swaps the two the moment it crosses the box - which is a
/// colour arriving and leaving in one step on a bar that is otherwise travelling.
fn module_before(modules: &[PlacedModule], edge: Option<f32>) -> Option<&PlacedModule> {
    let ground = |i: usize| match i {
        0 => modules[0].x,
        i => modules[i - 1].x + modules[i - 1].width,
    };
    (0..modules.len())
        .rev()
        .find(|&i| edge.is_none_or(|right| ground(i) < right))
        .map(|i| &modules[i])
        .or_else(|| modules.first())
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
            && self.content_right == other.content_right
            && self.text_right == other.text_right
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
            && self.cap == other.cap
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
            && self.content_right == other.content_right
            && self.text_right == other.text_right
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
            // well as cover where it is now. What each of them painted, rather than what
            // each of them measured - an end cap reaches past the island it belongs to.
            rects.push(old.paint_bounds());
            rects.push(new.paint_bounds());
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

/// A group reserves its button before any child can act on it.
pub enum ClickTarget<'a> {
    Group(&'a str),
    Module(&'a PlacedModule),
}

impl Frame {
    pub fn click_at(&self, x: f32, y: f32, button: u32) -> Option<ClickTarget<'_>> {
        for group in &self.groups {
            if contains(x, y, group.x, group.y, group.width, group.height)
                && let Some((name, reserved)) = &group.collapse
                && button == reserved.number()
            {
                return Some(ClickTarget::Group(name));
            }
        }
        self.module_at(x, y).map(ClickTarget::Module)
    }

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
    collapse: Option<(String, Button)>,
    width: f32,
    /// Whether what is inside reaches past the island and has to be cut off at its edge.
    clipped: bool,
    /// How far the fold has got, 0.0 to 1.0, and 0.0 for an island that is not folding.
    ///
    /// The cut needs it. Everything else a fold moves was worked out once, here, and the
    /// island carries the answer; the shape its contents are cut with is the one thing that
    /// has to know where in the travel it is, because it has to have stopped cutting by the
    /// time the island lands.
    travel: f32,
    /// How far short of that edge the wording stops, for a fold on its way. The empty room
    /// the collapsed island keeps around its icon, which no text ever reaches.
    text_inset: f32,
    /// How far the leading module's icon and wording are moved inside their own box, for a
    /// fold on its way. Zero for either settled shape: only a fold has two layouts to
    /// reconcile, and what it reconciles is where the icon sits.
    shift: f32,
    /// What the leading module's box travels to, for a fold on its way.
    ///
    /// It is the one the island is left holding, so it takes the shut island's width and
    /// the rest of the contents pack along behind it. That is what carries them off the
    /// end: an island closing on a wide collapsed padding would otherwise still be showing
    /// its second module on the frame it arrives.
    leading: Option<f32>,
    /// What this island charges the run it is in, which is `width` for everything but a
    /// fold on its way. A travelling island is charged the widest it will ever be, so the
    /// groups after it are measured against one budget from end to end of the travel.
    reserve: f32,
    background: Color,
    opacity: f32,
    edges: Edges,
    /// Vertical padding is settled: wording travel changes width, not height.
    padding: f32,
    /// Horizontal padding travels when every module in the island appears or disappears.
    horizontal_padding: f32,
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
    wording_at(module, alt.get(&module.name).copied().unwrap_or(0))
}

/// One numbered wording, which is what a module travelling has two of.
fn wording_at(module: &ModuleCfg, showing: usize) -> &Format {
    match showing {
        0 => &module.format,
        showing => module.format_alt.get(showing - 1).unwrap_or(&module.format),
    }
}

/// One wording of a module, fitted to the room its run has left.
///
/// Everything here answers to the text: a state rule can key on what the module says, so
/// the style, the icon and the padding are all the wording's own rather than the module's.
/// A module travelling between two of them is fitted twice, and that is the whole reason
/// this is a value rather than a run of locals.
#[derive(Default)]
struct Fitted {
    /// Whether the module is drawn at all.
    ///
    /// A wording that renders empty hides the module, the way the i3bar protocol asks, and
    /// one that strips or truncates away to nothing hides it unless an icon is left to
    /// carry it. Not drawn is a width of nothing rather than an absence, which is what
    /// lets a click on to a wording that says nothing shrink the box away instead of
    /// taking it off the bar between two frames.
    drawn: bool,
    style: Style,
    hover_style: Option<Style>,
    icon: Option<(Icon, usize)>,
    /// A source-owned native icon follows its wording; configured style icons lead it.
    icon_after_text: bool,
    /// What survived stripping and truncation, which is what is actually drawn.
    content: String,
    text_width: f32,
    icon_advance: f32,
    /// Width of a configured glyph leading the wording, or zero for native and
    /// source-owned decorations.
    written_icon_advance: f32,
    width: f32,
}

struct SizedModule {
    /// The width this module's expanded contents were fitted and centred against.
    ///
    /// A wording travel changes this because the two wordings really have two fitted
    /// widths. A fold does not: it keeps the expanded measurement and carries the visible
    /// box separately, the same way a group fold keeps the modules it is closing over.
    width: f32,
    /// The box a module fold is visibly carrying, or `None` when it is settled.
    drawn_width: Option<f32>,
    /// How far the expanded contents have travelled towards the collapsed icon's centre.
    shift: f32,
    /// How far before the travelling box edge wording stops during a module fold.
    text_inset: f32,
    /// What it charges the group, which is `width` for every settled module. A wording or
    /// fold in flight is charged the wider endpoint from end to end, so nothing behind it
    /// is re-measured on the way.
    reserve: f32,
    /// How much of a drawn module exists at this point of a wording travel.
    presence: f32,
    /// Whether the module exists at each endpoint, for reserving the wider topology.
    from_drawn: bool,
    to_drawn: bool,
    /// The visible part of the gap before this module.
    before: f32,
    /// Whether its icon or wording reaches past the box it is being drawn in and has to be
    /// cut off at its own edge.
    clipped: bool,
    text_width: f32,
    /// The module's name, when a gesture on it has to name it.
    name: Option<String>,
    collapsible: bool,
    /// Paint overrides applied while the pointer is over this module.
    hover_style: Option<Style>,
    /// Width of the icon plus its gap, or zero.
    icon_advance: f32,
    /// Width of a configured glyph at the start of `text`.
    written_icon_advance: f32,
    icon: Option<(Icon, usize)>,
    icon_after_text: bool,
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
    /// What the source put beside this item, for a source that decorates its own items.
    /// `None` on every source that leaves the icon to the module's style.
    decoration: Option<Decoration<'g>>,
}

/// What a source puts beside one of its items, in the place a style's icon would go.
///
/// A source that decorates at all decorates every item it publishes, `Nothing` included:
/// letting the style's icon stand in for the items the config passed over would draw it on
/// exactly the workspaces `icons` says nothing about, and put it on the other side of the
/// wording from the ones it does.
#[derive(Clone, Copy)]
enum Decoration<'g> {
    /// Geometry, drawn after the wording in the module's own icon colour.
    Native(Icon),
    /// Text, shaped after the wording as part of it, which is what an emoji or an icon
    /// font's glyph is.
    Text(&'g str),
    Nothing,
}

/// Everything a group shows, in the order the group asks for.
///
/// A module drawn from the compositor expands here: `sway:workspaces` becomes one candidate
/// per workspace, so each is its own rectangle with its own state and click target.
fn collect<'g>(group: &'g GroupCfg, inputs: &Inputs<'_>) -> Vec<Candidate<'g>> {
    #[cfg(test)]
    tests::COLLECTIONS.with(|count| count.set(count.get() + 1));
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
        decoration: None,
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
                            decoration: None,
                        });
                    }
                    continue;
                };
                out.push(Candidate {
                    art: None,
                    decoration: None,
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
                        decoration: None,
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
                        decoration: None,
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
                        decoration: None,
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
                        decoration: None,
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
            Source::SwayWorkspaces(view) => {
                for workspace in &inputs.sway.workspaces {
                    if !inputs.on_this_screen(view.scope, &workspace.output) {
                        continue;
                    }
                    let mut fields = Fields::default();
                    fields.set("name", Value::Text(workspace.name.clone()));
                    // Geometry goes in the icon slot and text is shaped with the wording,
                    // so each kind of workspace icon is placed by the rules for its kind.
                    // A module that names no icons at all decorates nothing and leaves the
                    // slot to its style, the way it did before there were any.
                    let decoration =
                        (!view.icons.is_empty()).then(|| match view.icon(&workspace.name) {
                            Some(Some(IconSpec::Native(icon))) => Decoration::Native(*icon),
                            Some(Some(IconSpec::Text(icon))) if !icon.is_empty() => {
                                Decoration::Text(icon)
                            }
                            _ => Decoration::Nothing,
                        });
                    out.push(Candidate {
                        art: None,
                        decoration,
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
    // The space between two modules: a configured separator owns it, otherwise `spacing`.
    // Named apart from the icon advance below, which is a different distance entirely.
    let between = if group.separator.shape.is_none() {
        group.spacing
    } else {
        group.separator.width
    };
    // What is left for the modules once everything drawn beside them is paid for: the
    // group's padding, and the ends, which are drawn in room of their own rather than over
    // the modules. Reserving them here is what keeps the width worked out at the bottom
    // inside the budget this was given. A run with nothing else beside it gets infinity,
    // and nothing below has to think about it.
    let mut left = budget - group.padding * 2.0 - ends.left_width() - ends.right_width();
    let mut modules = Vec::new();
    // How far the fold has got, for a group that is still moving. A group that is not is
    // one of the two settled shapes below, and pays nothing for this - not even the hash
    // of its own name, which is the same reason `waiting` is asked the same way.
    let folding = match inputs.folding.is_empty() {
        true => None,
        false => inputs.folding.get(&group.name).copied(),
    };
    if let Some(collapse) = &group.collapse
        && inputs.collapsed_groups.contains(&group.name)
        && folding.is_none()
    {
        // Do not even collect candidates: that would format hidden children and copy
        // provider/tray data. The source registry continues updating independently.
        let style = collapse.style.clone();
        let spec = style.icon.as_ref().expect("validated collapsed icon");
        let (icon, shut_text, advance) = collapsed_icon(spec, &style, text);
        // A glyph is wording and is charged to the text; geometry is charged to the icon.
        // Either way the island is that wide plus its padding, which is what a fold lands on.
        let icon_advance = match icon.is_some() {
            true => advance,
            false => 0.0,
        };
        let width = shut_width(&style, advance);
        // Only whether the run has room for it. `max_width` is not asked: a native
        // collapsed icon that could not fit inside it is refused when the config is read,
        // and a glyph is not measurable until then - so the only thing checking here could
        // do is take away the icon that opens the island again.
        if width > left {
            return None;
        }
        modules.push(SizedModule {
            width,
            drawn_width: None,
            shift: 0.0,
            text_inset: 0.0,
            reserve: width,
            presence: 1.0,
            from_drawn: true,
            to_drawn: true,
            before: 0.0,
            clipped: false,
            text_width: advance - icon_advance,
            name: None,
            collapsible: false,
            hover_style: None,
            icon_advance,
            written_icon_advance: 0.0,
            icon,
            icon_after_text: false,
            art: None,
            text: shut_text,
            foreground: style.foreground,
            background: style.background,
            style,
            action: None,
            alt: None,
            alt_button: Button::Left,
            collapse_button: Button::Right,
            refresh: None,
            mute: None,
            paged: None,
            on_click: None,
        });
        return finish_group(group, ends, between, modules);
    }
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
            decoration,
        } = candidate;
        // Geometry stands in the icon's place; text is shaped with the wording. Either way
        // a source that decorates keeps the slot, so `style.icon` is left for folding.
        let (source_icon, suffix) = match decoration {
            Some(Decoration::Native(icon)) => (Some(icon), None),
            Some(Decoration::Text(text)) => (None, Some(text)),
            Some(Decoration::Nothing) | None => (None, None),
        };
        // Whether this module's program is out, which the spinner is drawn for. The check
        // is skipped outright while nothing is waiting, which is nearly always.
        let waiting = !inputs.waiting.is_empty()
            && matches!(&module.source, Source::Native(which) if inputs.waiting.contains(which));
        // The i3bar protocol uses an empty `full_text` to mean "hide this block". A module
        // folded down is empty on purpose and stays, because its icon is still there, and
        // so does one whose spinner is the only thing it has to show.
        let shut = module.collapse.is_some() && inputs.collapsed.contains(&module.name);
        // How far a fold has got, for a module still on its way between its two shapes.
        // Asked the way a group's is: a bar with nothing folding pays nothing for the
        // question, not even the hash of a module's own name.
        let folding = match inputs.module_folding.is_empty() {
            true => None,
            false => inputs.module_folding.get(&module.name).copied(),
        };
        // Settled only once the travel has arrived; until then both shapes are wanted.
        let folded = shut && folding.is_none();
        // Whether a click has this module between two wordings. A module folded down to
        // its icon says the same thing in both wordings, so there is nothing for it to
        // travel between - and one that is folding has its width spoken for already.
        let travelling = match inputs.switching.is_empty() || shut || folding.is_some() {
            true => None,
            false => inputs.switching.get(&module.name).copied(),
        };
        // The state rules and the graded icons both key on what the source published, not
        // on whatever the text ended up saying; a rule reads the field it names, or the
        // value the source is mainly about.
        let resolve = |hovered: bool, text: &str| {
            module
                .states
                .iter()
                .find(|rule| rule.matches(flags, hovered, &values, text))
                .map(|rule| rule.style.clone())
                .unwrap_or_else(|| module.style.clone())
        };
        let value = values.primary().and_then(|v| v.num());

        // A provider often has to spell a state into the text for a rule to match on. Once
        // it has been matched the wording has done its job, and the icon says it better.
        let strip = |content: String| match module
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
        // A module that would outgrow max_width, or the room its run has left, loses text
        // rather than pushing its neighbours aside: a window title has no length limit of
        // its own, and a bar can run out of width whatever the config says.
        // Every module after the first is preceded by whatever goes between two of them,
        // and that space is charged here rather than at the bottom, where the width is
        // only added up.
        let available = match modules.is_empty() {
            true => left,
            false => left - between,
        };
        // Everything one wording costs, fitted to the room the run has left. A closure
        // because the wording a click is leaving behind goes through the whole of it too:
        // a rule can key on the text, so each wording resolves its own style, its own icon
        // and its own padding, and measuring the one being left behind in the style the
        // one arriving happens to wear is a first frame that jumps.
        let fitted = |raw: String, folded: bool, text: &mut dyn Measure| -> Fitted {
            // What a folded module wears, for one that asked to wear something definite.
            // Resolved before the rules so the table wins outright, the way a group's does.
            let collapsed = module.collapse.as_ref().and_then(|c| c.style.as_ref());
            let resolved = resolve(false, &raw);
            // What a fold leaves behind: the icon its `collapsed` table named, and
            // otherwise whatever the module is wearing. The second is the interesting one -
            // it is the matching state's icon, so a player that swaps to a pause icon folds
            // to that rather than to whatever it wore when nothing was playing, and the
            // collapsed table does not have to repeat every state the module has.
            let icon = match (
                folded,
                module.collapse.as_ref().and_then(|c| c.icon.as_ref()),
            ) {
                (true, Some(named)) => Some(named.clone()),
                _ => resolved.icon.clone(),
            };
            let (style, wearing_collapsed) = match (folded, collapsed) {
                (true, Some(worn)) => (worn.clone(), true),
                _ => (resolved, false),
            };
            // A wording that renders empty hides the module, which is what the i3bar
            // protocol means by an empty `full_text`. A module with a picture to show is
            // not empty whatever its wording says: a tray item is its icon, and most of
            // them have nothing written on them at all.
            let hidden = Fitted {
                drawn: false,
                style: style.clone(),
                ..Fitted::default()
            };
            if raw.is_empty()
                && !folded
                && !waiting
                && art.is_none()
                && source_icon.is_none()
                && suffix.is_none()
            {
                return hidden;
            }
            let mut content = strip(raw);
            // Stripping can empty the text entirely, which is fine when an icon is left to
            // carry the module: a muted volume is the icon and nothing else, and so is a
            // command that has not answered yet.
            if content.is_empty()
                && style.icon.is_none()
                && !waiting
                && art.is_none()
                && source_icon.is_none()
                && suffix.is_none()
            {
                return hidden;
            }

            // Hover is deliberately paint-only. Letting it change padding or the icon
            // would resize the module under the pointer, which can move the pointer off it
            // and oscillate, so the metrics always come from the unhovered style.
            //
            // Hover is a state rule, and a written `collapsed` table stands in place of
            // the rules for everything but the icon - so a module wearing one has no hover
            // style, rather than one resolved from a cascade it is not wearing. Comparing
            // the two differs on nearly every key, which handed the open colours to
            // anything under the pointer and took the collapsed ones away.
            let hover_style = match wearing_collapsed {
                true => None,
                false => {
                    let hovered = resolve(true, &content);
                    (hovered != style).then(|| Style {
                        padding: style.padding,
                        min_width: style.min_width,
                        icon_size: style.icon_size,
                        icon: icon.clone(),
                        ..hovered
                    })
                }
            };

            // The rules and the icon read what the module would have said, so folding
            // changes what is drawn without changing what the module is: a paused player
            // keeps its paused styling while it is a single icon.
            if folded {
                content.clear();
            }
            // The decoration a source wrote as text lands here rather than in the wording
            // it follows: a rule reads what the module says, and folding leaves the icon,
            // which for a decoration written as text is the decoration itself.
            if let Some(suffix) = suffix {
                if !content.is_empty() {
                    content.push(' ');
                }
                content.push_str(suffix);
            }

            // A command with a run on its way says so where its icon goes, so the reading
            // that is coming lands in the place the spinner was and nothing else moves. A
            // module with no icon of its own grows one for as long as it is waiting.
            // A picture the source handed over stands in for whatever icon the style
            // names: an application's own artwork is the thing a tray module exists to
            // show.
            // The style's icon splits by what kind it is: geometry takes the icon slot
            // and is graded on the value, a glyph is wording and leads the text the way it
            // would if it had been typed at the front of the format. Both are the icon as
            // far as folding is concerned, which is the point of writing them the same way.
            let styled = || match icon.as_ref() {
                Some(IconSpec::Native(icon)) => {
                    let level = match icon.is_graded() {
                        true => value.map(icon::level_of).unwrap_or(0),
                        false => 0,
                    };
                    (Some((*icon, level)), None)
                }
                Some(IconSpec::Text(glyph)) => (None, Some(glyph.as_ref())),
                None => (None, None),
            };
            let (icon, icon_after_text, lead) = match (waiting, art.is_some(), source_icon) {
                (true, _, _) => (Some((Icon::Spinner, inputs.spin)), false, None),
                (false, true, _) => (Some((Icon::Raster, 0)), false, None),
                (false, false, Some(icon)) => (Some((icon, 0)), true, None),
                (false, false, None) => match (decoration, folded) {
                    // A source that decorates its own items and gave this one nothing gets
                    // nothing: the style's icon would land on exactly the workspaces the
                    // config passed over. Folding is the exception, since it takes the
                    // wording away and the style's icon is then all there is left to click.
                    (Some(Decoration::Nothing), true) | (None, _) => {
                        let (icon, lead) = styled();
                        (icon, false, lead)
                    }
                    (Some(_), _) => (None, false, None),
                },
            };
            let written_icon_advance = lead.map_or(0.0, |lead| text.measure(lead));
            // In front of the wording rather than behind it, which is the one thing that
            // separates a configured icon from a decoration a source wrote for itself.
            // Folding cleared the wording just above, so a folded module is the glyph
            // alone - the whole reason a glyph can be an icon at all.
            if let Some(lead) = lead {
                if !content.is_empty() {
                    content.insert(0, ' ');
                }
                content.insert_str(0, lead);
            }
            // The icon and the space after it, which is what the text starts behind. An
            // icon is as tall as `icon_size` and as wide as its own shape asks for, which
            // is the same thing for everything but the battery.
            //
            // The gap belongs to the text rather than to the icon, so a module with
            // nothing written on it does not get one. Keeping it would pad the far side of
            // a module folded down to its icon and leave the icon sitting half a gap off
            // centre, which is exactly the case the gap was never for.
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
            let fixed = advance(&content) + style.padding * 2.0;
            let cap = match style.max_width > 0.0 {
                true => style.max_width.min(available),
                false => available,
            };
            // A folded module is its icon and nothing besides, and that icon is the only
            // thing left to click to open it again. Cutting it away would take the module
            // and its own way back off the bar together, so a fold keeps what it has
            // whatever `max_width` says: a native icon is measured against `max_width`
            // when the config is read, and a glyph cannot be measured until there are
            // fonts to measure it with.
            let content = match cap.is_finite() && !folded {
                true => truncate(&content, cap - fixed, text),
                false => content,
            };
            // Truncating to nothing takes the gap with it, the same as folding does.
            // Measured again rather than kept, because only the text that survived says
            // whether there is anything for a gap to separate.
            let icon_advance = advance(&content);
            // Truncation can take the last of it, which an icon still carries - a spinner
            // included, since a module waiting on its first answer has nothing else.
            if content.is_empty() && icon.is_none() {
                return hidden;
            }
            let text_width = text.measure(&content);
            let width = (text_width + icon_advance + style.padding * 2.0).max(style.min_width);
            Fitted {
                drawn: true,
                style,
                hover_style,
                icon,
                icon_after_text,
                content,
                text_width,
                icon_advance,
                written_icon_advance,
                width,
            }
        };
        // What the module is going to and what it is coming from. A wording that is not
        // drawn is a width of nothing at either end of a travel rather than a module that
        // is not there: travelled on to, the box shrinks away instead of being taken off
        // between two frames, and travelled from, it grows out of nothing the way it went
        // in.
        //
        // A fold is the same two ends, reached by fitting one wording twice rather than
        // two wordings once. What travels is the module's edge and not what is written
        // inside it, so the open shape is the one drawn and cut for the whole of it - the
        // rule a group fold already follows. Drawing the folded shape instead held the
        // collapsed icon all the way through an opening and jumped to the wording on the
        // last frame.
        let (arriving, leaving) = match folding {
            Some(_) => (
                fitted(content.clone(), false, text),
                Some(fitted(content, true, text)),
            ),
            None => (
                fitted(content, folded, text),
                travelling.map(|leaving| {
                    fitted(
                        wording_at(module, leaving.from).render(&values),
                        folded,
                        text,
                    )
                }),
            ),
        };
        // The same destination description a group fold uses. A native icon owns the
        // icon advance and a written one owns the text width; together they are exactly
        // the mark the collapsed module leaves on the bar.
        let module_shut = folding.zip(leaving.as_ref()).map(|(_, folded)| {
            // Provider colours win over a style in the settled frame, so they also belong
            // to the destination a travelling frame carries. Leaving the raw collapsed
            // style here would replace them for one frame and hand them back on settling.
            let mut style = folded.style.clone();
            style.foreground = foreground.unwrap_or(style.foreground);
            style.background = background.unwrap_or(style.background);
            Shut {
                width: folded.width,
                module: folded.width,
                style,
            }
        });
        // One number for both kinds of travel, since a module is never on two at once.
        // A fold's is turned around: it counts how shut the module is, and the width below
        // eases towards whatever `arriving` holds, which for a fold is the open end.
        let at = folding
            .map(|at| 1.0 - at)
            .or(travelling.map(|leaving| leaving.at));
        if !arriving.drawn && leaving.as_ref().is_none_or(|leaving| !leaving.drawn) {
            continue;
        }
        let from_drawn = leaving
            .as_ref()
            .map_or(arriving.drawn, |leaving| leaving.drawn);
        let to_drawn = arriving.drawn;
        let presence = match at {
            Some(at) => {
                f32::from(u8::from(from_drawn))
                    + (f32::from(u8::from(to_drawn)) - f32::from(u8::from(from_drawn))) * at
            }
            None => f32::from(u8::from(to_drawn)),
        };
        let was = leaving.as_ref().map(|leaving| leaving.width);
        let Fitted {
            style,
            hover_style,
            icon,
            icon_after_text,
            content,
            text_width,
            icon_advance,
            written_icon_advance,
            width,
            ..
        } = arriving;
        // A wording travel really is laid out between two fitted widths. A fold keeps the
        // expanded fitted width and carries only its visible box; `folded` below does that
        // part after the ordinary module has been assembled.
        let expanded_width = width;
        let (travel_width, reserve) = match (at, was) {
            (Some(at), Some(was)) => (was + (width - was) * at, width.max(was)),
            _ => (width, width),
        };
        let width = match folding {
            Some(_) => expanded_width,
            None => travel_width,
        };
        // A module with nothing left to draw in is left out entirely, rather than drawn
        // over whatever the run was making room for.
        if travel_width > available {
            continue;
        }
        let mut sized = SizedModule {
            width,
            drawn_width: None,
            shift: 0.0,
            text_inset: 0.0,
            reserve,
            presence,
            from_drawn,
            to_drawn,
            before: 0.0,
            // Cut at its own edge while it is travelling: the wording it is going to was
            // fitted to the width it lands at, which is not the width it is being drawn
            // in until it arrives.
            clipped: travelling.is_some(),
            text_width,
            hover_style,
            icon_advance,
            written_icon_advance,
            icon,
            icon_after_text,
            art,
            text: content,
            foreground: foreground.unwrap_or(style.foreground),
            background: background.unwrap_or(style.background),
            style,
            action,
            // How many views this module has in all, so a click knows where it wraps.
            alt: (!module.format_alt.is_empty()).then(|| module.format_alt.len() + 1),
            alt_button: module.alt_button,
            collapsible: module.collapse.is_some(),
            collapse_button: module
                .collapse
                .as_ref()
                .map_or(Button::Right, |collapse| collapse.button),
            refresh: module.refresh_button,
            mute: module.mute_button,
            // Only where there is somewhere to scroll to: a command reporting on one
            // thing leaves the wheel alone.
            paged: (pages > 1).then_some(pages),
            // Named only where something on the module answers to a gesture, so an
            // ordinary module costs no allocation on the path that runs every frame.
            name: (!module.format_alt.is_empty()
                || module.collapse.is_some()
                || module.refresh_button.is_some()
                || pages > 1)
                .then(|| module.name.clone()),
            on_click: module.on_click.clone(),
        };
        if let (Some(at), Some(shut)) = (folding, module_shut.as_ref()) {
            sized.fold(at, shut);
            // With no collapsed table the module keeps the appearance it resolved from
            // its state and provider. A table asks for a definite look, and that look is
            // carried over the travel just as a group's collapsed style is.
            if module.collapse.as_ref().is_some_and(|c| c.style.is_some()) {
                sized.carry_style(at, &shut.style);
            }
        }
        // Charge the final reserve, after every kind of travel has had the opportunity to
        // raise it. Today a module fold's `Shut` comes from the same folded `Fitted` already
        // included in `reserve`; keeping this after `fold` makes that safety independent of
        // how a future destination is built. Holding the wider endpoint is what prevents
        // modules behind a travel from being re-truncated on every frame.
        left = (available - sized.reserve).max(0.0);
        modules.push(sized);
    }
    let _ = height;
    let sized = finish_group(group, ends, between, modules)?;
    // Laid out open, whichever way it is going: what a fold moves is the island's edge,
    // not what is written inside it. Measuring the content against the width it has got
    // to would re-truncate every module on every frame of the fold, and a title would
    // shed a character at a time instead of sliding out of view.
    let (Some(at), Some(collapse)) = (folding, &group.collapse) else {
        return Some(sized);
    };
    let style = collapse.style.clone();
    let spec = style.icon.as_ref().expect("validated collapsed icon");
    // The island the fold is travelling to, worked out the way the shut branch above and
    // `finish_group` between them would have worked it out: the icon, and everything drawn
    // beside it that belongs to the group rather than to a module. Landing anywhere else
    // would show as a jump on the last frame.
    let (_, _, advance) = collapsed_icon(spec, &style, text);
    let module = shut_width(&style, advance);
    let shut = Shut {
        width: module + group.padding * 2.0 + ends.left_width() + ends.right_width(),
        module,
        style,
    };
    let folded = sized.folded(at, shut);
    // A fold does not only ever shrink: a group holding one narrow module can be closing
    // over an icon wider than all of it. The open width was checked against the budget on
    // the way in and the shut one is checked by the branch above, but the widths in
    // between are neither, and a group that grows out of its run would be drawn over its
    // neighbour or off the end of the bar. Left out instead, which is what the shut branch
    // does with an island that cannot fit - and a fold heading for one that cannot fit was
    // going to end up left out anyway. `max_width` is not checked here for the same
    // reason: a collapsed icon that could not fit inside it is refused when the config is
    // read, so no fold can be travelling towards one.
    (folded.width <= budget).then_some(folded)
}

/// The island a fold is travelling to: how wide it is, how wide the one module in it is,
/// and what that module is drawn in.
#[derive(Clone)]
struct Shut {
    width: f32,
    module: f32,
    style: Style,
}

/// What a collapsed thing draws in place of its contents, and how wide that is.
///
/// Geometry goes in the icon slot and is sized by `icon_size`. A glyph is wording, shaped
/// with the font like any other, so it goes in the text and only the backend knows how wide
/// it comes out - which is why this takes a measurer at all.
fn collapsed_icon(
    icon: &IconSpec,
    style: &Style,
    text: &mut dyn Measure,
) -> (Option<(Icon, usize)>, String, f32) {
    match icon {
        IconSpec::Native(native) => (
            Some((*native, 0)),
            String::new(),
            style.icon_size * native.width(),
        ),
        IconSpec::Text(glyph) => (None, glyph.to_string(), text.measure(glyph)),
    }
}

/// The width the collapsed icon needs, which is what a fold travels to and from.
fn shut_width(style: &Style, icon_advance: f32) -> f32 {
    (icon_advance + style.padding * 2.0).max(style.min_width)
}

fn finish_group(
    group: &GroupCfg,
    mut ends: Ends,
    between: f32,
    mut modules: Vec<SizedModule>,
) -> Option<SizedGroup> {
    if modules.is_empty() {
        return None;
    }

    // A gap belongs to the visible run before the module, rather than to either endpoint
    // outright. This makes the right gap survive when a middle module disappears while
    // the redundant left one shrinks with it, and handles the same topology at either end
    // of a group. The prefix maximum is the nearest visible run on the left.
    let mut prior = 0.0_f32;
    for module in &mut modules {
        module.before = between * module.presence.min(prior);
        prior = prior.max(module.presence);
    }
    let presence = prior;

    let content: f32 = modules.iter().map(SizedModule::drawn_width).sum();
    // What the modules between them have reserved, which is more than they are showing
    // while one is travelling between wordings or folding. The island charges the run
    // that instead, so the groups after it are measured against one budget from end to end
    // of either travel.
    let held: f32 = modules.iter().map(|m| m.reserve).sum();
    let gaps: f32 = modules.iter().map(|m| m.before).sum();
    // A shaped end needs room of its own: it is drawn beside the modules, not over them.
    // When the whole island appears or disappears, its horizontal padding and caps are
    // part of the width being travelled. Vertical padding stays put: this is a width
    // animation and changing height would make the contents bob while they move.
    let full_ends = ends.left_width() + ends.right_width();
    ends.width *= presence;
    ends.overlap *= presence;
    let horizontal_padding = group.padding * presence;
    let furniture = gaps + ends.left_width() + ends.right_width() + horizontal_padding * 2.0;
    // Reserve the wider endpoint topology so modules in later groups are fitted once for
    // the whole travel. Several modules can move at once, so count both endpoints rather
    // than assuming one appearing module or one disappearing module.
    let from = modules.iter().filter(|module| module.from_drawn).count();
    let to = modules.iter().filter(|module| module.to_drawn).count();
    let reserved_gaps = between * from.saturating_sub(1).max(to.saturating_sub(1)) as f32;
    let reserved_furniture = reserved_gaps + full_ends + group.padding * 2.0;
    let width = content + furniture;
    Some(SizedGroup {
        collapse: group
            .collapse
            .as_ref()
            .map(|c| (group.name.clone(), c.button)),
        width,
        reserve: held + reserved_furniture,
        clipped: false,
        travel: 0.0,
        text_inset: 0.0,
        shift: 0.0,
        leading: None,
        background: group.background,
        opacity: group.opacity,
        edges: group.edges,
        padding: group.padding,
        horizontal_padding,
        separator: group.separator,
        ends,
        modules,
    })
}

impl SizedModule {
    /// The box this module contributes to its island at this point in a fold.
    fn drawn_width(&self) -> f32 {
        self.drawn_width.unwrap_or(self.width)
    }

    /// Carry the module's box and expanded contents towards a collapsed destination.
    fn fold(&mut self, at: f32, shut: &Shut) {
        let at = at.clamp(0.0, 1.0);
        if at == 0.0 {
            return;
        }
        self.reserve = self.reserve.max(shut.module);
        self.drawn_width = Some(self.width + (shut.module - self.width) * at);
        self.shift += self.travel(shut) * at;
        self.text_inset = ((shut.module - self.mark_width(&shut.style)) / 2.0).max(0.0) * at;
        self.clipped = true;
    }

    /// Carry the parts of a module's appearance that can change continuously to a collapsed
    /// style. Padding and minimum width are already represented by the travelling box, while
    /// the icon's size has to move with its paint or the last animated frame and the settled
    /// one draw different geometry.
    fn carry_style(&mut self, at: f32, style: &Style) {
        let at = at.clamp(0.0, 1.0);
        self.foreground = self.foreground.mix(style.foreground, at);
        self.background = self.background.mix(style.background, at);
        self.style.radius += (style.radius - self.style.radius) * at;
        self.style.icon_size += (style.icon_size - self.style.icon_size) * at;
        if let Some(hover) = self.hover_style.as_mut() {
            hover.foreground = hover.foreground.mix(style.foreground, at);
            hover.background = hover.background.mix(style.background, at);
            hover.radius += (style.radius - hover.radius) * at;
        }
    }

    /// How far the contents still have to move for their icon to reach the centre of the
    /// collapsed module.
    ///
    /// The module's own fold may already have moved it before the group holding it folds.
    /// Returning the distance from that carried position lets the two travels compose
    /// without counting the module contribution twice.
    fn travel(&self, shut: &Shut) -> f32 {
        if self.icon_after_text || (self.icon_advance <= 0.0 && self.written_icon_advance <= 0.0) {
            return 0.0;
        }
        // Start from the same content origin `place` uses. A native icon can change size on
        // the way, so its destination is based on the size it will be wearing rather than on
        // its expanded advance. A configured glyph is part of the wording run and keeps the
        // measured width of that glyph. Source-owned text decorations trail the wording and
        // never set `written_icon_advance`, which deliberately leaves their current behaviour.
        let content = self.icon_advance + self.text_width;
        let from = ((self.width - content) / 2.0).max(self.style.padding);
        let mark = self.mark_width(&shut.style);
        let to = (shut.module - mark) / 2.0;
        to - from - self.shift
    }

    /// Width of the mark this module will still be drawing in `style` once its wording has
    /// gone. A native icon follows `icon_size`; a configured glyph keeps its measured width.
    fn mark_width(&self, style: &Style) -> f32 {
        match self.icon {
            Some((icon, _)) => style.icon_size * icon.width(),
            None => self.written_icon_advance,
        }
    }
}

impl SizedGroup {
    /// The island part-way shut: as wide as the fold has got, holding what it held.
    ///
    /// The readings keep the width they were measured at and the edge travels over them,
    /// which is what makes the two ends of a fold line up with the frames either side of
    /// it. The leading module is the exception, because it is the one the island is left
    /// holding: its box travels to the shut island's, its icon travels to where the shut
    /// island draws one, and everything behind it packs along after its box the way it
    /// always does. That last part is what carries the rest off the end - a reading is
    /// pushed past the edge by the box in front of it, not by a shove of its own, so the
    /// spacing between them never opens up and nothing is left standing inside the island
    /// on the frame the fold arrives.
    ///
    /// The frame this arrives on is the shut island pixel for pixel, with one licensed
    /// exception: the collapsed style names its own icon, and where that is not the icon
    /// the first module already draws, the last frame swaps one picture for another. A
    /// glyph does not blend into a glyph, and folding a whole island down to a mark of its
    /// own - `gruvbox-islands` folds its readings down to Tux - is the point rather than an
    /// oversight. Everything else about the two frames has to agree.
    fn folded(mut self, at: f32, shut: Shut) -> SizedGroup {
        let at = at.clamp(0.0, 1.0);
        if at == 0.0 {
            // A fold about to leave is an island that has not moved, and an island that
            // has not moved is the open one: cutting it here would put its contents behind
            // the outline mask for a frame, and a square module corner inset into a round
            // island is outside that outline. The frame the click lands on would flash.
            return self;
        }
        // What the run charges this island stays where it was for the whole travel, at the
        // widest the island is ever going to be. Charging what it is showing would hand
        // every group after it a different budget on every frame, and a window title
        // downstream would shed and regain a character at a time all the way through -
        // which is the artefact the fold holds its own contents still to avoid.
        self.reserve = self.reserve.max(shut.width);
        self.width += (shut.width - self.width) * at;
        self.shift = self.modules[0].travel(&shut) * at;
        // The leading module's box: as far as its own edge has been carried, or as far as
        // the shut island reaches, whichever is further. The first is what a fold has
        // always done, and it leaves the readings behind it exactly where they were while
        // the edge sweeps over them. The second is for when that is not enough to carry
        // them off the end - an island closing on a collapsed padding wider than the
        // reading it is closing over arrives with its second module still inside the edge,
        // where the settled frame has only the one. Stretching the module the island is
        // left holding pushes the rest out, and never opens a gap for them to show in.
        let first = self.modules[0].drawn_width();
        self.leading = Some((first + self.shift).max(first + (shut.module - first) * at));
        // The room the shut island keeps around its icon, which the wording has to be out
        // of before the fold arrives. Cutting text where the fills are cut would leave it
        // standing in that padding to the last frame: an island closing on a generous
        // collapsed padding shows the first characters of its reading until the settled
        // frame takes them away in one step.
        self.text_inset =
            ((shut.module - self.modules[0].mark_width(&shut.style)) / 2.0).max(0.0) * at;
        // The island is left holding the collapsed style's icon on the collapsed style's
        // ground, so the module that stays behind arrives wearing them. Both ends of the
        // hand-off are then the same picture and the swap on the last frame is a swap of
        // nothing but the icon itself.
        self.modules[0].carry_style(at, &shut.style);
        self.clipped = true;
        self.travel = at;
        self
    }
}

fn place(sized: SizedGroup, mut x: f32, height: f32, pointer: Option<(f32, f32)>) -> PlacedGroup {
    let group_x = x;
    let inner_y = sized.padding;
    let inner_h = (height - sized.padding * 2.0).max(0.0);
    x += sized.horizontal_padding;

    let separator = sized.separator;
    let draw_separators = !separator.shape.is_none();

    let mut modules: Vec<PlacedModule> = Vec::with_capacity(sized.modules.len());
    let mut separators = Vec::new();

    let ends = sized.ends;
    let lead = ends.left_width();
    x += lead;
    for (i, m) in sized.modules.into_iter().enumerate() {
        x += m.before;
        let drawn_width = m.drawn_width();
        // The box a fold is taking to the shut island's width, for the leading module; the
        // measured one for everything behind it, which packs along after that box and is
        // pushed off the end by it.
        let width = match i {
            0 => sized.leading.unwrap_or(drawn_width),
            _ => drawn_width,
        };
        // Icon and text are centred together inside the module box - the one it was
        // measured at, plus wherever a fold is carrying it. Centring them in a travelling
        // box instead would land them by the width the island happens to have rather than
        // on the icon the shut island draws.
        let content_width = m.icon_advance + m.text_width;
        let carried = match i {
            0 => m.shift + sized.shift,
            _ => m.shift,
        };
        // Never further in than the module's own padding. A settled box is its content
        // plus that padding on both sides, so the centre is already at or past it and the
        // floor changes nothing; a box a click is carrying between two wordings can be
        // narrower than what is written in it, and centring that would hang the first
        // characters off the left of the module and into the one before it.
        let content_x = x + ((m.width - content_width) / 2.0).max(m.style.padding) + carried;
        let icon_gap = if m.text.is_empty() {
            0.0
        } else {
            m.style.gap()
        };
        let icon_x = match m.icon_after_text {
            true => content_x + m.text_width + icon_gap,
            false => content_x,
        };
        let placed_icon = m.icon.map(|(icon, level)| PlacedIcon {
            icon,
            level,
            x: icon_x,
            y: inner_y + (inner_h - m.style.icon_size) / 2.0,
            size: m.style.icon_size,
            art: m.art.clone(),
        });
        // `icon_advance` is the expanded measurement and stays fixed while a fold moves.
        // Text still has to follow the icon actually being painted when the collapsed style
        // gives that icon a different size, so derive only this placement advance anew.
        let painted_icon_advance = match (m.icon_after_text, m.icon) {
            (true, _) => 0.0,
            (false, Some((icon, _))) if m.style.icon_size > 0.0 => {
                m.style.icon_size * icon.width() + icon_gap
            }
            (false, Some(_)) => 0.0,
            (false, None) => m.icon_advance,
        };

        // Hover is resolved here, against the final rectangle, so it is always the module
        // actually under the pointer rather than one from a previous frame.
        let over = pointer.is_some_and(|(px, py)| contains(px, py, x, inner_y, width, inner_h));
        let paint = match (over, &m.hover_style) {
            (true, Some(hover)) => hover,
            _ => &m.style,
        };
        let (foreground, background) = if over && m.hover_style.is_some() {
            (paint.foreground, paint.background)
        } else {
            (m.foreground, m.background)
        };

        // A wording transition stops inside the box by the fitted style's padding, as it
        // always has. A module fold uses the travelling box edge itself for marks and
        // progressively moves only the wording further in, leaving the collapsed padding
        // empty without clipping the icon that remains there.
        let content_right = m.clipped.then_some(match m.drawn_width {
            Some(_) => x + width,
            None => x + width - m.style.padding,
        });
        let text_right = content_right.map(|right| match m.drawn_width {
            Some(_) => right - m.text_inset,
            None => right,
        });
        modules.push(PlacedModule {
            x,
            y: inner_y,
            width,
            height: inner_h,
            icon: placed_icon,
            text: m.text,
            text_x: content_x + painted_icon_advance,
            content_right,
            text_right,
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
        x += width;
    }

    if lead > 0.0
        && let Some(first) = modules.first()
    {
        separators.push(end_separator(
            ends.left,
            group_x + sized.horizontal_padding,
            inner_y,
            lead,
            inner_h,
            &ends,
            separator.direction,
            first.background,
            true,
        ));
    }
    let trail = ends.right_width();
    // Where the island ends, which is where its content ends until a fold moves the edge
    // in over it. Everything drawn at a group's trailing end follows the island: a cap
    // left behind at the content's edge is a cap outside the island, and the clip that
    // keeps the content in would take it off the screen altogether.
    let trail_x = match sized.clipped {
        true => group_x + sized.width - trail - sized.horizontal_padding,
        false => x,
    };
    // Where the contents stop, for an island holding more than it shows. The cap hangs
    // off it, the colour it takes comes from whichever module is still visible beside it,
    // and the island carries it out for the renderer and for the join beyond.
    let content_right = sized.clipped.then_some(trail_x);
    if draw_separators {
        for pair in modules.windows(2) {
            let width = (pair[1].x - pair[0].x - pair[0].width).max(0.0);
            if width > 0.0 {
                let overlap = match separator.width > 0.0 {
                    true => separator.overlap * (width / separator.width).clamp(0.0, 1.0),
                    false => 0.0,
                };
                separators.push(separator_between(
                    Separator {
                        width,
                        overlap,
                        ..separator
                    },
                    &pair[0],
                    &pair[1],
                    sized.background,
                    None,
                ));
            }
        }
    }
    if trail > 0.0
        && let Some(last) = module_before(&modules, content_right)
    {
        separators.push(end_separator(
            ends.right,
            trail_x,
            inner_y,
            trail,
            inner_h,
            &ends,
            separator.direction,
            last.background,
            false,
        ));
    }

    // The island's own cap is the edge its contents are cut with, until a join replaces it,
    // narrowing to a straight column as the fold lands: the frame it lands on is the shut
    // island, which holds one module and cuts nothing, so a cut still leaning on that frame
    // is a wedge of the readings behind it standing inside the cap.
    let content_edge = content_right
        .and_then(|_| separators.last())
        .filter(|edge| edge.cap && !edge.shape.is_none())
        .map(|edge| cut_edge(edge, sized.travel));

    PlacedGroup {
        collapse: sized.collapse,
        x: group_x,
        y: 0.0,
        width: sized.width,
        height,
        background: sized.background,
        opacity: sized.opacity,
        edges: sized.edges,
        content_right,
        text_right: content_right.map(|right| right - sized.text_inset),
        content_edge,
        modules,
        separators,
    }
}

/// The shape a folding island's contents are cut with: its trailing transition, closing on
/// a straight column as the island arrives at the one it is folding down to.
fn cut_edge(edge: &PlacedSeparator, travel: f32) -> PlacedSeparator {
    PlacedSeparator {
        width: edge.width * (1.0 - travel.clamp(0.0, 1.0)),
        ..edge.clone()
    }
}

/// The same colour rule serves internal separators and joins between groups.
fn separator_between(
    separator: Separator,
    previous: &PlacedModule,
    next: &PlacedModule,
    ground: Color,
    at: Option<f32>,
) -> PlacedSeparator {
    PlacedSeparator {
        x: at.unwrap_or(previous.x + previous.width),
        y: previous.y,
        width: separator.width,
        height: previous.height,
        shape: separator.shape,
        direction: separator.direction,
        overlap: separator.overlap,
        inverted: false,
        cap: false,
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
            previous.reserve -= reclaimed;
            previous.ends.right = SeparatorShape::None;
            previous.edges.right = EdgeShape::None;
            sized.edges.left = EdgeShape::None;
        }
        left = room - sized.reserve;
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
        cap: true,
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

    // A run measures twice: what it is showing, which is what it is placed by, and what it
    // has reserved, which is what the runs after it are given to fit in. The two differ
    // only while an island, module or wording is travelling, and holding them apart keeps
    // that travel from re-truncating its neighbours on every frame.
    let run_span = |groups: &Vec<SizedGroup>, separator: Option<Separator>, shown: bool| -> f32 {
        if groups.is_empty() {
            return 0.0;
        }
        let width = |g: &SizedGroup| if shown { g.width } else { g.reserve };
        groups.iter().map(width).sum::<f32>()
            + separator.map_or(gap, |s| s.width) * (groups.len() - 1) as f32
    };
    let run_width = |groups: &Vec<SizedGroup>, separator: Option<Separator>| -> f32 {
        run_span(groups, separator, true)
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
            left -= sized.reserve + gap;
            out.push(sized);
        }
        out
    };

    let right = size_run(&cfg.positions[2], width, text);
    let right_width = run_width(&right, cfg.positions[2].separator);
    let right_held = run_span(&right, cfg.positions[2].separator, false);
    let left = size_run(&cfg.positions[0], (width - right_held - gap).max(0.0), text);
    let left_width = run_width(&left, cfg.positions[0].separator);
    let left_held = run_span(&left, cfg.positions[0].separator, false);
    let between = (width - right_held - left_held - gap * 2.0).max(0.0);
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
        // Where the island before this one ends, for a fold that has moved its edge in
        // over its content. The join between two of them belongs to the islands, so it
        // travels with the edge rather than staying behind on the last module - which is
        // where the two part company, and where a join left behind would be drawn over
        // whatever the fold made room for.
        let mut travelled: Option<f32> = None;
        // How far that island's own fold has got, for the cut the join hands back to it.
        let mut behind_travel = 0.0;
        for group in groups {
            let (w, travel) = (group.width, group.travel);
            let placed = place(group, x, height, pointer);
            // Where this island's contents stop, which is where anything drawn at its
            // trailing end belongs - the join below included.
            let edge = placed.content_right;
            if let Some(separator) = position.separator
                && frame.groups.len() > first
                && let Some(previous) = frame.groups.last().and_then(PlacedGroup::trailing_module)
                && let Some(next) = placed.modules.first()
            {
                let join =
                    separator_between(separator, previous, next, frame.background, travelled);
                // A joined island has no cap of its own, so the join is what its contents
                // are cut with. It is drawn from the frame, but the shape belongs to the
                // island behind it as much as a cap would.
                if let Some(behind) = frame.groups.last_mut()
                    && behind.content_edge.is_none()
                    && behind.content_right.is_some()
                    && !join.shape.is_none()
                {
                    behind.content_edge = Some(cut_edge(&join, behind_travel));
                }
                frame.group_separators.push(join);
            }
            (travelled, behind_travel) = (edge, travel);
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
            collapse: None,
            content_right: None,
            text_right: None,
            content_edge: None,
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
                content_right: None,
                text_right: None,
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
mod tests;
