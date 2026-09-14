//! What a thread does when the thing it was listening to goes away.
//!
//! Three of the bar's sources are somebody else's service: PipeWire for the volume, and the
//! session bus for what is playing and for the tray. None of them is guaranteed to be there
//! when dbar starts - a bar usually starts with the session, which is a race it can lose -
//! and any of them can leave while the bar runs.
//!
//! Giving up on the first refusal is what made a volume module stay empty for a whole
//! session because PipeWire was a second late. So none of them gives up: the connection is
//! tried again, waiting longer each time, and the module shows nothing meanwhile rather
//! than the last thing it saw. A reading from a service that is no longer there is not a
//! reading.
//!
//! The waiting doubles because the two cases want opposite things. A service arriving a
//! moment after the bar should be picked up a moment later; one that is never coming should
//! be asked about once a minute, not once a second, for as long as the machine is on.

use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::time::Duration;

use anyhow::Result;

/// A command pipe that never waits for room. The reader still sleeps in `poll()` until
/// data arrives; nonblocking I/O only changes what happens when the pipe is full or empty.
pub fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut ends = [0; 2];
    // SAFETY: the array holds exactly the two descriptors pipe2 writes.
    if unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just created and have no other owner.
    Ok(unsafe { (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) })
}

/// Send one command byte, or report that the reader is not keeping up. Only an interrupted
/// syscall is retried; a full pipe is never polled or waited on here.
pub fn send_byte(pipe: &OwnedFd, byte: u8) -> std::io::Result<()> {
    loop {
        // SAFETY: the descriptor and the single byte remain valid for the call.
        let written = unsafe { libc::write(pipe.as_raw_fd(), (&byte as *const u8).cast(), 1) };
        if written == 1 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Read the bytes currently in a command pipe without ever waiting for more.
///
/// Interrupted syscalls are retried here so every worker gets the same policy. An empty
/// pipe still returns `WouldBlock`, and zero still means every writer has gone away.
pub fn read_bytes(pipe: &OwnedFd, buffer: &mut [u8]) -> std::io::Result<usize> {
    loop {
        // SAFETY: the descriptor and the writable buffer remain valid for the call.
        let read =
            unsafe { libc::read(pipe.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
        if read >= 0 {
            return Ok(read as usize);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Fill a command pipe without letting a regression turn a test into a blocking write.
#[cfg(test)]
pub(crate) fn fill_pipe(pipe: &OwnedFd) -> usize {
    // SAFETY: F_GETFL inspects this owned descriptor without modifying it.
    let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0 && flags & libc::O_NONBLOCK != 0);
    let mut sent = 0;
    loop {
        match send_byte(pipe, 0) {
            Ok(()) => sent += 1,
            Err(e) => {
                assert_eq!(e.kind(), std::io::ErrorKind::WouldBlock);
                return sent;
            }
        }
    }
}

/// How long to wait before trying again, and the longest that wait becomes.
const FIRST_WAIT: Duration = Duration::from_secs(1);
const LONGEST_WAIT: Duration = Duration::from_secs(60);

/// Run `work` for as long as the bar wants it, connecting again whenever it stops.
///
/// `what` names the service in the log. `work` connects and stays connected, returning when
/// the service has gone. `empty` publishes whatever "there is nothing to show" means for
/// this source, and says whether the bar is still listening - it is the one thing that runs
/// on every way out, so it is also how the thread learns the bar has gone and there is
/// nothing left to reconnect for.
pub fn forever(what: &str, mut work: impl FnMut() -> Result<()>, mut empty: impl FnMut() -> bool) {
    let mut wait = FIRST_WAIT;
    loop {
        match work() {
            // It was there and now it is not, which is worth trying again at once: a
            // service that was restarted is usually back within the second.
            Ok(()) => {
                log::info!("{what} has gone; waiting for it to come back");
                wait = FIRST_WAIT;
            }
            Err(e) => log::warn!("{what} is unavailable: {e:#}"),
        }
        if !empty() {
            return;
        }
        std::thread::sleep(wait);
        wait = (wait * 2).min(LONGEST_WAIT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_pipe_is_nonblocking_and_reports_when_its_writer_has_gone() {
        let (read, write) = pipe().unwrap();
        let mut bytes = [0; 2];
        assert_eq!(
            read_bytes(&read, &mut bytes).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );

        send_byte(&write, 7).unwrap();
        assert_eq!(read_bytes(&read, &mut bytes).unwrap(), 1);
        assert_eq!(bytes[0], 7);

        drop(write);
        assert_eq!(read_bytes(&read, &mut bytes).unwrap(), 0);
    }
}
