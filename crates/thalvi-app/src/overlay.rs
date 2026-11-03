//! When the overlay window is shown, and the window itself.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tauri::{AppHandle, LogicalPosition, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use thalvi_desktop::host::{Desktop, Host, OVERLAY_TITLE};

use crate::status::Status;

pub const LABEL: &str = "overlay";

const WIDTH: f64 = 460.0;
/// Room for the neon pill under its gap and for the holo and gunmetal module with its shadow.
/// A fixed-size GTK window is never lower than this, and a mapped one cannot be resized, so every
/// look uses the one size.
const HEIGHT: f64 = 200.0;

/// Where the overlay window goes, in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frame {
    left: f64,
    top: f64,
}

impl Frame {
    /// The window centred on a screen `screen_width` wide and `top` below its top edge.
    #[must_use]
    pub fn new(screen_width: f64, top: f64) -> Self {
        Self {
            left: ((screen_width - WIDTH) / 2.0).max(0.0),
            top,
        }
    }

    /// The frame on the primary monitor, below whatever the desktop reserves at its top.
    fn on_screen(app: &AppHandle, host: &Desktop) -> Result<Self, String> {
        let top = host
            .prepare_overlay(WIDTH, HEIGHT)
            .map_err(|error| error.to_string())?;
        let screen_width = app
            .primary_monitor()
            .map_err(|error| error.to_string())?
            .map_or(0.0, |monitor| {
                f64::from(monitor.size().width) / monitor.scale_factor()
            });
        Ok(Self::new(screen_width, top))
    }
}

/// How long a delivery's confirmation stays on screen.
const DONE_FOR: Duration = Duration::from_secs(1);

/// What the overlay window does when the status changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    Show,
    Hide,
    /// Show now, then hide.
    ShowFor(Duration),
}

impl Visibility {
    /// Shown from the wake word until the delivery has been confirmed, and a failure stays until
    /// the next capture starts or `dismiss_error` hides it; never when `enabled` is off.
    #[must_use]
    pub fn of(status: &Status, enabled: bool) -> Self {
        if !enabled {
            return Self::Hide;
        }
        match status {
            Status::Paused | Status::Listening => Self::Hide,
            Status::Starting { .. }
            | Status::Capturing { .. }
            | Status::Pending { .. }
            | Status::Working { .. }
            | Status::Loading { .. }
            | Status::Failed { .. } => Self::Show,
            Status::Done { .. } => Self::ShowFor(DONE_FOR),
        }
    }
}

/// Creates the hidden overlay window; the desktop keeps it unfocused (see `Host::prepare_overlay`).
///
/// # Errors
/// Why the desktop could not place the window or why it could not be created.
pub fn create(app: &AppHandle, host: &Desktop) -> Result<WebviewWindow, String> {
    let frame = Frame::on_screen(app, host)?;
    WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("overlay.html".into()))
        .title(OVERLAY_TITLE)
        .inner_size(WIDTH, HEIGHT)
        .position(frame.left, frame.top)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .resizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focusable(false)
        .focused(false)
        .visible(false)
        .build()
        .map_err(|error| error.to_string())
}

/// Places the overlay window again, below whatever the desktop reserves at the top now.
///
/// # Errors
/// Why the desktop could not place the window or why it could not be moved; it then stays as it was.
pub fn replace(app: &AppHandle, host: &Desktop) -> Result<(), String> {
    let Some(window) = app.get_webview_window(LABEL) else {
        return Ok(());
    };
    let frame = Frame::on_screen(app, host)?;
    window
        .set_position(LogicalPosition::new(frame.left, frame.top))
        .map_err(|error| error.to_string())
}

/// Shows and hides the overlay window as the status asks.
#[derive(Default)]
pub struct Overlay {
    /// Bumped by every change, so a delayed hide that was overtaken does nothing.
    generation: AtomicU64,
}

impl Overlay {
    pub fn apply(&self, app: &AppHandle, visibility: Visibility) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        match visibility {
            Visibility::Show => set_visible(app, true),
            Visibility::Hide => set_visible(app, false),
            Visibility::ShowFor(delay) => {
                set_visible(app, true);
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(delay).await;
                    let overlay = app.state::<crate::app::AppState>();
                    if overlay.overlay.generation.load(Ordering::SeqCst) == generation {
                        set_visible(&app, false);
                    }
                });
            }
        }
    }
}

fn set_visible(app: &AppHandle, visible: bool) {
    if let Some(window) = app.get_webview_window(LABEL) {
        let _ = if visible {
            window.show()
        } else {
            window.hide()
        };
    }
}

#[cfg(test)]
mod tests {
    use thalvi_desktop::capture::CaptureRoute;

    use super::*;

    #[test]
    fn the_frame_is_centred_and_hangs_below_the_panel() {
        for top in [0.0, 40.0] {
            assert_eq!(Frame::new(2048.0, top), Frame { left: 794.0, top });
        }
        // A screen narrower than the window starts at its left edge.
        assert_eq!(Frame::new(300.0, 0.0).left, 0.0);
    }

    #[test]
    fn the_overlay_shows_from_the_wake_word_and_hides_after_the_confirmation_but_keeps_a_failure() {
        let target = Some("kitty".to_owned());
        let statuses = [
            (Status::Paused, Visibility::Hide),
            (Status::Listening, Visibility::Hide),
            (
                Status::Starting {
                    target: target.clone(),
                },
                Visibility::Show,
            ),
            (
                Status::Capturing {
                    target: target.clone(),
                    committed: String::new(),
                    partial: None,
                },
                Visibility::Show,
            ),
            (
                Status::Pending {
                    target: target.clone(),
                    route: CaptureRoute::Paste,
                    text: String::new(),
                    waiting_for_quiet: false,
                    pause_ms: 700,
                },
                Visibility::Show,
            ),
            (
                Status::Working {
                    target: target.clone(),
                    route: CaptureRoute::Submit,
                    cancellable: false,
                },
                Visibility::Show,
            ),
            (
                Status::Done {
                    target,
                    route: CaptureRoute::Paste,
                    notice: String::new(),
                },
                Visibility::ShowFor(Duration::from_secs(1)),
            ),
            (
                Status::Failed {
                    notice: String::new(),
                    causes: Vec::new(),
                },
                Visibility::Show,
            ),
            (
                Status::Loading {
                    notice: String::new(),
                },
                Visibility::Show,
            ),
        ];
        for (status, expected) in statuses {
            assert_eq!(Visibility::of(&status, true), expected, "{status:?}");
            assert_eq!(
                Visibility::of(&status, false),
                Visibility::Hide,
                "{status:?}"
            );
        }
    }
}
