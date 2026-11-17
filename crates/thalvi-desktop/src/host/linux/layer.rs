//! Hyprland's overlay surface: a layer-shell surface on the overlay layer, drawn from shared memory.

use std::sync::{Arc, Mutex, PoisonError, mpsc};

use crate::host::OverlayEvent;

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Region};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer as Level, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_registry, registry_handlers};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_output, wl_shm, wl_surface};
use wayland_client::{Connection, QueueHandle};

use crate::host::{HostError, OVERLAY_TITLE, OverlayFrame};

use super::place::{self, Configure, Seat};

fn failed(cause: impl std::fmt::Display) -> HostError {
    HostError::Unsupported(format!(
        "The status overlay could not open a layer surface: {cause}."
    ))
}

/// A layer surface committed and not yet placed, and when that wait started.
struct Pending {
    layer: LayerSurface,
    since: std::time::Instant,
}

/// The overlay's layer surface, made and drawn on the overlay's thread; its events are read on a
/// thread of their own and handed over.
pub struct Layer {
    conn: Connection,
    queue: QueueHandle<Events>,
    compositor: CompositorState,
    shell: LayerShell,
    pool: SlotPool,
    /// The outputs' integer scale; a fractional one rounds up and the compositor scales down.
    scale: i32,
    /// In points.
    size: (u32, u32),
    surface: Option<LayerSurface>,
    configures: Arc<Mutex<mpsc::Receiver<Configure>>>,
    /// A show waiting on the compositor; `None` once it is placed, closed or given up.
    pending: Option<Pending>,
    /// The overlay thread hears when the event thread's connection ends, or a show is still placing.
    events: mpsc::Sender<OverlayEvent>,
}

impl Layer {
    pub fn open(
        width: f32,
        height: f32,
        events: mpsc::Sender<OverlayEvent>,
    ) -> Result<Self, HostError> {
        let conn = Connection::connect_to_env().map_err(failed)?;
        let (globals, mut queue_events) = registry_queue_init::<Events>(&conn).map_err(failed)?;
        let queue = queue_events.handle();
        let compositor = CompositorState::bind(&globals, &queue).map_err(failed)?;
        let shell = LayerShell::bind(&globals, &queue).map_err(failed)?;
        let shm = Shm::bind(&globals, &queue).map_err(failed)?;
        let (sender, configures) = mpsc::channel();
        let mut state = Events {
            registry: RegistryState::new(&globals),
            output: OutputState::new(&globals, &queue),
            shm,
            configures: sender,
        };
        // The second round trip brings each output's scale.
        for _ in 0..2 {
            queue_events.roundtrip(&mut state).map_err(failed)?;
        }
        let scale = state
            .output
            .outputs()
            .filter_map(|output| state.output.info(&output))
            .map(|info| info.scale_factor)
            .max()
            .unwrap_or(1)
            .max(1);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the overlay's size in points"
        )]
        let size = (width.round() as u32, height.round() as u32);
        #[expect(clippy::cast_sign_loss, reason = "a scale of at least 1")]
        let bytes = (size.0 * size.1 * 4) as usize * (scale * scale) as usize;
        // Two frames' room: one the compositor holds while the next is drawn.
        let pool = SlotPool::new(2 * bytes, &state.shm).map_err(failed)?;
        let report = events.clone();
        std::thread::Builder::new()
            .name("overlay-wayland".to_owned())
            .spawn(move || {
                while queue_events.blocking_dispatch(&mut state).is_ok() {}
                let _ = report.send(OverlayEvent::Lost(
                    "The status overlay lost its Wayland connection.".to_owned(),
                ));
            })
            .map_err(failed)?;
        Ok(Self {
            conn,
            queue,
            compositor,
            shell,
            pool,
            scale,
            size,
            surface: None,
            configures: Arc::new(Mutex::new(configures)),
            pending: None,
            events,
        })
    }

    #[expect(clippy::cast_precision_loss, reason = "a small integer scale")]
    pub fn scale(&self) -> f32 {
        self.scale as f32
    }

    pub fn show(&mut self, frame: &OverlayFrame) -> Result<(), HostError> {
        self.step(frame, true)
    }

    pub fn present(&mut self, frame: &OverlayFrame) -> Result<(), HostError> {
        self.step(frame, false)
    }

    /// One placement decision, then the pixels. `show` may make the surface; `present` only draws
    /// on the one already placed. A configure still outstanding leaves it unplaced, so the caller
    /// retries later instead of waiting here.
    fn step(&mut self, frame: &OverlayFrame, show: bool) -> Result<(), HostError> {
        let seat = match &self.pending {
            Some(pending) => Seat::Pending {
                waited: pending.since.elapsed(),
            },
            None if self.surface.is_some() => Seat::Placed,
            None => Seat::None,
        };
        // One drain: a show with no surface discards what was queued, and every other step keeps it.
        let (seat, step) = place::show_step(seat, self.drain());
        match step {
            place::Step::Create if show => self.make_surface()?,
            place::Step::Create | place::Step::NotYet => {
                return Err(failed("Hyprland did not place it"));
            }
            place::Step::Closed => {
                self.surface = None;
                self.pending = None;
                return Err(failed("the layer surface closed"));
            }
            place::Step::Attach { gave_up: true } => {
                // Keep the surface. Dropping it and showing again leaves a layer behind on a
                // compositor that never configured it, and the next status would make another.
                if let Some(pending) = self.pending.take() {
                    self.surface = Some(pending.layer);
                }
                return Err(failed("Hyprland did not place it"));
            }
            place::Step::Attach { gave_up: false } => {
                if matches!(seat, Seat::Placed)
                    && self.surface.is_none()
                    && let Some(pending) = self.pending.take()
                {
                    self.surface = Some(pending.layer);
                }
            }
        }
        if matches!(step, place::Step::Create) {
            // The surface was just committed. Its configure, if the compositor already sent one,
            // places it on this same show.
            return match self.step(frame, false) {
                Err(_) if self.pending.is_some() => {
                    let _ = self.events.send(OverlayEvent::Pending);
                    Err(failed("the layer surface is not placed yet"))
                }
                other => other,
            };
        }
        let Some(layer) = &self.surface else {
            return Err(failed("Hyprland did not place it"));
        };
        let (width, height) = (
            i32::try_from(frame.width).map_err(failed)?,
            i32::try_from(frame.height).map_err(failed)?,
        );
        let (buffer, pixels) = self
            .pool
            .create_buffer(width, height, width * 4, wl_shm::Format::Argb8888)
            .map_err(failed)?;
        pixels.copy_from_slice(&frame.bgra);
        let surface = layer.wl_surface();
        surface.damage_buffer(0, 0, width, height);
        buffer.attach_to(surface).map_err(failed)?;
        layer.commit();
        self.conn.flush().map_err(failed)
    }

    /// Commits a new layer surface and waits for the compositor to place it.
    fn make_surface(&mut self) -> Result<(), HostError> {
        let surface = self.compositor.create_surface(&self.queue);
        surface.set_buffer_scale(self.scale);
        // An empty input region: clicks reach the windows below, as the overlay takes none here.
        let empty = Region::new(&self.compositor).map_err(failed)?;
        surface.set_input_region(Some(empty.wl_region()));
        let layer = self.shell.create_layer_surface(
            &self.queue,
            surface,
            Level::Overlay,
            Some(OVERLAY_TITLE),
            None,
        );
        layer.set_anchor(Anchor::TOP);
        layer.set_size(self.size.0, self.size.1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();
        self.conn.flush().map_err(failed)?;
        self.pending = Some(Pending {
            layer,
            since: std::time::Instant::now(),
        });
        Ok(())
    }

    pub fn hide(&mut self) {
        self.surface = None;
        self.pending = None;
        let _ = self.conn.flush();
    }

    /// Every configure queued since the last step. `show_step` keeps the latest of them.
    fn drain(&mut self) -> impl Iterator<Item = Configure> {
        let guard = self
            .configures
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        guard.try_iter().collect::<Vec<_>>().into_iter()
    }
}

/// The state the event thread dispatches into.
struct Events {
    registry: RegistryState,
    output: OutputState,
    shm: Shm,
    configures: mpsc::Sender<Configure>,
}

impl LayerShellHandler for Events {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        let _ = self.configures.send(Configure::Closed);
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        _: LayerSurfaceConfigure,
        _: u32,
    ) {
        let _ = self.configures.send(Configure::Ready);
    }
}

impl CompositorHandler for Events {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for Events {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for Events {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for Events {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}

delegate_registry!(Events);
smithay_client_toolkit::delegate_dispatch2!(Events);
