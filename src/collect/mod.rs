//! Native collectors: what dbar measures for itself.
//!
//! A collector reads one thing the system knows and publishes it as typed fields. It does
//! no formatting and holds no opinion about how it is drawn, so the same reading serves a
//! module that shows a percentage and one that shows a bar.
//!
//! Cheap collectors run on the event loop rather than on threads. A `/proc` read takes
//! microseconds, so a thread would cost more than it saves; sources that can wait on a
//! driver, daemon or remote filesystem get a thread each and reach the loop the same way
//! the compositor connection already does.

pub mod audio;
pub mod backlight;
pub mod battery;
pub mod command;
pub mod cpu;
pub mod disk;
pub mod load;
pub mod media;
pub mod memory;
pub mod network;
pub mod nl80211;
pub mod slow;
pub mod temperature;
pub mod time;
pub mod watch;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::status::{FieldSpec, Fields, State};

/// One thing a native module can be built on.
///
/// Two of these carry what they are pointed at, because "the disk" and "the network" are
/// not single things. That also makes them the registry's key, so two modules watching the
/// same path share one reading while two watching different paths do not.
/// A command module: what to run, and what it says it will publish.
///
/// Two modules naming the same command the same way share one process, so the argv and
/// how it is run decide identity - the declared fields are for checking the config, not
/// for telling one command from another. Two modules that want the same program on
/// different schedules want two of it, and get two.
#[derive(Clone, Debug)]
pub struct CommandSpec {
    pub argv: Vec<String>,
    /// Whether the command streams, answers on an interval, or answers once.
    pub run: command::Run,
    /// Whether every line of a run is a reading of its own, rather than the last one
    /// being the answer. One command, several places to report on.
    pub pages: bool,
    /// Declared in the config, because dbar cannot know what somebody else's program
    /// prints, and leaked while the config is read so the thread parsing that program's
    /// output can hold it. A reload reads the file again and leaks another one; see the
    /// note where they are made.
    pub fields: &'static [FieldSpec],
    /// How long one run is given before it is stopped. Part of what makes two specs
    /// different, like the schedule: two modules that run the same program but disagree
    /// about how long to wait for it are asking for two different things.
    pub timeout: Duration,
}

impl PartialEq for CommandSpec {
    fn eq(&self, other: &Self) -> bool {
        self.argv == other.argv
            && self.run == other.run
            && self.pages == other.pages
            && self.timeout == other.timeout
    }
}

impl Eq for CommandSpec {}

impl std::hash::Hash for CommandSpec {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.argv.hash(state);
        self.run.hash(state);
        self.pages.hash(state);
        self.timeout.hash(state);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Which {
    Audio,
    Media,
    Cpu,
    Memory,
    Battery,
    Backlight,
    Load,
    Temperature(Option<String>),
    Disk(String),
    Network(Option<String>),
    Time,
    /// A program of your own, streaming a line per reading.
    Command(CommandSpec),
}

impl Which {
    /// The bare source name, without whatever it is pointed at.
    pub fn name(&self) -> &'static str {
        match self {
            Which::Audio => "audio",
            Which::Media => "media",
            Which::Cpu => "cpu",
            Which::Memory => "memory",
            Which::Battery => "battery",
            Which::Backlight => "backlight",
            Which::Load => "load",
            Which::Temperature(_) => "temperature",
            Which::Disk(_) => "disk",
            Which::Network(_) => "network",
            Which::Time => "time",
            Which::Command(_) => "command",
        }
    }

    /// How this source appears in a log, which for a parameterised one includes what it is
    /// pointed at, or there is no telling two of them apart.
    fn describe(&self) -> String {
        match self {
            Which::Disk(path) => format!("disk {path}"),
            Which::Temperature(Some(chip)) => format!("temperature {chip}"),
            Which::Network(Some(device)) => format!("network {device}"),
            other => other.name().to_string(),
        }
    }

    pub fn fields(&self) -> &'static [FieldSpec] {
        match self {
            Which::Audio => audio::FIELDS,
            Which::Media => media::FIELDS,
            Which::Cpu => cpu::FIELDS,
            Which::Memory => memory::FIELDS,
            Which::Battery => battery::FIELDS,
            Which::Backlight => backlight::FIELDS,
            Which::Load => load::FIELDS,
            Which::Temperature(_) => temperature::FIELDS,
            Which::Disk(_) => disk::FIELDS,
            Which::Network(_) => network::FIELDS,
            Which::Time => time::FIELDS,
            Which::Command(spec) => spec.fields,
        }
    }

    /// The sources the kernel can report a change on, so the watcher knows what to try.
    ///
    /// A source that is not here is read on an interval; one that is here is read on an
    /// interval only while its file is missing.
    pub const WATCHABLE: &'static [Which] = &[Which::Backlight, Which::Battery];

    /// How the kernel reports a change to this source, if it does.
    fn watch(&self) -> Option<watch::Watch> {
        match self {
            Which::Backlight => backlight::watch_path().map(watch::Watch::Attribute),
            // A battery has no attribute to wait on, and what its firmware announces is
            // not everything that happens to it, so this brings the reading forward
            // rather than replacing the interval.
            Which::Battery => Some(watch::Watch::Uevent("power_supply")),
            _ => None,
        }
    }

    /// What the module says when the config does not give it a format.
    pub fn default_format(&self) -> &'static str {
        match self {
            Which::Audio => " $volume ",
            Which::Media => " $title{  $artist} ",
            Which::Cpu => " $utilization ",
            Which::Memory => " $percent ",
            Which::Battery => " $percent ",
            Which::Backlight => " $brightness ",
            Which::Load => " $one ",
            Which::Temperature(_) => " $temp ",
            Which::Disk(_) => " $available ",
            Which::Network(_) => " $down  $up ",
            Which::Time => " $now.time(f:'%a %d %b %H:%M') ",
            // A command that declares no fields has said one thing, and this is it.
            Which::Command(_) => " $text ",
        }
    }

    /// How often to read, when the config does not say.
    pub fn default_interval(&self) -> Duration {
        match self {
            // Never used: both of these arrive when they change rather than being read.
            Which::Audio | Which::Media => Duration::from_secs(60),
            Which::Cpu | Which::Memory | Which::Load | Which::Network(_) => Duration::from_secs(2),
            Which::Temperature(_) => Duration::from_secs(5),
            // A disk fills slowly, and reading it can wake a spinning one.
            Which::Disk(_) => Duration::from_secs(60),
            // A battery moves slowly, and reading it wakes the embedded controller.
            Which::Battery => Duration::from_secs(30),
            // A backlight only changes when something changes it.
            Which::Backlight => Duration::from_secs(5),
            Which::Time => Duration::from_secs(60),
            // Never read on the timer: the command pushes when it has something to say.
            Which::Command(_) => Duration::from_secs(60),
        }
    }

    /// Whether readings should land on the wall clock rather than drifting from start-up.
    ///
    /// A clock that ticks 1.3 seconds after every minute is visibly wrong for most of a
    /// second; nothing else cares when in the second it is sampled.
    fn aligned(&self) -> bool {
        matches!(self, Which::Time)
    }

    /// Whether this source arrives on its own rather than being read.
    ///
    /// A pushed source is never on the timer and has no collector to call: what it knows
    /// comes from a thread that is told, and the registry only holds the last of it.
    pub fn pushed(&self) -> bool {
        matches!(self, Which::Audio | Which::Media | Which::Command(_)) || self.blocking()
    }

    /// Whether reading this source can wait on something outside the event loop's control.
    ///
    /// `statvfs` answers when the filesystem does, which for a network mount or a FUSE
    /// daemon that has gone may be never; a wireless link is a netlink request and a wait
    /// for the driver to answer. Battery and explicitly selected temperature attributes
    /// are sysfs files, but reading them may still ask firmware or hardware through their
    /// drivers. The default temperature source only selects the CPU drivers listed in
    /// `temperature::CPU_CHIPS`; the measured default path stays on the shared timer.
    ///
    /// Backlight remains on the shared timer deliberately. Moving only its reads would
    /// leave adjustment writes on the event loop; that path needs measurement and, if it
    /// proves material, one worker that orders writes with the reads they cause.
    ///
    /// A source like that is read on a thread of its own and arrives the way a command's
    /// reading does, because the alternative is a mount that stopped answering stopping
    /// the clock, the pointer and the compositor's own events with it.
    pub fn blocking(&self) -> bool {
        matches!(
            self,
            Which::Battery | Which::Temperature(Some(_)) | Which::Disk(_) | Which::Network(_)
        )
    }

    /// The collector itself, whichever thread is going to read it.
    pub fn collector(&self) -> Box<dyn Collector> {
        match self {
            Which::Audio | Which::Media | Which::Command(_) => Box::new(Pushed),
            Which::Cpu => Box::new(cpu::Cpu::new()),
            Which::Memory => Box::new(memory::Memory::new()),
            Which::Battery => Box::new(battery::Battery::new()),
            Which::Backlight => Box::new(backlight::Backlight::new()),
            Which::Load => Box::new(load::Load::new()),
            Which::Temperature(chip) => Box::new(temperature::Temperature::new(chip.clone())),
            Which::Disk(path) => Box::new(disk::Disk::new(path.clone())),
            Which::Network(device) => Box::new(network::Network::new(device.clone())),
            Which::Time => Box::new(time::Time),
        }
    }
}

/// The way to ask a source for another reading before its schedule would have one.
///
/// A module asks by being clicked or sent its signal, and a watcher asks when the kernel
/// reports a change. The thread behind the source waits on this rather than sleeping, so
/// the reading is taken when the ask arrives rather than when its interval runs out.
pub struct Trigger(std::sync::mpsc::SyncSender<()>);

impl Trigger {
    pub(crate) fn new(sender: std::sync::mpsc::SyncSender<()>) -> Trigger {
        Trigger(sender)
    }

    pub fn ask(&self) {
        // One pending ask is enough: several notifications or impatient clicks still
        // want one fresh reading, not one per event. A full queue and a worker that has
        // gone are both deliberately silent, and neither may stall the event loop.
        let _ = self.0.try_send(());
    }
}

/// What a collector produces for one tick.
#[derive(Clone, Debug, Default)]
pub struct Reading {
    pub fields: Fields,
    pub state: State,
}

impl Reading {
    /// Whether this reading would draw the same as another.
    pub fn same(&self, other: &Reading) -> bool {
        self.state == other.state && self.fields.same(&other.fields)
    }
}

pub trait Collector {
    fn read(&mut self) -> Result<Reading>;
}

/// A source that arrives rather than being read. Nothing asks it anything.
struct Pushed;

impl Collector for Pushed {
    fn read(&mut self) -> Result<Reading> {
        anyhow::bail!("this source is pushed rather than read")
    }
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
    /// The last thing this said, and never empty. Kept across a failure, so a momentary
    /// error does not blank a module that was working a second ago.
    ///
    /// A source that reports on one thing holds one reading. A command reporting on
    /// several - the weather in three cities - holds one per page, and the module scrolls
    /// between them; everything below here is the same either way, since a page is just a
    /// reading like any other.
    readings: Vec<Reading>,
    /// When to read next, or nothing at all while the kernel is reporting changes.
    due: Option<Instant>,
    /// Whether a watcher is reporting this source's changes, so it costs no wake-ups.
    watched: bool,
    /// Consecutive failures, which lengthen the wait before trying again.
    failures: u32,
    /// Whether the current failure has been reported, so a broken sensor logs once.
    reported: bool,
    /// What the wall clock said when an aligned source was last read, which is the only
    /// way to notice that the reading is about a minute that has since gone by.
    read_at: Option<std::time::SystemTime>,
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
            .map(|(which, &interval)| Entry {
                // A source that can block is read on a thread of its own; the registry
                // holds its readings and never asks for them itself.
                collector: match which.pushed() {
                    true => Box::new(Pushed),
                    false => which.collector(),
                },
                interval,
                readings: vec![Reading::default()],
                due: match which.pushed() {
                    true => None,
                    false => Some(now),
                },
                watched: which.pushed(),
                which: which.clone(),
                failures: 0,
                reported: false,
                read_at: None,
            })
            .collect();
        // A stable order keeps logs and tests from depending on hash iteration.
        entries.sort_by_key(|e| e.which.describe());
        Registry { entries }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Read everything that has come due, and say whether anything changed.
    pub fn tick(&mut self) -> bool {
        let now = Instant::now();
        let wall = std::time::SystemTime::now();
        let mut changed = false;
        for at in 0..self.entries.len() {
            let entry = &self.entries[at];
            // Due on the timer, or holding a reading the wall clock has left behind.
            if entry.due.is_none_or(|due| due > now) && !entry.late(wall) {
                continue;
            }
            let read = self.entries[at].collector.read();
            changed |= self.record(at, read);
            if self.entries[at].which.aligned() {
                self.entries[at].read_at = Some(std::time::SystemTime::now());
            }
            // Timed from after the read rather than from the top of the tick. An aligned
            // source scheduled from `now` aims short by however long this tick's reads
            // took, which lands the next one back on the wrong side of the boundary.
            self.entries[at].due = self.entries[at].next_due(Instant::now());
        }
        changed
    }

    /// Take a reading from the thread that owns a source that can block, and say whether
    /// the bar has anything new to draw.
    ///
    /// The same bookkeeping as a reading taken here: a source is stale or fresh, and says
    /// so once rather than every interval, wherever it was read.
    pub fn arrived(&mut self, which: &Which, read: Result<Reading>) -> bool {
        match self.entries.iter().position(|e| &e.which == which) {
            Some(at) => self.record(at, read),
            None => false,
        }
    }

    /// Keep what a read came to, and say whether it changes what is drawn.
    fn record(&mut self, at: usize, read: Result<Reading>) -> bool {
        let entry = &mut self.entries[at];
        match read {
            Ok(reading) => {
                if entry.failures > 0 {
                    log::info!("{} is reporting again", entry.which.describe());
                }
                entry.failures = 0;
                entry.reported = false;
                // Most of what a bar shows changes rarely: a disk that is still 41% full
                // draws exactly what it drew a minute ago. Saying so lets the whole redraw
                // be skipped, and saves replacing the reading as well.
                if matches!(entry.readings.as_slice(), [old] if old.same(&reading)) {
                    return false;
                }
                entry.readings = vec![reading];
                true
            }
            Err(e) => {
                // The last good reading stays on screen, marked as stale, rather than the
                // module vanishing because a file was busy for one tick. A source that
                // stays broken is already drawn as stale, so only the first failure is
                // worth a redraw.
                let changed = entry.readings.iter().any(|r| r.state != State::Error);
                for reading in &mut entry.readings {
                    reading.state = State::Error;
                }
                entry.failures = entry.failures.saturating_add(1);
                if !entry.reported {
                    log::warn!("{} could not be read: {e:#}", entry.which.describe());
                    entry.reported = true;
                }
                changed
            }
        }
    }

    /// Read one source again at the next opportunity, whatever its interval said.
    ///
    /// The interval starts over from the refresh, so a source that is asked for often is
    /// not then read again a moment later out of habit.
    ///
    /// A pushed source has nothing to read: what it knows arrives from a thread. Bringing
    /// one forward here would ask a collector that exists only to say it cannot answer,
    /// and mark a working module as broken; a command is asked through its own trigger
    /// instead.
    pub fn refresh(&mut self, which: &Which) {
        if which.pushed() {
            return;
        }
        if let Some(entry) = self.entries.iter_mut().find(|e| &e.which == which) {
            entry.due = Some(Instant::now());
        }
    }

    /// Take what a source sent of its own accord: one reading, or a page each, and say
    /// whether it changes what is drawn.
    ///
    /// A publication with nothing in it is a source that ran and had nothing to say, which
    /// is one empty reading rather than no reading at all - a module with no pages would
    /// have nothing to draw and nothing left to click.
    ///
    /// A source that arrives says so whenever its own world moved, and most of those
    /// movements are not ones a bar shows: a player reporting its position again, a volume
    /// set to what it already was. Saying that nothing changed is what lets the caller
    /// skip a layout, the same way a reading taken on the timer already does.
    pub fn push(&mut self, which: &Which, readings: Vec<Reading>) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|e| &e.which == which) else {
            return false;
        };
        let readings = match readings.is_empty() {
            true => vec![Reading::default()],
            false => readings,
        };
        if entry.readings.len() == readings.len()
            && std::iter::zip(&entry.readings, &readings).all(|(old, new)| old.same(new))
        {
            return false;
        }
        entry.readings = readings;
        true
    }

    /// Say that the kernel is reporting this source's changes, or has stopped.
    ///
    /// A watched source is taken off the timer entirely: it is read when its watcher says
    /// something moved, so an interval would only be asking a question already answered.
    pub fn set_watched(&mut self, which: &Which, watched: bool) {
        if let Some(entry) = self.entries.iter_mut().find(|e| &e.which == which) {
            entry.watched = watched;
            if !watched && entry.due.is_none() {
                entry.due = Some(Instant::now());
            }
        }
    }

    /// Whether anything is still due to be read on a timer.
    pub fn is_scheduled(&self) -> bool {
        self.entries.iter().any(|e| e.due.is_some())
    }

    /// When the loop should wake up next, if anything is scheduled at all.
    pub fn next_due(&self) -> Option<Instant> {
        self.entries.iter().filter_map(|e| e.due).min()
    }

    /// A registry holding one reading that never changes, for exercising the code that
    /// draws collectors without depending on the machine running the test.
    #[cfg(test)]
    pub fn fixture(which: Which, reading: Reading) -> Registry {
        Registry::fixture_pages(which, vec![reading])
    }

    /// The same, for several sources at once.
    #[cfg(test)]
    pub fn fixtures(sources: Vec<(Which, Reading)>) -> Registry {
        let entries = sources
            .into_iter()
            .flat_map(|(which, reading)| Registry::fixture(which, reading).entries)
            .collect();
        Registry { entries }
    }

    /// The same, for a source that published several readings at once.
    #[cfg(test)]
    pub fn fixture_pages(which: Which, readings: Vec<Reading>) -> Registry {
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
                readings,
                due: Some(Instant::now() + Duration::from_secs(3600)),
                watched: false,
                failures: 0,
                reported: false,
                read_at: None,
            }],
        }
    }

    /// The page a module is scrolled to, and how many there are to scroll through.
    ///
    /// Both come from one pass over the sources, because this is the redraw path: layout
    /// asks it for every module of every frame.
    ///
    /// The index is the module's, not the source's, so it wraps: a fetch that came back
    /// with fewer places than the last one leaves a module pointing past the end, and it
    /// should show something rather than nothing.
    pub fn showing(&self, which: &Which, index: usize) -> Option<(&Reading, usize)> {
        let entry = self.entries.iter().find(|e| &e.which == which)?;
        let pages = entry.readings.len();
        let reading = entry.readings.get(index % pages.max(1))?;
        Some((reading, pages))
    }

    /// What a source last said, or its first page when it said several things at once.
    #[cfg(test)]
    pub fn reading(&self, which: &Which) -> Option<&Reading> {
        self.page(which, 0)
    }

    #[cfg(test)]
    pub fn page(&self, which: &Which, index: usize) -> Option<&Reading> {
        self.showing(which, index).map(|(reading, _)| reading)
    }

    #[cfg(test)]
    pub fn pages(&self, which: &Which) -> usize {
        self.showing(which, 0).map_or(0, |(_, pages)| pages)
    }
}

impl Entry {
    /// Whether an aligned reading is about a boundary the wall clock has already left.
    ///
    /// The deadline is on the monotonic clock, which does not run while the machine is
    /// suspended; the reading is about the wall clock, which does. So a laptop that slept
    /// through three boundaries wakes with a clock that is right about none of them and a
    /// deadline still in the future.
    ///
    /// This is a check, not a wake-up: it runs on ticks that were happening anyway, so it
    /// costs the comparison below and nothing else. What corrects the clock after a resume
    /// is therefore whichever source is due first - the monotonic deadlines all froze
    /// together, so a bar with a cpu module reading every two seconds notices within two
    /// seconds of waking. A bar whose only source is the clock has no such tick, and waits
    /// out the rest of its own interval before it is right again.
    ///
    /// Closing that last gap needs a timer that counts through suspend - a `CLOCK_BOOTTIME`
    /// timerfd in place of calloop's monotonic one - which is a wake-up path of its own to
    /// own and to get wrong. It is not worth it for a bar that shows nothing but the time;
    /// it would be worth it if anything else ever needed waking on wall-clock boundaries.
    fn late(&self, wall: std::time::SystemTime) -> bool {
        let (Some(read_at), true) = (self.read_at, self.which.aligned()) else {
            return false;
        };
        let bucket = |at: std::time::SystemTime| {
            at.duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|since| since.as_nanos() / self.interval.as_nanos().max(1))
        };
        match (bucket(read_at), bucket(wall)) {
            (Some(then), Some(now)) => now > then,
            _ => false,
        }
    }

    fn next_due(&self, now: Instant) -> Option<Instant> {
        // A watched source is read when it changes. It goes back on the timer only while
        // it is failing, so a sensor that has broken is still retried.
        if self.watched && self.failures == 0 {
            return None;
        }
        let wait = backoff(self.interval, self.failures);
        if !self.which.aligned() || self.failures > 0 {
            return Some(now + wait);
        }
        // The wall clock first and the monotonic clock second, in that order and not the
        // other way round. Whatever happens between the two samples - a descheduled
        // thread, a slow page fault - is then time the deadline is late by rather than
        // early by, and late is a reading a hair after the boundary while early is the
        // minute before it standing on the bar.
        let wait = align(wait);
        Some(Instant::now() + wait)
    }
}

/// How long to wait before reading a source again, given how often reading it has failed.
///
/// Doubling while a source is failing is what keeps a missing sensor or an interface that
/// was unplugged from being asked at its full rate forever. It is the same policy wherever
/// the reading is taken - on the shared timer, or on the thread of a source that can block
/// - because a source that has gone is a source that has gone either way.
pub fn backoff(interval: Duration, failures: u32) -> Duration {
    interval * 2u32.pow(failures.min(MAX_BACKOFF))
}

/// How far past the boundary an aligned reading aims, at most.
///
/// Aiming exactly at the minute is a coin toss, and losing it is expensive: a timer that
/// fires a hair early reads a clock that still says the minute before, and that wording
/// then stands for the whole of the next minute. Two milliseconds is longer than the
/// jitter and shorter than anyone can see.
///
/// It is a ceiling rather than a fixed amount, because an interval may be shorter than it:
/// the shortest a config may ask for is a millisecond, and a guard longer than the interval
/// would step over the boundaries it exists to land on.
const ALIGNMENT_GUARD: Duration = Duration::from_millis(2);

/// The guard this interval can afford: the whole of it, or a quarter of a short one.
fn guard_for(interval: Duration) -> Duration {
    ALIGNMENT_GUARD.min(interval / 4)
}

/// The wait that lands just after the next whole multiple of `interval` on the wall clock.
///
/// Uses the system clock only for the offset within the interval, so the deadline itself
/// stays on the monotonic clock and a clock adjustment cannot stall the bar.
fn align(interval: Duration) -> Duration {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(since_epoch) => align_from(since_epoch, interval),
        // A clock set before the epoch is not one to schedule against.
        Err(_) => interval,
    }
}

/// The same arithmetic, against a wall clock that is handed in rather than read.
///
/// A remainder is never rounded up to a whole interval. Doing that used to mean that a
/// reading taken a fraction of a millisecond early - which is what a timer aimed at the
/// boundary does - waited a further whole minute, leaving the minute before it on the bar
/// the entire time.
fn align_from(since_epoch: Duration, interval: Duration) -> Duration {
    let step = interval.as_nanos();
    if step == 0 {
        return interval;
    }
    let past = since_epoch.as_nanos() % step;
    let remaining = u64::try_from(step - past).unwrap_or(u64::MAX);
    Duration::from_nanos(remaining).saturating_add(guard_for(interval))
}

/// Read a file that the kernel generates, where the reported length is meaningless.
///
/// `std::fs::read_to_string` sizes its buffer from the length the file reports, and every
/// file under `/proc` and `/sys` reports zero however much it is about to hand over. That
/// leaves the buffer growing from nothing on every read, a few times a second, forever.
/// Asking for a page up front is enough for all of them: `/proc/stat` on a large machine
/// is the biggest of the lot and still fits.
fn read_to_string(path: impl AsRef<std::path::Path>) -> Result<String> {
    use std::io::Read as _;
    let path = path.as_ref();
    let read = |path: &std::path::Path| -> std::io::Result<String> {
        let mut file = std::fs::File::open(path)?;
        let mut out = String::with_capacity(PSEUDO_FILE);
        file.read_to_string(&mut out)?;
        Ok(out)
    };
    read(path).map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))
}

/// What to make room for when reading a file the kernel generates.
const PSEUDO_FILE: usize = 4096;

/// A file the kernel generates, held open and read again from the start on every tick.
///
/// `/proc` and `/sys` produce a fresh copy for a read at offset zero, so opening the file
/// again each time buys nothing: it is a path walk, a descriptor and a close, several
/// times every couple of seconds, for as long as the bar runs. `pread` at zero asks for
/// the same fresh copy through a descriptor kept for the purpose, and the text is read
/// into the buffer the last tick left behind.
///
/// A read that fails lets the descriptor go, so the next one opens the file again: a
/// device that went away and came back is found where it now is.
pub struct Pseudo {
    path: std::path::PathBuf,
    file: Option<std::fs::File>,
    text: String,
}

impl Pseudo {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Pseudo {
        Pseudo {
            path: path.into(),
            file: None,
            text: String::new(),
        }
    }

    pub fn read(&mut self) -> Result<&str> {
        match self.fill() {
            Ok(()) => Ok(&self.text),
            Err(e) => {
                self.file = None;
                Err(anyhow::anyhow!("reading {}: {e}", self.path.display()))
            }
        }
    }

    fn fill(&mut self) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt as _;
        let file = match &mut self.file {
            Some(file) => file,
            None => self.file.insert(std::fs::File::open(&self.path)?),
        };
        let mut bytes = std::mem::take(&mut self.text).into_bytes();
        bytes.clear();
        let mut chunk = [0u8; PSEUDO_FILE];
        loop {
            match file.read_at(&mut chunk, bytes.len() as u64) {
                Ok(0) => break,
                Ok(n) => bytes.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        self.text = String::from_utf8(bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every source, so a new one cannot be added without the checks below covering it.
    fn all() -> Vec<Which> {
        vec![
            Which::Cpu,
            Which::Memory,
            Which::Battery,
            Which::Backlight,
            Which::Load,
            Which::Temperature(None),
            Which::Disk("/".to_string()),
            Which::Network(None),
            Which::Time,
        ]
    }

    #[test]
    fn every_source_has_a_default_format_it_can_actually_render() {
        for which in all() {
            let format = crate::format::Format::parse(which.default_format())
                .unwrap_or_else(|e| panic!("{} default format: {e:#}", which.name()));
            format
                .check(which.fields())
                .unwrap_or_else(|e| panic!("{} default format: {e:#}", which.name()));
        }
    }

    /// Sources whose read can wait on a driver, daemon or filesystem are never
    /// read on the bar's own thread: their readings arrive from a thread of their own,
    /// and the registry only holds them.
    #[test]
    fn a_source_that_can_block_is_not_read_on_the_timer() {
        let disk = Which::Disk("/".to_string());
        assert!(disk.blocking() && disk.pushed());
        let wifi = Which::Network(None);
        assert!(wifi.blocking() && wifi.pushed());
        let battery = Which::Battery;
        assert!(battery.blocking() && battery.pushed());
        let temperature = Which::Temperature(Some("nvme".to_string()));
        assert!(temperature.blocking() && temperature.pushed());
        let default_temperature = Which::Temperature(None);
        assert!(!default_temperature.blocking() && !default_temperature.pushed());
        for which in all() {
            assert_eq!(
                which.blocking(),
                matches!(
                    which,
                    Which::Battery
                        | Which::Temperature(Some(_))
                        | Which::Disk(_)
                        | Which::Network(_)
                ),
                "{} is classified wrongly",
                which.describe()
            );
        }

        let wanted = HashMap::from([
            (disk.clone(), Duration::from_secs(1)),
            (battery, Duration::from_secs(1)),
            (temperature, Duration::from_secs(1)),
        ]);
        let mut registry = Registry::new(&wanted);
        assert!(!registry.tick(), "the timer has nothing to read");
        assert!(registry.next_due().is_none(), "and nothing to wake up for");

        // What the thread sends is kept, and counts the same way a timed reading does.
        let reading = |v: f64| {
            let mut fields = Fields::default();
            fields.set(
                "used",
                crate::status::Value::Num {
                    v,
                    unit: crate::status::Unit::Percent,
                },
            );
            Reading {
                fields,
                state: State::Idle,
            }
        };
        assert!(
            registry.arrived(&disk, Ok(reading(41.0))),
            "a first reading"
        );
        assert!(
            !registry.arrived(&disk, Ok(reading(41.0))),
            "the same again"
        );
        assert!(
            registry.arrived(&disk, Ok(reading(42.0))),
            "and a different one"
        );
        assert!(
            registry.arrived(&disk, Err(anyhow::anyhow!("the mount has gone"))),
            "going stale is a change"
        );
        assert!(
            !registry.arrived(&disk, Err(anyhow::anyhow!("still gone"))),
            "staying stale is not"
        );
    }

    /// A source that arrives reports whenever its own world moved, which is far more often
    /// than what a bar draws moves. The registry says so, so nothing above it lays the bar
    /// out again to discover it.
    #[test]
    fn a_pushed_reading_that_changes_nothing_says_so() {
        let mut registry = Registry::new(&HashMap::from([(Which::Audio, Duration::from_secs(1))]));
        let said = |text: &str| {
            let mut fields = Fields::default();
            fields.set("text", crate::status::Value::Text(text.to_string()));
            Reading {
                fields,
                state: State::Idle,
            }
        };
        assert!(registry.push(&Which::Audio, vec![said("40%")]), "the first");
        assert!(!registry.push(&Which::Audio, vec![said("40%")]), "the same");
        assert!(registry.push(&Which::Audio, vec![said("45%")]), "a change");
        // Pages are compared whole: the same readings in another order is a change.
        assert!(registry.push(&Which::Audio, vec![said("a"), said("b")]));
        assert!(!registry.push(&Which::Audio, vec![said("a"), said("b")]));
        assert!(registry.push(&Which::Audio, vec![said("b"), said("a")]));
    }

    #[test]
    fn refresh_requests_are_bounded_and_coalesced() {
        let (sender, asked) = std::sync::mpsc::sync_channel(1);
        let trigger = Trigger::new(sender);
        trigger.ask();
        trigger.ask();
        trigger.ask();
        assert_eq!(asked.try_recv(), Ok(()));
        assert_eq!(asked.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty));
    }

    /// A source that has gone is asked less and less often, wherever it is read: the
    /// shared timer and the thread of a source that can block ask the same question of
    /// the same function, so moving a read off the main thread cannot quietly turn a
    /// missing mount into a request every two seconds forever.
    #[test]
    fn a_failing_source_is_asked_less_and_less_often() {
        let interval = Duration::from_secs(2);
        assert_eq!(backoff(interval, 0), interval);
        assert_eq!(backoff(interval, 1), interval * 2);
        assert_eq!(backoff(interval, 5), interval * 32);
        // And stops doubling, rather than growing until it overflows.
        let capped = backoff(interval, MAX_BACKOFF);
        assert_eq!(backoff(interval, MAX_BACKOFF + 100), capped);
        assert_eq!(capped, interval * 2u32.pow(MAX_BACKOFF));
    }

    /// A source that reads the same value again has nothing for the bar to redraw, and
    /// says so: a redraw repaints the whole surface, so a disk that is still 41% full
    /// would otherwise cost a full frame every time its interval came round.
    #[test]
    fn a_reading_that_did_not_change_is_not_a_change() {
        struct Fixed(Option<f64>);
        impl Collector for Fixed {
            fn read(&mut self) -> Result<Reading> {
                let Some(v) = self.0 else {
                    anyhow::bail!("nothing to read");
                };
                let mut fields = Fields::default();
                fields.set(
                    "used",
                    crate::status::Value::Num {
                        v,
                        unit: crate::status::Unit::Percent,
                    },
                );
                Ok(Reading {
                    fields,
                    state: State::Idle,
                })
            }
        }

        let entry = |collector: Box<dyn Collector>| Entry {
            collector,
            interval: Duration::from_secs(0),
            readings: vec![Reading::default()],
            due: Some(Instant::now()),
            watched: false,
            which: Which::Cpu,
            failures: 0,
            reported: false,
            read_at: None,
        };

        let mut registry = Registry {
            entries: vec![entry(Box::new(Fixed(Some(41.0))))],
        };
        assert!(registry.tick(), "the first reading is new");
        registry.entries[0].due = Some(Instant::now());
        assert!(!registry.tick(), "the same reading again is not");

        // A source that has gone is drawn as stale once, not repainted for as long as it
        // stays broken.
        let mut failing = Registry {
            entries: vec![entry(Box::new(Fixed(None)))],
        };
        assert!(failing.tick(), "going stale is a change");
        failing.entries[0].due = Some(Instant::now());
        assert!(!failing.tick(), "staying stale is not");
    }

    #[test]
    fn a_parameterised_source_says_what_it_is_pointed_at() {
        // Two disk modules on different paths are two sources to read, and a log that
        // called both "disk" would be no help at all.
        let root = Which::Disk("/".to_string());
        let home = Which::Disk("/home".to_string());
        assert_ne!(root, home);
        assert_eq!(root.name(), home.name());
        assert_eq!(home.describe(), "disk /home");
        // An unparameterised one has nothing to add.
        assert_eq!(Which::Cpu.describe(), "cpu");
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

    /// A source that says several things at once holds them all, and the module picks
    /// one. Everything else about a page is what a reading always was.
    #[test]
    fn a_publication_of_several_readings_is_a_page_each() {
        let mut registry = Registry::new(&HashMap::from([(Which::Audio, Duration::from_secs(1))]));
        let said = |text: &str| {
            let mut fields = Fields::default();
            fields.set("text", crate::status::Value::Text(text.to_string()));
            Reading {
                fields,
                state: State::Idle,
            }
        };
        registry.push(&Which::Audio, vec![said("one"), said("two")]);
        assert_eq!(registry.pages(&Which::Audio), 2);
        let text = |index: usize| match registry.page(&Which::Audio, index) {
            Some(reading) => match reading.fields.get("text") {
                Some(crate::status::Value::Text(t)) => t.clone(),
                other => panic!("text is {other:?}"),
            },
            None => panic!("no page {index}"),
        };
        assert_eq!(text(0), "one");
        assert_eq!(text(1), "two");
        // Past the end wraps, so a module left pointing at a page a later fetch no longer
        // has still draws something.
        assert_eq!(text(2), "one");
    }

    /// A run that printed nothing is one empty reading, not no readings: a module with no
    /// pages at all would have nothing to draw and nothing left to click.
    #[test]
    fn a_source_that_said_nothing_still_has_a_page() {
        let mut registry = Registry::new(&HashMap::from([(Which::Audio, Duration::from_secs(1))]));
        registry.push(&Which::Audio, Vec::new());
        assert_eq!(registry.pages(&Which::Audio), 1);
        assert!(registry.page(&Which::Audio, 0).is_some());
    }

    fn scripted(answers: Vec<Option<&'static str>>) -> Registry {
        Registry {
            entries: vec![Entry {
                which: Which::Cpu,
                interval: Duration::from_secs(1),
                collector: Box::new(Scripted { answers }),
                readings: vec![Reading::default()],
                due: Some(Instant::now()),
                watched: false,
                failures: 0,
                reported: false,
                read_at: None,
            }],
        }
    }

    fn text_of(registry: &Registry) -> Option<String> {
        match registry.reading(&Which::Cpu)?.fields.get("text")? {
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
        registry.entries[0].due = Some(Instant::now());
        assert!(registry.tick());
        assert_eq!(
            text_of(&registry).as_deref(),
            Some("first"),
            "a momentary failure should not blank a module that was working"
        );
        assert_eq!(
            registry.reading(&Which::Cpu).map(|r| r.state),
            Some(State::Error)
        );
    }

    #[test]
    fn a_failing_collector_is_tried_less_and_less_often() {
        let mut registry = scripted(vec![None, None, None]);
        let mut waits = Vec::new();
        for _ in 0..3 {
            let before = Instant::now();
            registry.entries[0].due = Some(before);
            registry.tick();
            let due = registry.entries[0]
                .due
                .expect("a failing collector is tried again");
            waits.push(due.saturating_duration_since(before));
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
    fn a_watched_source_costs_no_wake_ups() {
        let mut registry = scripted(vec![Some("read once")]);
        registry.set_watched(&Which::Cpu, true);
        assert!(registry.tick(), "it is still read once to have a value");
        assert_eq!(
            registry.next_due(),
            None,
            "a watched source should be waited on rather than asked"
        );
        assert!(!registry.is_scheduled());
    }

    #[test]
    fn a_watched_source_that_fails_is_still_tried_again() {
        // Whatever the watcher is reporting, a collector that cannot read has to be
        // retried, or a sensor that came back would never be noticed.
        let mut registry = scripted(vec![None]);
        registry.set_watched(&Which::Cpu, true);
        registry.tick();
        assert!(registry.next_due().is_some());
    }

    #[test]
    fn losing_a_watch_puts_the_source_back_on_its_interval() {
        let mut registry = scripted(vec![Some("first"), Some("second")]);
        registry.set_watched(&Which::Cpu, true);
        registry.tick();
        assert!(!registry.is_scheduled());

        registry.set_watched(&Which::Cpu, false);
        assert!(registry.is_scheduled(), "it is due straight away");
        assert!(registry.tick());
        assert!(registry.next_due().is_some(), "and on its interval after");
    }

    /// A held file is read from the start every time, so it says what the file says now
    /// rather than what was left after the last read.
    #[test]
    fn a_held_file_is_read_again_from_the_start() {
        let path = std::env::temp_dir().join("dbar-pseudo-again");
        std::fs::write(&path, "first\n").expect("writable");
        let mut file = Pseudo::new(&path);
        assert_eq!(file.read().expect("reads"), "first\n");
        assert_eq!(file.read().expect("reads"), "first\n");
        std::fs::write(&path, "second, and longer\n").expect("writable");
        assert_eq!(file.read().expect("reads"), "second, and longer\n");
        std::fs::write(&path, "3\n").expect("writable");
        assert_eq!(file.read().expect("reads"), "3\n");
    }

    #[test]
    fn a_held_file_that_was_not_there_is_opened_once_it_is() {
        let path = std::env::temp_dir().join("dbar-pseudo-later");
        let _ = std::fs::remove_file(&path);
        let mut file = Pseudo::new(&path);
        let e = file.read().expect_err("nothing to read yet");
        assert!(format!("{e:#}").contains("dbar-pseudo-later"), "{e:#}");
        std::fs::write(&path, "here now\n").expect("writable");
        assert_eq!(file.read().expect("reads"), "here now\n");
    }

    #[test]
    fn a_held_file_longer_than_one_read_is_read_whole() {
        let path = std::env::temp_dir().join("dbar-pseudo-long");
        let text = "x".repeat(PSEUDO_FILE * 2 + 17);
        std::fs::write(&path, &text).expect("writable");
        assert_eq!(Pseudo::new(&path).read().expect("reads"), text);
    }

    #[test]
    #[ignore]
    fn benchmark_held_pseudo_files() {
        const N: u32 = 200_000;
        for path in ["/proc/stat", "/proc/meminfo", "/proc/loadavg"] {
            let start = Instant::now();
            for _ in 0..N {
                std::hint::black_box(read_to_string(path).expect("reads"));
            }
            let reopened = start.elapsed() / N;
            let mut held = Pseudo::new(path);
            let start = Instant::now();
            for _ in 0..N {
                std::hint::black_box(held.read().expect("reads").len());
            }
            let kept = start.elapsed() / N;
            println!("{path}: reopened {reopened:?} held {kept:?}");
        }
    }

    #[test]
    fn alignment_lands_inside_the_interval() {
        for secs in [1, 5, 60] {
            let interval = Duration::from_secs(secs);
            let wait = align(interval);
            assert!(
                wait > Duration::ZERO && wait <= interval + guard_for(interval),
                "for {secs}s"
            );
        }
    }

    /// A clock that is a whole minute wrong for a whole minute, which is what rounding a
    /// tiny remainder up to a full interval used to do.
    ///
    /// A timer aimed at the minute can fire a fraction of a millisecond before it. The
    /// reading taken then says the minute before, so the next one has to be taken at the
    /// boundary that is moments away - not at the one after it.
    #[test]
    fn a_reading_taken_a_hair_early_waits_for_the_boundary_it_missed() {
        let minute = Duration::from_secs(60);
        // Half a millisecond before a minute boundary.
        let hair = Duration::from_secs(600) - Duration::from_micros(500);
        let wait = align_from(hair, minute);
        assert!(
            wait < Duration::from_millis(10),
            "waited {wait:?} rather than crossing the boundary in front of it"
        );
        assert!(wait > Duration::ZERO, "and it is still in the future");
    }

    /// The shortest interval a config may ask for is a millisecond, which is shorter than
    /// the guard. A guard that outran its own interval would step over the boundaries it
    /// is there to land just after.
    #[test]
    fn the_guard_never_outruns_a_short_interval() {
        for (interval, longest) in [
            (Duration::from_millis(1), Duration::from_micros(250)),
            (Duration::from_millis(4), Duration::from_millis(1)),
            (Duration::from_millis(8), ALIGNMENT_GUARD),
            (Duration::from_secs(60), ALIGNMENT_GUARD),
        ] {
            assert_eq!(guard_for(interval), longest, "for {interval:?}");
            // However far into the interval the clock is, the wait still lands inside the
            // next one rather than past it.
            for offset in [0, 1, 7, 999_999] {
                let wait = align_from(
                    Duration::from_secs(600) + Duration::from_nanos(offset),
                    interval,
                );
                assert!(
                    wait <= interval + longest,
                    "{interval:?} at +{offset}ns waited {wait:?}"
                );
            }
        }
    }

    /// A machine that suspends stops the monotonic clock the deadline sits on but not the
    /// wall clock the reading is about, so a resume can find a clock module holding a
    /// minute that went by while it slept, with its deadline still in the future.
    #[test]
    fn a_clock_that_slept_through_its_minute_is_read_again() {
        let mut registry = Registry::new(&HashMap::from([(Which::Time, Duration::from_secs(60))]));
        registry.tick();
        let at = registry
            .entries
            .iter()
            .position(|e| e.which == Which::Time)
            .expect("the clock");

        let due = registry.entries[at].due.expect("a deadline");
        assert!(due > Instant::now(), "the deadline is still in front of it");
        assert!(
            !registry.entries[at].late(std::time::SystemTime::now()),
            "and the reading it holds is about the minute it is in"
        );

        // Waking an hour later, with that deadline still unreached: the same shape as a
        // reading taken an hour ago, which is what this stands in for.
        registry.entries[at].read_at =
            Some(std::time::SystemTime::now() - Duration::from_secs(3600));
        assert!(registry.entries[at].late(std::time::SystemTime::now()));
        assert!(
            registry.tick(),
            "so the tick reads it rather than waiting out a deadline from before the sleep"
        );
        // Both readings aim at the same boundary, so the deadline barely moves; what
        // matters is that the reading it now holds is about the minute the bar is in.
        assert!(
            registry.entries[at].due.expect("a new deadline") > Instant::now(),
            "and it is scheduled for a boundary still in front of it"
        );
        assert!(!registry.entries[at].late(std::time::SystemTime::now()));
    }

    /// Everywhere else the wait is the time left plus the guard, and a reading taken on the
    /// boundary itself waits for the next one rather than firing again immediately.
    #[test]
    fn alignment_aims_just_past_each_boundary() {
        let minute = Duration::from_secs(60);
        assert_eq!(
            align_from(Duration::from_secs(630), minute),
            Duration::from_secs(30) + guard_for(minute),
            "halfway through a minute"
        );
        assert_eq!(
            align_from(Duration::from_secs(600), minute),
            minute + guard_for(minute),
            "exactly on a boundary"
        );
        assert_eq!(
            align_from(Duration::from_secs(600) + Duration::from_millis(1), minute),
            minute - Duration::from_millis(1) + guard_for(minute),
            "a millisecond past one"
        );
    }
}
