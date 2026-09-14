//! What the compositor tells the bar: the workspaces, the window each screen is showing,
//! the keyboard layout and the binding mode.
//!
//! This is the compositor's state in the bar's own terms. A backend speaks its compositor's
//! IPC on threads of its own and publishes a whole `Desktop`; layout reads that and never
//! learns which compositor it came from, or how that compositor spells a command. Sway is
//! the only backend so far.

use std::collections::HashMap;

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

/// Something a click asks the compositor to do, in the bar's terms.
///
/// Each backend writes it in its own compositor's language, so layout can say what a click
/// is for without knowing how any compositor quotes a workspace name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    FocusWorkspace(String),
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
    pub fn desktop(self) -> bool {
        self.windows || self.workspaces
    }
}

/// The compositors dbar can talk to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Sway,
}

impl Backend {
    /// The compositor this session is running, which each one says by the socket it puts
    /// in the environment.
    pub fn detect() -> Result<Backend> {
        if std::env::var_os("SWAYSOCK").is_some() {
            return Ok(Backend::Sway);
        }
        bail!("SWAYSOCK is not set; is this a Sway session?")
    }

    /// Connect, read what the compositor has now, and forward its changes into the event
    /// loop from a thread of the backend's own.
    pub fn spawn(
        self,
        sender: calloop::channel::Sender<DesktopEvent>,
        watching: Watching,
    ) -> Result<()> {
        match self {
            Backend::Sway => crate::sway::spawn(sender, watching),
        }
    }

    /// Start the thread that carries clicks to the compositor.
    pub fn commands(self) -> Commands {
        match self {
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
