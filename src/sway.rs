//! Sway IPC: the focused window, the workspace list and the active keyboard layout.
//!
//! The protocol is small enough to speak directly - a fixed header and a JSON body - so this
//! costs no dependencies. Two connections are used: one stays subscribed to events, which
//! the protocol says must not carry other requests, and one issues queries.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;

use crate::status::{FieldSpec, Kind, Unit};

const MAGIC: &[u8; 6] = b"i3-ipc";

const RUN_COMMAND: u32 = 0;
const GET_WORKSPACES: u32 = 1;
const SUBSCRIBE: u32 = 2;
const GET_TREE: u32 = 4;
const GET_INPUTS: u32 = 100;

/// An input event, as the protocol numbers it: the high bit marks a message as an event
/// rather than a reply to something asked.
const EVENT_INPUT: u32 = 0x8000_0015;

/// What the focused-window module can offer a format.
pub const WINDOW_FIELDS: &[FieldSpec] = &[FieldSpec {
    name: "title",
    kind: Kind::Text,
}];

/// What one workspace can offer a format.
pub const WORKSPACE_FIELDS: &[FieldSpec] = &[FieldSpec {
    name: "name",
    kind: Kind::Text,
}];

/// What the keyboard-layout module can offer a format.
pub const LANGUAGE_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        name: "layout",
        kind: Kind::Text,
    },
    FieldSpec {
        name: "short",
        kind: Kind::Text,
    },
    FieldSpec {
        name: "index",
        kind: Kind::Num(Unit::None),
    },
];

/// One entry of the workspace list.
#[derive(Clone, Debug, Deserialize)]
pub struct Workspace {
    pub name: String,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub visible: bool,
    #[serde(default)]
    pub urgent: bool,
}

/// The keyboard layout the compositor has active.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    /// What xkb calls it, which is written for a person to read: "English (US)".
    pub name: String,
    /// Its place in the list the keyboard was configured with, which is the one part of a
    /// layout's identity that does not depend on how xkb spells it.
    pub index: u32,
}

/// Everything dbar tracks from the compositor.
#[derive(Clone, Debug, Default)]
pub struct SwayState {
    pub workspaces: Vec<Workspace>,
    /// Title of the focused window, if any.
    pub window: Option<String>,
    /// The layout of the keyboard last switched, or nothing while no module asks for one.
    pub layout: Option<Layout>,
}

#[derive(Debug)]
pub enum SwayEvent {
    State(Box<SwayState>),
    Stopped(String),
}

fn socket_path() -> Result<PathBuf> {
    std::env::var_os("SWAYSOCK")
        .map(PathBuf::from)
        .context("SWAYSOCK is not set; is this a Sway session?")
}

fn connect() -> Result<UnixStream> {
    let path = socket_path()?;
    UnixStream::connect(&path).with_context(|| format!("connecting to {}", path.display()))
}

fn send(stream: &mut UnixStream, kind: u32, payload: &[u8]) -> Result<()> {
    let mut header = Vec::with_capacity(14 + payload.len());
    header.extend_from_slice(MAGIC);
    header.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
    header.extend_from_slice(&kind.to_ne_bytes());
    header.extend_from_slice(payload);
    stream
        .write_all(&header)
        .context("writing an IPC message")?;
    stream.flush().context("flushing an IPC message")
}

fn recv(stream: &mut UnixStream) -> Result<(u32, Vec<u8>)> {
    let mut header = [0u8; 14];
    stream
        .read_exact(&mut header)
        .context("reading an IPC header")?;
    if &header[..6] != MAGIC {
        bail!("IPC reply did not start with the magic string");
    }
    let len = u32::from_ne_bytes(header[6..10].try_into().unwrap()) as usize;
    let kind = u32::from_ne_bytes(header[10..14].try_into().unwrap());
    let mut body = vec![0u8; len];
    stream
        .read_exact(&mut body)
        .context("reading an IPC body")?;
    Ok((kind, body))
}

fn query(stream: &mut UnixStream, kind: u32) -> Result<Vec<u8>> {
    send(stream, kind, b"")?;
    let (_, body) = recv(stream)?;
    Ok(body)
}

/// Depth-first search for the focused node, which is where the title lives.
fn focused(node: &serde_json::Value) -> Option<&serde_json::Value> {
    if node.get("focused").and_then(|v| v.as_bool()) == Some(true) {
        return Some(node);
    }
    for key in ["nodes", "floating_nodes"] {
        for child in node
            .get(key)
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            if let Some(found) = focused(child) {
                return Some(found);
            }
        }
    }
    None
}

/// Re-read the two halves a workspace or window event can have changed.
fn read_desktop(query_stream: &mut UnixStream, state: &mut SwayState) -> Result<()> {
    state.workspaces = serde_json::from_slice(&query(query_stream, GET_WORKSPACES)?)
        .context("parsing the workspace list")?;

    let tree: serde_json::Value =
        serde_json::from_slice(&query(query_stream, GET_TREE)?).context("parsing the tree")?;
    // The root and the workspace containers report themselves focused when no window is,
    // and neither carries an application id, which is what tells them apart.
    state.window = focused(&tree)
        .filter(|n| n.get("app_id").is_some() || n.get("window_properties").is_some())
        .and_then(|n| n.get("name"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    Ok(())
}

/// The layout of the first keyboard the compositor lists one for.
///
/// Only asked at start-up: after that a layout change announces itself, and says which
/// keyboard it happened on.
fn read_layout(query_stream: &mut UnixStream) -> Result<Option<Layout>> {
    let inputs: serde_json::Value =
        serde_json::from_slice(&query(query_stream, GET_INPUTS)?).context("parsing the inputs")?;
    Ok(inputs.as_array().into_iter().flatten().find_map(layout_of))
}

/// The active layout an input device describes, if it has one.
///
/// A pointer or a switch carries no layout at all, so what is missing is what says this is
/// not a keyboard; a keyboard configured with one layout still reports that one by name.
fn layout_of(input: &serde_json::Value) -> Option<Layout> {
    Some(Layout {
        name: input.get("xkb_active_layout_name")?.as_str()?.to_string(),
        index: input
            .get("xkb_active_layout_index")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
    })
}

/// The layout an input event switched to, if it switched one.
///
/// Devices are added, removed and reconfigured under the same event, and the bar has
/// nothing to say about any of that; taking the layout from the device the event names is
/// also what makes two keyboards work, since the one that was switched is the one being
/// typed on.
fn layout_change(body: &[u8]) -> Option<Layout> {
    let event: serde_json::Value = serde_json::from_slice(body).ok()?;
    if event.get("change")?.as_str()? != "xkb_layout" {
        return None;
    }
    layout_of(event.get("input")?)
}

/// A short form of a layout name, for a bar that has room for two letters.
///
/// xkb names a layout for a person to read - "English (US)" - and offers no code beside
/// it, so the qualifier in brackets is taken where it is short enough to be one, and the
/// initials of the words are taken where it is not. Two letters of one word would put
/// "Serbian" and "Serbian (Latin)" both at "SE", and a layout that cannot be told from the
/// one beside it is worse than a long name. A module that wants the exact wording gives
/// its own with `layouts`.
pub fn abbreviate(name: &str) -> String {
    if let Some(open) = name.rfind('(')
        && let Some(close) = name[open..].find(')')
    {
        let inner = name[open + 1..open + close].trim();
        let letters = inner.chars().count();
        if (1..=3).contains(&letters) && inner.chars().all(char::is_alphanumeric) {
            return inner.to_uppercase();
        }
    }
    let mut words = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty());
    let first = words.next().unwrap_or_default();
    match words.next() {
        Some(second) => first
            .chars()
            .take(1)
            .chain(second.chars().take(1))
            .collect::<String>()
            .to_uppercase(),
        None => first.chars().take(2).collect::<String>().to_uppercase(),
    }
}

/// Run a Sway command on its own connection, since the subscribed one cannot carry it.
pub fn run_command(command: &str) {
    let result = (|| -> Result<()> {
        let mut stream = connect()?;
        send(&mut stream, RUN_COMMAND, command.as_bytes())?;
        let (_, body) = recv(&mut stream)?;
        log::debug!(
            "sway command {command:?} -> {}",
            String::from_utf8_lossy(&body)
        );
        Ok(())
    })();
    if let Err(e) = result {
        log::warn!("running sway command {command:?}: {e}");
    }
}

/// Subscribe to the compositor and forward its state into the event loop.
///
/// Input devices are only subscribed to and asked about when something on the bar is
/// going to draw a keyboard layout, so a bar without one pays nothing for the question.
pub fn spawn(sender: calloop::channel::Sender<SwayEvent>, language: bool) -> Result<()> {
    // Fail loudly here rather than on the helper thread, so a missing socket is reported
    // at startup instead of silently leaving the modules empty.
    let mut events = connect()?;
    let mut queries = connect()?;

    let subscription: &[u8] = match language {
        true => br#"["workspace","window","input"]"#,
        false => br#"["workspace","window"]"#,
    };
    send(&mut events, SUBSCRIBE, subscription)?;
    let (_, reply) = recv(&mut events)?;
    log::debug!("sway subscribe -> {}", String::from_utf8_lossy(&reply));

    let mut state = SwayState::default();
    read_desktop(&mut queries, &mut state)?;
    if language {
        // A compositor that will not list its inputs still has workspaces and windows to
        // report, so this is a module that stays empty rather than a reason to give up.
        state.layout = read_layout(&mut queries).unwrap_or_else(|e| {
            log::warn!("the compositor did not report a keyboard layout: {e:#}");
            None
        });
    }
    let _ = sender.send(SwayEvent::State(Box::new(state.clone())));

    std::thread::Builder::new()
        .name("sway-ipc".to_string())
        .spawn(move || {
            loop {
                let kind = match recv(&mut events) {
                    Ok((kind, body)) => {
                        if kind == EVENT_INPUT {
                            // Most input events say nothing about the layout, and a redraw
                            // for one would be a wake-up spent on nothing.
                            let Some(layout) = layout_change(&body) else {
                                continue;
                            };
                            state.layout = Some(layout);
                        }
                        kind
                    }
                    Err(e) => {
                        let _ = sender.send(SwayEvent::Stopped(e.to_string()));
                        return;
                    }
                };
                // Any workspace or window event can change either half, and the queries are
                // cheap next to a redraw, so both are re-read rather than patched.
                if kind != EVENT_INPUT
                    && let Err(e) = read_desktop(&mut queries, &mut state)
                {
                    let _ = sender.send(SwayEvent::Stopped(e.to_string()));
                    return;
                }
                if sender
                    .send(SwayEvent::State(Box::new(state.clone())))
                    .is_err()
                {
                    return;
                }
            }
        })
        .context("spawning the sway IPC thread")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_layout_is_taken_from_the_keyboard_that_was_switched() {
        let body = br#"{"change":"xkb_layout","input":{"identifier":"1:1:kbd","type":"keyboard",
            "xkb_active_layout_index":1,"xkb_active_layout_name":"Serbian"}}"#;
        assert_eq!(
            layout_change(body),
            Some(Layout {
                name: "Serbian".to_string(),
                index: 1,
            })
        );
    }

    #[test]
    fn an_input_event_that_is_not_a_switch_changes_nothing() {
        // A device being plugged in is an input event too, and redrawing for it would be a
        // wake-up spent on a layout that has not moved.
        let body = br#"{"change":"added","input":{"identifier":"1:1:kbd","type":"keyboard",
            "xkb_active_layout_index":0,"xkb_active_layout_name":"English (US)"}}"#;
        assert_eq!(layout_change(body), None);
    }

    #[test]
    fn a_device_with_no_layout_is_not_a_keyboard() {
        let pointer = serde_json::json!({"identifier": "2:2:mouse", "type": "pointer"});
        assert_eq!(layout_of(&pointer), None);
    }

    #[test]
    fn a_short_form_prefers_the_qualifier_xkb_put_in_brackets() {
        assert_eq!(abbreviate("English (US)"), "US");
        assert_eq!(abbreviate("English (UK)"), "UK");
        // A longer qualifier is a description rather than a code, so the initials are
        // taken - and they are what keeps these two apart, which is the whole point of
        // showing a layout at all.
        assert_eq!(abbreviate("Serbian (Latin)"), "SL");
        assert_eq!(abbreviate("Serbian"), "SE");
        assert_eq!(abbreviate("German (Neo 2)"), "GN");
        assert_eq!(abbreviate(""), "");
    }
}
