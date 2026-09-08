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

use std::time::Duration;

use anyhow::Result;

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
