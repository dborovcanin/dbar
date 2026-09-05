//! Declarative TOML configuration and the style cascade.
//!
//! Parsing happens in two steps: serde fills the `raw` structs, then `resolve` turns
//! `$name` color references and style names into concrete values so that nothing downstream
//! has to do lookups while rendering.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use serde::Deserialize;

use crate::collect::Which;
use crate::color::Color;
use crate::format::Format;
use crate::icon::Icon;
use crate::status::{Control, FieldSpec, Fields, State, Value};

pub const DEFAULT_CONFIG: &str = include_str!("../examples/config.toml");

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    Top,
    Bottom,
}

/// Which layer-shell layer the bar sits on, and so what it is drawn over.
///
/// `top` is above ordinary windows and below fullscreen-style overlays, which is what a bar
/// normally wants. `bottom` puts the bar under floating windows, so one can be dragged over
/// it; `overlay` puts it above everything, including screen lockers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BarLayer {
    Background,
    Bottom,
    Top,
    Overlay,
}

/// The transition drawn between two neighbouring modules.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SeparatorShape {
    #[default]
    None,
    Line,
    Slant,
    Chevron,
    Notch,
    Round,
    Curve,
}

impl SeparatorShape {
    pub fn is_none(self) -> bool {
        self == SeparatorShape::None
    }
}

/// Which way a separator shape points.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    #[default]
    Right,
    Left,
}

/// How a group's outer corners are cut.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeShape {
    #[default]
    Round,
    None,
}

/// Where a module's content comes from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Source {
    /// Something dbar measures itself.
    Native(Which),
    /// A block from an external status provider, matched by name.
    #[default]
    Provider,
    /// The title of the focused window.
    SwayWindow,
    /// One entry per workspace, expanded at layout time.
    SwayWorkspaces,
    /// The active keyboard layout, with the short forms the module gives its layouts.
    SwayLanguage(BTreeMap<String, String>),
    /// The binding mode the compositor is in.
    SwayMode,
}

impl Source {
    /// What a format written against this source may name.
    pub fn fields(&self) -> &'static [FieldSpec] {
        match self {
            Source::Native(which) => which.fields(),
            Source::Provider => crate::status::i3bar::FIELDS,
            Source::SwayWindow => crate::sway::WINDOW_FIELDS,
            Source::SwayWorkspaces => crate::sway::WORKSPACE_FIELDS,
            Source::SwayLanguage(_) => crate::sway::LANGUAGE_FIELDS,
            Source::SwayMode => crate::sway::MODE_FIELDS,
        }
    }

    /// What the module says when the config does not give it a format.
    ///
    /// Each source has one thing it is obviously for, so the common case needs no `format`
    /// line at all.
    fn default_format(&self) -> &'static str {
        match self {
            Source::Native(which) => which.default_format(),
            Source::Provider => "$text",
            Source::SwayWindow => "$title",
            Source::SwayWorkspaces => "$name",
            Source::SwayLanguage(_) => " $short ",
            Source::SwayMode => " $mode ",
        }
    }
}

/// Where a separator takes its colour from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SeparatorColor {
    /// The background of the module before the separator - classic Powerline.
    Previous,
    /// The background of the module after it.
    Next,
    /// The foreground of the module before it.
    Foreground,
    /// The group background.
    Background,
    Fixed(Color),
}

// ---------------------------------------------------------------------------
// Raw (as written in TOML)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    bar: RawBar,
    #[serde(default, rename = "i3bar")]
    i3bar: RawI3Bar,
    #[serde(default)]
    colors: HashMap<String, String>,
    #[serde(default)]
    left: RawPosition,
    #[serde(default)]
    center: RawPosition,
    #[serde(default)]
    right: RawPosition,
    #[serde(default, rename = "style")]
    styles: HashMap<String, RawStyle>,
    #[serde(default, rename = "group")]
    groups: HashMap<String, RawGroup>,
    #[serde(default, rename = "module")]
    modules: HashMap<String, RawModule>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBar {
    #[serde(default = "default_height")]
    height: u32,
    #[serde(default = "default_edge")]
    position: Edge,
    /// Where the bar sits in the compositor's stack. Defaults to above ordinary windows.
    #[serde(default = "default_layer")]
    layer: BarLayer,
    #[serde(default)]
    margin: i32,
    #[serde(default = "default_gap")]
    gap: f32,
    #[serde(default = "default_font")]
    font: String,
    /// Base icon size; defaults to a multiple of the font size.
    icon_size: Option<f32>,
    #[serde(default)]
    background: RawBarBackground,
    /// Reserve space so windows are not covered. Defaults to on.
    #[serde(default = "default_true")]
    exclusive: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBarBackground {
    #[serde(default)]
    color: Option<String>,
    #[serde(default)]
    radius: f32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawI3Bar {
    #[serde(default = "default_i3bar_command")]
    command: String,
    #[serde(default)]
    args: Vec<String>,
    /// Names for the provider's blocks, in the order it emits them.
    #[serde(default)]
    names: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPosition {
    #[serde(default)]
    groups: Vec<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStyle {
    background: Option<String>,
    foreground: Option<String>,
    padding: Option<f32>,
    radius: Option<f32>,
    min_width: Option<f32>,
    max_width: Option<f32>,
    icon: Option<String>,
    icon_size: Option<f32>,
    /// Space between an icon and the text beside it. Defaults to a share of the icon.
    icon_gap: Option<f32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGroup {
    #[serde(default)]
    modules: Vec<String>,
    background: Option<String>,
    opacity: Option<f32>,
    #[serde(default)]
    radius: f32,
    #[serde(default)]
    padding: f32,
    #[serde(default)]
    spacing: f32,
    separator: Option<RawSeparator>,
    edges: Option<RawEdges>,
    ends: Option<RawEnds>,
}

/// `[group.*.ends]`: the transition drawn where the group meets the bar.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEnds {
    #[serde(default)]
    left: SeparatorShape,
    #[serde(default)]
    right: SeparatorShape,
    /// Falls back to the width of the group's own separators.
    width: Option<f32>,
    overlap: Option<f32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSeparator {
    #[serde(default)]
    shape: SeparatorShape,
    #[serde(default = "default_separator_width")]
    width: f32,
    #[serde(default)]
    direction: Direction,
    #[serde(default = "default_separator_color")]
    color: String,
    #[serde(default)]
    overlap: f32,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEdges {
    #[serde(default)]
    left: EdgeShape,
    #[serde(default)]
    right: EdgeShape,
    /// Falls back to the group's own `radius`.
    radius: Option<f32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModule {
    /// Name of a `[style.*]` table to inherit from.
    style: Option<String>,
    /// Where the content comes from: something dbar measures, an external provider, or
    /// the compositor.
    source: Option<String>,
    /// What the module says, written against the source's fields.
    format: Option<String>,
    /// Further wordings, which a left click moves through and back round. One is written
    /// as a string; several as a list.
    format_alt: Option<RawAlt>,
    /// How often to read, for a source dbar measures itself: "2s", "500ms", "1m".
    interval: Option<String>,
    /// Which filesystem a `disk` module is about. Defaults to the root.
    path: Option<String>,
    /// Which interface a `network` module watches. Defaults to whichever is up.
    interface: Option<String>,
    /// Which hwmon chip a `temperature` module reads. Defaults to the processor's own.
    chip: Option<String>,
    /// What a `sway:language` module calls each layout, keyed by the name xkb gives it.
    /// A layout named here is what `$short` says; anything else is abbreviated.
    #[serde(default)]
    layouts: BTreeMap<String, String>,
    /// Read this module's source again on SIGRTMIN+N.
    signal: Option<i32>,
    /// What one scroll notch over this module is worth: "5%". Only for the sources dbar
    /// can change as well as read.
    scroll: Option<String>,
    /// Whether clicks on this module operate what it is showing. Only for a player, whose
    /// buttons are play, pause and skip rather than a step in either direction.
    controls: Option<bool>,
    /// Whether a right click folds this module down to its icon, and back.
    collapsible: Option<bool>,
    /// Conditional restyling, keyed on the block's value or its urgent flag.
    #[serde(default)]
    /// Ordered by name, and tried in that order: the first rule that matches is the one
    /// that applies, so a `HashMap` here would pick a different winner between runs
    /// whenever two rules can be true at once.
    states: BTreeMap<String, RawState>,
    #[serde(flatten)]
    overrides: RawStyle,
}

/// `format_alt` is one wording or a list of them, because a module with a single second
/// wording should not have to be written as a list of one.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawAlt {
    One(String),
    Several(Vec<String>),
}

impl RawAlt {
    fn written(&self) -> &[String] {
        match self {
            RawAlt::One(one) => std::slice::from_ref(one),
            RawAlt::Several(several) => several,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawState {
    /// Name of a `[style.*]` table whose keys are applied over the module's own.
    style: Option<String>,
    /// Matches when the source itself rates what it is reporting this way: "good",
    /// "warning", "critical", "error".
    state: Option<String>,
    /// The field a bound applies to. Without it, `above` and `below` read whichever value
    /// the source nominated as the one it is mainly about.
    field: Option<String>,
    /// Matches when the named field reads exactly this, ignoring case.
    equals: Option<String>,
    /// Matches when the block's percentage is under this.
    below: Option<f32>,
    /// Matches when the block's percentage is over this.
    above: Option<f32>,
    /// Matches when the provider marks the block urgent.
    #[serde(default)]
    urgent: bool,
    /// Matches while the pointer is over the module.
    #[serde(default)]
    hover: bool,
    /// Matches the focused workspace.
    #[serde(default)]
    focused: bool,
    /// Matches a workspace shown on some output.
    #[serde(default)]
    visible: bool,
    /// Matches when the module's text contains this.
    contains: Option<String>,
    /// Remove the matched text from what is drawn.
    #[serde(default)]
    strip: bool,
    #[serde(flatten)]
    overrides: RawStyle,
}

fn default_height() -> u32 {
    34
}
fn default_edge() -> Edge {
    Edge::Top
}
fn default_layer() -> BarLayer {
    BarLayer::Top
}
fn default_gap() -> f32 {
    6.0
}
fn default_font() -> String {
    "sans-serif 10".to_string()
}
fn default_true() -> bool {
    true
}
fn default_i3bar_command() -> String {
    "i3status-rs".to_string()
}
fn default_separator_width() -> f32 {
    10.0
}
fn default_separator_color() -> String {
    "previous".to_string()
}

impl Default for RawBar {
    fn default() -> Self {
        RawBar {
            height: default_height(),
            position: default_edge(),
            layer: default_layer(),
            margin: 0,
            gap: default_gap(),
            font: default_font(),
            icon_size: None,
            background: RawBarBackground::default(),
            exclusive: true,
        }
    }
}

impl Default for RawI3Bar {
    fn default() -> Self {
        RawI3Bar {
            command: default_i3bar_command(),
            args: Vec::new(),
            names: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Resolved
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Config {
    pub bar: Bar,
    pub i3bar: I3Bar,
    /// Groups per position, in `POSITIONS` order.
    pub positions: [Vec<Group>; 3],
}

#[derive(Debug, Clone)]
pub struct Bar {
    pub height: u32,
    pub position: Edge,
    pub layer: BarLayer,
    pub margin: i32,
    pub gap: f32,
    pub font_family: String,
    pub font_size: f32,
    /// Icon edge length used unless a style or module overrides it.
    pub icon_size: f32,
    pub background: Color,
    pub radius: f32,
    pub exclusive: bool,
}

/// How to start an external i3bar-protocol provider, when a module reads from one.
#[derive(Debug, Clone)]
pub struct I3Bar {
    pub command: String,
    pub args: Vec<String>,
    /// Stable names for the provider's blocks, by position.
    ///
    /// The i3bar protocol has no way for a provider to name its blocks usefully -
    /// i3status-rs numbers them - so groups would otherwise have to select on "0", "1",
    /// and silently follow the wrong block whenever the provider's order changed.
    pub names: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Group {
    pub background: Color,
    /// How much of the finished island reaches the screen, 0.0 to 1.0.
    ///
    /// A group is drawn opaque and then composited once at this alpha, so the modules and
    /// separators inside it meet each other at full opacity however they overlap. Alpha
    /// written into a colour cannot do this: a filled separator paints its ground across
    /// the whole gap and its shape over the top, and two translucent fills composite where
    /// they overlap, which leaves the shape heavier than the modules it runs between.
    pub opacity: f32,
    pub padding: f32,
    pub spacing: f32,
    pub separator: Separator,
    pub edges: Edges,
    pub ends: Ends,
    /// `modules = ["*"]` takes every block the provider emits, in provider order.
    pub wildcard: bool,
    pub modules: Vec<Module>,
}

#[derive(Debug, Clone, Copy)]
pub struct Separator {
    pub shape: SeparatorShape,
    /// Horizontal space the transition occupies between two modules.
    pub width: f32,
    pub direction: Direction,
    pub color: SeparatorColor,
    /// Bleed drawn past each side, to hide seams between antialiased edges.
    pub overlap: f32,
}

impl Default for Separator {
    fn default() -> Self {
        Separator {
            shape: SeparatorShape::None,
            width: default_separator_width(),
            direction: Direction::Right,
            color: SeparatorColor::Previous,
            overlap: 0.0,
        }
    }
}

/// How a group's outer boundary meets the bar behind it.
///
/// A separator is a transition between two modules; this is the same transition between a
/// module and nothing, which is what turns a run of blocks into a ribbon with a point on
/// the end. The shapes face the way the group's separators do.
#[derive(Debug, Clone, Copy, Default)]
pub struct Ends {
    pub left: SeparatorShape,
    pub right: SeparatorShape,
    pub width: f32,
    pub overlap: f32,
}

impl Ends {
    /// Space the left end needs beside the modules, which is none unless it is drawn.
    pub fn left_width(&self) -> f32 {
        if self.left.is_none() { 0.0 } else { self.width }
    }

    pub fn right_width(&self) -> f32 {
        if self.right.is_none() {
            0.0
        } else {
            self.width
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Edges {
    pub left: EdgeShape,
    pub right: EdgeShape,
    pub radius: f32,
}

#[derive(Debug, Clone)]
pub struct Module {
    pub name: String,
    pub source: Source,
    /// How often the source behind this module is read. Only native sources are read by
    /// dbar, so this is `None` for everything else.
    pub interval: Option<Duration>,
    /// The offset from SIGRTMIN that reads this module's source again.
    pub signal: Option<i32>,
    /// What a click or a scroll here operates, and by how much where that means anything.
    pub control: Option<(Control, f64)>,
    /// Whether a right click folds this module down to its icon.
    pub collapsible: bool,
    /// What the module says, already parsed and checked against the source's fields.
    pub format: Format,
    /// The further wordings a left click moves through, in order. Empty when the config
    /// gives none, and a click then does nothing.
    pub format_alt: Vec<Format>,
    pub style: Style,
    /// Checked in order; the first match replaces the module's style.
    pub states: Vec<StateRule>,
}

/// What a module currently is, for matching state rules against.
#[derive(Clone, Copy, Debug, Default)]
pub struct StateFlags {
    pub urgent: bool,
    pub focused: bool,
    pub visible: bool,
    /// How the source rates what it is reporting.
    pub state: State,
}

/// One conditional restyling of a module.
#[derive(Debug, Clone)]
pub struct StateRule {
    pub urgent: bool,
    pub hover: bool,
    pub focused: bool,
    pub visible: bool,
    /// Matches when the source rates itself this way.
    pub state: Option<State>,
    /// The field the bounds read. Without one they read the source's primary value.
    pub field: Option<String>,
    /// What the named field has to say, compared without case.
    pub equals: Option<String>,
    /// Substring the module's text must contain.
    pub contains: Option<String>,
    /// Whether that substring is removed from the drawn text once matched.
    pub strip: bool,
    pub below: Option<f32>,
    pub above: Option<f32>,
    pub style: Style,
}

impl StateRule {
    /// Every condition the rule states has to hold. A rule stating none never fires.
    /// Whether this rule applies right now.
    ///
    /// `fields` is what the source published and `text` is what the format made of it. Every
    /// condition but `contains` reads the former, because a rule keyed on the wording would
    /// be reading dbar's own output rather than anything that was measured.
    pub fn matches(&self, flags: StateFlags, hovered: bool, fields: &Fields, text: &str) -> bool {
        let value = match &self.field {
            Some(name) => fields.get(name).and_then(|v| v.num()),
            None => fields.primary().and_then(|v| v.num()),
        };
        if self.urgent && !flags.urgent {
            return false;
        }
        if let Some(state) = self.state
            && flags.state != state
        {
            return false;
        }
        if self.hover && !hovered {
            return false;
        }
        if self.focused && !flags.focused {
            return false;
        }
        if self.visible && !flags.visible {
            return false;
        }
        if let Some(needle) = &self.contains
            && !text.contains(needle.as_str())
        {
            return false;
        }
        if let Some(wanted) = &self.equals {
            let said = match &self.field {
                Some(name) => fields.get(name),
                None => fields.primary(),
            };
            match said {
                Some(Value::Text(t)) if t.eq_ignore_ascii_case(wanted) => {}
                _ => return false,
            }
        }
        if let Some(limit) = self.below {
            match value {
                Some(v) if v < limit as f64 => {}
                _ => return false,
            }
        }
        if let Some(limit) = self.above {
            match value {
                Some(v) if v > limit as f64 => {}
                _ => return false,
            }
        }
        self.urgent
            || self.hover
            || self.focused
            || self.visible
            || self.state.is_some()
            || self.contains.is_some()
            || self.equals.is_some()
            || self.below.is_some()
            || self.above.is_some()
    }

    /// How specific the rule is, for ordering. Urgent first, then the tightest bound; a
    /// rule keyed only on hover carries no bound and so sorts last, leaving a warning or a
    /// critical state visible while the pointer is over it.
    fn specificity(&self) -> (bool, f32) {
        (
            !(self.urgent
                || self.focused
                || self.state.is_some()
                || self.contains.is_some()
                || self.equals.is_some()),
            self.below
                .unwrap_or(f32::MAX)
                .min(self.above.map(|a| 100.0 - a).unwrap_or(f32::MAX)),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Style {
    pub background: Color,
    pub foreground: Color,
    pub padding: f32,
    pub radius: f32,
    pub min_width: f32,
    /// Widest the module may draw, in logical pixels. Zero leaves it unbounded.
    pub max_width: f32,
    pub icon: Option<Icon>,
    /// Icon edge length in logical pixels, independent of the font size.
    pub icon_size: f32,
    /// Space between the icon and the text, in logical pixels. Absent means a share of
    /// the icon size, so a bigger icon keeps its breathing room without being told.
    pub icon_gap: Option<f32>,
}

impl Style {
    /// The space between an icon and the text beside it.
    pub fn gap(&self) -> f32 {
        self.icon_gap.unwrap_or(self.icon_size * ICON_GAP_RATIO)
    }
}

/// Space between an icon and its text, as a share of the icon size, when nothing says
/// otherwise.
const ICON_GAP_RATIO: f32 = 0.25;

/// Icon edge length as a multiple of the font size, when `[bar] icon_size` is absent.
///
/// Ties the two together so that changing `[bar] font` scales the icons with the text.
const ICON_SIZE_RATIO: f32 = 1.4;

/// The starting point of the cascade, carrying the bar-wide icon size.
fn base_style(icon_size: f32) -> Style {
    Style {
        icon_size,
        ..Style::default()
    }
}

impl Default for Style {
    fn default() -> Self {
        Style {
            background: Color::TRANSPARENT,
            foreground: Color::rgba(0xcd, 0xd6, 0xf4, 0xff),
            padding: 8.0,
            radius: 0.0,
            min_width: 0.0,
            max_width: 0.0,
            icon: None,
            icon_size: 14.0,
            icon_gap: None,
        }
    }
}

impl Style {
    /// Apply the non-empty fields of `over` on top of `self`.
    fn overlay(mut self, over: &RawStyle, colors: &Palette) -> Result<Style> {
        if let Some(c) = &over.background {
            self.background = colors.get(c)?;
        }
        if let Some(c) = &over.foreground {
            self.foreground = colors.get(c)?;
        }
        if let Some(v) = over.padding {
            self.padding = v;
        }
        if let Some(v) = over.radius {
            self.radius = v;
        }
        if let Some(v) = over.min_width {
            self.min_width = v;
        }
        if let Some(v) = over.max_width {
            self.max_width = v.max(0.0);
        }
        if let Some(name) = &over.icon {
            self.icon = match name.as_str() {
                "none" => None,
                other => Some(Icon::parse(other).ok_or_else(|| anyhow!("unknown icon {other:?}"))?),
            };
        }
        if let Some(v) = over.icon_size {
            self.icon_size = v.max(0.0);
        }
        if let Some(v) = over.icon_gap {
            self.icon_gap = Some(v.max(0.0));
        }
        Ok(self)
    }
}

/// Named colors, so a config can say `background = "$surface"`.
struct Palette(HashMap<String, Color>);

impl Palette {
    fn new(raw: &HashMap<String, String>) -> Result<Palette> {
        let mut map = HashMap::new();
        for (name, value) in raw {
            // A named color may not itself be a reference; that keeps resolution non-recursive.
            let color =
                Color::parse(value).with_context(|| format!("in [colors] entry {name:?}"))?;
            map.insert(name.clone(), color);
        }
        Ok(Palette(map))
    }

    fn get(&self, spec: &str) -> Result<Color> {
        match spec.strip_prefix('$') {
            Some(name) => self
                .0
                .get(name)
                .copied()
                .ok_or_else(|| anyhow!("unknown color reference ${name}")),
            None => Color::parse(spec),
        }
    }
}

/// Split a `"Family Name 12"` font string into family and point size.
fn parse_font(s: &str) -> (String, f32) {
    let trimmed = s.trim();
    if let Some((family, size)) = trimmed.rsplit_once(' ')
        && let Ok(size) = size.parse::<f32>()
        && !family.trim().is_empty()
    {
        return (family.trim().to_string(), size);
    }
    (trimmed.to_string(), 10.0)
}

impl Config {
    /// Every module in the config, wherever it sits.
    pub fn modules(&self) -> impl Iterator<Item = &Module> {
        self.positions.iter().flatten().flat_map(|g| &g.modules)
    }

    /// The collectors this config needs, each at the shortest interval any module asked
    /// it for. Two modules showing the same thing are read once.
    pub fn collectors(&self) -> HashMap<Which, Duration> {
        let mut wanted: HashMap<Which, Duration> = HashMap::new();
        for module in self.modules() {
            let Source::Native(which) = &module.source else {
                continue;
            };
            let interval = module.interval.unwrap_or_else(|| which.default_interval());
            wanted
                .entry(which.clone())
                .and_modify(|current| *current = (*current).min(interval))
                .or_insert(interval);
        }
        wanted
    }

    /// Which sources each realtime signal reads again, by offset from SIGRTMIN.
    ///
    /// One signal may refresh several sources, and several modules may share one signal;
    /// what is refreshed is the source behind them, since that is what does the reading.
    pub fn signals(&self) -> HashMap<i32, Vec<Which>> {
        let mut wanted: HashMap<i32, Vec<Which>> = HashMap::new();
        for module in self.modules() {
            let (Some(offset), Source::Native(which)) = (module.signal, &module.source) else {
                continue;
            };
            let sources = wanted.entry(offset).or_default();
            if !sources.contains(which) {
                sources.push(which.clone());
            }
        }
        wanted
    }

    /// Whether anything on the bar shows the keyboard layout.
    ///
    /// The compositor is only asked about input devices when there is something to draw
    /// the answer, so a bar without a language module never asks.
    pub fn needs_language(&self) -> bool {
        self.modules()
            .any(|m| matches!(m.source, Source::SwayLanguage(_)))
    }

    /// Whether anything on the bar draws the compositor's binding mode.
    pub fn needs_mode(&self) -> bool {
        self.modules().any(|m| m.source == Source::SwayMode)
    }

    /// Whether anything in this config comes from an external status provider.
    ///
    /// Nothing does on a native configuration, and then there is no child process to run.
    pub fn needs_provider(&self) -> bool {
        self.positions.iter().flatten().any(|group| {
            group.wildcard || group.modules.iter().any(|m| m.source == Source::Provider)
        })
    }

    pub fn parse(text: &str) -> Result<Config> {
        let raw: RawConfig = toml::from_str(text).context("parsing config")?;
        let palette = Palette::new(&raw.colors)?;

        let (font_family, font_size) = parse_font(&raw.bar.font);
        let bar = Bar {
            height: raw.bar.height.max(1),
            position: raw.bar.position,
            layer: raw.bar.layer,
            margin: raw.bar.margin,
            gap: raw.bar.gap,
            font_family,
            font_size,
            icon_size: raw
                .bar
                .icon_size
                .map(|v| v.max(0.0))
                .unwrap_or(font_size * ICON_SIZE_RATIO),
            background: match &raw.bar.background.color {
                Some(c) => palette.get(c)?,
                None => Color::TRANSPARENT,
            },
            radius: raw.bar.background.radius,
            exclusive: raw.bar.exclusive,
        };

        // Named styles resolve against the built-in defaults, once.
        let base = base_style(bar.icon_size);
        let mut styles: HashMap<String, Style> = HashMap::new();
        for (name, raw_style) in &raw.styles {
            let style = base
                .overlay(raw_style, &palette)
                .with_context(|| format!("in [style.{name}]"))?;
            styles.insert(name.clone(), style);
        }

        let mut positions = [Vec::new(), Vec::new(), Vec::new()];
        for (slot, raw_pos) in positions
            .iter_mut()
            .zip([&raw.left, &raw.center, &raw.right])
        {
            for group_name in &raw_pos.groups {
                let raw_group = raw
                    .groups
                    .get(group_name)
                    .ok_or_else(|| anyhow!("group {group_name:?} is used but not defined"))?;
                slot.push(resolve_group(
                    group_name, raw_group, &raw, &palette, &styles, base,
                )?);
            }
        }

        Ok(Config {
            bar,
            i3bar: I3Bar {
                command: raw.i3bar.command.clone(),
                args: raw.i3bar.args.clone(),
                names: raw.i3bar.names.clone(),
            },
            positions,
        })
    }

    pub fn load(path: Option<&Path>) -> Result<Config> {
        match path {
            Some(p) => {
                let text = std::fs::read_to_string(p)
                    .with_context(|| format!("reading {}", p.display()))?;
                Config::parse(&text).with_context(|| format!("in {}", p.display()))
            }
            None => match default_config_path() {
                Some(p) if p.exists() => {
                    let text = std::fs::read_to_string(&p)
                        .with_context(|| format!("reading {}", p.display()))?;
                    log::info!("using config {}", p.display());
                    Config::parse(&text).with_context(|| format!("in {}", p.display()))
                }
                _ => {
                    log::info!("no config file found, using built-in defaults");
                    Config::parse(DEFAULT_CONFIG)
                }
            },
        }
    }
}

/// How many realtime signals this system has above SIGRTMIN.
///
/// The range is decided by the C library rather than fixed, because the first few are
/// reserved for the threading implementation.
pub fn signal_range() -> i32 {
    (libc::SIGRTMAX() - libc::SIGRTMIN()).max(0)
}

/// The names a config uses for how a source rates itself.
fn parse_state(name: &str) -> Result<State> {
    Ok(match name {
        "idle" => State::Idle,
        "info" => State::Info,
        "good" => State::Good,
        "warning" => State::Warning,
        "critical" => State::Critical,
        "error" => State::Error,
        other => {
            bail!("unknown state {other:?}; expected idle, info, good, warning, critical or error")
        }
    })
}

/// Parse a duration written the way a person would: "500ms", "2s", "1m", "1h".
///
/// A bare number is refused. `interval = 2` reads as two of something, and which something
/// is exactly the thing worth being explicit about.
/// What a module can be made to change, if anything.
///
/// dbar changes these itself rather than running a helper: it already knows where the
/// brightness lives and holds the connection the volume travels over, so shelling out to
/// a program that does the same thing would be a slower way to be less sure it worked.
pub fn control_of(source: &Source) -> Option<Control> {
    match source {
        Source::Native(Which::Backlight) => Some(Control::Brightness),
        Source::Native(Which::Audio) => Some(Control::Volume),
        _ => None,
    }
}

/// A step written as a percentage: "5%", or "5" for the same thing.
///
/// It is a share of the whole range rather than of the current value, because a scroll
/// that moves less the darker it gets never reaches either end.
fn parse_percent(written: &str) -> Result<f64> {
    let text = written.trim().strip_suffix('%').unwrap_or(written.trim());
    let step: f64 = text
        .parse()
        .with_context(|| format!("{written:?} is not a percentage like \"5%\""))?;
    if !(step.is_finite() && step > 0.0 && step <= 100.0) {
        bail!("a scroll step is between 0 and 100 percent, not {written:?}");
    }
    Ok(step)
}

fn parse_duration(written: &str) -> Result<Duration> {
    let text = written.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .ok_or_else(|| anyhow!("{text:?} needs a unit: try {text:?}s, or ms, m or h"))?;
    let (number, unit) = text.split_at(split);
    let value: f64 = number
        .parse()
        .map_err(|_| anyhow!("{number:?} in {text:?} is not a number"))?;
    if !value.is_finite() || value <= 0.0 {
        bail!("{text:?} must be a positive length of time");
    }
    let seconds = match unit.trim() {
        "ms" => value / 1000.0,
        "s" => value,
        "m" => value * 60.0,
        "h" => value * 3600.0,
        other => bail!("unknown unit {other:?} in {text:?}; use ms, s, m or h"),
    };
    Ok(Duration::from_secs_f64(seconds))
}

/// Work out where a module's content comes from.
///
/// Two sources are pointed at something - a filesystem, an interface - and take that from
/// the module's own keys rather than from the source name, so the name stays a plain word
/// and the parameter reads as what it is.
fn resolve_source(module_name: &str, raw: Option<&RawModule>) -> Result<Source> {
    let name = raw.and_then(|m| m.source.as_deref()).unwrap_or("provider");
    let source = match name {
        "provider" => Source::Provider,
        "sway:window" => Source::SwayWindow,
        "sway:workspaces" => Source::SwayWorkspaces,
        "sway:language" => Source::SwayLanguage(raw.map(|m| m.layouts.clone()).unwrap_or_default()),
        "sway:mode" => Source::SwayMode,
        "audio" => Source::Native(Which::Audio),
        "media" => Source::Native(Which::Media),
        "cpu" => Source::Native(Which::Cpu),
        "memory" => Source::Native(Which::Memory),
        "battery" => Source::Native(Which::Battery),
        "backlight" => Source::Native(Which::Backlight),
        "load" => Source::Native(Which::Load),
        "time" => Source::Native(Which::Time),
        "temperature" => Source::Native(Which::Temperature(raw.and_then(|m| m.chip.clone()))),
        // A disk module has to be pointed at something, and the root filesystem is what
        // one is usually about.
        "disk" => Source::Native(Which::Disk(
            raw.and_then(|m| m.path.clone())
                .unwrap_or_else(|| "/".to_string()),
        )),
        "network" => Source::Native(Which::Network(raw.and_then(|m| m.interface.clone()))),
        other => bail!(
            "module {module_name:?} has unknown source {other:?}; expected one of cpu, \
             memory, battery, backlight, load, temperature, disk, network, time, provider, \
             sway:window, sway:workspaces or sway:language"
        ),
    };

    // A key that belongs to a source this module is not built on would silently do
    // nothing, and silently doing nothing is how a config comes to be wrong for months.
    let misplaced = [
        ("path", raw.is_some_and(|m| m.path.is_some()), "disk"),
        (
            "interface",
            raw.is_some_and(|m| m.interface.is_some()),
            "network",
        ),
        ("chip", raw.is_some_and(|m| m.chip.is_some()), "temperature"),
        (
            "layouts",
            raw.is_some_and(|m| !m.layouts.is_empty()),
            "sway:language",
        ),
    ];
    for (key, given, belongs_to) in misplaced {
        if given && name != belongs_to {
            bail!("module {module_name:?} sets `{key}`, which only a {belongs_to} module reads");
        }
    }
    Ok(source)
}

/// Parse a module's format and check it against what its source can publish.
///
/// Checking here means a typo in a field name is a message when dbar starts, rather than a
/// module that silently says nothing.
fn resolve_format(source: &Source, written: Option<&str>) -> Result<Format> {
    let format = Format::parse(written.unwrap_or_else(|| source.default_format()))?;
    format.check(source.fields())?;
    Ok(format)
}

fn resolve_group(
    name: &str,
    raw_group: &RawGroup,
    raw: &RawConfig,
    palette: &Palette,
    styles: &HashMap<String, Style>,
    base: Style,
) -> Result<Group> {
    let wildcard = raw_group.modules.iter().any(|m| m == "*");
    let mut modules = Vec::new();
    for module_name in raw_group.modules.iter().filter(|m| *m != "*") {
        let raw_module = raw.modules.get(module_name);
        let source = resolve_source(module_name, raw_module)?;

        let style = match raw.modules.get(module_name) {
            Some(raw_module) => {
                let start = match &raw_module.style {
                    Some(style_name) => *styles.get(style_name).ok_or_else(|| {
                        anyhow!("module {module_name:?} references unknown style {style_name:?}")
                    })?,
                    None => base,
                };
                start
                    .overlay(&raw_module.overrides, palette)
                    .with_context(|| format!("in [module.{module_name}]"))?
            }
            // A module listed in a group but never configured still renders with defaults.
            None => base,
        };

        // A state applies the named style's own keys over the module's, rather than
        // replacing it wholesale, so per-module settings such as the icon survive.
        let mut states = Vec::new();
        if let Some(raw_module) = raw.modules.get(module_name) {
            for (state_name, raw_state) in &raw_module.states {
                let mut state_style = style;
                if let Some(style_name) = &raw_state.style {
                    let named = raw.styles.get(style_name).ok_or_else(|| {
                        anyhow!(
                            "state {state_name:?} of module {module_name:?} references \
                             unknown style {style_name:?}"
                        )
                    })?;
                    state_style = state_style.overlay(named, palette).with_context(|| {
                        format!("in [module.{module_name}.states.{state_name}]")
                    })?;
                }
                let state_style = state_style
                    .overlay(&raw_state.overrides, palette)
                    .with_context(|| format!("in [module.{module_name}.states.{state_name}]"))?;
                let rule_state = match &raw_state.state {
                    Some(name) => Some(parse_state(name).with_context(|| {
                        format!("in [module.{module_name}.states.{state_name}]")
                    })?),
                    None => None,
                };
                // A field a rule reads has to be one this source publishes, and has to hold
                // the kind of thing the rule asks of it, or the rule could never fire.
                if let Some(field) = &raw_state.field {
                    let known = source.fields().iter().find(|f| f.name == field);
                    let Some(kind) = known.map(|f| f.kind) else {
                        bail!(
                            "[module.{module_name}.states.{state_name}] keys on ${field}, \
                             which this module's source does not publish"
                        );
                    };
                    let wants_number = raw_state.above.is_some() || raw_state.below.is_some();
                    let wants_text = raw_state.equals.is_some();
                    if wants_number && !matches!(kind, crate::status::Kind::Num(_)) {
                        bail!(
                            "[module.{module_name}.states.{state_name}] compares ${field} \
                             against a number, but it is {}",
                            kind.describe()
                        );
                    }
                    if wants_text && !matches!(kind, crate::status::Kind::Text) {
                        bail!(
                            "[module.{module_name}.states.{state_name}] compares ${field} \
                             against a word, but it is {}",
                            kind.describe()
                        );
                    }
                    if !wants_number && !wants_text {
                        bail!(
                            "[module.{module_name}.states.{state_name}] names ${field} but \
                             says nothing about it; add `above`, `below` or `equals`"
                        );
                    }
                }
                // Matching on text is what a rule has to do when text is all there is. A
                // native source publishes values, so a rule that reads its wording would
                // be reading the format's output rather than what was measured.
                if (raw_state.contains.is_some() || raw_state.strip)
                    && !matches!(source, Source::Provider)
                {
                    bail!(
                        "[module.{module_name}.states.{state_name}] matches on text, which \
                         only makes sense for a module fed by an external provider; key on \
                         a value or on `state` instead"
                    );
                }

                states.push(StateRule {
                    urgent: raw_state.urgent,
                    hover: raw_state.hover,
                    focused: raw_state.focused,
                    visible: raw_state.visible,
                    state: rule_state,
                    field: raw_state.field.clone(),
                    equals: raw_state.equals.clone(),
                    contains: raw_state.contains.clone(),
                    strip: raw_state.strip,
                    below: raw_state.below,
                    above: raw_state.above,
                    style: state_style,
                });
            }
        }
        // Tightest bound first, so "below 15" wins over "below 30".
        states.sort_by(|a, b| {
            a.specificity()
                .partial_cmp(&b.specificity())
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let interval = match raw_module.and_then(|m| m.interval.as_deref()) {
            Some(written) => Some(
                parse_duration(written)
                    .with_context(|| format!("in [module.{module_name}] interval"))?,
            ),
            // Only dbar's own collectors are on a schedule dbar controls.
            None => match &source {
                Source::Native(which) => Some(which.default_interval()),
                _ => None,
            },
        };
        if interval.is_some() && !matches!(source, Source::Native(_)) {
            bail!(
                "module {module_name:?} sets an interval, but its source is not one dbar \
                 reads; how often it updates is the provider's own business"
            );
        }

        let signal = raw_module.and_then(|m| m.signal);
        if let Some(offset) = signal {
            if !matches!(source, Source::Native(_)) {
                bail!(
                    "module {module_name:?} sets a signal, but its source is not one dbar \
                     reads; a provider handles its own signals"
                );
            }
            let highest = signal_range();
            if offset < 0 || offset > highest {
                bail!(
                    "module {module_name:?} asks for signal {offset}, but only 0 to {highest} \
                     exist on this system; they are counted from SIGRTMIN"
                );
            }
        }

        let scroll = match raw_module.and_then(|m| m.scroll.as_deref()) {
            Some(written) => Some(
                parse_percent(written)
                    .with_context(|| format!("in [module.{module_name}] scroll"))?,
            ),
            None => None,
        };
        // Folding a module with no icon leaves an empty box on the bar, and no way back:
        // there would be nothing left to click on.
        let collapsible = raw_module.and_then(|m| m.collapsible).unwrap_or(false);
        if collapsible && style.icon.is_none() {
            bail!(
                "module {module_name:?} is collapsible but has no icon; folded down it \
                 would leave nothing to see or click"
            );
        }

        let controls = raw_module.and_then(|m| m.controls).unwrap_or(false);
        let control = match (scroll, controls) {
            (Some(_), true) => bail!(
                "module {module_name:?} sets both scroll and controls; a player is operated \
                 by its buttons, and a step means nothing to it"
            ),
            (Some(step), false) => match control_of(&source) {
                Some(what) => Some((what, step)),
                None => bail!(
                    "module {module_name:?} asks to be scrolled, but dbar can only change \
                     what it can also set: a backlight or the volume"
                ),
            },
            (None, true) => match source {
                Source::Native(Which::Media) => Some((Control::Media, 0.0)),
                _ => bail!(
                    "module {module_name:?} asks for controls, which only a media module \
                     has; a backlight or the volume takes scroll instead"
                ),
            },
            (None, false) => None,
        };

        let format = resolve_format(&source, raw_module.and_then(|m| m.format.as_deref()))
            .with_context(|| format!("in [module.{module_name}] format"))?;

        let mut format_alt = Vec::new();
        for (index, written) in raw_module
            .and_then(|m| m.format_alt.as_ref())
            .map(RawAlt::written)
            .unwrap_or_default()
            .iter()
            .enumerate()
        {
            format_alt.push(resolve_format(&source, Some(written)).with_context(|| {
                match index {
                    0 => format!("in [module.{module_name}] format_alt"),
                    // Named by position, since a list of wordings has no other name to
                    // give the one that will not parse.
                    _ => format!(
                        "in [module.{module_name}] format_alt, wording {}",
                        index + 1
                    ),
                }
            })?);
        }

        modules.push(Module {
            name: module_name.clone(),
            source,
            interval,
            signal,
            control,
            collapsible,
            format,
            format_alt,
            style,
            states,
        });
    }

    // Wildcard groups need a style for blocks that have no `[module.*]` table.
    let fallback = styles.get("default").copied().unwrap_or(base);

    let separator = match &raw_group.separator {
        Some(raw) => Separator {
            shape: raw.shape,
            width: raw.width.max(0.0),
            direction: raw.direction,
            color: parse_separator_color(&raw.color, palette)
                .with_context(|| format!("in [group.{name}.separator]"))?,
            overlap: raw.overlap.max(0.0),
        },
        None => Separator::default(),
    };

    let ends = match &raw_group.ends {
        Some(raw) => Ends {
            left: raw.left,
            right: raw.right,
            width: raw.width.unwrap_or(separator.width).max(0.0),
            overlap: raw.overlap.unwrap_or(separator.overlap).max(0.0),
        },
        None => Ends::default(),
    };

    let edges = match &raw_group.edges {
        Some(raw) => Edges {
            left: raw.left,
            right: raw.right,
            radius: raw.radius.unwrap_or(raw_group.radius),
        },
        // Without an [edges] table both corners simply use the group radius.
        None => Edges {
            left: EdgeShape::Round,
            right: EdgeShape::Round,
            radius: raw_group.radius,
        },
    };

    let opacity = raw_group.opacity.unwrap_or(1.0);
    if !(0.0..=1.0).contains(&opacity) {
        bail!("in [group.{name}]: opacity is {opacity}, but it has to be between 0.0 and 1.0");
    }

    Ok(Group {
        background: match &raw_group.background {
            Some(c) => palette
                .get(c)
                .with_context(|| format!("in [group.{name}]"))?,
            None => Color::TRANSPARENT,
        },
        opacity,
        padding: raw_group.padding,
        spacing: raw_group.spacing,
        separator,
        edges,
        ends,
        wildcard,
        modules: if wildcard && modules.is_empty() {
            vec![Module {
                name: "*".to_string(),
                source: Source::Provider,
                interval: None,
                signal: None,
                control: None,
                collapsible: false,
                format: resolve_format(&Source::Provider, None)?,
                format_alt: Vec::new(),
                style: fallback,
                states: Vec::new(),
            }]
        } else {
            modules
        },
    })
}

/// `previous`, `next`, `foreground` and `background` name a source; anything else is
/// taken as a literal colour or a `$name` reference.
fn parse_separator_color(spec: &str, palette: &Palette) -> Result<SeparatorColor> {
    Ok(match spec {
        "previous" => SeparatorColor::Previous,
        "next" => SeparatorColor::Next,
        "foreground" => SeparatorColor::Foreground,
        "background" => SeparatorColor::Background,
        other => SeparatorColor::Fixed(palette.get(other)?),
    })
}

pub fn default_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("dbar").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_in_default_config_parses() {
        Config::parse(DEFAULT_CONFIG).expect("the compiled-in default must parse");
    }

    #[test]
    fn a_format_naming_an_unknown_field_is_rejected() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
format = "$nope"
"##;
        let e = Config::parse(config).expect_err("an unknown field must be reported");
        let message = format!("{e:#}");
        assert!(message.contains("[module.cpu]"), "{message}");
        assert!(message.contains("$nope"), "{message}");
    }

    #[test]
    fn a_format_is_checked_against_the_source_it_reads() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["win"]

[module.win]
source = "sway:window"
format = "$text"
"##;
        // `$text` is the provider's field; the window module publishes `$title`.
        let e = Config::parse(config).expect_err("the wrong source's field must be reported");
        assert!(format!("{e:#}").contains("title"), "{e:#}");
    }

    #[test]
    fn a_layout_mapping_belongs_to_the_module_that_reads_it() {
        let config = |source: &str| {
            format!(
                r##"
[left]
groups = ["g"]

[group.g]
modules = ["lang"]

[module.lang]
source = "{source}"

[module.lang.layouts]
"English (US)" = "EN"
"##
            )
        };

        let parsed = Config::parse(&config("sway:language")).expect("parses");
        assert!(parsed.needs_language());
        let Source::SwayLanguage(layouts) = &parsed.modules().next().expect("one module").source
        else {
            panic!("the module should read the compositor's keyboard layout");
        };
        assert_eq!(layouts["English (US)"], "EN");

        // On anything else the table would quietly do nothing, which is how a config comes
        // to be wrong for months.
        let e = Config::parse(&config("cpu")).expect_err("a misplaced mapping must be reported");
        assert!(format!("{e:#}").contains("layouts"), "{e:#}");
    }

    #[test]
    fn a_bar_without_a_language_module_never_asks_for_one() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["win"]

[module.win]
source = "sway:window"
"##;
        assert!(!Config::parse(config).expect("parses").needs_language());
    }

    #[test]
    fn durations_need_a_unit() {
        assert_eq!(parse_duration("2s").unwrap(), Duration::from_secs(2));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("1m").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_duration(" 1h ").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("0.5s").unwrap(), Duration::from_millis(500));

        // "2" reads as two of something, and which something is the point.
        assert!(parse_duration("2").is_err());
        assert!(parse_duration("2w").is_err());
        assert!(parse_duration("0s").is_err());
        assert!(parse_duration("-1s").is_err());
        assert!(parse_duration("s").is_err());
    }

    #[test]
    fn a_collector_is_read_once_however_many_modules_show_it() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["a", "b"]

[module.a]
source = "cpu"
interval = "5s"

[module.b]
source = "cpu"
interval = "1s"
"##;
        let collectors = Config::parse(config).expect("parses").collectors();
        assert_eq!(collectors.len(), 1);
        // The shortest interval anyone asked for wins, so nobody waits longer than they said.
        assert_eq!(collectors[&Which::Cpu], Duration::from_secs(1));
    }

    #[test]
    fn a_native_config_needs_no_provider() {
        let native = r##"
[left]
groups = ["g"]

[group.g]
modules = ["clock"]

[module.clock]
source = "time"
"##;
        assert!(!Config::parse(native).expect("parses").needs_provider());
        // The built-in default reads everything itself, so it starts nothing.
        assert!(
            !Config::parse(DEFAULT_CONFIG)
                .expect("parses")
                .needs_provider()
        );

        let external = r##"
[left]
groups = ["g"]

[group.g]
modules = ["net"]
"##;
        assert!(Config::parse(external).expect("parses").needs_provider());
    }

    #[test]
    fn an_interval_on_a_source_dbar_does_not_read_is_rejected() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
interval = "1s"
"##;
        let e = Config::parse(config).expect_err("an interval on a provider module is a mistake");
        assert!(format!("{e:#}").contains("interval"), "{e:#}");
    }

    #[test]
    fn an_unknown_source_says_what_the_sources_are() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["x"]

[module.x]
source = "nonesuch"
"##;
        let e = Config::parse(config).expect_err("an unknown source is a mistake");
        let message = format!("{e:#}");
        assert!(message.contains("nonesuch"), "{message}");
        assert!(message.contains("cpu"), "{message}");
    }

    #[test]
    fn matching_on_text_is_only_for_provider_modules() {
        let native = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
source = "cpu"

[module.cpu.states.busy]
contains = "busy"
"##;
        let e = Config::parse(native).expect_err("a native source publishes values, not wording");
        assert!(format!("{e:#}").contains("text"), "{e:#}");

        // The same rule is exactly right for a module fed rendered text.
        let provider = native.replace("source = \"cpu\"\n", "");
        assert!(Config::parse(&provider).is_ok());
    }

    #[test]
    fn a_threshold_field_has_to_be_a_number_the_source_publishes() {
        let template = r##"
[left]
groups = ["g"]

[group.g]
modules = ["mem"]

[module.mem]
source = "memory"

[module.mem.states.rule]
field = "FIELD"
above = 10
"##;
        assert!(Config::parse(&template.replace("FIELD", "swap_percent")).is_ok());

        let unknown = Config::parse(&template.replace("FIELD", "nonesuch"))
            .expect_err("a field the source does not publish is a mistake");
        assert!(format!("{unknown:#}").contains("nonesuch"), "{unknown:#}");

        let wrong_kind = r##"
[left]
groups = ["g"]

[group.g]
modules = ["clock"]

[module.clock]
source = "time"

[module.clock.states.rule]
field = "now"
above = 10
"##;
        let e = Config::parse(wrong_kind).expect_err("a bound on a time could never fire");
        assert!(format!("{e:#}").contains("number"), "{e:#}");
    }

    #[test]
    fn a_rule_comparing_a_word_needs_a_field_that_holds_one() {
        let template = r##"
[left]
groups = ["g"]

[group.g]
modules = ["bat"]

[module.bat]
source = "battery"

[module.bat.states.rule]
field = "FIELD"
COMPARE
"##;
        let ok = template
            .replace("FIELD", "status")
            .replace("COMPARE", "equals = \"charging\"");
        assert!(Config::parse(&ok).is_ok());

        // A word against a number, and a number against a word, are both mistakes.
        let wrong_kind = template
            .replace("FIELD", "percent")
            .replace("COMPARE", "equals = \"charging\"");
        let e = Config::parse(&wrong_kind).expect_err("percent holds no word");
        assert!(format!("{e:#}").contains("word"), "{e:#}");

        let wrong_bound = template
            .replace("FIELD", "status")
            .replace("COMPARE", "above = 10");
        let e = Config::parse(&wrong_bound).expect_err("status holds no number");
        assert!(format!("{e:#}").contains("number"), "{e:#}");

        // Naming a field and then saying nothing about it can never fire.
        let silent = template.replace("FIELD", "status").replace("COMPARE", "");
        let e = Config::parse(&silent).expect_err("a rule with no comparison is a mistake");
        assert!(format!("{e:#}").contains("equals"), "{e:#}");
    }

    #[test]
    fn state_names_are_the_ones_a_source_can_report() {
        assert_eq!(parse_state("critical").unwrap(), State::Critical);
        assert_eq!(parse_state("error").unwrap(), State::Error);
        let e = parse_state("urgent").expect_err("urgent is a flag, not a rating");
        assert!(format!("{e:#}").contains("warning"), "{e:#}");
    }

    #[test]
    fn a_second_wording_is_checked_like_the_first() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
source = "cpu"
format_alt = "$nonesuch"
"##;
        let e = Config::parse(config).expect_err("format_alt is checked too");
        let message = format!("{e:#}");
        assert!(message.contains("format_alt"), "{message}");
        assert!(message.contains("nonesuch"), "{message}");
    }

    #[test]
    fn a_module_may_have_one_further_wording_or_several() {
        let config = Config::parse(
            r#"
[right]
groups = ["g"]

[group.g]
modules = ["one", "several"]

[module.one]
source = "cpu"
format_alt = " $utilization "

[module.several]
source = "network"
format_alt = [" $down ", " $signal ", " $dbm "]
"#,
        )
        .expect("both spellings are allowed");
        let wordings = |name: &str| {
            config
                .modules()
                .find(|m| m.name == name)
                .expect("the module is there")
                .format_alt
                .len()
        };
        assert_eq!(wordings("one"), 1);
        assert_eq!(wordings("several"), 3);
    }

    #[test]
    fn a_wording_that_names_a_field_the_source_lacks_says_which_one() {
        let broken = Config::parse(
            r#"
[right]
groups = ["g"]

[group.g]
modules = ["net"]

[module.net]
source = "network"
format_alt = [" $down ", " $nonsense "]
"#,
        )
        .expect_err("the second wording names nothing");
        let message = format!("{:#}", broken);
        assert!(message.contains("wording 2"), "{message}");
        assert!(message.contains("nonsense"), "{message}");
    }

    #[test]
    fn folding_needs_something_left_to_click_on() {
        let broken = Config::parse(
            r#"
[right]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
source = "cpu"
collapsible = true
"#,
        )
        .expect_err("a module with no icon has nothing to fold down to");
        let message = format!("{broken:#}");
        assert!(message.contains("icon"), "{message}");

        Config::parse(
            r#"
[right]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
source = "cpu"
icon = "cpu"
collapsible = true
"#,
        )
        .expect("with an icon it is fine");
    }

    #[test]
    fn only_what_dbar_can_set_may_be_scrolled() {
        let broken = r#"
[right]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
source = "cpu"
scroll = "5%"
"#;
        let message = Config::parse(broken)
            .expect_err("cpu cannot be scrolled")
            .to_string();
        assert!(message.contains("cpu"), "{message}");

        let fine = r#"
[right]
groups = ["g"]

[group.g]
modules = ["light"]

[module.light]
source = "backlight"
scroll = "5%"
"#;
        let config = Config::parse(fine).expect("a backlight can be scrolled");
        let module = config
            .modules()
            .find(|m| m.name == "light")
            .expect("the module is there");
        assert_eq!(module.control, Some((Control::Brightness, 5.0)));
    }

    #[test]
    fn a_scroll_step_is_a_percentage_or_an_error_saying_so() {
        for written in ["0%", "101%", "some"] {
            assert!(parse_percent(written).is_err(), "{written} was accepted");
        }
        assert_eq!(parse_percent("5%").ok(), Some(5.0));
        assert_eq!(parse_percent(" 2.5 ").ok(), Some(2.5));
    }

    #[test]
    fn a_signal_names_the_sources_it_reads_again() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["light", "light2", "cpu"]

[module.light]
source = "backlight"
signal = 8

[module.light2]
source = "backlight"
signal = 8

[module.cpu]
source = "cpu"
signal = 9
"##;
        let signals = Config::parse(config).expect("parses").signals();
        // Two modules on one source and one signal is still one source to read.
        assert_eq!(signals[&8], vec![Which::Backlight]);
        assert_eq!(signals[&9], vec![Which::Cpu]);
    }

    #[test]
    fn a_signal_outside_the_realtime_range_is_rejected() {
        let template = r##"
[left]
groups = ["g"]

[group.g]
modules = ["light"]

[module.light]
source = "backlight"
signal = N
"##;
        assert!(Config::parse(&template.replace("N", "0")).is_ok());
        assert!(Config::parse(&template.replace("N", "-1")).is_err());
        let too_high = (signal_range() + 1).to_string();
        let e = Config::parse(&template.replace("N", &too_high))
            .expect_err("a signal this system does not have is a mistake");
        assert!(format!("{e:#}").contains("SIGRTMIN"), "{e:#}");
    }

    #[test]
    fn a_signal_on_a_source_dbar_does_not_read_is_rejected() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
signal = 8
"##;
        let e = Config::parse(config).expect_err("a provider handles its own signals");
        assert!(format!("{e:#}").contains("signal"), "{e:#}");
    }

    #[test]
    fn the_bar_sits_above_ordinary_windows_unless_told_otherwise() {
        assert_eq!(Config::parse("").unwrap().bar.layer, BarLayer::Top);
        let config = Config::parse("[bar]\nlayer = \"bottom\"\n").unwrap();
        assert_eq!(config.bar.layer, BarLayer::Bottom);
        let e = Config::parse("[bar]\nlayer = \"above\"\n").expect_err("not a layer");
        assert!(format!("{e:#}").contains("layer"), "{e:#}");
    }

    #[test]
    fn an_island_is_all_there_unless_it_asks_not_to_be() {
        let group = |line: &str| {
            format!(
                "[right]\ngroups = [\"system\"]\n\
                 [group.system]\nmodules = [\"cpu\"]\n{line}\n\
                 [module.cpu]\nsource = \"cpu\"\n"
            )
        };
        let opacity = |toml: &str| {
            Config::parse(toml).map(|c| {
                c.positions
                    .iter()
                    .flatten()
                    .next()
                    .expect("the group was placed")
                    .opacity
            })
        };

        assert_eq!(opacity(&group("")).expect("a group needs no opacity"), 1.0);
        assert_eq!(opacity(&group("opacity = 0.5")).expect("half is fine"), 0.5);

        // Named, and pointing at the group, so the mistake is findable at startup rather
        // than at three in the morning.
        for bad in ["opacity = 1.8", "opacity = -0.2"] {
            let e = opacity(&group(bad)).expect_err("outside 0.0 to 1.0");
            let message = format!("{e:#}");
            assert!(message.contains("opacity"), "{message}");
            assert!(message.contains("group.system"), "{message}");
        }
    }

    #[test]
    fn every_shipped_example_parses() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/examples");
        for entry in std::fs::read_dir(dir).expect("examples/ is readable") {
            let path = entry.expect("a readable directory entry").path();
            if path.extension().is_none_or(|e| e != "toml") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("a readable example");
            if let Err(e) = Config::parse(&text) {
                panic!("{} does not parse: {e:#}", path.display());
            }
        }
    }
}
