//! When the overlay is shown, and the Rune overlay surface that draws it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rune::{Align, App, AppSpec, Cx, El, OverlayHandle, OverlaySpec, ScreenEdge};
use thalvi_desktop::host::OVERLAY_TITLE;

use crate::status::Status;

const WIDTH: f32 = 460.0;
const HEIGHT: f32 = 200.0;

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

/// What the overlay draws.
struct View {
    status: Status,
}

fn view(view: &View, _: &mut Cx<View>) -> El {
    let label = format!("{:?}", view.status);
    rune::row().key("overlay").w_full().h_full().center().child(
        rune::el()
            .key("box")
            .px(14.0)
            .py(8.0)
            .radius(8.0)
            .fill(rune::oklch(0.2, 0.0, 0.0))
            .child(rune::text(label).key("label").color(rune::Color::WHITE)),
    )
}

/// The Rune app that runs the overlay's event loop; it owns the thread that calls `try_run`.
pub type Ui = App<()>;

/// The windowless Rune app with the hidden overlay, and the handle that feeds it from any thread.
#[must_use]
pub fn prepare() -> (Ui, Overlay) {
    let mut app = App::new(AppSpec::new(
        OVERLAY_TITLE,
        (1.0, 1.0),
        || (),
        |(), _| rune::el(),
    ))
    .windowless()
    .args(Vec::new());
    let handle = app.overlay(
        OverlaySpec {
            name: OVERLAY_TITLE,
            size: (WIDTH, HEIGHT),
            edge: ScreenEdge::Top,
            margin: 0.0,
            align: Align::Center,
            inset: 0.0,
            interactive: false,
            view,
        },
        View {
            status: Status::Paused,
        },
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
    pub fn show(&self, status: &Status, visibility: Visibility) {
        let next = status.clone();
        self.handle.update(move |view, _| view.status = next);
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
