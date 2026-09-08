//! Programs dbar starts, and taking them with it when it goes.
//!
//! Two things here run somebody else's program: a command module, and the i3bar provider.
//! Both have the same problem, and it is not the one it looks like. Killing the program
//! dbar spawned is easy; the trouble is what that program started. A script is usually a
//! shell, and a shell that is killed leaves the pipeline it forked running - so `sh -c
//! 'curl ... | jq ...'` outlives the bar, and every restart leaves another one behind.
//!
//! So a program is started in a process group of its own, which makes one signal reach it
//! and everything it forked, and the group is written down where the way out can find it.
//! There are three ways out and all of them go through here: the loop ending, an error on
//! the way out of it, and a signal - which is how a bar is usually stopped, and the one
//! case where nothing of dbar's own would otherwise run at all.

use std::process::{Child, Command};
use std::sync::Once;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use std::os::unix::process::CommandExt as _;

/// How many programs dbar can have running at once.
///
/// A fixed number rather than a list that grows, because the place this most needs to be
/// read from is a signal handler, and a handler may not take a lock. Every module that runs
/// a program runs one at a time, so the config is checked against this when it is read and
/// a bar that could overrun it does not start.
pub const AT_ONCE: usize = 64;

/// The process group of each program running now, or zero for a place nobody is using.
static GROUPS: [AtomicI32; AT_ONCE] = [const { AtomicI32::new(0) }; AT_ONCE];

/// How long a program has to stop on its own terms before it is made to.
///
/// A script that traps the signal to tidy up gets a moment to do it. Anything still there
/// afterwards was not going to leave, and the bar is not going to wait for it.
const GRACE: Duration = Duration::from_millis(100);

/// A program's place in the table, given up when the program is done with.
pub struct Listed(Option<usize>);

impl Drop for Listed {
    fn drop(&mut self) {
        if let Some(at) = self.0 {
            GROUPS[at].store(0, Ordering::Release);
        }
    }
}

/// Start `command` as a program of dbar's own, listed until the `Listed` is dropped.
///
/// Every program dbar runs goes through here, so there is one answer to what happens to it
/// when the bar stops rather than one per caller.
pub fn spawn(command: &mut Command) -> std::io::Result<(Child, Listed)> {
    stop_when_signalled();
    own(command);
    let child = command.spawn()?;
    let listed = list(child.id());
    Ok((child, listed))
}

/// Set a command up to be stoppable, without starting it.
fn own(command: &mut Command) {
    // A group of its own, so stopping the program stops what it forked. Asking for group 0
    // makes the child its own leader, so its group id is its process id and there is
    // nothing to look up later.
    command.process_group(0);

    // And the kernel takes it with us, for the case where nothing else can: a thread
    // blocked reading from a program it started notices nothing when the bar dies.
    //
    // SAFETY: between fork and exec only async-signal-safe calls are allowed, and prctl is
    // one. It is set against the thread that spawned it, which lives as long as the bar.
    unsafe {
        command.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
}

fn list(pid: u32) -> Listed {
    let pid = pid as i32;
    for (at, slot) in GROUPS.iter().enumerate() {
        if slot
            .compare_exchange(0, pid, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Listed(Some(at));
        }
    }
    // The config was checked against `AT_ONCE` when it was read, so this is a bug rather
    // than a configuration; say so, and let the parent-death signal do what it can.
    log::error!("more than {AT_ONCE} programs are running at once; one may outlive the bar");
    Listed(None)
}

/// Stop one program and everything it started.
///
/// The whole group goes, not only the child: `spawn` made the program a group leader, so
/// its group id is its process id.
pub fn stop(pid: u32, signal: i32) {
    signal_group(pid as i32, signal);
}

/// Stop every program dbar is running, and everything each of them started.
///
/// Nothing here but atomics, `kill` and `nanosleep`, all of which a signal handler may use.
pub fn stop_all() {
    let mut any = false;
    for slot in &GROUPS {
        let pid = slot.load(Ordering::Acquire);
        if pid > 0 {
            signal_group(pid, libc::SIGTERM);
            any = true;
        }
    }
    if !any {
        return;
    }
    // Asking is not telling. A program that traps the signal, or ignores it, is still
    // there afterwards - and the bar is about to stop being the thing that could notice.
    pause(GRACE);
    for slot in &GROUPS {
        let pid = slot.swap(0, Ordering::AcqRel);
        if pid > 0 {
            signal_group(pid, libc::SIGKILL);
        }
    }
}

/// Arrange for the running programs to be stopped when the bar is signalled.
///
/// dbar ends when something tells it to: a Ctrl-C, a session shutting down, `pkill dbar`.
/// A program used to share dbar's own process group, so a Ctrl-C reached it and everything
/// it had forked; one in a group of its own has to be told separately, and nothing of
/// dbar's runs at all on the way out of a signal nobody handled.
///
/// `SIGABRT` is in the list because a release build aborts on a panic, and an abort runs no
/// destructor either.
fn stop_when_signalled() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        for number in [
            libc::SIGTERM,
            libc::SIGINT,
            libc::SIGHUP,
            libc::SIGQUIT,
            libc::SIGABRT,
        ] {
            // SAFETY: the handler reads atomics, calls kill and waits, which is all a
            // handler is allowed to do, and then lets the signal do what it would have.
            let installed = unsafe {
                signal_hook::low_level::register(number, move || {
                    stop_all();
                    let _ = signal_hook::low_level::emulate_default_handler(number);
                })
            };
            if let Err(e) = installed {
                log::warn!("programs will outlive a signalled bar: {e}");
            }
        }
    });
}

fn signal_group(pid: i32, signal: i32) {
    if pid <= 0 {
        return;
    }
    // SAFETY: a negative pid names a process group, and this one is a program's own -
    // `own` made it the leader of a group of its own, so the group id is the process id.
    unsafe {
        libc::kill(-pid, signal);
    }
}

/// Wait, without a lock and without the runtime, since a signal handler calls this too.
fn pause(how_long: Duration) {
    let mut left = libc::timespec {
        tv_sec: how_long.as_secs() as libc::time_t,
        tv_nsec: how_long.subsec_nanos() as libc::c_long,
    };
    // A wait cut short by another signal is a shorter grace, which is not worth a loop:
    // what follows it is the signal nothing can decline.
    //
    // SAFETY: both are owned here and live for the length of the call.
    unsafe {
        libc::nanosleep(&left, &mut left);
    }
}

/// Stop everything dbar started, however the bar is leaving.
///
/// The loop can end because it was asked to, and it can end because dispatching failed -
/// the compositor going away is the ordinary case - and a program dbar started must not
/// outlive either. A guard rather than a line after the loop, because only one of those
/// two paths reaches such a line.
pub struct StopEverything;

impl Drop for StopEverything {
    fn drop(&mut self) {
        stop_all();
    }
}

#[cfg(test)]
pub use tests::alone;

#[cfg(test)]
mod tests {
    /// One test at a time for anything that starts a program.
    ///
    /// They share the table of what is running, and `stop_all` empties it, so a test that
    /// stops everything would otherwise stop a program another test was in the middle of.
    pub fn alone() -> std::sync::MutexGuard<'static, ()> {
        static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        ONE.lock().unwrap_or_else(|held| held.into_inner())
    }

    /// The loop can end because it was asked to and it can end because dispatching failed,
    /// and a program dbar started must not outlive either. Only one of those two paths
    /// reaches a line written after the loop, which is why the guard exists.
    #[test]
    fn the_guard_stops_everything_however_the_bar_leaves() {
        let _alone = alone();
        let file = std::env::temp_dir().join(format!("dbar-guard-{}", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let script = format!("sh -c 'echo $$ > {}; exec sleep 30' & wait", file.display());
        let mut command = std::process::Command::new("sh");
        command
            .arg("-c")
            .arg(script)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let (mut child, listed) = super::spawn(&mut command).expect("a shell to run");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let pid = loop {
            if let Ok(text) = std::fs::read_to_string(&file)
                && let Ok(pid) = text.trim().parse::<i32>()
            {
                break pid;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the shell never forked"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let _ = std::fs::remove_file(&file);

        // What an error on the way out of the loop does, which is the path that skipped
        // the stopping altogether when it was a line written after the loop.
        drop(super::StopEverything);

        // SAFETY: signal 0 asks whether the pid exists and sends nothing.
        let gone = |pid| unsafe { libc::kill(pid, 0) } != 0;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !gone(pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "a program outlived the bar's way out"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        drop(listed);
        let _ = child.kill();
        let _ = child.wait();
    }
}
