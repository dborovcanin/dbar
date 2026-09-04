//! Declarative TOML configuration and the style cascade.
//!
//! Parsing happens in two steps: serde fills the `raw` structs, then `resolve` turns
//! `$name` color references and style names into concrete values so that nothing downstream
//! has to do lookups while rendering.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow};
use serde::Deserialize;

use crate::color::Color;
use crate::format::Format;
use crate::icon::Icon;
use crate::status::FieldSpec;

pub const DEFAULT_CONFIG: &str = include_str!("../examples/config.toml");

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    Top,
    Bottom,
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Source {
    /// A block from the status provider, matched by name.
    #[default]
    Provider,
    /// The title of the focused window.
    SwayWindow,
    /// One entry per workspace, expanded at layout time.
    SwayWorkspaces,
}

impl Source {
    /// What a format written against this source may name.
    pub fn fields(self) -> &'static [FieldSpec] {
        match self {
            Source::Provider => crate::status::i3bar::FIELDS,
            Source::SwayWindow => crate::sway::WINDOW_FIELDS,
            Source::SwayWorkspaces => crate::sway::WORKSPACE_FIELDS,
        }
    }

    /// What the module says when the config does not give it a format.
    ///
    /// Each source has one field that is the obvious thing to show, so the common case
    /// needs no `format` line at all.
    fn default_format(self) -> &'static str {
        match self {
            Source::Provider => "$text",
            Source::SwayWindow => "$title",
            Source::SwayWorkspaces => "$name",
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
    #[serde(default)]
    status: RawStatus,
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
struct RawStatus {
    #[serde(default = "default_status_command")]
    command: String,
    #[serde(default)]
    args: Vec<String>,
    /// Names for the provider's blocks, in the order it emits them. External mode only.
    #[serde(default)]
    blocks: Vec<String>,
    /// `[[status.block]]` entries. Their presence switches to generated mode: dbar writes
    /// the provider's own configuration and starts it against that.
    #[serde(default, rename = "block")]
    block: Vec<toml::Table>,
    /// Passed through to the generated configuration verbatim.
    theme: Option<toml::Table>,
    icons: Option<toml::Table>,
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
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGroup {
    #[serde(default)]
    modules: Vec<String>,
    background: Option<String>,
    #[serde(default)]
    radius: f32,
    #[serde(default)]
    padding: f32,
    #[serde(default)]
    spacing: f32,
    separator: Option<RawSeparator>,
    edges: Option<RawEdges>,
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
    /// Where the content comes from: the provider, or the compositor.
    source: Option<String>,
    /// What the module says, written against the source's fields.
    format: Option<String>,
    /// Conditional restyling, keyed on the block's value or its urgent flag.
    #[serde(default)]
    states: HashMap<String, RawState>,
    #[serde(flatten)]
    overrides: RawStyle,
}

#[derive(Debug, Default, Deserialize)]
struct RawState {
    /// Name of a `[style.*]` table whose keys are applied over the module's own.
    style: Option<String>,
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
fn default_gap() -> f32 {
    6.0
}
fn default_font() -> String {
    "sans-serif 10".to_string()
}
fn default_true() -> bool {
    true
}
fn default_status_command() -> String {
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
            margin: 0,
            gap: default_gap(),
            font: default_font(),
            icon_size: None,
            background: RawBarBackground::default(),
            exclusive: true,
        }
    }
}

impl Default for RawStatus {
    fn default() -> Self {
        RawStatus {
            command: default_status_command(),
            args: Vec::new(),
            blocks: Vec::new(),
            block: Vec::new(),
            theme: None,
            icons: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Resolved
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Config {
    pub bar: Bar,
    pub status: Status,
    /// Groups per position, in `POSITIONS` order.
    pub positions: [Vec<Group>; 3],
}

#[derive(Debug, Clone)]
pub struct Bar {
    pub height: u32,
    pub position: Edge,
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

#[derive(Debug, Clone)]
pub struct Status {
    pub command: String,
    pub args: Vec<String>,
    /// Stable names for the provider's blocks, by position.
    ///
    /// The i3bar protocol has no way for a provider to name its blocks usefully -
    /// i3status-rs numbers them - so groups would otherwise have to select on "0", "1",
    /// and silently follow the wrong block whenever the provider's order changed.
    pub blocks: Vec<String>,
    /// Set when dbar writes the provider's configuration itself.
    pub generated: Option<Generated>,
}

/// A provider configuration dbar writes and owns.
///
/// Block bodies are carried as opaque tables, so dbar never models the provider's schema
/// and does not drift as that schema changes.
#[derive(Debug, Clone)]
pub struct Generated {
    pub theme: Option<toml::Table>,
    pub icons: Option<toml::Table>,
    pub blocks: Vec<toml::Table>,
}

impl Generated {
    /// Render the provider's configuration file.
    pub fn to_toml(&self) -> Result<String> {
        let mut doc = toml::Table::new();
        if let Some(theme) = &self.theme {
            doc.insert("theme".to_string(), toml::Value::Table(theme.clone()));
        }
        if let Some(icons) = &self.icons {
            doc.insert("icons".to_string(), toml::Value::Table(icons.clone()));
        }
        doc.insert(
            "block".to_string(),
            toml::Value::Array(
                self.blocks
                    .iter()
                    .cloned()
                    .map(toml::Value::Table)
                    .collect(),
            ),
        );
        toml::to_string_pretty(&doc).context("rendering the generated provider config")
    }
}

#[derive(Debug, Clone)]
pub struct Group {
    pub background: Color,
    pub padding: f32,
    pub spacing: f32,
    pub separator: Separator,
    pub edges: Edges,
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
    /// What the module says, already parsed and checked against the source's fields.
    pub format: Format,
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
}

/// One conditional restyling of a module.
#[derive(Debug, Clone)]
pub struct StateRule {
    pub urgent: bool,
    pub hover: bool,
    pub focused: bool,
    pub visible: bool,
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
    pub fn matches(
        &self,
        flags: StateFlags,
        hovered: bool,
        value: Option<f64>,
        text: &str,
    ) -> bool {
        if self.urgent && !flags.urgent {
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
            || self.contains.is_some()
            || self.below.is_some()
            || self.above.is_some()
    }

    /// How specific the rule is, for ordering. Urgent first, then the tightest bound; a
    /// rule keyed only on hover carries no bound and so sorts last, leaving a warning or a
    /// critical state visible while the pointer is over it.
    fn specificity(&self) -> (bool, f32) {
        (
            !(self.urgent || self.focused || self.contains.is_some()),
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
}

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
    pub fn parse(text: &str) -> Result<Config> {
        let raw: RawConfig = toml::from_str(text).context("parsing config")?;
        let palette = Palette::new(&raw.colors)?;

        let (font_family, font_size) = parse_font(&raw.bar.font);
        let bar = Bar {
            height: raw.bar.height.max(1),
            position: raw.bar.position,
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
            status: resolve_status(&raw.status)?,
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

/// Parse a module's format and check it against what its source can publish.
///
/// Checking here means a typo in a field name is a message when dbar starts, rather than a
/// module that silently says nothing.
fn resolve_format(source: Source, written: Option<&str>) -> Result<Format> {
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
                states.push(StateRule {
                    urgent: raw_state.urgent,
                    hover: raw_state.hover,
                    focused: raw_state.focused,
                    visible: raw_state.visible,
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

        let source = match raw
            .modules
            .get(module_name)
            .and_then(|m| m.source.as_deref())
        {
            None | Some("provider") => Source::Provider,
            Some("sway:window") => Source::SwayWindow,
            Some("sway:workspaces") => Source::SwayWorkspaces,
            Some(other) => anyhow::bail!(
                "module {module_name:?} has unknown source {other:?}; expected \"provider\", \
                 \"sway:window\" or \"sway:workspaces\""
            ),
        };

        let format = resolve_format(
            source,
            raw.modules
                .get(module_name)
                .and_then(|m| m.format.as_deref()),
        )
        .with_context(|| format!("in [module.{module_name}] format"))?;

        modules.push(Module {
            name: module_name.clone(),
            source,
            format,
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

    Ok(Group {
        background: match &raw_group.background {
            Some(c) => palette
                .get(c)
                .with_context(|| format!("in [group.{name}]"))?,
            None => Color::TRANSPARENT,
        },
        padding: raw_group.padding,
        spacing: raw_group.spacing,
        separator,
        edges,
        wildcard,
        modules: if wildcard && modules.is_empty() {
            vec![Module {
                name: "*".to_string(),
                source: Source::Provider,
                format: resolve_format(Source::Provider, None)?,
                style: fallback,
                states: Vec::new(),
            }]
        } else {
            modules
        },
    })
}

/// Split `[status]` into the two provider modes.
fn resolve_status(raw: &RawStatus) -> Result<Status> {
    if raw.block.is_empty() {
        return Ok(Status {
            command: raw.command.clone(),
            args: raw.args.clone(),
            blocks: raw.blocks.clone(),
            generated: None,
        });
    }

    if !raw.blocks.is_empty() {
        anyhow::bail!(
            "[status] sets both `blocks` and [[status.block]]; the first names an external \
             provider's blocks, the second declares them, so only one applies"
        );
    }
    if !raw.args.is_empty() {
        anyhow::bail!(
            "[status] sets both `args` and [[status.block]]; dbar passes the generated \
             config path as the only argument, so `args` would be ignored"
        );
    }

    // Each block's `name` is dbar's handle for it; everything else belongs to the provider.
    let mut names = Vec::with_capacity(raw.block.len());
    let mut blocks = Vec::with_capacity(raw.block.len());
    for (index, table) in raw.block.iter().enumerate() {
        let mut table = table.clone();
        let name = match table.remove("name") {
            Some(toml::Value::String(name)) => name,
            Some(other) => anyhow::bail!(
                "[[status.block]] #{} has a non-string name {other}",
                index + 1
            ),
            None => anyhow::bail!(
                "[[status.block]] #{} needs a `name`, which is how groups refer to it",
                index + 1
            ),
        };
        if names.contains(&name) {
            anyhow::bail!("two [[status.block]] entries are both named {name:?}");
        }
        names.push(name);
        blocks.push(table);
    }

    Ok(Status {
        command: raw.command.clone(),
        args: Vec::new(),
        blocks: names,
        generated: Some(Generated {
            theme: raw.theme.clone(),
            icons: raw.icons.clone(),
            blocks,
        }),
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
