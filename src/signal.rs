//! Realtime signals for refreshing sources, and child exits for reaping click commands.
//!
//! A collector on a five-second interval is fine until something changes the thing it is
//! measuring: after `brightnessctl set +10%` the bar should say so now, not in four
//! seconds. `signal = 8` on a module means SIGRTMIN+8 reads its source again.
//!
//! The offsets are counted from SIGRTMIN rather than written as absolute numbers, because
//! where the realtime range starts is decided by the C library - the first few are reserved
//! for the threading implementation - so an absolute number is not portable even between
//! two Linux machines.

use anyhow::{Context as _, Result};

#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    Refresh(i32),
    ChildExited,
}

/// Stop the sleeping signal thread when the event loop is leaving.
pub struct Watching(signal_hook::iterator::Handle);

impl Drop for Watching {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// Watch the signals the config needs. Child exits share the existing listener and are
/// enabled even without realtime signals when a click can launch a program.
///
/// The waiting happens on its own thread, which is what the signal crate wants, and the
/// offsets arrive on the event loop the same way the compositor's events already do.
pub fn spawn(
    offsets: &[i32],
    children: bool,
    sender: calloop::channel::Sender<Event>,
) -> Result<Option<Watching>> {
    if offsets.is_empty() && !children {
        return Ok(None);
    }

    let base = libc::SIGRTMIN();
    let mut numbers: Vec<i32> = offsets.iter().map(|offset| base + offset).collect();
    if children {
        numbers.push(libc::SIGCHLD);
    }
    let mut signals =
        signal_hook::iterator::Signals::new(&numbers).context("watching for signals")?;
    let watching = Watching(signals.handle());

    std::thread::Builder::new()
        .name("signals".to_string())
        .spawn(move || {
            for number in &mut signals {
                // The channel closes when the bar is shutting down, and there is nothing
                // useful left to do with a signal at that point.
                let event = match number {
                    libc::SIGCHLD => Event::ChildExited,
                    _ => Event::Refresh(number - base),
                };
                if sender.send(event).is_err() {
                    return;
                }
            }
        })
        .context("spawning the signal thread")?;

    if !offsets.is_empty() {
        log::info!(
            "watching SIGRTMIN+{}",
            offsets
                .iter()
                .map(|o| o.to_string())
                .collect::<Vec<_>>()
                .join(", SIGRTMIN+")
        );
    }
    Ok(Some(watching))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    #[test]
    fn child_exits_reap_owned_handles_without_a_collector_timer() {
        let _alone = crate::proc::alone();
        let mut event_loop = calloop::EventLoop::<Vec<Child>>::try_new().unwrap();
        let (sender, receiver) = calloop::channel::channel();
        let _watching = spawn(&[], true, sender).unwrap().unwrap();
        event_loop
            .handle()
            .insert_source(receiver, |event, _, children| {
                if let calloop::channel::Event::Msg(Event::ChildExited) = event {
                    crate::proc::reap(children);
                }
            })
            .unwrap();

        // This status belongs to a provider/command worker and must remain waitable.
        let mut unrelated = Command::new("sh").args(["-c", "exit 23"]).spawn().unwrap();
        let running = Command::new("sh")
            .args(["-c", "read line"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let mut children = vec![running];
        let mut exited = Vec::new();
        for _ in 0..3 {
            let child = Command::new("true").spawn().unwrap();
            exited.push(child.id());
            children.push(child);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while children.len() > 1 {
            assert!(
                Instant::now() < deadline,
                "child exits did not wake the event loop"
            );
            event_loop
                .dispatch(Some(Duration::from_millis(100)), &mut children)
                .unwrap();
        }
        assert!(children[0].try_wait().unwrap().is_none());
        assert_eq!(unrelated.wait().unwrap().code(), Some(23));
        for pid in exited {
            let mut status = 0;
            // SAFETY: these are the test's children; WNOHANG cannot wait for anything.
            assert_eq!(
                unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
        }

        // Closing stdin lets the last child exit and wake a loop with no other sources.
        drop(children[0].stdin.take());
        while !children.is_empty() {
            assert!(Instant::now() < deadline, "the final child was not reaped");
            event_loop
                .dispatch(Some(Duration::from_millis(100)), &mut children)
                .unwrap();
        }
    }

    #[test]
    fn no_requested_signals_need_no_listener() {
        let (sender, _receiver) = calloop::channel::channel();
        assert!(spawn(&[], false, sender).unwrap().is_none());
    }
}
