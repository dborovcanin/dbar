//! Sway IPC: the focused window, the workspace list and the active keyboard layout.
//!
//! The protocol is small enough to speak directly - a fixed header and a JSON body - so this
//! costs no dependencies. Two connections are used: one stays subscribed to events, which
//! the protocol says must not carry other requests, and one issues queries.

use std::collections::HashMap;
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
const GET_BINDING_STATE: u32 = 12;

/// An input event, as the protocol numbers it: the high bit marks a message as an event
/// rather than a reply to something asked.
const EVENT_INPUT: u32 = 0x8000_0015;
/// A binding-mode event, numbered the same way.
const EVENT_MODE: u32 = 0x8000_0002;

/// The mode a compositor is in when no binding mode is held.
///
/// Sway names it this itself, and a bar has nothing to say about it: the point of a mode
/// indicator is that it appears when the keyboard means something unusual.
pub const DEFAULT_MODE: &str = "default";

/// What the focused-window module can offer a format.
///
/// The title is what a window says it is showing, and changes as it does. The other two
/// are what it *is*: `app_id` for a Wayland client, `class` for an X11 one through
/// Xwayland, and an application sets one or the other rather than both. Together they are
/// what a state rule keys on to give one program its own colour without matching on a
/// title that changes every time a tab does.
pub const WINDOW_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        name: "title",
        kind: Kind::Text,
    },
    FieldSpec {
        name: "app_id",
        kind: Kind::Text,
    },
    FieldSpec {
        name: "class",
        kind: Kind::Text,
    },
];

/// A window, as much of it as a bar has any use for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Window {
    pub title: String,
    /// What a Wayland client calls itself. Empty for an X11 one.
    pub app_id: String,
    /// What an X11 client calls itself, through Xwayland. Empty for a Wayland one.
    pub class: String,
}

/// What one workspace can offer a format.
pub const WORKSPACE_FIELDS: &[FieldSpec] = &[FieldSpec {
    name: "name",
    kind: Kind::Text,
}];

/// What the binding-mode module can offer a format.
pub const MODE_FIELDS: &[FieldSpec] = &[FieldSpec {
    name: "mode",
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
    /// The screen it is on, named the way the compositor names it: "DP-1". A bar on one
    /// screen lists the workspaces of that screen, so this is what ties the two together.
    #[serde(default)]
    pub output: String,
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
    /// The title each screen has focused, by the name of that screen.
    ///
    /// One per output rather than one altogether, because only one window in the session
    /// is focused and every other screen still has something on it: a bar that showed the
    /// focused title on all of them would be wrong everywhere but where the pointer is.
    pub windows: HashMap<String, Window>,
    /// Which screen the compositor's focus is on, for a bar that does not know its own.
    pub focused_output: Option<String>,
    /// The layout of the keyboard last switched, or nothing while no module asks for one.
    pub layout: Option<Layout>,
    /// The binding mode the compositor is in, which is `default` unless one is held.
    pub mode: Option<String>,
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

/// Whether another message has already arrived, so a burst can be taken in one go.
///
/// Messages are read straight off the socket rather than through a buffer, so asking the
/// kernel is the whole of it: nothing can be waiting anywhere else.
fn more_waiting(stream: &UnixStream) -> bool {
    use std::os::fd::AsRawFd as _;

    let mut poll = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one descriptor owned by the caller for the length of this call, a count
    // that matches, and a timeout of zero, so nothing here waits.
    unsafe { libc::poll(&mut poll, 1, 0) > 0 }
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

/// The children of a node, ordinary and floating alike.
fn children(node: &serde_json::Value) -> impl Iterator<Item = &serde_json::Value> {
    ["nodes", "floating_nodes"]
        .into_iter()
        .flat_map(move |key| {
            node.get(key)
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
        })
}

/// The node a container would focus, found by following its focus chain to the end.
///
/// `focused` is true on exactly one node in the whole session, so it cannot say what the
/// other screens are showing. Every container instead lists its children most recently
/// focused first, and following that from an output arrives at the window that screen is
/// on, whether or not the keyboard is there.
fn focus_head(node: &serde_json::Value) -> &serde_json::Value {
    let mut node = node;
    loop {
        let wanted = node
            .get("focus")
            .and_then(|v| v.as_array())
            .and_then(|f| f.first())
            .and_then(|v| v.as_u64());
        let Some(wanted) = wanted else {
            return node;
        };
        let child = children(node).find(|c| c.get("id").and_then(|v| v.as_u64()) == Some(wanted));
        match child {
            Some(child) => node = child,
            None => return node,
        }
    }
}

/// What each screen is showing, by the name the compositor gives that screen.
///
/// The root's children are the outputs, so one pass over them covers every screen rather
/// than only the one the keyboard is on.
fn windows_by_output(tree: &serde_json::Value) -> HashMap<String, Window> {
    let mut windows = HashMap::new();
    for output in children(tree) {
        let Some(name) = output.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        if let Some(window) = window_of(focus_head(output)) {
            windows.insert(name.to_string(), window);
        }
    }
    windows
}

/// The window a node is, if it is one at all.
///
/// The root, the outputs and the workspace containers all have a name and none of them is
/// a window; what tells them apart is the kind sway gives every node, and a window is a
/// container with nothing inside it. What the window calls itself is read afterwards and
/// separately: a Wayland client that never set an app id, and an X11 one whose properties
/// carry no class, are both still windows with a title worth showing.
fn window_of(node: &serde_json::Value) -> Option<Window> {
    let text = |value: Option<&serde_json::Value>| {
        value
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let kind = node.get("type").and_then(|v| v.as_str());
    if !matches!(kind, Some("con" | "floating_con")) || children(node).next().is_some() {
        return None;
    }
    let app_id = node.get("app_id");
    let class = node
        .get("window_properties")
        .and_then(|properties| properties.get("class"));
    Some(Window {
        title: text(node.get("name")),
        app_id: text(app_id),
        class: text(class),
    })
}

/// Re-read the two halves a workspace or window event can have changed.
fn read_desktop(query_stream: &mut UnixStream, state: &mut SwayState, windows: bool) -> Result<()> {
    // The workspace list is read either way: it is where the focused screen comes from,
    // and a window module on one screen has to know which screen that is. The tree is the
    // expensive half, and only a window module has anything to do with it.
    state.workspaces = serde_json::from_slice(&query(query_stream, GET_WORKSPACES)?)
        .context("parsing the workspace list")?;

    if windows {
        let tree: serde_json::Value =
            serde_json::from_slice(&query(query_stream, GET_TREE)?).context("parsing the tree")?;
        state.windows = windows_by_output(&tree);
    }
    state.focused_output = state
        .workspaces
        .iter()
        .find(|w| w.focused)
        .map(|w| w.output.clone());

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

/// The binding mode named by a `mode` event.
fn mode_change(body: &[u8]) -> Option<String> {
    #[derive(Deserialize)]
    struct Event {
        change: String,
    }
    serde_json::from_slice::<Event>(body).ok().map(|e| e.change)
}

/// The binding mode the compositor is in right now.
fn read_mode(queries: &mut UnixStream) -> Result<Option<String>> {
    #[derive(Deserialize)]
    struct BindingState {
        name: String,
    }
    send(queries, GET_BINDING_STATE, b"")?;
    let (_, reply) = recv(queries)?;
    let state: BindingState =
        serde_json::from_slice(&reply).context("reading the compositor's binding state")?;
    Ok(Some(state.name))
}

/// How many commands may be waiting for the compositor at once.
///
/// Clicking a workspace is one command, and a hand clicking as fast as it can is a few a
/// second. Anything past this is a compositor that has stopped answering, and queueing
/// for one of those only means switching to workspaces nobody wants any more.
const QUEUED_COMMANDS: usize = 16;

/// The way to run a Sway command without waiting for the compositor to answer.
///
/// Connecting, writing and reading a reply all block, and the thread a click arrives on
/// is the one that draws: a compositor that is slow to answer would stop the bar
/// redrawing and stop it dispatching Wayland, which is a bar that has frozen.
pub struct Commands(std::sync::mpsc::SyncSender<String>);

impl Commands {
    pub fn send(&self, command: String) {
        // A queue this full is a compositor that is not listening, and a click nobody is
        // going to act on is better dropped than remembered.
        if let Err(e) = self.0.try_send(command) {
            log::debug!("the compositor is not keeping up with commands: {e}");
        }
    }
}

/// Start the thread that runs Sway commands, each on its own connection since the
/// subscribed one cannot carry them.
pub fn commands() -> Commands {
    let (sender, receiver) = std::sync::mpsc::sync_channel::<String>(QUEUED_COMMANDS);
    let started = std::thread::Builder::new()
        .name("sway-commands".to_string())
        .spawn(move || {
            while let Ok(command) = receiver.recv() {
                run_command(&command);
            }
        });
    if let Err(e) = started {
        log::warn!("no thread for compositor commands: {e}");
    }
    Commands(sender)
}

/// Run a Sway command on its own connection, since the subscribed one cannot carry it.
fn run_command(command: &str) {
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
/// What the bar has asked the compositor for.
///
/// Each of these costs a subscription and a question at startup, and the desktop ones
/// cost a workspace list - and, for windows, a whole tree - every time the desktop moves.
/// A bar that draws none of them never connects at all.
#[derive(Clone, Copy, Debug, Default)]
pub struct Watching {
    pub language: bool,
    pub mode: bool,
    pub windows: bool,
    pub workspaces: bool,
}

impl Watching {
    /// Whether anything on the bar comes from the compositor.
    pub fn anything(self) -> bool {
        self.language || self.mode || self.windows || self.workspaces
    }

    /// Whether the workspace list and the windows on it have to be followed.
    fn desktop(self) -> bool {
        self.windows || self.workspaces
    }
}

/// Input devices and binding modes are only subscribed to when something on the bar is
/// going to draw them, so a bar without those modules pays nothing for the questions.
pub fn spawn(sender: calloop::channel::Sender<SwayEvent>, watching: Watching) -> Result<()> {
    // Fail loudly here rather than on the helper thread, so a missing socket is reported
    // at startup instead of silently leaving the modules empty.
    let mut events = connect()?;
    let mut queries = connect()?;

    // A workspace list and a window title both move with the workspace, so both follow
    // workspace events; only a window module has any use for a title changing, which is
    // the noisiest thing the compositor reports.
    let mut wanted = Vec::new();
    if watching.desktop() {
        wanted.push("\"workspace\"");
    }
    if watching.windows {
        wanted.push("\"window\"");
    }
    if watching.language {
        wanted.push("\"input\"");
    }
    if watching.mode {
        wanted.push("\"mode\"");
    }
    let subscription = format!("[{}]", wanted.join(","));
    send(&mut events, SUBSCRIBE, subscription.as_bytes())?;
    let (_, reply) = recv(&mut events)?;
    log::debug!(
        "sway subscribe {subscription} -> {}",
        String::from_utf8_lossy(&reply)
    );

    let mut state = SwayState::default();
    if watching.desktop() {
        read_desktop(&mut queries, &mut state, watching.windows)?;
    }
    if watching.mode {
        // Sway only reports a mode when it changes, so the one it is already in has to be
        // asked for. A compositor too old to answer leaves the module empty rather than
        // stopping the bar.
        state.mode = read_mode(&mut queries).unwrap_or_else(|e| {
            log::warn!("the compositor did not report its binding mode: {e:#}");
            None
        });
    }
    if watching.language {
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
                // Everything the compositor has already said is taken before anything is
                // asked or drawn. Moving to another workspace is several events - the
                // workspace, the window that came with it, sometimes a mode - and reading
                // the desktop once for the lot is the difference between one redraw and
                // four that nobody can see apart.
                let mut news = false;
                let mut desktop = false;
                loop {
                    match recv(&mut events) {
                        Ok((EVENT_MODE, body)) => {
                            state.mode = mode_change(&body);
                            news = true;
                        }
                        // Most input events say nothing about the layout, and a redraw for
                        // one would be a wake-up spent on nothing.
                        Ok((EVENT_INPUT, body)) => {
                            if let Some(layout) = layout_change(&body) {
                                state.layout = Some(layout);
                                news = true;
                            }
                        }
                        // Any workspace or window event can change either half, and the
                        // queries are cheap next to a redraw, so both are re-read rather
                        // than patched.
                        Ok(_) => {
                            desktop = true;
                            news = true;
                        }
                        Err(e) => {
                            let _ = sender.send(SwayEvent::Stopped(e.to_string()));
                            return;
                        }
                    }
                    if !more_waiting(&events) {
                        break;
                    }
                }
                if desktop && let Err(e) = read_desktop(&mut queries, &mut state, watching.windows)
                {
                    let _ = sender.send(SwayEvent::Stopped(e.to_string()));
                    return;
                }
                if news
                    && sender
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

    /// What the bar asks for decides what it subscribes to and what it re-reads, so this
    /// is the switch that keeps a clock-only bar off the compositor entirely.
    #[test]
    fn nothing_from_the_compositor_means_nothing_to_ask_it() {
        assert!(!Watching::default().anything());
        assert!(!Watching::default().desktop());

        let language = Watching {
            language: true,
            ..Watching::default()
        };
        assert!(language.anything(), "a layout still needs a connection");
        assert!(!language.desktop(), "but not the workspaces or the tree");

        let workspaces = Watching {
            workspaces: true,
            ..Watching::default()
        };
        assert!(workspaces.desktop());
        assert!(
            !workspaces.windows,
            "and no tree, which is the expensive half"
        );
    }

    /// The tree as sway reports it with two screens: the keyboard is on the left one, and
    /// the right one is still showing what it was last used for.
    const TREE: &str = r#"{
      "id": 1, "name": "root", "type": "root", "focus": [3, 4],
      "nodes": [
        { "id": 3, "name": "DP-1", "type": "output", "focus": [6],
          "nodes": [
            { "id": 6, "name": "1", "type": "workspace", "focus": [9], "nodes": [
              { "id": 9, "name": "vim", "type": "con", "app_id": "foot", "focused": true,
                "focus": [] },
              { "id": 10, "name": "mail", "type": "con", "app_id": "thunderbird", "focus": [] }
            ]}
          ]},
        { "id": 4, "name": "HDMI-A-1", "type": "output", "focus": [7],
          "nodes": [
            { "id": 7, "name": "2", "type": "workspace", "focus": [11], "nodes": [
              { "id": 11, "name": "a page", "type": "con", "app_id": "firefox", "focus": [] }
            ]}
          ]},
        { "id": 5, "name": "__i3", "type": "output", "focus": [] }
      ]
    }"#;

    /// Only one window in the session is focused, so a bar that took the focused title
    /// would say the same thing on every screen and be wrong on all but one of them.
    #[test]
    fn every_screen_reports_the_window_it_is_showing() {
        let tree: serde_json::Value = serde_json::from_str(TREE).expect("a tree parses");
        let windows = windows_by_output(&tree);
        assert_eq!(windows.get("DP-1").map(|w| w.title.as_str()), Some("vim"));
        assert_eq!(
            windows.get("HDMI-A-1").map(|w| w.title.as_str()),
            Some("a page")
        );
    }

    /// A title changes every time a tab does; what the window *is* does not. A rule that
    /// wants one program its own colour keys on that instead, so both names the window
    /// could be known by are published - the Wayland one, and the X11 one that arrives
    /// through Xwayland.
    #[test]
    fn a_window_says_what_it_is_as_well_as_what_it_shows() {
        let tree: serde_json::Value = serde_json::from_str(TREE).expect("a tree parses");
        let windows = windows_by_output(&tree);
        let wayland = windows.get("DP-1").expect("a window on DP-1");
        assert_eq!(wayland.app_id, "foot");
        assert!(wayland.class.is_empty(), "a Wayland client has no class");

        // What Xwayland reports instead: no app_id, and a class in the X11 properties.
        let x11: serde_json::Value = serde_json::from_str(
            r#"{"id":1,"focus":[3],"nodes":[
                 {"id":3,"name":"DP-1","type":"output","focus":[6],"nodes":[
                   {"id":6,"name":"1","type":"workspace","focus":[9],"nodes":[
                     {"id":9,"name":"doc.pdf","type":"con","app_id":null,"focus":[],
                      "window_properties":{"class":"Zathura"}}]}]}]}"#,
        )
        .expect("a tree parses");
        let window = windows_by_output(&x11);
        let window = window.get("DP-1").expect("an X11 window on DP-1");
        assert_eq!(window.class, "Zathura");
        assert_eq!(window.title, "doc.pdf");
        assert!(window.app_id.is_empty(), "an X11 client has no app_id");
    }

    /// Sway leaves `app_id` null for a Wayland client that never set one, and an X11 client
    /// can arrive with properties that carry no class at all. Neither is any less a window,
    /// and a title is exactly what a bar has to show for them.
    #[test]
    fn a_window_that_says_nothing_about_itself_still_has_a_title() {
        let nameless: serde_json::Value = serde_json::from_str(
            r#"{"id":1,"focus":[3],"nodes":[
                 {"id":3,"name":"DP-1","type":"output","focus":[6],"nodes":[
                   {"id":6,"name":"1","type":"workspace","focus":[9],"nodes":[
                     {"id":9,"name":"a scratch window","type":"con","app_id":null,
                      "focus":[]}]}]}]}"#,
        )
        .expect("a tree parses");
        let windows = windows_by_output(&nameless);
        let window = windows.get("DP-1").expect("a window on DP-1");
        assert_eq!(window.title, "a scratch window");
        assert!(window.app_id.is_empty());
        assert!(window.class.is_empty());

        let classless: serde_json::Value = serde_json::from_str(
            r#"{"id":1,"focus":[3],"nodes":[
                 {"id":3,"name":"DP-1","type":"output","focus":[6],"nodes":[
                   {"id":6,"name":"1","type":"workspace","focus":[9],"nodes":[
                     {"id":9,"name":"an X11 window","type":"con","app_id":null,"focus":[],
                      "window_properties":{"instance":"xterm"}}]}]}]}"#,
        )
        .expect("a tree parses");
        let windows = windows_by_output(&classless);
        let window = windows.get("DP-1").expect("an X11 window on DP-1");
        assert_eq!(window.title, "an X11 window");
        assert!(window.class.is_empty());
    }

    /// The root, the outputs and the workspace containers all have names, and none of them
    /// is a window: a screen with nothing on it says nothing rather than saying "1".
    #[test]
    fn an_empty_screen_has_no_title_rather_than_its_workspace_name() {
        let tree: serde_json::Value = serde_json::from_str(
            r#"{"id":1,"focus":[3],"nodes":[
                 {"id":3,"name":"DP-1","type":"output","focus":[6],
                  "nodes":[{"id":6,"name":"1","type":"workspace","focus":[],"nodes":[]}]}]}"#,
        )
        .expect("a tree parses");
        assert_eq!(windows_by_output(&tree).get("DP-1"), None);
    }

    /// Sway names the screen each workspace is on, which is what lets a bar list its own.
    #[test]
    fn a_workspace_says_which_screen_it_is_on() {
        let list: Vec<Workspace> = serde_json::from_str(
            r#"[{"name":"1","output":"DP-1","focused":true,"visible":true},
                {"name":"2","output":"HDMI-A-1","visible":true}]"#,
        )
        .expect("a workspace list parses");
        assert_eq!(list[0].output, "DP-1");
        assert_eq!(list[1].output, "HDMI-A-1");
        assert!(!list[1].focused);
    }

    #[test]
    fn a_mode_event_names_the_mode_it_switched_to() {
        let body = br#"{"change":"resize","pango_markup":false}"#;
        assert_eq!(mode_change(body), Some("resize".to_string()));
    }

    /// Leaving a mode is reported the same way, as a switch back to `default` - which is
    /// what the bar reads to know the module should disappear again.
    #[test]
    fn leaving_a_mode_is_a_switch_to_the_default_one() {
        let body = br#"{"change":"default","pango_markup":false}"#;
        assert_eq!(mode_change(body).as_deref(), Some(DEFAULT_MODE));
    }

    #[test]
    fn a_mode_event_that_makes_no_sense_names_nothing() {
        assert_eq!(mode_change(b"{}"), None);
        assert_eq!(mode_change(b"not json"), None);
    }

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
