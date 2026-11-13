//! Hyprland's overlay surface: a layer-shell surface on the overlay layer, drawn from shared memory.

use std::sync::mpsc;
use std::time::Duration;

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

/// How long a new layer surface may wait for the compositor's first configure.
const CONFIGURE_WITHIN: Duration = Duration::from_secs(1);

fn failed(cause: impl std::fmt::Display) -> HostError {
    HostError::Unsupported(format!(
        "The status overlay could not open a layer surface: {cause}."
    ))
}

/// What the compositor said about the layer surface last made.
enum Configure {
    Ready,
    Closed,
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
    configures: mpsc::Receiver<Configure>,
}

impl Layer {
    pub fn open(width: f32, height: f32) -> Result<Self, HostError> {
        let conn = Connection::connect_to_env().map_err(failed)?;
        let (globals, mut events_queue) = registry_queue_init::<Events>(&conn).map_err(failed)?;
        let queue = events_queue.handle();
        let compositor = CompositorState::bind(&globals, &queue).map_err(failed)?;
        let shell = LayerShell::bind(&globals, &queue).map_err(failed)?;
        let shm = Shm::bind(&globals, &queue).map_err(failed)?;
        let (sender, configures) = mpsc::channel();
        let mut events = Events {
            registry: RegistryState::new(&globals),
            output: OutputState::new(&globals, &queue),
            shm,
            configures: sender,
        };
        // The second round trip brings each output's scale.
        for _ in 0..2 {
            events_queue.roundtrip(&mut events).map_err(failed)?;
        }
        let scale = events
            .output
            .outputs()
            .filter_map(|output| events.output.info(&output))
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
        let pool = SlotPool::new(2 * bytes, &events.shm).map_err(failed)?;
        std::thread::Builder::new()
            .name("overlay-wayland".to_owned())
            .spawn(move || while events_queue.blocking_dispatch(&mut events).is_ok() {})
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
            configures,
        })
    }

    #[expect(clippy::cast_precision_loss, reason = "a small integer scale")]
    pub fn scale(&self) -> f32 {
        self.scale as f32
    }

    pub fn show(&mut self, frame: &OverlayFrame) -> Result<(), HostError> {
        while self.configures.try_recv().is_ok() {}
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
        match self.configures.recv_timeout(CONFIGURE_WITHIN) {
            Ok(Configure::Ready) => {}
            Ok(Configure::Closed) | Err(_) => return Err(failed("Hyprland did not place it")),
        }
        self.surface = Some(layer);
        self.present(frame)
    }

    pub fn present(&mut self, frame: &OverlayFrame) -> Result<(), HostError> {
        // A closed surface (its output went away) is made again at the next show.
        if matches!(self.configures.try_recv(), Ok(Configure::Closed)) {
            self.surface = None;
        }
        let Some(layer) = &self.surface else {
            return Ok(());
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

    pub fn hide(&mut self) {
        self.surface = None;
        let _ = self.conn.flush();
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
