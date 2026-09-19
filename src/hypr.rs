//! The Hyprland backend: its event stream, read back through the request socket.
//!
//! Hyprland writes one event a line - `EVENT>>DATA` - and most of them carry only the name
//! of what moved, so the session is read again after a burst the way sway's is. The two
//! sockets are not alike: the event one stays open for as long as the bar runs, while a
//! request gets a connection of its own that is written, drained and dropped at once.
//! Hyprland serves that socket synchronously, and a connection left open stops the
//! compositor until its five-second timeout.

use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;

use crate::desktop::{
    Command, Desktop, DesktopEvent, Layout, Publisher, REQUEST_TIMEOUT, Watching, Window,
    Workspace, more_waiting,
};
use crate::lines::{self, Lines};

/// The socket every event arrives on.
const EVENTS: &str = ".socket2.sock";
/// The socket everything is asked on.
const REQUESTS: &str = ".socket.sock";

/// How many session reads in a row have to fail before the bar gives up on the compositor.
///
/// One is a Hyprland busy enough to refuse a connection in the middle of a burst, and every
/// request here opens one of its own. A compositor that is really gone closes the event
/// stream, which is the other way out of the loop and the one that usually comes first.
const READS_BEFORE_GIVING_UP: u32 = 5;

/// The largest answer the bar will read from the compositor's socket.
const MAX_REPLY_BYTES: usize = 8 * 1024 * 1024;

/// What Hyprland calls the mode its keyboard is in when no submap is held.
///
/// It says so two ways - the name when asked, nothing at all in an event - and both mean a
/// keyboard doing what it ordinarily does, which is not a mode worth showing.
const DEFAULT_MODE: &str = "default";

/// The ids Hyprland keeps for special workspaces, which are scratchpads rather than places
/// the bar lists. Named workspaces are negative too, but far below this.
const SPECIAL: std::ops::RangeInclusive<i64> = -99..=-2;

/// One entry of the workspace list, as Hyprland writes it.
#[derive(Deserialize)]
struct HyprWorkspace {
    /// Positive where the workspace is numbered, and a large negative number where it was
    /// created by name.
    id: i64,
    name: String,
    /// The screen it is on, by the name Hyprland gives that screen.
    #[serde(default)]
    monitor: String,
    #[serde(default)]
    windows: u32,
    /// The window this workspace would focus, by address, or `0x0` for one that holds none.
    #[serde(default)]
    lastwindow: String,
}

/// A screen, as much of Hyprland's description of one as a bar reads.
#[derive(Deserialize)]
struct HyprMonitor {
    #[serde(default)]
    name: String,
    #[serde(default)]
    focused: bool,
    #[serde(rename = "activeWorkspace")]
    active: WorkspaceRef,
    /// The scratchpad open over the ordinary workspace, where one is. Hyprland keeps the
    /// two apart and leaves `active` naming the workspace underneath, so this is the only
    /// thing that says a screen is showing something else. Its id is `0` when none is up.
    #[serde(rename = "specialWorkspace", default)]
    special: WorkspaceRef,
}

/// A workspace named from somewhere else: a screen's active one, or a window's.
#[derive(Default, Deserialize)]
struct WorkspaceRef {
    #[serde(default)]
    id: i64,
}

/// A window, as much of Hyprland's description of one as a bar reads.
#[derive(Deserialize)]
struct HyprClient {
    address: String,
    #[serde(default)]
    class: String,
    #[serde(default)]
    title: String,
    /// Whether the window reached Hyprland through Xwayland, which is what says whether
    /// `class` is an X11 class or a Wayland app id.
    #[serde(default)]
    xwayland: bool,
    workspace: WorkspaceRef,
}

/// The keyboards a device list carries.
#[derive(Deserialize)]
struct Devices {
    #[serde(default)]
    keyboards: Vec<HyprKeyboard>,
}

#[derive(Deserialize)]
struct HyprKeyboard {
    /// Missing on a keyboard xkb has no layout for, which is also how Hyprland spells it.
    #[serde(default)]
    active_layout_index: Option<u32>,
    #[serde(default)]
    active_keymap: String,
    /// Whether this is the keyboard being typed on.
    #[serde(default)]
    main: bool,
}

/// What the backend keeps between events, for the little Hyprland says only once.
#[derive(Default)]
struct Tracked {
    /// Windows that have asked for attention, by address.
    ///
    /// Hyprland reports urgency as an event and never reports it ending, so unlike the
    /// other two compositors there is no flag to read back: the addresses are held here
    /// and dropped when that window is focused or closes.
    urgent: HashSet<String>,
    /// Which window each screen is showing, by address, for a title event to find its
    /// screen without reading the session again.
    shown: Vec<(String, String)>,
}

/// What a burst of events has left to be read again.
#[derive(Default)]
struct Due {
    session: bool,
    layout: bool,
}

/// Where Hyprland keeps this session's sockets.
fn socket_dir() -> Result<PathBuf> {
    let signature = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")
        .context("HYPRLAND_INSTANCE_SIGNATURE is not set; is this a Hyprland session?")?;
    let runtime = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir),
        // What Hyprland itself falls back to, so a session started without the variable is
        // still found rather than reported missing.
        // SAFETY: `getuid` reads the calling process and cannot fail.
        None => PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })),
    };
    Ok(runtime.join("hypr").join(signature))
}

fn connect(socket: &str) -> Result<UnixStream> {
    let path = socket_dir()?.join(socket);
    UnixStream::connect(&path).with_context(|| format!("connecting to {}", path.display()))
}

/// Ask Hyprland something and read the whole answer.
///
/// A connection of its own each time, closed as soon as the answer is in: Hyprland answers
/// this socket synchronously, so one held open stops the compositor rather than the bar.
fn ask(request: &str) -> Result<Vec<u8>> {
    let mut stream = connect(REQUESTS)?;
    // A compositor too busy to answer must not take the bar with it. These reads happen on
    // the main thread before the event loop starts, so without a bound a Hyprland wedged on
    // its own main thread is a bar that never draws and never says why. The event socket is
    // left unbounded on purpose: it is meant to sit idle for hours.
    stream
        .set_read_timeout(Some(REQUEST_TIMEOUT))
        .context("bounding how long a request may wait")?;
    stream
        .set_write_timeout(Some(REQUEST_TIMEOUT))
        .context("bounding how long a request may take to send")?;
    stream
        .write_all(request.as_bytes())
        .with_context(|| format!("asking Hyprland for {request}"))?;
    stream.flush().context("flushing a request")?;
    let mut body = Vec::new();
    // Read to the end of the answer, but only so far: the length is the compositor's to
    // decide and the buffer is the bar's to pay for.
    std::io::Read::take(&mut stream, MAX_REPLY_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .with_context(|| format!("reading Hyprland's answer to {request}"))?;
    if body.len() > MAX_REPLY_BYTES {
        bail!("Hyprland's answer to {request} is longer than the bar will hold");
    }
    Ok(body)
}

/// Ask Hyprland for something it writes as JSON, and read it into what the bar holds.
fn ask_json<T: serde::de::DeserializeOwned>(request: &str, what: &str) -> Result<T> {
    let body = ask(request)?;
    serde_json::from_slice(&body).with_context(|| format!("parsing {what}"))
}

/// A mode as the bar holds it, where Hyprland's ordinary one is no mode at all.
///
/// An event says so with nothing at all and a question is answered with the name, so both
/// spellings arrive here.
fn held(mode: &str) -> Option<String> {
    (!mode.is_empty() && mode != DEFAULT_MODE).then(|| mode.to_string())
}

/// A window's address as its events spell it.
///
/// Hyprland writes an address two ways: `0x55d1c4` when asked, and `55d1c4` in an event.
/// Everything kept here is in the event's spelling, since that is the one arriving while
/// nothing is being read.
fn address_of(client: &HyprClient) -> &str {
    client.address.strip_prefix("0x").unwrap_or(&client.address)
}

/// Whether a workspace is one of Hyprland's scratchpads rather than a place on the bar.
fn special(id: i64) -> bool {
    SPECIAL.contains(&id)
}

/// Where a workspace belongs in the list.
///
/// Numbered workspaces come first and in their own order. A workspace made by name is given
/// a large negative id counting downwards, so the further from zero it is the later it was
/// made, which puts named ones after the numbers and in the order they appeared.
fn order(id: i64) -> (u8, i64) {
    match id > 0 {
        true => (0, id),
        false => (1, -id),
    }
}

/// How a command names the workspace it wants, which depends on how that workspace was made.
///
/// Hyprland switches to a numbered workspace by its number and to a named one by `name:`,
/// and a named workspace's id is an internal number no dispatcher takes. The bar's ids are
/// unsigned, so the sign Hyprland gave one survives the cast and is read back here.
fn selector(id: u64, name: &str) -> String {
    match id as i64 {
        id if id > 0 => id.to_string(),
        _ => format!("name:{name}"),
    }
}

/// What a window calls itself, in the two ways a bar can key on.
///
/// Hyprland puts a Wayland app id and an X11 class in the same field and says which it is
/// beside it, so the one this window has not got is left empty the way the other backends
/// leave it.
fn window_of(client: &HyprClient) -> Window {
    let (app_id, class) = match client.xwayland {
        true => (String::new(), client.class.clone()),
        false => (client.class.clone(), String::new()),
    };
    Window {
        title: client.title.clone(),
        app_id,
        class,
    }
}

/// Take a freshly read session into the state the bar draws from.
///
/// `tracked.shown` is left holding which window each screen is showing, by address, for a
/// title event to find its screen by.
fn take_session(
    monitors: &[HyprMonitor],
    spaces: &[HyprWorkspace],
    clients: &[HyprClient],
    windows: bool,
    state: &mut Desktop,
    tracked: &mut Tracked,
) {
    let focused = monitors.iter().find(|m| m.focused);
    state.focused_output = focused.map(|m| m.name.clone());
    let on_a_screen: HashSet<i64> = monitors.iter().map(|m| m.active.id).collect();
    let urgent: HashSet<i64> = clients
        .iter()
        .filter(|c| tracked.urgent.contains(address_of(c)))
        .map(|c| c.workspace.id)
        .collect();

    // A workspace Hyprland keeps because a rule made it persistent holds nothing until
    // somebody opens something on it, and a list of names that hold nothing is what a
    // workspaces module is not for.
    let mut listed: Vec<&HyprWorkspace> = spaces
        .iter()
        .filter(|w| !special(w.id))
        .filter(|w| w.windows > 0 || on_a_screen.contains(&w.id))
        .collect();
    listed.sort_by(|a, b| (&a.monitor, order(a.id)).cmp(&(&b.monitor, order(b.id))));
    state.workspaces = listed
        .into_iter()
        .map(|w| Workspace {
            id: w.id as u64,
            name: w.name.clone(),
            output: w.monitor.clone(),
            focused: focused.is_some_and(|m| m.active.id == w.id),
            visible: on_a_screen.contains(&w.id),
            urgent: urgent.contains(&w.id),
        })
        .collect();

    state.windows.clear();
    tracked.shown.clear();
    if !windows {
        return;
    }
    // What a screen shows is the window its active workspace would focus, which is the one
    // Hyprland already keeps; `focused` is true of one window in the session and would
    // leave every other screen empty.
    let by_address: HashMap<&str, &HyprClient> =
        clients.iter().map(|c| (address_of(c), c)).collect();
    for monitor in monitors {
        // A scratchpad open on a screen is drawn over the ordinary workspace and holds the
        // focus, so it is what that screen is showing. Hyprland goes on naming the
        // workspace underneath in `activeWorkspace` the whole time it is up.
        let showing = match special(monitor.special.id) {
            true => monitor.special.id,
            false => monitor.active.id,
        };
        let Some(space) = spaces.iter().find(|w| w.id == showing) else {
            continue;
        };
        let address = space.lastwindow.strip_prefix("0x").unwrap_or("");
        let Some(client) = by_address.get(address) else {
            continue;
        };
        tracked
            .shown
            .push((address.to_string(), monitor.name.clone()));
        state
            .windows
            .insert(monitor.name.clone(), window_of(client));
    }
}

/// Re-read the session: the screens, the workspaces, and the windows if anything draws one.
fn read_session(state: &mut Desktop, watching: Watching, tracked: &mut Tracked) -> Result<()> {
    let monitors: Vec<HyprMonitor> = ask_json("j/monitors", "the screens")?;
    let spaces: Vec<HyprWorkspace> = ask_json("j/workspaces", "the workspace list")?;
    // The window list is the expensive half, and two things want it: a window module, and
    // an urgent window whose workspace has to be found. A bar with neither never asks.
    let asked = watching.windows || !tracked.urgent.is_empty();
    let clients: Vec<HyprClient> = match asked {
        true => ask_json("j/clients", "the window list")?,
        false => Vec::new(),
    };
    // A window that has gone takes its urgency with it. An address is where the window was,
    // so the next window can be given the same one, and an address kept past its window
    // would hand that urgency to a stranger.
    if asked {
        tracked
            .urgent
            .retain(|address| clients.iter().any(|c| address_of(c) == address));
    }
    take_session(
        &monitors,
        &spaces,
        &clients,
        watching.windows,
        state,
        tracked,
    );
    Ok(())
}

/// The keyboard layout, read from the device list.
///
/// Hyprland's layout event carries the name and no index, and the index is the one part of
/// a layout's identity that does not depend on how xkb spells it, so the devices are asked
/// rather than the event believed.
fn read_layout() -> Result<Option<Layout>> {
    let devices = devices_of(&ask("j/devices")?)?;
    Ok(pick_keyboard(&devices.keyboards).map(|k| Layout {
        name: k.active_keymap.clone(),
        index: k.active_layout_index.unwrap_or_default(),
    }))
}

/// The keyboard a bar should follow.
///
/// The main one is the one being typed on. A keyboard xkb has no layout for is passed over
/// rather than drawn, because it has no name and no index and would show as an empty module
/// sitting on the first layout. That can be the main one - a virtual keyboard Hyprland made
/// for itself - so the fallback is any keyboard that does have a layout, and a session where
/// none does shows nothing at all.
fn pick_keyboard(keyboards: &[HyprKeyboard]) -> Option<&HyprKeyboard> {
    let usable = |k: &&HyprKeyboard| k.active_layout_index.is_some();
    keyboards
        .iter()
        .find(|k| k.main && usable(k))
        .or_else(|| keyboards.iter().find(usable))
}

/// The field Hyprland writes as a bare word for a keyboard xkb has no active layout for.
const INDEX_KEY: &str = "\"active_layout_index\"";
/// The word it writes there, which is not JSON.
const NOT_JSON: &str = "none";

/// Read a device list, around the one thing Hyprland writes that is not JSON.
///
/// A keyboard with no active layout is written `"active_layout_index": none`, an unquoted
/// word that costs the whole reply rather than the one field: serde refuses it while
/// tokenising, before any deserializer of this module's could be asked about the field, so
/// it has to go before the parse rather than during it. One odd keyboard would otherwise
/// take every other keyboard's layout with it.
fn devices_of(body: &[u8]) -> Result<Devices> {
    let text = String::from_utf8_lossy(body);
    serde_json::from_str(&repair_index(&text)).context("parsing the input devices")
}

/// Turn the bare `none` after an `active_layout_index` into the null it means.
///
/// Hyprland's spacing is not assumed: the key is found, the colon and whatever space sits
/// around it stepped over, and only a whole `none` standing there as the value is
/// rewritten. Anything else - a number, or a longer word starting the same way - is left
/// exactly as it came.
fn repair_index(text: &str) -> String {
    let bytes = text.as_bytes();
    let space = |at: &mut usize| {
        while bytes.get(*at).is_some_and(u8::is_ascii_whitespace) {
            *at += 1;
        }
    };
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(found) = text[cursor..].find(INDEX_KEY) {
        let key_end = cursor + found + INDEX_KEY.len();
        let mut at = key_end;
        space(&mut at);
        let value = match bytes.get(at) {
            Some(b':') => {
                at += 1;
                space(&mut at);
                Some(at)
            }
            _ => None,
        };
        // A value ends where the object does, so a `none` running into anything else is a
        // longer word and not the one being repaired.
        let bare = value.filter(|&v| {
            text[v..].starts_with(NOT_JSON)
                && bytes
                    .get(v + NOT_JSON.len())
                    .is_none_or(|b| b.is_ascii_whitespace() || *b == b',' || *b == b'}')
        });
        match bare {
            Some(v) => {
                out.push_str(&text[cursor..v]);
                out.push_str("null");
                cursor = v + NOT_JSON.len();
            }
            None => {
                out.push_str(&text[cursor..key_end]);
                cursor = key_end;
            }
        }
    }
    out.push_str(&text[cursor..]);
    out
}

/// The binding mode Hyprland is in right now.
fn read_mode() -> Result<Option<String>> {
    let submap: String = ask_json("j/submap", "the binding mode")?;
    Ok(held(&submap))
}

/// Take one event into the state, and note what it leaves to be read again.
///
/// Hyprland reports far more than a bar draws - screencasts, groups, layer surfaces,
/// floating - so what is read again is decided by a list of what can move something on the
/// bar rather than by what is not recognised.
fn apply(
    line: &str,
    watching: Watching,
    state: &mut Desktop,
    tracked: &mut Tracked,
    due: &mut Due,
) {
    let Some((event, data)) = line.split_once(">>") else {
        return;
    };
    match event {
        // A title is the noisiest thing a compositor reports - a terminal running a build,
        // a browser playing a video - and this event carries the title itself, so the
        // screen showing that window is written straight into and nothing is read again.
        // A title on no screen changes nothing.
        "windowtitlev2" if watching.windows => {
            if let Some((address, title)) = data.split_once(',')
                && let Some((_, output)) = tracked.shown.iter().find(|(shown, _)| shown == address)
                && let Some(window) = state.windows.get_mut(output)
            {
                window.title = title.to_string();
            }
        }
        "activelayout" => due.layout |= watching.language,
        // The mode is the whole event, so there is nothing to ask.
        "submap" if watching.mode => state.mode = held(data),
        // Kept only where something draws it. A bar with neither module would otherwise
        // collect addresses nothing ever reads, and the next focus into one of them would
        // clear it and read the whole session back for a bar that shows none of it.
        "urgent" if watching.follows_workspaces() => {
            tracked.urgent.insert(data.to_string());
            due.session = true;
        }
        // Hyprland clears a window's urgency when it is focused and says nothing about it,
        // so this is where the bar hears that too.
        "activewindowv2" => {
            let cleared = tracked.urgent.remove(data);
            due.session |= cleared || watching.windows;
        }
        "closewindow" => {
            tracked.urgent.remove(data);
            due.session |= watching.follows_workspaces();
        }
        // Everything else that can move a workspace, a screen or a window. The `v2` events
        // are taken and their older twins left, since Hyprland sends both.
        "workspacev2" | "createworkspacev2" | "destroyworkspacev2" | "moveworkspacev2"
        | "renameworkspace" | "activespecialv2" | "focusedmonv2" | "monitoraddedv2"
        | "monitorremovedv2" | "openwindow" | "movewindowv2" | "configreloaded" => {
            due.session |= watching.follows_workspaces();
        }
        _ => {}
    }
}

/// Follow Hyprland's event stream and forward what it says into the event loop.
pub fn spawn(sender: calloop::channel::SyncSender<DesktopEvent>, watching: Watching) -> Result<()> {
    // Connected here rather than on the thread, so a socket that is not there is reported
    // at startup instead of silently leaving the modules empty.
    let events = connect(EVENTS)?;

    let mut state = Desktop::default();
    let mut tracked = Tracked::default();
    if watching.follows_workspaces() {
        read_session(&mut state, watching, &mut tracked)?;
    }
    if watching.mode {
        // A compositor that will not answer leaves the module empty rather than stopping
        // the bar; the next submap change says what it is anyway.
        state.mode = read_mode().unwrap_or_else(|e| {
            log::warn!("Hyprland did not report its binding mode: {e:#}");
            None
        });
    }
    if watching.language {
        state.layout = read_layout().unwrap_or_else(|e| {
            log::warn!("Hyprland did not report a keyboard layout: {e:#}");
            None
        });
    }
    let mut publisher = Publisher::new(sender);
    publisher.publish(&state);

    std::thread::Builder::new()
        .name("hypr-ipc".to_string())
        .spawn(move || {
            follow(
                lines::capped(BufReader::new(events)),
                publisher,
                watching,
                state,
                tracked,
            )
        })
        .context("spawning the Hyprland IPC thread")?;
    Ok(())
}

fn follow(
    mut lines: Lines<BufReader<UnixStream>>,
    mut publisher: Publisher,
    watching: Watching,
    mut state: Desktop,
    mut tracked: Tracked,
) {
    let mut failed = 0u32;
    loop {
        // Everything Hyprland has already said is taken before anything is asked or drawn.
        // Moving to another workspace is several events - the workspace, the screen, the
        // window that came with it - and reading the session once for the lot is the
        // difference between one redraw and four that nobody can see apart.
        let mut due = Due::default();
        loop {
            let line = match lines.next() {
                Some(Ok(line)) => line,
                Some(Err(e)) => {
                    publisher.stop(format!("reading Hyprland's event stream: {e}"));
                    return;
                }
                None => {
                    publisher.stop("Hyprland closed its event stream".to_string());
                    return;
                }
            };
            // A cut line is a title too long to have been meant for a bar, and what it was
            // cut from is still worth reading properly.
            match line.dropped > 0 {
                true => due.session |= watching.follows_workspaces(),
                false => apply(&line.text, watching, &mut state, &mut tracked, &mut due),
            }
            if lines.reader().buffer().is_empty() && !more_waiting(lines.reader().get_ref()) {
                break;
            }
        }
        // A read that fails leaves the state it was given untouched - nothing is written
        // until every request has answered - so the bar goes on drawing what it last knew
        // rather than emptying every compositor module over one refused connection.
        if due.session {
            match read_session(&mut state, watching, &mut tracked) {
                Ok(()) => failed = 0,
                Err(e) => {
                    failed += 1;
                    if failed >= READS_BEFORE_GIVING_UP {
                        publisher.stop(format!("{e:#}"));
                        return;
                    }
                    log::warn!("re-reading the Hyprland session, {failed} in a row now: {e:#}");
                }
            }
        }
        if due.layout {
            // One keyboard unplugged mid-session is not a reason to stop following the rest
            // of the desktop.
            match read_layout() {
                Ok(layout) => state.layout = layout,
                Err(e) => log::warn!("Hyprland did not report a keyboard layout: {e:#}"),
            }
        }
        if !publisher.publish(&state) {
            return;
        }
    }
}

/// Send Hyprland a command on a connection of its own.
pub fn run_command(command: Command) {
    let request = match &command {
        Command::FocusWorkspace { id, name } => {
            format!("dispatch workspace {}", selector(*id, name))
        }
    };
    match ask(&request) {
        Ok(reply) => log::debug!(
            "Hyprland {request:?} -> {}",
            String::from_utf8_lossy(&reply)
        ),
        Err(e) => log::warn!("asking Hyprland for {command:?}: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVERYTHING: Watching = Watching {
        language: true,
        mode: true,
        windows: true,
        workspaces: true,
    };

    const MONITORS: &str = r#"[
        {"id":0,"name":"DP-1","focused":true,
         "activeWorkspace":{"id":1,"name":"1"},"specialWorkspace":{"id":0,"name":""}},
        {"id":1,"name":"HDMI-A-1","focused":false,
         "activeWorkspace":{"id":2,"name":"2"},"specialWorkspace":{"id":0,"name":""}}
    ]"#;

    const SPACES: &str = r#"[
        {"id":1,"name":"1","monitor":"DP-1","windows":1,"lastwindow":"0x1a","lastwindowtitle":"a"},
        {"id":2,"name":"2","monitor":"HDMI-A-1","windows":1,"lastwindow":"0x2b","lastwindowtitle":"b"},
        {"id":3,"name":"3","monitor":"DP-1","windows":0,"lastwindow":"0x0","lastwindowtitle":""},
        {"id":-1337,"name":"mail","monitor":"DP-1","windows":1,"lastwindow":"0x3c","lastwindowtitle":"c"},
        {"id":-98,"name":"special:magic","monitor":"DP-1","windows":1,"lastwindow":"0x4d","lastwindowtitle":"d"}
    ]"#;

    const CLIENTS: &str = r#"[
        {"address":"0x1a","class":"foot","title":"a","xwayland":false,
         "workspace":{"id":1,"name":"1"},"monitor":0},
        {"address":"0x2b","class":"Gimp","title":"b","xwayland":true,
         "workspace":{"id":2,"name":"2"},"monitor":1},
        {"address":"0x3c","class":"thunderbird","title":"c","xwayland":false,
         "workspace":{"id":-1337,"name":"mail"},"monitor":0}
    ]"#;

    fn session(watching: Watching, tracked: &mut Tracked) -> Desktop {
        let monitors: Vec<HyprMonitor> = serde_json::from_str(MONITORS).expect("the screens");
        let spaces: Vec<HyprWorkspace> = serde_json::from_str(SPACES).expect("the workspaces");
        let clients: Vec<HyprClient> = serde_json::from_str(CLIENTS).expect("the windows");
        let mut state = Desktop::default();
        take_session(
            &monitors,
            &spaces,
            &clients,
            watching.windows,
            &mut state,
            tracked,
        );
        state
    }

    /// A scratchpad is not a place on the bar, and a workspace a rule keeps alive while it
    /// holds nothing is a name with nothing behind it.
    #[test]
    fn the_list_leaves_out_scratchpads_and_workspaces_holding_nothing() {
        let desktop = session(EVERYTHING, &mut Tracked::default());
        let names: Vec<&str> = desktop.workspaces.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(
            names,
            ["1", "mail", "2"],
            "numbers first, then names, by screen"
        );
        assert!(!names.contains(&"3"), "an empty workspace no screen is on");
        assert!(!names.contains(&"special:magic"));
    }

    #[test]
    fn a_workspace_says_which_screen_it_is_on_and_what_it_is_doing() {
        let desktop = session(EVERYTHING, &mut Tracked::default());
        let first = &desktop.workspaces[0];
        assert_eq!((first.name.as_str(), first.output.as_str()), ("1", "DP-1"));
        assert!(first.focused && first.visible && !first.urgent);
        let other = desktop
            .workspaces
            .iter()
            .find(|w| w.name == "2")
            .expect("the second screen's workspace");
        assert!(
            other.visible && !other.focused,
            "a screen showing a workspace the keyboard is not on"
        );
        assert_eq!(desktop.focused_output.as_deref(), Some("DP-1"));
    }

    /// One window is focused in the session and every other screen still has something on
    /// it, so each screen is asked what its own workspace would focus.
    #[test]
    fn every_screen_reports_the_window_it_is_showing() {
        let desktop = session(EVERYTHING, &mut Tracked::default());
        assert_eq!(desktop.windows.len(), 2);
        let wayland = &desktop.windows["DP-1"];
        assert_eq!(
            (wayland.title.as_str(), wayland.app_id.as_str()),
            ("a", "foot")
        );
        assert!(wayland.class.is_empty(), "a Wayland client has no class");
        let x11 = &desktop.windows["HDMI-A-1"];
        assert_eq!((x11.title.as_str(), x11.class.as_str()), ("b", "Gimp"));
        assert!(x11.app_id.is_empty(), "an X11 client has no app id");
    }

    /// A bar with no window module keeps none of this, so a title changing is nothing it
    /// reads the session for.
    #[test]
    fn a_bar_without_a_window_module_keeps_no_windows() {
        let watching = Watching {
            windows: false,
            ..EVERYTHING
        };
        let mut tracked = Tracked::default();
        let desktop = session(watching, &mut tracked);
        assert!(desktop.windows.is_empty());
        assert!(tracked.shown.is_empty());
        assert_eq!(desktop.workspaces.len(), 3, "the workspaces still arrive");
    }

    /// Hyprland says a window wants attention once and never says it stopped, so the bar
    /// keeps the address and drops it when that window is focused.
    #[test]
    fn urgency_is_held_until_the_window_is_focused() {
        let mut tracked = Tracked::default();
        let mut state = Desktop::default();
        let mut due = Due::default();
        apply(">>", EVERYTHING, &mut state, &mut tracked, &mut due);
        apply("urgent>>2b", EVERYTHING, &mut state, &mut tracked, &mut due);
        assert!(
            due.session,
            "an urgent window has to be found on a workspace"
        );

        let desktop = session(EVERYTHING, &mut tracked);
        let marked = desktop
            .workspaces
            .iter()
            .find(|w| w.name == "2")
            .expect("the workspace holding it");
        assert!(marked.urgent);
        assert!(
            desktop.workspaces.iter().filter(|w| w.urgent).count() == 1,
            "and no other workspace"
        );

        apply(
            "activewindowv2>>2b",
            EVERYTHING,
            &mut state,
            &mut tracked,
            &mut due,
        );
        assert!(tracked.urgent.is_empty());
        assert!(
            !session(EVERYTHING, &mut tracked)
                .workspaces
                .iter()
                .any(|w| w.urgent)
        );
    }

    /// The title event carries the title, so the screen showing that window is written
    /// into and nothing is asked. A title on no screen is nothing at all.
    #[test]
    fn a_title_is_written_into_the_screen_showing_that_window() {
        let mut tracked = Tracked::default();
        let mut state = session(EVERYTHING, &mut tracked);
        let mut due = Due::default();

        apply(
            "windowtitlev2>>1a,building, still",
            EVERYTHING,
            &mut state,
            &mut tracked,
            &mut due,
        );
        assert_eq!(state.windows["DP-1"].title, "building, still");
        assert_eq!(
            state.windows["DP-1"].app_id, "foot",
            "and nothing else moved"
        );

        apply(
            "windowtitlev2>>3c,unread",
            EVERYTHING,
            &mut state,
            &mut tracked,
            &mut due,
        );
        assert!(
            !due.session,
            "a window no screen is showing is not worth reading the session for"
        );
        assert_eq!(state.windows.len(), 2);
    }

    /// Hyprland reports a great deal a bar has nothing to say about, and asking it what
    /// every screencast is doing would be a redraw for each one.
    #[test]
    fn only_events_that_can_move_something_are_read_again() {
        let mut state = Desktop::default();
        let mut tracked = Tracked::default();
        for quiet in [
            "screencast>>1,0",
            "openlayer>>waybar",
            "changefloatingmode>>1a,1",
            "activewindow>>foot,a",
            "ignoregrouplock>>0",
        ] {
            let mut due = Due::default();
            apply(quiet, EVERYTHING, &mut state, &mut tracked, &mut due);
            assert!(!due.session && !due.layout, "{quiet} asked for a read");
        }
        for loud in [
            "workspacev2>>2,2",
            "focusedmonv2>>DP-1,1",
            "openwindow>>1a,1,foot,a",
            "monitoraddedv2>>1,HDMI-A-1,a screen",
        ] {
            let mut due = Due::default();
            apply(loud, EVERYTHING, &mut state, &mut tracked, &mut due);
            assert!(due.session, "{loud} did not ask for a read");
        }
    }

    /// The bar asks the compositor only for what it draws, so a workspaces-only bar reads
    /// nothing when a keyboard is switched and a language-only bar reads nothing when a
    /// window opens.
    #[test]
    fn nothing_is_read_again_for_a_module_the_bar_does_not_have() {
        let mut state = Desktop::default();
        let mut tracked = Tracked::default();
        let mut due = Due::default();
        let spaces_only = Watching {
            workspaces: true,
            ..Watching::default()
        };
        apply(
            "activelayout>>kbd,Serbian",
            spaces_only,
            &mut state,
            &mut tracked,
            &mut due,
        );
        apply(
            "submap>>resize",
            spaces_only,
            &mut state,
            &mut tracked,
            &mut due,
        );
        assert!(!due.layout && state.mode.is_none());

        let language_only = Watching {
            language: true,
            ..Watching::default()
        };
        apply(
            "openwindow>>1a,1,foot,a",
            language_only,
            &mut state,
            &mut tracked,
            &mut due,
        );
        assert!(!due.session);
    }

    /// A submap is the whole event, and the one a keyboard is in anyway is no mode at all.
    #[test]
    fn a_submap_is_taken_from_the_event_and_the_ordinary_one_is_not_a_mode() {
        let mut state = Desktop::default();
        let mut tracked = Tracked::default();
        let mut due = Due::default();
        apply(
            "submap>>resize",
            EVERYTHING,
            &mut state,
            &mut tracked,
            &mut due,
        );
        assert_eq!(state.mode.as_deref(), Some("resize"));
        apply("submap>>", EVERYTHING, &mut state, &mut tracked, &mut due);
        assert_eq!(state.mode, None, "an empty submap is the default one");
        assert_eq!(
            held(DEFAULT_MODE),
            None,
            "and so is the name it is asked by"
        );
    }

    /// The index is the one part of a layout's identity that does not depend on how xkb
    /// spells it, and Hyprland writes a missing one as a word that is not JSON.
    #[test]
    fn a_layout_is_read_from_the_keyboard_being_typed_on() {
        let devices = devices_of(
            br#"{"mice":[],"keyboards":[
                {"address":"0x1","name":"virtual","layout":"us","active_layout_index": none,
                 "active_keymap":"English (US)","main":false},
                {"address":"0x2","name":"kbd","layout":"us,rs","active_layout_index": 1,
                 "active_keymap":"Serbian","main":true}],"tablets":[]}"#,
        )
        .expect("a device list parses around the word that is not JSON");
        let keyboard = devices
            .keyboards
            .iter()
            .find(|k| k.main)
            .expect("the main keyboard");
        assert_eq!(keyboard.active_layout_index, Some(1));
        assert_eq!(keyboard.active_keymap, "Serbian");
        assert_eq!(
            devices.keyboards[0].active_layout_index, None,
            "a keyboard xkb has no layout for"
        );
    }

    /// A scratchpad is drawn over the workspace it was opened on and takes the focus with
    /// it, while Hyprland goes on naming the workspace underneath as the active one.
    #[test]
    fn a_screen_showing_a_scratchpad_reports_the_scratchpad() {
        const OPEN: &str = r#"[
            {"id":0,"name":"DP-1","focused":true,
             "activeWorkspace":{"id":1,"name":"1"},
             "specialWorkspace":{"id":-98,"name":"special:magic"}}
        ]"#;
        const HELD: &str = r#"[
            {"address":"0x1a","class":"foot","title":"underneath","xwayland":false,
             "workspace":{"id":1,"name":"1"}},
            {"address":"0x4d","class":"footclient","title":"scratch","xwayland":false,
             "workspace":{"id":-98,"name":"special:magic"}}
        ]"#;
        let monitors: Vec<HyprMonitor> = serde_json::from_str(OPEN).expect("the screen");
        let spaces: Vec<HyprWorkspace> = serde_json::from_str(SPACES).expect("the workspaces");
        let clients: Vec<HyprClient> = serde_json::from_str(HELD).expect("the windows");
        let mut state = Desktop::default();
        let mut tracked = Tracked::default();
        take_session(&monitors, &spaces, &clients, true, &mut state, &mut tracked);

        assert_eq!(
            state.windows["DP-1"].title, "scratch",
            "the window on top, not the one it covers"
        );
        assert!(
            !state.workspaces.iter().any(|w| w.name == "special:magic"),
            "and it is still no place on the bar"
        );
    }

    /// The addresses are kept so an urgent window can be found on a workspace, so a bar
    /// drawing neither workspaces nor windows has nothing to find and keeps none.
    #[test]
    fn urgency_is_not_collected_for_a_bar_that_draws_none_of_it() {
        let mut state = Desktop::default();
        let mut tracked = Tracked::default();
        let mut due = Due::default();
        let language_only = Watching {
            language: true,
            ..Watching::default()
        };
        apply(
            "urgent>>2b",
            language_only,
            &mut state,
            &mut tracked,
            &mut due,
        );
        assert!(tracked.urgent.is_empty(), "nothing to remember it for");
        assert!(!due.session, "and nothing to read the session for");

        // And with a workspaces module it is kept, which is what the gate is guarding.
        let spaces_only = Watching {
            workspaces: true,
            ..Watching::default()
        };
        apply(
            "urgent>>2b",
            spaces_only,
            &mut state,
            &mut tracked,
            &mut due,
        );
        assert!(tracked.urgent.contains("2b"));
        assert!(due.session);
    }

    /// The keyboard being typed on can be one xkb has no layout for - a virtual one
    /// Hyprland made - and an empty name sitting on the first layout is worse than the
    /// layout of the keyboard that does have one.
    #[test]
    fn a_main_keyboard_with_no_layout_is_passed_over() {
        let devices = devices_of(
            br#"{"keyboards":[
                {"name":"virtual","active_layout_index": none,"active_keymap":"","main":true},
                {"name":"kbd","active_layout_index": 1,"active_keymap":"Serbian","main":false}]}"#,
        )
        .expect("a device list");
        let keyboard = pick_keyboard(&devices.keyboards).expect("the keyboard that has a layout");
        assert_eq!(keyboard.active_keymap, "Serbian");
        assert_eq!(keyboard.active_layout_index, Some(1));

        // And a session where nothing has a layout shows nothing, rather than an empty
        // name sitting on index 0.
        let none = devices_of(
            br#"{"keyboards":[
                {"name":"virtual","active_layout_index": none,"active_keymap":"","main":true}]}"#,
        )
        .expect("a device list");
        assert!(pick_keyboard(&none.keyboards).is_none());
    }

    /// The word is repaired by finding the field rather than by matching one spelling of
    /// it, so Hyprland moving its spacing does not cost the whole reply.
    #[test]
    fn the_word_that_is_not_json_is_repaired_however_it_is_spaced() {
        for spacing in [
            r#"{"active_layout_index":none}"#,
            r#"{"active_layout_index" : none}"#,
            r#"{"active_layout_index":
                none}"#,
        ] {
            let repaired = repair_index(spacing);
            assert!(repaired.contains("null"), "{spacing} was left as it was");
            assert!(!repaired.contains(NOT_JSON));
        }
        // A number is a value already, and a longer word only begins the same way.
        assert_eq!(
            repair_index(r#"{"active_layout_index": 0}"#),
            r#"{"active_layout_index": 0}"#
        );
        assert_eq!(
            repair_index(r#"{"active_layout_index": "nonesuch"}"#),
            r#"{"active_layout_index": "nonesuch"}"#
        );
    }

    /// Hyprland switches to a numbered workspace by its number and to a named one by name,
    /// and the sign of the id it gave is what says which is which.
    #[test]
    fn a_click_names_the_workspace_the_way_hyprland_switches_by() {
        assert_eq!(selector(3, "3"), "3");
        assert_eq!(
            selector(3, "build"),
            "3",
            "a renamed number is still a number"
        );
        assert_eq!(selector(-1337i64 as u64, "mail"), "name:mail");
    }
}
