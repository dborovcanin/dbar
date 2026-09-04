//! Sway IPC: the focused window and the workspace list.
//!
//! The protocol is small enough to speak directly - a fixed header and a JSON body - so this
//! costs no dependencies. Two connections are used: one stays subscribed to events, which
//! the protocol says must not carry other requests, and one issues queries.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;

use crate::status::{FieldSpec, Kind};

const MAGIC: &[u8; 6] = b"i3-ipc";

const RUN_COMMAND: u32 = 0;
const GET_WORKSPACES: u32 = 1;
const SUBSCRIBE: u32 = 2;
const GET_TREE: u32 = 4;

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

/// Everything dbar tracks from the compositor, refreshed as a whole.
#[derive(Clone, Debug, Default)]
pub struct SwayState {
    pub workspaces: Vec<Workspace>,
    /// Title of the focused window, if any.
    pub window: Option<String>,
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

fn read_state(query_stream: &mut UnixStream) -> Result<SwayState> {
    let workspaces: Vec<Workspace> = serde_json::from_slice(&query(query_stream, GET_WORKSPACES)?)
        .context("parsing the workspace list")?;

    let tree: serde_json::Value =
        serde_json::from_slice(&query(query_stream, GET_TREE)?).context("parsing the tree")?;
    // The root and the workspace containers report themselves focused when no window is,
    // and neither carries an application id, which is what tells them apart.
    let window = focused(&tree)
        .filter(|n| n.get("app_id").is_some() || n.get("window_properties").is_some())
        .and_then(|n| n.get("name"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    Ok(SwayState { workspaces, window })
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
pub fn spawn(sender: calloop::channel::Sender<SwayEvent>) -> Result<()> {
    // Fail loudly here rather than on the helper thread, so a missing socket is reported
    // at startup instead of silently leaving the modules empty.
    let mut events = connect()?;
    let mut queries = connect()?;

    send(&mut events, SUBSCRIBE, br#"["workspace","window"]"#)?;
    let (_, reply) = recv(&mut events)?;
    log::debug!("sway subscribe -> {}", String::from_utf8_lossy(&reply));

    let initial = read_state(&mut queries)?;
    let _ = sender.send(SwayEvent::State(Box::new(initial)));

    std::thread::Builder::new()
        .name("sway-ipc".to_string())
        .spawn(move || {
            loop {
                if let Err(e) = recv(&mut events) {
                    let _ = sender.send(SwayEvent::Stopped(e.to_string()));
                    return;
                }
                // Any workspace or window event can change either half, and the queries are
                // cheap next to a redraw, so the whole state is re-read rather than patched.
                match read_state(&mut queries) {
                    Ok(state) => {
                        if sender.send(SwayEvent::State(Box::new(state))).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = sender.send(SwayEvent::Stopped(e.to_string()));
                        return;
                    }
                }
            }
        })
        .context("spawning the sway IPC thread")?;
    Ok(())
}
