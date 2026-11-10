//! When the overlay is shown, and the Rune overlay surface that draws it.

mod gunmetal;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rune::{
    Align, App, AppSpec, OverlayHandle, OverlayInput, OverlayOptions, OverlaySpec, ScreenEdge,
};
use thalvi_desktop::host::OVERLAY_TITLE;

use crate::status::Status;
use gunmetal::View;

/// How long a delivery's confirmation stays on screen.
const DONE_FOR: Duration = Duration::from_secs(1);

/// How long a failure's headline stays on screen; the tray and Settings keep the failure.
const FAILED_FOR: Duration = Duration::from_secs(5);

/// What the overlay window does when the status changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    Show,
    Hide,
    /// Show now, then hide.
    ShowFor(Duration),
}

impl Visibility {
    /// Shown from the wake word until the delivery has been confirmed, and briefly for a failure,
    /// which `dismiss_error` can hide sooner; never when `enabled` is off.
    #[must_use]
    pub fn of(status: &Status, enabled: bool) -> Self {
        if !enabled {
            return Self::Hide;
        }
        match status {
            Status::Paused
            | Status::Listening
            | Status::Loading {
                downloading: false, ..
            } => Self::Hide,
            Status::Starting { .. }
            | Status::Capturing { .. }
            | Status::Pending { .. }
            | Status::Writing { .. }
            | Status::Thinking { .. }
            | Status::Working { .. }
            | Status::Loading {
                downloading: true, ..
            } => Self::Show,
            Status::Done { .. } => Self::ShowFor(DONE_FOR),
            Status::Failed { .. } => Self::ShowFor(FAILED_FOR),
        }
    }
}

/// The Rune app that runs the overlay's event loop; it owns the thread that calls `try_run`.
pub type Ui = App<()>;

/// The windowless Rune app with the hidden overlay, and the handle that feeds it from any thread.
/// `takes_clicks` lets the cancel button take the pointer; everything else passes clicks through.
#[must_use]
pub fn prepare(takes_clicks: bool) -> (Ui, Overlay) {
    let mut app = App::new(AppSpec::new(
        OVERLAY_TITLE,
        (1.0, 1.0),
        || (),
        |(), _| rune::el(),
    ))
    .windowless()
    .args(Vec::new());
    let spec = OverlaySpec {
        name: OVERLAY_TITLE,
        size: (gunmetal::WIDTH, gunmetal::HEIGHT),
        edge: ScreenEdge::Top,
        margin: 0.0,
        align: Align::Center,
        inset: 0.0,
        interactive: false,
        view: gunmetal::view,
    };
    let options = OverlayOptions {
        input: if takes_clicks {
            OverlayInput::Regions
        } else {
            OverlayInput::None
        },
        xwayland_input: takes_clicks,
        ..OverlayOptions::default()
    };
    let handle = app.overlay(
        spec.options(options).poll(gunmetal::poll),
        View::new(takes_clicks),
    );
    (
        app,
        Overlay {
            handle,
            generation: Arc::default(),
        },
    )
}

/// Shows and hides the overlay as the status asks.
pub struct Overlay {
    handle: OverlayHandle<View>,
    /// Bumped by every change, so a delayed hide that was overtaken does nothing.
    generation: Arc<AtomicU64>,
}

impl Overlay {
    /// Moves the overlay `margin` points below the top of the screen.
    pub fn place(&self, margin: f64) {
        #[expect(clippy::cast_possible_truncation, reason = "a panel height in points")]
        self.handle.place(ScreenEdge::Top, margin as f32);
    }

    pub fn on_cancel(&self, cancel: impl Fn() + Send + Sync + 'static) {
        let cancel: Arc<dyn Fn() + Send + Sync> = Arc::new(cancel);
        self.handle.update(move |view, _| view.on_cancel(cancel));
    }

    pub fn show_draft(&self, show: bool) {
        self.handle.update(move |view, _| view.set_draft(show));
    }

    /// The microphone's loudness, 0 to 1; the wave picks it up on its next frame.
    pub fn level(&self, level: f32) {
        self.handle.update_if(move |view, _| {
            view.set_level(level);
            false
        });
    }

    pub fn show(&self, status: &Status, visibility: Visibility) {
        let next = status.clone();
        self.handle.update(move |view, tx| {
            view.set_status(next, tx.clock());
            // Like the old overlay's styles, a new status lands at once rather than on Rune's springs.
            tx.settle();
            tx.wake_after(Duration::ZERO);
        });
        self.apply(visibility);
    }

    pub fn apply(&self, visibility: Visibility) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        match visibility {
            Visibility::Show => self.handle.show(),
            Visibility::Hide => self.handle.hide(),
            Visibility::ShowFor(delay) => {
                self.handle.show();
                let handle = self.handle.clone();
                let current = Arc::clone(&self.generation);
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(delay).await;
                    if current.load(Ordering::SeqCst) == generation {
                        handle.hide();
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use thalvi_desktop::capture::CaptureRoute;

    use crate::status::FailureKind;

    use super::*;

    #[test]
    fn the_overlay_shows_from_the_wake_word_and_hides_after_a_confirmation_or_a_failure() {
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
                    notice: None,
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
                    copies: false,
                },
                Visibility::Show,
            ),
            (
                Status::Writing {
                    target: target.clone(),
                    text: String::new(),
                },
                Visibility::Show,
            ),
            (
                Status::Thinking {
                    target: target.clone(),
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
                    kind: FailureKind::Other,
                    notice: String::new(),
                    headline: "Failed",
                    causes: Vec::new(),
                },
                Visibility::ShowFor(Duration::from_secs(5)),
            ),
            (
                Status::Loading {
                    notice: String::new(),
                    downloading: true,
                },
                Visibility::Show,
            ),
            (
                Status::Loading {
                    notice: String::new(),
                    downloading: false,
                },
                Visibility::Hide,
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
