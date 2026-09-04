//! Realtime signals, for refreshing a source on demand.
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

/// Watch for the signals a config asks for, and forward each as its offset from SIGRTMIN.
///
/// The waiting happens on its own thread, which is what the signal crate wants, and the
/// offsets arrive on the event loop the same way the compositor's events already do.
pub fn spawn(offsets: &[i32], sender: calloop::channel::Sender<i32>) -> Result<()> {
    if offsets.is_empty() {
        return Ok(());
    }

    let base = libc::SIGRTMIN();
    let numbers: Vec<i32> = offsets.iter().map(|offset| base + offset).collect();
    let mut signals =
        signal_hook::iterator::Signals::new(&numbers).context("watching for realtime signals")?;

    std::thread::Builder::new()
        .name("signals".to_string())
        .spawn(move || {
            for number in &mut signals {
                // The channel closes when the bar is shutting down, and there is nothing
                // useful left to do with a signal at that point.
                if sender.send(number - base).is_err() {
                    return;
                }
            }
        })
        .context("spawning the signal thread")?;

    log::info!(
        "watching SIGRTMIN+{}",
        offsets
            .iter()
            .map(|o| o.to_string())
            .collect::<Vec<_>>()
            .join(", SIGRTMIN+")
    );
    Ok(())
}
