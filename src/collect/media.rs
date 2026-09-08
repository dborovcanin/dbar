//! What is playing, over MPRIS.
//!
//! Every player that wants to be controlled from outside puts itself on the session bus
//! under `org.mpris.MediaPlayer2.*`, and says what it is doing through the standard
//! properties. So this watches the bus rather than asking on a timer: a track changes when
//! it changes, and nothing about a paused player is worth waking up for.
//!
//! Which player is a real question on a desktop where a browser tab, a music player and a
//! video call are all on the bus at once. What a person means by "what is playing" is the
//! one that is actually playing, so that is what is preferred, and a paused player only
//! speaks when nothing else does.

use std::collections::HashMap;
use std::os::fd::{AsRawFd as _, OwnedFd};

use anyhow::{Context as _, Result};

use super::Reading;
use crate::dbus::{Arg, Connection, Value};
use crate::status::{FieldSpec, Fields, Kind, State, Value as Field};

pub const FIELDS: &[FieldSpec] = &[
    FieldSpec {
        name: "title",
        kind: Kind::Text,
    },
    FieldSpec {
        name: "artist",
        kind: Kind::Text,
    },
    FieldSpec {
        name: "album",
        kind: Kind::Text,
    },
    // `playing`, `paused` or `stopped`, lowercased so a state rule can match it without
    // knowing how the player capitalises.
    FieldSpec {
        name: "status",
        kind: Kind::Text,
    },
    // What is playing it: "Firefox", "Spotify". Its own name for itself.
    FieldSpec {
        name: "player",
        kind: Kind::Text,
    },
];

const PREFIX: &str = "org.mpris.MediaPlayer2.";
const OBJECT: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// What the bar can ask a player to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    PlayPause,
    Next,
    Previous,
}

impl Command {
    /// The MPRIS method that does it.
    fn member(self) -> &'static str {
        match self {
            Command::PlayPause => "PlayPause",
            Command::Next => "Next",
            Command::Previous => "Previous",
        }
    }

    fn from_byte(byte: u8) -> Option<Command> {
        Some(match byte {
            0 => Command::PlayPause,
            1 => Command::Next,
            2 => Command::Previous,
            _ => return None,
        })
    }
}

/// The way into the media thread, which is blocked on the bus and cannot be interrupted
/// any other way.
///
/// A pipe rather than a channel, because the thread has to wait on the bus and on the bar
/// at the same time, and `poll()` takes descriptors.
pub struct Commands {
    pipe: OwnedFd,
}

impl Commands {
    pub fn send(&self, command: Command) {
        let byte = [match command {
            Command::PlayPause => 0u8,
            Command::Next => 1,
            Command::Previous => 2,
        }];
        // SAFETY: a write of one byte from a buffer owned here, to a descriptor this
        // struct owns.
        let written = unsafe { libc::write(self.pipe.as_raw_fd(), byte.as_ptr().cast(), 1) };
        if written != 1 {
            log::debug!("the media thread is not listening");
        }
    }
}

/// Start watching the session bus, and report what is playing as it changes.
pub fn spawn(sender: calloop::channel::Sender<Reading>) -> Result<Commands> {
    let (read, write) = pipe()?;
    std::thread::Builder::new()
        .name("media".to_string())
        .spawn(move || {
            crate::worker::forever(
                "the session bus",
                || run(&sender, &read),
                || silence(&sender).is_ok(),
            );
        })
        .context("spawning the media thread")?;
    Ok(Commands { pipe: write })
}

/// Say that nothing is playing, which is what a bar with no bus to ask has.
///
/// Also how the thread learns the bar has gone: the channel closes with it, and a track
/// nobody is going to draw is not worth reconnecting for.
fn silence(
    sender: &calloop::channel::Sender<Reading>,
) -> Result<(), std::sync::mpsc::SendError<Reading>> {
    let mut fields = Fields::default();
    for spec in FIELDS {
        fields.set(spec.name, Field::Absent);
    }
    sender.send(Reading {
        fields,
        state: State::Idle,
    })
}

fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut ends = [0 as libc::c_int; 2];
    // SAFETY: the array is owned here and is the length the call requires.
    let made = unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC) };
    if made < 0 {
        return Err(std::io::Error::last_os_error()).context("making a pipe for media commands");
    }
    // SAFETY: both descriptors are fresh, checked, and owned by nothing else.
    unsafe {
        use std::os::fd::FromRawFd as _;
        Ok((OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])))
    }
}

/// What one player last said about itself.
#[derive(Clone, Debug, Default, PartialEq)]
struct Playing {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    status: String,
    player: Option<String>,
}

impl Playing {
    /// Whether this player is worth showing over another.
    ///
    /// One that is playing beats one that is paused, which beats one that is stopped; a
    /// player with nothing to say at all loses to anything.
    fn interest(&self) -> u8 {
        match (self.status.as_str(), self.title.is_some()) {
            ("playing", _) => 3,
            ("paused", _) => 2,
            (_, true) => 1,
            _ => 0,
        }
    }
}

fn run(sender: &calloop::channel::Sender<Reading>, commands: &OwnedFd) -> Result<()> {
    let mut bus = Connection::session()?;
    // Every player's property changes, and every player appearing or going away.
    bus.add_match(&format!(
        "type='signal',interface='{PROPERTIES}',member='PropertiesChanged',path='{OBJECT}'"
    ))?;
    bus.add_match(
        "type='signal',interface='org.freedesktop.DBus',member='NameOwnerChanged',\
         arg0namespace='org.mpris.MediaPlayer2'",
    )?;

    log::info!("watching the session bus for a player");
    let mut showing: Option<Playing> = None;
    let mut name: Option<String> = None;
    // What each player calls itself, which it will go on calling itself.
    let mut identities = Identities::new();
    publish(&mut bus, sender, &mut showing, &mut name, &mut identities);

    loop {
        let mut fds = [
            libc::pollfd {
                fd: bus.fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: commands.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // A read takes everything the socket had, which is often several messages, so a
        // message can be in hand while the socket itself has nothing more to report.
        // Sleeping then would hold a track change until some other application spoke; not
        // asking at all would hold play and pause behind whatever backlog there is, so the
        // wait is skipped rather than the question.
        let in_hand = bus.has_message();
        let timeout = match in_hand {
            true => 0,
            false => -1,
        };
        // SAFETY: both descriptors are owned by this thread and outlive the call, and the
        // length is the length of the array they are in.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("waiting on the session bus");
        }
        if in_hand {
            fds[0].revents = libc::POLLIN;
        }

        if fds[0].revents != 0 {
            // Everything the bus has to say is taken before anything is asked of anyone.
            // Any of these can change which player is worth showing, and all of them are
            // rare enough to answer by asking again rather than by tracking deltas - but a
            // player that changes three properties sends three signals, and answering each
            // one separately is three passes over every player on the bus for one event.
            let mut stale = false;
            loop {
                let message = bus.receive()?;
                if message.is_signal("org.freedesktop.DBus", "NameOwnerChanged") {
                    // The name is the same name whoever holds it, so what the last holder
                    // called itself is not what this one is called. Taking it out here
                    // rather than when the name leaves the bus is the difference that
                    // matters now that a burst is answered once: a player restarting
                    // inside one is a name that never appeared to go away.
                    if let Some(name) = message.body.first().and_then(Value::as_str) {
                        identities.remove(name);
                    }
                    stale = true;
                }
                stale |= message.is_signal(PROPERTIES, "PropertiesChanged");
                if !bus.has_message() && !readable(bus.fd()) {
                    break;
                }
            }
            if stale {
                publish(&mut bus, sender, &mut showing, &mut name, &mut identities);
            }
        }

        if fds[1].revents != 0 {
            let mut byte = [0u8; 8];
            // SAFETY: the buffer is owned here and the length is its own.
            let read =
                unsafe { libc::read(commands.as_raw_fd(), byte.as_mut_ptr().cast(), byte.len()) };
            if read <= 0 {
                return Ok(());
            }
            for command in byte[..read as usize]
                .iter()
                .filter_map(|b| Command::from_byte(*b))
            {
                act(&mut bus, name.as_deref(), command);
            }
        }
    }
}

/// Do what the bar asked of whichever player it is showing.
fn act(bus: &mut Connection, name: Option<&str>, command: Command) {
    let Some(name) = name else {
        return;
    };
    // The answer is a reply nobody reads: what the player did comes back as a property
    // change like any other, which is what the bar then draws.
    if let Err(e) = bus.send(name, OBJECT, PLAYER, command.member(), &[]) {
        log::warn!("{} did not reach {name}: {e:#}", command.member());
    }
}

/// Find the player worth showing and send what it is playing, if it has changed.
fn publish(
    bus: &mut Connection,
    sender: &calloop::channel::Sender<Reading>,
    showing: &mut Option<Playing>,
    name: &mut Option<String>,
    identities: &mut Identities,
) {
    let (found, playing) = match look(bus, identities) {
        Ok(found) => found,
        Err(e) => {
            log::debug!("the players could not be read: {e:#}");
            (None, None)
        }
    };
    *name = found;
    if *showing == playing {
        return;
    }
    *showing = playing.clone();
    log::debug!("playing: {playing:?}");

    let mut fields = Fields::default();
    let text = |value: Option<String>| match value {
        Some(text) if !text.is_empty() => Field::Text(text),
        // A track with no title is one the player has not loaded yet, and a format that
        // mentions it should draw nothing rather than an empty gap.
        _ => Field::Absent,
    };
    let playing = playing.unwrap_or_default();
    fields.set("title", text(playing.title));
    fields.set("artist", text(playing.artist));
    fields.set("album", text(playing.album));
    fields.set("status", text(Some(playing.status)));
    fields.set("player", text(playing.player));
    fields.set_primary("title");

    let _ = sender.send(Reading {
        fields,
        state: State::Idle,
    });
}

/// The most interesting player on the bus, and what it is playing.
fn look(
    bus: &mut Connection,
    identities: &mut Identities,
) -> Result<(Option<String>, Option<Playing>)> {
    let names = bus.call(
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "ListNames",
        &[],
    )?;
    let Some(Value::Seq(names)) = names.first() else {
        return Ok((None, None));
    };

    let players: Vec<&str> = names
        .iter()
        .filter_map(Value::as_str)
        .filter(|name| name.starts_with(PREFIX))
        .collect();
    // A player that has left the bus takes what it called itself with it, so the cache is
    // the size of what is running rather than of everything that ever ran.
    identities.retain(|known, _| players.contains(&known.as_str()));

    let mut best: Option<(String, Playing)> = None;
    for name in players {
        let Ok(playing) = ask(bus, name, identities) else {
            continue;
        };
        let better = match &best {
            Some((_, current)) => playing.interest() > current.interest(),
            None => true,
        };
        if better {
            best = Some((name.to_string(), playing));
        }
    }

    match best {
        // Nothing to say is not nothing to report: the module draws nothing, rather than
        // keeping the last track it saw for ever.
        Some((_, playing)) if playing.interest() == 0 => Ok((None, None)),
        Some((name, playing)) => Ok((Some(name), Some(playing))),
        None => Ok((None, None)),
    }
}

/// What one player says about itself.
fn ask(bus: &mut Connection, name: &str, identities: &mut Identities) -> Result<Playing> {
    let reply = bus.call(name, OBJECT, PROPERTIES, "GetAll", &[Arg::Str(PLAYER)])?;
    let properties = reply.first().context("a player that answered nothing")?;
    let metadata = properties.get("Metadata");

    let from = |key: &str| {
        metadata
            .and_then(|m| m.get(key))
            .and_then(Value::first_str)
            .map(str::to_string)
    };

    Ok(Playing {
        title: from("xesam:title"),
        artist: from("xesam:artist"),
        album: from("xesam:album"),
        status: properties
            .get("PlaybackStatus")
            .and_then(Value::as_str)
            .unwrap_or("stopped")
            .to_lowercase(),
        player: identity(bus, name, identities),
    })
}

/// What each player on the bus calls itself, by the name it is on the bus under.
type Identities = HashMap<String, String>;

/// The player's own name for itself, which is friendlier than its bus name.
///
/// Asked once per player and then remembered. A well-known name on the bus is one
/// application and an application does not rename itself, so this is the one property of
/// the three that can be kept - and it is a round trip like any other, which is half of
/// what a single property change costs when every player is asked again for all of it.
///
/// A player that does not answer is not remembered as having no name: an application that
/// is still starting up answers nothing for a moment, and it would be called nothing for
/// as long as it ran.
fn identity(bus: &mut Connection, name: &str, identities: &mut Identities) -> Option<String> {
    if let Some(known) = identities.get(name) {
        return Some(known.clone());
    }
    let asked = ask_identity(bus, name)?;
    identities.insert(name.to_string(), asked.clone());
    Some(asked)
}

fn ask_identity(bus: &mut Connection, name: &str) -> Option<String> {
    let reply = bus
        .call(
            name,
            OBJECT,
            PROPERTIES,
            "Get",
            &[Arg::Str("org.mpris.MediaPlayer2"), Arg::Str("Identity")],
        )
        .ok()?;
    reply.first()?.as_str().map(str::to_string)
}

/// Whether the bus has more to say without waiting for it.
fn readable(fd: std::os::fd::RawFd) -> bool {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one descriptor owned by the caller for the length of the call, a count that
    // matches, and no wait at all.
    unsafe { libc::poll(&mut poll, 1, 0) > 0 && poll.revents != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playing(status: &str, title: Option<&str>) -> Playing {
        Playing {
            title: title.map(str::to_string),
            artist: None,
            album: None,
            status: status.to_string(),
            player: None,
        }
    }

    #[test]
    fn a_player_that_is_playing_beats_one_that_is_paused() {
        assert!(
            playing("playing", Some("a song")).interest()
                > playing("paused", Some("another")).interest()
        );
        assert!(
            playing("paused", Some("a song")).interest()
                > playing("stopped", Some("another")).interest()
        );
    }

    #[test]
    fn a_player_with_nothing_loaded_is_worth_nothing() {
        assert_eq!(playing("stopped", None).interest(), 0);
        // A browser tab that has stopped still holds the last thing it played, and that is
        // worth showing when nothing else is.
        assert!(playing("stopped", Some("a song")).interest() > 0);
    }

    #[test]
    fn every_command_survives_the_pipe() {
        for command in [Command::PlayPause, Command::Next, Command::Previous] {
            let byte = match command {
                Command::PlayPause => 0,
                Command::Next => 1,
                Command::Previous => 2,
            };
            assert_eq!(Command::from_byte(byte), Some(command));
        }
        assert_eq!(Command::from_byte(9), None);
    }
}
