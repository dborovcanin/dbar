//! dbar - a small, event-driven Wayland status bar.

mod app;
mod collect;
mod color;
mod config;
mod dbus;
mod format;
mod geometry;
mod icon;
mod layout;
mod lines;
mod proc;
mod render;
mod signal;
mod status;
mod sway;
mod text;
mod tray;
mod worker;

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use calloop::EventLoop;
use calloop_wayland_source::WaylandSource;
use wayland_client::{Connection, globals::registry_queue_init};

use crate::app::App;
use crate::config::Config;
use crate::status::I3BarProvider;

const USAGE: &str = "\
dbar - a small Wayland status bar

USAGE:
    dbar [OPTIONS]

OPTIONS:
    -c, --config <PATH>     Use this config file instead of the default
        --check-config      Read the config, report what is wrong with it, and exit
        --fields [SOURCE]   List what each source publishes, or just this one
        --print-config      Write the built-in default config to stdout
    -h, --help              Show this message
    -V, --version           Show the version

Without -c, dbar reads $XDG_CONFIG_HOME/dbar/config.toml and falls back to its
built-in defaults when that file does not exist.
";

/// How many updates from somebody else's program may be waiting for the bar at once.
///
/// A script and an i3bar provider both send on their own thread, and the bar draws on
/// this one. Left unbounded, a program printing faster than the bar can draw would queue
/// every one of those updates - memory the bar cannot get back, spent on states nobody
/// will ever see, and a bar working through a backlog rather than showing what is true
/// now. A full queue blocks the sender instead, which is backpressure the program itself
/// feels: it is the shape a pipe already has.
///
/// A handful, because everything past the newest update is going to be drawn over anyway;
/// the depth is only there so an ordinary burst is not paced by the frame rate.
const QUEUED_UPDATES: usize = 8;

struct Args {
    config: Option<PathBuf>,
}

/// Read the config the way a run would, and say whether it is any good.
///
/// Nothing else starts: no Wayland connection, no collectors, no child processes. A
/// config is checked where it is written - in an editor, over ssh, in whatever runs
/// before the session does - and none of those have a compositor to hand.
fn check_config(path: Option<&PathBuf>) -> Result<()> {
    let config = Config::load(path.map(PathBuf::as_path))?;
    let modules = config.modules().count();
    let groups: usize = config.positions.iter().map(|p| p.groups.len()).sum();
    println!("the config is good: {modules} modules in {groups} groups");
    Ok(())
}

/// Print what each source publishes, which is what a format may name.
///
/// The fields are already declared - it is how a format is checked when the config is
/// read - so this is the same list the error message would have quoted, asked for before
/// making the mistake rather than after.
fn print_fields(only: Option<&str>) -> Result<()> {
    use std::io::Write as _;

    let sources = config::sources();
    if let Some(name) = only
        && !sources.iter().any(|(source, _)| *source == name)
    {
        anyhow::bail!(
            "no source is called {name:?}; there is {}",
            sources
                .iter()
                .map(|(source, _)| *source)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Written rather than printed, because this is a list somebody pipes into `less` or
    // `head`, and a closed pipe is that person having read enough rather than an error.
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let written = (|| -> std::io::Result<()> {
        for (name, source) in sources {
            if only.is_some_and(|wanted| wanted != name) {
                continue;
            }
            writeln!(out, "{name}")?;
            let fields = source.fields();
            if fields.is_empty() {
                writeln!(out, "    (nothing; what it publishes the config declares)")?;
            }
            for field in fields {
                writeln!(out, "    ${:<16}{}", field.name, describe_kind(field.kind))?;
            }
            writeln!(out)?;
        }
        Ok(())
    })();
    match written {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other.context("writing the field list"),
    }
}

/// What a field holds, in the words a config would use for it.
fn describe_kind(kind: crate::status::Kind) -> &'static str {
    use crate::status::{Kind, Unit};
    match kind {
        Kind::Num(Unit::None) => "number",
        Kind::Num(Unit::Percent) => "percent",
        Kind::Num(Unit::Bytes) => "bytes",
        Kind::Num(Unit::BytesPerSec) => "bytes per second",

        Kind::Num(Unit::Celsius) => "degrees celsius",
        Kind::Num(Unit::Watts) => "watts",

        Kind::Text => "text",
        Kind::Time => "a moment, for .time()",
        Kind::Dur => "a length of time, for .dur()",
        Kind::Flag => "true or false",
    }
}

fn parse_args() -> Result<Option<Args>> {
    let mut config = None;
    let mut check = false;
    let mut fields: Option<Option<String>> = None;
    let mut args = std::env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("dbar {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--print-config" => {
                print!("{}", config::DEFAULT_CONFIG);
                return Ok(None);
            }
            "-c" | "--config" => {
                let path = args.next().context("-c/--config needs a path")?;
                config = Some(PathBuf::from(path));
            }
            "--check-config" => check = true,
            // The source is optional, so the next argument is looked at before it is
            // taken: one that starts with a dash is the next option rather than a source
            // nobody would name that way, and it is left where it is to be parsed as one.
            "--fields" => {
                let named = match args.peek() {
                    Some(next) if !next.starts_with('-') => args.next(),
                    _ => None,
                };
                fields = Some(named);
            }
            other => anyhow::bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }

    // Each of these ends the run having answered one question, so being asked two is a
    // mistake worth naming rather than one of them silently going unanswered.
    if check && fields.is_some() {
        anyhow::bail!("--check-config and --fields do different things\n\n{USAGE}");
    }
    if let Some(only) = fields {
        print_fields(only.as_deref())?;
        return Ok(None);
    }
    if check {
        check_config(config.as_ref())?;
        return Ok(None);
    }
    Ok(Some(Args { config }))
}

/// Set the shared collector timer going.
///
/// It reads everything that has come due and asks for the next deadline, and stops
/// altogether when nothing is left to read on a schedule.
fn schedule(handle: &calloop::LoopHandle<'static, App>) -> Result<()> {
    handle
        .insert_source(
            calloop::timer::Timer::immediate(),
            |_, _, app: &mut App| match app.on_collect() {
                Some(next) => calloop::timer::TimeoutAction::ToInstant(next),
                None => calloop::timer::TimeoutAction::Drop,
            },
        )
        .map_err(|e| anyhow::anyhow!("inserting the collector timer: {e}"))?;
    Ok(())
}

/// Start the timer that turns the spinners, if one is not already going.
///
/// Separate from the collector timer on purpose: this one exists only while a command is
/// out, and drops itself the moment the last answer arrives. Putting the two together
/// would mean a bar that animates is a bar that reads its collectors at animation rate.
fn schedule_spin(handle: &calloop::LoopHandle<'static, App>) -> Result<()> {
    handle
        .insert_source(
            calloop::timer::Timer::from_duration(std::time::Duration::from_millis(0)),
            |_, _, app: &mut App| match app.on_spin() {
                Some(next) => calloop::timer::TimeoutAction::ToInstant(next),
                None => calloop::timer::TimeoutAction::Drop,
            },
        )
        .map_err(|e| anyhow::anyhow!("inserting the spinner timer: {e}"))?;
    Ok(())
}

/// Start the timer that moves a fold along, if one is not already going.
///
/// Its own timer for the same reason the spinner has one: it exists only while an island
/// is travelling and drops itself the moment the last one arrives, so a bar nobody is
/// clicking on never wakes for it.
fn schedule_fold(handle: &calloop::LoopHandle<'static, App>) -> Result<()> {
    handle
        .insert_source(
            calloop::timer::Timer::from_duration(std::time::Duration::from_millis(0)),
            |_, _, app: &mut App| match app.on_fold() {
                Some(next) => calloop::timer::TimeoutAction::ToInstant(next),
                None => calloop::timer::TimeoutAction::Drop,
            },
        )
        .map_err(|e| anyhow::anyhow!("inserting the fold timer: {e}"))?;
    Ok(())
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp(None)
        .init();

    let Some(args) = parse_args()? else {
        return Ok(());
    };
    let config = Config::load(args.config.as_deref())?;

    let conn = Connection::connect_to_env().context("connecting to the Wayland compositor")?;
    let (globals, mut event_queue) =
        registry_queue_init::<App>(&conn).context("initializing the Wayland registry")?;
    let qh = event_queue.handle();

    // Everything dbar started stops when this goes, which is on every way out of here: the
    // loop ending, and an error before or after it.
    let _stop = crate::proc::StopEverything;

    let mut event_loop: EventLoop<App> = EventLoop::try_new().context("creating the event loop")?;
    let handle = event_loop.handle();

    // An external provider is started only when something in the config reads from one.
    // A native configuration runs no child process at all.
    let (status_tx, status_rx) = calloop::channel::sync_channel(QUEUED_UPDATES);
    let provider = if config.needs_provider() {
        Some(I3BarProvider::spawn(&config.i3bar, status_tx)?)
    } else {
        log::info!("no module reads from a status provider, so none is started");
        None
    };

    let config_collectors = config.collectors();
    // Read before the config is handed to the app, which is what owns it from here on.
    let watching = crate::sway::Watching {
        language: config.needs_language(),
        mode: config.needs_mode(),
        windows: config.needs_windows(),
        workspaces: config.needs_workspaces(),
    };
    let collectors = !config_collectors.is_empty();
    let listening = provider.is_some();
    // A signal brings a reading forward: after `brightnessctl set`, the bar should say so
    // now rather than when the interval next comes round.
    let offsets: Vec<i32> = config.signals().keys().copied().collect();
    // Which sources a click or a signal can ask for another reading, so a command that
    // nothing can ask keeps no thread waiting to be asked.
    let askable = config.refreshable();
    // Read before the config is handed over: a tray icon is drawn at the size the bar uses
    // for every other icon, and resolved once at that size rather than per frame.
    let tray_wanted = config.needs_tray();
    let tray_size = config.bar.icon_size.round().max(1.0) as u32;
    let tray_theme = config.bar.icon_theme.clone();
    let mut app = App::new(&globals, &qh, conn.clone(), config, provider)?;

    if listening {
        handle
            .insert_source(status_rx, |event, _, app: &mut App| {
                if let calloop::channel::Event::Msg(event) = event {
                    app.on_status(event);
                }
            })
            .map_err(|e| anyhow::anyhow!("inserting the status source: {e}"))?;
    }

    let (signal_tx, signal_rx) = calloop::channel::channel();
    crate::signal::spawn(&offsets, signal_tx)?;
    if !offsets.is_empty() {
        handle
            .insert_source(signal_rx, |event, _, app: &mut App| {
                if let calloop::channel::Event::Msg(offset) = event {
                    app.on_signal(offset);
                }
            })
            .map_err(|e| anyhow::anyhow!("inserting the signal source: {e}"))?;
    }

    // The volume is not read at all: PipeWire says when it moves, from a thread of its
    // own, and the reading arrives here finished.
    if config_collectors.contains_key(&crate::collect::Which::Audio) {
        let (audio_tx, audio_rx) = calloop::channel::channel();
        let commands = crate::collect::audio::spawn(audio_tx)?;
        app.set_audio(commands);
        handle
            .insert_source(audio_rx, |event, _, app: &mut App| {
                if let calloop::channel::Event::Msg(reading) = event {
                    app.on_audio(reading);
                }
            })
            .map_err(|e| anyhow::anyhow!("inserting the audio source: {e}"))?;
    }

    // What is playing arrives from the session bus, on a thread of its own for the same
    // reason: a bus connection blocks, and the bar must not.
    if config_collectors.contains_key(&crate::collect::Which::Media) {
        let (media_tx, media_rx) = calloop::channel::channel();
        match crate::collect::media::spawn(media_tx) {
            Ok(commands) => {
                app.set_media(commands);
                handle
                    .insert_source(media_rx, |event, _, app: &mut App| {
                        if let calloop::channel::Event::Msg(reading) = event {
                            app.on_media(reading);
                        }
                    })
                    .map_err(|e| anyhow::anyhow!("inserting the media source: {e}"))?;
            }
            Err(e) => log::warn!("what is playing is unavailable: {e:#}"),
        }
    }

    // Sources the kernel reports changes on are read when they change and never in
    // between, so they are taken off the timer before it is first set.
    if collectors {
        let (watch_tx, watch_rx) = calloop::channel::channel();
        let asked: Vec<crate::collect::Which> = config_collectors.keys().cloned().collect();
        let watching = crate::collect::watch::spawn(watch_tx, &asked);
        if watching.running {
            app.on_watching(&watching.covered);
            let handle_for_timer = handle.clone();
            handle
                .insert_source(watch_rx, move |event, _, app: &mut App| {
                    let calloop::channel::Event::Msg(event) = event else {
                        return;
                    };
                    // A source whose watch has gone needs its interval back, and with it a
                    // timer, which has stopped if everything left was being watched.
                    if app.on_watch(event)
                        && let Err(e) = schedule(&handle_for_timer)
                    {
                        log::error!("{e}");
                    }
                })
                .map_err(|e| anyhow::anyhow!("inserting the watch source: {e}"))?;
        }
    }

    // A source whose read can wait on something outside this machine - a filesystem that
    // has stopped answering, a wireless driver - is read on a thread of its own, so a
    // mount that hangs cannot take the clock and the pointer down with it.
    let slow: Vec<crate::collect::Which> = config_collectors
        .keys()
        .filter(|which| which.blocking())
        .cloned()
        .collect();
    if !slow.is_empty() {
        let (slow_tx, slow_rx) = calloop::channel::sync_channel(QUEUED_UPDATES);
        for which in slow {
            let interval = config_collectors[&which];
            let askable = askable.contains(&which);
            if let Some(trigger) =
                crate::collect::slow::spawn(which.clone(), interval, askable, slow_tx.clone())
            {
                app.set_trigger(which, trigger);
            }
        }
        handle
            .insert_source(slow_rx, |event, _, app: &mut App| {
                if let calloop::channel::Event::Msg(taken) = event {
                    app.on_slow(taken);
                }
            })
            .map_err(|e| anyhow::anyhow!("inserting the slow-source channel: {e}"))?;
    }

    // Collectors share one timer: it fires when the earliest is due, reads everything that
    // has come due, and is set again for whatever is next.
    if collectors {
        schedule(&handle)?;
    }

    // The compositor is optional: without it the workspace and window modules simply have
    // nothing to show, and the rest of the bar is unaffected.
    // A command module runs a program of your own once and reads a line per reading, so it
    // costs a thread and no wake-ups rather than a process per tick.
    for which in config_collectors.keys() {
        let crate::collect::Which::Command(spec) = which else {
            continue;
        };
        let (tx, rx) = calloop::channel::sync_channel(QUEUED_UPDATES);
        let trigger = match crate::collect::command::spawn(
            spec.argv.clone(),
            spec.run,
            spec.fields,
            tx,
            askable.contains(which),
            spec.pages,
            spec.timeout,
        ) {
            Ok(trigger) => trigger,
            Err(e) => {
                log::error!("{e:#}");
                continue;
            }
        };
        if let Some(trigger) = trigger {
            app.set_trigger(which.clone(), trigger);
        }
        let which = which.clone();
        let handle_for_spin = handle.clone();
        handle
            .insert_source(rx, move |event, _, app: &mut App| {
                let calloop::channel::Event::Msg(message) = event else {
                    return;
                };
                match message {
                    crate::collect::command::Message::Started => {
                        if app.on_command_started(&which)
                            && let Err(e) = schedule_spin(&handle_for_spin)
                        {
                            log::error!("{e}");
                        }
                    }
                    crate::collect::command::Message::Readings(readings) => {
                        app.on_command(&which, readings)
                    }
                }
            })
            .map_err(|e| anyhow::anyhow!("inserting a command source: {e}"))?;
    }

    // The tray is started only when something on the bar draws one. Nothing else in dbar
    // takes a name on the bus that other programs look for, and taking that one without
    // drawing anything would leave applications registered with a bar that never shows
    // them, and keep a real tray from ever taking it.
    if tray_wanted {
        let (tray_tx, tray_rx) = calloop::channel::channel();
        match crate::tray::spawn(tray_tx, tray_size, tray_theme) {
            Ok(commands) => {
                app.set_tray(commands);
                handle
                    .insert_source(tray_rx, |event, _, app: &mut App| {
                        if let calloop::channel::Event::Msg(event) = event {
                            app.on_tray(event);
                        }
                    })
                    .map_err(|e| anyhow::anyhow!("inserting the tray source: {e}"))?;
            }
            Err(e) => log::warn!("the system tray is unavailable: {e:#}"),
        }
    }

    // The compositor is asked nothing at all by a bar that draws none of what it knows:
    // no sockets, no threads, and no tree read every time a window title changes.
    if !watching.anything() {
        log::info!("no module comes from the compositor, so it is not connected to");
    } else {
        let (sway_tx, sway_rx) = calloop::channel::channel();
        match crate::sway::spawn(sway_tx, watching) {
            Ok(()) => {
                // Clicking a workspace is the only thing that sends the compositor a
                // command, and it goes out on a thread of its own: a click must not wait
                // on the compositor, because the thread it arrives on is the one that
                // draws.
                if watching.workspaces {
                    app.set_sway_commands(crate::sway::commands());
                }
                handle
                    .insert_source(sway_rx, |event, _, app: &mut App| {
                        if let calloop::channel::Event::Msg(event) = event {
                            app.on_sway(event);
                        }
                    })
                    .map_err(|e| anyhow::anyhow!("inserting the sway source: {e}"))?;
            }
            Err(e) => log::warn!("compositor integration unavailable: {e}"),
        }
    }

    // Two rounds with the compositor before the loop starts: the first brings the outputs
    // and the second what each of them is called, which is what decides where a bar goes.
    // Judging a config's `outputs` before that would call a good one wrong for naming a
    // screen that had simply not arrived yet.
    for _ in 0..2 {
        event_queue
            .roundtrip(&mut app)
            .context("asking the compositor what screens there are")?;
    }
    app.warn_if_nowhere();
    // The compositor lists its buffer formats when wl_shm is bound, so this is the first
    // point at which the cheapest one can be chosen.
    app.choose_pixel_format();

    let handle_for_fold = handle.clone();
    WaylandSource::new(conn, event_queue)
        .insert(handle)
        .map_err(|e| anyhow::anyhow!("inserting the Wayland source: {e}"))?;

    while !app.exit {
        event_loop
            .dispatch(None, &mut app)
            .context("dispatching events")?;
        // A click is a Wayland event, and the pointer handler has no way to reach the
        // loop from inside a dispatch, so a fold it started is picked up here instead.
        if app.take_fold_timer()
            && let Err(e) = schedule_fold(&handle_for_fold)
        {
            log::error!("{e}");
        }
        // Anything the handlers marked dirty but could not draw yet gets drawn here.
        app.draw_if_needed();
    }

    Ok(())
}
