//! i3bar-protocol status provider.
//!
//! The provider owns an `i3status-rs` (or any i3bar-compatible) child process. Its stdout is
//! read on a helper thread and forwarded to the event loop over a calloop channel, so the main
//! thread never polls. Click events travel back over the child's stdin.

use std::borrow::Cow;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::config;

/// One status block as described by the i3bar protocol.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Block {
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
    /// Either `"none"` or `"pango"`; i3status-rs always sends the latter.
    #[serde(default)]
    pub markup: Option<String>,
}

impl Block {
    /// The text to actually draw.
    ///
    /// Providers that set `markup = "pango"` may wrap text in span tags and escape it as XML.
    /// V0 has no rich text, so tags are dropped and entities decoded; without this the bar
    /// would literally show `&#39;` and `<span ...>`.
    pub fn display_text(&self) -> Cow<'_, str> {
        if self.markup.as_deref() != Some("pango") {
            return Cow::Borrowed(&self.full_text);
        }
        if !self.full_text.contains(['<', '&']) {
            return Cow::Borrowed(&self.full_text);
        }
        Cow::Owned(strip_pango(&self.full_text))
    }

    /// A synthetic block used to surface provider failures on the bar itself.
    pub fn error(text: impl Into<String>) -> Block {
        Block {
            full_text: text.into(),
            color: Some("#f38ba8".to_string()),
            ..Block::default()
        }
    }
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
    Blocks(Vec<Block>),
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
        let mut child = Command::new(&cfg.command)
            .args(&cfg.args)
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

        match serde_json::from_str::<Vec<Block>>(trimmed) {
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
