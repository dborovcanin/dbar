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
    /// The title of the focused window, on this screen or in the session.
    SwayWindow(Scope),
    /// One entry per workspace, expanded at layout time.
    SwayWorkspaces(Scope),
    /// The active keyboard layout, with the short forms the module gives its layouts.
    SwayLanguage(BTreeMap<String, String>),
    /// The binding mode the compositor is in.
    SwayMode,
    /// One entry per application in the system tray, expanded at layout time.
    Tray(TrayView),
}

/// Which tray items a module shows, and in what order.
///
/// An application decides when its icon matters, and the protocol has a word for one that
/// does not: `Passive`. Some applications sit there permanently passive, which is theirs
/// to decide and the bar's to ignore if the config says so.
///
/// Order is the other half. Items arrive as their applications start, so a bar that has
/// been up for a day is in start-up order and a bar restarted at lunch is in another one.
/// Naming the ones that matter pins those; everything else keeps arriving after them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrayView {
    pub show_passive: bool,
    /// Item ids, first to last. An id the tray does not have costs nothing.
    pub order: Vec<String>,
}

impl TrayView {
    /// Whether this item is one the config asked to see.
    pub fn shows(&self, status: crate::tray::Status) -> bool {
        self.show_passive || status != crate::tray::Status::Passive
    }

    /// Where an item sits: the position the config gave its id, or after everything named.
    pub fn rank(&self, id: &str) -> usize {
        self.order
            .iter()
            .position(|named| named == id)
            .unwrap_or(usize::MAX)
    }
}

impl Source {
    /// What a format written against this source may name.
    pub fn fields(&self) -> &'static [FieldSpec] {
        match self {
            Source::Native(which) => which.fields(),
            Source::Provider => crate::status::i3bar::FIELDS,
            Source::SwayWindow(_) => crate::sway::WINDOW_FIELDS,
            Source::SwayWorkspaces(_) => crate::sway::WORKSPACE_FIELDS,
            Source::SwayLanguage(_) => crate::sway::LANGUAGE_FIELDS,
            Source::SwayMode => crate::sway::MODE_FIELDS,
            Source::Tray(_) => crate::tray::FIELDS,
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
            Source::SwayWindow(_) => "$title",
            Source::SwayWorkspaces(_) => "$name",
            Source::SwayLanguage(_) => " $short ",
            Source::SwayMode => " $mode ",
            // A tray item is its icon; the application's name beside every one of them
            // would be a row of words where a row of pictures was asked for.
            Source::Tray(_) => "",
        }
    }
}

/// How much of the session a module drawn from the compositor is about.
///
/// A bar exists once per screen, so a workspace list is about that screen and so is the
/// window title above it. Naming the session instead gives every bar the same thing, which
/// is what a single-screen configuration always had.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Only what is on the screen this bar is on.
    #[default]
    Output,
    /// Everything, whichever screen it is on.
    Session,
}

/// The kind a command module says one of its fields will hold.
fn field_kind(name: &str) -> Result<crate::status::Kind> {
    use crate::status::{Kind, Unit};
    match name {
        "number" => Ok(Kind::Num(Unit::None)),
        "percent" => Ok(Kind::Num(Unit::Percent)),
        "text" => Ok(Kind::Text),
        other => bail!("{other:?} is not a field kind; use number, percent or text"),
    }
}

/// The word a field kind is written as in the config, for a message about two of them.
fn kind_word(kind: crate::status::Kind) -> &'static str {
    use crate::status::{Kind, Unit};
    match kind {
        Kind::Num(Unit::Percent) => "percent",
        Kind::Num(_) => "number",
        _ => "text",
    }
}

/// Give every module built on one command the fields all of them declared.
///
/// Two modules naming the same program on the same schedule share one process and one
/// reading, and the declared fields are not part of what tells two commands apart. The
/// process is spawned with one module's schema, so anything only the other module
/// declared was parsed out of the output and dropped: the module validated, ran, and drew
/// nothing. The schemas are unioned here instead, so the shared reading carries every
/// field any of them asked for.
///
/// A name two of them disagree about the kind of is a config error rather than a silent
/// choice between the two, and says which modules to look at.
fn share_command_fields(positions: &mut [Position; 3]) -> Result<()> {
    use crate::collect::{CommandSpec, Which};

    // `mixed` says whether the modules on one command declared the same schema, which is
    // the ordinary case: they then keep the slice the config already leaked for them.
    struct Union {
        fields: Vec<(FieldSpec, String)>,
        first: &'static [FieldSpec],
        mixed: bool,
    }

    let mut shared: HashMap<CommandSpec, Union> = HashMap::new();
    for module in positions
        .iter()
        .flat_map(|p| &p.groups)
        .flat_map(|g| &g.modules)
    {
        let Source::Native(Which::Command(spec)) = &module.source else {
            continue;
        };
        let entry = shared.entry(spec.clone()).or_insert_with(|| Union {
            fields: Vec::new(),
            first: spec.fields,
            mixed: false,
        });
        let same = entry.first.len() == spec.fields.len()
            && std::iter::zip(entry.first, spec.fields)
                .all(|(a, b)| a.name == b.name && a.kind == b.kind);
        entry.mixed |= !same;
        let union = &mut entry.fields;
        for declared in spec.fields {
            match union.iter().find(|(f, _)| f.name == declared.name) {
                Some((seen, first)) if seen.kind != declared.kind => bail!(
                    "modules {first:?} and {:?} run the same command on the same schedule, \
                     so they share one reading, but {first:?} declares field {:?} as {} and \
                     {:?} declares it as {}",
                    module.name,
                    declared.name,
                    kind_word(seen.kind),
                    module.name,
                    kind_word(declared.kind),
                ),
                Some(_) => {}
                None => union.push((*declared, module.name.clone())),
            }
        }
    }

    // Leaked once per shared command while the config is read, the same as the schemas it
    // is built from, so the fields stay `'static` for the thread that parses the output.
    let unions: HashMap<CommandSpec, &'static [FieldSpec]> = shared
        .into_iter()
        .filter(|(_, union)| union.mixed)
        .map(|(spec, union)| {
            let fields: Vec<FieldSpec> = union.fields.into_iter().map(|(f, _)| f).collect();
            let leaked: &'static [FieldSpec] = Box::leak(fields.into_boxed_slice());
            (spec, leaked)
        })
        .collect();

    for module in positions
        .iter_mut()
        .flat_map(|p| &mut p.groups)
        .flat_map(|g| &mut g.modules)
    {
        let Source::Native(Which::Command(spec)) = &mut module.source else {
            continue;
        };
        if let Some(fields) = unions.get(spec) {
            spec.fields = fields;
        }
    }
    Ok(())
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
    menu: RawMenu,
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
    /// Families to draw characters `font` has no glyph for. Empty means dbar chooses.
    #[serde(default)]
    fallback: Vec<String>,
    /// Base icon size; defaults to a multiple of the font size.
    icon_size: Option<f32>,
    #[serde(default)]
    background: RawBarBackground,
    /// Reserve space so windows are not covered. Defaults to on.
    #[serde(default = "default_true")]
    exclusive: bool,
    /// Which screens to appear on, named the way the compositor names them: "DP-1".
    /// Empty, or a single "*", is every screen there is and every one plugged in later.
    #[serde(default)]
    outputs: Vec<String>,
    /// The icon theme a tray item's named icon is looked for in, before the fallback theme
    /// every application installs into.
    #[serde(default = "default_icon_theme")]
    icon_theme: String,
}

/// How long a command gets to answer before it is stopped.
///
/// Long enough that a script fetching something over a slow network still gets there, and
/// short enough that one which never will does not sit in front of a spinner forever - a
/// module that is waiting animates, and animating is the one thing this bar is not
/// supposed to do at rest.
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

fn default_icon_theme() -> String {
    "hicolor".to_string()
}

/// The menu a tray item opens, as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMenu {
    /// What the menu is made of. Defaults to the bar's own background, so a menu looks
    /// like the bar it came out of without being told to.
    background: Option<String>,
    foreground: Option<String>,
    /// The row under the pointer.
    highlight: Option<String>,
    highlight_foreground: Option<String>,
    /// What a row that cannot be chosen is written in.
    disabled: Option<String>,
    separator: Option<String>,
    #[serde(default = "default_menu_padding")]
    padding: f32,
    #[serde(default = "default_menu_radius")]
    radius: f32,
    /// How wide a menu may get before its labels are cut short.
    #[serde(default = "default_menu_width")]
    max_width: f32,
}

impl Default for RawMenu {
    fn default() -> Self {
        RawMenu {
            background: None,
            foreground: None,
            highlight: None,
            highlight_foreground: None,
            disabled: None,
            separator: None,
            padding: default_menu_padding(),
            radius: default_menu_radius(),
            max_width: default_menu_width(),
        }
    }
}

fn default_menu_padding() -> f32 {
    8.0
}

fn default_menu_radius() -> f32 {
    8.0
}

fn default_menu_width() -> f32 {
    360.0
}

/// `on_click` as written: a program per button, each an argv.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawClickActions {
    #[serde(default)]
    left: Vec<String>,
    #[serde(default)]
    middle: Vec<String>,
    #[serde(default)]
    right: Vec<String>,
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
    separator: Option<RawSeparator>,
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
struct RawCollapsedGroup {
    style: Option<String>,
    #[serde(flatten)]
    overrides: RawStyle,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGroup {
    #[serde(default)]
    collapsible: bool,
    collapse_button: Option<Button>,
    collapsed: Option<RawCollapsedGroup>,
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
    /// Optional orientation for caps, independent of the internal separators.
    direction: Option<Direction>,
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
    /// How much of the session a compositor module is about: `output`, which is the screen
    /// this bar is on, or `session`. Defaults to the screen.
    scope: Option<Scope>,
    /// Whether a `tray` module draws the items whose applications say they are passive.
    /// Defaults to showing them, which is what the bar always did.
    show_passive: Option<bool>,
    /// The ids a `tray` module puts first, in the order it puts them. Everything else
    /// follows, in the order the applications registered.
    #[serde(default)]
    order: Vec<String>,
    /// A command module's argv. Executed directly: dbar never inserts a shell.
    #[serde(default)]
    command: Vec<String>,
    /// Further arguments for that command, in the order it reads them. Separate from
    /// `command` because these are the knobs: what the program is stays put while where
    /// it looks, what units it answers in and what key it uses are changed here.
    #[serde(default)]
    params: Vec<String>,
    /// Whether every line the command prints is a reading of its own, which the wheel
    /// scrolls between. Without it the last line is the answer.
    #[serde(default)]
    pages: bool,
    /// How long the command is given to answer before it is stopped: "10s", "1m".
    timeout: Option<String>,
    /// What that command publishes, and what kind each is: `number`, `percent` or `text`.
    #[serde(default)]
    fields: BTreeMap<String, String>,
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
    /// Which button moves through `format_alt`. Defaults to the left.
    alt_button: Option<Button>,
    /// Which button folds the module down to its icon. Defaults to the right.
    collapse_button: Option<Button>,
    /// Which button reads this module's source again. No default: a button is claimed
    /// only where the config asks for one.
    refresh_button: Option<Button>,
    /// Which button mutes and unmutes. Defaults to the middle, and means nothing on a
    /// module that is not showing the volume.
    mute_button: Option<Button>,
    /// What to run when this module is clicked, by button.
    on_click: Option<RawClickActions>,
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
    /// Matches when every field named here reads exactly what it is paired with, ignoring
    /// case. `field` and `equals` say one thing about one field; this says several, for the
    /// states that are a combination rather than a single reading.
    fields: Option<BTreeMap<String, String>>,
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
            fallback: Vec::new(),
            icon_size: None,
            background: RawBarBackground::default(),
            exclusive: true,
            outputs: Vec::new(),
            icon_theme: default_icon_theme(),
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
    pub menu: Menu,
    pub i3bar: I3Bar,
    /// Groups per position, in `POSITIONS` order.
    pub positions: [Position; 3],
}

/// One alignment: independent groups, optionally connected by shared separators.
#[derive(Debug, Clone, Default)]
pub struct Position {
    pub groups: Vec<Group>,
    pub separator: Option<Separator>,
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
    /// Families to try for characters the bar's own font cannot draw.
    ///
    /// Empty is not "no fallback": it means the config named none and dbar picks a short
    /// set from what is installed. Naming families here replaces that choice rather than
    /// adding to it, so a config that lists its own says exactly what the bar may load.
    pub font_fallback: Vec<String>,
    /// Icon edge length used unless a style or module overrides it.
    pub icon_size: f32,
    pub background: Color,
    pub radius: f32,
    pub exclusive: bool,
    /// The screens this bar appears on, empty for all of them.
    pub outputs: Vec<String>,
    /// Which icon theme a tray item's named icon is looked for in.
    pub icon_theme: String,
}

impl Bar {
    /// Whether this bar belongs on a screen the compositor calls `name`.
    ///
    /// An output the compositor has not named yet is taken only by a bar that asked for
    /// every screen: a list of names cannot be checked against a screen that has none.
    pub fn shows_on(&self, name: Option<&str>) -> bool {
        if self.outputs.is_empty() || self.outputs.iter().any(|o| o == "*") {
            return true;
        }
        name.is_some_and(|name| self.outputs.iter().any(|o| o == name))
    }
}

/// How the menu behind a tray icon is painted.
#[derive(Debug, Clone)]
pub struct Menu {
    pub background: Color,
    pub foreground: Color,
    pub highlight: Color,
    pub highlight_foreground: Color,
    pub disabled: Color,
    pub separator: Color,
    pub padding: f32,
    pub radius: f32,
    pub max_width: f32,
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
pub struct GroupCollapse {
    pub button: Button,
    pub style: Style,
}

#[derive(Debug, Clone)]
pub struct Group {
    pub name: String,
    pub collapse: Option<GroupCollapse>,
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
/// the end. Shapes inherit the group's separator direction unless overridden.
#[derive(Debug, Clone, Copy, Default)]
pub struct Ends {
    pub left: SeparatorShape,
    pub right: SeparatorShape,
    pub width: f32,
    pub overlap: f32,
    pub direction: Option<Direction>,
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Edges {
    pub left: EdgeShape,
    pub right: EdgeShape,
    pub radius: f32,
}

/// One of the three pointer buttons a module can be given something to do with.
///
/// Scrolling is not here: a notch is a step in a direction rather than a press, and what
/// it does belongs to `scroll` on the sources dbar can operate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Button {
    Left,
    Middle,
    Right,
}

impl Button {
    /// The i3bar protocol's number for this button, which is what click dispatch speaks.
    pub fn number(self) -> u32 {
        match self {
            Button::Left => 1,
            Button::Middle => 2,
            Button::Right => 3,
        }
    }

    /// What to call it in an error, spelled the way the config would.
    fn name(self) -> &'static str {
        match self {
            Button::Left => "left",
            Button::Middle => "middle",
            Button::Right => "right",
        }
    }
}

/// The programs a module runs when it is clicked.
///
/// dbar covers what a bar is for and no more, so a click that should do something else -
/// a calendar over the clock, a mixer over the volume - runs a program of the user's own.
/// The argv is executed directly, exactly as a `command` source is: no shell, so nothing
/// here has to think about quoting or what a stray space in a path would do.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ClickActions {
    pub left: Option<Vec<String>>,
    pub middle: Option<Vec<String>>,
    pub right: Option<Vec<String>>,
}

impl ClickActions {
    pub fn for_button(&self, button: Button) -> Option<&[String]> {
        match button {
            Button::Left => self.left.as_deref(),
            Button::Middle => self.middle.as_deref(),
            Button::Right => self.right.as_deref(),
        }
    }

    fn buttons(&self) -> impl Iterator<Item = Button> + '_ {
        [Button::Left, Button::Middle, Button::Right]
            .into_iter()
            .filter(|b| self.for_button(*b).is_some())
    }
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
    /// Which button moves through `format_alt`. Meaningless without further wordings.
    pub alt_button: Button,
    /// Which button folds the module down to its icon. Meaningless unless `collapsible`.
    pub collapse_button: Button,
    /// Which button reads this module's source again, when the config gives one that job.
    pub refresh_button: Option<Button>,
    /// Which button mutes and unmutes, for a module that operates the volume. None where
    /// there is nothing to mute, so nothing is claimed and the press falls through.
    pub mute_button: Option<Button>,
    /// Programs to run when this module is clicked.
    ///
    /// Shared rather than cloned: layout rebuilds every placed module on every redraw, and
    /// an argv copied per module per frame would be paid for on the one path that runs
    /// forever.
    pub on_click: Option<std::sync::Arc<ClickActions>>,
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
    /// Fields that all have to read what they are paired with, compared without case.
    pub fields: BTreeMap<String, String>,
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
        for (name, wanted) in &self.fields {
            match fields.get(name) {
                Some(Value::Text(said)) if said.eq_ignore_ascii_case(wanted) => {}
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
            || !self.fields.is_empty()
            || self.below.is_some()
            || self.above.is_some()
    }

    /// How specific the rule is, for ordering. Urgent first, then the tightest bound; a
    /// rule keyed only on hover carries no bound and so sorts last, leaving a warning or a
    /// critical state visible while the pointer is over it.
    fn specificity(&self) -> (std::cmp::Reverse<usize>, f32) {
        // How many things the rule insists on, so a rule that names two readings beats one
        // that names either alone: muted headphones are not just headphones, and not just
        // muted. A rule that insists on nothing categorical still comes last, which is the
        // order this has always had.
        let named = usize::from(self.urgent)
            + usize::from(self.focused)
            + usize::from(self.state.is_some())
            + usize::from(self.contains.is_some())
            + usize::from(self.equals.is_some())
            + self.fields.len();
        (
            std::cmp::Reverse(named),
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
const ICON_SIZE_RATIO: f32 = 1.6;

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
            icon_size: 16.0,
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

/// How a command module's `interval` is read.
///
/// Absent means the command streams, which is the arrangement that costs nothing while
/// nothing is happening. A period means run it that often, and `"once"` means run it at
/// startup and never again.
fn parse_run(written: Option<&str>, module: &str) -> Result<crate::collect::command::Run> {
    use crate::collect::command::Run;
    let Some(written) = written else {
        return Ok(Run::Stream);
    };
    if written.trim().eq_ignore_ascii_case("once") {
        return Ok(Run::Once);
    }
    let period = parse_duration(written).with_context(|| {
        format!("in [module.{module}] interval, which takes a period or \"once\"")
    })?;
    Ok(Run::Every(period))
}

/// Turn `on_click` as written into argvs, rejecting a button given nothing to run.
///
/// An empty list is a mistake rather than a way of saying "do nothing": the key was
/// written, so something was meant by it, and finding out at three in the morning that a
/// click does nothing is exactly what startup checking is for.
fn click_actions(raw: &RawClickActions, module: &str) -> Result<ClickActions> {
    let one = |argv: &Vec<String>, button: Button| -> Result<Option<Vec<String>>> {
        if argv.is_empty() {
            return Ok(None);
        }
        if argv[0].trim().is_empty() {
            bail!(
                "[module.{module}] on_click.{} names no program to run",
                button.name()
            );
        }
        Ok(Some(argv.clone()))
    };
    Ok(ClickActions {
        left: one(&raw.left, Button::Left)?,
        middle: one(&raw.middle, Button::Middle)?,
        right: one(&raw.right, Button::Right)?,
    })
}

/// Whether a source can be read again on demand, and what to say when it cannot.
///
/// Everything dbar reads itself can be. Of the sources that arrive rather than being read,
/// only a command can be asked, and only one that answers: a streaming command says what
/// it has when it has it, so there is no run to bring forward.
fn can_refresh(source: &Source) -> std::result::Result<(), &'static str> {
    match source {
        Source::Native(Which::Command(spec)) => match spec.run {
            crate::collect::command::Run::Stream => Err(
                "a streaming command speaks when it has something to say; give it an \
                 `interval` for there to be a run to bring forward",
            ),
            _ => Ok(()),
        },
        Source::Native(which) if which.pushed() => Err(
            "this source arrives when it changes rather than being read, so there is \
             nothing to ask it for",
        ),
        Source::Native(_) => Ok(()),
        _ => Err(
            "its source is not one dbar reads, so there is no reading to bring forward; a \
             provider and the compositor both speak when they have something to say",
        ),
    }
}

/// What a module has given its buttons to do, each named only where it was asked for.
///
/// A module with no further wordings has not claimed its `alt_button`, so writing one and
/// nothing to use it with is not a collision with anything.
struct Claims {
    alt: Option<Button>,
    collapse: Option<Button>,
    refresh: Option<Button>,
    control: Option<Control>,
    /// Which button mutes, for the control that has one. Meaningless for the others, and
    /// read only where `control` says it is the volume.
    mute: Button,
}

/// Every button this module has given a job, and the key that gave it.
///
/// One list, so the two places that care - a module against itself, and a module against
/// the group it sits in - are asking the same question of the same answer.
fn buttons_claimed(claims: &Claims, on_click: Option<&ClickActions>) -> Vec<(Button, String)> {
    let mut claimed: Vec<(Button, String)> = Vec::new();
    if let Some(button) = claims.alt {
        claimed.push((button, "format_alt".to_string()));
    }
    if let Some(button) = claims.collapse {
        claimed.push((button, "collapsible".to_string()));
    }
    if let Some(button) = claims.refresh {
        claimed.push((button, "refresh_button".to_string()));
    }
    // What `controls` and `scroll` bind, for the sources dbar can operate as well as read.
    // Only the presses are listed: a scroll notch is not a button and collides with
    // nothing here.
    match claims.control {
        Some(Control::Media) => claimed.push((Button::Left, "controls".to_string())),
        Some(Control::Volume) => claimed.push((claims.mute, "mute_button".to_string())),
        Some(Control::Brightness) | None => {}
    }
    if let Some(actions) = on_click {
        for button in actions.buttons() {
            claimed.push((button, format!("on_click.{}", button.name())));
        }
    }
    claimed
}

/// Reject a module where two things want the same button.
///
/// A button does one thing. Silently letting the first claimant win would make the loser
/// a key that is present, spelled correctly and simply ignored, which is the kind of
/// mistake a config file should not be able to express.
fn claim_buttons(module: &str, claimed: &[(Button, String)]) -> Result<()> {
    for (at, (button, by)) in claimed.iter().enumerate() {
        if let Some((_, first)) = claimed[..at].iter().find(|(b, _)| b == button) {
            bail!(
                "[module.{module}] gives the {} button to both {first} and {by}",
                button.name()
            );
        }
    }
    Ok(())
}

/// Reject a group that reserves a button one of its own modules is already using.
///
/// The group wins at the pointer - it has to, since it answers for its whole island - so
/// the module's key would still be there, still spelled correctly, and never again do
/// anything. That is the same mistake `claim_buttons` refuses one level down.
fn claim_group_button(
    group: &str,
    reserved: Button,
    modules: &[(String, Vec<(Button, String)>)],
) -> Result<()> {
    for (module, claimed) in modules {
        if let Some((_, by)) = claimed.iter().find(|(b, _)| *b == reserved) {
            bail!(
                "[group.{group}] reserves the {} button, but module {module:?} gives it to \
                 {by}; a group answers for its whole island, so the module would never see \
                 that press",
                reserved.name()
            );
        }
    }
    Ok(())
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
        self.positions
            .iter()
            .flat_map(|p| &p.groups)
            .flat_map(|g| &g.modules)
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

    /// The sources something in this config can ask to be read again.
    ///
    /// A command whose module has neither a signal nor a button for it is never asked, and
    /// then a command that answers once is done when it has answered rather than keeping a
    /// thread parked on a question that cannot come.
    pub fn refreshable(&self) -> std::collections::HashSet<Which> {
        let mut wanted = std::collections::HashSet::new();
        for module in self.modules() {
            let Source::Native(which) = &module.source else {
                continue;
            };
            if module.signal.is_some() || module.refresh_button.is_some() {
                wanted.insert(which.clone());
            }
        }
        wanted
    }

    /// Whether anything on the bar shows the system tray.
    ///
    /// Nothing about the tray is started otherwise - not the thread, not the bus
    /// connection, and above all not the watcher name, which an application registers with
    /// and then waits to be drawn by.
    pub fn needs_tray(&self) -> bool {
        self.modules().any(|m| matches!(m.source, Source::Tray(_)))
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

    /// Whether anything on the bar draws the focused window.
    pub fn needs_windows(&self) -> bool {
        self.modules()
            .any(|m| matches!(m.source, Source::SwayWindow(_)))
    }

    /// Whether anything on the bar draws the workspace list.
    pub fn needs_workspaces(&self) -> bool {
        self.modules()
            .any(|m| matches!(m.source, Source::SwayWorkspaces(_)))
    }

    /// Whether anything in this config comes from an external status provider.
    ///
    /// Nothing does on a native configuration, and then there is no child process to run.
    pub fn needs_provider(&self) -> bool {
        self.positions.iter().flat_map(|p| &p.groups).any(|group| {
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
            font_fallback: raw.bar.fallback.clone(),
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
            outputs: raw.bar.outputs.clone(),
            icon_theme: raw.bar.icon_theme.clone(),
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

        let mut positions = std::array::from_fn(|_| Position::default());
        for ((slot, raw_pos), name) in positions
            .iter_mut()
            .zip([&raw.left, &raw.center, &raw.right])
            .zip(["left", "center", "right"])
        {
            slot.separator = raw_pos
                .separator
                .as_ref()
                .map(|s| resolve_separator(s, &palette))
                .transpose()
                .with_context(|| format!("in [{name}.separator]"))?
                .filter(|s| !s.shape.is_none());
            if let Some(sep) = slot.separator
                && (!sep.width.is_finite() || sep.width <= 0.0 || !sep.overlap.is_finite())
            {
                bail!(
                    "in [{name}.separator]: width must be finite and positive, and overlap finite"
                );
            }
            for group_name in &raw_pos.groups {
                let raw_group = raw
                    .groups
                    .get(group_name)
                    .ok_or_else(|| anyhow!("group {group_name:?} is used but not defined"))?;
                let group = resolve_group(group_name, raw_group, &raw, &palette, &styles, base)?;
                if slot.separator.is_some() && (group.opacity != 1.0 || group.padding != 0.0) {
                    bail!(
                        "[{name}.separator] joins group {group_name:?}, which must have opacity = 1 and padding = 0"
                    );
                }
                slot.groups.push(group);
            }
        }

        // A menu is not part of the bar's own surface, so it cannot inherit anything by
        // sitting on it: what is not said here is taken from the bar and from the style
        // every module starts out with.
        let colour = |written: &Option<String>, fallback: Color| -> Result<Color> {
            match written {
                Some(name) => palette.get(name),
                None => Ok(fallback),
            }
        };
        let menu_background = colour(&raw.menu.background, bar.background)?;
        let menu = Menu {
            background: menu_background,
            foreground: colour(&raw.menu.foreground, base.foreground)?,
            highlight: colour(&raw.menu.highlight, base.foreground.faded(0.18))?,
            highlight_foreground: colour(&raw.menu.highlight_foreground, base.foreground)?,
            disabled: colour(&raw.menu.disabled, base.foreground.faded(0.45))?,
            separator: colour(&raw.menu.separator, base.foreground.faded(0.25))?,
            padding: raw.menu.padding.max(0.0),
            radius: raw.menu.radius.max(0.0),
            max_width: raw.menu.max_width.max(40.0),
        };

        share_command_fields(&mut positions)?;

        let config = Config {
            bar,
            menu,
            i3bar: I3Bar {
                command: raw.i3bar.command.clone(),
                args: raw.i3bar.args.clone(),
                names: raw.i3bar.names.clone(),
            },
            positions,
        };
        config.check_how_many_programs()?;
        Ok(config)
    }

    /// Refuse a config that could run more programs than dbar can stop.
    ///
    /// The table of what is running is a fixed size, because a signal handler reads it and
    /// a handler may not take a lock. What fills it is a collector rather than a module:
    /// two modules asking for the same command share one, the way they share every other
    /// source, and each collector runs one program at a time. Counting them here, plus the
    /// provider, is what makes the table enough - and a bar that would silently leave a
    /// program behind at shutdown never starts.
    fn check_how_many_programs(&self) -> Result<()> {
        let commands = self
            .collectors()
            .keys()
            .filter(|which| matches!(which, Which::Command(_)))
            .count();
        let wanted = commands + usize::from(self.needs_provider());
        if wanted > crate::proc::AT_ONCE {
            bail!(
                "this config runs {wanted} programs at once, and dbar can keep track of {}; \
                 use fewer command modules",
                crate::proc::AT_ONCE
            );
        }
        Ok(())
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
    // Both ends are refused rather than rounded to something that looks like an answer.
    // A length too small to be one leaves its collector due the moment it has been read,
    // which is a bar reading a source as fast as the machine can - what an interval
    // exists to prevent. A length too large is not a schedule anybody wrote on purpose,
    // and `Duration` cannot hold it: converting it panics, which took `--check-config`
    // down with it.
    if seconds < SHORTEST_INTERVAL.as_secs_f64() {
        bail!(
            "{text:?} is shorter than {SHORTEST_INTERVAL:?}, which is as often as a source \
             can be read"
        );
    }
    Duration::try_from_secs_f64(seconds)
        .map_err(|_| anyhow!("{text:?} is longer than a length of time dbar can schedule"))
}

/// The shortest interval a source may be given.
///
/// A millisecond is already far more often than anything a bar shows can change, and it
/// is a length a person can write on purpose. Below it lies the range where a rounding
/// error is the whole value.
const SHORTEST_INTERVAL: Duration = Duration::from_millis(1);

/// Every source a module can be built on, under the name a config writes for it.
///
/// The parameterised ones stand here as what they are without their parameter - a disk
/// with no path, a command with no argv - because this exists to answer what a source
/// publishes, and that does not depend on where it is pointed.
pub fn sources() -> Vec<(&'static str, Source)> {
    use crate::collect::{CommandSpec, Which, command::Run};

    vec![
        ("cpu", Source::Native(Which::Cpu)),
        ("memory", Source::Native(Which::Memory)),
        ("battery", Source::Native(Which::Battery)),
        ("backlight", Source::Native(Which::Backlight)),
        ("load", Source::Native(Which::Load)),
        ("temperature", Source::Native(Which::Temperature(None))),
        ("disk", Source::Native(Which::Disk("/".to_string()))),
        ("network", Source::Native(Which::Network(None))),
        ("time", Source::Native(Which::Time)),
        ("audio", Source::Native(Which::Audio)),
        ("media", Source::Native(Which::Media)),
        (
            "command",
            Source::Native(Which::Command(CommandSpec {
                argv: Vec::new(),
                run: Run::Stream,
                pages: false,
                fields: crate::collect::command::PLAIN,
                timeout: DEFAULT_COMMAND_TIMEOUT,
            })),
        ),
        ("provider", Source::Provider),
        ("sway:window", Source::SwayWindow(Scope::Output)),
        ("sway:workspaces", Source::SwayWorkspaces(Scope::Output)),
        ("sway:language", Source::SwayLanguage(Default::default())),
        ("sway:mode", Source::SwayMode),
        ("tray", Source::Tray(TrayView::default())),
    ]
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
        "sway:window" => Source::SwayWindow(raw.and_then(|m| m.scope).unwrap_or_default()),
        "sway:workspaces" => Source::SwayWorkspaces(raw.and_then(|m| m.scope).unwrap_or_default()),
        "sway:language" => Source::SwayLanguage(raw.map(|m| m.layouts.clone()).unwrap_or_default()),
        "sway:mode" => Source::SwayMode,
        "tray" => Source::Tray(TrayView {
            show_passive: raw.and_then(|m| m.show_passive).unwrap_or(true),
            order: raw.map(|m| m.order.clone()).unwrap_or_default(),
        }),
        "command" => {
            let mut argv = raw.map(|m| m.command.clone()).unwrap_or_default();
            if argv.is_empty() {
                bail!(
                    "module {module_name:?} reads from a command but names none; \
                     add `command = [\"program\", \"argument\"]`"
                );
            }
            // The knobs are handed over as further arguments, in the order they were
            // written: what they mean is the program's business, and dbar looking at
            // them would be dbar deciding what a script is allowed to be about.
            if let Some(params) = raw.map(|m| &m.params) {
                argv.extend(params.iter().cloned());
            }
            let declared = raw.map(|m| &m.fields).filter(|f| !f.is_empty());
            let fields = match declared {
                None => crate::collect::command::PLAIN,
                Some(declared) => {
                    let mut specs = Vec::with_capacity(declared.len());
                    for (name, kind) in declared {
                        specs.push(FieldSpec {
                            // The config is read once, so a name that outlives it is a
                            // handful of bytes rather than a leak that grows.
                            name: Box::leak(name.clone().into_boxed_str()),
                            kind: field_kind(kind)
                                .with_context(|| format!("in [module.{module_name}.fields]"))?,
                        });
                    }
                    Box::leak(specs.into_boxed_slice())
                }
            };
            let run = parse_run(raw.and_then(|m| m.interval.as_deref()), module_name)?;
            let pages = raw.is_some_and(|m| m.pages);
            // A streaming command says one thing at a time, as it happens; there is no
            // run whose lines could be pages of one another.
            if pages && run == crate::collect::command::Run::Stream {
                bail!(
                    "module {module_name:?} asks for pages, but a streaming command sends \
                     a reading per line as it prints them; give it an `interval` for a run \
                     whose lines are pages of one answer"
                );
            }
            // A command that streams is not waited on: it is expected to sit there, and
            // a deadline on it would mean killing a working program for doing its job.
            let timeout = match raw.and_then(|m| m.timeout.as_deref()) {
                Some(written) => parse_duration(written)
                    .with_context(|| format!("in module {module_name:?}, `timeout`"))?,
                None => DEFAULT_COMMAND_TIMEOUT,
            };
            if raw.is_some_and(|m| m.timeout.is_some())
                && run == crate::collect::command::Run::Stream
            {
                bail!(
                    "module {module_name:?} sets `timeout`, but a streaming command is meant                      to keep running; a timeout only applies to a command that is run for an                      answer, which is what an `interval` asks for"
                );
            }
            Source::Native(Which::Command(crate::collect::CommandSpec {
                argv,
                run,
                pages,
                fields,
                timeout,
            }))
        }
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
             tray, sway:window, sway:workspaces or sway:language"
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
            "command",
            raw.is_some_and(|m| !m.command.is_empty()),
            "command",
        ),
        (
            "fields",
            raw.is_some_and(|m| !m.fields.is_empty()),
            "command",
        ),
        (
            "params",
            raw.is_some_and(|m| !m.params.is_empty()),
            "command",
        ),
        ("pages", raw.is_some_and(|m| m.pages), "command"),
        (
            "timeout",
            raw.is_some_and(|m| m.timeout.is_some()),
            "command",
        ),
        (
            "layouts",
            raw.is_some_and(|m| !m.layouts.is_empty()),
            "sway:language",
        ),
        (
            "show_passive",
            raw.is_some_and(|m| m.show_passive.is_some()),
            "tray",
        ),
        ("order", raw.is_some_and(|m| !m.order.is_empty()), "tray"),
    ];
    for (key, given, belongs_to) in misplaced {
        if given && name != belongs_to {
            bail!("module {module_name:?} sets `{key}`, which only a {belongs_to} module reads");
        }
    }
    // `scope` is the one key two sources share, since both are about what is on a screen.
    if raw.is_some_and(|m| m.scope.is_some()) && !matches!(name, "sway:window" | "sway:workspaces")
    {
        bail!(
            "module {module_name:?} sets `scope`, which only a sway:window or \
             sway:workspaces module reads"
        );
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
    let collapsed_style = raw_group
        .collapsed
        .as_ref()
        .map(|collapsed| {
            let start = match &collapsed.style {
                Some(style) => *styles
                    .get(style)
                    .ok_or_else(|| anyhow!("unknown style {style:?}"))?,
                None => base,
            };
            start.overlay(&collapsed.overrides, palette)
        })
        .transpose()
        .with_context(|| format!("in [group.{name}.collapsed]"))?;
    let collapse = if raw_group.collapsible {
        let button = raw_group.collapse_button.ok_or_else(|| {
            anyhow!("[group.{name}]: collapsible requires an explicit collapse_button")
        })?;
        let style = collapsed_style.unwrap_or(base);
        let Some(icon) = style.icon else {
            bail!(
                "[group.{name}.collapsed]: collapsible requires an icon with positive finite icon_size"
            );
        };
        if !style.icon_size.is_finite() || style.icon_size <= 0.0 {
            bail!(
                "[group.{name}.collapsed]: collapsible requires an icon with positive finite icon_size"
            );
        }
        // The collapsed icon is the only thing left to click on to expand the group again,
        // so a width it can never fit into is not a group that renders narrow - it is a
        // group that disappears until dbar is restarted. Worked out the way layout does.
        let width = (style.icon_size * icon.width() + style.padding * 2.0).max(style.min_width);
        if style.max_width > 0.0 && width > style.max_width {
            bail!(
                "[group.{name}.collapsed]: the collapsed icon needs {width}px but max_width \
                 is {max}px, so the group would have nothing left to expand it with",
                max = style.max_width
            );
        }
        Some(GroupCollapse { button, style })
    } else {
        None
    };
    let wildcard = raw_group.modules.iter().any(|m| m == "*");
    let mut modules = Vec::new();
    // What each module in this group has given its buttons to do, kept until the group's
    // own reservation can be checked against all of them.
    let mut claimed_here: Vec<(String, Vec<(Button, String)>)> = Vec::new();
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
                // Every field a combined rule names has to exist and hold a word, for the
                // same reason: a rule keyed on something the source never publishes, or on
                // a number compared against a word, could not fire and is a typo.
                for field in raw_state.fields.iter().flatten().map(|(name, _)| name) {
                    let known = source.fields().iter().find(|f| f.name == field);
                    let Some(kind) = known.map(|f| f.kind) else {
                        bail!(
                            "[module.{module_name}.states.{state_name}] keys on ${field}, \
                             which this module's source does not publish"
                        );
                    };
                    if !matches!(kind, crate::status::Kind::Text) {
                        bail!(
                            "[module.{module_name}.states.{state_name}] compares ${field} \
                             against a word, but it is {}",
                            kind.describe()
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
                    fields: raw_state.fields.clone().unwrap_or_default(),
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

        let interval = match &source {
            // A command module's interval says how its program is run rather than when
            // dbar reads it - the reading arrives from the command's own thread - and it
            // was taken apart along with the source, above.
            Source::Native(Which::Command(_)) => None,
            _ => match raw_module.and_then(|m| m.interval.as_deref()) {
                Some(written) => Some(
                    parse_duration(written)
                        .with_context(|| format!("in [module.{module_name}] interval"))?,
                ),
                // Only dbar's own collectors are on a schedule dbar controls.
                None => match &source {
                    Source::Native(which) => Some(which.default_interval()),
                    _ => None,
                },
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
            if let Err(why) = can_refresh(&source) {
                bail!("module {module_name:?} sets a signal, but {why}");
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
                None if matches!(source, Source::Native(Which::Command(_))) => bail!(
                    "module {module_name:?} asks to be scrolled, but a command is read \
                     rather than set; `pages = true` is what puts its readings on the wheel"
                ),
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

        let alt_button = raw_module
            .and_then(|m| m.alt_button)
            .unwrap_or(Button::Left);
        let collapse_button = raw_module
            .and_then(|m| m.collapse_button)
            .unwrap_or(Button::Right);
        // Muting is the volume's alone. A key naming a button on a module with nothing to
        // mute is a button that would be present, spelled correctly and ignored.
        if raw_module.and_then(|m| m.mute_button).is_some()
            && !matches!(control, Some((Control::Volume, _)))
        {
            bail!(
                "module {module_name:?} sets `mute_button`, but nothing about it can be \
                 muted; a module operates the volume once it names a `scroll` step"
            );
        }
        let mute_button = raw_module
            .and_then(|m| m.mute_button)
            .unwrap_or(Button::Middle);
        // No default: a button is claimed only where the config asked for it, so a module
        // that never mentions refreshing leaves all three for whatever else wants them.
        let refresh_button = raw_module.and_then(|m| m.refresh_button);
        if refresh_button.is_some()
            && let Err(why) = can_refresh(&source)
        {
            bail!("module {module_name:?} gives a button to refreshing it, but {why}");
        }
        let on_click = raw_module
            .and_then(|m| m.on_click.as_ref())
            .map(|raw| click_actions(raw, module_name))
            .transpose()?;
        let claims = Claims {
            alt: (!format_alt.is_empty()).then_some(alt_button),
            collapse: collapsible.then_some(collapse_button),
            refresh: refresh_button,
            control: control.map(|(what, _)| what),
            mute: mute_button,
        };
        let claimed = buttons_claimed(&claims, on_click.as_ref());
        claim_buttons(module_name, &claimed)?;
        claimed_here.push((module_name.clone(), claimed));

        modules.push(Module {
            name: module_name.clone(),
            source,
            interval,
            signal,
            control,
            collapsible,
            alt_button,
            collapse_button,
            refresh_button,
            // Carried only where there is something to mute, so a press on anything else
            // falls through to whatever the module was already forwarding it to.
            mute_button: matches!(control, Some((Control::Volume, _))).then_some(mute_button),
            on_click: on_click.map(std::sync::Arc::new),
            format,
            format_alt,
            style,
            states,
        });
    }

    if let Some(collapse) = &collapse {
        claim_group_button(name, collapse.button, &claimed_here)?;
    }

    // Wildcard groups need a style for blocks that have no `[module.*]` table.
    let fallback = styles.get("default").copied().unwrap_or(base);

    let separator = raw_group
        .separator
        .as_ref()
        .map(|s| resolve_separator(s, palette))
        .transpose()
        .with_context(|| format!("in [group.{name}.separator]"))?
        .unwrap_or_default();

    let ends = match &raw_group.ends {
        Some(raw) => Ends {
            left: raw.left,
            right: raw.right,
            width: raw.width.unwrap_or(separator.width).max(0.0),
            overlap: raw.overlap.unwrap_or(separator.overlap).max(0.0),
            direction: raw.direction,
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
        name: name.to_string(),
        collapse,
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
                alt_button: Button::Left,
                collapse_button: Button::Right,
                refresh_button: None,
                mute_button: None,
                on_click: None,
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

fn resolve_separator(raw: &RawSeparator, palette: &Palette) -> Result<Separator> {
    Ok(Separator {
        shape: raw.shape,
        width: raw.width.max(0.0),
        direction: raw.direction,
        color: parse_separator_color(&raw.color, palette)?,
        overlap: raw.overlap.max(0.0),
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
    /// A config with one module, so a button test says only what it is about.
    fn one_module(body: &str) -> String {
        format!(
            r#"
[bar]
height = 30

[right]
groups = ["g"]

[group.g]
modules = ["m"]

[module.m]
{body}
"#
        )
    }

    /// How a command module's program is run, given what its `interval` says.
    fn run_of(interval: &str) -> crate::collect::command::Run {
        let body = match interval.is_empty() {
            true => "source = \"command\"\ncommand = [\"true\"]".to_string(),
            false => {
                format!("source = \"command\"\ncommand = [\"true\"]\ninterval = \"{interval}\"")
            }
        };
        let cfg = Config::parse(&one_module(&body)).expect("a command module");
        match &cfg.modules().next().unwrap().source {
            Source::Native(Which::Command(spec)) => spec.run,
            other => panic!("source is {other:?}"),
        }
    }

    #[test]
    fn a_command_with_no_interval_streams() {
        use crate::collect::command::Run;
        // The cheapest arrangement, and so the one you get by saying nothing: no process
        // is started to find out that nothing changed.
        assert_eq!(run_of(""), Run::Stream);
    }

    #[test]
    fn an_interval_runs_the_command_again_that_often() {
        use crate::collect::command::Run;
        assert_eq!(run_of("30s"), Run::Every(Duration::from_secs(30)));
        assert_eq!(run_of("2m"), Run::Every(Duration::from_secs(120)));
    }

    #[test]
    fn once_runs_it_at_startup_and_then_only_when_asked() {
        use crate::collect::command::Run;
        assert_eq!(run_of("once"), Run::Once);
        assert_eq!(run_of("ONCE"), Run::Once, "case is not the point");
    }

    #[test]
    fn an_interval_that_would_never_stop_running_is_refused() {
        // A command run every no-time forks as fast as the machine can. Durations are
        // already required to be positive, which covers it.
        let e = Config::parse(&one_module(
            "source = \"command\"\ncommand = [\"true\"]\ninterval = \"0s\"",
        ))
        .expect_err("a zero interval never stops running");
        assert!(format!("{e:#}").contains("positive"), "{e:#}");
    }

    #[test]
    fn an_interval_that_is_neither_a_period_nor_once_says_so() {
        let e = Config::parse(&one_module(
            "source = \"command\"\ncommand = [\"true\"]\ninterval = \"sometimes\"",
        ))
        .expect_err("that is not a schedule");
        let message = format!("{e:#}");
        assert!(message.contains("once"), "{message}");
    }

    #[test]
    fn the_same_program_on_two_schedules_is_two_programs() {
        // Identity is the argv and how it is run: two modules that want the same script
        // at different rates want two of it, and sharing one would give one of them the
        // other's schedule.
        use crate::collect::command::Run;
        let a = crate::collect::CommandSpec {
            argv: vec!["s".to_string()],
            run: Run::Every(Duration::from_secs(1)),
            pages: false,
            fields: crate::collect::command::PLAIN,
            timeout: DEFAULT_COMMAND_TIMEOUT,
        };
        let b = crate::collect::CommandSpec {
            run: Run::Every(Duration::from_secs(60)),
            ..a.clone()
        };
        let same = a.clone();
        assert_ne!(a, b);
        assert_eq!(a, same);

        // And so is the deadline: one module willing to wait a minute for a script and
        // another that is not are asking for different things from the same program.
        let patient = crate::collect::CommandSpec {
            timeout: Duration::from_secs(60),
            ..a.clone()
        };
        assert_ne!(a, patient);
    }

    /// The list `--fields` answers from and the list a module may name have to be the
    /// same list. They are written out separately - one is a table, the other a match -
    /// so this is what stops them drifting apart, which would be help that lies.
    #[test]
    fn every_source_a_module_can_name_is_one_the_help_lists() {
        for (name, listed) in sources() {
            let body = match name {
                "command" => format!("source = \"{name}\"\ncommand = [\"true\"]"),
                _ => format!("source = \"{name}\""),
            };
            let cfg = Config::parse(&one_module(&body))
                .unwrap_or_else(|e| panic!("a module reading from {name:?}: {e:#}"));
            let resolved = &cfg.modules().next().expect("one module").source;
            let listed: Vec<&str> = listed.fields().iter().map(|f| f.name).collect();
            let real: Vec<&str> = resolved.fields().iter().map(|f| f.name).collect();
            assert_eq!(listed, real, "{name} publishes something else than it says");
        }
    }

    /// Two modules on one command share a process, so they have to share a schema too:
    /// what only one of them declared would otherwise be parsed out of the output and
    /// thrown away, leaving a module that validated and then drew nothing.
    #[test]
    fn modules_sharing_a_command_share_every_field_they_declared() {
        let text = r#"
[bar]
height = 30

[right]
groups = ["g"]

[group.g]
modules = ["temp", "wind"]

[module.temp]
source = "command"
command = ["weather"]
interval = "10m"
fields = { temp = "number" }
format = "$temp"

[module.wind]
source = "command"
command = ["weather"]
interval = "10m"
fields = { wind = "number" }
format = "$wind"
"#;
        let cfg = Config::parse(text).expect("two modules on one command");
        for module in cfg.modules() {
            let Source::Native(Which::Command(spec)) = &module.source else {
                panic!("{} is not a command module", module.name);
            };
            let names: Vec<&str> = spec.fields.iter().map(|f| f.name).collect();
            assert_eq!(names, ["temp", "wind"], "in module {:?}", module.name);
        }

        // And one command, not two: the schemas differing is not what tells two of them
        // apart.
        assert_eq!(cfg.collectors().len(), 1);
    }

    /// Sharing a reading means agreeing about what is in it. Two modules that do not are
    /// told so by name, rather than one of them silently deciding.
    #[test]
    fn modules_sharing_a_command_may_not_disagree_about_a_field() {
        let text = r#"
[bar]
height = 30

[right]
groups = ["g"]

[group.g]
modules = ["a", "b"]

[module.a]
source = "command"
command = ["weather"]
interval = "10m"
fields = { load = "number" }
format = "$load"

[module.b]
source = "command"
command = ["weather"]
interval = "10m"
fields = { load = "text" }
format = "$load"
"#;
        let e = Config::parse(text).expect_err("two kinds for one field");
        let message = format!("{e:#}");
        assert!(
            message.contains("\"a\"") && message.contains("\"b\""),
            "{message}"
        );
        assert!(message.contains("load"), "{message}");
    }

    /// A bar that draws nothing the compositor knows must not connect to it: two sockets,
    /// two threads and a tree read on every window title are the cost of asking.
    #[test]
    fn a_config_says_which_halves_of_the_compositor_it_needs() {
        let native = Config::parse(
            "[bar]\nheight = 30\n\n[right]\ngroups = [\"g\"]\n\n\
             [group.g]\nmodules = [\"cpu\"]\n\n[module.cpu]\nsource = \"cpu\"\n",
        )
        .expect("a native config");
        assert!(!native.needs_windows());
        assert!(!native.needs_workspaces());
        assert!(!native.needs_language());
        assert!(!native.needs_mode());

        let desktop = Config::parse(
            "[bar]\nheight = 30\n\n[right]\ngroups = [\"g\"]\n\n\
             [group.g]\nmodules = [\"ws\"]\n\n[module.ws]\nsource = \"sway:workspaces\"\n",
        )
        .expect("a workspace config");
        // A workspace list is not a window title: the tree is what costs, and nothing here
        // reads it.
        assert!(desktop.needs_workspaces());
        assert!(!desktop.needs_windows());
    }

    /// The argv a command module ends up with, given what it wrote.
    fn argv_of(body: &str) -> Vec<String> {
        let cfg = Config::parse(&one_module(body)).expect("a command module");
        match &cfg.modules().next().unwrap().source {
            Source::Native(Which::Command(spec)) => spec.argv.clone(),
            other => panic!("source is {other:?}"),
        }
    }

    /// The knobs are arguments, in the order they were written: what they mean is the
    /// script's business, which is what keeps one script good for two cities.
    #[test]
    fn params_are_handed_to_the_command_as_further_arguments() {
        let argv = argv_of(
            "source = \"command\"\ncommand = [\"weather\", \"--quiet\"]\n\
             params = [\"metric\", \"45.25,19.83\"]",
        );
        assert_eq!(argv, ["weather", "--quiet", "metric", "45.25,19.83"]);
    }

    /// Two modules running the same script with different knobs want two of it, since
    /// what is run is what tells one command from another.
    #[test]
    fn changing_a_param_asks_for_a_different_command() {
        let here = argv_of("source = \"command\"\ncommand = [\"w\"]\nparams = [\"here\"]");
        let there = argv_of("source = \"command\"\ncommand = [\"w\"]\nparams = [\"there\"]");
        assert_ne!(here, there);
    }

    /// One command asked about three places says three things, and the module scrolls
    /// between them.
    #[test]
    fn a_command_can_publish_a_page_per_line() {
        let cfg = Config::parse(&one_module(
            "source = \"command\"\ncommand = [\"weather\"]\ninterval = \"once\"\npages = true",
        ))
        .expect("a command with pages");
        match &cfg.modules().next().unwrap().source {
            Source::Native(Which::Command(spec)) => assert!(spec.pages),
            other => panic!("source is {other:?}"),
        }
    }

    /// Paging is what a run's lines mean, so two modules that read them differently are
    /// two commands rather than one they would have to agree about.
    #[test]
    fn a_command_is_given_a_deadline_whether_it_asks_for_one_or_not() {
        let cfg = Config::parse(&one_module(
            "source = \"command\"\ncommand = [\"w\"]\ninterval = \"1s\"",
        ))
        .expect("a command module");
        let spec = match &cfg.modules().next().unwrap().source {
            Source::Native(Which::Command(spec)) => spec.clone(),
            other => panic!("expected a command, got {other:?}"),
        };
        assert_eq!(spec.timeout, DEFAULT_COMMAND_TIMEOUT);

        let cfg = Config::parse(&one_module(
            "source = \"command\"\ncommand = [\"w\"]\ninterval = \"1s\"\ntimeout = \"2s\"",
        ))
        .expect("a command module with a deadline");
        let spec = match &cfg.modules().next().unwrap().source {
            Source::Native(Which::Command(spec)) => spec.clone(),
            other => panic!("expected a command, got {other:?}"),
        };
        assert_eq!(spec.timeout, Duration::from_secs(2));
    }

    /// The table of running programs is a fixed size, because a signal handler reads it and
    /// a handler may not take a lock. A config that could overrun it is refused when it is
    /// read: the alternative is a program silently left behind when the bar stops.
    ///
    /// What counts is the collector, not the module. Two modules that name the same command
    /// share one collector and so one program, the way two modules reading the processor
    /// share one reading.
    #[test]
    fn a_config_that_runs_more_programs_than_can_be_stopped_is_refused() {
        // `how_many` modules, each running its own program unless `share` is set, in which
        // case they all name the same one.
        let modules = |how_many: usize, share: bool| {
            let names: Vec<String> = (0..how_many).map(|n| format!("run{n}")).collect();
            let mut text = format!("[left]\ngroups = [\"g\"]\n[group.g]\nmodules = {names:?}\n");
            for (n, name) in names.iter().enumerate() {
                let argv = match share {
                    true => "\"true\"".to_string(),
                    false => format!("\"true\", \"{n}\""),
                };
                text += &format!(
                    "[module.{name}]\nsource = \"command\"\ncommand = [{argv}]\nformat = \"$text\"\n"
                );
            }
            text
        };
        let all = crate::proc::AT_ONCE;
        Config::parse(&modules(all, false)).expect("as many programs as dbar can keep track of");
        // Sharing one command is one program, however many modules show it.
        Config::parse(&modules(all + 20, true)).expect("many modules, one program");

        let e = Config::parse(&modules(all + 1, false))
            .expect_err("one more program than dbar can keep track of");
        let message = format!("{e:#}");
        assert!(message.contains(&format!("{all}")), "{message}");
        assert!(message.contains("command modules"), "{message}");
    }

    /// A streaming command is meant to sit there, so a deadline on one would mean killing
    /// a working program for doing its job.""
    #[test]
    fn a_streaming_command_cannot_be_given_a_deadline() {
        let e = Config::parse(&one_module(
            "source = \"command\"\ncommand = [\"tail\"]\ntimeout = \"2s\"",
        ))
        .expect_err("a streaming command has nothing to time out");
        let message = format!("{e:#}");
        assert!(message.contains("timeout"), "{message}");
        assert!(message.contains("interval"), "{message}");
    }

    #[test]
    fn a_deadline_on_a_module_that_runs_nothing_is_rejected() {
        let e = Config::parse(&one_module("source = \"cpu\"\ntimeout = \"2s\""))
            .expect_err("only a command has a run to time out");
        let message = format!("{e:#}");
        assert!(message.contains("timeout"), "{message}");
        assert!(message.contains("command"), "{message}");
    }

    #[test]
    fn paging_tells_one_command_from_another() {
        use crate::collect::command::Run;
        let plain = crate::collect::CommandSpec {
            argv: vec!["w".to_string()],
            run: Run::Once,
            pages: false,
            fields: crate::collect::command::PLAIN,
            timeout: DEFAULT_COMMAND_TIMEOUT,
        };
        let paged = crate::collect::CommandSpec {
            pages: true,
            ..plain.clone()
        };
        assert_ne!(plain, paged);
    }

    #[test]
    fn a_streaming_command_has_no_lines_to_page_between() {
        let e = Config::parse(&one_module(
            "source = \"command\"\ncommand = [\"tail\"]\npages = true",
        ))
        .expect_err("a streaming command sends a reading per line as it goes");
        let message = format!("{e:#}");
        assert!(message.contains("interval"), "{message}");
    }

    #[test]
    fn pages_belong_to_a_command_and_nothing_else() {
        let e = Config::parse(&one_module("source = \"cpu\"\npages = true"))
            .expect_err("a cpu module publishes one reading");
        let message = format!("{e:#}");
        assert!(message.contains("pages"), "{message}");
    }

    /// `scroll` is for what dbar can set as well as read. A command module is scrolled
    /// too, but through its pages, so the error says which key that is.
    #[test]
    fn scrolling_a_command_points_at_pages() {
        let e = Config::parse(&one_module(
            "source = \"command\"\ncommand = [\"w\"]\nscroll = \"5%\"",
        ))
        .expect_err("a command is not something dbar can set");
        let message = format!("{e:#}");
        assert!(message.contains("pages = true"), "{message}");
    }

    #[test]
    fn params_belong_to_a_command_and_nothing_else() {
        let e = Config::parse(&one_module("source = \"cpu\"\nparams = [\"metric\"]"))
            .expect_err("a cpu module runs nothing to pass them to");
        let message = format!("{e:#}");
        assert!(message.contains("params"), "{message}");
        assert!(message.contains("command"), "{message}");
    }

    /// A reading that costs a request to somebody else's server is worth asking for
    /// rather than taking on a schedule, and this is what asks.
    #[test]
    fn a_button_can_ask_a_source_to_be_read_again() {
        let cfg = Config::parse(&one_module(
            "source = \"command\"\ncommand = [\"weather\"]\ninterval = \"once\"\n\
             refresh_button = \"left\"",
        ))
        .expect("a command that answers when asked");
        let module = cfg.modules().next().expect("the one module");
        assert_eq!(module.refresh_button, Some(Button::Left));
        assert_eq!(cfg.refreshable().len(), 1, "something can ask for it");
    }

    /// A command nothing can ask keeps no way of being asked, so one that answers once is
    /// done when it has answered rather than parking a thread on a question that cannot
    /// come.
    #[test]
    fn a_command_nobody_can_ask_is_not_askable() {
        let cfg = Config::parse(&one_module(
            "source = \"command\"\ncommand = [\"uname\"]\ninterval = \"once\"",
        ))
        .expect("a command module");
        assert!(cfg.refreshable().is_empty());
    }

    #[test]
    fn a_streaming_command_has_no_run_to_bring_forward() {
        let e = Config::parse(&one_module(
            "source = \"command\"\ncommand = [\"tail\"]\nrefresh_button = \"left\"",
        ))
        .expect_err("a streaming command speaks for itself");
        let message = format!("{e:#}");
        assert!(message.contains("interval"), "{message}");
    }

    /// The volume arrives when it moves. Asking for it would be asking a collector that
    /// exists only to say it cannot answer, so the config says so instead.
    #[test]
    fn a_source_that_arrives_on_its_own_cannot_be_asked() {
        for key in ["refresh_button = \"left\"", "signal = 3"] {
            let e = Config::parse(&one_module(&format!("source = \"audio\"\n{key}")))
                .expect_err("the volume is not read");
            let message = format!("{e:#}");
            assert!(message.contains("arrives when it changes"), "{message}");
        }
    }

    #[test]
    fn refreshing_cannot_take_a_button_something_else_has() {
        let e = Config::parse(&one_module(
            "source = \"command\"\ncommand = [\"weather\"]\ninterval = \"once\"\n\
             format_alt = \"$text!\"\nrefresh_button = \"left\"",
        ))
        .expect_err("the left button was already turning the page");
        let message = format!("{e:#}");
        assert!(message.contains("format_alt"), "{message}");
        assert!(message.contains("refresh_button"), "{message}");
    }

    #[test]
    fn a_click_can_run_a_program_of_your_own() {
        let cfg = Config::parse(&one_module(
            "source = \"time\"\non_click = { left = [\"cal\", \"-3\"] }",
        ))
        .expect("a clock with something to run");
        let module = cfg.modules().next().expect("the one module");
        let actions = module.on_click.as_ref().expect("on_click was written");
        assert_eq!(
            actions.for_button(Button::Left),
            Some(["cal".to_string(), "-3".to_string()].as_slice())
        );
        assert_eq!(actions.for_button(Button::Right), None);
    }

    #[test]
    fn a_button_given_nothing_to_run_is_a_mistake_worth_stopping_for() {
        let e = Config::parse(&one_module(
            "source = \"time\"\non_click = { left = [\"\"] }",
        ))
        .expect_err("an empty program name runs nothing");
        let message = format!("{e:#}");
        assert!(message.contains("on_click.left"), "{message}");
        assert!(message.contains("[module.m]"), "{message}");
    }

    #[test]
    fn further_wordings_answer_to_the_left_button_unless_told_otherwise() {
        let cfg = Config::parse(&one_module(
            "source = \"cpu\"\nformat_alt = \"$utilization\"",
        ))
        .expect("a module with a second wording");
        assert_eq!(cfg.modules().next().unwrap().alt_button, Button::Left);

        let moved = Config::parse(&one_module(
            "source = \"cpu\"\nformat_alt = \"$utilization\"\nalt_button = \"middle\"",
        ))
        .expect("the button is the config's to choose");
        assert_eq!(moved.modules().next().unwrap().alt_button, Button::Middle);
    }

    #[test]
    fn two_things_wanting_one_button_is_a_startup_error() {
        // The left button cannot both run a program and move through wordings; picking a
        // winner silently would leave the loser spelled correctly and doing nothing.
        let e = Config::parse(&one_module(
            "source = \"cpu\"\nformat_alt = \"$utilization\"\non_click = { left = [\"true\"] }",
        ))
        .expect_err("the left button was given away twice");
        let message = format!("{e:#}");
        assert!(message.contains("left"), "{message}");
        assert!(message.contains("format_alt"), "{message}");
        assert!(message.contains("on_click.left"), "{message}");

        // Moving one of them off it settles the matter.
        Config::parse(&one_module(
            "source = \"cpu\"\nformat_alt = \"$utilization\"\nalt_button = \"middle\"\non_click = { left = [\"true\"] }",
        ))
        .expect("nothing is claimed twice now");
    }

    #[test]
    fn folding_answers_to_the_right_button_unless_told_otherwise() {
        let cfg = Config::parse(&one_module(
            "source = \"cpu\"\ncollapsible = true\nicon = \"cpu\"",
        ))
        .expect("a module that folds");
        assert_eq!(cfg.modules().next().unwrap().collapse_button, Button::Right);

        // Moved off the right, which then leaves the right free for something else.
        let moved = Config::parse(&one_module(
            "source = \"cpu\"\ncollapsible = true\nicon = \"cpu\"\ncollapse_button = \"middle\"\non_click = { right = [\"true\"] }",
        ))
        .expect("nothing is claimed twice");
        let module = moved.modules().next().unwrap();
        assert_eq!(module.collapse_button, Button::Middle);
        assert!(module.on_click.is_some());
    }

    #[test]
    fn muting_answers_to_the_middle_button_unless_told_otherwise() {
        let cfg = Config::parse(&one_module("source = \"audio\"\nscroll = \"5%\""))
            .expect("a volume module");
        assert_eq!(
            cfg.modules().next().unwrap().mute_button,
            Some(Button::Middle)
        );

        // Moved onto the left, with the wordings moved off it so nothing is claimed twice.
        let moved = Config::parse(&one_module(
            "source = \"audio\"\nscroll = \"5%\"\nmute_button = \"left\"\nformat_alt = [\" $volume \"]\nalt_button = \"right\"",
        ))
        .expect("nothing is claimed twice");
        assert_eq!(
            moved.modules().next().unwrap().mute_button,
            Some(Button::Left)
        );

        // A module with nothing to mute carries no button for it, so the press falls
        // through to whatever the module was forwarding to.
        let cpu = Config::parse(&one_module("source = \"cpu\"")).expect("a cpu module");
        assert_eq!(cpu.modules().next().unwrap().mute_button, None);
    }

    #[test]
    fn muting_is_refused_where_there_is_nothing_to_mute() {
        let e = Config::parse(&one_module("source = \"cpu\"\nmute_button = \"left\""))
            .expect_err("a cpu module cannot be muted");
        assert!(format!("{e:#}").contains("mute_button"), "{e:#}");
    }

    #[test]
    fn the_button_that_mutes_is_claimed_like_any_other() {
        // The volume's own button is taken, so a program cannot be given the same one.
        let e = Config::parse(&one_module(
            "source = \"audio\"\nscroll = \"5%\"\non_click = { middle = [\"true\"] }",
        ))
        .expect_err("muting is already on the middle");
        assert!(format!("{e:#}").contains("mute_button"), "{e:#}");

        // Moving the mute frees the button it left behind.
        Config::parse(&one_module(
            "source = \"audio\"\nscroll = \"5%\"\nmute_button = \"right\"\non_click = { middle = [\"true\"] }",
        ))
        .expect("the middle is free once muting has moved off it");
    }

    #[test]
    fn a_button_a_native_control_already_uses_is_claimed_too() {
        // `controls` on a player is the left button, and a right click folds any module
        // down, so neither is free to be given away a second time.
        let e = Config::parse(&one_module(
            "source = \"media\"\ncontrols = true\non_click = { left = [\"true\"] }",
        ))
        .expect_err("play and pause are already on the left");
        assert!(format!("{e:#}").contains("controls"), "{e:#}");

        let e = Config::parse(&one_module(
            "source = \"cpu\"\ncollapsible = true\non_click = { right = [\"true\"] }",
        ))
        .expect_err("folding is already on the right");
        assert!(format!("{e:#}").contains("collapsible"), "{e:#}");
    }

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

    /// A length is refused at both ends rather than rounded into something that looks
    /// like an answer. Too small used to arrive as zero, which leaves a collector due the
    /// moment it has been read - a bar reading a source as fast as the machine can, which
    /// is what an interval exists to prevent. Too large used to panic in the conversion,
    /// so a config nobody could run took `--check-config` down rather than being reported.
    #[test]
    fn a_length_of_time_has_two_ends() {
        assert_eq!(parse_duration("1ms").unwrap(), Duration::from_millis(1));

        for shorter in ["0.0000000001s", "0.0001ms", "0.00000001m"] {
            let e = parse_duration(shorter).expect_err("shorter than dbar can schedule");
            assert!(format!("{e:#}").contains("shorter"), "{e:#}");
        }
        // Scientific notation is refused a step earlier - "e300s" is not a unit - so the
        // cases here are the ones that reach the conversion as a number.
        for longer in ["999999999999999999999999999s", "99999999999999999999h"] {
            let e = parse_duration(longer).expect_err("longer than dbar can schedule");
            assert!(format!("{e:#}").contains("longer"), "{e:#}");
        }
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

    /// A bar with nothing to say about screens goes on all of them, including one plugged
    /// in later, which is what a session with one monitor has always had.
    #[test]
    fn a_bar_that_names_no_screens_goes_on_every_one() {
        let bar = Config::parse("[bar]\nheight = 20\n").expect("parses").bar;
        assert!(bar.shows_on(Some("DP-1")));
        assert!(bar.shows_on(None));
    }

    #[test]
    fn a_bar_that_names_screens_goes_only_on_those() {
        let bar = Config::parse("[bar]\noutputs = [\"DP-1\", \"DP-2\"]\n")
            .expect("parses")
            .bar;
        assert!(bar.shows_on(Some("DP-1")));
        assert!(!bar.shows_on(Some("HDMI-A-1")));
        // A screen the compositor has not named yet cannot be one of two written down.
        assert!(!bar.shows_on(None));

        let all = Config::parse("[bar]\noutputs = [\"*\"]\n")
            .expect("parses")
            .bar;
        assert!(all.shows_on(Some("HDMI-A-1")));
        assert!(all.shows_on(None));
    }

    /// `scope` is about what is on a screen, so it means nothing on a module that is not
    /// drawn from the compositor - and a key that quietly does nothing is how a config
    /// comes to be wrong for months.
    #[test]
    fn scope_on_a_module_that_is_not_about_a_screen_is_rejected() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
source = "cpu"
scope = "session"
"##;
        let e = Config::parse(config).expect_err("scope means nothing to a cpu module");
        let message = format!("{e:#}");
        assert!(message.contains("scope"), "{message}");
        assert!(message.contains("sway:window"), "{message}");
    }

    #[test]
    fn a_compositor_module_is_about_its_own_screen_unless_it_says_otherwise() {
        let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["ws", "win"]

[module.ws]
source = "sway:workspaces"

[module.win]
source = "sway:window"
scope = "session"
"##;
        let cfg = Config::parse(config).expect("parses");
        let source = |name: &str| {
            cfg.modules()
                .find(|m| m.name == name)
                .expect("the module is there")
                .source
                .clone()
        };
        assert_eq!(source("ws"), Source::SwayWorkspaces(Scope::Output));
        assert_eq!(source("win"), Source::SwayWindow(Scope::Session));
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
                    .flat_map(|p| &p.groups)
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

    /// A state that is a combination of readings has to beat the rules that name either
    /// half, or muted headphones would show whichever of the two got sorted first.
    #[test]
    fn a_rule_naming_two_readings_beats_one_naming_either() {
        let config = r##"
[right]
groups = ["g"]

[group.g]
modules = ["volume"]

[module.volume]
source = "audio"
icon = "volume"

[module.volume.states.zz_both]
fields = { muted = "yes", port = "headphones" }
icon = "headphones-muted"

[module.volume.states.muted]
field = "muted"
equals = "yes"
icon = "volume-muted"

[module.volume.states.port]
field = "port"
equals = "headphones"
icon = "headphones"
"##;
        let cfg = Config::parse(config).expect("parses");
        let module = cfg
            .positions
            .iter()
            .flat_map(|p| &p.groups)
            .next()
            .unwrap()
            .modules[0]
            .clone();

        let says = |muted: &str, port: &str| {
            let mut fields = crate::status::Fields::default();
            fields.set("muted", crate::status::Value::Text(muted.to_string()));
            fields.set("port", crate::status::Value::Text(port.to_string()));
            module
                .states
                .iter()
                .find(|rule| rule.matches(StateFlags::default(), false, &fields, ""))
                .and_then(|rule| rule.style.icon)
        };

        assert_eq!(says("yes", "headphones"), Icon::parse("headphones-muted"));
        assert_eq!(says("yes", "speaker"), Icon::parse("volume-muted"));
        assert_eq!(says("no", "headphones"), Icon::parse("headphones"));
        assert_eq!(
            says("no", "speaker"),
            None,
            "nothing unusual is being reported"
        );
    }

    /// A combined rule is checked against the source the same way a single one is.
    #[test]
    fn a_combined_rule_cannot_name_a_field_the_source_does_not_publish() {
        let config = r##"
[right]
groups = ["g"]

[group.g]
modules = ["volume"]

[module.volume]
source = "audio"

[module.volume.states.odd]
fields = { muted = "yes", jack = "in" }
"##;
        let e = Config::parse(config).expect_err("jack is not an audio field");
        let message = format!("{e:#}");
        assert!(message.contains("$jack"), "{message}");
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
    #[test]
    fn group_joins_are_opt_in_and_validate_their_groups() {
        let config = |separator: &str, group: &str| {
            format!(
                r#"
[right]
groups = ["g"]
{separator}
[group.g]
modules = ["cpu"]
{group}
[module.cpu]
source = "cpu"
"#
            )
        };
        for separator in ["", "[right.separator]\nshape = 'none'"] {
            let cfg = Config::parse(&config(separator, "opacity = 0.5\npadding = 3")).unwrap();
            assert!(cfg.positions[2].separator.is_none());
            assert_eq!(cfg.positions[2].groups[0].opacity, 0.5);
        }
        let joined = "[right.separator]\nshape = 'slant'\nwidth = 6";
        let cfg = Config::parse(&config(joined, "")).unwrap();
        assert_eq!(cfg.positions[2].separator.unwrap().width, 6.0);
        assert!(cfg.positions[..2].iter().all(|p| p.separator.is_none()));
        for group in ["opacity = 0.5", "padding = 3"] {
            let error = Config::parse(&config(joined, group))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("right.separator") && error.contains("group \"g\""),
                "{error}"
            );
        }
        for bad in [
            "width = 0",
            "width = -1",
            "width = inf",
            "overlap = inf",
            "color = '$missing'",
            "shape = 'unknown'",
        ] {
            let separator = format!(
                "[right.separator]\n{}\n{bad}",
                if bad.starts_with("shape") {
                    ""
                } else {
                    "shape = 'slant'"
                }
            );
            assert!(Config::parse(&config(&separator, "")).is_err(), "{bad}");
        }
    }

    /// A group answers for its whole island, so a button it reserves never reaches the
    /// modules inside it. dbar refuses to let a module hand one button two jobs; a group
    /// taking a button a module is already using is the same mistake one level up, and the
    /// module's key would be there, spelled correctly, and dead.
    #[test]
    fn a_group_may_not_reserve_a_button_one_of_its_modules_uses() {
        let config = |group: &str, module: &str| {
            format!(
                "[left]\ngroups = ['g']\n[group.g]\nmodules = ['m']\ncollapsible = true\n\
                 collapse_button = '{group}'\ncollapsed = {{ icon = 'cpu' }}\n\
                 [module.m]\nsource = 'cpu'\n{module}\n"
            )
        };

        for (button, module, by) in [
            (
                "right",
                "collapsible = true
icon = 'cpu'",
                "collapsible",
            ),
            ("left", "format_alt = '$utilization'", "format_alt"),
            ("middle", "refresh_button = 'middle'", "refresh_button"),
            ("left", "on_click = { left = ['true'] }", "on_click.left"),
        ] {
            let e = Config::parse(&config(button, module))
                .expect_err("the group and the module both want that button");
            let message = format!("{e:#}");
            assert!(message.contains(by), "{message}");
            assert!(message.contains("\"m\""), "{message}");
            assert!(message.contains(button), "{message}");
        }

        // A module operating the volume claims the button that mutes it.
        let volume = "[left]\ngroups = ['g']\n[group.g]\nmodules = ['m']\ncollapsible = true\n\
             collapse_button = 'middle'\ncollapsed = { icon = 'cpu' }\n\
             [module.m]\nsource = 'audio'\nscroll = '5%'\n";
        let e = Config::parse(volume).expect_err("middle mutes");
        assert!(format!("{e:#}").contains("mute_button"), "{e:#}");

        // And a module that leaves the reserved button alone is fine, however much else
        // it does with the other two.
        Config::parse(&config(
            "right",
            "format_alt = '$utilization'\nrefresh_button = 'middle'",
        ))
        .expect("nothing here wants the right button");
    }

    #[test]
    fn group_collapse_defaults_requirements_and_style_cascade() {
        let prefix = "[left]\ngroups = ['system']\n[group.system]\nmodules = []\n";
        let parse = |extra: &str| Config::parse(&format!("{prefix}{extra}"));
        let cfg = parse("").unwrap();
        assert_eq!(cfg.positions[0].groups[0].name, "system");
        assert!(cfg.positions[0].groups[0].collapse.is_none());
        for (extra, message) in [
            ("collapsible = true", "collapse_button"),
            ("collapsible = true\ncollapse_button = 'right'", "icon"),
            ("collapsible = true\ncollapse_button = 'wheel'", "wheel"),
            (
                "collapsible = true\ncollapse_button = 'right'\ncollapsed = { icon = 'bogus' }",
                "unknown icon",
            ),
            (
                "collapsible = true\ncollapse_button = 'right'\ncollapsed = { icon = 'none' }",
                "icon",
            ),
            (
                "collapsible = true\ncollapse_button = 'right'\ncollapsed = { icon = 'cpu', icon_size = 0 }",
                "icon_size",
            ),
            (
                "collapsible = true\ncollapse_button = 'right'\ncollapsed = { icon = 'cpu', icon_size = -1 }",
                "icon_size",
            ),
            (
                "collapsible = true\ncollapse_button = 'right'\ncollapsed = { icon = 'cpu', icon_size = inf }",
                "icon_size",
            ),
            (
                "collapsible = true\ncollapse_button = 'right'\ncollapsed = { style = 'missing' }",
                "unknown style",
            ),
            ("collapsed = { typo = 1 }", "unknown field"),
        ] {
            let error = format!("{:#}", parse(extra).unwrap_err());
            assert!(error.contains(message), "{error}");
        }
        for button in ["left", "middle", "right"] {
            let cfg = parse(&format!(
                "collapsible = true\ncollapse_button = '{button}'\ncollapsed = {{ icon = 'cpu' }}"
            ))
            .unwrap();
            let collapse = cfg.positions[0].groups[0].collapse.as_ref().unwrap();
            assert_eq!(collapse.button.name(), button);
            assert_eq!(collapse.style.padding, Style::default().padding);
            assert_eq!(collapse.style.icon_size, cfg.bar.icon_size);
        }
        let cfg = parse("collapsible = true\ncollapse_button = 'middle'\ncollapsed = { style = 'tile', icon = 'cpu', padding = 7 }\n[style.tile]\nbackground = '#123456'\nicon = 'memory'\nicon_size = 12\npadding = 3").unwrap();
        let style = cfg.positions[0].groups[0].collapse.as_ref().unwrap().style;
        assert_eq!(style.icon, Icon::parse("cpu"));
        assert_eq!(style.icon_size, 12.0);
        assert_eq!(style.padding, 7.0);
        assert_eq!(style.background, Color::parse("#123456").unwrap());
        assert!(
            parse("collapsible = false\ncollapse_button = 'right'\ncollapsed = { icon = 'cpu' }")
                .unwrap()
                .positions[0]
                .groups[0]
                .collapse
                .is_none()
        );
        // Group reservation must not relax conflicting bindings inside the child.
        let error = Config::parse("[left]\ngroups = ['g']\n[group.g]\nmodules = ['m']\ncollapsible = true\ncollapse_button = 'left'\ncollapsed = { icon = 'cpu' }\n[module.m]\nformat_alt = 'alt'\non_click = { left = ['true'] }").unwrap_err();
        assert!(format!("{error:#}").contains("on_click.left"));
    }
}
