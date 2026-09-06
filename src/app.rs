//! Wayland layer-shell surface, event wiring and redraw scheduling.
//!
//! The bar is strictly event-driven: a redraw is queued by setting `dirty`, and is throttled to
//! the compositor's pace by a frame callback. With nothing happening, no work is done at all.

use anyhow::{Context as _, Result};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData},
    delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        pointer::{AxisScroll, PointerEvent, PointerEventKind, PointerHandler},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use wayland_client::{
    Connection, QueueHandle,
    globals::GlobalList,
    protocol::{wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
};

use crate::collect::{Registry, Which, watch};
use crate::config::{BarLayer, Button, Config, Edge};
use crate::layout::{self, Frame, Inputs, PlacedModule};
use crate::render;
use crate::status::{
    ActionTarget, ClickEvent, Control, I3BarProvider, StatusEvent, StatusItem, i3bar,
};
use crate::sway::{self, SwayEvent, SwayState};
use crate::text::TextRenderer;

/// How the i3bar protocol numbers a wheel notch, which is what click dispatch speaks.
const SCROLL_UP: u32 = 4;
const SCROLL_DOWN: u32 = 5;

/// Linux input button codes, as delivered by `wl_pointer`.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

/// How long a command's program runs before the bar says out loud that it is waiting.
///
/// Long enough that the scripts which answer straight away never animate at all, which is
/// what keeps a spinner from costing anything in the ordinary case, and short enough that
/// a slow one is admitted to before it reads as a bar that has hung.
const SPIN_AFTER: std::time::Duration = std::time::Duration::from_millis(400);
/// How long one step of a spinner's turn lasts, and so the only rate at which this bar
/// ever animates.
///
/// Sixteen a second is where the sweep stops reading as a sequence of positions. It is
/// also the whole price of the spinner, since every step is a redraw, which is why it is
/// paid only while a program is actually out and never at idle.
const SPIN_STEP: std::time::Duration = std::time::Duration::from_millis(60);

/// One bar: a layer surface on one screen, and what belongs to that surface rather than to
/// what is drawn on it.
///
/// Everything the bar shows is the app's and is shared by every screen; a bar keeps only
/// its own geometry, the frame it last laid out and where the pointer is over it.
struct Bar {
    output: wl_output::WlOutput,
    /// What the compositor calls this screen - "DP-1" - once it has said. Sway names its
    /// outputs the same way, which is how a bar knows which workspaces are its own.
    name: Option<String>,
    layer: LayerSurface,
    pool: SlotPool,
    /// The clip mask for this surface, which is the one thing the renderer keeps per screen.
    clip: render::Clip,
    frame: Frame,

    /// Surface size in logical pixels.
    width: u32,
    height: u32,
    scale: i32,

    /// Pointer position in surface coordinates, while it is over this bar.
    pointer_at: Option<(f32, f32)>,
    /// Scrolling not yet worth a step, in steps.
    ///
    /// A wheel notch and a finger on a touchpad both arrive as a stream of small amounts,
    /// and acting on each one turns a flick of the wrist into thirty adjustments. What is
    /// left over is carried to the next event so slow scrolling still gets there.
    scrolled: f64,
    configured: bool,
    dirty: bool,
    frame_pending: bool,
}

impl Bar {
    /// Put a bar on one screen.
    ///
    /// The surface is bound to that output rather than left to the compositor's choice, so
    /// two bars cannot end up on the same screen with nothing on the other.
    fn new(
        app: &App,
        qh: &QueueHandle<App>,
        output: wl_output::WlOutput,
        name: Option<String>,
        width: u32,
        scale: i32,
    ) -> Result<Bar> {
        let cfg = &app.config.bar;
        let stack = match cfg.layer {
            BarLayer::Background => Layer::Background,
            BarLayer::Bottom => Layer::Bottom,
            BarLayer::Top => Layer::Top,
            BarLayer::Overlay => Layer::Overlay,
        };

        let surface = app.compositor.create_surface(qh);
        let layer =
            app.layer_shell
                .create_layer_surface(qh, surface, stack, Some("dbar"), Some(&output));

        let edge = match cfg.position {
            Edge::Top => Anchor::TOP,
            Edge::Bottom => Anchor::BOTTOM,
        };
        layer.set_anchor(edge | Anchor::LEFT | Anchor::RIGHT);
        // A zero width lets the compositor stretch us between the left and right anchors.
        layer.set_size(0, cfg.height);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        let m = cfg.margin;
        match cfg.position {
            Edge::Top => layer.set_margin(m, m, 0, m),
            Edge::Bottom => layer.set_margin(0, m, m, m),
        }
        layer.set_exclusive_zone(if cfg.exclusive {
            cfg.height as i32 + cfg.margin
        } else {
            0
        });
        // The first commit must carry no buffer; the compositor answers with a configure.
        layer.commit();

        // Only a starting size: the pool grows itself when a buffer does not fit, so a
        // guess costs a resize at worst and an exactly-sized screen costs nothing.
        let bytes = (width as usize * scale.max(1) as usize).max(1)
            * cfg.height as usize
            * scale.max(1) as usize
            * 4;
        let pool = SlotPool::new(bytes, &app.shm).context("creating the shm pool")?;

        Ok(Bar {
            output,
            name,
            layer,
            pool,
            clip: render::Clip::default(),
            frame: Frame::default(),
            width: 0,
            height: cfg.height,
            scale: scale.max(1),
            pointer_at: None,
            scrolled: 0.0,
            configured: false,
            dirty: true,
            frame_pending: false,
        })
    }
}

pub struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    compositor: CompositorState,
    layer_shell: LayerShell,
    /// One per screen the config asks for, in the order the compositor announced them.
    bars: Vec<Bar>,
    conn: Connection,
    qh: QueueHandle<App>,

    config: Config,
    /// What the renderer keeps between frames: the text backend and its spare layer.
    painter: render::Painter,
    /// The external status provider, when the config asks for one.
    provider: Option<I3BarProvider>,
    items: Vec<StatusItem>,
    /// What dbar measures for itself.
    native: Registry,
    /// Modules showing their second wording, by name.
    alt: std::collections::HashMap<String, usize>,
    /// Which page each module is scrolled to, by name, for a source that says several
    /// things at once - the weather in three cities from one fetch.
    pages: std::collections::HashMap<String, usize>,
    /// Programs started by a click, kept only until they have been reaped.
    children: Vec<std::process::Child>,
    /// Modules a right click has folded down to their icon, by name.
    collapsed: std::collections::HashSet<String>,
    /// Which sources each realtime signal reads again.
    signals: std::collections::HashMap<i32, Vec<Which>>,
    /// The way to ask a command module's program for another reading, by source.
    triggers: std::collections::HashMap<Which, crate::collect::command::Trigger>,
    /// Command sources with a run on its way, and when that run started.
    ///
    /// A command that answers quickly is in here for a few milliseconds and never draws
    /// anything, which is the point: the spinner is what a slow command gets, and a fast
    /// one costs nothing to have waited.
    running: std::collections::HashMap<Which, std::time::Instant>,
    /// Command sources that have been waiting long enough to be drawing a spinner.
    waiting: std::collections::HashSet<Which>,
    /// Which step of its turn the spinner is on, and whether a timer is driving it.
    spin: usize,
    spin_scheduled: bool,
    /// Whether a timer is waiting to read collectors. False once every source left is
    /// watched, since then there is nothing to wait for.
    collect_scheduled: bool,
    /// The way into the PipeWire thread, when a module can change the volume.
    audio: Option<crate::collect::audio::Commands>,
    /// The way into the media thread, when a module can operate a player.
    media: Option<crate::collect::media::Commands>,
    /// Whether a failed control has already been reported, so a scroll logs once.
    control_warned: bool,
    /// Workspaces and the focused window, when a compositor is talking to us.
    sway: SwayState,
    /// Set when the status provider itself has failed; shown in place of the groups.
    fault: Option<String>,
    /// Item names from the last "nothing matched" warning, so it is not repeated per redraw.
    warned_names: Option<Vec<String>>,
    /// Whether the block-count mismatch has already been reported.
    name_count_warned: bool,
    /// Whether a config that names screens which are not here has been reported.
    no_output_warned: bool,

    pointer: Option<wl_pointer::WlPointer>,
    pub exit: bool,
}

impl App {
    pub fn new(
        globals: &GlobalList,
        qh: &QueueHandle<App>,
        conn: Connection,
        config: Config,
        provider: Option<I3BarProvider>,
    ) -> Result<App> {
        let compositor =
            CompositorState::bind(globals, qh).context("wl_compositor is not available")?;
        let layer_shell = LayerShell::bind(globals, qh)
            .context("zwlr_layer_shell_v1 is not available; is this a wlroots compositor?")?;
        let shm = Shm::bind(globals, qh).context("wl_shm is not available")?;

        let config_collectors = config.collectors();
        let config_signals = config.signals();
        let bar = &config.bar;
        let text = TextRenderer::new(&bar.font_family, bar.font_size, &bar.font_fallback)?;

        Ok(App {
            registry_state: RegistryState::new(globals),
            seat_state: SeatState::new(globals, qh),
            output_state: OutputState::new(globals, qh),
            shm,
            compositor,
            layer_shell,
            bars: Vec::new(),
            conn,
            qh: qh.clone(),
            config,
            painter: render::Painter::new(text),
            native: Registry::new(&config_collectors),
            alt: std::collections::HashMap::new(),
            pages: std::collections::HashMap::new(),
            children: Vec::new(),
            collapsed: std::collections::HashSet::new(),
            signals: config_signals,
            triggers: std::collections::HashMap::new(),
            running: std::collections::HashMap::new(),
            waiting: std::collections::HashSet::new(),
            spin: 0,
            spin_scheduled: false,
            collect_scheduled: true,
            audio: None,
            media: None,
            control_warned: false,
            provider,
            items: Vec::new(),
            sway: SwayState::default(),
            fault: None,
            warned_names: None,
            name_count_warned: false,
            no_output_warned: false,
            pointer: None,
            exit: false,
        })
    }

    /// Handle one message from the status provider.
    pub fn on_status(&mut self, event: StatusEvent) {
        match event {
            StatusEvent::Header(header) => {
                log::debug!(
                    "status header: version {}, clicks {}",
                    header.version,
                    header.click_events
                );
                if let Some(provider) = self.provider.as_mut() {
                    provider.set_accepts_clicks(header.click_events);
                }
            }
            StatusEvent::Blocks(blocks) => {
                self.warn_about_names(&blocks);
                self.items = i3bar::to_items(&blocks, &self.config.i3bar.names);
                self.fault = None;
                self.invalidate();
            }
            StatusEvent::Stopped(reason) => {
                log::error!("status provider stopped: {reason}");
                self.items.clear();
                self.fault = Some(format!("status provider stopped: {reason}"));
                self.invalidate();
            }
        }
    }

    /// Handle one message from the compositor.
    pub fn on_sway(&mut self, event: SwayEvent) {
        match event {
            SwayEvent::State(state) => {
                self.sway = *state;
                self.invalidate();
            }
            SwayEvent::Stopped(reason) => {
                // The bar keeps working without the compositor; only its modules go quiet.
                log::warn!("sway IPC stopped: {reason}");
                self.sway = SwayState::default();
                self.invalidate();
            }
        }
    }

    /// Report a provider that sends a different number of blocks than the config names.
    ///
    /// The naming itself happens in the i3bar backend; this is only the explanation of why
    /// a config's names may not have taken effect.
    fn warn_about_names(&mut self, blocks: &[i3bar::I3BarBlock]) {
        let names = &self.config.i3bar.names;
        if names.is_empty() || blocks.len() == names.len() || self.name_count_warned {
            return;
        }
        if blocks.len() < names.len() {
            // Still starting up. The names are held back until the counts agree, because a
            // short list would show one block's value under another block's name.
            log::debug!(
                "provider sent {} block(s), {} named in the config; names are positional, so \
                 they are applied once the counts agree",
                blocks.len(),
                names.len()
            );
            return;
        }
        log::warn!(
            "provider sends {} block(s) but [i3bar] names {}; the extra blocks keep \
             the names the provider gave them",
            blocks.len(),
            names.len()
        );
        self.name_count_warned = true;
    }

    /// Remember how to ask a command module's program for another reading.
    pub fn set_trigger(&mut self, which: Which, trigger: crate::collect::command::Trigger) {
        self.triggers.insert(which, trigger);
    }

    /// Read again whatever a signal asks for.
    ///
    /// The timer that was already scheduled still fires at its old deadline; it finds
    /// nothing due by then and simply asks for the next one, so it corrects itself rather
    /// than needing to be rescheduled from here.
    pub fn on_signal(&mut self, offset: i32) {
        let Some(sources) = self.signals.get(&offset) else {
            return;
        };
        log::debug!(
            "SIGRTMIN+{offset}: reading {} source(s) again",
            sources.len()
        );
        for which in sources.clone() {
            self.refresh_source(&which);
        }
        self.collect();
    }

    /// Ask one source for a fresh reading, however that source is read.
    ///
    /// A collector is brought forward on the shared timer. A command is not read at all -
    /// its program runs on a thread of its own - so it is asked there instead, and the
    /// reading arrives the way every other one from it does.
    fn refresh_source(&mut self, which: &Which) {
        if let Some(trigger) = self.triggers.get(which) {
            trigger.ask();
            return;
        }
        self.native.refresh(which);
    }

    /// The source behind a module, by name.
    ///
    /// A click carries the module it landed on rather than what that module reads, since
    /// a source cloned into every module of every frame would be paid for on the path
    /// that runs forever.
    fn source_of(&self, module: &str) -> Option<Which> {
        self.config
            .modules()
            .find(|m| m.name == module)
            .and_then(|m| match &m.source {
                crate::config::Source::Native(which) => Some(which.clone()),
                _ => None,
            })
    }

    /// Read every collector that has come due, and say when the next one is.
    ///
    /// One pass over the whole set, then one redraw: ten modules sharing an interval cost
    /// one wake-up between them.
    pub fn on_collect(&mut self) -> Option<std::time::Instant> {
        let next = self.collect();
        // Only the timer's own callback can say whether the timer still exists, because
        // returning nothing from here is what stops it.
        self.collect_scheduled = next.is_some();
        next
    }

    /// Read what is due and redraw, without touching the timer.
    fn collect(&mut self) -> Option<std::time::Instant> {
        self.reap();
        if self.native.tick() {
            self.invalidate();
        }
        self.native.next_due()
    }

    /// Change what a module is showing, because someone scrolled or clicked on it.
    ///
    /// The bar does not record what it asked for. The brightness comes back through the
    /// watcher and the volume through PipeWire, so what is drawn is what the hardware
    /// accepted rather than what dbar hoped for.
    fn control(&mut self, what: Control, step: f64, button: u32) {
        // The i3bar numbering the pointer handler already speaks: 4 is a notch up, 5 a
        // notch down, 2 the middle button.
        let delta = match button {
            4 => step,
            5 => -step,
            _ => 0.0,
        };
        match (what, button) {
            // A player has buttons rather than a range: the left one plays and pauses,
            // and the wheel moves between tracks the way it moves through a playlist.
            (Control::Media, 1) => self.tell_media(crate::collect::media::Command::PlayPause),
            (Control::Media, 4) => self.tell_media(crate::collect::media::Command::Next),
            (Control::Media, 5) => self.tell_media(crate::collect::media::Command::Previous),
            (Control::Volume, 4 | 5) => {
                self.tell_audio(crate::collect::audio::Command::Volume(delta))
            }
            (Control::Brightness, 4 | 5) => {
                if let Err(e) = crate::collect::backlight::adjust(delta) {
                    // Once: a scroll is a dozen notches, and a dozen identical lines say
                    // nothing the first did not.
                    if !self.control_warned {
                        log::warn!("the brightness could not be changed: {e:#}");
                        self.control_warned = true;
                    }
                    return;
                }
                self.control_warned = false;
                // The watcher reports this a moment later anyway; asking now means the
                // number moves with the scroll rather than after it.
                self.native.refresh(&Which::Backlight);
                self.collect();
            }
            _ => {}
        }
    }

    fn tell_audio(&self, command: crate::collect::audio::Command) {
        if let Some(commands) = &self.audio
            && commands.send(command).is_err()
        {
            log::warn!("the volume could not be changed: PipeWire is not listening");
        }
    }

    fn tell_media(&self, command: crate::collect::media::Command) {
        match &self.media {
            Some(commands) => commands.send(command),
            None => log::debug!("no player to tell: the media module is not running"),
        }
    }

    /// Run a module's program, and clear away any that have already finished.
    ///
    /// The child is detached the moment it starts: a bar must not wait on a calendar the
    /// user is still reading, and it has nothing to say about what the program printed, so
    /// the three standard streams go nowhere.
    ///
    /// Reaping is done by hand rather than through SIGCHLD, and deliberately. Setting the
    /// signal to be ignored, or sweeping with `waitpid(-1)`, would take the exit status of
    /// every child in the process - and the i3bar provider and the `command` sources wait
    /// on theirs, which would then fail. Holding these handles reaps exactly what a click
    /// started and leaves the rest alone.
    fn run(&mut self, argv: &[String]) {
        self.reap();
        let Some((program, args)) = argv.split_first() else {
            return;
        };
        let spawned = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        match spawned {
            Ok(child) => {
                log::debug!("click ran {argv:?} as pid {}", child.id());
                self.children.push(child);
            }
            // A command that is not there is worth saying once per click rather than
            // taking the bar down: the rest of it still works.
            Err(e) => log::warn!("running {argv:?}: {e}"),
        }
    }

    /// Collect the children that have exited, so a click does not leave a zombie behind.
    fn reap(&mut self) {
        self.children
            .retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_)) | Err(_)));
    }

    /// Where to send what a click on a volume module asks for.
    pub fn set_audio(&mut self, commands: crate::collect::audio::Commands) {
        self.audio = Some(commands);
    }

    /// Where to send what a click on a media module asks for.
    pub fn set_media(&mut self, commands: crate::collect::media::Commands) {
        self.media = Some(commands);
    }

    /// Take what the session bus says is playing.
    pub fn on_media(&mut self, reading: crate::collect::Reading) {
        self.native.push(&Which::Media, vec![reading]);
        self.invalidate();
    }

    /// Take a reading a source pushed of its own accord, like the volume from PipeWire.
    pub fn on_audio(&mut self, reading: crate::collect::Reading) {
        self.native.push(&Which::Audio, vec![reading]);
        self.invalidate();
    }

    /// Take what a command of your own published, routed to the module that runs it: one
    /// reading, or a page each.
    pub fn on_command(&mut self, which: &Which, readings: Vec<crate::collect::Reading>) {
        // The answer is here, so whatever was said about waiting for it stops being said.
        // The timer finds nothing left to do on its next firing and drops itself.
        self.running.remove(which);
        self.waiting.remove(which);
        self.native.push(which, readings);
        self.invalidate();
    }

    /// Note that a command's program is running, and say whether a timer is now wanted.
    ///
    /// Nothing is drawn from here. The bar has no idea yet whether this run is one of the
    /// quick ones, and starting a spinner for a script that answers in ten milliseconds
    /// would be a flicker bought with a wake-up.
    pub fn on_command_started(&mut self, which: &Which) -> bool {
        self.running
            .insert(which.clone(), std::time::Instant::now());
        let needed = !self.spin_scheduled;
        self.spin_scheduled = true;
        needed
    }

    /// Advance the spinner if anything has been waiting long enough, and say when it is
    /// next wanted.
    ///
    /// Returning nothing stops the timer, which is what happens the moment the last
    /// command answers: a bar with nothing outstanding is back to costing nothing.
    pub fn on_spin(&mut self) -> Option<std::time::Instant> {
        let now = std::time::Instant::now();
        let was = self.waiting.len();
        self.waiting.clear();
        let mut soonest = None;
        for (which, started) in &self.running {
            match now.duration_since(*started) >= SPIN_AFTER {
                true => {
                    self.waiting.insert(which.clone());
                }
                // Not yet worth saying anything about, but worth waking for when it is.
                false => {
                    let due = *started + SPIN_AFTER;
                    soonest = Some(soonest.map_or(due, |s: std::time::Instant| s.min(due)));
                }
            }
        }
        if !self.waiting.is_empty() {
            self.spin = (self.spin + 1) % crate::icon::SPINNER_FRAMES;
        }
        // A spinner that has just appeared or just gone changes the bar as much as one
        // that has turned, so both are a redraw.
        if !self.waiting.is_empty() || was != self.waiting.len() {
            self.invalidate();
        }
        let next = match self.waiting.is_empty() {
            false => Some(now + SPIN_STEP),
            true => soonest,
        };
        self.spin_scheduled = next.is_some();
        next
    }

    /// Take the sources a watcher covers off the timer.
    pub fn on_watching(&mut self, covered: &[Which]) {
        for which in covered {
            self.native.set_watched(which, true);
        }
    }

    /// Act on what a watcher says, and report whether the timer has to be started again.
    ///
    /// A change is a reading brought forward; a lost watch puts its source back on its
    /// interval, which needs a timer if every remaining source was watched and the timer
    /// had therefore stopped.
    pub fn on_watch(&mut self, event: watch::Event) -> bool {
        match event {
            watch::Event::Changed(which) => {
                log::debug!("{} changed", which.name());
                self.native.refresh(&which);
            }
            watch::Event::Lost(which) => self.native.set_watched(&which, false),
        }
        self.collect();
        let needed = !self.collect_scheduled && self.native.is_scheduled();
        self.collect_scheduled |= needed;
        needed
    }

    /// Mark every bar as needing a redraw, and draw the ones the compositor is ready for.
    ///
    /// What changed is what the bars show, and they all show the same thing, so a reading
    /// arriving is a redraw on each screen. They are laid out separately because their
    /// widths differ, but the work of collecting was done once.
    fn invalidate(&mut self) {
        for bar in &mut self.bars {
            bar.dirty = true;
        }
        self.draw_if_needed();
    }

    /// Draw every bar that has something to draw and no frame callback outstanding.
    pub fn draw_if_needed(&mut self) {
        let mut drawn = false;
        for i in 0..self.bars.len() {
            let bar = &self.bars[i];
            if !bar.dirty || bar.frame_pending || !bar.configured || bar.width == 0 {
                continue;
            }
            match self.draw(i) {
                Ok(()) => {
                    self.bars[i].dirty = false;
                    drawn = true;
                }
                // Left dirty, so the next thing to happen tries it again.
                Err(e) => log::error!("draw failed: {e}"),
            }
        }
        if drawn {
            let _ = self.conn.flush();
        }
    }

    fn draw(&mut self, i: usize) -> Result<()> {
        let frame = self.lay_out(i);
        self.bars[i].frame = frame;

        let App {
            bars,
            painter,
            config,
            qh,
            ..
        } = self;
        let bar = &mut bars[i];
        log::debug!(
            "draw {}: {}x{} scale {}, {} groups, {} modules",
            bar.name.as_deref().unwrap_or("?"),
            bar.width,
            bar.height,
            bar.scale,
            bar.frame.groups.len(),
            bar.frame
                .groups
                .iter()
                .map(|g| g.modules.len())
                .sum::<usize>()
        );
        let scale = bar.scale.max(1);
        let pw = (bar.width * scale as u32) as i32;
        let ph = (bar.height * scale as u32) as i32;
        let stride = pw * 4;
        let (buffer, canvas) = bar
            .pool
            .create_buffer(pw, ph, stride, wl_shm::Format::Argb8888)
            .context("creating an shm buffer")?;

        render::render_to_buffer(
            render::Target {
                canvas,
                width: pw as u32,
                height: ph as u32,
                clip: &mut bar.clip,
            },
            config,
            &bar.frame,
            scale as f32,
            painter,
        )?;

        let surface = bar.layer.wl_surface();
        surface.set_buffer_scale(scale);
        surface.damage_buffer(0, 0, pw, ph);
        surface.frame(qh, FrameCallbackData(surface.clone()));
        bar.frame_pending = true;
        buffer.attach_to(surface).context("attaching the buffer")?;
        bar.layer.commit();
        Ok(())
    }

    /// Work out what one bar looks like at its own size and scale.
    ///
    /// Separate from drawing it because the warning below wants the whole app while the
    /// frame is being held, and because a bar's geometry is the only thing that differs:
    /// everything laid out here came from one round of collecting.
    fn lay_out(&mut self, i: usize) -> Frame {
        let App {
            bars,
            painter,
            config,
            fault,
            items,
            native,
            sway,
            alt,
            pages,
            collapsed,
            waiting,
            spin,
            ..
        } = self;
        let bar = &bars[i];
        painter.text.set_scale(bar.scale.max(1) as f32);
        let (width, height) = (bar.width as f32, bar.height as f32);
        if let Some(message) = fault {
            return layout::fault(message, width, height, &mut painter.text);
        }
        let inputs = Inputs {
            items,
            native,
            sway,
            alt,
            pages,
            collapsed,
            waiting,
            spin: *spin,
            output: bar.name.as_deref(),
        };
        let frame = layout::compute(
            config,
            &inputs,
            width,
            height,
            &mut painter.text,
            bar.pointer_at,
        );
        self.warn_if_nothing_matched(&frame);
        frame
    }

    /// Put a bar on a screen, if the config asks for one there.
    fn add_bar(&mut self, output: wl_output::WlOutput) {
        if self.bars.iter().any(|b| b.output == output) {
            return;
        }
        let info = self.output_state.info(&output);
        let name = info.as_ref().and_then(|i| i.name.clone());
        if !self.config.bar.shows_on(name.as_deref()) {
            log::debug!(
                "not showing on output {}: the config names other screens",
                name.as_deref().unwrap_or("?")
            );
            return;
        }
        // Only a hint for the first buffer, so an unknown width is not worth asking twice
        // about: the pool grows to whatever the compositor configures.
        let width = info
            .as_ref()
            .and_then(|i| i.logical_size)
            .map(|(w, _)| w.max(0) as u32)
            .unwrap_or(1920);
        let scale = info.as_ref().map_or(1, |i| i.scale_factor);
        match Bar::new(self, &self.qh, output, name.clone(), width, scale) {
            Ok(bar) => {
                log::info!("bar on output {}", name.as_deref().unwrap_or("?"));
                self.bars.push(bar);
                self.no_output_warned = false;
            }
            Err(e) => log::error!("no bar on output {}: {e:#}", name.as_deref().unwrap_or("?")),
        }
    }

    /// Take the bar off a screen that has gone, or that the config no longer wants.
    fn drop_bar(&mut self, output: &wl_output::WlOutput) {
        let Some(i) = self.bars.iter().position(|b| &b.output == output) else {
            return;
        };
        let bar = self.bars.remove(i);
        log::info!("bar off output {}", bar.name.as_deref().unwrap_or("?"));
        self.warn_if_nowhere();
    }

    /// Say once when a config names screens that are not here, which is otherwise a bar
    /// that simply never appears.
    ///
    /// Asked once the compositor has finished listing its outputs, and again whenever one
    /// goes: outputs are announced one at a time, and judging after the first would call a
    /// perfectly good config wrong because the screen it names had not arrived yet.
    pub fn warn_if_nowhere(&mut self) {
        if !self.bars.is_empty() || self.no_output_warned || self.config.bar.outputs.is_empty() {
            return;
        }
        let here: Vec<String> = self
            .output_state
            .outputs()
            .filter_map(|o| self.output_state.info(&o).and_then(|i| i.name))
            .collect();
        log::warn!(
            "no bar on any screen: [bar] outputs names {:?}, and the screens announced so \
             far are {:?}",
            self.config.bar.outputs,
            here
        );
        self.no_output_warned = true;
    }

    /// Which bar a surface belongs to.
    fn bar_of(&self, surface: &wl_surface::WlSurface) -> Option<usize> {
        self.bars
            .iter()
            .position(|b| b.layer.wl_surface() == surface)
    }

    /// Point out a group list that selects nothing, which would otherwise leave a blank bar
    /// with no explanation of why.
    fn warn_if_nothing_matched(&mut self, frame: &Frame) {
        let drawn: usize = frame.groups.iter().map(|g| g.modules.len()).sum();
        if drawn > 0 || self.items.is_empty() || !self.native.is_empty() {
            self.warned_names = None;
            return;
        }
        let names: Vec<String> = self
            .items
            .iter()
            .map(|i| i.id.clone().unwrap_or_else(|| "<unnamed>".to_string()))
            .collect();
        if self.warned_names.as_ref() == Some(&names) {
            return;
        }
        log::warn!(
            "no configured module matched any of the {} item(s) the status provider is \
             sending ({}); groups select items by name, and modules = [\"*\"] takes them all",
            names.len(),
            names.join(", ")
        );
        self.warned_names = Some(names);
    }

    /// Track the pointer, redrawing only when the module under it changes.
    ///
    /// Motion inside one module changes nothing that is drawn, and the bar is meant to sit
    /// idle, so a redraw per motion event would be wasted work.
    fn set_pointer(&mut self, i: usize, at: Option<(f32, f32)>) {
        let bar = &mut self.bars[i];
        let before = bar.frame.hover_key(bar.pointer_at);
        let after = bar.frame.hover_key(at);
        bar.pointer_at = at;
        if before != after {
            // Only the bar under the pointer has changed; the others are showing the same
            // thing they were.
            bar.dirty = true;
            self.draw_if_needed();
        }
    }

    fn on_click(&mut self, i: usize, x: f64, y: f64, button: u32) {
        let Some(i3_button) = i3bar_button(button) else {
            return;
        };
        self.dispatch_click(i, x, y, i3_button);
    }

    fn dispatch_click(&mut self, i: usize, x: f64, y: f64, button: u32) {
        let Some(module) = self.bars[i].frame.module_at(x as f32, y as f32) else {
            return;
        };
        // Everything the module has to say about this press, taken before anything is
        // done about it: the module is borrowed out of the frame, and acting needs the
        // whole bar.
        let what = gesture(module, button);
        let (mx, my, mw, mh) = (module.x, module.y, module.width, module.height);
        let named = module.name.clone();
        let action = module.action.clone();
        let argv = match what {
            Gesture::Run(button) => module
                .on_click
                .as_ref()
                .and_then(|actions| actions.for_button(button))
                .map(<[String]>::to_vec),
            _ => None,
        };

        match what {
            // A program of the user's own, which the config asked for outright.
            Gesture::Run(_) => {
                if let Some(argv) = argv {
                    self.run(&argv);
                }
            }
            // Asking the source for a fresh reading. A weather script fetched over the
            // network is the case: it is worth a click far more often than it is worth an
            // interval, and the click is what says the answer is wanted now.
            Gesture::Refresh => {
                if let Some(which) = named.as_deref().and_then(|name| self.source_of(name)) {
                    self.refresh_source(&which);
                    self.collect();
                }
            }
            // Turning the pages of a source that said several things at once, which is
            // how one weather module covers three cities.
            Gesture::Page { count, forward } => {
                let Some(name) = named else {
                    return;
                };
                let showing = self.pages.entry(name).or_insert(0);
                *showing = match forward {
                    true => (*showing + 1) % count,
                    false => (*showing + count - 1) % count,
                };
                self.invalidate();
            }
            // Moving on to the next wording, and round to the first again.
            Gesture::Alt(views) => {
                let Some(name) = named else {
                    return;
                };
                let showing = self.alt.entry(name).or_insert(0);
                *showing = (*showing + 1) % views;
                self.invalidate();
            }
            // Muting, which the sound server reports back the way it reports a volume
            // changed from anywhere else, so what is drawn is what actually happened.
            Gesture::Mute => self.tell_audio(crate::collect::audio::Command::ToggleMute),
            // Folding a module down to its icon, and unfolding it.
            Gesture::Collapse => {
                let Some(name) = named else {
                    return;
                };
                if !self.collapsed.remove(&name) {
                    self.collapsed.insert(name);
                }
                self.invalidate();
            }
            // Nothing on the module wanted the press, so whatever it is showing gets it.
            Gesture::Forward => {
                let Some(action) = action else {
                    return;
                };
                match action {
                    // A module backed by the compositor acts on its own rather than
                    // forwarding.
                    ActionTarget::Sway(command) => {
                        if button == Button::Left.number() {
                            sway::run_command(&command);
                        }
                    }
                    ActionTarget::Control { what, step } => self.control(what, step, button),
                    ActionTarget::I3Bar { name, instance } => {
                        let event = ClickEvent {
                            name: name.as_deref(),
                            instance: instance.as_deref(),
                            button,
                            x: x as i32,
                            y: y as i32,
                            relative_x: (x as f32 - mx) as i32,
                            relative_y: (y as f32 - my) as i32,
                            width: mw as i32,
                            height: mh as i32,
                        };
                        log::debug!(
                            "click button {button} on block {:?} instance {:?}",
                            event.name,
                            event.instance
                        );
                        if let Some(provider) = self.provider.as_mut() {
                            provider.send_click(&event);
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Wayland handlers
// ---------------------------------------------------------------------------

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        let Some(i) = self.bar_of(surface) else {
            return;
        };
        if new_factor.max(1) != self.bars[i].scale {
            self.bars[i].scale = new_factor.max(1);
            self.bars[i].dirty = true;
            self.draw_if_needed();
        }
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        if let Some(i) = self.bar_of(surface) {
            self.bars[i].frame_pending = false;
        }
        self.draw_if_needed();
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for App {
    /// One surface has been taken away, which is a screen going rather than the bar
    /// stopping: the rest keep drawing, and the connection ending is what ends dbar.
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        let output = self
            .bar_of(layer.wl_surface())
            .map(|i| self.bars[i].output.clone());
        if let Some(output) = output {
            self.drop_bar(&output);
        }
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let Some(i) = self.bar_of(layer.wl_surface()) else {
            return;
        };
        let bar = &mut self.bars[i];
        let (w, h) = configure.new_size;
        if w != 0 {
            bar.width = w;
        }
        if h != 0 {
            bar.height = h;
        }
        bar.configured = true;
        // A configure invalidates any pending frame callback expectation.
        bar.frame_pending = false;
        bar.dirty = true;
        self.draw_if_needed();
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            match self.seat_state.get_pointer(qh, &seat) {
                Ok(pointer) => self.pointer = Some(pointer),
                Err(e) => log::warn!("could not get a pointer: {e}"),
            }
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer
            && let Some(pointer) = self.pointer.take()
        {
            pointer.release();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            let Some(i) = self.bar_of(&event.surface) else {
                continue;
            };
            let (x, y) = event.position;
            match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    self.set_pointer(i, Some((x as f32, y as f32)));
                }
                PointerEventKind::Leave { .. } => self.set_pointer(i, None),
                PointerEventKind::Press { button, .. } => self.on_click(i, x, y, button),
                PointerEventKind::Axis { vertical, .. } => {
                    let steps = steps_of(&vertical, &mut self.bars[i].scrolled);
                    // i3bar encodes scroll as buttons 4 (up) and 5 (down).
                    let button = match steps.is_negative() {
                        true => SCROLL_UP,
                        false => SCROLL_DOWN,
                    };
                    for _ in 0..steps.unsigned_abs() {
                        self.dispatch_click(i, x, y, button);
                    }
                }
                _ => {}
            }
        }
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.add_bar(output);
    }

    /// A screen has changed: its name may have only just arrived, and with it the answer
    /// to whether this config wanted a bar there at all.
    fn update_output(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        let name = self.output_state.info(&output).and_then(|i| i.name);
        match self.bars.iter().position(|b| b.output == output) {
            Some(i) if self.config.bar.shows_on(name.as_deref()) => {
                if self.bars[i].name != name {
                    // The workspaces a bar lists follow its name, so a bar that has just
                    // learned one is showing the wrong screen's until it draws again.
                    self.bars[i].name = name;
                    self.bars[i].dirty = true;
                    self.draw_if_needed();
                }
            }
            Some(_) => self.drop_bar(&output),
            None => self.add_bar(output),
        }
    }

    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.drop_bar(&output);
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_registry!(App);
smithay_client_toolkit::delegate_dispatch2!(App);

/// Map a Linux button code onto the i3bar protocol's button numbering.
fn i3bar_button(code: u32) -> Option<u32> {
    match code {
        BTN_LEFT => Some(1),
        BTN_MIDDLE => Some(2),
        BTN_RIGHT => Some(3),
        _ => None,
    }
}

/// What a press or a wheel notch on a module means.
///
/// Deciding is kept apart from doing: what a gesture means is about the module the pointer
/// landed on and nothing else, so it can be checked without a compositor, while acting on
/// it needs the whole bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gesture {
    /// Run the program the config gave this button.
    Run(Button),
    /// Read the module's source again.
    Refresh,
    /// Turn to another of the readings its source published, of `count` in all.
    Page { count: usize, forward: bool },
    /// Move on to the next of `views` wordings.
    Alt(usize),
    /// Fold the module down to its icon, or unfold it.
    Collapse,
    /// Mute what the module is showing, or unmute it.
    Mute,
    /// Nothing here wanted it; whatever the module is showing gets it.
    Forward,
}

/// What this button does on this module.
///
/// A program of the user's own comes first: it is the one thing on a module that the
/// config asked for outright, so nothing built in may quietly take the button out from
/// under it. Two claims on one button never reach here - that is a startup error - so the
/// order below only decides between things that cannot collide.
fn gesture(module: &PlacedModule, button: u32) -> Gesture {
    if let Some(pressed) = button_of(button)
        && module
            .on_click
            .as_ref()
            .is_some_and(|actions| actions.for_button(pressed).is_some())
    {
        return Gesture::Run(pressed);
    }
    // Muting comes before the name test: it acts on the sound server rather than on
    // anything the bar remembers against a module, so a volume module that is otherwise
    // anonymous still answers to it.
    if let Some(mute) = module.mute
        && button == mute.number()
    {
        return Gesture::Mute;
    }
    // The rest are remembered against the module by name, so a module the frame did not
    // name has nothing here to do.
    if module.name.is_none() {
        return Gesture::Forward;
    }
    if let Some(refresh) = module.refresh
        && button == refresh.number()
    {
        return Gesture::Refresh;
    }
    // The wheel is not a button, so paging takes nothing away from what the presses do; a
    // module whose source published one reading has nothing to turn and lets it through.
    if let Some(count) = module.paged
        && matches!(button, SCROLL_UP | SCROLL_DOWN)
    {
        return Gesture::Page {
            count: count.max(1),
            forward: button == SCROLL_DOWN,
        };
    }
    if let Some(views) = module.alt
        && button == module.alt_button.number()
    {
        return Gesture::Alt(views.max(1));
    }
    if module.collapsible && button == module.collapse_button.number() {
        return Gesture::Collapse;
    }
    Gesture::Forward
}

/// Map the i3bar protocol's numbering back onto the buttons a config can name.
///
/// Scroll notches arrive here as 4 and 5 and have no name in the config, because a notch
/// is a step in a direction rather than a press.
fn button_of(number: u32) -> Option<Button> {
    match number {
        1 => Some(Button::Left),
        2 => Some(Button::Middle),
        3 => Some(Button::Right),
        _ => None,
    }
}

/// How many whole steps a scroll event is worth, carrying the remainder in `carried`.
///
/// The three ways a compositor can describe scrolling all reduce to steps, because a step
/// is what the bar acts on. `value120` is what a modern one sends and counts 120 to a
/// notch, however finely the wheel itself reports; `discrete` is the older whole-notch
/// form; and a touchpad has neither, so its pixels are divided by how far a finger should
/// travel for one step.
///
/// Acting on every event instead is what made a flick of the wrist thirty adjustments: a
/// high-resolution wheel sends eight events to the notch and a touchpad sends a stream of
/// fractions. What does not reach a whole step is carried, so slow scrolling still arrives,
/// and it is dropped when the direction changes so a scroll back does not start part-way.
fn steps_of(axis: &AxisScroll, carried: &mut f64) -> i32 {
    /// What `value120` counts to one notch.
    const WHEEL_NOTCH: f64 = 120.0;
    /// How far a finger travels for one step, in the pixels a touchpad reports.
    const TOUCHPAD_STEP: f64 = 15.0;

    let moved = if axis.value120 != 0 {
        f64::from(axis.value120) / WHEEL_NOTCH
    } else if axis.discrete != 0 {
        f64::from(axis.discrete)
    } else {
        axis.absolute / TOUCHPAD_STEP
    };

    if axis.stop || moved == 0.0 {
        *carried = 0.0;
        return 0;
    }
    if *carried != 0.0 && carried.is_sign_negative() != moved.is_sign_negative() {
        *carried = 0.0;
    }

    *carried += moved;
    let whole = carried.trunc();
    *carried -= whole;
    whole as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ClickActions;

    /// A placed module with nothing on it, for saying what one gesture key does without
    /// describing a whole bar.
    fn placed() -> PlacedModule {
        PlacedModule {
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
            icon: None,
            text: String::new(),
            text_x: 0.0,
            foreground: crate::color::Color::TRANSPARENT,
            background: crate::color::Color::TRANSPARENT,
            radius: 0.0,
            action: None,
            name: Some("weather".to_string()),
            alt: None,
            alt_button: Button::Left,
            refresh: None,
            mute: None,
            paged: None,
            collapsible: false,
            collapse_button: Button::Right,
            on_click: None,
        }
    }

    /// The wheel turns pages on a module that has them. It used to be answered by
    /// whatever the module's buttons did, because a notch has no button to compare.
    #[test]
    fn a_notch_turns_a_page_and_says_which_way() {
        let module = PlacedModule {
            paged: Some(3),
            refresh: Some(Button::Left),
            ..placed()
        };
        assert_eq!(
            gesture(&module, SCROLL_DOWN),
            Gesture::Page {
                count: 3,
                forward: true
            }
        );
        assert_eq!(
            gesture(&module, SCROLL_UP),
            Gesture::Page {
                count: 3,
                forward: false
            }
        );
        assert_eq!(gesture(&module, Button::Left.number()), Gesture::Refresh);
    }

    /// A module with nothing to page through leaves the notch alone, and it goes on to
    /// whatever the module is showing - a volume that scrolls, or a provider's block.
    #[test]
    fn a_notch_on_a_module_with_one_reading_is_left_alone() {
        let module = PlacedModule {
            refresh: Some(Button::Left),
            ..placed()
        };
        assert_eq!(gesture(&module, SCROLL_UP), Gesture::Forward);
        assert_eq!(gesture(&module, SCROLL_DOWN), Gesture::Forward);
    }

    /// A program of the user's own is the one thing the config asked for outright, so it
    /// takes its button before anything built in looks at it.
    #[test]
    fn a_program_of_your_own_comes_before_anything_built_in() {
        let actions = ClickActions {
            left: Some(vec!["cal".to_string()]),
            ..ClickActions::default()
        };
        let module = PlacedModule {
            on_click: Some(std::sync::Arc::new(actions)),
            alt: Some(2),
            ..placed()
        };
        assert_eq!(
            gesture(&module, Button::Left.number()),
            Gesture::Run(Button::Left)
        );
        // The button it was not given still does what the module says.
        assert_eq!(gesture(&module, Button::Right.number()), Gesture::Forward);
    }

    #[test]
    fn the_wordings_and_the_folding_answer_to_their_own_buttons() {
        let module = PlacedModule {
            alt: Some(3),
            collapsible: true,
            ..placed()
        };
        assert_eq!(gesture(&module, Button::Left.number()), Gesture::Alt(3));
        assert_eq!(gesture(&module, Button::Right.number()), Gesture::Collapse);
        assert_eq!(gesture(&module, Button::Middle.number()), Gesture::Forward);
    }

    /// A module the frame did not name has nothing remembered against it, so every press
    /// goes to what it is showing.
    #[test]
    fn a_module_no_gesture_names_forwards_everything() {
        let module = PlacedModule {
            name: None,
            alt: Some(2),
            collapsible: true,
            ..placed()
        };
        for button in [1, 2, 3, SCROLL_UP, SCROLL_DOWN] {
            assert_eq!(gesture(&module, button), Gesture::Forward);
        }
    }

    fn wheel(value120: i32) -> AxisScroll {
        AxisScroll {
            value120,
            ..AxisScroll::default()
        }
    }

    fn finger(pixels: f64) -> AxisScroll {
        AxisScroll {
            absolute: pixels,
            ..AxisScroll::default()
        }
    }

    /// One notch is one step, however many events the wheel takes to report it. A
    /// high-resolution wheel sends eighths of a notch, and eight of those are one step
    /// rather than eight.
    #[test]
    fn a_wheel_notch_is_one_step_however_finely_it_arrives() {
        let mut carried = 0.0;
        assert_eq!(steps_of(&wheel(120), &mut carried), 1);

        let mut carried = 0.0;
        let stepped: i32 = (0..8).map(|_| steps_of(&wheel(15), &mut carried)).sum();
        assert_eq!(stepped, 1, "eight eighths of a notch made {stepped} steps");
    }

    /// A touchpad reports pixels and no notches at all, which is what used to turn a
    /// two-finger drag into an adjustment per frame.
    #[test]
    fn a_finger_has_to_travel_before_anything_moves() {
        let mut carried = 0.0;
        for _ in 0..4 {
            assert_eq!(steps_of(&finger(3.0), &mut carried), 0, "moved too early");
        }
        assert_eq!(steps_of(&finger(3.0), &mut carried), 1);
    }

    /// Scrolling the other way starts from nothing, so a nudge back does not land on a
    /// step that the previous direction had almost paid for.
    #[test]
    fn turning_around_drops_what_was_carried() {
        let mut carried = 0.0;
        // Three quarters of a notch up: not a step yet, but carried.
        assert_eq!(steps_of(&wheel(90), &mut carried), 0);
        // A full notch down is a full step down. Had the upward remainder still been
        // there it would have paid for three quarters of this one, and nothing would
        // have moved.
        assert_eq!(steps_of(&wheel(-120), &mut carried), -1);
    }

    /// A fast scroll is still every step it asked for, rather than one.
    #[test]
    fn a_flick_is_worth_every_step_in_it() {
        let mut carried = 0.0;
        assert_eq!(steps_of(&wheel(600), &mut carried), 5);
    }

    /// The end of a kinetic scroll leaves nothing behind to leak into the next one.
    #[test]
    fn the_end_of_a_scroll_clears_what_was_carried() {
        let mut carried = 0.0;
        assert_eq!(steps_of(&finger(10.0), &mut carried), 0);
        let stop = AxisScroll {
            stop: true,
            ..AxisScroll::default()
        };
        assert_eq!(steps_of(&stop, &mut carried), 0);
        assert_eq!(carried, 0.0);
        assert_eq!(
            steps_of(&finger(10.0), &mut carried),
            0,
            "carried across a stop"
        );
    }
}
