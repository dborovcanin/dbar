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

pub struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
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
    frame: Frame,
    /// Set when the status provider itself has failed; shown in place of the groups.
    fault: Option<String>,
    /// Item names from the last "nothing matched" warning, so it is not repeated per redraw.
    warned_names: Option<Vec<String>>,
    /// Whether the block-count mismatch has already been reported.
    name_count_warned: bool,

    /// Surface size in logical pixels.
    width: u32,
    height: u32,
    scale: i32,

    /// Pointer position in surface coordinates, while it is over the bar.
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
        let stack = match bar.layer {
            BarLayer::Background => Layer::Background,
            BarLayer::Bottom => Layer::Bottom,
            BarLayer::Top => Layer::Top,
            BarLayer::Overlay => Layer::Overlay,
        };

        let surface = compositor.create_surface(qh);
        let layer = layer_shell.create_layer_surface(qh, surface, stack, Some("dbar"), None);

        let edge = match bar.position {
            Edge::Top => Anchor::TOP,
            Edge::Bottom => Anchor::BOTTOM,
        };
        layer.set_anchor(edge | Anchor::LEFT | Anchor::RIGHT);
        // A zero width lets the compositor stretch us between the left and right anchors.
        layer.set_size(0, bar.height);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        let m = bar.margin;
        match bar.position {
            Edge::Top => layer.set_margin(m, m, 0, m),
            Edge::Bottom => layer.set_margin(0, m, m, m),
        }
        layer.set_exclusive_zone(if bar.exclusive {
            bar.height as i32 + bar.margin
        } else {
            0
        });
        // The first commit must carry no buffer; the compositor answers with a configure.
        layer.commit();

        let height = bar.height;
        let pool =
            SlotPool::new(1920 * height as usize * 4, &shm).context("creating the shm pool")?;
        let text = TextRenderer::new(&bar.font_family, bar.font_size, &bar.font_fallback)?;

        Ok(App {
            registry_state: RegistryState::new(globals),
            seat_state: SeatState::new(globals, qh),
            output_state: OutputState::new(globals, qh),
            shm,
            pool,
            layer,
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
            frame: Frame::default(),
            fault: None,
            warned_names: None,
            name_count_warned: false,
            width: 0,
            height,
            scale: 1,
            pointer_at: None,
            scrolled: 0.0,
            configured: false,
            dirty: true,
            frame_pending: false,
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

    /// Mark the bar as needing a redraw and draw immediately if the compositor is ready.
    fn invalidate(&mut self) {
        self.dirty = true;
        self.draw_if_needed();
    }

    /// Draw when there is something to draw and no frame callback is outstanding.
    pub fn draw_if_needed(&mut self) {
        if !self.dirty || self.frame_pending || !self.configured || self.width == 0 {
            return;
        }
        if let Err(e) = self.draw() {
            log::error!("draw failed: {e}");
            return;
        }
        self.dirty = false;
        let _ = self.conn.flush();
    }

    fn draw(&mut self) -> Result<()> {
        let scale = self.scale.max(1) as f32;
        self.painter.text.set_scale(scale);
        let (width, height) = (self.width as f32, self.height as f32);
        self.frame = match &self.fault {
            Some(message) => layout::fault(message, width, height, &mut self.painter.text),
            None => {
                let inputs = Inputs {
                    items: &self.items,
                    native: &self.native,
                    sway: &self.sway,
                    alt: &self.alt,
                    pages: &self.pages,
                    collapsed: &self.collapsed,
                    waiting: &self.waiting,
                    spin: self.spin,
                };
                let frame = layout::compute(
                    &self.config,
                    &inputs,
                    width,
                    height,
                    &mut self.painter.text,
                    self.pointer_at,
                );
                self.warn_if_nothing_matched(&frame);
                frame
            }
        };

        log::debug!(
            "draw: {}x{} scale {}, {} item(s) -> {} groups, {} modules",
            self.width,
            self.height,
            self.scale,
            self.items.len(),
            self.frame.groups.len(),
            self.frame
                .groups
                .iter()
                .map(|g| g.modules.len())
                .sum::<usize>()
        );
        let pw = (self.width * self.scale.max(1) as u32) as i32;
        let ph = (self.height * self.scale.max(1) as u32) as i32;
        let stride = pw * 4;
        let (buffer, canvas) = self
            .pool
            .create_buffer(pw, ph, stride, wl_shm::Format::Argb8888)
            .context("creating an shm buffer")?;

        render::render_to_buffer(
            canvas,
            pw as u32,
            ph as u32,
            &self.config,
            &self.frame,
            scale,
            &mut self.painter,
        )?;

        let surface = self.layer.wl_surface();
        surface.set_buffer_scale(self.scale.max(1));
        surface.damage_buffer(0, 0, pw, ph);
        surface.frame(&self.qh, FrameCallbackData(surface.clone()));
        self.frame_pending = true;
        buffer.attach_to(surface).context("attaching the buffer")?;
        self.layer.commit();
        Ok(())
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
    fn set_pointer(&mut self, at: Option<(f32, f32)>) {
        let before = self.frame.hover_key(self.pointer_at);
        let after = self.frame.hover_key(at);
        self.pointer_at = at;
        if before != after {
            self.invalidate();
        }
    }

    fn on_click(&mut self, x: f64, y: f64, button: u32) {
        let Some(i3_button) = i3bar_button(button) else {
            return;
        };
        self.dispatch_click(x, y, i3_button);
    }

    fn dispatch_click(&mut self, x: f64, y: f64, button: u32) {
        let Some(module) = self.frame.module_at(x as f32, y as f32) else {
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
        _surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        if new_factor != self.scale {
            self.scale = new_factor.max(1);
            self.invalidate();
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
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        self.frame_pending = false;
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
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let (w, h) = configure.new_size;
        if w != 0 {
            self.width = w;
        }
        if h != 0 {
            self.height = h;
        }
        self.configured = true;
        // A configure invalidates any pending frame callback expectation.
        self.frame_pending = false;
        self.invalidate();
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
            if &event.surface != self.layer.wl_surface() {
                continue;
            }
            let (x, y) = event.position;
            match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    self.set_pointer(Some((x as f32, y as f32)));
                }
                PointerEventKind::Leave { .. } => self.set_pointer(None),
                PointerEventKind::Press { button, .. } => self.on_click(x, y, button),
                PointerEventKind::Axis { vertical, .. } => {
                    let steps = steps_of(&vertical, &mut self.scrolled);
                    // i3bar encodes scroll as buttons 4 (up) and 5 (down).
                    let button = match steps.is_negative() {
                        true => SCROLL_UP,
                        false => SCROLL_DOWN,
                    };
                    for _ in 0..steps.unsigned_abs() {
                        self.dispatch_click(x, y, button);
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

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
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
