//! The status overlay: when it shows, and its own thread, which draws it on the CPU and hands each
//! frame to the desktop's overlay surface.

mod gunmetal;
mod paint;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use thalvi_desktop::host::{OverlayEvent, OverlayPlan, OverlaySurface, OverlayWaker};

use crate::status::Status;
use gunmetal::{Painter, View};

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
            | Status::KeyboardOnly
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

/// What the overlay's thread is told.
enum Message {
    Status(Box<Status>, Visibility),
    Visibility(Visibility),
    Draft(bool),
}

/// Feeds the overlay's thread from any thread. What it is told before the thread starts waits.
pub struct Overlay {
    messages: mpsc::Sender<Message>,
    waiting: Mutex<Option<mpsc::Receiver<Message>>>,
    waker: Arc<Mutex<Option<OverlayWaker>>>,
    /// The mic level as `f32` bits; read at each frame rather than sent.
    level: Arc<AtomicU32>,
}

impl Default for Overlay {
    fn default() -> Self {
        let (messages, waiting) = mpsc::channel();
        Self {
            messages,
            waiting: Mutex::new(Some(waiting)),
            waker: Arc::default(),
            level: Arc::default(),
        }
    }
}

impl Overlay {
    /// Starts the overlay's thread, which opens `plan`'s surface; `cancel` runs when its cancel
    /// button is pressed. A second call does nothing.
    pub fn start(&self, plan: OverlayPlan, takes_clicks: bool, cancel: impl Fn() + Send + 'static) {
        let Some(messages) = self
            .waiting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        else {
            return;
        };
        let (waker, level) = (Arc::clone(&self.waker), Arc::clone(&self.level));
        let started = std::thread::Builder::new()
            .name("overlay".to_owned())
            .spawn(move || run(plan, takes_clicks, &messages, &waker, &level, &cancel));
        if let Err(error) = started {
            println!("The status overlay could not start: {error}. It stays off.");
        }
    }

    /// Shows `status`, and shows or hides the overlay as `visibility` says.
    pub fn show(&self, status: &Status, visibility: Visibility) {
        self.send(Message::Status(Box::new(status.clone()), visibility));
    }

    pub fn apply(&self, visibility: Visibility) {
        self.send(Message::Visibility(visibility));
    }

    /// Whether the live draft shows on the overlay's line.
    pub fn show_draft(&self, show: bool) {
        self.send(Message::Draft(show));
    }

    /// The microphone's loudness, 0 to 1; the wave picks it up on its next frame.
    pub fn level(&self, level: f32) {
        self.level.store(level.to_bits(), Ordering::Relaxed);
    }

    fn send(&self, message: Message) {
        let _ = self.messages.send(message);
        if let Some(waker) = &*self.waker.lock().unwrap_or_else(PoisonError::into_inner) {
            waker.wake();
        }
    }
}

/// The overlay's thread: opens the surface, then shows, animates and hides it as told.
fn run(
    plan: OverlayPlan,
    takes_clicks: bool,
    messages: &mpsc::Receiver<Message>,
    waker: &Mutex<Option<OverlayWaker>>,
    level: &AtomicU32,
    cancel: &dyn Fn(),
) {
    let mut surface = match plan.open(gunmetal::WIDTH, gunmetal::HEIGHT, takes_clicks) {
        Ok(surface) => surface,
        Err(error) => {
            println!("{error} The status overlay stays off.");
            return;
        }
    };
    *waker.lock().unwrap_or_else(PoisonError::into_inner) = Some(surface.waker());
    // Fonts load here, before the first show, so no show waits for them.
    let Some(mut painter) = Painter::new(surface.scale()) else {
        println!("The status overlay could not be drawn at this scale. It stays off.");
        return;
    };
    let mut overlay = Overlaying::new(takes_clicks);
    loop {
        while let Ok(message) = messages.try_recv() {
            overlay.take(message);
        }
        overlay.settle(
            &mut surface,
            &mut painter,
            f32::from_bits(level.load(Ordering::Relaxed)),
        );
        match surface.wait(overlay.deadline()) {
            OverlayEvent::Woken => {}
            OverlayEvent::Press { x, y } => {
                if overlay.visible && overlay.view.cancels(x, y) {
                    cancel();
                }
            }
            OverlayEvent::Due => overlay.due(),
        }
    }
}

/// What the overlay's thread keeps between waits.
struct Overlaying {
    view: View,
    started: Instant,
    /// Whether the surface shows.
    visible: bool,
    /// Whether the surface should show.
    wanted: bool,
    /// When a shown-for-a-while overlay hides.
    hide_at: Option<Instant>,
    /// When the next frame is due while something moves.
    frame_at: Option<Instant>,
    /// Whether the view changed since it was last drawn.
    stale: bool,
    /// The last failure to show, reported once.
    failure: Option<String>,
}

impl Overlaying {
    fn new(takes_clicks: bool) -> Self {
        Self {
            view: View::new(takes_clicks),
            started: Instant::now(),
            visible: false,
            wanted: false,
            hide_at: None,
            frame_at: None,
            stale: false,
            failure: None,
        }
    }

    fn clock(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    fn take(&mut self, message: Message) {
        match message {
            Message::Status(status, visibility) => {
                self.view.set_status(*status, self.clock());
                self.stale = true;
                self.want(visibility);
            }
            Message::Visibility(visibility) => self.want(visibility),
            Message::Draft(show) => {
                self.view.set_draft(show);
                self.stale = true;
            }
        }
    }

    fn want(&mut self, visibility: Visibility) {
        let (wanted, hide_at) = match visibility {
            Visibility::Show => (true, None),
            Visibility::Hide => (false, None),
            Visibility::ShowFor(time) => (true, Some(Instant::now() + time)),
        };
        self.wanted = wanted;
        self.hide_at = hide_at;
    }

    /// A deadline passed: hide once shown long enough, or draw the next frame.
    fn due(&mut self) {
        if self.hide_at.is_some_and(|at| Instant::now() >= at) {
            self.wanted = false;
            self.hide_at = None;
        } else if self.frame_at.is_some_and(|at| Instant::now() >= at) {
            self.stale = true;
        }
    }

    fn deadline(&self) -> Option<Instant> {
        match (self.hide_at, self.frame_at) {
            (Some(hide), Some(frame)) => Some(hide.min(frame)),
            (hide, frame) => hide.or(frame),
        }
    }

    /// Brings the surface in line with what is wanted, drawing a frame when the view moved.
    fn settle(&mut self, surface: &mut OverlaySurface, painter: &mut Painter, level: f32) {
        if !self.wanted {
            if self.visible {
                surface.hide();
                self.visible = false;
            }
            self.frame_at = None;
            return;
        }
        if self.visible && !self.stale {
            return;
        }
        let next = self.view.poll(self.clock(), level);
        let frame = painter.draw(&self.view);
        let drawn = if self.visible {
            surface.present(frame)
        } else {
            surface.show(frame)
        };
        match drawn {
            Ok(()) => {
                self.visible = true;
                self.failure = None;
            }
            Err(error) => {
                let message = error.to_string();
                if self.failure.as_ref() != Some(&message) {
                    println!("{message} The status overlay stays hidden.");
                }
                self.failure = Some(message);
            }
        }
        self.stale = false;
        self.frame_at = next.map(|wait| Instant::now() + wait);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

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
