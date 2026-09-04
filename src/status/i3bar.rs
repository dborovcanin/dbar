//! The i3bar-protocol backend.
//!
//! The provider owns an `i3status-rs` (or any i3bar-compatible) child process. Its stdout is
//! read on a helper thread and forwarded to the event loop over a calloop channel, so the main
//! thread never polls. Click events travel back over the child's stdin.
//!
//! The protocol carries rendered text and little else, so this is the one backend where the
//! text really is the data. What structure can be recovered from it is recovered here, at the
//! boundary, and everything downstream sees ordinary `StatusItem`s.

use std::borrow::Cow;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::color::Color;
use crate::config;
use crate::status::{ActionTarget, FieldSpec, Fields, Kind, State, StatusItem, Unit, Value};

/// One status block as described by the i3bar protocol.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct I3BarBlock {
    #[serde(default)]
    pub full_text: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub instance: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub background: Option<String>,
    /// The provider's own alarm flag, which a module state can key on.
    #[serde(default)]
    pub urgent: bool,
    /// Either `"none"` or `"pango"`; i3status-rs always sends the latter.
    #[serde(default)]
    pub markup: Option<String>,
}

impl I3BarBlock {
    /// The text to actually draw.
    ///
    /// Providers that set `markup = "pango"` may wrap text in span tags and escape it as XML.
    /// V0 has no rich text, so tags are dropped and entities decoded; without this the bar
    /// would literally show `&#39;` and `<span ...>`.
    fn display_text(&self) -> Cow<'_, str> {
        if self.markup.as_deref() != Some("pango") {
            return Cow::Borrowed(&self.full_text);
        }
        if !self.full_text.contains(['<', '&']) {
            return Cow::Borrowed(&self.full_text);
        }
        Cow::Owned(strip_pango(&self.full_text))
    }
}

/// What a block can offer a format.
///
/// The protocol sends rendered text, so this is as much structure as there is: the text
/// itself, and whatever percentage can be read out of it.
pub const FIELDS: &[FieldSpec] = &[
    FieldSpec {
        name: "text",
        kind: Kind::Text,
    },
    FieldSpec {
        name: "percent",
        kind: Kind::Num(Unit::Percent),
    },
];

/// Turn one provider update into status items.
///
/// `names` are the names the config gave the provider's blocks, by position, because the
/// protocol gives them none worth selecting on - i3status-rs numbers them. Positions are
/// only trustworthy once the provider has emitted every block it is going to: until then
/// the list is short and every name after a missing block would land on the wrong one, so
/// the blocks keep whatever the provider called them for a moment instead.
pub fn to_items(blocks: &[I3BarBlock], names: &[String]) -> Vec<StatusItem> {
    let named = blocks.len() >= names.len();
    blocks
        .iter()
        .enumerate()
        .map(|(i, block)| {
            let text = block.display_text().into_owned();

            // The protocol sends text, so the only value that can be recovered is whatever
            // percentage the text spells out. A native source publishes what it measured.
            let mut fields = Fields::default();
            if let Some(p) = percent(&text) {
                fields.set(
                    "percent",
                    Value::Num {
                        v: p as f64,
                        unit: Unit::Percent,
                    },
                );
                fields.set_primary("percent");
            }
            fields.set("text", Value::Text(text.clone()));

            let id = named
                .then(|| names.get(i).cloned())
                .flatten()
                .or_else(|| block.name.clone());

            StatusItem {
                id,
                fields,
                // The protocol has no state scale; the urgent flag is the whole of it.
                state: if block.urgent {
                    State::Critical
                } else {
                    State::Idle
                },
                urgent: block.urgent,
                foreground: block.color.as_deref().and_then(|c| Color::parse(c).ok()),
                background: block
                    .background
                    .as_deref()
                    .and_then(|c| Color::parse(c).ok()),
                // Clicks must carry the name the provider gave the block, not the one the
                // config chose, or the provider cannot route them back.
                action: Some(ActionTarget::I3Bar {
                    name: block.name.clone(),
                    instance: block.instance.clone(),
                }),
            }
        })
        .collect()
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Header {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub click_events: bool,
}

/// What the reader thread sends to the event loop.
#[derive(Debug)]
pub enum StatusEvent {
    Header(Header),
    Blocks(Vec<I3BarBlock>),
    /// The child exited or its stdout closed; the string explains why.
    Stopped(String),
}

/// A click delivered back to the status provider.
#[derive(Debug, Serialize)]
pub struct ClickEvent<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<&'a str>,
    pub button: u32,
    /// Pointer position in bar-surface coordinates.
    pub x: i32,
    pub y: i32,
    /// Position relative to the clicked block.
    pub relative_x: i32,
    pub relative_y: i32,
    pub width: i32,
    pub height: i32,
}

pub struct I3BarProvider {
    child: Child,
    stdin: Option<ChildStdin>,
    /// The protocol wants the click stream opened with a `[`, then comma-separated objects.
    click_stream_open: bool,
    accepts_clicks: bool,
}

impl I3BarProvider {
    /// Spawn the provider and start forwarding its output into `sender`.
    pub fn spawn(
        cfg: &config::Status,
        sender: calloop::channel::Sender<StatusEvent>,
    ) -> Result<I3BarProvider> {
        // In generated mode dbar owns the provider's configuration, and passes its path as
        // the only argument.
        let mut args = cfg.args.clone();
        if let Some(generated) = &cfg.generated {
            let path = write_generated(generated)?;
            log::info!("wrote provider config to {}", path.display());
            args = vec![path.to_string_lossy().into_owned()];
        }

        let mut child = Command::new(&cfg.command)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Leave stderr attached so provider diagnostics reach our own log.
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("spawning status command {:?}", cfg.command))?;

        let stdout = child.stdout.take().expect("stdout was piped");
        let stdin = child.stdin.take();

        std::thread::Builder::new()
            .name("status-reader".to_string())
            .spawn(move || read_loop(stdout, sender))
            .context("spawning status reader thread")?;

        Ok(I3BarProvider {
            child,
            stdin,
            click_stream_open: false,
            accepts_clicks: false,
        })
    }

    pub fn set_accepts_clicks(&mut self, yes: bool) {
        self.accepts_clicks = yes;
    }

    /// Forward a click to the provider. Errors are logged rather than fatal: a provider that
    /// ignores clicks should not take the bar down.
    pub fn send_click(&mut self, event: &ClickEvent<'_>) {
        if !self.accepts_clicks {
            return;
        }
        let Some(stdin) = self.stdin.as_mut() else {
            return;
        };
        let json = match serde_json::to_string(event) {
            Ok(j) => j,
            Err(e) => {
                log::warn!("serializing click event: {e}");
                return;
            }
        };
        let result = (|| -> std::io::Result<()> {
            if !self.click_stream_open {
                stdin.write_all(b"[\n")?;
                self.click_stream_open = true;
            } else {
                stdin.write_all(b",")?;
            }
            stdin.write_all(json.as_bytes())?;
            stdin.write_all(b"\n")?;
            stdin.flush()
        })();
        if let Err(e) = result {
            log::warn!("sending click event: {e}");
            // A broken pipe will not heal; stop trying.
            self.stdin = None;
        }
    }
}

impl Drop for I3BarProvider {
    fn drop(&mut self) {
        // Close stdin first so a well-behaved provider exits on its own.
        self.stdin = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Write the generated provider configuration, and return its path.
///
/// It lives in the runtime directory rather than a temporary file, so it survives for the
/// life of the session and can be read when diagnosing what the provider was told.
fn write_generated(generated: &config::Generated) -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join("dbar");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let path = dir.join("provider.toml");
    let body = generated.to_toml()?;
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

fn read_loop(stdout: std::process::ChildStdout, sender: calloop::channel::Sender<StatusEvent>) {
    let reader = BufReader::new(stdout);
    let mut header_seen = false;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                let _ = sender.send(StatusEvent::Stopped(format!("read error: {e}")));
                return;
            }
        };
        // The protocol frames updates as a never-ending JSON array: an opening `[` on its own
        // line, then one array per update. Implementations differ on whether the separating
        // comma leads or trails the line, so strip it from either end.
        let trimmed = line
            .trim()
            .trim_start_matches(',')
            .trim_end_matches(',')
            .trim();
        if trimmed.is_empty() || trimmed == "[" {
            continue;
        }

        if !header_seen {
            match serde_json::from_str::<Header>(trimmed) {
                Ok(header) => {
                    header_seen = true;
                    if sender.send(StatusEvent::Header(header)).is_err() {
                        return;
                    }
                    continue;
                }
                Err(e) => {
                    log::warn!("status provider sent no usable header ({e}); continuing anyway");
                    header_seen = true;
                    // Fall through: this line may already be the first update.
                }
            }
        }

        match serde_json::from_str::<Vec<I3BarBlock>>(trimmed) {
            Ok(blocks) => {
                if sender.send(StatusEvent::Blocks(blocks)).is_err() {
                    return;
                }
            }
            Err(e) => log::warn!("ignoring unparsable status line: {e}"),
        }
    }

    let _ = sender.send(StatusEvent::Stopped(
        "status provider closed its output".to_string(),
    ));
}

/// Drop pango markup tags and decode XML entities.
fn strip_pango(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // Skip the whole tag. Unterminated tags simply consume the rest of the string,
            // which matches how a lenient markup parser would treat them.
            '<' => {
                for c in chars.by_ref() {
                    if c == '>' {
                        break;
                    }
                }
            }
            '&' => {
                let mut entity = String::new();
                let mut terminated = false;
                while let Some(&c) = chars.peek() {
                    chars.next();
                    if c == ';' {
                        terminated = true;
                        break;
                    }
                    // A bare `&` is common enough that we should not swallow the rest of a word.
                    if entity.len() > 8 || c.is_whitespace() {
                        break;
                    }
                    entity.push(c);
                }
                match decode_entity(&entity) {
                    Some(decoded) if terminated => out.push(decoded),
                    _ => {
                        out.push('&');
                        out.push_str(&entity);
                        if terminated {
                            out.push(';');
                        }
                    }
                }
            }
            _ => out.push(c),
        }
    }
    out
}

fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => {
            let digits = entity.strip_prefix('#')?;
            let code = match digits.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => digits.parse::<u32>().ok()?,
            };
            char::from_u32(code)
        }
    }
}

/// The first percentage in `text`, as 0..=100.
///
/// Only `NN%` counts. Values such as "92GB", "23:59" or "3h 5m" carry no percent sign, so
/// nothing keyed on a percentage fires on a number that means something else.
fn percent(text: &str) -> Option<u8> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'%' {
            return text[start..i].parse::<u32>().ok().map(|v| v.min(100) as u8);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(name: &str, text: &str) -> I3BarBlock {
        I3BarBlock {
            full_text: text.to_string(),
            name: Some(name.to_string()),
            ..I3BarBlock::default()
        }
    }

    #[test]
    fn names_are_applied_by_position() {
        let blocks = [block("0", "a"), block("1", "b")];
        let names = ["cpu".to_string(), "mem".to_string()];
        let items = to_items(&blocks, &names);
        assert_eq!(items[0].id.as_deref(), Some("cpu"));
        assert_eq!(items[1].id.as_deref(), Some("mem"));
    }

    #[test]
    fn names_are_held_back_until_the_counts_agree() {
        let blocks = [block("0", "a")];
        let names = ["cpu".to_string(), "mem".to_string()];
        let items = to_items(&blocks, &names);
        // Naming a short list would show one block's value under another block's name.
        assert_eq!(items[0].id.as_deref(), Some("0"));
    }

    #[test]
    fn extra_blocks_keep_the_providers_names() {
        let blocks = [block("0", "a"), block("1", "b")];
        let names = ["cpu".to_string()];
        let items = to_items(&blocks, &names);
        assert_eq!(items[0].id.as_deref(), Some("cpu"));
        assert_eq!(items[1].id.as_deref(), Some("1"));
    }

    #[test]
    fn a_percentage_in_the_text_becomes_the_primary_value() {
        let items = to_items(&[block("bat", " 58% 1:20")], &[]);
        assert_eq!(items[0].fields.primary().and_then(|v| v.num()), Some(58.0));
    }

    #[test]
    fn text_without_a_percent_sign_publishes_no_number() {
        for text in [" 92GB ", " 23:59 ", " 3h 5m "] {
            let items = to_items(&[block("x", text)], &[]);
            assert!(
                items[0].fields.primary().is_none(),
                "{text:?} should not read as a percentage"
            );
        }
    }

    #[test]
    fn urgent_blocks_arrive_urgent_and_critical() {
        let mut b = block("x", "!");
        b.urgent = true;
        let items = to_items(&[b], &[]);
        assert!(items[0].urgent);
        assert_eq!(items[0].state, State::Critical);
    }

    #[test]
    fn provider_colours_are_parsed_at_the_boundary() {
        let mut b = block("x", "hi");
        b.color = Some("#ff0000".to_string());
        let items = to_items(&[b], &[]);
        assert_eq!(items[0].foreground, Some(Color::rgba(0xff, 0, 0, 0xff)));
    }

    #[test]
    fn pango_markup_is_stripped_before_the_text_becomes_data() {
        let mut b = block("x", "<span foreground='#f00'>7&#37;</span>");
        b.markup = Some("pango".to_string());
        let items = to_items(&[b], &[]);
        assert!(matches!(
            items[0].fields.get("text"),
            Some(Value::Text(t)) if t == "7%"
        ));
        assert_eq!(items[0].fields.primary().and_then(|v| v.num()), Some(7.0));
    }
}
