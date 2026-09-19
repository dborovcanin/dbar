//! Sources that are read on a thread of their own, because reading them can wait.
//!
//! `statvfs` answers when the filesystem answers, which for a network mount whose server
//! has gone or a FUSE daemon that stopped is not a length of time anyone can put a number
//! on. A wireless link waits for its driver, and a sysfs read may invoke a battery,
//! embedded-controller or storage driver's hardware operation rather than returning a
//! cached value.
//!
//! Read on the thread that draws, any of those stops the whole bar: no clock, no
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
/// `askable` says whether the config can ask for a reading early through a signal or a
/// button. A source with a kernel watcher is askable independently of the config. Without
/// either, the thread only ever sleeps out its interval and keeps no trigger.
///
/// The channel closing is what stops the thread, which is how it ends when the bar does.
pub fn spawn(
    which: Which,
    interval: Duration,
    askable: bool,
    sender: calloop::channel::SyncSender<Taken>,
) -> Option<Trigger> {
    let channel = needs_trigger(&which, askable).then(|| mpsc::sync_channel::<()>(1));
    let (trigger, asked) = match channel {
        Some((ask, asked)) => (Some(Trigger::new(ask)), Some(asked)),
        None => (None, None),
    };

    let name = which.name().to_string();
    let started = std::thread::Builder::new()
        .name(format!("read:{name}"))
        .spawn(move || {
            let mut collector = which.collector();
            let mut failures = 0u32;
            loop {
                let read = collector.read();
                failures = match read.is_ok() {
                    true => 0,
                    false => failures.saturating_add(1),
                };
                if sender
                    .send(Taken {
                        which: which.clone(),
                        read,
                    })
                    .is_err()
                {
                    return;
                }
                // The same backoff a source read on the shared timer gets: a mount that
                // has gone or an interface that was unplugged is asked less and less often
                // rather than at its full rate forever. Reading here rather than there
                // changed which thread does the work, and nothing else.
                let wait = super::backoff(interval, failures);
                if failures > 0 {
                    log::debug!(
                        "{} has failed {failures} times; reading it again in {wait:?}",
                        which.describe()
                    );
                }
                // Asked for early or left to its interval, the same as a command: three
                // impatient clicks want the reading, not three of it.
                match asked.as_ref() {
                    Some(asked) => match asked.recv_timeout(wait) {
                        Ok(()) => while asked.try_recv().is_ok() {},
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        // Nothing is left to ask, so the interval is all there is.
                        Err(mpsc::RecvTimeoutError::Disconnected) => std::thread::sleep(wait),
                    },
                    None => std::thread::sleep(wait),
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

/// Whether this worker needs an early-refresh channel.
///
/// Kernel notifications are requests just like configured buttons and signals. Keeping
/// the rule beside worker construction prevents a watchable blocking source from being
/// started without the trigger its watcher needs.
fn needs_trigger(which: &Which, configured: bool) -> bool {
    configured || Which::WATCHABLE.contains(which)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_watchable_worker_keeps_a_trigger_for_kernel_notifications() {
        let workers: Vec<&Which> = Which::WATCHABLE
            .iter()
            .filter(|which| which.blocking())
            .collect();
        assert!(!workers.is_empty(), "the assertion must cover a worker");
        for which in workers {
            assert!(
                needs_trigger(which, false),
                "{} has no trigger",
                which.name()
            );
        }
    }
}
