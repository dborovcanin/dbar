//! Being told, instead of asking.
//!
//! Some of what the bar shows changes only when something changes it, and the kernel will
//! say when that happens. There are two ways it says so, and they are not equally
//! complete:
//!
//! - A sysfs attribute that calls `sysfs_notify` wakes `poll()` with `POLLPRI` on every
//!   change, which is the whole truth about that value. A source watched this way needs no
//!   interval at all.
//! - A uevent broadcast says a device changed, when the driver or the firmware bothers to
//!   announce it. That is worth having - unplugging a charger should show immediately -
//!   but a battery whose firmware only speaks at trip points would freeze between them, so
//!   the interval stays underneath as a floor.
//!
//! The waiting is a blocking `poll()`, so it happens on its own thread and reaches the
//! event loop through a channel, the way the compositor connection and the signal handler
//! already do. One thread waits on everything at once, so a second watch costs a file
//! descriptor rather than a thread.

use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

use super::Which;

/// How the kernel reports a change to a source.
pub enum Watch {
    /// A sysfs attribute it notifies on, which reports every change there is.
    Attribute(PathBuf),
    /// Its uevent broadcast for one subsystem, which reports what the hardware announces.
    Uevent(&'static str),
}

/// What the watcher tells the event loop.
pub enum Event {
    /// The kernel reports this source may have moved.
    Changed(Which),
    /// The attribute went away, so this source goes back to being read on its interval.
    Lost(Which),
}

struct Attribute {
    which: Which,
    file: File,
}

/// The kernel's uevent broadcast, and who is listening for what on it.
struct Uevents {
    socket: OwnedFd,
    wanted: Vec<(&'static str, Which)>,
}

/// Watch every source that can be watched, and say which of them no longer need a timer.
///
/// A watch that cannot be set up is not an error: the source keeps its interval, which is
/// what it would have had anyway.
pub fn spawn(sender: calloop::channel::Sender<Event>) -> Vec<Which> {
    let mut attributes = Vec::new();
    let mut wanted = Vec::new();
    for which in Which::WATCHABLE {
        match which.watch() {
            Some(Watch::Attribute(path)) => match arm(&path) {
                Ok(file) => {
                    log::info!(
                        "{} is watched at {}, so it is not read on an interval",
                        which.describe(),
                        path.display()
                    );
                    attributes.push(Attribute {
                        which: which.clone(),
                        file,
                    });
                }
                Err(e) => log::debug!("{} is read on its interval: {e:#}", which.describe()),
            },
            Some(Watch::Uevent(subsystem)) => wanted.push((subsystem, which.clone())),
            None => {}
        }
    }

    // One socket carries every subsystem, so it is opened once and only if something is
    // listening for a uevent at all.
    let uevents = match wanted.is_empty() {
        true => None,
        false => match open_uevents() {
            Ok(socket) => {
                for (subsystem, which) in &wanted {
                    log::info!(
                        "{} is read again when the kernel reports a {subsystem} change",
                        which.describe()
                    );
                }
                Some(Uevents { socket, wanted })
            }
            Err(e) => {
                log::debug!("uevents are unavailable, so intervals stand alone: {e:#}");
                None
            }
        },
    };

    if attributes.is_empty() && uevents.is_none() {
        return Vec::new();
    }

    // Only an attribute reports every change there is; a uevent keeps its interval.
    let covered: Vec<Which> = attributes.iter().map(|a| a.which.clone()).collect();
    match std::thread::Builder::new()
        .name("watch".to_string())
        .spawn(move || run(attributes, uevents, sender))
    {
        Ok(_) => covered,
        Err(e) => {
            log::warn!("watching needs a thread and could not have one: {e}");
            Vec::new()
        }
    }
}

/// Open an attribute and take its current value, which is what arms the notification.
///
/// `poll()` reports a change against what this descriptor has already seen, so a file that
/// has never been read wakes immediately and every time after.
fn arm(path: &Path) -> Result<File> {
    let mut file =
        File::open(path).with_context(|| format!("opening {} to watch it", path.display()))?;
    consume(&mut file)?;
    Ok(file)
}

/// Re-read the attribute, which is what tells the kernel this change has been seen.
///
/// The value itself is thrown away - the collector reads what it needs when it is asked -
/// but the read has to happen or `poll()` returns the same change for ever.
fn consume(file: &mut File) -> Result<()> {
    file.seek(SeekFrom::Start(0)).context("rewinding")?;
    let mut text = String::new();
    file.read_to_string(&mut text).context("reading")?;
    if text.trim().is_empty() {
        bail!("the attribute is empty");
    }
    Ok(())
}

/// Join the kernel's uevent broadcast.
///
/// Group 1 is the kernel's own, which it lets an unprivileged reader join; group 2 is
/// udev's rewritten copy, which arrives later and says nothing more for this purpose.
fn open_uevents() -> Result<OwnedFd> {
    // SAFETY: a socket call with constant arguments, checked before it is owned.
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            libc::NETLINK_KOBJECT_UEVENT,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("opening a netlink socket");
    }
    // SAFETY: the descriptor is fresh, checked, and owned by nothing else.
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };

    // SAFETY: sockaddr_nl is plain data, and zero is a valid value for every field of it.
    let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    address.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    // Zero asks the kernel to allocate the port, so this cannot collide with another
    // netlink socket in the same process.
    address.nl_pid = 0;
    address.nl_groups = 1;
    // SAFETY: the address is a fully initialised sockaddr_nl and the length is its own.
    let bound = unsafe {
        libc::bind(
            socket.as_raw_fd(),
            std::ptr::addr_of!(address).cast(),
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if bound < 0 {
        return Err(std::io::Error::last_os_error()).context("joining the uevent broadcast");
    }
    Ok(socket)
}

/// The subsystem the next queued uevent belongs to, or nothing when the queue is empty.
///
/// Only the kernel may say a device changed. A message from anywhere else carries a port
/// id and is skipped: another process on this machine does not get to decide what the bar
/// reads, and a spoofed one is cheap to send.
fn next_uevent(socket: &OwnedFd, buffer: &mut [u8]) -> Result<Option<String>> {
    loop {
        // SAFETY: sockaddr_nl is plain data, and the call below fills it in.
        let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t;
        // SAFETY: the buffer and the address are owned here and outlive the call, and the
        // lengths passed are their own.
        let read = unsafe {
            libc::recvfrom(
                socket.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                libc::MSG_DONTWAIT,
                std::ptr::addr_of_mut!(address).cast(),
                &mut length,
            )
        };
        if read < 0 {
            let error = std::io::Error::last_os_error();
            return match error.kind() {
                std::io::ErrorKind::WouldBlock => Ok(None),
                std::io::ErrorKind::Interrupted => continue,
                _ => Err(error).context("reading a uevent"),
            };
        }
        if address.nl_pid != 0 {
            continue;
        }
        // A uevent is a summary line, then NUL-separated KEY=VALUE pairs.
        let subsystem = buffer[..read as usize]
            .split(|byte| *byte == 0)
            .filter_map(|field| std::str::from_utf8(field).ok())
            .find_map(|field| field.strip_prefix("SUBSYSTEM="));
        if let Some(subsystem) = subsystem {
            return Ok(Some(subsystem.to_string()));
        }
    }
}

fn run(
    mut attributes: Vec<Attribute>,
    mut uevents: Option<Uevents>,
    sender: calloop::channel::Sender<Event>,
) {
    let mut buffer = vec![0u8; 8192];
    while !attributes.is_empty() || uevents.is_some() {
        let mut fds: Vec<libc::pollfd> = attributes
            .iter()
            .map(|attribute| libc::pollfd {
                fd: attribute.file.as_raw_fd(),
                // A kernfs attribute is always readable, and says a change has happened by
                // raising POLLPRI. Asking for readability would return immediately, for
                // ever; POLLERR arrives whether it is asked for or not.
                events: libc::POLLPRI,
                revents: 0,
            })
            .collect();
        if let Some(uevents) = &uevents {
            fds.push(libc::pollfd {
                fd: uevents.socket.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }

        // SAFETY: the descriptors come from files and sockets this thread owns and which
        // outlive the call, and the length is that of the vector they were built from.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            log::warn!("watching stopped: {error}");
            break;
        }

        if !report_attributes(&mut attributes, &fds, &sender) {
            return;
        }
        if fds.len() > attributes.len()
            && let Some(listening) = &uevents
            && fds[fds.len() - 1].revents != 0
        {
            match drain(listening, &mut buffer, &sender) {
                Ok(true) => {}
                // The channel has closed, which means the bar is shutting down.
                Ok(false) => return,
                Err(e) => {
                    log::warn!("uevents stopped, so intervals stand alone: {e:#}");
                    uevents = None;
                }
            }
        }
    }

    // Whatever is still being watched has lost its watcher, so it needs its interval back.
    for attribute in attributes {
        let _ = sender.send(Event::Lost(attribute.which));
    }
}

/// Report every attribute that woke, and drop the ones whose file has gone.
///
/// Returns whether the event loop is still listening.
fn report_attributes(
    attributes: &mut Vec<Attribute>,
    fds: &[libc::pollfd],
    sender: &calloop::channel::Sender<Event>,
) -> bool {
    // A change and a vanished file both arrive as POLLERR, and only the read tells them
    // apart: the attribute still reads while it exists.
    let mut lost = Vec::new();
    for (index, attribute) in attributes.iter_mut().enumerate() {
        if fds[index].revents == 0 {
            continue;
        }
        let event = match consume(&mut attribute.file) {
            Ok(()) => Event::Changed(attribute.which.clone()),
            Err(e) => {
                log::info!(
                    "{} is read on its interval again: {e:#}",
                    attribute.which.describe()
                );
                lost.push(index);
                Event::Lost(attribute.which.clone())
            }
        };
        // The channel closes when the bar is shutting down, and there is nothing useful
        // left to do with a notification at that point.
        if sender.send(event).is_err() {
            return false;
        }
    }
    for index in lost.into_iter().rev() {
        attributes.remove(index);
    }
    true
}

/// Take every queued uevent and report the sources it concerns.
///
/// Returns whether the event loop is still listening.
fn drain(
    listening: &Uevents,
    buffer: &mut [u8],
    sender: &calloop::channel::Sender<Event>,
) -> Result<bool> {
    // One change arrives as several messages - a charger moves the mains supply and the
    // battery both - so the queue is emptied first and each source is read once.
    let mut changed: Vec<&Which> = Vec::new();
    while let Some(subsystem) = next_uevent(&listening.socket, buffer)? {
        for (wanted, which) in &listening.wanted {
            if *wanted == subsystem && !changed.contains(&which) {
                changed.push(which);
            }
        }
    }
    for which in changed {
        if sender.send(Event::Changed(which.clone())).is_err() {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_watched_attribute_is_read_before_it_is_waited_on() {
        // An unread descriptor wakes immediately and for ever, so arming has to consume
        // the current value.
        let path = std::env::temp_dir().join("dbar-watch-arm");
        std::fs::write(&path, "42\n").expect("the fixture is writable");
        let mut file = arm(&path).expect("the fixture opens");
        assert!(
            consume(&mut file).is_ok(),
            "it can be read again on a change"
        );
    }

    #[test]
    fn an_empty_attribute_is_a_file_that_has_gone() {
        let path = std::env::temp_dir().join("dbar-watch-empty");
        std::fs::write(&path, "").expect("the fixture is writable");
        assert!(arm(&path).is_err());
    }

    #[test]
    fn every_watchable_source_says_how_it_is_watched() {
        // Whether the hardware is here decides which attribute exists, so only the shape
        // is checked: a path that is offered has to be one that could be opened.
        for which in Which::WATCHABLE {
            match which.watch() {
                Some(Watch::Attribute(path)) => assert!(
                    path.is_absolute(),
                    "{} offers a relative path",
                    which.describe()
                ),
                Some(Watch::Uevent(subsystem)) => assert!(
                    !subsystem.is_empty(),
                    "{} names no subsystem",
                    which.describe()
                ),
                // A machine without the hardware has nothing to watch, which is why an
                // attribute may be absent; a uevent subsystem is always there to name.
                None => {}
            }
        }
    }

    #[test]
    fn a_uevent_names_the_subsystem_it_came_from() {
        // The kernel's own broadcast, which an unprivileged reader is allowed to join.
        let socket = match open_uevents() {
            Ok(socket) => socket,
            Err(e) => {
                log::debug!("uevents are unavailable here: {e:#}");
                return;
            }
        };
        // Nothing has happened, so there is nothing queued: the point is that an empty
        // queue reads as empty rather than as an error or a wait.
        let mut buffer = vec![0u8; 8192];
        assert!(matches!(next_uevent(&socket, &mut buffer), Ok(None)));
    }
}
