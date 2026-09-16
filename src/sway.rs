//! The Sway backend: its IPC, read into a `Desktop` and written back as commands.
//!
//! The protocol is small enough to speak directly - a fixed header and a JSON body - so this
//! costs no dependencies. Two connections are used: one stays subscribed to events, which
//! the protocol says must not carry other requests, and one issues queries.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;

use crate::desktop::{
    Command, Desktop, DesktopEvent, Layout, Publisher, Watching, Window, Workspace, more_waiting,
};

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
/// A window event, numbered the same way.
const EVENT_WINDOW: u32 = 0x8000_0003;

/// What sway calls the mode its keyboard is in when no binding mode is held.
///
/// A bar has nothing to say about it: the point of a mode indicator is that it appears when
/// the keyboard means something unusual, so this one is reported as no mode at all.
const DEFAULT_MODE: &str = "default";

/// One entry of the workspace list, as sway writes it.
#[derive(Deserialize)]
struct SwayWorkspace {
    id: u64,
    name: String,
    #[serde(default)]
    output: String,
    #[serde(default)]
    focused: bool,
    #[serde(default)]
    visible: bool,
    #[serde(default)]
    urgent: bool,
}

/// The workspace list a `GET_WORKSPACES` reply carries.
fn workspaces_of(body: &[u8]) -> Result<Vec<Workspace>> {
    let list: Vec<SwayWorkspace> =
        serde_json::from_slice(body).context("parsing the workspace list")?;
    Ok(list
        .into_iter()
        .map(|w| Workspace {
            id: w.id,
            name: w.name,
            output: w.output,
            focused: w.focused,
            visible: w.visible,
            urgent: w.urgent,
        })
        .collect())
}

/// A mode as the bar holds it, where sway's ordinary one is no mode at all.
fn held(mode: String) -> Option<String> {
    (mode != DEFAULT_MODE).then_some(mode)
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

/// One node of the tree, holding only what a bar reads from it.
///
/// The tree describes every container's geometry, marks, borders and more besides, and a
/// busy session sends a lot of it. Read into a general JSON value, all of that would be
/// built into maps and strings only to be looked at once and dropped; read into this, the
/// parser steps over it.
#[derive(Deserialize, Default)]
struct Node {
    #[serde(default)]
    id: u64,
    #[serde(default, rename = "type")]
    kind: NodeKind,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    window_properties: Option<WindowProperties>,
    /// The children, most recently focused first, by id.
    #[serde(default)]
    focus: Vec<u64>,
    #[serde(default)]
    nodes: Vec<Node>,
    #[serde(default)]
    floating_nodes: Vec<Node>,
}

#[derive(Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
enum NodeKind {
    Con,
    FloatingCon,
    #[default]
    #[serde(other)]
    Other,
}

#[derive(Deserialize, Default)]
struct WindowProperties {
    #[serde(default)]
    class: Option<String>,
}

impl Node {
    /// The children of a node, ordinary and floating alike.
    fn children(&self) -> impl Iterator<Item = &Node> {
        self.nodes.iter().chain(&self.floating_nodes)
    }

    /// The node a container would focus, found by following its focus chain to the end.
    ///
    /// `focused` is true on exactly one node in the whole session, so it cannot say what the
    /// other screens are showing. Every container instead lists its children most recently
    /// focused first, and following that from an output arrives at the window that screen is
    /// on, whether or not the keyboard is there.
    fn focus_head(&self) -> &Node {
        let mut node = self;
        while let Some(&wanted) = node.focus.first()
            && let Some(child) = node.children().find(|c| c.id == wanted)
        {
            node = child;
        }
        node
    }
}

/// What each screen is showing, by the name the compositor gives that screen, with the id
/// of the window it is showing.
///
/// The root's children are the outputs, so one pass over them covers every screen rather
/// than only the one the keyboard is on.
fn shown_by_output(tree: &Node) -> impl Iterator<Item = (&str, u64, Window)> {
    tree.children().filter_map(|output| {
        let name = output.name.as_deref()?;
        let head = output.focus_head();
        Some((name, head.id, window_of(head)?))
    })
}

#[cfg(test)]
fn windows_by_output(tree: &Node) -> std::collections::HashMap<String, Window> {
    shown_by_output(tree)
        .map(|(output, _, window)| (output.to_string(), window))
        .collect()
}

/// The window a node is, if it is one at all.
///
/// The root, the outputs and the workspace containers all have a name and none of them is
/// a window; what tells them apart is the kind sway gives every node, and a window is a
/// container with nothing inside it. What the window calls itself is read afterwards and
/// separately: a Wayland client that never set an app id, and an X11 one whose properties
/// carry no class, are both still windows with a title worth showing.
fn window_of(node: &Node) -> Option<Window> {
    if !matches!(node.kind, NodeKind::Con | NodeKind::FloatingCon)
        || node.children().next().is_some()
    {
        return None;
    }
    Some(Window {
        title: node.name.clone().unwrap_or_default(),
        app_id: node.app_id.clone().unwrap_or_default(),
        class: node
            .window_properties
            .as_ref()
            .and_then(|properties| properties.class.clone())
            .unwrap_or_default(),
    })
}

/// A window event, reduced to what it says about the bar.
#[derive(Deserialize)]
struct WindowEvent {
    change: WindowChange,
    #[serde(default)]
    container: Node,
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum WindowChange {
    Title,
    Mark,
    #[serde(other)]
    Other,
}

/// Take a window event into the state, and say whether the desktop has to be read again.
///
/// A title is the noisiest thing sway reports - a terminal running a build, a browser
/// playing a video - and the event carries the window whole. Reading the workspaces and
/// the whole tree again for every one of those would parse the session to change one
/// string, so a title is written straight into the screen showing that window, and a
/// window no screen is showing changes nothing. A mark is never drawn. Everything else can
/// move focus, windows or urgency, and is read again.
fn window_event(body: &[u8], state: &mut Desktop, shown: &[(u64, String)]) -> bool {
    let Ok(event) = serde_json::from_slice::<WindowEvent>(body) else {
        return true;
    };
    match event.change {
        WindowChange::Mark => false,
        WindowChange::Title => {
            if let Some((_, output)) = shown.iter().find(|(id, _)| *id == event.container.id)
                && let Some(window) = window_of(&event.container)
            {
                state.windows.insert(output.clone(), window);
            }
            false
        }
        WindowChange::Other => true,
    }
}

/// Take what every screen is showing from a freshly read tree, and which window that is
/// by id, for a title event to find its screen by.
fn take_tree(tree: &Node, state: &mut Desktop, shown: &mut Vec<(u64, String)>) {
    state.windows.clear();
    shown.clear();
    for (output, id, window) in shown_by_output(tree) {
        shown.push((id, output.to_string()));
        state.windows.insert(output.to_string(), window);
    }
}

/// Re-read the two halves a workspace or window event can have changed.
///
/// `shown` is left holding which window each screen is showing, by id, for a title event
/// to find its screen by.
fn read_desktop(
    query_stream: &mut UnixStream,
    state: &mut Desktop,
    windows: bool,
    shown: &mut Vec<(u64, String)>,
) -> Result<()> {
    // The workspace list is read either way: it is where the focused screen comes from,
    // and a window module on one screen has to know which screen that is. The tree is the
    // expensive half, and only a window module has anything to do with it.
    state.workspaces = workspaces_of(&query(query_stream, GET_WORKSPACES)?)?;

    if windows {
        let tree: Node =
            serde_json::from_slice(&query(query_stream, GET_TREE)?).context("parsing the tree")?;
        take_tree(&tree, state, shown);
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

/// The binding mode a `mode` event switched to, or nothing for the ordinary one.
fn mode_change(body: &[u8]) -> Option<String> {
    #[derive(Deserialize)]
    struct Event {
        change: String,
    }
    serde_json::from_slice::<Event>(body)
        .ok()
        .and_then(|e| held(e.change))
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
    Ok(held(state.name))
}

/// Run a command on its own connection, since the subscribed one cannot carry it.
pub fn run_command(command: Command) {
    let command = match command {
        Command::FocusWorkspace { name, .. } => format!("workspace {}", quote(&name)),
    };
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

/// Wrap a workspace name for sway's command parser.
fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Subscribe to the compositor and forward its state into the event loop.
///
/// Input devices and binding modes are only subscribed to when something on the bar is
/// going to draw them, so a bar without those modules pays nothing for the questions.
pub fn spawn(sender: calloop::channel::Sender<DesktopEvent>, watching: Watching) -> Result<()> {
    // Fail loudly here rather than on the helper thread, so a missing socket is reported
    // at startup instead of silently leaving the modules empty.
    let mut events = connect()?;
    let mut queries = connect()?;

    // A workspace list and a window title both move with the workspace, so both follow
    // workspace events; only a window module has any use for a title changing, which is
    // the noisiest thing the compositor reports.
    let mut wanted = Vec::new();
    if watching.follows_workspaces() {
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

    let mut state = Desktop::default();
    let mut shown = Vec::new();
    if watching.follows_workspaces() {
        read_desktop(&mut queries, &mut state, watching.windows, &mut shown)?;
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
    let mut publisher = Publisher::new(sender);
    publisher.publish(&state);

    std::thread::Builder::new()
        .name("sway-ipc".to_string())
        .spawn(move || {
            loop {
                // Everything the compositor has already said is taken before anything is
                // asked or drawn. Moving to another workspace is several events - the
                // workspace, the window that came with it, sometimes a mode - and reading
                // the desktop once for the lot is the difference between one redraw and
                // four that nobody can see apart.
                let mut desktop = false;
                loop {
                    match recv(&mut events) {
                        Ok((EVENT_MODE, body)) => state.mode = mode_change(&body),
                        // Most input events say nothing about the layout.
                        Ok((EVENT_INPUT, body)) => {
                            if let Some(layout) = layout_change(&body) {
                                state.layout = Some(layout);
                            }
                        }
                        Ok((EVENT_WINDOW, body)) => {
                            desktop |= window_event(&body, &mut state, &shown);
                        }
                        // Any workspace event, and any window event other than a title
                        // or a mark, can change either half, so both are re-read rather
                        // than patched.
                        Ok(_) => desktop = true,
                        Err(e) => {
                            publisher.stop(e.to_string());
                            return;
                        }
                    }
                    if !more_waiting(&events) {
                        break;
                    }
                }
                if desktop
                    && let Err(e) =
                        read_desktop(&mut queries, &mut state, watching.windows, &mut shown)
                {
                    publisher.stop(e.to_string());
                    return;
                }
                if !publisher.publish(&state) {
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
        let tree: Node = serde_json::from_str(TREE).expect("a tree parses");
        let windows = windows_by_output(&tree);
        assert_eq!(windows.get("DP-1").map(|w| w.title.as_str()), Some("vim"));
        assert_eq!(
            windows.get("HDMI-A-1").map(|w| w.title.as_str()),
            Some("a page")
        );
    }

    /// The worker sends only a state that differs from the last one, so a title changing
    /// behind the window a screen shows has to leave the state exactly as it was.
    #[test]
    fn a_window_nobody_is_looking_at_changes_nothing_on_the_bar() {
        let before: Node = serde_json::from_str(TREE).expect("a tree parses");
        let after: Node =
            serde_json::from_str(&TREE.replace("\"mail\"", "\"mail (1)\"")).expect("a tree parses");
        assert_eq!(windows_by_output(&before), windows_by_output(&after));

        let focused: Node = serde_json::from_str(&TREE.replace("\"vim\"", "\"vim - notes\""))
            .expect("a tree parses");
        assert_ne!(windows_by_output(&before), windows_by_output(&focused));
    }

    /// A title changes every time a tab does; what the window *is* does not. A rule that
    /// wants one program its own colour keys on that instead, so both names the window
    /// could be known by are published - the Wayland one, and the X11 one that arrives
    /// through Xwayland.
    #[test]
    fn a_window_says_what_it_is_as_well_as_what_it_shows() {
        let tree: Node = serde_json::from_str(TREE).expect("a tree parses");
        let windows = windows_by_output(&tree);
        let wayland = windows.get("DP-1").expect("a window on DP-1");
        assert_eq!(wayland.app_id, "foot");
        assert!(wayland.class.is_empty(), "a Wayland client has no class");

        // What Xwayland reports instead: no app_id, and a class in the X11 properties.
        let x11: Node = serde_json::from_str(
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
        let nameless: Node = serde_json::from_str(
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

        let classless: Node = serde_json::from_str(
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
        let tree: Node = serde_json::from_str(
            r#"{"id":1,"focus":[3],"nodes":[
                 {"id":3,"name":"DP-1","type":"output","focus":[6],
                  "nodes":[{"id":6,"name":"1","type":"workspace","focus":[],"nodes":[]}]}]}"#,
        )
        .expect("a tree parses");
        assert_eq!(windows_by_output(&tree).get("DP-1"), None);
    }

    /// The state and window ids a freshly read tree leaves behind.
    fn read(tree: &str) -> (Desktop, Vec<(u64, String)>) {
        let tree: Node = serde_json::from_str(tree).expect("a tree parses");
        let mut state = Desktop::default();
        let mut shown = Vec::new();
        take_tree(&tree, &mut state, &mut shown);
        (state, shown)
    }

    fn title_event(id: u64, title: &str) -> String {
        format!(
            r#"{{"change":"title","container":{{"id":{id},"name":"{title}","type":"con",
                "app_id":"foot","focus":[],"nodes":[],"floating_nodes":[],"marks":[],
                "rect":{{"x":0,"y":0,"width":10,"height":10}}}}}}"#
        )
    }

    /// A title is taken from its event into the screen showing that window, without the
    /// tree being asked for again.
    #[test]
    fn a_title_event_retitles_the_screen_showing_that_window() {
        let (mut state, shown) = read(TREE);
        let reread = window_event(
            title_event(11, "another page").as_bytes(),
            &mut state,
            &shown,
        );
        assert!(!reread, "a title needs nothing read again");
        assert_eq!(state.windows["HDMI-A-1"].title, "another page");
        assert_eq!(state.windows["HDMI-A-1"].app_id, "foot");
        assert_eq!(state.windows["DP-1"].title, "vim");
    }

    #[test]
    fn a_title_event_for_a_window_nobody_is_looking_at_changes_nothing() {
        let (mut state, shown) = read(TREE);
        let before = state.clone();
        assert!(!window_event(
            title_event(10, "mail (1)").as_bytes(),
            &mut state,
            &shown
        ));
        assert_eq!(state, before);
    }

    /// Focus, new and closed windows, urgency and moves can change what every screen shows,
    /// and a mark changes nothing a bar draws.
    #[test]
    fn only_a_title_or_a_mark_is_spared_reading_the_desktop_again() {
        let (mut state, shown) = read(TREE);
        for change in ["focus", "new", "close", "move", "urgent", "floating"] {
            let body = format!(r#"{{"change":"{change}","container":{{"id":9}}}}"#);
            assert!(
                window_event(body.as_bytes(), &mut state, &shown),
                "{change}"
            );
        }
        let mark = br#"{"change":"mark","container":{"id":9,"marks":["a"]}}"#;
        assert!(!window_event(mark, &mut state, &shown));
        assert!(window_event(b"not json", &mut state, &shown));
    }

    /// Sway names the screen each workspace is on, which is what lets a bar list its own.
    #[test]
    fn a_workspace_says_which_screen_it_is_on() {
        let list = workspaces_of(
            br#"[{"id":4,"name":"1","output":"DP-1","focused":true,"visible":true},
                {"id":7,"name":"2","output":"HDMI-A-1","visible":true}]"#,
        )
        .expect("a workspace list parses");
        assert_eq!(list[0].output, "DP-1");
        assert_eq!(list[1].output, "HDMI-A-1");
        assert_eq!(list[1].id, 7);
        assert!(!list[1].focused);
    }

    #[test]
    fn a_mode_event_names_the_mode_it_switched_to() {
        let body = br#"{"change":"resize","pango_markup":false}"#;
        assert_eq!(mode_change(body), Some("resize".to_string()));
    }

    /// Leaving a mode is reported the same way, as a switch back to `default`. The bar is
    /// told there is no mode at all, which is what makes the module disappear again.
    #[test]
    fn leaving_a_mode_is_no_mode_at_all() {
        let body = br#"{"change":"default","pango_markup":false}"#;
        assert_eq!(mode_change(body), None);
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
}
