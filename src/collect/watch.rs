//! Being told, instead of asking.
//!
//! Some of what the bar shows changes only when something changes it, and the kernel will
//! say when that happens: a sysfs attribute that calls `sysfs_notify` wakes `poll()` with
//! `POLLPRI` on every change. A source watched this way needs no interval at all - it is
//! read when the value moves and never in between - which is both faster to react and
//! cheaper than any polling interval could be.
//!
//! The waiting is a blocking `poll()`, so it happens on its own thread and reaches the
//! event loop through a channel, the way the compositor connection and the signal handler
//! already do. One thread waits on every watched file at once, so a second watch costs a
//! file descriptor rather than a thread.

use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::os::fd::AsRawFd as _;
use std::path::Path;

use anyhow::{Context as _, Result, bail};

use super::Which;

/// What the watcher tells the event loop.
pub enum Event {
    /// The kernel reports this source's value has moved.
    Changed(Which),
    /// The file went away, so this source goes back to being read on its interval.
    Lost(Which),
}

struct Watched {
    which: Which,
    file: File,
}

/// Watch every source that has a file behind it, and say which ones are covered.
///
/// A source whose file cannot be opened is not an error: it keeps its interval, which is
/// what it would have had anyway.
pub fn spawn(sender: calloop::channel::Sender<Event>) -> Vec<Which> {
    let watched: Vec<Watched> = super::Which::WATCHABLE
        .iter()
        .filter_map(|which| {
            let path = which.watch_path()?;
            match arm(&path) {
                Ok(file) => {
                    log::info!(
                        "{} is watched at {}, so it is not read on an interval",
                        which.describe(),
                        path.display()
                    );
                    Some(Watched {
                        which: which.clone(),
                        file,
                    })
                }
                Err(e) => {
                    log::debug!("{} is read on its interval: {e:#}", which.describe());
                    None
                }
            }
        })
        .collect();

    if watched.is_empty() {
        return Vec::new();
    }

    let covered: Vec<Which> = watched.iter().map(|w| w.which.clone()).collect();
    match std::thread::Builder::new()
        .name("watch".to_string())
        .spawn(move || run(watched, sender))
    {
        Ok(_) => covered,
        Err(e) => {
            log::warn!("watching sysfs needs a thread and could not have one: {e}");
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

fn run(mut watched: Vec<Watched>, sender: calloop::channel::Sender<Event>) {
    while !watched.is_empty() {
        let mut fds: Vec<libc::pollfd> = watched
            .iter()
            .map(|w| libc::pollfd {
                fd: w.file.as_raw_fd(),
                // A kernfs attribute is always readable, and says a change has happened by
                // raising POLLPRI. Asking for readability would return immediately, for
                // ever; POLLERR arrives whether it is asked for or not.
                events: libc::POLLPRI,
                revents: 0,
            })
            .collect();

        // SAFETY: the descriptors come from files this thread owns and outlive the call,
        // and the length is the length of the vector they were built from.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            log::warn!("watching sysfs stopped: {error}");
            break;
        }

        // A change and a vanished file both arrive as POLLERR, and only the read tells
        // them apart: the attribute still reads while it exists.
        let mut lost = Vec::new();
        for (index, fd) in fds.iter().enumerate() {
            if fd.revents == 0 {
                continue;
            }
            let entry = &mut watched[index];
            let event = match consume(&mut entry.file) {
                Ok(()) => Event::Changed(entry.which.clone()),
                Err(e) => {
                    log::info!(
                        "{} is read on its interval again: {e:#}",
                        entry.which.describe()
                    );
                    lost.push(index);
                    Event::Lost(entry.which.clone())
                }
            };
            // The channel closes when the bar is shutting down, and there is nothing
            // useful left to do with a notification at that point.
            if sender.send(event).is_err() {
                return;
            }
        }
        for index in lost.into_iter().rev() {
            watched.remove(index);
        }
    }

    // Whatever is still being watched has lost its watcher, so it needs its interval back.
    for entry in watched {
        let _ = sender.send(Event::Lost(entry.which));
    }
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
    fn every_watchable_source_names_a_file_or_none_at_all() {
        // Whether the hardware is here decides the answer, so only the shape is checked:
        // a path that is offered has to be one that could be opened.
        for which in Which::WATCHABLE {
            if let Some(path) = which.watch_path() {
                assert!(
                    path.is_absolute(),
                    "{} offers a relative path",
                    which.describe()
                );
            }
        }
    }
}
