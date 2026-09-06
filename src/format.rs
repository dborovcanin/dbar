//! The format grammar.
//!
//! A format is the sentence a module says, written against the fields its source publishes:
//!
//! ```text
//! " $icon $used.n(d:1) {of $total} "
//! ```
//!
//! The grammar is deliberately small, and has no conditionals. What would otherwise need an
//! `if` is expressed two ways instead:
//!
//! - a **group** in braces disappears whole when a field inside it has nothing to report, so
//!   `{of $total}` is simply absent on a machine that cannot say what the total is - and the
//!   format itself is a group, so a module with nothing to report draws nothing at all;
//! - a **chain** falls through alternatives, so `$ssid|$device|'offline'` says the best thing
//!   it can.
//!
//! Formatting never consumes the values it reads. Thresholds and graded icons go on reading
//! the numbers the source measured, whatever the text ends up saying.
//!
//! ```text
//! format      ::= item*
//! item        ::= literal | placeholder | group
//! placeholder ::= ( '$' name | '${' name '}' ) ( '.' func )? ( '|' alternative )*
//! group       ::= '{' item* '}'
//! func        ::= ident '(' arg ( ',' arg )* ')'
//! arg         ::= ident ':' ( number | ident | quoted )
//! escape      ::= '$$' | '{{' | '}}'
//! ```

use anyhow::{Result, anyhow, bail};

use crate::status::{FieldSpec, Kind, Unit, Value};

/// How many significant digits a scaled number keeps when the format does not say.
///
/// Three is what reads well in a bar: `7.42 GiB`, `74.2 MiB`, `742 KiB`.
const SIGNIFICANT: i32 = 3;

/// A parsed format string.
#[derive(Clone, Debug, Default)]
pub struct Format {
    items: Vec<Item>,
}

#[derive(Clone, Debug)]
enum Item {
    Literal(String),
    /// A field reference, possibly with alternatives to fall back on.
    Chain(Vec<Alt>),
    /// Dropped whole when a chain directly inside it has nothing to report.
    Group(Vec<Item>),
}

#[derive(Clone, Debug)]
enum Alt {
    Field {
        name: String,
        func: Option<Func>,
    },
    /// A quoted last resort: `$ssid|'offline'`.
    Literal(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scale {
    /// Steps of 1000, with SI prefixes.
    Si,
    /// Steps of 1024, with binary prefixes.
    Bin,
    /// Never scaled: a percentage is not measured in kilopercent.
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sign {
    Auto,
    Always,
}

#[derive(Clone, Copy, Debug, Default)]
struct NumArgs {
    /// Fixed decimal places. Without it the number keeps three significant digits.
    decimals: Option<usize>,
    /// Minimum width, space padded on the left so columns of numbers line up.
    width: usize,
    scale: Option<Scale>,
    /// Force one prefix rather than picking by magnitude, so a value that crosses a
    /// boundary does not make the bar jump.
    prefix: Option<Prefix>,
    sign: Option<Sign>,
    /// Whether the unit is printed. On by default for units that have one.
    suffix: Option<bool>,
}

#[derive(Clone, Debug, Default)]
struct StrArgs {
    /// Minimum width, space padded on the right.
    width: usize,
    /// Longest the text may be, in characters.
    max: Option<usize>,
    ellipsis: Option<String>,
}

/// A strftime pattern, checked when the config is read.
#[derive(Clone, Debug)]
struct TimeArgs {
    pattern: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum DurStyle {
    /// `1:23:45`, dropping leading parts that are zero.
    #[default]
    Hms,
    /// `1h23m`: the two largest units that say anything.
    Short,
}

#[derive(Clone, Debug)]
enum Func {
    Num(NumArgs),
    Str(StrArgs),
    Time(TimeArgs),
    Dur(DurStyle),
    Upper,
    Lower,
}

impl Func {
    fn name(&self) -> &'static str {
        match self {
            Func::Num(_) => "n",
            Func::Str(_) => "str",
            Func::Time(_) => "time",
            Func::Dur(_) => "dur",
            Func::Upper => "up",
            Func::Lower => "low",
        }
    }

    /// The kind of value this can format. A function that cannot make sense of a field is
    /// a mistake worth reporting when the config is read, not a surprise at render time.
    fn accepts(&self, kind: Kind) -> bool {
        match self {
            Func::Num(_) => matches!(kind, Kind::Num(_)),
            Func::Str(_) | Func::Upper | Func::Lower => matches!(kind, Kind::Text),
            Func::Time(_) => matches!(kind, Kind::Time),
            Func::Dur(_) => matches!(kind, Kind::Dur),
        }
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

struct Parser<'a> {
    input: &'a str,
    pos: usize,
}

impl Format {
    pub fn parse(input: &str) -> Result<Format> {
        let mut parser = Parser { input, pos: 0 };
        // Only a group stops the scan early, and there is no open group here, so this
        // either consumes the whole input or reports why it could not.
        Ok(Format {
            items: parser.items(false)?,
        })
    }

    /// Check every field this format names against what the source can publish.
    pub fn check(&self, spec: &[FieldSpec]) -> Result<()> {
        check_items(&self.items, spec)
    }
}

fn check_items(items: &[Item], spec: &[FieldSpec]) -> Result<()> {
    for item in items {
        match item {
            Item::Literal(_) => {}
            Item::Group(inner) => check_items(inner, spec)?,
            Item::Chain(alts) => {
                for alt in alts {
                    let Alt::Field { name, func } = alt else {
                        continue;
                    };
                    let field = spec.iter().find(|f| f.name == name).ok_or_else(|| {
                        let known: Vec<&str> = spec.iter().map(|f| f.name).collect();
                        anyhow!(
                            "this source publishes no field ${name}; it has {}",
                            if known.is_empty() {
                                "none".to_string()
                            } else {
                                known.join(", ")
                            }
                        )
                    })?;
                    if let Some(func) = func
                        && !func.accepts(field.kind)
                    {
                        bail!(
                            "${name} is {} and cannot be formatted with .{}()",
                            field.kind.describe(),
                            func.name()
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

impl<'a> Parser<'a> {
    fn rest(&self) -> &'a str {
        &self.input[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += c.len_utf8();
            return true;
        }
        false
    }

    /// Parse until the end of the input, or until the `}` that closes a group.
    fn items(&mut self, in_group: bool) -> Result<Vec<Item>> {
        let mut items: Vec<Item> = Vec::new();
        let mut literal = String::new();

        macro_rules! flush {
            () => {
                if !literal.is_empty() {
                    items.push(Item::Literal(std::mem::take(&mut literal)));
                }
            };
        }

        while let Some(c) = self.peek() {
            match c {
                '$' => {
                    self.pos += 1;
                    // `$$` is how a format says a literal dollar.
                    if self.eat('$') {
                        literal.push('$');
                        continue;
                    }
                    flush!();
                    items.push(Item::Chain(self.chain()?));
                }
                '{' => {
                    self.pos += 1;
                    if self.eat('{') {
                        literal.push('{');
                        continue;
                    }
                    flush!();
                    let inner = self.items(true)?;
                    if !self.eat('}') {
                        bail!("unclosed `{{` in format");
                    }
                    items.push(Item::Group(inner));
                }
                '}' => {
                    // Inside a group a brace always closes it. Reading `}}` as an escape
                    // here would swallow the two closers that end `{$used{ of $total}}`,
                    // which is an ordinary thing to write; a literal brace inside a group
                    // is not.
                    if in_group {
                        break;
                    }
                    if self.input[self.pos..].starts_with("}}") {
                        self.pos += 2;
                        literal.push('}');
                        continue;
                    }
                    // A stray `}` at the top level is a typo for `}}`, and saying so beats
                    // silently drawing a brace.
                    bail!("unmatched `}}` in format; write `}}}}` for a literal brace");
                }
                _ => {
                    self.pos += c.len_utf8();
                    literal.push(c);
                }
            }
        }
        flush!();
        Ok(items)
    }

    /// A field reference and any alternatives it falls back on.
    fn chain(&mut self) -> Result<Vec<Alt>> {
        let mut alts = vec![self.field()?];
        // `|` only separates alternatives when it sits tight against them. That leaves a
        // spaced ` | ` free to be the literal divider people write between two modules.
        while self.rest().starts_with('|')
            && matches!(self.input[self.pos + 1..].chars().next(), Some('$' | '\''))
        {
            self.pos += 1;
            if self.peek() == Some('\'') {
                alts.push(Alt::Literal(self.quoted()?));
            } else {
                self.pos += 1; // the `$`
                alts.push(self.field()?);
            }
        }
        Ok(alts)
    }

    /// A field name, with the `$` already consumed, and its optional function.
    fn field(&mut self) -> Result<Alt> {
        let braced = self.eat('{');
        let name = self.ident();
        if name.is_empty() {
            bail!("expected a field name after `$` in format");
        }
        let func = if self.eat('.') {
            Some(self.func()?)
        } else {
            None
        };
        if braced && !self.eat('}') {
            bail!("unclosed `${{` around field {name:?}");
        }
        Ok(Alt::Field { name, func })
    }

    fn ident(&mut self) -> String {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == '_' {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
        self.input[start..self.pos].to_string()
    }

    /// A single-quoted literal. `''` inside one is an escaped quote.
    fn quoted(&mut self) -> Result<String> {
        if !self.eat('\'') {
            bail!("expected a quoted value in format");
        }
        let mut out = String::new();
        loop {
            let Some(c) = self.peek() else {
                bail!("unclosed quote in format");
            };
            self.pos += c.len_utf8();
            if c != '\'' {
                out.push(c);
                continue;
            }
            if self.eat('\'') {
                out.push('\'');
                continue;
            }
            return Ok(out);
        }
    }

    fn func(&mut self) -> Result<Func> {
        let name = self.ident();
        let mut args: Vec<(String, String)> = Vec::new();
        if self.eat('(') {
            while !self.eat(')') {
                let key = self.ident();
                if key.is_empty() {
                    bail!("expected an argument name in .{name}()");
                }
                if !self.eat(':') {
                    bail!("argument {key:?} in .{name}() needs a `:` and a value");
                }
                let value = if self.peek() == Some('\'') {
                    self.quoted()?
                } else {
                    let start = self.pos;
                    while let Some(c) = self.peek() {
                        if c == ',' || c == ')' {
                            break;
                        }
                        self.pos += c.len_utf8();
                    }
                    self.input[start..self.pos].trim().to_string()
                };
                args.push((key, value));
                if self.eat(',') {
                    continue;
                }
                if !self.eat(')') {
                    bail!("unclosed `(` in .{name}()");
                }
                break;
            }
        }
        build_func(&name, &args)
    }
}

fn build_func(name: &str, args: &[(String, String)]) -> Result<Func> {
    let number = |v: &str, what: &str| -> Result<usize> {
        v.parse::<usize>()
            .map_err(|_| anyhow!("{what} in .{name}() wants a whole number, not {v:?}"))
    };

    match name {
        "n" => {
            let mut out = NumArgs::default();
            for (key, value) in args {
                match key.as_str() {
                    "d" => out.decimals = Some(number(value, "d")?),
                    "w" => out.width = number(value, "w")?,
                    "scale" => {
                        out.scale = Some(match value.as_str() {
                            "si" => Scale::Si,
                            "bin" => Scale::Bin,
                            "none" => Scale::None,
                            other => bail!("scale in .n() is si, bin or none, not {other:?}"),
                        })
                    }
                    "prefix" => out.prefix = Some(Prefix::parse(value)?),
                    "sign" => {
                        out.sign = Some(match value.as_str() {
                            "auto" => Sign::Auto,
                            "always" => Sign::Always,
                            other => bail!("sign in .n() is auto or always, not {other:?}"),
                        })
                    }
                    "suffix" => {
                        out.suffix = Some(match value.as_str() {
                            "on" | "true" => true,
                            "off" | "false" => false,
                            other => bail!("suffix in .n() is on or off, not {other:?}"),
                        })
                    }
                    other => {
                        bail!(".n() takes d, w, scale, prefix, sign and suffix, not {other:?}")
                    }
                }
            }
            Ok(Func::Num(out))
        }
        "str" => {
            let mut out = StrArgs::default();
            for (key, value) in args {
                match key.as_str() {
                    "w" => out.width = number(value, "w")?,
                    "max" => out.max = Some(number(value, "max")?),
                    "ell" => out.ellipsis = Some(value.clone()),
                    other => bail!(".str() takes w, max and ell, not {other:?}"),
                }
            }
            Ok(Func::Str(out))
        }
        "time" => {
            let mut pattern = None;
            for (key, value) in args {
                match key.as_str() {
                    "f" => pattern = Some(value.clone()),
                    other => bail!(".time() takes f, not {other:?}"),
                }
            }
            let pattern = pattern.ok_or_else(|| anyhow!(".time() needs a pattern, as f:'%R'"))?;
            check_pattern(&pattern)?;
            Ok(Func::Time(TimeArgs { pattern }))
        }
        "dur" => {
            let mut style = DurStyle::default();
            for (key, value) in args {
                match key.as_str() {
                    "style" => {
                        style = match value.as_str() {
                            "hms" => DurStyle::Hms,
                            "short" => DurStyle::Short,
                            other => bail!("style in .dur() is hms or short, not {other:?}"),
                        }
                    }
                    other => bail!(".dur() takes style, not {other:?}"),
                }
            }
            Ok(Func::Dur(style))
        }
        "up" | "low" => {
            if let Some((key, _)) = args.first() {
                bail!(".{name}() takes no arguments, but was given {key:?}");
            }
            Ok(if name == "up" {
                Func::Upper
            } else {
                Func::Lower
            })
        }
        other => bail!("unknown format function .{other}()"),
    }
}

// ---------------------------------------------------------------------------
// Prefixes
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Prefix {
    symbol: &'static str,
    /// Base-10 exponent for SI, or the power of 1024 for binary.
    step: i32,
    binary: bool,
}

const SI: [Prefix; 7] = [
    Prefix {
        symbol: "m",
        step: -1,
        binary: false,
    },
    Prefix {
        symbol: "",
        step: 0,
        binary: false,
    },
    Prefix {
        symbol: "k",
        step: 1,
        binary: false,
    },
    Prefix {
        symbol: "M",
        step: 2,
        binary: false,
    },
    Prefix {
        symbol: "G",
        step: 3,
        binary: false,
    },
    Prefix {
        symbol: "T",
        step: 4,
        binary: false,
    },
    Prefix {
        symbol: "P",
        step: 5,
        binary: false,
    },
];

const BIN: [Prefix; 6] = [
    Prefix {
        symbol: "",
        step: 0,
        binary: true,
    },
    Prefix {
        symbol: "Ki",
        step: 1,
        binary: true,
    },
    Prefix {
        symbol: "Mi",
        step: 2,
        binary: true,
    },
    Prefix {
        symbol: "Gi",
        step: 3,
        binary: true,
    },
    Prefix {
        symbol: "Ti",
        step: 4,
        binary: true,
    },
    Prefix {
        symbol: "Pi",
        step: 5,
        binary: true,
    },
];

impl Prefix {
    fn parse(name: &str) -> Result<Prefix> {
        if name == "none" {
            return Ok(SI[1]);
        }
        SI.iter()
            .chain(BIN.iter())
            .find(|p| p.symbol == name && !p.symbol.is_empty())
            .copied()
            .ok_or_else(|| anyhow!("unknown prefix {name:?} in .n(); try k, M, G, Ki, Mi or Gi"))
    }

    fn factor(self) -> f64 {
        let base: f64 = if self.binary { 1024.0 } else { 1000.0 };
        base.powi(self.step)
    }

    /// The prefix that puts `v` in front of the decimal point.
    fn best(v: f64, scale: Scale) -> Prefix {
        let table: &[Prefix] = match scale {
            Scale::None => return SI[1],
            Scale::Si => &SI,
            Scale::Bin => &BIN,
        };
        let v = v.abs();
        if !v.is_finite() || v == 0.0 {
            return table.iter().find(|p| p.step == 0).copied().unwrap_or(SI[1]);
        }
        // Walk up while the value still fills the next step, which keeps the choice the
        // same whether the table starts at 1 or below it.
        let mut best = table[0];
        for candidate in table {
            if v >= candidate.factor() {
                best = *candidate;
            }
        }
        best
    }
}

/// What a unit prints after the number, and whether a space comes first.
fn unit_suffix(unit: Unit) -> (&'static str, bool) {
    match unit {
        Unit::None => ("", false),
        Unit::Percent => ("%", false),
        Unit::Bytes => ("B", true),
        Unit::BytesPerSec => ("B/s", true),
        Unit::Hertz => ("Hz", true),
        Unit::Celsius => ("°C", false),
        Unit::Watts => ("W", true),
        Unit::Volts => ("V", true),
        Unit::Seconds => ("s", true),
    }
}

/// Whether a unit is worth scaling, and by what steps.
///
/// Bytes step by 1024 because that is what the kernel reports; a percentage never steps at
/// all, because kilopercent is not a thing.
fn default_scale(unit: Unit) -> Scale {
    match unit {
        Unit::Bytes | Unit::BytesPerSec => Scale::Bin,
        Unit::Hertz | Unit::Watts | Unit::Volts => Scale::Si,
        Unit::None | Unit::Percent | Unit::Celsius | Unit::Seconds => Scale::None,
    }
}

/// Units whose fractional part is noise unless a format asks for it.
fn defaults_to_whole(unit: Unit) -> bool {
    matches!(
        unit,
        Unit::None | Unit::Percent | Unit::Celsius | Unit::Seconds
    )
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

impl Format {
    /// Render against the values a source published.
    ///
    /// A field the source did not set reads the same as one it set to `Absent`: it has
    /// nothing to report, which is ordinary rather than an error. Naming a field the source
    /// could never publish is the error, and `check` catches it when the config is read.
    ///
    /// The whole format is itself a group, so a module whose fields have nothing to report
    /// says nothing at all rather than drawing the spaces around the number that is
    /// missing. Whatever should survive a missing field goes in braces.
    pub fn render(&self, fields: &crate::status::Fields) -> String {
        let mut out = String::new();
        if !render_items(&self.items, fields, &mut out) {
            out.clear();
        }
        out
    }
}

/// Render a run of items, treating it as a group: `false` means something inside had
/// nothing to report, and the caller should drop what was written.
fn render_items(items: &[Item], fields: &crate::status::Fields, out: &mut String) -> bool {
    let mut complete = true;
    for item in items {
        match item {
            Item::Literal(text) => out.push_str(text),
            Item::Chain(alts) => match render_chain(alts, fields) {
                Some(text) => out.push_str(&text),
                None => complete = false,
            },
            Item::Group(inner) => {
                // A nested group answers for itself. Its absence is not the outer group's
                // problem, so `{used {of $total}}` still shows what it knows.
                let mark = out.len();
                if !render_items(inner, fields, out) {
                    out.truncate(mark);
                }
            }
        }
    }
    complete
}

fn render_chain(alts: &[Alt], fields: &crate::status::Fields) -> Option<String> {
    for alt in alts {
        match alt {
            Alt::Literal(text) => return Some(text.clone()),
            Alt::Field { name, func } => {
                let Some(value) = fields.get(name) else {
                    continue;
                };
                if let Some(text) = render_value(value, func.as_ref()) {
                    return Some(text);
                }
            }
        }
    }
    None
}

fn render_value(value: &Value, func: Option<&Func>) -> Option<String> {
    match (value, func) {
        (Value::Absent, _) => None,
        (Value::Num { v, unit }, Some(Func::Num(args))) => Some(format_num(*v, *unit, args)),
        (Value::Num { v, unit }, None) => Some(format_num(*v, *unit, &NumArgs::default())),
        (Value::Text(text), Some(Func::Str(args))) => Some(format_str(text, args)),
        (Value::Text(text), Some(Func::Upper)) => Some(text.to_uppercase()),
        (Value::Text(text), Some(Func::Lower)) => Some(text.to_lowercase()),
        (Value::Text(text), None) => Some(text.clone()),
        (Value::Time(t), Some(Func::Time(args))) => format_time(*t, &args.pattern),
        (Value::Time(t), None) => format_time(*t, "%H:%M"),
        (Value::Dur(d), Some(Func::Dur(style))) => Some(format_dur(*d, *style)),
        (Value::Dur(d), None) => Some(format_dur(*d, DurStyle::default())),
        (Value::Flag(b), None) => Some(if *b { "yes" } else { "no" }.to_string()),
        // `check` rejects these when the config is read; a source that publishes a
        // different kind than it declared falls through to saying nothing.
        _ => None,
    }
}

fn format_num(v: f64, unit: Unit, args: &NumArgs) -> String {
    let scale = args.scale.unwrap_or_else(|| default_scale(unit));
    let prefix = args.prefix.unwrap_or_else(|| Prefix::best(v, scale));
    let scaled = if scale == Scale::None && args.prefix.is_none() {
        v
    } else {
        v / prefix.factor()
    };

    let decimals = match args.decimals {
        Some(d) => d,
        None if defaults_to_whole(unit) && args.prefix.is_none() => 0,
        // Keep a fixed number of significant digits, so a value reads the same width
        // whether it is 7.42 GiB or 742 KiB.
        None => {
            let magnitude = scaled.abs();
            let before = if magnitude >= 1.0 {
                magnitude.log10().floor() as i32 + 1
            } else {
                1
            };
            (SIGNIFICANT - before).clamp(0, 6) as usize
        }
    };

    let mut out = String::new();
    if args.sign == Some(Sign::Always) && scaled >= 0.0 {
        out.push('+');
    }
    out.push_str(&format!("{scaled:.decimals$}"));

    let (symbol, spaced) = unit_suffix(unit);
    if args.suffix.unwrap_or(true) && !(symbol.is_empty() && prefix.symbol.is_empty()) {
        if spaced {
            out.push(' ');
        }
        out.push_str(prefix.symbol);
        out.push_str(symbol);
    }

    // Numbers pad on the left, so a column of them lines up on the decimal point.
    let width = args.width.saturating_sub(out.chars().count());
    if width > 0 {
        out.insert_str(0, &" ".repeat(width));
    }
    out
}

fn format_str(text: &str, args: &StrArgs) -> String {
    let ellipsis = args.ellipsis.as_deref().unwrap_or("\u{2026}");
    let mut out = match args.max {
        Some(max) if text.chars().count() > max => {
            let keep = max.saturating_sub(ellipsis.chars().count());
            let cut = text
                .char_indices()
                .nth(keep)
                .map(|(i, _)| i)
                .unwrap_or(text.len());
            format!("{}{ellipsis}", &text[..cut])
        }
        _ => text.to_string(),
    };
    // Text pads on the right, the way a label sits in a column.
    let width = args.width.saturating_sub(out.chars().count());
    if width > 0 {
        out.push_str(&" ".repeat(width));
    }
    out
}

/// Reject a strftime pattern that would draw itself instead of the time.
///
/// An unknown specifier is written out literally rather than reported, so a mistyped `%Q`
/// would leave a clock reading `%Q` forever. Since a real `%` has to be written `%%`, a
/// pattern that has no `%%` and still produces one did not consume something.
///
/// A pattern that mixes `%%` with a typo goes unreported. It still prints the `%%`
/// correctly, and chasing the remainder would mean keeping our own list of every
/// specifier the calendar understands.
fn check_pattern(pattern: &str) -> Result<()> {
    let at = jiff::Timestamp::UNIX_EPOCH.to_zoned(jiff::tz::TimeZone::UTC);
    let rendered = jiff::fmt::strtime::format(pattern, &at)
        .map_err(|e| anyhow!("in .time(f:{pattern:?}): {e}"))?;
    if !pattern.contains("%%") && rendered.contains('%') {
        bail!("in .time(f:{pattern:?}): the calendar does not know one of these specifiers");
    }
    Ok(())
}

/// Render an instant in the machine's own time zone.
fn strftime(pattern: &str, at: jiff::Timestamp) -> Result<String> {
    let zoned = at.to_zoned(jiff::tz::TimeZone::system());
    jiff::fmt::strtime::format(pattern, &zoned).map_err(|e| anyhow!("{e}"))
}

fn format_time(at: std::time::SystemTime, pattern: &str) -> Option<String> {
    let timestamp = jiff::Timestamp::try_from(at).ok()?;
    // The pattern was checked when the config was read, so a failure here is a clock the
    // calendar cannot express rather than a typo.
    strftime(pattern, timestamp).ok()
}

fn format_dur(d: std::time::Duration, style: DurStyle) -> String {
    let total = d.as_secs();
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    match style {
        DurStyle::Hms if h > 0 => format!("{h}:{m:02}:{s:02}"),
        DurStyle::Hms => format!("{m}:{s:02}"),
        // Two units is as much as a bar has room for, and the smallest one is noise once
        // the largest is hours.
        DurStyle::Short if h > 0 => format!("{h}h{m:02}m"),
        DurStyle::Short if m > 0 => format!("{m}m{s:02}s"),
        DurStyle::Short => format!("{s}s"),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::status::Fields;

    fn render(format: &str, fields: &Fields) -> String {
        Format::parse(format)
            .unwrap_or_else(|e| panic!("{format:?} should parse: {e:#}"))
            .render(fields)
    }

    fn num(v: f64, unit: Unit) -> Value {
        Value::Num { v, unit }
    }

    fn fields(entries: &[(&'static str, Value)]) -> Fields {
        let mut out = Fields::default();
        for (name, value) in entries {
            out.set(name, value.clone());
        }
        out
    }

    /// The shape a fetched reading is written in: everything optional in a group, and one
    /// chain with a last resort, so a module whose source could say nothing still draws
    /// something. A module that draws nothing has nothing left to click, and a click is
    /// how a command that answers on demand is asked to try again.
    #[test]
    fn a_chain_with_a_last_resort_keeps_a_module_on_the_bar() {
        let format = "{$icon }$weather|'unavailable'{ ($location)}{ $temp.n(d:0) °C}";
        let got = fields(&[
            ("icon", Value::Text("☁".into())),
            ("weather", Value::Text("Clouds".into())),
            ("location", Value::Text("Novi Sad".into())),
            ("temp", num(21.4, Unit::None)),
        ]);
        assert_eq!(render(format, &got), "☁ Clouds (Novi Sad) 21 °C");
        // Nothing came back at all, which is what a failed fetch looks like.
        assert_eq!(render(format, &Fields::default()), "unavailable");
    }

    #[test]
    fn literal_text_passes_through() {
        assert_eq!(render(" hello ", &Fields::default()), " hello ");
    }

    #[test]
    fn escapes_produce_their_own_character() {
        assert_eq!(render("$${{}}", &Fields::default()), "${}");
    }

    #[test]
    fn a_field_renders_by_its_unit() {
        let f = fields(&[
            ("pct", num(58.0, Unit::Percent)),
            ("mem", num(7_950_000_000.0, Unit::Bytes)),
            ("temp", num(61.4, Unit::Celsius)),
            ("name", Value::Text("eth0".into())),
        ]);
        assert_eq!(render("$pct", &f), "58%");
        assert_eq!(render("$mem", &f), "7.40 GiB");
        assert_eq!(render("$temp", &f), "61°C");
        assert_eq!(render("$name", &f), "eth0");
    }

    #[test]
    fn braces_delimit_a_field_name() {
        let f = fields(&[("up", num(1000.0, Unit::BytesPerSec))]);
        assert_eq!(render("${up}s", &f), "1000 B/ss");
    }

    #[test]
    fn significant_digits_keep_the_width_steady() {
        for (bytes, expected) in [
            (742.0, "742 B"),
            (759_808.0, "742 KiB"),
            (778_043_392.0, "742 MiB"),
            (7_964_600_832.0, "7.42 GiB"),
        ] {
            let f = fields(&[("v", num(bytes, Unit::Bytes))]);
            assert_eq!(render("$v", &f), expected, "for {bytes} bytes");
        }
    }

    #[test]
    fn a_forced_prefix_stops_the_bar_jumping() {
        let f = fields(&[("v", num(759_808.0, Unit::Bytes))]);
        assert_eq!(render("$v.n(prefix:Mi,d:2)", &f), "0.72 MiB");
    }

    #[test]
    fn numbers_take_decimals_width_sign_and_suffix() {
        let f = fields(&[("v", num(5.0, Unit::Percent))]);
        assert_eq!(render("$v.n(d:1)", &f), "5.0%");
        assert_eq!(render("$v.n(w:5)", &f), "   5%");
        assert_eq!(render("$v.n(sign:always)", &f), "+5%");
        assert_eq!(render("$v.n(suffix:off)", &f), "5");
    }

    #[test]
    fn a_percentage_is_never_scaled() {
        let f = fields(&[("v", num(4200.0, Unit::Percent))]);
        assert_eq!(render("$v", &f), "4200%");
    }

    #[test]
    fn text_pads_right_and_truncates_with_an_ellipsis() {
        let f = fields(&[("t", Value::Text("workspace".into()))]);
        assert_eq!(render("$t.str(w:12)|", &f), "workspace   |");
        assert_eq!(render("$t.str(max:6)", &f), "works\u{2026}");
        // `max` is the whole budget, ellipsis included.
        assert_eq!(render("$t.str(max:6,ell:'..')", &f), "work..");
        assert_eq!(render("$t.up()", &f), "WORKSPACE");
    }

    #[test]
    fn times_render_through_a_strftime_pattern() {
        let epoch = std::time::UNIX_EPOCH;
        let f = fields(&[("now", Value::Time(epoch))]);
        // Rendered in the machine's own zone, so assert on what cannot drift: the pattern
        // is honoured and the fixed parts come through.
        assert_eq!(render("$now.time(f:'%Y')", &f), "1970");
        assert_eq!(render("$now.time(f:'day %j')", &f), "day 001");
        assert_eq!(render("$now.time(f:'%H:%M')", &f).len(), 5);
        assert_eq!(render("$now", &f).len(), 5);
    }

    #[test]
    fn a_time_pattern_is_checked_when_it_is_written() {
        assert!(Format::parse("$now.time(f:'%R')").is_ok());
        assert!(Format::parse("$now.time()").is_err());
        assert!(Format::parse("$now.time(f:'%+')").is_err());
        // A real percent sign has to be written `%%`, and still works.
        assert!(Format::parse("$now.time(f:'%H%%')").is_ok());
    }

    #[test]
    fn durations_have_two_styles() {
        let f = fields(&[
            ("long", Value::Dur(Duration::from_secs(5025))),
            ("short", Value::Dur(Duration::from_secs(45))),
        ]);
        assert_eq!(render("$long", &f), "1:23:45");
        assert_eq!(render("$long.dur(style:short)", &f), "1h23m");
        assert_eq!(render("$short.dur(style:short)", &f), "45s");
    }

    #[test]
    fn a_group_disappears_when_a_field_inside_it_is_absent() {
        let known = fields(&[
            ("used", num(4.0, Unit::Bytes)),
            ("total", num(8.0, Unit::Bytes)),
        ]);
        assert_eq!(render("$used{ of $total}", &known), "4.00 B of 8.00 B");

        let unknown = fields(&[("used", num(4.0, Unit::Bytes)), ("total", Value::Absent)]);
        assert_eq!(render("$used{ of $total}", &unknown), "4.00 B");
    }

    #[test]
    fn a_field_the_source_never_set_reads_as_absent() {
        let f = fields(&[("used", num(4.0, Unit::Bytes))]);
        assert_eq!(render("$used{ of $total}", &f), "4.00 B");
    }

    #[test]
    fn a_format_with_nothing_to_report_says_nothing_at_all() {
        let f = fields(&[("used", Value::Absent)]);
        // Not " B " - the padding around a missing number would draw an empty box.
        assert_eq!(render(" $used ", &f), "");
        // Braces are how the part that should survive is marked.
        assert_eq!(render(" mem{ $used} ", &f), " mem ");
    }

    #[test]
    fn a_nested_group_answers_only_for_itself() {
        let f = fields(&[
            ("a", Value::Text("a".into())),
            ("b", Value::Text("b".into())),
            ("c", Value::Absent),
        ]);
        assert_eq!(render("{$a{ $b}{ $c}}", &f), "a b");
        // The outer group still goes when its own field has nothing to say.
        assert_eq!(render("{$c{ $b}}", &f), "");
    }

    #[test]
    fn groups_may_close_together() {
        let f = fields(&[
            ("used", num(4.0, Unit::Bytes)),
            ("total", num(8.0, Unit::Bytes)),
        ]);
        // Two closers in a row end two groups; they are not an escaped brace.
        assert_eq!(render("{$used{ of $total}}", &f), "4.00 B of 8.00 B");
    }

    #[test]
    fn a_chain_falls_through_to_the_first_thing_it_can_say() {
        let f = fields(&[
            ("ssid", Value::Absent),
            ("device", Value::Text("wlan0".into())),
        ]);
        assert_eq!(render("$ssid|$device|'offline'", &f), "wlan0");

        let nothing = fields(&[("ssid", Value::Absent), ("device", Value::Absent)]);
        assert_eq!(render("$ssid|$device|'offline'", &nothing), "offline");
    }

    #[test]
    fn a_spaced_pipe_stays_a_literal_divider() {
        let f = fields(&[
            ("a", Value::Text("x".into())),
            ("b", Value::Text("y".into())),
        ]);
        assert_eq!(render("$a | $b", &f), "x | y");
    }

    #[test]
    fn a_quoted_alternative_can_contain_a_quote() {
        let f = fields(&[("a", Value::Absent)]);
        assert_eq!(render("$a|'it''s off'", &f), "it's off");
    }

    #[test]
    fn parsing_rejects_what_it_cannot_mean() {
        for bad in [
            "$",
            "$a.",
            "$a.nope()",
            "$a.n(z:1)",
            "$a.n(d:x)",
            "$a.up(d:1)",
            "{$a",
            "$a}",
            "${a",
            "$a|'unclosed",
        ] {
            assert!(
                Format::parse(bad).is_err(),
                "{bad:?} should not have parsed"
            );
        }
    }

    #[test]
    fn checking_catches_a_field_the_source_cannot_publish() {
        let spec = [FieldSpec {
            name: "text",
            kind: Kind::Text,
        }];
        assert!(Format::parse("$text").unwrap().check(&spec).is_ok());

        let e = Format::parse("$nope")
            .unwrap()
            .check(&spec)
            .expect_err("an unknown field must be reported");
        assert!(e.to_string().contains("$nope"), "{e}");
        assert!(e.to_string().contains("text"), "{e}");
    }

    #[test]
    fn checking_catches_a_function_the_field_cannot_use() {
        let spec = [FieldSpec {
            name: "text",
            kind: Kind::Text,
        }];
        let e = Format::parse("$text.n(d:1)")
            .unwrap()
            .check(&spec)
            .expect_err("a number function on text must be reported");
        assert!(e.to_string().contains(".n()"), "{e}");
    }

    #[test]
    fn checking_reaches_inside_groups_and_chains() {
        let spec = [FieldSpec {
            name: "text",
            kind: Kind::Text,
        }];
        assert!(Format::parse("{$nope}").unwrap().check(&spec).is_err());
        assert!(Format::parse("$text|$nope").unwrap().check(&spec).is_err());
    }
}
