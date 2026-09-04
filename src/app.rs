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
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
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

use crate::collect::Registry;
use crate::config::{Config, Edge};
use crate::layout::{self, Frame, Inputs};
use crate::render;
use crate::status::{ActionTarget, ClickEvent, I3BarProvider, StatusEvent, StatusItem, i3bar};
use crate::sway::{self, SwayEvent, SwayState};
use crate::text::TextRenderer;

/// Linux input button codes, as delivered by `wl_pointer`.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

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
    text: TextRenderer,
    /// The external status provider, when the config asks for one.
    provider: Option<I3BarProvider>,
    items: Vec<StatusItem>,
    /// What dbar measures for itself.
    native: Registry,
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

        let surface = compositor.create_surface(qh);
        let layer = layer_shell.create_layer_surface(qh, surface, Layer::Top, Some("dbar"), None);

        let config_collectors = config.collectors();
        let bar = &config.bar;
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
        let text = TextRenderer::new(&bar.font_family, bar.font_size);

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
            text,
            native: Registry::new(&config_collectors),
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
                self.items = i3bar::to_items(&blocks, &self.config.status.blocks);
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
        let names = &self.config.status.blocks;
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
            "provider sends {} block(s) but [status] blocks names {}; the extra blocks keep \
             the names the provider gave them",
            blocks.len(),
            names.len()
        );
        self.name_count_warned = true;
    }

    /// Read every collector that has come due, and say when the next one is.
    ///
    /// One pass over the whole set, then one redraw: ten modules sharing an interval cost
    /// one wake-up between them.
    pub fn on_collect(&mut self) -> Option<std::time::Instant> {
        if self.native.tick() {
            self.invalidate();
        }
        self.native.next_due()
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
        self.text.set_scale(scale);
        let (width, height) = (self.width as f32, self.height as f32);
        self.frame = match &self.fault {
            Some(message) => layout::fault(message, width, height, &mut self.text),
            None => {
                let inputs = Inputs {
                    items: &self.items,
                    native: &self.native,
                    sway: &self.sway,
                };
                let frame = layout::compute(
                    &self.config,
                    &inputs,
                    width,
                    height,
                    &mut self.text,
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
            &mut self.text,
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
        let (mx, my, mw, mh) = (module.x, module.y, module.width, module.height);
        // Cloned because acting on the target needs the provider, and the module is
        // borrowed out of the frame we are still holding.
        let Some(action) = module.action.clone() else {
            return;
        };

        match action {
            // A module backed by the compositor acts on its own rather than forwarding.
            ActionTarget::Sway(command) => {
                if button == 1 {
                    sway::run_command(&command);
                }
            }
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
                    // i3bar encodes scroll as buttons 4 (up) and 5 (down).
                    let button = if vertical.absolute < 0.0 || vertical.discrete < 0 {
                        Some(4)
                    } else if vertical.absolute > 0.0 || vertical.discrete > 0 {
                        Some(5)
                    } else {
                        None
                    };
                    if let Some(button) = button {
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
