//! GNOME's overlay surface: an override-redirect ARGB window on `XWayland`, which Mutter never
//! focuses, mapped only while shown so a hidden overlay never takes a click.

use std::sync::{Arc, mpsc};

use x11rb::connection::Connection as _;
use x11rb::protocol::Event;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    AtomEnum, ClipOrdering, ColormapAlloc, ConfigureWindowAux, ConnectionExt as _, CreateGCAux,
    CreateWindowAux, EventMask, ImageFormat, PropMode, StackMode, VisualClass, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::host::{HostError, OVERLAY_TITLE, OverlayEvent, OverlayFrame};

/// Rows per image request, which keeps each under the core protocol's request size.
const STRIP_ROWS: usize = 50;

fn failed(cause: impl std::fmt::Display) -> HostError {
    HostError::Unsupported(format!(
        "The status overlay could not open its XWayland window: {cause}."
    ))
}

pub struct Window {
    conn: Arc<RustConnection>,
    id: u32,
    gc: u32,
    scale: f32,
}

impl Window {
    /// A window `size` points large, centred on the primary monitor `top` logical pixels down.
    pub fn open(
        size: (f32, f32),
        top: f64,
        takes_clicks: bool,
        events: mpsc::Sender<OverlayEvent>,
    ) -> Result<Self, HostError> {
        let (conn, screen) = x11rb::connect(None).map_err(failed)?;
        let conn = Arc::new(conn);
        let root = conn.setup().roots[screen].clone();
        let scale = desktop_scale(&conn, root.root);
        let visual = root
            .allowed_depths
            .iter()
            .filter(|depth| depth.depth == 32)
            .flat_map(|depth| depth.visuals.iter())
            .find(|visual| visual.class == VisualClass::TRUE_COLOR)
            .ok_or_else(|| failed("XWayland offers no 32-bit visual"))?
            .visual_id;
        let monitor = conn
            .randr_get_monitors(root.root, true)
            .map_err(failed)?
            .reply()
            .map_err(failed)?
            .monitors
            .into_iter()
            .max_by_key(|monitor| monitor.primary)
            .ok_or_else(|| failed("XWayland lists no monitor"))?;
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "pixel sizes on one screen"
        )]
        let (width, height, down) = (
            (size.0 * scale).ceil() as u16,
            (size.1 * scale).ceil() as u16,
            (top * f64::from(scale)).round() as i16,
        );
        let x = monitor.x + i16::try_from((monitor.width.saturating_sub(width)) / 2).unwrap_or(0);
        let colormap = conn.generate_id().map_err(failed)?;
        conn.create_colormap(ColormapAlloc::NONE, colormap, root.root, visual)
            .map_err(failed)?;
        let window = conn.generate_id().map_err(failed)?;
        let mask = if takes_clicks {
            EventMask::BUTTON_PRESS
        } else {
            EventMask::NO_EVENT
        };
        conn.create_window(
            32,
            window,
            root.root,
            x,
            monitor.y + down,
            width,
            height,
            0,
            WindowClass::INPUT_OUTPUT,
            visual,
            &CreateWindowAux::new()
                .override_redirect(1)
                .background_pixel(0)
                .border_pixel(0)
                .colormap(colormap)
                .event_mask(mask),
        )
        .map_err(failed)?;
        if !takes_clicks {
            conn.shape_rectangles(
                shape::SO::SET,
                shape::SK::INPUT,
                ClipOrdering::UNSORTED,
                window,
                0,
                0,
                &[],
            )
            .map_err(failed)?;
        }
        name(&conn, window)?;
        let gc = conn.generate_id().map_err(failed)?;
        conn.create_gc(gc, window, &CreateGCAux::new())
            .map_err(failed)?;
        conn.flush().map_err(failed)?;
        forward_presses(Arc::clone(&conn), scale, events)?;
        Ok(Self {
            conn,
            id: window,
            gc,
            scale,
        })
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    pub fn show(&mut self, frame: &OverlayFrame) -> Result<(), HostError> {
        self.conn.map_window(self.id).map_err(failed)?;
        self.conn
            .configure_window(
                self.id,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )
            .map_err(failed)?;
        self.present(frame)
    }

    pub fn present(&mut self, frame: &OverlayFrame) -> Result<(), HostError> {
        let stride = frame.width as usize * 4;
        let width = u16::try_from(frame.width).map_err(failed)?;
        for (index, rows) in frame.bgra.chunks(stride * STRIP_ROWS).enumerate() {
            let height = u16::try_from(rows.len() / stride).map_err(failed)?;
            let top = i16::try_from(index * STRIP_ROWS).map_err(failed)?;
            self.conn
                .put_image(
                    ImageFormat::Z_PIXMAP,
                    self.id,
                    self.gc,
                    width,
                    height,
                    0,
                    top,
                    0,
                    32,
                    rows,
                )
                .map_err(failed)?;
        }
        self.conn.flush().map_err(failed)
    }

    pub fn hide(&mut self) {
        let _ = self.conn.unmap_window(self.id);
        let _ = self.conn.flush();
    }
}

/// Marks the window as a notification named after the overlay, as desktops recognise it.
fn name(conn: &RustConnection, window: u32) -> Result<(), HostError> {
    let kind = intern(conn, b"_NET_WM_WINDOW_TYPE")?;
    let notification = intern(conn, b"_NET_WM_WINDOW_TYPE_NOTIFICATION")?;
    conn.change_property32(
        PropMode::REPLACE,
        window,
        kind,
        AtomEnum::ATOM,
        &[notification],
    )
    .map_err(failed)?;
    conn.change_property8(
        PropMode::REPLACE,
        window,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        OVERLAY_TITLE.as_bytes(),
    )
    .map_err(failed)?;
    Ok(())
}

/// Hands each left-button press on the window to `events`, in points, from a thread of its own.
fn forward_presses(
    conn: Arc<RustConnection>,
    scale: f32,
    events: mpsc::Sender<OverlayEvent>,
) -> Result<(), HostError> {
    std::thread::Builder::new()
        .name("overlay-x11".to_owned())
        .spawn(move || {
            while let Ok(event) = conn.wait_for_event() {
                if let Event::ButtonPress(press) = event
                    && press.detail == 1
                {
                    let at = OverlayEvent::Press {
                        x: f32::from(press.event_x) / scale,
                        y: f32::from(press.event_y) / scale,
                    };
                    if events.send(at).is_err() {
                        break;
                    }
                }
            }
        })
        .map(|_| ())
        .map_err(failed)
}

fn intern(conn: &RustConnection, name: &[u8]) -> Result<u32, HostError> {
    Ok(conn
        .intern_atom(false, name)
        .map_err(failed)?
        .reply()
        .map_err(failed)?
        .atom)
}

/// The desktop's scale: `Xft.dpi` / 96, which GNOME sets for `XWayland` apps; 1 when unset.
fn desktop_scale(conn: &RustConnection, root: u32) -> f32 {
    let resources = conn
        .get_property(
            false,
            root,
            AtomEnum::RESOURCE_MANAGER,
            AtomEnum::STRING,
            0,
            1 << 16,
        )
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|reply| reply.value)
        .unwrap_or_default();
    xft_scale(&String::from_utf8_lossy(&resources))
}

fn xft_scale(resources: &str) -> f32 {
    resources
        .lines()
        .find_map(|line| line.strip_prefix("Xft.dpi:"))
        .and_then(|dpi| dpi.trim().parse::<f32>().ok())
        .filter(|dpi| *dpi >= 48.0)
        .map_or(1.0, |dpi| dpi / 96.0)
}

#[cfg(test)]
mod tests {
    use super::xft_scale;

    #[test]
    fn the_scale_comes_from_xft_dpi_and_is_one_without_a_usable_value() {
        for (resources, scale) in [
            ("Xcursor.size:\t24\nXft.dpi:\t144\n", 1.5),
            ("Xft.dpi:\t96", 1.0),
            ("Xcursor.size:\t24\n", 1.0),
            ("Xft.dpi:\tlots", 1.0),
            ("Xft.dpi:\t0", 1.0),
        ] {
            assert!(
                (xft_scale(resources) - scale).abs() < f32::EPSILON,
                "{resources:?}"
            );
        }
    }
}
