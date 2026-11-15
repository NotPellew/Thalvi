//! The status overlay: when it shows, and its own thread, which draws it on the CPU and hands each
//! frame to the desktop's overlay surface.

mod module;
mod neon;
mod paint;
mod painter;
mod view;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use thalvi_desktop::config::Look;
use thalvi_desktop::host::{
    HostError, OverlayEvent, OverlayFrame, OverlayPlan, OverlaySurface, OverlayWaker,
    overlay_failure,
};

use crate::status::{Failure, FailureKind, LastError, Status};
use painter::Painter;
use view::View;

/// How long a delivery's confirmation stays on screen.
const DONE_FOR: Duration = Duration::from_secs(1);

/// How long a failure's headline stays on screen; the tray and Settings keep the failure.
const FAILED_FOR: Duration = Duration::from_secs(5);

/// How soon a failed show or present may be tried again.
const RETRY_AFTER: Duration = Duration::from_secs(1);

/// How soon a show still waiting on the compositor is looked at again.
const PLACE_AFTER: Duration = Duration::from_millis(50);

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
            | Status::Transcribing { .. }
            | Status::Merging { .. }
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
    Look(Look),
    /// Whether the system's theme is dark.
    Theme(bool),
}

/// Where an overlay failure is shown, off the overlay's own thread.
pub struct FailureSink {
    /// Records the failure the tray and Settings copy.
    note: Arc<dyn Fn(Option<Failure>) + Send + Sync>,
}

impl FailureSink {
    /// `show` receives the failure, or `None` once the surface has recovered.
    pub fn new(show: impl Fn(Option<Failure>) + Send + Sync + 'static) -> Self {
        Self {
            note: Arc::new(show),
        }
    }

    fn show(&self, failure: Option<Failure>) {
        (self.note)(failure);
    }
}

/// Feeds the overlay's thread from any thread. What it is told before the thread starts waits.
pub struct Overlay {
    messages: mpsc::Sender<Message>,
    waiting: Mutex<Option<mpsc::Receiver<Message>>>,
    waker: Arc<Mutex<Option<OverlayWaker>>>,
    /// The mic level as `f32` bits; read at each frame rather than sent.
    level: Arc<AtomicU32>,
    /// Overlay failures the tray and Settings show, beside the engine's own status.
    failures: LastError,
    sink: Mutex<Option<FailureSink>>,
}

impl Default for Overlay {
    fn default() -> Self {
        let (messages, waiting) = mpsc::channel();
        Self {
            messages,
            waiting: Mutex::new(Some(waiting)),
            waker: Arc::default(),
            level: Arc::default(),
            failures: LastError::default(),
            sink: Mutex::default(),
        }
    }
}

impl Overlay {
    /// Where overlay failures are shown. Set before [`Self::start`].
    pub fn report_to(&self, sink: FailureSink) {
        *self.sink.lock().unwrap_or_else(PoisonError::into_inner) = Some(sink);
    }

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
        let (waker, level, failures, sink) = (
            Arc::clone(&self.waker),
            Arc::clone(&self.level),
            self.failures.clone(),
            self.sink
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
        let started = std::thread::Builder::new()
            .name("overlay".to_owned())
            .spawn(move || {
                run(
                    Thread {
                        plan,
                        takes_clicks,
                        messages,
                        waker,
                        level,
                        failures,
                        sink,
                    },
                    &cancel,
                );
            });
        if let Err(error) = started {
            self.report(&format!("The status overlay could not start: {error}."));
        }
    }

    /// The latest overlay failure, for the tray and Settings.
    #[must_use]
    pub fn failure(&self) -> Option<Failure> {
        self.failures.get()
    }

    fn report(&self, reason: &str) {
        publish(
            &self.failures,
            self.sink
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref(),
            reason,
        );
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

    pub fn set_look(&self, look: Look) {
        self.send(Message::Look(look));
    }

    /// Follows the system's theme: dark, or light.
    pub fn set_dark(&self, dark: bool) {
        self.send(Message::Theme(dark));
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
struct Thread {
    plan: OverlayPlan,
    takes_clicks: bool,
    messages: mpsc::Receiver<Message>,
    waker: Arc<Mutex<Option<OverlayWaker>>>,
    level: Arc<AtomicU32>,
    failures: LastError,
    sink: Option<FailureSink>,
}

fn run(thread: Thread, cancel: &dyn Fn()) {
    let Thread {
        plan,
        takes_clicks,
        messages,
        waker,
        level,
        failures,
        sink,
    } = thread;
    let mut surface = match plan.open(view::WIDTH, view::HEIGHT, takes_clicks) {
        Ok(surface) => surface,
        Err(error) => {
            publish(&failures, sink.as_ref(), &error.to_string());
            return;
        }
    };
    *waker.lock().unwrap_or_else(PoisonError::into_inner) = Some(surface.waker());
    // Fonts load here, before the first show, so no show waits for them.
    let Some(mut painter) = Painter::new(surface.scale(), Look::default(), true) else {
        publish(
            &failures,
            sink.as_ref(),
            "The status overlay could not be drawn at this scale.",
        );
        return;
    };
    let mut overlay = Overlaying::new(takes_clicks, Instant::now());
    loop {
        while let Ok(message) = messages.try_recv() {
            overlay.take(message, Instant::now());
        }
        let level = f32::from_bits(level.load(Ordering::Relaxed));
        let outcome = overlay.settle(&mut surface, &mut painter, level, Instant::now());
        match outcome {
            Draw::Shown => {
                failures.clear_overlay();
                if let Some(sink) = sink.as_ref() {
                    sink.show(None);
                }
            }
            Draw::Failed(reason) => publish(&failures, sink.as_ref(), &reason),
            Draw::Waiting | Draw::Idle => {}
        }
        match surface.wait(overlay.deadline()) {
            OverlayEvent::Woken | OverlayEvent::Pending => {}
            OverlayEvent::Press { x, y } => {
                if overlay.cancels(&painter, x, y) {
                    cancel();
                }
            }
            OverlayEvent::Due => overlay.due(Instant::now()),
            OverlayEvent::Lost(reason) => {
                overlay.lost();
                publish(&failures, sink.as_ref(), &reason);
            }
        }
    }
}

/// Keeps `reason` where the tray and Settings read it, once per distinct reason, and shows it.
fn publish(failures: &LastError, sink: Option<&FailureSink>, reason: &str) {
    let (headline, notice) = overlay_failure(reason);
    if failures.get().is_some_and(|held| held.notice == notice) {
        return;
    }
    let failure = Failure {
        notice,
        headline,
        causes: vec![reason.to_owned()],
        kind: FailureKind::Other,
    };
    failures.note(failure.clone());
    if let Some(sink) = sink {
        sink.show(Some(failure));
    }
}

/// What the overlay's thread asks of its surface: the desktop's, or a test's.
trait Surface {
    fn show(&mut self, frame: &OverlayFrame) -> Result<(), HostError>;
    fn present(&mut self, frame: &OverlayFrame) -> Result<(), HostError>;
    fn hide(&mut self);
}

impl Surface for OverlaySurface {
    fn show(&mut self, frame: &OverlayFrame) -> Result<(), HostError> {
        Self::show(self, frame)
    }

    fn present(&mut self, frame: &OverlayFrame) -> Result<(), HostError> {
        Self::present(self, frame)
    }

    fn hide(&mut self) {
        Self::hide(self);
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
    /// The next time a failed show or present may be tried.
    retry_at: Option<Instant>,
    /// A show has committed a surface the compositor has not placed.
    placing: bool,
    /// The look, and whether the system's theme is dark.
    style: (Look, bool),
}

/// What one settle did with the surface.
enum Draw {
    /// Nothing was asked of the surface.
    Idle,
    /// A show is waiting for the compositor.
    Waiting,
    /// The surface is showing.
    Shown,
    /// The surface could not be shown; the reason is new.
    Failed(String),
}

impl Overlaying {
    fn new(takes_clicks: bool, now: Instant) -> Self {
        Self {
            view: View::new(takes_clicks),
            started: now,
            visible: false,
            wanted: false,
            hide_at: None,
            frame_at: None,
            stale: false,
            failure: None,
            retry_at: None,
            placing: false,
            style: (Look::default(), true),
        }
    }

    fn clock(&self, now: Instant) -> f64 {
        now.saturating_duration_since(self.started).as_secs_f64()
    }

    fn take(&mut self, message: Message, now: Instant) {
        match message {
            Message::Status(status, visibility) => {
                self.view.set_status(*status, self.clock(now));
                self.stale = true;
                self.want(visibility, now);
            }
            Message::Visibility(visibility) => self.want(visibility, now),
            Message::Draft(show) => {
                self.view.set_draft(show);
                self.stale = true;
            }
            Message::Look(look) => {
                self.style.0 = look;
                self.stale = true;
            }
            Message::Theme(dark) => {
                self.style.1 = dark;
                self.stale = true;
            }
        }
    }

    fn want(&mut self, visibility: Visibility, now: Instant) {
        let (wanted, hide_at) = match visibility {
            Visibility::Show => (true, None),
            Visibility::Hide => (false, None),
            Visibility::ShowFor(time) => (true, Some(now + time)),
        };
        self.wanted = wanted;
        self.hide_at = hide_at;
        // A status that asks to show again also asks to draw, so a vanished surface is made again.
        if wanted {
            self.stale = true;
        }
    }

    /// A deadline passed: hide once shown long enough, or draw the next frame.
    fn due(&mut self, now: Instant) {
        if self.hide_at.is_some_and(|at| now >= at) {
            self.wanted = false;
            self.hide_at = None;
        } else if self.frame_at.is_some_and(|at| now >= at) {
            self.stale = true;
        }
    }

    /// Whether a press at `(x, y)` points cancels: only on a shown overlay's cancel button.
    fn cancels(&self, painter: &Painter, x: f32, y: f32) -> bool {
        self.visible && painter.cancels(&self.view, x, y)
    }

    /// The surface vanished or its connection ended: the next show makes it again.
    fn lost(&mut self) {
        self.visible = false;
        self.placing = false;
        self.stale = true;
        self.retry_at = None;
    }

    fn deadline(&self) -> Option<Instant> {
        [self.hide_at, self.frame_at, self.retry_at]
            .into_iter()
            .flatten()
            .min()
    }

    /// Brings the surface in line with what is wanted, drawing a frame when the view moved.
    fn settle(
        &mut self,
        surface: &mut impl Surface,
        painter: &mut Painter,
        level: f32,
        now: Instant,
    ) -> Draw {
        if !self.wanted {
            if self.visible || self.placing {
                surface.hide();
                self.visible = false;
                self.placing = false;
            }
            self.frame_at = None;
            self.retry_at = None;
            return Draw::Idle;
        }
        if self.retry_at.is_some_and(|at| now < at) {
            return Draw::Idle;
        }
        if self.visible && !self.stale {
            return Draw::Idle;
        }
        let clock = self.clock(now);
        painter.restyle(self.style.0, self.style.1);
        let next = match (self.view.poll(clock, level), painter.motion(clock)) {
            (Some(view), Some(look)) => Some(view.min(look)),
            (view, look) => view.or(look),
        };
        let frame = painter.draw(&self.view, clock);
        let drawn = if self.visible {
            surface.present(frame)
        } else {
            surface.show(frame)
        };
        let outcome = match drawn {
            Ok(()) => {
                self.visible = true;
                self.placing = false;
                self.stale = false;
                self.failure = None;
                self.retry_at = None;
                Draw::Shown
            }
            Err(HostError::Unsupported(message)) if message.contains("not placed yet") => {
                self.visible = false;
                self.placing = true;
                self.retry_at = Some(now + PLACE_AFTER);
                Draw::Waiting
            }
            Err(error) => {
                let message = error.to_string();
                self.visible = false;
                self.placing = false;
                // A vanished surface is shown again as soon as the next status asks. A refusal waits.
                self.retry_at = (!message.contains("closed")).then_some(now + RETRY_AFTER);
                let fresh = self.failure.as_ref() != Some(&message);
                self.failure = Some(message.clone());
                if fresh {
                    Draw::Failed(message)
                } else {
                    Draw::Idle
                }
            }
        };
        self.frame_at = next.map(|wait| now + wait);
        outcome
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use thalvi_desktop::capture::CaptureRoute;

    use crate::status::FailureKind;

    use super::*;

    /// Records what the overlay's thread asked of it.
    #[derive(Default)]
    struct Fake {
        calls: Vec<&'static str>,
        /// Refuses show and present while set.
        refuse: bool,
        /// The next present reports the surface gone.
        vanish: bool,
        /// Show answers that the compositor has not placed the surface yet.
        pending: bool,
    }

    impl Surface for Fake {
        fn show(&mut self, _: &OverlayFrame) -> Result<(), HostError> {
            self.calls.push("show");
            if self.pending {
                return Err(HostError::Unsupported(
                    "the layer surface is not placed yet".to_owned(),
                ));
            }
            if self.refuse {
                return Err(HostError::Unsupported("The desktop said no.".to_owned()));
            }
            Ok(())
        }

        fn present(&mut self, _: &OverlayFrame) -> Result<(), HostError> {
            self.calls.push("present");
            if self.vanish {
                self.vanish = false;
                return Err(HostError::Unsupported(
                    "the layer surface closed".to_owned(),
                ));
            }
            if self.refuse {
                return Err(HostError::Unsupported("The desktop said no.".to_owned()));
            }
            Ok(())
        }

        fn hide(&mut self) {
            self.calls.push("hide");
        }
    }

    fn capturing() -> Status {
        Status::Capturing {
            target: None,
            committed: "hello".to_owned(),
            partial: None,
            notice: None,
        }
    }

    #[test]
    fn the_surface_shows_once_redraws_what_changed_and_hides_when_told_or_when_its_time_is_up() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut painter = Painter::new(1.0, Look::Gunmetal, true).unwrap();
        let (mut surface, mut overlay) = (Fake::default(), Overlaying::new(false, start));
        let mut step = |message: Option<Message>, due: bool, now: Instant| {
            if let Some(message) = message {
                overlay.take(message, now);
            }
            if due {
                overlay.due(now);
            }
            let _ = overlay.settle(&mut surface, &mut painter, 0.5, now);
            overlay.deadline()
        };
        let first = step(
            Some(Message::Status(Box::new(capturing()), Visibility::Show)),
            false,
            at(0),
        );
        assert_eq!(first, Some(at(0) + view::FRAME), "the wave's next frame");
        // Nothing changed and no frame is due: the surface is left alone.
        step(None, false, at(10));
        // The wave asks for its next frame.
        step(None, true, at(40));
        let done = Box::new(Status::Done {
            target: None,
            route: CaptureRoute::Paste,
            notice: "Delivered.".to_owned(),
        });
        let deadline = step(
            Some(Message::Status(
                done,
                Visibility::ShowFor(Duration::from_secs(1)),
            )),
            false,
            at(100),
        );
        assert_eq!(
            deadline,
            Some(at(1100)),
            "the confirmation's time is the only deadline"
        );
        step(None, true, at(1100));
        step(
            Some(Message::Status(Box::new(capturing()), Visibility::Show)),
            false,
            at(2000),
        );
        step(Some(Message::Visibility(Visibility::Hide)), false, at(2100));
        assert_eq!(
            surface.calls,
            ["show", "present", "present", "hide", "show", "hide"]
        );
    }

    #[test]
    fn a_surface_that_refuses_to_show_leaves_the_overlay_hidden_and_the_next_status_tries_again() {
        let start = Instant::now();
        let mut painter = Painter::new(1.0, Look::Gunmetal, true).unwrap();
        let mut surface = Fake {
            refuse: true,
            ..Fake::default()
        };
        let mut overlay = Overlaying::new(true, start);
        overlay.take(
            Message::Status(Box::new(capturing()), Visibility::Show),
            start,
        );
        overlay.settle(&mut surface, &mut painter, 0.0, start);
        assert!(
            !overlay.cancels(&painter, 386.0, 17.0),
            "a hidden overlay cancels nothing"
        );
        surface.refuse = false;
        overlay.take(
            Message::Status(Box::new(capturing()), Visibility::Show),
            start + RETRY_AFTER,
        );
        overlay.settle(&mut surface, &mut painter, 0.0, start + RETRY_AFTER);
        assert_eq!(surface.calls, ["show", "show"]);
        assert!(
            overlay.cancels(&painter, 386.0, 17.0),
            "a shown overlay's button cancels"
        );
    }

    #[test]
    fn the_overlay_shows_from_the_wake_word_and_hides_after_a_confirmation_or_a_failure() {
        let target = Some("kitty".to_owned());
        let statuses = [
            (Status::Paused, Visibility::Hide),
            (Status::Listening, Visibility::Hide),
            (Status::KeyboardOnly, Visibility::Hide),
            (
                Status::Starting {
                    target: target.clone(),
                    loading: false,
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
    #[test]
    fn what_the_overlay_is_told_before_its_thread_starts_waits_for_it_in_order() {
        let overlay = Overlay::default();
        overlay.show_draft(false);
        overlay.set_look(Look::Neon);
        overlay.set_dark(false);
        overlay.show(&capturing(), Visibility::Show);
        overlay.apply(Visibility::Hide);
        overlay.level(0.25);
        let waiting = overlay.waiting.lock().unwrap().take().unwrap();
        let start = Instant::now();
        let mut thread = Overlaying::new(false, start);
        let mut taken = 0;
        for message in waiting.try_iter() {
            thread.take(message, start);
            taken += 1;
        }
        assert_eq!(taken, 5);
        assert!(!thread.view.show_draft);
        assert_eq!(thread.style, (Look::Neon, false));
        assert!(matches!(thread.view.status, Status::Capturing { .. }));
        assert!(!thread.wanted, "the later hide wins");
        assert!(
            (f32::from_bits(overlay.level.load(Ordering::Relaxed)) - 0.25).abs() < f32::EPSILON
        );
    }

    #[test]
    fn the_countdown_counts_down_on_the_overlay_threads_own_clock() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut painter = Painter::new(1.0, Look::Gunmetal, true).unwrap();
        let (mut surface, mut overlay) = (Fake::default(), Overlaying::new(false, start));
        let pending = Status::Pending {
            target: None,
            route: CaptureRoute::Paste,
            text: "hello".to_owned(),
            waiting_for_quiet: false,
            pause_ms: 2500,
            copies: false,
        };
        overlay.take(
            Message::Status(Box::new(pending), Visibility::Show),
            at(10_000),
        );
        overlay.settle(&mut surface, &mut painter, 0.0, at(10_000));
        assert_eq!(overlay.view.label.as_deref(), Some("PASTE IN 3 s"));
        overlay.due(at(11_000));
        overlay.settle(&mut surface, &mut painter, 0.0, at(11_000));
        assert_eq!(overlay.view.label.as_deref(), Some("PASTE IN 2 s"));
    }

    #[test]
    fn a_vanished_surface_is_shown_again_on_the_next_status() {
        let start = Instant::now();
        let mut painter = Painter::new(1.0, Look::Gunmetal, true).unwrap();
        let mut surface = Fake::default();
        let mut overlay = Overlaying::new(false, start);
        overlay.take(
            Message::Status(Box::new(capturing()), Visibility::Show),
            start,
        );
        overlay.settle(&mut surface, &mut painter, 0.0, start);
        surface.vanish = true;
        overlay.stale = true;
        overlay.settle(&mut surface, &mut painter, 0.0, start + view::FRAME);
        overlay.take(
            Message::Status(Box::new(capturing()), Visibility::Show),
            start + view::FRAME,
        );
        overlay.settle(&mut surface, &mut painter, 0.0, start + view::FRAME);
        assert_eq!(surface.calls, ["show", "present", "show"]);
    }

    #[test]
    fn a_failed_show_or_present_retries_at_most_once_a_second() {
        let start = Instant::now();
        let mut painter = Painter::new(1.0, Look::Gunmetal, true).unwrap();
        let mut surface = Fake {
            refuse: true,
            ..Fake::default()
        };
        let mut overlay = Overlaying::new(false, start);
        let mut again = |overlay: &mut Overlaying, surface: &mut Fake, now: Instant| {
            overlay.take(
                Message::Status(Box::new(capturing()), Visibility::Show),
                now,
            );
            overlay.settle(surface, &mut painter, 0.0, now);
        };
        again(&mut overlay, &mut surface, start);
        again(
            &mut overlay,
            &mut surface,
            start + Duration::from_millis(40),
        );
        again(
            &mut overlay,
            &mut surface,
            start + Duration::from_millis(999),
        );
        assert_eq!(surface.calls, ["show"]);
        again(&mut overlay, &mut surface, start + RETRY_AFTER);
        assert_eq!(surface.calls, ["show", "show"]);
        surface.refuse = false;
        surface.calls.clear();
        again(&mut overlay, &mut surface, start + 2 * RETRY_AFTER);
        assert_eq!(surface.calls, ["show"]);
        surface.refuse = true;
        surface.calls.clear();
        overlay.stale = true;
        overlay.settle(&mut surface, &mut painter, 0.0, start + 3 * RETRY_AFTER);
        overlay.take(
            Message::Status(Box::new(capturing()), Visibility::Show),
            start + 3 * RETRY_AFTER + Duration::from_millis(40),
        );
        overlay.settle(
            &mut surface,
            &mut painter,
            0.0,
            start + 3 * RETRY_AFTER + Duration::from_millis(40),
        );
        assert_eq!(surface.calls, ["present"]);
        overlay.settle(&mut surface, &mut painter, 0.0, start + 4 * RETRY_AFTER);
        assert_eq!(surface.calls, ["present", "show"]);
    }

    #[test]
    fn a_pending_configure_does_not_hold_hide_or_press() {
        let start = Instant::now();
        let mut painter = Painter::new(1.0, Look::Gunmetal, true).unwrap();
        let mut surface = Fake {
            pending: true,
            ..Fake::default()
        };
        let mut overlay = Overlaying::new(true, start);
        overlay.take(
            Message::Status(Box::new(capturing()), Visibility::Show),
            start,
        );
        overlay.settle(&mut surface, &mut painter, 0.0, start);
        let hide_at = start + Duration::from_millis(20);
        overlay.take(Message::Visibility(Visibility::Hide), hide_at);
        overlay.settle(&mut surface, &mut painter, 0.0, hide_at);
        assert_eq!(surface.calls, ["show", "hide"]);
        assert!(
            hide_at.saturating_duration_since(start) < Duration::from_millis(100),
            "hide ran without the configure wait"
        );
        assert!(!overlay.visible);
        assert!(
            !overlay.cancels(&painter, 386.0, 17.0),
            "a press on an unplaced overlay cancels nothing"
        );
    }

    #[test]
    fn an_overlay_failure_reaches_the_tray_once_and_clears_when_it_recovers() {
        let shown = Arc::new(Mutex::new(Vec::new()));
        let overlay = Overlay::default();
        let record = Arc::clone(&shown);
        overlay.report_to(FailureSink::new(move |failure| {
            record
                .lock()
                .unwrap()
                .push(failure.map(|failure| failure.notice));
        }));
        overlay.report("The desktop said no.");
        overlay.report("The desktop said no.");
        let held = overlay.failure().expect("a failure");
        assert_eq!(held.headline, "Status overlay unavailable");
        assert!(held.notice.contains("The desktop said no."));
        overlay.report("The status overlay lost its Wayland connection.");
        assert_eq!(
            shown.lock().unwrap().len(),
            2,
            "the same failure is shown once"
        );
        overlay.failures.clear_overlay();
        assert!(overlay.failure().is_none());
    }
}
