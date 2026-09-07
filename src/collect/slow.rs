//! Sources that are read on a thread of their own, because reading them can wait.
//!
//! Everything else a collector reads is a file the kernel fills in on the spot: `/proc`
//! and `/sys` answer or they do not. Two do not. `statvfs` answers when the filesystem
//! answers, which for a network mount whose server has gone or a FUSE daemon that stopped
//! is not a length of time anyone can put a number on, and a wireless link is a netlink
//! request that waits for a driver to reply.
//!
//! Read on the thread that draws, either of those stops the whole bar: no clock, no
//! pointer, no compositor events, until a mount somewhere decides to answer. So they are
//! read here instead, and their readings arrive the way a command's do - through a
//! channel, into the registry that already knows how to hold a reading somebody else
//! took.
//!
//! One thread per source rather than one for all of them, and deliberately: the point is
//! that a source which stalls stalls nothing else, and a shared thread would hand the
//! wireless link a filesystem's problems.

use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;

use super::{Reading, Trigger, Which};

/// A reading from a source read away from the bar's own thread.
pub struct Taken {
    pub which: Which,
    /// What the read came to, since a source that cannot be read has to say so the same
    /// way one read on the timer does.
    pub read: Result<Reading>,
}

/// Read `which` on a thread of its own, every `interval`, and send what it says.
///
/// `askable` says whether anything in the config can ask for a reading early - a signal,
/// or a button. Without it the thread only ever sleeps out its interval, and there is no
/// channel for an ask that cannot come.
///
/// The channel closing is what stops the thread, which is how it ends when the bar does.
pub fn spawn(
    which: Which,
    interval: Duration,
    askable: bool,
    sender: calloop::channel::SyncSender<Taken>,
) -> Option<Trigger> {
    let channel = askable.then(mpsc::channel::<()>);
    let (trigger, asked) = match channel {
        Some((ask, asked)) => (Some(Trigger::new(ask)), Some(asked)),
        None => (None, None),
    };

    let name = which.name().to_string();
    let started = std::thread::Builder::new()
        .name(format!("read:{name}"))
        .spawn(move || {
            let mut collector = which.collector();
            loop {
                let read = collector.read();
                if sender
                    .send(Taken {
                        which: which.clone(),
                        read,
                    })
                    .is_err()
                {
                    return;
                }
                // Asked for early or left to its interval, the same as a command: three
                // impatient clicks want the reading, not three of it.
                match asked.as_ref() {
                    Some(asked) => match asked.recv_timeout(interval) {
                        Ok(()) => while asked.try_recv().is_ok() {},
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        // Nothing is left to ask, so the interval is all there is.
                        Err(mpsc::RecvTimeoutError::Disconnected) => std::thread::sleep(interval),
                    },
                    None => std::thread::sleep(interval),
                }
            }
        });

    match started {
        Ok(_) => trigger,
        Err(e) => {
            // The module is left showing nothing rather than the bar refusing to start,
            // which is what every other source that cannot be read does.
            log::warn!("{name} needs a thread of its own and could not have one: {e}");
            None
        }
    }
}
