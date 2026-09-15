//! The niri backend: its event stream, read into a `Desktop`, and its actions for clicks.
//!
//! niri speaks one JSON value a line. Asked for its event stream, it answers with everything
//! it knows and then with every change, so unlike sway nothing is ever asked twice: the
//! backend keeps niri's side of the desktop itself and rebuilds the bar's view after each
//! burst.

use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;

use crate::desktop::{
    Command, Desktop, DesktopEvent, Layout, Publisher, Watching, Window, Workspace, more_waiting,
};
use crate::lines::{self, Lines};

fn socket_path() -> Result<PathBuf> {
    std::env::var_os("NIRI_SOCKET")
        .map(PathBuf::from)
        .context("NIRI_SOCKET is not set; is this a niri session?")
}

fn connect() -> Result<UnixStream> {
    let path = socket_path()?;
    UnixStream::connect(&path).with_context(|| format!("connecting to {}", path.display()))
}

/// Read niri's answer to a request: `Ok` and whatever was asked for, or `Err` and why not.
///
/// Nothing dbar asks niri for needs the answer itself, only whether there was one.
fn check(reply: &str) -> Result<()> {
    let reply: Result<serde::de::IgnoredAny, String> =
        serde_json::from_str(reply).context("reading niri's reply")?;
    match reply {
        Ok(_) => Ok(()),
        Err(message) => bail!("niri refused: {message}"),
    }
}

/// A workspace, as niri describes one.
#[derive(Clone, Debug, Deserialize)]
struct NiriWorkspace {
    id: u64,
    /// Its position on its screen, counted from one, which is what an unnamed one is called.
    idx: u8,
    name: Option<String>,
    output: Option<String>,
    is_urgent: bool,
    is_active: bool,
    is_focused: bool,
    active_window_id: Option<u64>,
}

/// A window, as much of niri's description of one as a bar reads.
#[derive(Deserialize)]
struct NiriWindow {
    id: u64,
    title: Option<String>,
    app_id: Option<String>,
    workspace_id: Option<u64>,
}

/// A window, and the workspace it is on.
struct Held {
    workspace: Option<u64>,
    window: Window,
}

impl From<NiriWindow> for Held {
    fn from(window: NiriWindow) -> Held {
        Held {
            workspace: window.workspace_id,
            // An X11 program reaches niri through xwayland-satellite as a Wayland client, so
            // what would have been its class arrives as an app id and `class` stays empty.
            window: Window {
                title: window.title.unwrap_or_default(),
                app_id: window.app_id.unwrap_or_default(),
                class: String::new(),
            },
        }
    }
}

#[derive(Deserialize)]
struct KeyboardLayouts {
    names: Vec<String>,
    current_idx: u8,
}

/// One line of the event stream.
///
/// niri writes each event as an object whose one key names it. Only the events that can
/// change what a bar draws are named here; serde steps over the rest - window geometry, focus
/// timestamps, screencasts - without building anything out of them.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Event {
    workspaces_changed: Option<WorkspacesChanged>,
    workspace_urgency_changed: Option<WorkspaceUrgencyChanged>,
    workspace_activated: Option<WorkspaceActivated>,
    workspace_active_window_changed: Option<WorkspaceActiveWindowChanged>,
    windows_changed: Option<WindowsChanged>,
    window_opened_or_changed: Option<WindowOpenedOrChanged>,
    window_closed: Option<WindowClosed>,
    keyboard_layouts_changed: Option<KeyboardLayoutsChanged>,
    keyboard_layout_switched: Option<KeyboardLayoutSwitched>,
}

#[derive(Deserialize)]
struct WorkspacesChanged {
    workspaces: Vec<NiriWorkspace>,
}

#[derive(Deserialize)]
struct WorkspaceUrgencyChanged {
    id: u64,
    urgent: bool,
}

#[derive(Deserialize)]
struct WorkspaceActivated {
    id: u64,
    focused: bool,
}

#[derive(Deserialize)]
struct WorkspaceActiveWindowChanged {
    workspace_id: u64,
    active_window_id: Option<u64>,
}

#[derive(Deserialize)]
struct WindowsChanged {
    windows: Vec<NiriWindow>,
}

#[derive(Deserialize)]
struct WindowOpenedOrChanged {
    window: NiriWindow,
}

#[derive(Deserialize)]
struct WindowClosed {
    id: u64,
}

#[derive(Deserialize)]
struct KeyboardLayoutsChanged {
    keyboard_layouts: KeyboardLayouts,
}

#[derive(Deserialize)]
struct KeyboardLayoutSwitched {
    idx: u8,
}

/// niri's side of the desktop, kept up to date from its event stream.
#[derive(Default)]
struct State {
    workspaces: HashMap<u64, NiriWorkspace>,
    windows: HashMap<u64, Held>,
    layouts: Option<KeyboardLayouts>,
}

impl State {
    /// Take one event in. True if it can have changed what the bar draws.
    ///
    /// niri does not promise that its events agree with each other at every step - a
    /// workspace can name an active window that has not been announced yet - so an event
    /// about something that is not there is left alone rather than trusted.
    ///
    /// niri reports every title of every window, and a terminal running a build retitles
    /// itself many times a second. A change the bar is not showing is taken in and goes no
    /// further, rather than rebuilding a desktop only for it to be found unchanged.
    fn apply(&mut self, event: Event, watching: Watching) -> bool {
        let mut news = false;
        if let Some(e) = event.workspaces_changed {
            self.workspaces = e.workspaces.into_iter().map(|w| (w.id, w)).collect();
            news = true;
        }
        if let Some(e) = event.workspace_urgency_changed {
            if let Some(workspace) = self.workspaces.get_mut(&e.id) {
                workspace.is_urgent = e.urgent;
            }
            news = true;
        }
        if let Some(e) = event.workspace_activated {
            // Active is per screen and focused is per session: the others on the same screen
            // stop being active, and only an activation that focuses takes focus from the rest.
            if let Some(output) = self.workspaces.get(&e.id).map(|w| w.output.clone()) {
                for workspace in self.workspaces.values_mut() {
                    if workspace.output == output {
                        workspace.is_active = workspace.id == e.id;
                    }
                    if e.focused {
                        workspace.is_focused = workspace.id == e.id;
                    }
                }
            }
            news = true;
        }
        if let Some(e) = event.workspace_active_window_changed
            && let Some(workspace) = self.workspaces.get_mut(&e.workspace_id)
            && workspace.active_window_id != e.active_window_id
        {
            workspace.active_window_id = e.active_window_id;
            // Only the workspace a screen is on puts its active window on the bar.
            news |= watching.windows && workspace.is_active;
        }
        if let Some(e) = event.windows_changed {
            self.windows = e.windows.into_iter().map(|w| (w.id, w.into())).collect();
            news = true;
        }
        if let Some(e) = event.window_opened_or_changed {
            let id = e.window.id;
            news |= match self.windows.insert(id, e.window.into()) {
                // A new window can fill a workspace the list had left off.
                None => true,
                Some(before) => {
                    let after = &self.windows[&id];
                    before.workspace != after.workspace
                        || (watching.windows && before.window != after.window && self.showing(id))
                }
            };
        }
        if let Some(e) = event.window_closed {
            news |= self.windows.remove(&e.id).is_some();
        }
        if let Some(e) = event.keyboard_layouts_changed {
            self.layouts = Some(e.keyboard_layouts);
            news = true;
        }
        if let Some(e) = event.keyboard_layout_switched {
            if let Some(layouts) = &mut self.layouts {
                layouts.current_idx = e.idx;
            }
            news = true;
        }
        news
    }

    /// Whether a screen is showing this window: it is the active window of the workspace
    /// active there.
    fn showing(&self, window: u64) -> bool {
        self.workspaces
            .values()
            .any(|w| w.is_active && w.active_window_id == Some(window))
    }

    /// The desktop as the bar holds it.
    fn desktop(&self, watching: Watching) -> Desktop {
        let occupied: HashSet<u64> = self.windows.values().filter_map(|h| h.workspace).collect();
        // niri keeps an empty workspace below the last one in use on every screen, so there is
        // always somewhere to open a window. Listed, it is a number that never holds anything,
        // so it is left off until a screen is showing it. A workspace the config named is
        // listed whether or not anything is on it.
        let mut listed: Vec<&NiriWorkspace> = self
            .workspaces
            .values()
            .filter(|w| w.is_active || w.name.is_some() || occupied.contains(&w.id))
            .collect();
        listed.sort_by(|a, b| (&a.output, a.idx).cmp(&(&b.output, b.idx)));
        let workspaces = listed
            .into_iter()
            .map(|w| Workspace {
                id: w.id,
                name: w.name.clone().unwrap_or_else(|| w.idx.to_string()),
                output: w.output.clone().unwrap_or_default(),
                focused: w.is_focused,
                visible: w.is_active,
                urgent: w.is_urgent,
            })
            .collect();

        // What a screen shows is the active window of the workspace active on it. A bar with
        // no window module keeps none of this, so a title changing is nothing it redraws for.
        let windows = match watching.windows {
            true => self
                .workspaces
                .values()
                .filter(|w| w.is_active)
                .filter_map(|w| {
                    let held = self.windows.get(&w.active_window_id?)?;
                    Some((w.output.clone()?, held.window.clone()))
                })
                .collect(),
            false => HashMap::new(),
        };
        let layout = match watching.language {
            true => self.layouts.as_ref().and_then(|layouts| {
                Some(Layout {
                    name: layouts.names.get(usize::from(layouts.current_idx))?.clone(),
                    index: u32::from(layouts.current_idx),
                })
            }),
            false => None,
        };
        Desktop {
            workspaces,
            windows,
            focused_output: self
                .workspaces
                .values()
                .find(|w| w.is_focused)
                .and_then(|w| w.output.clone()),
            layout,
            // niri has no binding modes.
            mode: None,
        }
    }
}

/// The longest event taken from niri, in bytes.
///
/// niri opens its stream with every window in the session on one line, a few hundred bytes
/// each, so that line grows with the session rather than with the bar. A cut event cannot
/// be read, and the bar would be left following a desktop niri no longer has, so this is set
/// far past any session a person runs. It is still a limit, and the memory is held only while
/// such a line is being read.
const EVENT_LIMIT: usize = 16 * 1024 * 1024;

/// Ask niri for its event stream and forward what it says into the event loop.
pub fn spawn(sender: calloop::channel::Sender<DesktopEvent>, watching: Watching) -> Result<()> {
    // Asked here rather than on the thread, so a socket that is not there or a niri that
    // refuses is reported at startup instead of silently leaving the modules empty.
    let mut stream = connect()?;
    stream
        .write_all(b"\"EventStream\"\n")
        .context("asking niri for its event stream")?;
    let mut lines = lines::capped_at(BufReader::new(stream), EVENT_LIMIT);
    let reply = lines
        .next()
        .context("niri closed its socket instead of answering")?
        .context("reading niri's answer")?;
    check(&reply.text).context("asking niri for its event stream")?;
    if watching.mode {
        log::info!("niri has no binding modes, so a mode module stays empty");
    }

    let publisher = Publisher::new(sender);
    std::thread::Builder::new()
        .name("niri-ipc".to_string())
        .spawn(move || follow(lines, publisher, watching))
        .context("spawning the niri IPC thread")?;
    Ok(())
}

fn follow(mut lines: Lines<BufReader<UnixStream>>, mut publisher: Publisher, watching: Watching) {
    let mut state = State::default();
    let mut unreadable = false;
    loop {
        // Everything niri has already sent is taken before the desktop is rebuilt. The stream
        // opens with six events at once, and moving to another workspace is an activation,
        // a focus change and often a window: one rebuild for the lot is one redraw.
        let mut news = false;
        loop {
            let line = match lines.next() {
                Some(Ok(line)) => line,
                Some(Err(e)) => {
                    publisher.stop(format!("reading niri's event stream: {e}"));
                    return;
                }
                None => {
                    publisher.stop("niri closed its event stream".to_string());
                    return;
                }
            };
            // A cut event is not JSON, and stepping over it would leave the bar showing a
            // desktop niri no longer has.
            if line.dropped > 0 {
                publisher.stop(format!(
                    "niri sent an event longer than {EVENT_LIMIT} bytes"
                ));
                return;
            }
            match serde_json::from_str::<Event>(&line.text) {
                Ok(event) => news |= state.apply(event, watching),
                // A newer niri may describe something in a way this dbar does not read. Said
                // once: the same event will keep arriving, and the log is not the place for it.
                Err(e) if !unreadable => {
                    unreadable = true;
                    log::warn!("skipping events from niri that this dbar cannot read: {e}");
                }
                Err(_) => {}
            }
            if lines.reader().buffer().is_empty() && !more_waiting(lines.reader().get_ref()) {
                break;
            }
        }
        if news && !publisher.publish(&state.desktop(watching)) {
            return;
        }
    }
}

/// A command, written as the request niri takes for it.
fn request(command: &Command) -> String {
    match command {
        // By id: an unnamed workspace has no name to give, and a position only means
        // something on the screen the keyboard is on.
        Command::FocusWorkspace { id, .. } => {
            serde_json::json!({ "Action": { "FocusWorkspace": { "reference": { "Id": id } } } })
                .to_string()
        }
    }
}

/// Send niri a command on its own connection, since the stream's cannot carry requests.
pub fn run_command(command: Command) {
    let result = (|| -> Result<()> {
        let mut stream = connect()?;
        stream
            .write_all(format!("{}\n", request(&command)).as_bytes())
            .context("sending niri a request")?;
        let reply = lines::capped(BufReader::new(stream))
            .next()
            .context("niri closed its socket instead of answering")?
            .context("reading niri's reply")?;
        check(&reply.text)
    })();
    match result {
        Ok(()) => log::debug!("niri did {command:?}"),
        Err(e) => log::warn!("asking niri for {command:?}: {e:#}"),
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

    /// How niri 26.04 opened its stream for one screen: a workspace the config names holding
    /// a terminal, and the empty one below it in focus.
    const OPENING: &[&str] = &[
        r#"{"WorkspacesChanged":{"workspaces":[{"id":1,"idx":1,"name":"chat","output":"winit","is_urgent":false,"is_active":false,"is_focused":false,"active_window_id":2},{"id":2,"idx":2,"name":null,"output":"winit","is_urgent":false,"is_active":true,"is_focused":true,"active_window_id":null}]}}"#,
        r#"{"WindowsChanged":{"windows":[{"id":2,"title":"title-b","app_id":"foot","pid":638145,"workspace_id":1,"is_focused":false,"is_floating":false,"is_urgent":false,"layout":{"pos_in_scrolling_layout":[1,1],"tile_size":[429.0,494.0],"window_size":[429,494],"tile_pos_in_workspace_view":null,"window_offset_in_tile":[0.0,0.0]},"focus_timestamp":{"secs":35454,"nanos":380363883}}]}}"#,
        r#"{"KeyboardLayoutsChanged":{"keyboard_layouts":{"names":["English (US)","Serbian"],"current_idx":1}}}"#,
        r#"{"OverviewOpenedOrClosed":{"is_open":false}}"#,
        r#"{"ConfigLoaded":{"failed":false}}"#,
        r#"{"CastsChanged":{"casts":[]}}"#,
    ];

    /// What followed: back up to the named workspace, where a second terminal opened.
    const OPENED_A_TERMINAL: &[&str] = &[
        r#"{"WorkspaceActivated":{"id":1,"focused":true}}"#,
        r#"{"WindowFocusChanged":{"id":2}}"#,
        r#"{"WorkspaceActiveWindowChanged":{"workspace_id":1,"active_window_id":3}}"#,
        r#"{"WindowOpenedOrChanged":{"window":{"id":3,"title":"title-a","app_id":"foot","pid":638599,"workspace_id":1,"is_focused":true,"is_floating":false,"is_urgent":false,"layout":{"pos_in_scrolling_layout":[2,1],"tile_size":[429.0,494.0],"window_size":[429,494],"tile_pos_in_workspace_view":null,"window_offset_in_tile":[0.0,0.0]},"focus_timestamp":null}}}"#,
        r#"{"WindowFocusTimestampChanged":{"id":2,"focus_timestamp":{"secs":35476,"nanos":860862081}}}"#,
    ];

    /// The new terminal setting its title.
    const RETITLED: &str = r#"{"WindowOpenedOrChanged":{"window":{"id":3,"title":"title-b","app_id":"foot","pid":638599,"workspace_id":1,"is_focused":true,"is_floating":false,"is_urgent":false,"focus_timestamp":null}}}"#;

    /// Take events in for a bar showing everything, and say whether any of them was news.
    fn replay(state: &mut State, lines: &[&str]) -> bool {
        replay_watching(state, lines, EVERYTHING)
    }

    fn replay_watching(state: &mut State, lines: &[&str], watching: Watching) -> bool {
        let mut news = false;
        for line in lines {
            let event = serde_json::from_str(line).expect("an event from niri parses");
            news |= state.apply(event, watching);
        }
        news
    }

    fn names(desktop: &Desktop) -> Vec<&str> {
        desktop.workspaces.iter().map(|w| w.name.as_str()).collect()
    }

    /// The stream opens with the whole desktop, so a bar is right from its first burst
    /// without asking niri anything.
    #[test]
    fn the_stream_opens_with_the_whole_desktop() {
        let mut state = State::default();
        replay(&mut state, OPENING);
        let desktop = state.desktop(EVERYTHING);
        assert_eq!(names(&desktop), ["chat", "2"]);
        assert_eq!(desktop.workspaces[0].id, 1);
        assert!(!desktop.workspaces[0].visible);
        assert!(
            desktop.workspaces[1].focused && desktop.workspaces[1].visible,
            "the empty workspace is listed while the screen is on it"
        );
        assert_eq!(desktop.focused_output.as_deref(), Some("winit"));
        assert!(
            desktop.windows.is_empty(),
            "an empty workspace shows no window"
        );
        assert_eq!(
            desktop.layout,
            Some(Layout {
                name: "Serbian".to_string(),
                index: 1,
            })
        );
        assert_eq!(desktop.mode, None);
    }

    /// A screen shows the active window of the workspace active on it, under the title it
    /// has now. The empty workspace the screen left is not listed any more.
    #[test]
    fn a_screen_shows_the_window_its_workspace_has_active() {
        let mut state = State::default();
        replay(&mut state, OPENING);
        replay(&mut state, OPENED_A_TERMINAL);
        let desktop = state.desktop(EVERYTHING);
        let window = &desktop.windows["winit"];
        assert_eq!(
            (window.title.as_str(), window.app_id.as_str()),
            ("title-a", "foot")
        );
        assert_eq!(names(&desktop), ["chat"]);

        replay(&mut state, &[RETITLED]);
        assert_eq!(state.desktop(EVERYTHING).windows["winit"].title, "title-b");
    }

    /// niri reports a title change for every window. One that is not the active window on a
    /// screen changes nothing the bar holds, and a bar with no window module holds no titles.
    #[test]
    fn a_title_no_screen_is_showing_changes_nothing() {
        let mut state = State::default();
        replay(&mut state, OPENING);
        replay(&mut state, OPENED_A_TERMINAL);
        let before = state.desktop(EVERYTHING);
        let news = replay(
            &mut state,
            &[
                r#"{"WindowOpenedOrChanged":{"window":{"id":2,"title":"make: building","app_id":"foot","workspace_id":1,"is_focused":false}}}"#,
            ],
        );
        assert!(!news, "nothing to rebuild the desktop for");
        assert_eq!(state.desktop(EVERYTHING), before);

        let workspaces_only = Watching {
            workspaces: true,
            ..Watching::default()
        };
        let before = state.desktop(workspaces_only);
        assert!(!replay_watching(&mut state, &[RETITLED], workspaces_only));
        assert_eq!(state.desktop(workspaces_only), before);
    }

    /// A window event is news when the bar can show it: the title on a screen, or a window
    /// opening or moving, which can change which workspaces are listed. The same title again,
    /// the active window of a workspace no screen is on, and a window niri never announced
    /// are not.
    #[test]
    fn only_a_window_change_the_bar_can_show_is_news() {
        let mut state = State::default();
        replay(&mut state, OPENING);
        replay(&mut state, OPENED_A_TERMINAL);
        let workspaces_only = Watching {
            workspaces: true,
            ..Watching::default()
        };

        assert!(replay(&mut state, &[RETITLED]), "the title on the screen");
        assert!(!replay(&mut state, &[RETITLED]), "the same title again");

        let moved = r#"{"WindowOpenedOrChanged":{"window":{"id":2,"title":"title-b","app_id":"foot","workspace_id":2,"is_focused":false}}}"#;
        assert!(
            replay_watching(&mut state, &[moved], workspaces_only),
            "a window moving can fill or empty a workspace"
        );
        let opened = r#"{"WindowOpenedOrChanged":{"window":{"id":4,"title":"new","app_id":"foot","workspace_id":2,"is_focused":false}}}"#;
        assert!(replay_watching(&mut state, &[opened], workspaces_only));

        assert!(!replay(&mut state, &[r#"{"WindowClosed":{"id":42}}"#]));
        let behind = r#"{"WorkspaceActiveWindowChanged":{"workspace_id":2,"active_window_id":4}}"#;
        assert!(
            !replay(&mut state, &[behind]),
            "no screen is on workspace 2"
        );

        let on_screen =
            r#"{"WorkspaceActiveWindowChanged":{"workspace_id":1,"active_window_id":2}}"#;
        assert!(!replay_watching(&mut state, &[on_screen], workspaces_only));
        let again = r#"{"WorkspaceActiveWindowChanged":{"workspace_id":1,"active_window_id":3}}"#;
        assert!(
            replay(&mut state, &[again]),
            "the screen shows another window"
        );
        assert_eq!(state.desktop(EVERYTHING).windows["winit"].title, "title-b");
    }

    /// Geometry, focus timestamps, the overview and screencasts all arrive on the same
    /// stream, and none of them is a reason to rebuild what the bar shows.
    #[test]
    fn events_a_bar_does_not_draw_are_stepped_over() {
        let mut state = State::default();
        for line in [
            r#"{"WindowFocusTimestampChanged":{"id":2,"focus_timestamp":{"secs":35476,"nanos":860862081}}}"#,
            r#"{"WindowLayoutsChanged":{"changes":[[2,{"pos_in_scrolling_layout":[1,1],"tile_size":[429.0,494.0],"window_size":[429,494],"tile_pos_in_workspace_view":null,"window_offset_in_tile":[0.0,0.0]}]]}}"#,
            r#"{"WindowFocusChanged":{"id":null}}"#,
            r#"{"OverviewOpenedOrClosed":{"is_open":true}}"#,
            r#"{"CastsChanged":{"casts":[]}}"#,
            r#"{"ScreenshotCaptured":{"path":null}}"#,
        ] {
            let event = serde_json::from_str(line).expect("an event dbar ignores still parses");
            assert!(!state.apply(event, EVERYTHING), "{line}");
        }
    }

    /// Two screens: each lists its own workspaces in order, an unnamed one is called by its
    /// position, and the empty workspace niri keeps below the last one in use is left off
    /// while no screen is showing it.
    #[test]
    fn each_screen_lists_its_workspaces_without_the_spare_one_below() {
        let mut state = State::default();
        replay(
            &mut state,
            &[
                r#"{"WorkspacesChanged":{"workspaces":[
                    {"id":6,"idx":2,"name":null,"output":"DP-1","is_urgent":false,"is_active":false,"is_focused":false,"active_window_id":null},
                    {"id":5,"idx":1,"name":null,"output":"DP-1","is_urgent":true,"is_active":true,"is_focused":true,"active_window_id":9},
                    {"id":7,"idx":1,"name":null,"output":"HDMI-A-1","is_urgent":false,"is_active":true,"is_focused":false,"active_window_id":null}]}}"#,
                r#"{"WindowsChanged":{"windows":[{"id":9,"title":"vim","app_id":"foot","workspace_id":5,"is_focused":true}]}}"#,
            ],
        );
        let desktop = state.desktop(EVERYTHING);
        let listed: Vec<(u64, &str, &str)> = desktop
            .workspaces
            .iter()
            .map(|w| (w.id, w.name.as_str(), w.output.as_str()))
            .collect();
        assert_eq!(listed, [(5, "1", "DP-1"), (7, "1", "HDMI-A-1")]);
        assert!(desktop.workspaces[0].urgent);
        assert_eq!(desktop.windows["DP-1"].title, "vim");
        assert!(!desktop.windows.contains_key("HDMI-A-1"));

        // Activating a workspace on one screen leaves the other screen's alone, and only an
        // activation that focuses moves the focus.
        replay(
            &mut state,
            &[r#"{"WorkspaceActivated":{"id":6,"focused":false}}"#],
        );
        let desktop = state.desktop(EVERYTHING);
        let active: Vec<(u64, bool, bool)> = desktop
            .workspaces
            .iter()
            .map(|w| (w.id, w.visible, w.focused))
            .collect();
        assert_eq!(
            active,
            [(5, false, true), (6, true, false), (7, true, false)]
        );
    }

    #[test]
    fn a_layout_switch_names_the_layout_switched_to() {
        let mut state = State::default();
        replay(&mut state, OPENING);
        replay(&mut state, &[r#"{"KeyboardLayoutSwitched":{"idx":0}}"#]);
        assert_eq!(
            state.desktop(EVERYTHING).layout,
            Some(Layout {
                name: "English (US)".to_string(),
                index: 0,
            })
        );
        let no_language = Watching {
            language: false,
            ..EVERYTHING
        };
        assert_eq!(state.desktop(no_language).layout, None);
    }

    /// niri does not promise that one event agrees with the next: a window can close while
    /// a workspace still names it active. That is a screen showing no window for a moment,
    /// not a reason to stop following niri.
    #[test]
    fn a_window_that_is_gone_is_not_shown() {
        let mut state = State::default();
        replay(&mut state, OPENING);
        replay(&mut state, OPENED_A_TERMINAL);
        replay(
            &mut state,
            &[
                r#"{"WindowClosed":{"id":3}}"#,
                r#"{"WorkspaceActiveWindowChanged":{"workspace_id":99,"active_window_id":3}}"#,
            ],
        );
        assert!(state.desktop(EVERYTHING).windows.is_empty());
    }

    #[test]
    fn clicking_a_workspace_asks_niri_for_it_by_id() {
        let command = Command::FocusWorkspace {
            id: 2,
            name: "2".to_string(),
        };
        assert_eq!(
            request(&command),
            r#"{"Action":{"FocusWorkspace":{"reference":{"Id":2}}}}"#
        );
    }

    #[test]
    fn niri_answers_yes_or_says_why_not() {
        assert!(check(r#"{"Ok":"Handled"}"#).is_ok());
        let refused = check(r#"{"Err":"no such workspace"}"#).expect_err("an Err is refused");
        assert!(format!("{refused:#}").contains("no such workspace"));
        assert!(check("not json").is_err());
    }
}
