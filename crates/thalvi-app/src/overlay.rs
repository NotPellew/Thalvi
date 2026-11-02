//! When the overlay window is shown, and the window itself.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use thalvi_desktop::host::OVERLAY_TITLE;

use crate::status::Status;

pub const LABEL: &str = "overlay";

const WIDTH: f64 = 460.0;
const HEIGHT: f64 = 150.0;
const TOP: f64 = 56.0;

/// How long a delivery's confirmation stays on screen.
const DONE_FOR: Duration = Duration::from_secs(1);

/// How long a failure's notice stays on screen.
const FAILED_FOR: Duration = Duration::from_secs(4);

/// What the overlay window does when the status changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    Show,
    Hide,
    /// Show now, then hide.
    ShowFor(Duration),
}

impl Visibility {
    /// Shown from the wake word until the delivery has been confirmed; never when `enabled` is off.
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
            | Status::Working { .. } => Self::Show,
            Status::Done { .. } => Self::ShowFor(DONE_FOR),
            Status::Failed { .. } => Self::ShowFor(FAILED_FOR),
        }
    }
}

/// Creates the hidden overlay window; the desktop keeps it unfocused (see `Host::prepare_overlay`).
///
/// # Errors
/// Fails when the window cannot be created.
pub fn create(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    let left = app
        .primary_monitor()?
        .map_or(0.0, |monitor| {
            let width = f64::from(monitor.size().width) / monitor.scale_factor();
            (width - WIDTH) / 2.0
        })
        .max(0.0);
    WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("overlay.html".into()))
        .title(OVERLAY_TITLE)
        .inner_size(WIDTH, HEIGHT)
        .position(left, TOP)
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
    fn the_overlay_shows_from_the_wake_word_and_hides_after_the_confirmation() {
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
                },
                Visibility::ShowFor(Duration::from_secs(4)),
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
