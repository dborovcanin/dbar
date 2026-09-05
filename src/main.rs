//! dbar - a small, event-driven Wayland status bar.

mod app;
mod collect;
mod color;
mod config;
mod dbus;
mod format;
mod icon;
mod layout;
mod render;
mod signal;
mod status;
mod sway;
mod text;

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
        --print-config      Write the built-in default config to stdout
    -h, --help              Show this message
    -V, --version           Show the version

Without -c, dbar reads $XDG_CONFIG_HOME/dbar/config.toml and falls back to its
built-in defaults when that file does not exist.
";

struct Args {
    config: Option<PathBuf>,
}

fn parse_args() -> Result<Option<Args>> {
    let mut config = None;
    let mut args = std::env::args().skip(1);
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
            other => anyhow::bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
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

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp(None)
        .init();

    let Some(args) = parse_args()? else {
        return Ok(());
    };
    let config = Config::load(args.config.as_deref())?;

    let conn = Connection::connect_to_env().context("connecting to the Wayland compositor")?;
    let (globals, event_queue) =
        registry_queue_init::<App>(&conn).context("initializing the Wayland registry")?;
    let qh = event_queue.handle();

    let mut event_loop: EventLoop<App> = EventLoop::try_new().context("creating the event loop")?;
    let handle = event_loop.handle();

    // An external provider is started only when something in the config reads from one.
    // A native configuration runs no child process at all.
    let (status_tx, status_rx) = calloop::channel::channel();
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
    };
    let collectors = !config_collectors.is_empty();
    let listening = provider.is_some();
    // A signal brings a reading forward: after `brightnessctl set`, the bar should say so
    // now rather than when the interval next comes round.
    let offsets: Vec<i32> = config.signals().keys().copied().collect();
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
        let covered = crate::collect::watch::spawn(watch_tx);
        if !covered.is_empty() {
            app.on_watching(&covered);
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
        let (tx, rx) = calloop::channel::channel();
        if let Err(e) = crate::collect::command::spawn(spec.argv.clone(), spec.run, spec.fields, tx)
        {
            log::error!("{e:#}");
            continue;
        }
        let which = which.clone();
        handle
            .insert_source(rx, move |event, _, app: &mut App| {
                if let calloop::channel::Event::Msg(reading) = event {
                    app.on_command(&which, reading);
                }
            })
            .map_err(|e| anyhow::anyhow!("inserting a command source: {e}"))?;
    }

    let (sway_tx, sway_rx) = calloop::channel::channel();
    match crate::sway::spawn(sway_tx, watching) {
        Ok(()) => {
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

    WaylandSource::new(conn, event_queue)
        .insert(handle)
        .map_err(|e| anyhow::anyhow!("inserting the Wayland source: {e}"))?;

    while !app.exit {
        event_loop
            .dispatch(None, &mut app)
            .context("dispatching events")?;
        // Anything the handlers marked dirty but could not draw yet gets drawn here.
        app.draw_if_needed();
    }

    Ok(())
}
