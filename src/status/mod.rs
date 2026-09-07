//! The protocol-neutral status model.
//!
//! Everything that produces data converts into a `StatusItem`: the i3bar backend today,
//! native collectors next. Layout and rendering see nothing else, so no protocol detail
//! reaches the code that draws, and no drawing concern reaches the code that collects.
//!
//! The point of the model is that values survive as values. A source publishes typed
//! fields; formatting turns them into text without consuming them, so thresholds and
//! graded icons read the number the source measured rather than parsing one back out of
//! whatever the text ended up saying.

pub mod i3bar;

use std::time::{Duration, SystemTime};

use crate::color::Color;

pub use i3bar::{ClickEvent, I3BarProvider, StatusEvent};

/// What a number measures.
///
/// The unit decides how a value formats by default: how many decimals are useful, whether
/// prefixes step by 1000 or 1024, and what suffix to print. Without it the formatter would
/// need those spelled out at every use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
// The full set exists so collectors have somewhere to put what they measure; the ones with
// no source yet arrive with their collector.
#[allow(dead_code)]
pub enum Unit {
    None,
    Percent,
    Bytes,
    BytesPerSec,
    Hertz,
    Celsius,
    Watts,
    Volts,
    Seconds,
}

/// One value a source publishes.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub enum Value {
    Num {
        v: f64,
        unit: Unit,
    },
    Text(String),
    Time(SystemTime),
    Dur(Duration),
    Flag(bool),
    /// A field the source knows about but cannot supply right now: no battery is fitted,
    /// the link is down, the sensor is missing.
    ///
    /// Distinct from the field being absent from the map, which is a configuration error.
    /// This one is ordinary, and is what makes a conditional part of a format disappear.
    Absent,
}

impl Value {
    /// The number this holds, if it holds one.
    pub fn num(&self) -> Option<f64> {
        match *self {
            Value::Num { v, .. } => Some(v),
            _ => None,
        }
    }
}

/// What kind of value a field holds, independent of any particular reading.
///
/// A source declares these up front so a format can be checked when the config is read
/// rather than quietly rendering nothing at three in the morning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
// The kinds with no source yet arrive with their collector.
#[allow(dead_code)]
pub enum Kind {
    Num(Unit),
    Text,
    Time,
    Dur,
    Flag,
}

impl Kind {
    /// How to name this kind in a message to whoever wrote the config.
    pub fn describe(self) -> &'static str {
        match self {
            Kind::Num(_) => "a number",
            Kind::Text => "text",
            Kind::Time => "a time",
            Kind::Dur => "a duration",
            Kind::Flag => "a flag",
        }
    }
}

/// One field a source promises to publish.
#[derive(Clone, Copy, Debug)]
pub struct FieldSpec {
    pub name: &'static str,
    pub kind: Kind,
}

/// The values one status item currently carries, in the order the source published them.
///
/// Sources publish a handful of fields each, so a vector beats a hash map on both lookup
/// and allocation, and it keeps the order stable for anything that lists them.
#[derive(Clone, Debug, Default)]
pub struct Fields {
    entries: Vec<(&'static str, Value)>,
    /// The field a state rule keys on when it names none.
    primary: Option<usize>,
}

impl Fields {
    pub fn set(&mut self, name: &'static str, value: Value) {
        match self.entries.iter_mut().find(|(n, _)| *n == name) {
            Some(entry) => entry.1 = value,
            None => self.entries.push((name, value)),
        }
    }

    /// Nominate the field thresholds apply to by default.
    ///
    /// A source knows which of its numbers is the one a user means by "above 80"; asking
    /// every config to name it would be noise.
    pub fn set_primary(&mut self, name: &'static str) {
        self.primary = self.entries.iter().position(|(n, _)| *n == name);
    }

    #[allow(dead_code)] // The formatter is the caller, and it lands with the format grammar.
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.entries
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v)
    }

    pub fn primary(&self) -> Option<&Value> {
        self.entries.get(self.primary?).map(|(_, v)| v)
    }
}

/// How a source rates what it is reporting.
///
/// A scale, not a set of flags: a source is in exactly one of these. Urgency is separate,
/// because it is an input from elsewhere rather than the source's own judgement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
// Collectors are what declare the middle of the scale; the i3bar protocol only distinguishes
// its two ends.
#[allow(dead_code)]
pub enum State {
    #[default]
    Idle,
    Info,
    Good,
    Warning,
    Critical,
    /// The source failed. The item still draws, saying so.
    Error,
}

/// What a click on a module does.
///
/// Hit testing produces one of these rather than an index into some provider's block list,
/// so the same pointer code serves every kind of source.
#[derive(Clone, Debug)]
pub enum ActionTarget {
    /// Send the click back over the i3bar protocol, which identifies blocks by the names
    /// the provider itself gave them.
    I3Bar {
        name: Option<String>,
        instance: Option<String>,
    },
    /// Run a compositor command.
    Sway(String),
    /// Change what the module is showing, by the step a scroll notch is worth.
    ///
    /// The bar is the control as well as the display: scrolling over the brightness is
    /// how a person expects to change it, and having to bind a key to a helper that then
    /// signals the bar is a worse version of the same thing.
    Control { what: Control, step: f64 },
    /// Ask a tray application to act on a click on its own icon.
    ///
    /// The item is named rather than pointed at: a frame outlives nothing, and the tray's
    /// list is the thread's, so what travels back is the key the tray gave it.
    Tray { key: String },
}

/// Something the bar can change, as well as show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Brightness,
    Volume,
    /// A player: the buttons mean play, pause and skip rather than more or less of
    /// something, so the step a scroll carries is not used.
    Media,
}

/// One thing the bar can show, independent of where it came from.
#[derive(Clone, Debug)]
pub struct StatusItem {
    /// The name a config selects this item by.
    ///
    /// Optional because the i3bar protocol lets a provider leave its blocks unnamed, in
    /// which case nothing can select them by name. Native sources always have one.
    pub id: Option<String>,
    /// What the source measured. The text to draw is the module's format applied to these,
    /// so a value is never recovered from a string that was written to be looked at.
    pub fields: Fields,
    /// The source's own rating of what it is reporting. Read once state rules can key on
    /// it, which is a configuration change rather than a model one.
    #[allow(dead_code)]
    pub state: State,
    /// Set by whatever is asking for attention: the provider's own flag, a workspace the
    /// compositor has marked.
    pub urgent: bool,
    /// Colours the source asked for, overriding the configured style.
    pub foreground: Option<Color>,
    pub background: Option<Color>,
    pub action: Option<ActionTarget>,
}
