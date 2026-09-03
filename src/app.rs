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

use crate::config::{Config, Edge};
use crate::layout::{self, Frame};
use crate::render;
use crate::status::{Block, ClickEvent, I3BarProvider, StatusEvent};
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
    provider: I3BarProvider,
    blocks: Vec<Block>,
    frame: Frame,
    /// Set when the status provider itself has failed; shown in place of the groups.
    fault: Option<String>,
    /// Block names from the last "nothing matched" warning, so it is not repeated per redraw.
    warned_names: Option<Vec<String>>,

    /// Surface size in logical pixels.
    width: u32,
    height: u32,
    scale: i32,

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
        provider: I3BarProvider,
    ) -> Result<App> {
        let compositor =
            CompositorState::bind(globals, qh).context("wl_compositor is not available")?;
        let layer_shell = LayerShell::bind(globals, qh)
            .context("zwlr_layer_shell_v1 is not available; is this a wlroots compositor?")?;
        let shm = Shm::bind(globals, qh).context("wl_shm is not available")?;

        let surface = compositor.create_surface(qh);
        let layer = layer_shell.create_layer_surface(qh, surface, Layer::Top, Some("dbar"), None);

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
            provider,
            blocks: Vec::new(),
            frame: Frame::default(),
            fault: None,
            warned_names: None,
            width: 0,
            height,
            scale: 1,
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
                self.provider.set_accepts_clicks(header.click_events);
            }
            StatusEvent::Blocks(mut blocks) => {
                // Positional names are all the protocol offers, so pin them to the
                // configured names once, here, rather than at every use.
                for (index, block) in blocks.iter_mut().enumerate() {
                    block.alias = self.config.status.blocks.get(index).cloned();
                }
                self.blocks = blocks;
                self.fault = None;
                self.invalidate();
            }
            StatusEvent::Stopped(reason) => {
                log::error!("status provider stopped: {reason}");
                self.blocks.clear();
                self.fault = Some(format!("status provider stopped: {reason}"));
                self.invalidate();
            }
        }
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
                let frame =
                    layout::compute(&self.config, &self.blocks, width, height, &mut self.text);
                self.warn_if_nothing_matched(&frame);
                frame
            }
        };

        log::debug!(
            "draw: {}x{} scale {}, {} blocks -> {} groups, {} modules",
            self.width,
            self.height,
            self.scale,
            self.blocks.len(),
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
        if drawn > 0 || self.blocks.is_empty() {
            self.warned_names = None;
            return;
        }
        let names: Vec<String> = self
            .blocks
            .iter()
            .map(|b| b.selector().unwrap_or("<unnamed>").to_string())
            .collect();
        if self.warned_names.as_ref() == Some(&names) {
            return;
        }
        log::warn!(
            "no configured module matched any of the {} block(s) the status provider is \
             sending ({}); groups select blocks by name, and modules = [\"*\"] takes them all",
            names.len(),
            names.join(", ")
        );
        self.warned_names = Some(names);
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
        let Some(block) = module.block.and_then(|i| self.blocks.get(i)) else {
            return;
        };

        let event = ClickEvent {
            name: block.name.as_deref(),
            instance: block.instance.as_deref(),
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
        self.provider.send_click(&event);
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
