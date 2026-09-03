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
    pub background: Color,
    pub radius: f32,
    pub exclusive: bool,
}

#[derive(Debug, Clone)]
pub struct Status {
    pub command: String,
    pub args: Vec<String>,
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
    pub style: Style,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Style {
    pub background: Color,
    pub foreground: Color,
    pub padding: f32,
    pub radius: f32,
    pub min_width: f32,
}

impl Default for Style {
    fn default() -> Self {
        Style {
            background: Color::TRANSPARENT,
            foreground: Color::rgba(0xcd, 0xd6, 0xf4, 0xff),
            padding: 8.0,
            radius: 0.0,
            min_width: 0.0,
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
            background: match &raw.bar.background.color {
                Some(c) => palette.get(c)?,
                None => Color::TRANSPARENT,
            },
            radius: raw.bar.background.radius,
            exclusive: raw.bar.exclusive,
        };

        // Named styles resolve against the built-in defaults, once.
        let mut styles: HashMap<String, Style> = HashMap::new();
        for (name, raw_style) in &raw.styles {
            let style = Style::default()
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
                    group_name, raw_group, &raw, &palette, &styles,
                )?);
            }
        }

        Ok(Config {
            bar,
            status: Status {
                command: raw.status.command.clone(),
                args: raw.status.args.clone(),
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

fn resolve_group(
    name: &str,
    raw_group: &RawGroup,
    raw: &RawConfig,
    palette: &Palette,
    styles: &HashMap<String, Style>,
) -> Result<Group> {
    let wildcard = raw_group.modules.iter().any(|m| m == "*");
    let mut modules = Vec::new();
    for module_name in raw_group.modules.iter().filter(|m| *m != "*") {
        let style = match raw.modules.get(module_name) {
            Some(raw_module) => {
                let base = match &raw_module.style {
                    Some(style_name) => *styles.get(style_name).ok_or_else(|| {
                        anyhow!("module {module_name:?} references unknown style {style_name:?}")
                    })?,
                    None => Style::default(),
                };
                base.overlay(&raw_module.overrides, palette)
                    .with_context(|| format!("in [module.{module_name}]"))?
            }
            // A module listed in a group but never configured still renders with defaults.
            None => Style::default(),
        };
        modules.push(Module {
            name: module_name.clone(),
            style,
        });
    }

    // Wildcard groups need a style for blocks that have no `[module.*]` table.
    let fallback = styles.get("default").copied().unwrap_or_default();

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
                style: fallback,
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
