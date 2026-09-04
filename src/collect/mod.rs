//! Native collectors: what dbar measures for itself.
//!
//! A collector reads one thing the system knows and publishes it as typed fields. It does
//! no formatting and holds no opinion about how it is drawn, so the same reading serves a
//! module that shows a percentage and one that shows a bar.
//!
//! Collectors run on the event loop rather than on threads. A `/proc` read takes
//! microseconds, so a thread would cost more than it saves; the sources that genuinely
//! block - D-Bus, PipeWire - get a thread each when they arrive, and reach the loop the
//! same way the compositor connection already does.

pub mod backlight;
pub mod cpu;
pub mod memory;
pub mod time;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::status::{FieldSpec, Fields, State};

/// One thing a native module can be built on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Which {
    Cpu,
    Memory,
    Backlight,
    Time,
}

impl Which {
    pub fn parse(name: &str) -> Option<Which> {
        Some(match name {
            "cpu" => Which::Cpu,
            "memory" => Which::Memory,
            "backlight" => Which::Backlight,
            "time" => Which::Time,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Which::Cpu => "cpu",
            Which::Memory => "memory",
            Which::Backlight => "backlight",
            Which::Time => "time",
        }
    }

    pub fn fields(self) -> &'static [FieldSpec] {
        match self {
            Which::Cpu => cpu::FIELDS,
            Which::Memory => memory::FIELDS,
            Which::Backlight => backlight::FIELDS,
            Which::Time => time::FIELDS,
        }
    }

    /// What the module says when the config does not give it a format.
    pub fn default_format(self) -> &'static str {
        match self {
            Which::Cpu => " $utilization ",
            Which::Memory => " $percent ",
            Which::Backlight => " $brightness ",
            Which::Time => " $now.time(f:'%a %d %b %H:%M') ",
        }
    }

    /// How often to read, when the config does not say.
    pub fn default_interval(self) -> Duration {
        match self {
            Which::Cpu | Which::Memory => Duration::from_secs(2),
            // A backlight only changes when something changes it.
            Which::Backlight => Duration::from_secs(5),
            Which::Time => Duration::from_secs(60),
        }
    }

    /// Whether readings should land on the wall clock rather than drifting from start-up.
    ///
    /// A clock that ticks 1.3 seconds after every minute is visibly wrong for most of a
    /// second; nothing else cares when in the second it is sampled.
    fn aligned(self) -> bool {
        matches!(self, Which::Time)
    }

    fn open(self) -> Box<dyn Collector> {
        match self {
            Which::Cpu => Box::new(cpu::Cpu::new()),
            Which::Memory => Box::new(memory::Memory),
            Which::Backlight => Box::new(backlight::Backlight::new()),
            Which::Time => Box::new(time::Time),
        }
    }
}

/// What a collector produces for one tick.
#[derive(Clone, Debug, Default)]
pub struct Reading {
    pub fields: Fields,
    pub state: State,
}

pub trait Collector {
    fn read(&mut self) -> Result<Reading>;
}

/// The collectors a config asks for, and when each is next due.
///
/// One deadline serves the whole set: the loop sleeps until the earliest, then reads
/// everything that has come due and redraws once. Ten modules on the same interval cause
/// one wake-up, not ten.
pub struct Registry {
    entries: Vec<Entry>,
}

struct Entry {
    which: Which,
    interval: Duration,
    collector: Box<dyn Collector>,
    /// The last thing this said. Kept across a failure, so a momentary error does not
    /// blank a module that was working a second ago.
    reading: Reading,
    due: Instant,
    /// Consecutive failures, which lengthen the wait before trying again.
    failures: u32,
    /// Whether the current failure has been reported, so a broken sensor logs once.
    reported: bool,
}

/// How far the wait is allowed to stretch while a collector keeps failing.
const MAX_BACKOFF: u32 = 5;

impl Registry {
    /// Open a collector for each source the config names, at the shortest interval any
    /// module asked it for.
    pub fn new(wanted: &HashMap<Which, Duration>) -> Registry {
        let now = Instant::now();
        let mut entries: Vec<Entry> = wanted
            .iter()
            .map(|(&which, &interval)| Entry {
                which,
                interval,
                collector: which.open(),
                reading: Reading::default(),
                due: now,
                failures: 0,
                reported: false,
            })
            .collect();
        // A stable order keeps logs and tests from depending on hash iteration.
        entries.sort_by_key(|e| e.which.name());
        Registry { entries }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Read everything that has come due, and say whether anything changed.
    pub fn tick(&mut self) -> bool {
        let now = Instant::now();
        let mut changed = false;
        for entry in &mut self.entries {
            if entry.due > now {
                continue;
            }
            match entry.collector.read() {
                Ok(reading) => {
                    if entry.failures > 0 {
                        log::info!("{} is reporting again", entry.which.name());
                    }
                    entry.failures = 0;
                    entry.reported = false;
                    entry.reading = reading;
                }
                Err(e) => {
                    // The last good reading stays on screen, marked as stale, rather than
                    // the module vanishing because a file was busy for one tick.
                    entry.reading.state = State::Error;
                    entry.failures = entry.failures.saturating_add(1);
                    if !entry.reported {
                        log::warn!("{} could not be read: {e:#}", entry.which.name());
                        entry.reported = true;
                    }
                }
            }
            entry.due = entry.next_due(now);
            changed = true;
        }
        changed
    }

    /// Read one source again at the next opportunity, whatever its interval said.
    ///
    /// The interval starts over from the refresh, so a source that is asked for often is
    /// not then read again a moment later out of habit.
    pub fn refresh(&mut self, which: Which) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.which == which) {
            entry.due = Instant::now();
        }
    }

    /// When the loop should wake up next, if anything is scheduled at all.
    pub fn next_due(&self) -> Option<Instant> {
        self.entries.iter().map(|e| e.due).min()
    }

    /// A registry holding one reading that never changes, for exercising the code that
    /// draws collectors without depending on the machine running the test.
    #[cfg(test)]
    pub fn fixture(which: Which, reading: Reading) -> Registry {
        /// Never due, so it is never asked for anything.
        struct Never;
        impl Collector for Never {
            fn read(&mut self) -> Result<Reading> {
                unreachable!("a fixture registry is never due")
            }
        }

        Registry {
            entries: vec![Entry {
                which,
                interval: Duration::from_secs(1),
                collector: Box::new(Never),
                reading,
                due: Instant::now() + Duration::from_secs(3600),
                failures: 0,
                reported: false,
            }],
        }
    }

    pub fn reading(&self, which: Which) -> Option<&Reading> {
        self.entries
            .iter()
            .find(|e| e.which == which)
            .map(|e| &e.reading)
    }
}

impl Entry {
    fn next_due(&self, now: Instant) -> Instant {
        // Back off while a collector is failing, so a missing sensor does not spin.
        let wait = self.interval * 2u32.pow(self.failures.min(MAX_BACKOFF));
        if !self.which.aligned() || self.failures > 0 {
            return now + wait;
        }
        now + align(wait)
    }
}

/// The wait that lands on the next whole multiple of `interval` on the wall clock.
///
/// Uses the system clock only for the offset within the interval, so the deadline itself
/// stays on the monotonic clock and a clock adjustment cannot stall the bar.
fn align(interval: Duration) -> Duration {
    let Ok(since_epoch) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
        return interval;
    };
    let step = interval.as_nanos();
    if step == 0 {
        return interval;
    }
    let past = since_epoch.as_nanos() % step;
    let remaining = step - past;
    // A deadline a hair in the future would fire twice on the same tick, so a reading
    // taken exactly on the boundary waits a whole interval instead.
    if remaining < Duration::from_millis(1).as_nanos() {
        interval
    } else {
        Duration::from_nanos(remaining as u64)
    }
}

/// Read a file that the kernel generates, where the reported length is meaningless.
fn read_to_string(path: impl AsRef<std::path::Path>) -> Result<String> {
    let path = path.as_ref();
    std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_source_has_a_default_format_it_can_actually_render() {
        for which in [Which::Cpu, Which::Memory, Which::Backlight, Which::Time] {
            let format = crate::format::Format::parse(which.default_format())
                .unwrap_or_else(|e| panic!("{} default format: {e:#}", which.name()));
            format
                .check(which.fields())
                .unwrap_or_else(|e| panic!("{} default format: {e:#}", which.name()));
        }
    }

    #[test]
    fn source_names_round_trip() {
        for which in [Which::Cpu, Which::Memory, Which::Backlight, Which::Time] {
            assert_eq!(Which::parse(which.name()), Some(which));
        }
        assert_eq!(Which::parse("nonesuch"), None);
    }

    /// A collector whose answers the test dictates.
    struct Scripted {
        answers: Vec<Option<&'static str>>,
    }

    impl Collector for Scripted {
        fn read(&mut self) -> Result<Reading> {
            match self.answers.remove(0) {
                Some(text) => {
                    let mut fields = Fields::default();
                    fields.set("text", crate::status::Value::Text(text.to_string()));
                    Ok(Reading {
                        fields,
                        state: State::Idle,
                    })
                }
                None => anyhow::bail!("the sensor said no"),
            }
        }
    }

    fn scripted(answers: Vec<Option<&'static str>>) -> Registry {
        Registry {
            entries: vec![Entry {
                which: Which::Cpu,
                interval: Duration::from_secs(1),
                collector: Box::new(Scripted { answers }),
                reading: Reading::default(),
                due: Instant::now(),
                failures: 0,
                reported: false,
            }],
        }
    }

    fn text_of(registry: &Registry) -> Option<String> {
        match registry.reading(Which::Cpu)?.fields.get("text")? {
            crate::status::Value::Text(t) => Some(t.clone()),
            _ => None,
        }
    }

    #[test]
    fn a_failure_keeps_the_last_good_reading_on_screen() {
        let mut registry = scripted(vec![Some("first"), None]);
        assert!(registry.tick());
        assert_eq!(text_of(&registry).as_deref(), Some("first"));

        // Due again straight away, so the failing read happens now.
        registry.entries[0].due = Instant::now();
        assert!(registry.tick());
        assert_eq!(
            text_of(&registry).as_deref(),
            Some("first"),
            "a momentary failure should not blank a module that was working"
        );
        assert_eq!(
            registry.reading(Which::Cpu).map(|r| r.state),
            Some(State::Error)
        );
    }

    #[test]
    fn a_failing_collector_is_tried_less_and_less_often() {
        let mut registry = scripted(vec![None, None, None]);
        let mut waits = Vec::new();
        for _ in 0..3 {
            let before = Instant::now();
            registry.entries[0].due = before;
            registry.tick();
            waits.push(registry.entries[0].due.saturating_duration_since(before));
        }
        assert!(
            waits[0] < waits[1] && waits[1] < waits[2],
            "the wait should grow while it keeps failing: {waits:?}"
        );
    }

    #[test]
    fn nothing_is_read_before_it_is_due() {
        let mut registry = scripted(vec![Some("only once")]);
        assert!(registry.tick());
        // The answers are exhausted, so a second read would panic rather than fail.
        assert!(
            !registry.tick(),
            "a collector should not be read before it is due"
        );
    }

    #[test]
    fn alignment_lands_inside_the_interval() {
        for secs in [1, 5, 60] {
            let interval = Duration::from_secs(secs);
            let wait = align(interval);
            assert!(wait > Duration::ZERO && wait <= interval, "for {secs}s");
        }
    }
}
