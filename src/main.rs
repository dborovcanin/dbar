//! dbar - a small, event-driven Wayland status bar.

mod app;
mod color;
mod config;
mod layout;
mod render;
mod status;
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

    // The provider's reader thread pushes updates in through this channel.
    let (status_tx, status_rx) = calloop::channel::channel();
    let provider = I3BarProvider::spawn(&config.status, status_tx)?;

    let mut app = App::new(&globals, &qh, conn.clone(), config, provider)?;

    handle
        .insert_source(status_rx, |event, _, app: &mut App| {
            if let calloop::channel::Event::Msg(event) = event {
                app.on_status(event);
            }
        })
        .map_err(|e| anyhow::anyhow!("inserting the status source: {e}"))?;

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
