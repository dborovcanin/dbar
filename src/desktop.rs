//! What the compositor tells the bar: the workspaces, the window each screen is showing,
//! the keyboard layout and the binding mode.
//!
//! This is the compositor's state in the bar's own terms. A backend speaks its compositor's
//! IPC on threads of its own and publishes a whole `Desktop`; layout reads that and never
//! learns which compositor it came from, or how that compositor spells a command. Sway,
//! niri and Hyprland are the backends so far.

use std::collections::HashMap;
use std::os::unix::net::UnixStream;

use anyhow::{Result, bail};

use crate::status::{FieldSpec, Kind, Unit};

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
#[derive(Clone, Debug, PartialEq)]
pub struct Workspace {
    /// What the compositor calls this workspace for as long as it exists. A name need not
    /// be that: niri's workspaces are often unnamed, and their positions move.
    pub id: u64,
    pub name: String,
    /// The screen it is on, named the way the compositor names it: "DP-1". A bar on one
    /// screen lists the workspaces of that screen, so this is what ties the two together.
    pub output: String,
    pub focused: bool,
    pub visible: bool,
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
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Desktop {
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
    /// The binding mode held, or nothing while the keyboard is in its ordinary one.
    ///
    /// A compositor that gives its ordinary mode a name - sway calls it `default` - has
    /// that turned into nothing by its backend, so a mode here is always one worth showing.
    pub mode: Option<String>,
}

#[derive(Debug)]
pub enum DesktopEvent {
    State(Box<Desktop>),
    Stopped(String),
}

/// The way a backend hands the bar its state: only when what the bar would draw has changed.
///
/// A compositor reports far more than a bar shows - a title change for every window, among
/// them ones no screen is showing - and a bar handed an identical state still lays out every
/// screen to find that nothing moved. Keeping the rule here means a new backend cannot
/// forget it.
pub struct Publisher {
    sender: calloop::channel::SyncSender<DesktopEvent>,
    shown: Option<Desktop>,
}

impl Publisher {
    pub fn new(sender: calloop::channel::SyncSender<DesktopEvent>) -> Publisher {
        Publisher {
            sender,
            shown: None,
        }
    }

    /// Hand the bar this state unless it already has it. False once the bar has gone, which
    /// is a backend's cue to stop.
    pub fn publish(&mut self, state: &Desktop) -> bool {
        if self.shown.as_ref() == Some(state) {
            return true;
        }
        self.shown = Some(state.clone());
        self.sender
            .send(DesktopEvent::State(Box::new(state.clone())))
            .is_ok()
    }

    /// Tell the bar the compositor can no longer be followed.
    pub fn stop(&self, reason: String) {
        let _ = self.sender.send(DesktopEvent::Stopped(reason));
    }
}

/// Something a click asks the compositor to do, in the bar's terms.
///
/// Each backend writes it in its own compositor's language, so layout can say what a click
/// is for without knowing how any compositor quotes a workspace name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Both ways of naming the workspace travel, and each backend uses the one its compositor
    /// switches by: sway takes a name, niri an id, and Hyprland one or the other depending
    /// on how the workspace was made.
    FocusWorkspace { id: u64, name: String },
}

/// How many commands may be waiting for the compositor at once.
///
/// Clicking a workspace is one command, and a hand clicking as fast as it can is a few a
/// second. Anything past this is a compositor that has stopped answering, and queueing
/// for one of those only means switching to workspaces nobody wants any more.
const QUEUED_COMMANDS: usize = 16;

/// The way to send the compositor a command without waiting for it to answer.
///
/// Connecting, writing and reading a reply all block, and the thread a click arrives on
/// is the one that draws: a compositor that is slow to answer would stop the bar
/// redrawing and stop it dispatching Wayland, which is a bar that has frozen.
pub struct Commands(std::sync::mpsc::SyncSender<Command>);

impl Commands {
    /// Start the thread a backend runs its commands on.
    fn spawn(name: &str, run: fn(Command)) -> Commands {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<Command>(QUEUED_COMMANDS);
        let started = std::thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                while let Ok(command) = receiver.recv() {
                    run(command);
                }
            });
        if let Err(e) = started {
            log::warn!("no thread for compositor commands: {e}");
        }
        Commands(sender)
    }

    pub fn send(&self, command: Command) {
        // A queue this full is a compositor that is not listening, and a click nobody is
        // going to act on is better dropped than remembered.
        if let Err(e) = self.0.try_send(command) {
            log::debug!("the compositor is not keeping up with commands: {e}");
        }
    }
}

/// What the bar has asked the compositor for.
///
/// Each of these is something a backend has to subscribe to and ask about at startup, and
/// windows are the noisiest thing a compositor reports. A bar that draws none of them never
/// connects at all.
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
    pub fn follows_workspaces(self) -> bool {
        self.windows || self.workspaces
    }
}

/// How long a request to a compositor may take before the bar stops waiting on it.
///
/// Each backend asks its compositor on the main thread, before the event loop is running, so
/// a compositor wedged on its own main thread would otherwise be a bar that never draws and
/// never says why. Bounded, that becomes the error path every backend already has.
///
/// Request sockets only. An event stream is meant to sit idle for hours and is left
/// unbounded, and so is a request socket once the bar is up, where giving up part way
/// through a reply would leave a shared connection out of step with its protocol. A backend
/// that gives every request a connection of its own keeps the bound, since a request that
/// gives up there costs nothing but that connection.
pub const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Whether another message has already arrived on a compositor's socket, so a burst can be
/// taken in one go.
///
/// This asks the kernel only: a reader with a buffer of its own has to look in that first.
pub fn more_waiting(stream: &UnixStream) -> bool {
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

/// The compositors dbar can talk to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Hyprland,
    Niri,
    Sway,
}

impl Backend {
    /// The compositor this session is running, which each one says by the socket it puts
    /// in the environment.
    pub fn detect() -> Result<Backend> {
        // Sway last: it hands its socket to everything it starts, so a compositor started
        // inside a Sway session - which is how one is usually tried out - passes Sway's
        // along with its own.
        if std::env::var_os("NIRI_SOCKET").is_some() {
            return Ok(Backend::Niri);
        }
        if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
            return Ok(Backend::Hyprland);
        }
        if std::env::var_os("SWAYSOCK").is_some() {
            return Ok(Backend::Sway);
        }
        bail!(
            "none of NIRI_SOCKET, HYPRLAND_INSTANCE_SIGNATURE or SWAYSOCK is set; dbar \
             follows niri, Hyprland and Sway"
        )
    }

    /// Connect, read what the compositor has now, and forward its changes into the event
    /// loop from a thread of the backend's own.
    pub fn spawn(
        self,
        sender: calloop::channel::SyncSender<DesktopEvent>,
        watching: Watching,
    ) -> Result<()> {
        match self {
            Backend::Hyprland => crate::hypr::spawn(sender, watching),
            Backend::Niri => crate::niri::spawn(sender, watching),
            Backend::Sway => crate::sway::spawn(sender, watching),
        }
    }

    /// Start the thread that carries clicks to the compositor.
    pub fn commands(self) -> Commands {
        match self {
            Backend::Hyprland => Commands::spawn("hypr-commands", crate::hypr::run_command),
            Backend::Niri => Commands::spawn("niri-commands", crate::niri::run_command),
            Backend::Sway => Commands::spawn("sway-commands", crate::sway::run_command),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// What the bar asks for decides what it subscribes to and what it re-reads, so this
    /// is the switch that keeps a clock-only bar off the compositor entirely.
    #[test]
    fn nothing_from_the_compositor_means_nothing_to_ask_it() {
        assert!(!Watching::default().anything());
        assert!(!Watching::default().follows_workspaces());

        let language = Watching {
            language: true,
            ..Watching::default()
        };
        assert!(language.anything(), "a layout still needs a connection");
        assert!(
            !language.follows_workspaces(),
            "but not the workspaces or the windows"
        );

        let workspaces = Watching {
            workspaces: true,
            ..Watching::default()
        };
        assert!(workspaces.follows_workspaces());
        assert!(
            !workspaces.windows,
            "and no windows, which are the noisy half"
        );
    }

    /// A burst of compositor events that changed nothing the bar draws must cost the bar
    /// nothing, and a backend must hear when there is no bar left to tell.
    #[test]
    fn a_state_the_bar_already_has_is_not_handed_over_again() {
        let (sender, channel) = calloop::channel::sync_channel(8);
        let mut publisher = Publisher::new(sender);
        let mut state = Desktop::default();
        assert!(publisher.publish(&state), "the first state always goes");
        assert!(publisher.publish(&state));
        state.mode = Some("resize".to_string());
        assert!(publisher.publish(&state));

        let mut event_loop = calloop::EventLoop::<Vec<Desktop>>::try_new().expect("an event loop");
        event_loop
            .handle()
            .insert_source(channel, |event, _, got: &mut Vec<Desktop>| {
                if let calloop::channel::Event::Msg(DesktopEvent::State(state)) = event {
                    got.push(*state);
                }
            })
            .expect("a channel is an event source");
        let mut got = Vec::new();
        event_loop
            .dispatch(Some(std::time::Duration::ZERO), &mut got)
            .expect("dispatching");
        assert_eq!(got.len(), 2, "the repeat was not sent");
        assert_eq!(got[1].mode.as_deref(), Some("resize"));

        drop(event_loop);
        state.mode = None;
        assert!(!publisher.publish(&state), "a bar that has gone says so");
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
