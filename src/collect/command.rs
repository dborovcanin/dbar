//! A module whose readings come from a program of your own.
//!
//! This is dbar's extension mechanism, and it is deliberately not a language. There are
//! two kinds of program worth writing against a bar, and `interval` is how a module says
//! which it has.
//!
//! Without one, the command streams: it is spawned once and its standard output is read a
//! line at a time, so a script that has something to say says it and the bar redraws, and
//! a script with nothing to say costs nothing at all. That is the same deal the volume and
//! the media modules get, and it is the cheapest arrangement there is - no process is
//! started to find out that nothing changed.
//!
//! With one, the command answers: it is run to completion, what it printed becomes the
//! reading, and it is run again when the interval comes round. `interval = "once"` runs it
//! at startup and then only when something asks, for something that changes rarely enough
//! that a schedule is the wrong way to find out. This costs a process each time, which is
//! why it is not the default, but it is what almost every script anybody already has is
//! shaped like.
//!
//! A command that answers can be asked for another one - by a click, or by a signal -
//! through a `Trigger`. The thread waits on that rather than sleeping through the rest of
//! its interval, so an answer that was fetched over the network is a click away instead of
//! a wait away.
//!
//! A command that answers can report on more than one thing at a time. With `pages`, every
//! line of a run is a reading of its own rather than the last one being the answer, so one
//! fetch covers the weather in three cities and the module scrolls between them. Nothing
//! below this file knows the difference: a page is a reading like any other.
//!
//! Either way the running happens on this module's own thread and readings arrive on a
//! channel, so a script that takes a second to answer delays nothing but itself.
//!
//! Nothing here inserts a shell. The command is argv and is executed directly, so a
//! pipeline is something you ask for - `["sh", "-c", "..."]` - rather than something dbar
//! decides to give you.

use std::io::BufReader;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Once, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};

use super::Reading;
use crate::status::{FieldSpec, Fields, Kind, State, Unit, Value};

/// How long to wait before starting a command again after it has stopped.
///
/// A command that exits at once - a typo in its name, a missing interpreter - would
/// otherwise be spawned as fast as the machine can fork. The wait doubles up to a minute,
/// so a script that is merely slow to settle recovers quickly and a broken one is quiet.
const FIRST_WAIT: Duration = Duration::from_secs(1);
const LONGEST_WAIT: Duration = Duration::from_secs(60);

/// What a command publishes when its config declares nothing: the line it printed.
pub const PLAIN: &[FieldSpec] = &[FieldSpec {
    name: "text",
    kind: Kind::Text,
}];

/// How often a command module's program is run, and what running it means.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Run {
    /// Spawned once and left running: a reading for every line it prints.
    Stream,
    /// Run to completion this often, taking what it printed as the reading.
    Every(Duration),
    /// Run to completion at startup, and after that only when something asks.
    Once,
}

/// What a command's thread has to say.
///
/// A run that is on its way is worth saying out loud, because a command that answers can
/// take as long as the network does and a bar that shows nothing meanwhile looks stuck.
/// A streaming command never sends this: it is always waiting, so waiting is not news.
pub enum Message {
    /// A run has begun, and the reading for it has not arrived yet.
    Started,
    Readings(Vec<Reading>),
}

/// Start `argv` and send readings from it, the way `run` says to.
///
/// `askable` says whether anything in the config can ask this command for another
/// reading. Without it a command that answers once is done when it has answered, rather
/// than keeping a thread parked on a question that can never come.
///
/// The channel closes when the bar is shutting down, which is what stops the thread.
pub fn spawn(
    argv: Vec<String>,
    run: Run,
    declared: &'static [FieldSpec],
    sender: calloop::channel::SyncSender<Message>,
    askable: bool,
    pages: bool,
    timeout: Duration,
) -> Result<Option<super::Trigger>> {
    stop_when_signalled();
    let (program, rest) = argv
        .split_first()
        .context("a command module names no command")?;
    let name = program.clone();
    let rest = rest.to_vec();
    // A streaming command is never asked: it says what it has when it has it, and there
    // is no run to bring forward.
    let channel = (askable && run != Run::Stream).then(mpsc::channel::<()>);
    let (trigger, asked) = match channel {
        Some((ask, asked)) => (Some(super::Trigger::new(ask)), Some(asked)),
        None => (None, None),
    };

    std::thread::Builder::new()
        .name(format!("cmd:{name}"))
        .spawn(move || match run {
            Run::Stream => stream_forever(&name, &rest, declared, &sender),
            Run::Once => {
                // One answer, and then nothing until something asks. A failure is
                // reported the same as any other and not retried, because "once" is what
                // the config asked for.
                loop {
                    if sender.send(Message::Started).is_err() {
                        return;
                    }
                    let readings = answer(&name, &rest, declared, pages, timeout);
                    if sender.send(Message::Readings(readings)).is_err() {
                        return;
                    }
                    // Nothing can ask, so there is nothing left to wait for.
                    let Some(asked) = asked.as_ref() else {
                        return;
                    };
                    if asked.recv().is_err() {
                        return;
                    }
                    drain(asked);
                }
            }
            Run::Every(period) => {
                loop {
                    if sender.send(Message::Started).is_err() {
                        return;
                    }
                    let readings = answer(&name, &rest, declared, pages, timeout);
                    if sender.send(Message::Readings(readings)).is_err() {
                        return;
                    }
                    // A command that fails is tried again at its own interval rather than
                    // backed off: a scheduled reading that needs the network is expected
                    // to miss one now and then, and the module says so meanwhile.
                    match asked.as_ref() {
                        Some(asked) => match asked.recv_timeout(period) {
                            Ok(()) => drain(asked),
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                            // Nothing is left to ask, so the interval is all there is.
                            Err(mpsc::RecvTimeoutError::Disconnected) => std::thread::sleep(period),
                        },
                        None => std::thread::sleep(period),
                    }
                }
            }
        })
        .with_context(|| format!("spawning the thread for {argv:?}"))?;
    Ok(trigger)
}

/// Take everything else that was asked for while a run was on its way.
///
/// Three impatient clicks want the reading, not three of it.
fn drain(asked: &mpsc::Receiver<()>) {
    while asked.try_recv().is_ok() {}
}

/// Keep a streaming command running, restarting it when it stops.
fn stream_forever(
    name: &str,
    rest: &[String],
    declared: &'static [FieldSpec],
    sender: &calloop::channel::SyncSender<Message>,
) {
    let mut wait = FIRST_WAIT;
    loop {
        match run_once(name, rest, declared, sender) {
            // The command ended of its own accord, having said whatever it said.
            Ok(()) => log::debug!("{name} ended; starting it again in {wait:?}"),
            Err(e) => {
                log::warn!("{name}: {e:#}");
                if sender.send(Message::Readings(vec![failed(&e)])).is_err() {
                    return;
                }
            }
        }
        std::thread::sleep(wait);
        wait = (wait * 2).min(LONGEST_WAIT);
    }
}

/// Run the command to completion and turn what it printed into readings.
///
/// Ordinarily the last non-empty line is the answer. A script that prints one line means
/// that line; one that prints several has said several things and the most recent is what
/// a bar should be showing. Anything a script wants kept out of this goes to standard
/// error, which is left alone and lands in dbar's log.
///
/// With `pages`, every non-empty line is a reading instead, in the order they were
/// printed. That is the shape of a script asked about several things at once, and it is
/// opt-in because the alternative would make a script that logs its progress into a module
/// with three pages of it.
fn answer(
    name: &str,
    rest: &[String],
    declared: &'static [FieldSpec],
    pages: bool,
    timeout: Duration,
) -> Vec<Reading> {
    match run_to_end(name, rest, timeout) {
        Ok(output) => {
            let readings: Vec<Reading> = match pages {
                true => said(&output)
                    .map(|line| reading_of(line, declared))
                    .collect(),
                false => last_word(&output)
                    .map(|line| reading_of(line, declared))
                    .into_iter()
                    .collect(),
            };
            match readings.is_empty() {
                // It ran, it worked, and it had nothing to say. An empty reading is the
                // honest answer, and a module whose format needs a field it did not get
                // draws nothing rather than something wrong.
                true => vec![Reading {
                    fields: Fields::default(),
                    state: State::Idle,
                }],
                false => readings,
            }
        }
        Err(e) => {
            log::warn!("{name}: {e:#}");
            vec![failed(&e)]
        }
    }
}

/// The lines a command had something on, which is a reading each when a module pages.
fn said(output: &str) -> impl DoubleEndedIterator<Item = &str> {
    output.lines().filter(|line| !line.trim().is_empty())
}

/// The line of a command's output that is its answer: the last one with anything on it.
fn last_word(output: &str) -> Option<&str> {
    said(output).next_back()
}

/// Run the command until it exits, and hand back what it wrote to standard output.
///
/// A command that never finishes is stopped rather than waited on. Without that, a script
/// blocked on a network that will not answer holds this thread forever and leaves the bar
/// showing a spinner - and a spinner is an animation, which is the one thing the bar is
/// not supposed to be doing while nothing is happening.
fn run_to_end(program: &str, args: &[String], timeout: Duration) -> Result<String> {
    let mut child = configured(program, args)
        .spawn()
        .with_context(|| format!("spawning {program}"))?;
    // Listed for as long as it runs, so a bar that is stopped stops it too.
    let _listed = Listed::add(child.id());
    let mut stdout = child.stdout.take().context("stdout was piped")?;

    // One deadline for the whole run, not one for the reading. A command that closes its
    // output and then carries on - `exec 1>&-` and a sleep, or a child of its own holding
    // the descriptor - reaches the end of its output at once and would otherwise be waited
    // on with no limit at all, which is the thing the timeout exists to prevent.
    let deadline = Instant::now() + timeout;
    let ended =
        read_until(&mut stdout, deadline).with_context(|| format!("reading from {program}"))?;
    // Why the read stopped is what matters, and only the read knows it: the clock cannot
    // be asked afterwards, because `poll` may return a hair before the deadline it was
    // given, and that looks exactly like a command that finished.
    let Ended::Output(output) = ended else {
        // `reap` kills and waits, so nothing is left behind.
        reap(&mut child);
        match ended {
            Ended::Flooded => {
                anyhow::bail!("{program} printed more than {OUTPUT_LIMIT} bytes and was stopped")
            }
            _ => anyhow::bail!("{program} had not answered after {timeout:?} and was stopped"),
        }
    };
    let Some(status) = wait_until(&mut child, deadline).context("waiting for the command")? else {
        reap(&mut child);
        anyhow::bail!("{program} had said everything but had not exited after {timeout:?}");
    };
    if !status.success() {
        anyhow::bail!("{program} exited with {status}");
    }
    Ok(output)
}

/// The most one run of a command may print before it is stopped.
///
/// A run's output is held whole, because the answer is the last line of it, so a program
/// stuck in a printing loop would otherwise grow the bar until the machine gave out. A
/// module's worth of readings is a few hundred bytes; a megabyte is a program that has
/// gone wrong.
const OUTPUT_LIMIT: usize = 1024 * 1024;

/// How reading a command's output finished.
enum Ended {
    /// The command closed its output, which is how a run that worked ends.
    Output(String),
    /// The deadline passed first. Whatever had been read by then is dropped: half an
    /// answer is worse than none, because the half would be published as a whole one.
    Overran,
    /// It printed more than `OUTPUT_LIMIT`, and was not going to be believed anyway.
    Flooded,
}

/// Read everything a command writes, giving up at `deadline`.
///
/// The descriptor is waited on rather than read straight through, so a command that says
/// nothing and never exits is noticed instead of blocking the thread it runs on. This is
/// the same `poll` the media and tray threads use to wait on a bus without going to sleep
/// on it forever.
/// The bytes are kept and decoded once at the end rather than read by read. A character
/// outside ASCII is several bytes, a pipe splits where it likes, and decoding half of one
/// turns it into replacement characters that no later read can put back together.
fn read_until(stdout: &mut std::process::ChildStdout, deadline: Instant) -> std::io::Result<Ended> {
    use std::io::Read as _;
    use std::os::fd::AsRawFd as _;

    let fd = stdout.as_raw_fd();
    let mut output: Vec<u8> = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(Ended::Overran);
        }
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one descriptor owned by the caller for the length of this call, and a
        // count that matches.
        let ready =
            unsafe { libc::poll(&mut poll, 1, left.as_millis().min(i32::MAX as u128) as i32) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ready == 0 {
            return Ok(Ended::Overran);
        }
        let read = stdout.read(&mut buffer)?;
        if read == 0 {
            return Ok(Ended::Output(String::from_utf8_lossy(&output).into_owned()));
        }
        if output.len() + read > OUTPUT_LIMIT {
            return Ok(Ended::Flooded);
        }
        output.extend_from_slice(&buffer[..read]);
    }
}

/// Run the command until it stops, sending a reading per line.
fn run_once(
    program: &str,
    args: &[String],
    declared: &'static [FieldSpec],
    sender: &calloop::channel::SyncSender<Message>,
) -> Result<()> {
    let mut child = configured(program, args)
        .spawn()
        .with_context(|| format!("spawning {program}"))?;
    // A streaming command runs for as long as the bar does, which makes it the one most
    // likely to be running when the bar is stopped.
    let _listed = Listed::add(child.id());

    let stdout = child.stdout.take().context("stdout was piped")?;
    for line in crate::lines::capped(BufReader::new(stdout)) {
        let line = match line {
            Ok(line) => line,
            Err(e) => {
                reap(&mut child);
                return Err(e).context("reading a line");
            }
        };
        // A line that had to be cut is not a reading: half of what a program printed is
        // not half an answer, it is a wrong one.
        if line.dropped > 0 {
            log::warn!(
                "{program} printed a line longer than {} bytes; it was ignored",
                crate::lines::LIMIT
            );
            continue;
        }
        if sender
            .send(Message::Readings(vec![reading_of(&line.text, declared)]))
            .is_err()
        {
            // The bar has gone; take the command with it.
            reap(&mut child);
            return Ok(());
        }
    }
    reap(&mut child);
    Ok(())
}

/// How many commands can be running at once and still be stopped by hand.
///
/// A config with more command modules than this is not a thing anybody has; the ones past
/// it keep the parent-death signal, which is what every command had before.
const AT_ONCE: usize = 32;

/// The process group of every command running now, or zero for a place nobody is using.
///
/// A fixed set of atomics rather than a list behind a lock, because the place this most
/// needs to be read from is a signal handler - dbar is ended with a signal far more often
/// than it is ended politely - and a handler may not take a lock.
static GROUPS: [AtomicI32; AT_ONCE] = [const { AtomicI32::new(0) }; AT_ONCE];

/// A command's place in that table, given up when the command is done.
struct Listed(Option<usize>);

impl Listed {
    fn add(pid: u32) -> Listed {
        let pid = pid as i32;
        for (at, slot) in GROUPS.iter().enumerate() {
            if slot
                .compare_exchange(0, pid, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Listed(Some(at));
            }
        }
        log::debug!("more than {AT_ONCE} commands are running; one will outlive the bar");
        Listed(None)
    }
}

impl Drop for Listed {
    fn drop(&mut self) {
        if let Some(at) = self.0 {
            GROUPS[at].store(0, Ordering::Release);
        }
    }
}

/// Stop every command that is running, and everything each of them started.
///
/// The parent-death signal reaches the command itself and nothing it forked, so a script
/// that is a shell leaves its pipeline behind when the bar goes. A command is its own
/// process group, so this is one signal each.
///
/// Nothing here but atomics and `kill`, both of which a signal handler may use.
pub fn stop_all() {
    for slot in &GROUPS {
        let pid = slot.swap(0, Ordering::AcqRel);
        if pid > 0 {
            // SAFETY: a negative pid names a process group, and this one is a command's
            // own - `process_group(0)` made it the leader of a group of its own.
            unsafe {
                libc::kill(-pid, libc::SIGTERM);
            }
        }
    }
}

/// Arrange for the running commands to be stopped when the bar is signalled.
///
/// dbar ends when something tells it to: a Ctrl-C, a session shutting down, `pkill dbar`.
/// A command used to share dbar's own process group, so a Ctrl-C reached it and everything
/// it had forked; one in a group of its own has to be told separately, and nothing at all
/// runs on the way out of a signal that is not handled.
fn stop_when_signalled() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        for number in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            // SAFETY: the handler swaps atomics and calls kill, which is all a handler is
            // allowed to do, and then lets the signal do what it would have done anyway.
            let installed = unsafe {
                signal_hook::low_level::register(number, move || {
                    stop_all();
                    let _ = signal_hook::low_level::emulate_default_handler(number);
                })
            };
            if let Err(e) = installed {
                log::warn!("commands will outlive a signalled bar: {e}");
            }
        }
    });
}

/// A command set up the way both ways of running one need it.
fn configured(program: &str, args: &[String]) -> Command {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Left alone, so whatever the command complains about lands in dbar's own log
        // rather than disappearing.
        .stderr(Stdio::inherit());

    // A group of its own, so stopping the command stops everything it started. A script is
    // usually a shell, and killing a shell leaves the pipeline it forked running: `sh -c
    // 'curl ... | jq ...'` past its deadline would otherwise leave the curl behind, once
    // per interval, for ever. Asking for group 0 makes the child its own leader, so its
    // group id is its process id and there is nothing to look up later.
    command.process_group(0);

    // Ask the kernel to take the command with us. Without this a command outlives the bar
    // that started it: the reader thread is blocked in a read that a signal to dbar never
    // reaches, so nothing is left to notice and kill it, and every restart of the bar
    // leaves another one behind.
    //
    // SAFETY: between fork and exec only async-signal-safe calls are allowed, and prctl is
    // one. It is set against the thread that spawned it, which is this command's own
    // thread, and that thread lives as long as the bar does.
    unsafe {
        command.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    command
}

/// Stop waiting on a command that has closed its output.
/// Wait for a command to exit, giving up at `deadline`.
///
/// `Child::wait` has no deadline of its own, so the wait is done in short steps: a command
/// that has closed its output has usually already exited, and the step is small enough
/// that the ordinary case costs one poll and long enough that a stuck one is not spun on.
fn wait_until(child: &mut Child, deadline: Instant) -> std::io::Result<Option<ExitStatus>> {
    const STEP: Duration = Duration::from_millis(5);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        std::thread::sleep(STEP.min(left));
    }
}

/// Stop a command and everything it started, and collect what is left.
///
/// The whole process group goes, not only the child: `configured` gave the command a group
/// of its own, so its group id is its process id and one signal reaches every descendant
/// that has not left the group of its own accord.
fn reap(child: &mut Child) {
    let pid = child.id() as libc::pid_t;
    // SAFETY: a negative pid names a process group, and this one is the child's own -
    // `process_group(0)` made it the leader, and a spawn where that failed never got here.
    // The child has not been waited on yet, so the id has not been reused.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// What the bar shows when the command could not be run at all.
fn failed(error: &anyhow::Error) -> Reading {
    let mut fields = Fields::default();
    fields.set("text", Value::Text(format!("{error}")));
    fields.set_primary("text");
    Reading {
        fields,
        state: State::Error,
    }
}

/// One line of output, as a reading.
///
/// A line with no `=` in it is the whole of what the command had to say, and lands in
/// `text`. Otherwise the line is tab-separated `key=value` pairs, because a tab is the one
/// character a value is unlikely to contain and every language can print one.
///
/// Only fields the config declared are taken. A command is somebody else's program and may
/// print anything; a bar that grew a field for whatever turned up would have no way to
/// check a format against it, and no way to stop a runaway one growing for ever.
fn reading_of(line: &str, declared: &'static [FieldSpec]) -> Reading {
    let mut fields = Fields::default();
    let mut state = State::Idle;

    if !line.contains('=') {
        if let Some(spec) = declared.iter().find(|f| f.name == "text") {
            fields.set(spec.name, Value::Text(line.to_string()));
            fields.set_primary(spec.name);
        }
        return Reading { fields, state };
    }

    let mut primary = None;
    for pair in line.split('\t') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let key = key.trim();
        // `state` is the one key dbar reads rather than draws: it is how a command rates
        // what it is reporting, the same as a native collector rating its own.
        if key == "state" {
            state = state_of(value).unwrap_or(State::Idle);
            continue;
        }
        let Some(spec) = declared.iter().find(|f| f.name == key) else {
            log::debug!("a command published {key:?}, which its module does not declare");
            continue;
        };
        let parsed = value_of(value, spec.kind);
        // The first number a command publishes is what `above` and `below` compare
        // against when a rule names no field, which is the rule the native collectors
        // follow too.
        if primary.is_none() && matches!(parsed, Value::Num { .. }) {
            primary = Some(spec.name);
        }
        fields.set(spec.name, parsed);
    }
    if let Some(name) = primary.or(declared.first().map(|f| f.name)) {
        fields.set_primary(name);
    }
    Reading { fields, state }
}

/// A value as the kind of thing the config said it would be.
///
/// A field declared as a number that arrives as a word is `Absent` rather than text: the
/// format and any threshold were written against a number, and drawing the word there
/// would satisfy neither.
fn value_of(value: &str, kind: Kind) -> Value {
    let value = value.trim();
    if value.is_empty() {
        return Value::Absent;
    }
    match kind {
        Kind::Text => Value::Text(value.to_string()),
        Kind::Num(unit) => {
            // A trailing percent sign is allowed on a percentage, since that is how a
            // script most naturally prints one.
            let number = match unit {
                Unit::Percent => value.strip_suffix('%').unwrap_or(value),
                _ => value,
            };
            match number.trim().parse::<f64>() {
                Ok(v) => Value::Num { v, unit },
                Err(_) => Value::Absent,
            }
        }
        // Nothing declares these yet; a command that wants a time can publish a number.
        _ => Value::Text(value.to_string()),
    }
}

/// How a command rates what it is reporting.
fn state_of(word: &str) -> Option<State> {
    match word.trim().to_ascii_lowercase().as_str() {
        "good" | "ok" => Some(State::Good),
        "warning" | "warn" => Some(State::Warning),
        "critical" | "crit" => Some(State::Critical),
        "error" => Some(State::Error),
        "idle" | "" => Some(State::Idle),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    /// One command-spawning test at a time.
    ///
    /// They share the table of what is running, and `stop_all` empties it, so a test that
    /// stops everything would otherwise stop a command another test was in the middle of.
    fn alone() -> std::sync::MutexGuard<'static, ()> {
        static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        ONE.lock().unwrap_or_else(|held| held.into_inner())
    }

    /// A script that never finishes is stopped rather than waited on. Without this the
    /// thread is held forever and the module sits in front of a spinner - and a spinner
    /// that never stops is an animation running while nothing is happening, which is the
    /// one thing this bar is not supposed to do.
    #[test]
    fn a_command_that_never_answers_is_stopped() {
        let _alone = alone();
        let started = std::time::Instant::now();
        let result = super::run_to_end(
            "sh",
            &["-c".to_string(), "sleep 30".to_string()],
            std::time::Duration::from_millis(300),
        );
        let e = result.expect_err("a command that outlives its deadline is a failure");
        let waited = started.elapsed();
        assert!(
            waited < std::time::Duration::from_secs(5),
            "waited {waited:?}, so the deadline did not apply"
        );
        let message = format!("{e:#}");
        assert!(message.contains("had not answered"), "{message}");
    }

    /// Closing standard output is not the same as exiting, and used to be treated as if
    /// it were: the read ended at once and the wait that followed had no deadline of its
    /// own, so a command that closed its output and then hung held the thread anyway.
    #[test]
    fn a_command_that_closes_its_output_and_stays_is_stopped() {
        let _alone = alone();
        let started = std::time::Instant::now();
        let result = super::run_to_end(
            "sh",
            &["-c".to_string(), "exec 1>&-; sleep 30".to_string()],
            std::time::Duration::from_millis(200),
        );
        let e = result.expect_err("a command that will not exit is a failure");
        let waited = started.elapsed();
        assert!(
            waited < std::time::Duration::from_secs(5),
            "waited {waited:?}, so the deadline did not cover the exit"
        );
        assert!(format!("{e:#}").contains("had not exited"), "{e:#}");
    }

    /// A command is stopped along with whatever it started. A script is usually a shell,
    /// and a shell that is killed leaves its pipeline running: one of those per interval
    /// accumulates until the machine notices.
    #[test]
    fn a_command_that_outlives_its_deadline_takes_its_children_with_it() {
        let _alone = alone();
        let file = std::env::temp_dir().join(format!("dbar-reap-{}", std::process::id()));
        let _ = std::fs::remove_file(&file);
        // The grandchild writes its own pid down and then becomes the sleep, so the pid
        // stays that of a process still running when the deadline passes.
        let script = format!("sh -c 'echo $$ > {}; exec sleep 30' & wait", file.display());
        let result = super::run_to_end(
            "sh",
            &["-c".to_string(), script],
            std::time::Duration::from_millis(300),
        );
        let e = result.expect_err("a command that outlives its deadline is a failure");
        assert!(format!("{e:#}").contains("had not answered"), "{e:#}");

        let pid: i32 = std::fs::read_to_string(&file)
            .expect("the grandchild wrote its pid")
            .trim()
            .parse()
            .expect("a pid is a number");
        let _ = std::fs::remove_file(&file);

        // Killed is not the same as gone: whoever adopts the grandchild reaps it a moment
        // later, so the answer is waited for rather than taken at once.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            // SAFETY: signal 0 asks whether the pid exists and sends nothing.
            let alive = unsafe { libc::kill(pid, 0) } == 0;
            if !alive {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the command's grandchild outlived it"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// The bar is stopped far more often than it stops itself, and the parent-death signal
    /// reaches the command alone - not the pipeline a shell forked from it. Those used to
    /// be reached because the command shared dbar's process group; a command in a group of
    /// its own has to be stopped on the way out.
    #[test]
    fn stopping_the_bar_stops_what_a_command_started() {
        let _alone = alone();
        let file = std::env::temp_dir().join(format!("dbar-stop-{}", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let script = format!("sh -c 'echo $$ > {}; exec sleep 30' & wait", file.display());
        let mut child = super::configured("sh", &["-c".to_string(), script])
            .spawn()
            .expect("a shell to run");
        let listed = super::Listed::add(child.id());

        // The grandchild says who it is, then becomes the sleep, so the pid stays that of
        // something still running when the bar goes.
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

        super::stop_all();
        // SAFETY: signal 0 asks whether the pid exists and sends nothing.
        let gone = |pid| unsafe { libc::kill(pid, 0) } != 0;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !gone(pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "what the command started outlived the bar"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        drop(listed);
        let _ = child.kill();
        let _ = child.wait();
    }

    /// Output arrives in whatever pieces the pipe felt like, and a character outside ASCII
    /// is several bytes. Decoding each read on its own turns one split character into two
    /// replacement characters, which no amount of reading afterwards can repair.
    #[test]
    fn a_character_split_across_two_reads_survives() {
        let _alone = alone();
        let output = super::run_to_end(
            "sh",
            &[
                "-c".to_string(),
                // The two bytes of "é", written either side of a pause.
                "printf '\\303'; sleep 0.1; printf '\\251\\n'".to_string(),
            ],
            std::time::Duration::from_secs(10),
        )
        .expect("a command that prints in pieces");
        assert_eq!(super::last_word(&output), Some("\u{e9}"));
    }

    /// The deadline must not cut short a command that answers in time, however slowly it
    /// gets around to it.
    #[test]
    fn a_command_that_answers_in_time_is_left_alone() {
        let _alone = alone();
        let output = super::run_to_end(
            "sh",
            &["-c".to_string(), "sleep 0.1; echo 42".to_string()],
            std::time::Duration::from_secs(10),
        )
        .expect("a command well inside its deadline");
        assert_eq!(super::last_word(&output), Some("42"));
    }

    /// Output that arrives in pieces over time is still all of it: the read waits for the
    /// command to finish rather than taking whatever the first read happened to catch.
    #[test]
    fn output_written_in_pieces_is_read_whole() {
        let _alone = alone();
        let output = super::run_to_end(
            "sh",
            &[
                "-c".to_string(),
                "echo one; sleep 0.1; echo two; sleep 0.1; echo three".to_string(),
            ],
            std::time::Duration::from_secs(10),
        )
        .expect("a command that dawdles but finishes");
        let lines: Vec<&str> = super::said(&output).collect();
        assert_eq!(lines, ["one", "two", "three"]);
    }

    #[test]
    fn a_scripts_answer_is_the_last_line_with_anything_on_it() {
        assert_eq!(last_word("42\n"), Some("42"));
        // Trailing blank lines are how a shell script ends, not something it said.
        assert_eq!(last_word("42\n\n\n"), Some("42"));
        // A script that says several things has most recently said the last of them.
        assert_eq!(last_word("starting\nworking\ndone\n"), Some("done"));
        assert_eq!(
            last_word("only line, no newline"),
            Some("only line, no newline")
        );
    }

    /// What a paging module gets: one reading per line that had anything on it, in the
    /// order they were printed, so three cities come back in the order they were asked
    /// about.
    #[test]
    fn a_paging_command_says_one_thing_per_line() {
        let out = "temp=1\n\ntemp=2\ntemp=3\n";
        let lines: Vec<&str> = said(out).collect();
        assert_eq!(lines, ["temp=1", "temp=2", "temp=3"]);
    }

    #[test]
    fn a_script_that_printed_nothing_has_nothing_to_say() {
        assert_eq!(last_word(""), None);
        assert_eq!(last_word("\n \n\t\n"), None);
    }
    use super::*;

    /// What a module in a config would declare for these tests.
    const DECLARED: &[FieldSpec] = &[
        FieldSpec {
            name: "text",
            kind: Kind::Text,
        },
        FieldSpec {
            name: "count",
            kind: Kind::Num(Unit::None),
        },
        FieldSpec {
            name: "used",
            kind: Kind::Num(Unit::Percent),
        },
        FieldSpec {
            name: "name",
            kind: Kind::Text,
        },
        FieldSpec {
            name: "title",
            kind: Kind::Text,
        },
        FieldSpec {
            name: "by",
            kind: Kind::Text,
        },
        FieldSpec {
            name: "artist",
            kind: Kind::Text,
        },
    ];

    fn read(line: &str) -> Reading {
        reading_of(line, DECLARED)
    }

    fn text_of(reading: &Reading, name: &str) -> Option<String> {
        match reading.fields.get(name) {
            Some(Value::Text(t)) => Some(t.clone()),
            _ => None,
        }
    }

    fn num_of(reading: &Reading, name: &str) -> Option<f64> {
        reading.fields.get(name).and_then(|v| v.num())
    }

    /// The simplest thing a command can be is one that prints a line, so a line that says
    /// nothing about fields is the line itself.
    #[test]
    fn a_plain_line_is_the_whole_of_what_was_said() {
        let r = read("3 updates");
        assert_eq!(text_of(&r, "text").as_deref(), Some("3 updates"));
        assert_eq!(r.state, State::Idle);
    }

    #[test]
    fn pairs_become_the_fields_they_name() {
        let r = read("count=3\tname=deploy");
        assert_eq!(num_of(&r, "count"), Some(3.0));
        assert_eq!(text_of(&r, "name").as_deref(), Some("deploy"));
    }

    /// A value with a space in it is ordinary; a tab is what separates one field from the
    /// next, which is why it is the separator.
    #[test]
    fn a_value_may_have_spaces_in_it() {
        let r = read("title=all along the watchtower\tby=hendrix");
        assert_eq!(
            text_of(&r, "title").as_deref(),
            Some("all along the watchtower")
        );
        assert_eq!(text_of(&r, "by").as_deref(), Some("hendrix"));
    }

    /// A percentage is a percentage, so `above` and `below` compare against it and a
    /// format can render it as one.
    #[test]
    fn a_trailing_percent_makes_it_a_percentage() {
        let r = read("used=42%");
        assert!(matches!(
            r.fields.get("used"),
            Some(Value::Num {
                unit: Unit::Percent,
                ..
            })
        ));
        assert_eq!(num_of(&r, "used"), Some(42.0));
    }

    /// An empty value is a field the command knows about but cannot supply, which is what
    /// makes a conditional part of a format disappear rather than draw a gap.
    #[test]
    fn an_empty_value_is_absent_rather_than_blank() {
        let r = read("artist=\ttitle=silence");
        assert!(matches!(r.fields.get("artist"), Some(Value::Absent)));
        assert_eq!(text_of(&r, "title").as_deref(), Some("silence"));
    }

    /// `state` is read rather than drawn: it is how the command rates itself, and it is
    /// what a `state = "warning"` rule matches on.
    #[test]
    fn state_is_taken_and_not_shown() {
        let r = read("state=warning\tcount=9");
        assert_eq!(r.state, State::Warning);
        assert!(r.fields.get("state").is_none());
        assert_eq!(num_of(&r, "count"), Some(9.0));
    }

    #[test]
    fn a_state_nobody_recognises_is_not_a_state() {
        assert_eq!(state_of("sideways"), None);
        assert_eq!(read("state=sideways").state, State::Idle);
    }

    /// The first number is what an unqualified `above` or `below` reads, so a rule that
    /// names no field still has something to compare.
    #[test]
    fn the_first_number_is_what_a_bound_compares_against() {
        let r = read("name=deploy\tcount=7");
        assert_eq!(r.fields.primary().and_then(|v| v.num()), Some(7.0));
    }

    /// A command may print anything; a module takes only what it said it would, so a
    /// format can be checked against the declaration at startup.
    #[test]
    fn a_field_the_module_never_declared_is_not_taken() {
        let r = read("count=2\tsurprise=17");
        assert_eq!(num_of(&r, "count"), Some(2.0));
        assert!(r.fields.get("surprise").is_none());
    }

    /// A number that arrives as a word is missing rather than shown: the format and any
    /// threshold were written against a number.
    #[test]
    fn a_number_that_is_not_one_is_absent() {
        let r = read("count=lots");
        assert!(matches!(r.fields.get("count"), Some(Value::Absent)));
    }
}
