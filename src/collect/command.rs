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
//! at startup and never again, for something that cannot change while the bar is up. This
//! costs a process each time, which is why it is not the default, but it is what almost
//! every script anybody already has is shaped like.
//!
//! Either way the running happens on this module's own thread and readings arrive on a
//! channel, so a script that takes a second to answer delays nothing but itself.
//!
//! Nothing here inserts a shell. The command is argv and is executed directly, so a
//! pipeline is something you ask for - `["sh", "-c", "..."]` - rather than something dbar
//! decides to give you.

use std::io::{BufRead as _, BufReader};
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

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
    /// Run to completion at startup and never again.
    Once,
}

/// Start `argv` and send readings from it, the way `run` says to.
///
/// The channel closes when the bar is shutting down, which is what stops the thread.
pub fn spawn(
    argv: Vec<String>,
    run: Run,
    declared: &'static [FieldSpec],
    sender: calloop::channel::Sender<Reading>,
) -> Result<()> {
    let (program, rest) = argv
        .split_first()
        .context("a command module names no command")?;
    let name = program.clone();
    let rest = rest.to_vec();

    std::thread::Builder::new()
        .name(format!("cmd:{name}"))
        .spawn(move || match run {
            Run::Stream => stream_forever(&name, &rest, declared, &sender),
            Run::Once => {
                // One answer, and the thread is done: there is nothing left to wait for.
                // A failure is reported the same as any other and not retried, because
                // "once" is what the config asked for.
                let _ = sender.send(answer(&name, &rest, declared));
            }
            Run::Every(period) => {
                loop {
                    if sender.send(answer(&name, &rest, declared)).is_err() {
                        return;
                    }
                    // A command that fails is tried again at its own interval rather than
                    // backed off: a scheduled reading that needs the network is expected
                    // to miss one now and then, and the module says so meanwhile.
                    std::thread::sleep(period);
                }
            }
        })
        .with_context(|| format!("spawning the thread for {argv:?}"))?;
    Ok(())
}

/// Keep a streaming command running, restarting it when it stops.
fn stream_forever(
    name: &str,
    rest: &[String],
    declared: &'static [FieldSpec],
    sender: &calloop::channel::Sender<Reading>,
) {
    let mut wait = FIRST_WAIT;
    loop {
        match run_once(name, rest, declared, sender) {
            // The command ended of its own accord, having said whatever it said.
            Ok(()) => log::debug!("{name} ended; starting it again in {wait:?}"),
            Err(e) => {
                log::warn!("{name}: {e:#}");
                if sender.send(failed(&e)).is_err() {
                    return;
                }
            }
        }
        std::thread::sleep(wait);
        wait = (wait * 2).min(LONGEST_WAIT);
    }
}

/// Run the command to completion and turn what it printed into one reading.
///
/// The last non-empty line is the answer. A script that prints one line means that line;
/// one that prints several has said several things and the most recent is what a bar
/// should be showing. Anything a script wants kept out of this goes to standard error,
/// which is left alone and lands in dbar's log.
fn answer(name: &str, rest: &[String], declared: &'static [FieldSpec]) -> Reading {
    match run_to_end(name, rest) {
        Ok(output) => {
            match last_word(&output) {
                Some(line) => reading_of(line, declared),
                // It ran, it worked, and it had nothing to say. An empty reading is the
                // honest answer, and a module whose format needs a field it did not get
                // draws nothing rather than something wrong.
                None => Reading {
                    fields: Fields::default(),
                    state: State::Idle,
                },
            }
        }
        Err(e) => {
            log::warn!("{name}: {e:#}");
            failed(&e)
        }
    }
}

/// The line of a command's output that is its answer: the last one with anything on it.
fn last_word(output: &str) -> Option<&str> {
    output.lines().rev().find(|line| !line.trim().is_empty())
}

/// Run the command until it exits, and hand back what it wrote to standard output.
fn run_to_end(program: &str, args: &[String]) -> Result<String> {
    let mut child = configured(program, args)
        .spawn()
        .with_context(|| format!("spawning {program}"))?;
    let mut stdout = child.stdout.take().context("stdout was piped")?;
    let mut output = String::new();
    let read = std::io::Read::read_to_string(&mut stdout, &mut output);
    let status = child.wait().context("waiting for the command")?;
    read.with_context(|| format!("reading from {program}"))?;
    if !status.success() {
        anyhow::bail!("{program} exited with {status}");
    }
    Ok(output)
}

/// Run the command until it stops, sending a reading per line.
fn run_once(
    program: &str,
    args: &[String],
    declared: &'static [FieldSpec],
    sender: &calloop::channel::Sender<Reading>,
) -> Result<()> {
    let mut child = configured(program, args)
        .spawn()
        .with_context(|| format!("spawning {program}"))?;

    let stdout = child.stdout.take().context("stdout was piped")?;
    for line in BufReader::new(stdout).lines() {
        let line = match line {
            Ok(line) => line,
            Err(e) => {
                reap(&mut child);
                return Err(e).context("reading a line");
            }
        };
        if sender.send(reading_of(&line, declared)).is_err() {
            // The bar has gone; take the command with it.
            reap(&mut child);
            return Ok(());
        }
    }
    reap(&mut child);
    Ok(())
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
fn reap(child: &mut Child) {
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
