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
use thalvi_desktop::engine::PreviewError;
use thalvi_desktop::host::{
    HostError, OverlayEvent, OverlayFrame, OverlayPlan, OverlaySurface, OverlayWaker,
    overlay_failure,
};
use thalvi_desktop::metrics::{Event, Metrics, RecordKinds, Recorder};
use thalvi_desktop::theme::{Style, defaults_of};

use crate::status::{Failure, FailureKind, LastError, Status};
use painter::Painter;
pub use view::{Sample, View};

/// Draws preview frames with one painter, restyling it when the look or palette changes.
#[derive(Default)]
pub struct Renderer {
    painter: Option<Painter>,
    scale: f32,
}

impl Renderer {
    /// Draws `view` in `style` at `scale`, rebuilding the painter only when the scale changes.
    /// Returns the pixel dimensions and straight-alpha RGBA bytes, or `None` if no skin was built.
    pub fn draw(
        &mut self,
        view: &View,
        style: Arc<Style>,
        dark: bool,
        scale: f32,
    ) -> Option<(u32, u32, Vec<u8>)> {
        if self.painter.is_none() || self.scale.to_bits() != scale.to_bits() {
            self.painter = Some(Painter::new(scale, style, dark)?);
            self.scale = scale;
        } else {
            let painter = self.painter.as_mut()?;
            if !painter.restyle(&style, dark) {
                return None;
            }
        }
        Some(self.painter.as_mut()?.rgba(view))
    }
}

/// Draws a synthetic view with a fresh live-overlay renderer, without opening a desktop surface.
/// Returns the pixel dimensions and straight-alpha RGBA bytes, or `None` if allocation fails.
#[must_use]
pub fn render_rgba(
    view: &View,
    style: Arc<Style>,
    dark: bool,
    scale: f32,
) -> Option<(u32, u32, Vec<u8>)> {
    Renderer::default().draw(view, style, dark, scale)
}

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
    /// which `dismiss_error` can hide sooner. Final Submit still shows the exact text when disabled.
    #[must_use]
    pub fn of(status: &Status, enabled: bool) -> Self {
        if status.requires_final_preview() {
            return Self::Show;
        }
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
            | Status::StartingChat { .. }
            | Status::Working { .. }
            | Status::Transcribing { .. }
            | Status::Finishing { .. }
            | Status::CleaningUp { .. }
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
    Status(Box<Status>),
    Enabled(bool),
    Visibility(Visibility),
    Draft(bool),
    Style(Arc<Style>),
    /// Whether the system's theme is dark.
    Theme(bool),
    /// The recordings choice now in effect. A later one replaces the metrics the overlay writes.
    Recordings(RecordKinds),
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
    /// button is pressed. A second call does nothing. `recorder` records each show, and a later
    /// [`Self::set_recordings`] changes which kinds it writes.
    pub fn start(
        &self,
        plan: OverlayPlan,
        takes_clicks: bool,
        recorder: Recorder,
        cancel: impl Fn() + Send + 'static,
    ) {
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
                        recorder,
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

    /// Hands `status` to the overlay's thread, which shows or hides as the status and the
    /// enabled switch say.
    pub fn show(&self, status: &Status) {
        self.send(Message::Status(Box::new(status.clone())));
    }

    /// Whether the "Show the overlay" setting is on; a change takes effect at once, without a
    /// new status.
    pub fn set_enabled(&self, enabled: bool) {
        self.send(Message::Enabled(enabled));
    }

    pub fn apply(&self, visibility: Visibility) {
        self.send(Message::Visibility(visibility));
    }

    /// Whether the live draft shows on the overlay's line.
    pub fn show_draft(&self, show: bool) {
        self.send(Message::Draft(show));
    }

    pub fn set_style(&self, style: Arc<Style>) {
        self.send(Message::Style(style));
    }

    /// Follows the system's theme: dark, or light.
    pub fn set_dark(&self, dark: bool) {
        self.send(Message::Theme(dark));
    }

    /// Records with `kinds` from now on. A show after metrics are turned off writes no line, and
    /// one after they are turned on does.
    pub fn set_recordings(&self, kinds: RecordKinds) {
        self.send(Message::Recordings(kinds));
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
    recorder: Recorder,
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
        recorder,
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
    let Some(mut painter) = Painter::new(
        surface.scale(),
        Arc::new(defaults_of(Look::default())),
        true,
    ) else {
        publish(
            &failures,
            sink.as_ref(),
            "The status overlay could not be drawn at this scale.",
        );
        return;
    };
    let mut overlay = Overlaying::with(takes_clicks, Instant::now(), recorder);
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
        headline: headline.to_owned(),
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

/// Where the surface is between waits.
enum Shown {
    /// Nothing is on screen.
    Hidden,
    /// A show committed a surface the compositor has not placed.
    Placing,
    /// The surface shows. `stale` means the view changed since the last draw.
    Showing { stale: bool },
}

/// Whether the surface should show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wanted {
    Hidden,
    Shown,
    /// Shown until this time, then hidden.
    Until(Instant),
}

/// What the overlay's thread keeps between waits.
struct Overlaying {
    view: View,
    started: Instant,
    /// Whether the "Show the overlay" setting is on.
    enabled: bool,
    wanted: Wanted,
    /// Where the surface is.
    shown: Shown,
    /// When the next frame is due while something moves.
    frame_at: Option<Instant>,
    /// The last failure to show, reported once.
    failure: Option<String>,
    /// The next time a failed show or present may be tried.
    retry_at: Option<Instant>,
    /// The look, and whether the system's theme is dark.
    style: Arc<Style>,
    dark: bool,
    style_dirty: bool,
    /// When the status that asked for this show arrived. `None` once its frame is attached or it failed.
    asked_at: Option<Instant>,
    /// The recorder the overlay started with. A recordings change switches its kinds in place.
    recorder: Recorder,
    metrics: Metrics,
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
    #[cfg(test)]
    fn new(takes_clicks: bool, now: Instant) -> Self {
        Self::with(takes_clicks, now, Recorder::disabled())
    }

    fn with(takes_clicks: bool, now: Instant, recorder: Recorder) -> Self {
        let metrics = recorder.metrics();
        Self {
            view: View::new(takes_clicks),
            started: now,
            enabled: true,
            wanted: Wanted::Hidden,
            shown: Shown::Hidden,
            frame_at: None,
            failure: None,
            retry_at: None,
            style: Arc::new(defaults_of(Look::default())),
            dark: true,
            style_dirty: false,
            asked_at: None,
            recorder,
            metrics,
        }
    }

    /// One show, from the status that asked for it. A later status while this one is still
    /// placing keeps the first ask, so the duration is the wait the user saw.
    fn note(&mut self, outcome: &str, now: Instant) {
        let Some(asked) = self.asked_at.take() else {
            return;
        };
        self.metrics.metric(
            Event::OverlayShown,
            outcome,
            Some(now.saturating_duration_since(asked)),
            serde_json::Value::Null,
        );
    }

    fn mark_stale(&mut self) {
        if let Shown::Showing { stale } = &mut self.shown {
            *stale = true;
        }
    }

    fn clock(&self, now: Instant) -> f64 {
        now.saturating_duration_since(self.started).as_secs_f64()
    }

    fn take(&mut self, message: Message, now: Instant) {
        match message {
            Message::Status(status) => {
                self.view.set_status(*status, self.clock(now));
                self.mark_stale();
                self.want(Visibility::of(&self.view.status, self.enabled), now);
            }
            Message::Enabled(enabled) if enabled != self.enabled => {
                self.enabled = enabled;
                let visibility = Visibility::of(&self.view.status, enabled);
                // A finished confirmation or failure headline is not brought back by switching on.
                if !matches!(visibility, Visibility::ShowFor(_)) {
                    self.want(visibility, now);
                }
            }
            Message::Visibility(visibility) => self.want(visibility, now),
            Message::Draft(show) => {
                self.view.set_draft(show);
                self.mark_stale();
            }
            Message::Style(style) if self.style != style => {
                self.style = style;
                self.style_dirty = true;
                self.mark_stale();
            }
            Message::Theme(dark) if self.dark != dark => {
                self.dark = dark;
                self.style_dirty = true;
                self.mark_stale();
            }
            Message::Enabled(_) | Message::Style(_) | Message::Theme(_) => {}
            Message::Recordings(kinds) => self.recorder.set_kinds(kinds),
        }
    }

    fn want(&mut self, visibility: Visibility, now: Instant) {
        if matches!(visibility, Visibility::Show | Visibility::ShowFor(_))
            && self.asked_at.is_none()
        {
            self.asked_at = Some(now);
        }
        self.wanted = match visibility {
            Visibility::Show => Wanted::Shown,
            Visibility::Hide => Wanted::Hidden,
            Visibility::ShowFor(time) => Wanted::Until(now + time),
        };
        // A status that asks to show again also asks to draw, so a vanished surface is made again.
        if self.wanted != Wanted::Hidden {
            self.mark_stale();
        }
    }

    /// A deadline passed: hide once shown long enough, or draw the next frame.
    fn due(&mut self, now: Instant) {
        if matches!(self.wanted, Wanted::Until(at) if now >= at) {
            self.wanted = Wanted::Hidden;
        } else if self.frame_at.is_some_and(|at| now >= at) {
            self.mark_stale();
        }
    }

    /// Whether a press at `(x, y)` points cancels: only on a shown overlay's cancel button.
    fn cancels(&self, painter: &Painter, x: f32, y: f32) -> bool {
        matches!(self.shown, Shown::Showing { .. }) && painter.cancels(&self.view, x, y)
    }

    /// The surface vanished or its connection ended: the next show makes it again.
    fn lost(&mut self) {
        if let Some(preview) = self.view.status.final_preview() {
            preview.failed(PreviewError::Unavailable);
        }
        self.shown = Shown::Hidden;
        self.retry_at = None;
    }

    fn deadline(&self) -> Option<Instant> {
        let hide_at = match self.wanted {
            Wanted::Until(at) => Some(at),
            Wanted::Hidden | Wanted::Shown => None,
        };
        [hide_at, self.frame_at, self.retry_at]
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
        if self.wanted == Wanted::Hidden {
            if !matches!(self.shown, Shown::Hidden) {
                surface.hide();
                self.shown = Shown::Hidden;
            }
            self.frame_at = None;
            self.retry_at = None;
            // A hide before the frame attached is a show that never happened.
            self.asked_at = None;
            return Draw::Idle;
        }
        if self.retry_at.is_some_and(|at| now < at) {
            return Draw::Idle;
        }
        if matches!(self.shown, Shown::Showing { stale: false }) {
            return Draw::Idle;
        }
        let clock = self.clock(now);
        if self.style_dirty && painter.restyle(&self.style, self.dark) {
            self.style_dirty = false;
        }
        let next = match (self.view.poll(clock, level), painter.motion(clock)) {
            (Some(view), Some(look)) => Some(view.min(look)),
            (view, look) => view.or(look),
        };
        let (frame, complete) = painter.render(&self.view, clock);
        if let Some(preview) = self.view.status.final_preview()
            && !complete
        {
            preview.failed(PreviewError::TooLong);
            self.frame_at = None;
            return Draw::Idle;
        }
        let drawn = if matches!(self.shown, Shown::Showing { .. }) {
            surface.present(frame)
        } else {
            surface.show(frame)
        };
        // A redraw of a surface already showing is the same show.
        let first_frame = !matches!(self.shown, Shown::Showing { .. });
        let outcome = match drawn {
            Ok(()) => {
                if let Some(preview) = self.view.status.final_preview() {
                    preview.shown();
                }
                self.shown = Shown::Showing { stale: false };
                self.failure = None;
                self.retry_at = None;
                if first_frame {
                    self.note("ok", now);
                }
                Draw::Shown
            }
            Err(HostError::Unsupported(message)) if message.contains("not placed yet") => {
                self.shown = Shown::Placing;
                self.retry_at = Some(now + PLACE_AFTER);
                Draw::Waiting
            }
            Err(error) => {
                if let Some(preview) = self.view.status.final_preview() {
                    preview.failed(PreviewError::Unavailable);
                }
                let message = error.to_string();
                self.shown = Shown::Hidden;
                // A vanished surface is shown again as soon as the next status asks. A refusal waits.
                self.retry_at = (!message.contains("closed")).then_some(now + RETRY_AFTER);
                let fresh = self.failure.as_ref() != Some(&message);
                self.failure = Some(message.clone());
                if fresh {
                    self.note(&message, now);
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
        let mut painter = Painter::new(1.0, Arc::new(defaults_of(Look::Gunmetal)), true).unwrap();
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
        let first = step(Some(Message::Status(Box::new(capturing()))), false, at(0));
        assert_eq!(first, Some(at(0) + view::FRAME), "the wave's next frame");
        // Nothing changed and no frame is due: the surface is left alone.
        step(None, false, at(10));
        // The wave asks for its next frame.
        step(None, true, at(40));
        let done = Box::new(Status::Done {
            target: None,
            route: CaptureRoute::Paste,
            notice: "Delivered.".to_owned(),
            wake_words: true,
        });
        let deadline = step(Some(Message::Status(done)), false, at(100));
        assert_eq!(
            deadline,
            Some(at(1100)),
            "the confirmation's time is the only deadline"
        );
        step(None, true, at(1100));
        step(
            Some(Message::Status(Box::new(capturing()))),
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
        let mut painter = Painter::new(1.0, Arc::new(defaults_of(Look::Gunmetal)), true).unwrap();
        let mut surface = Fake {
            refuse: true,
            ..Fake::default()
        };
        let mut overlay = Overlaying::new(true, start);
        overlay.take(Message::Status(Box::new(capturing())), start);
        overlay.settle(&mut surface, &mut painter, 0.0, start);
        assert!(
            !overlay.cancels(&painter, 386.0, 17.0),
            "a hidden overlay cancels nothing"
        );
        surface.refuse = false;
        overlay.take(Message::Status(Box::new(capturing())), start + RETRY_AFTER);
        overlay.settle(&mut surface, &mut painter, 0.0, start + RETRY_AFTER);
        assert_eq!(surface.calls, ["show", "show"]);
        assert!(
            overlay.cancels(&painter, 386.0, 17.0),
            "a shown overlay's button cancels"
        );
    }

    fn paste_countdown(target: Option<String>, text: &str, pause_ms: u64) -> Status {
        Status::Pending {
            target,
            route: CaptureRoute::Paste,
            text: text.to_owned(),
            waiting_for_quiet: false,
            pause_ms,
            copies: false,
            final_confirmation: None,
        }
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
            (paste_countdown(target.clone(), "", 700), Visibility::Show),
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
                Status::StartingChat {
                    target: target.clone(),
                    again: false,
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
                    wake_words: true,
                },
                Visibility::ShowFor(Duration::from_secs(1)),
            ),
            (
                Status::Failed {
                    kind: FailureKind::Other,
                    offer: None,
                    notice: String::new(),
                    headline: "Failed".to_owned(),
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
        overlay.set_style(Arc::new(defaults_of(Look::Neon)));
        overlay.set_dark(false);
        overlay.show(&capturing());
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
        assert_eq!(*thread.style, defaults_of(Look::Neon));
        assert!(!thread.dark);
        assert!(matches!(thread.view.status, Status::Capturing { .. }));
        assert_eq!(thread.wanted, Wanted::Hidden, "the later hide wins");
        assert!(
            (f32::from_bits(overlay.level.load(Ordering::Relaxed)) - 0.25).abs() < f32::EPSILON
        );
    }

    #[test]
    fn the_countdown_counts_down_on_the_overlay_threads_own_clock() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut painter = Painter::new(1.0, Arc::new(defaults_of(Look::Gunmetal)), true).unwrap();
        let (mut surface, mut overlay) = (Fake::default(), Overlaying::new(false, start));
        let pending = paste_countdown(None, "hello", 2500);
        overlay.take(Message::Status(Box::new(pending)), at(10_000));
        overlay.settle(&mut surface, &mut painter, 0.0, at(10_000));
        assert_eq!(overlay.view.label.as_deref(), Some("PASTE IN 3 s"));
        overlay.due(at(11_000));
        overlay.settle(&mut surface, &mut painter, 0.0, at(11_000));
        assert_eq!(overlay.view.label.as_deref(), Some("PASTE IN 2 s"));
    }

    #[test]
    fn a_vanished_surface_is_shown_again_on_the_next_status() {
        let start = Instant::now();
        let mut painter = Painter::new(1.0, Arc::new(defaults_of(Look::Gunmetal)), true).unwrap();
        let mut surface = Fake::default();
        let mut overlay = Overlaying::new(false, start);
        overlay.take(Message::Status(Box::new(capturing())), start);
        overlay.settle(&mut surface, &mut painter, 0.0, start);
        surface.vanish = true;
        overlay.shown = Shown::Showing { stale: true };
        overlay.settle(&mut surface, &mut painter, 0.0, start + view::FRAME);
        overlay.take(Message::Status(Box::new(capturing())), start + view::FRAME);
        overlay.settle(&mut surface, &mut painter, 0.0, start + view::FRAME);
        assert_eq!(surface.calls, ["show", "present", "show"]);
    }

    #[test]
    fn a_failed_show_or_present_retries_at_most_once_a_second() {
        let start = Instant::now();
        let mut painter = Painter::new(1.0, Arc::new(defaults_of(Look::Gunmetal)), true).unwrap();
        let mut surface = Fake {
            refuse: true,
            ..Fake::default()
        };
        let mut overlay = Overlaying::new(false, start);
        let mut again = |overlay: &mut Overlaying, surface: &mut Fake, now: Instant| {
            overlay.take(Message::Status(Box::new(capturing())), now);
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
        overlay.shown = Shown::Showing { stale: true };
        overlay.settle(&mut surface, &mut painter, 0.0, start + 3 * RETRY_AFTER);
        overlay.take(
            Message::Status(Box::new(capturing())),
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
        let mut painter = Painter::new(1.0, Arc::new(defaults_of(Look::Gunmetal)), true).unwrap();
        let mut surface = Fake {
            pending: true,
            ..Fake::default()
        };
        let mut overlay = Overlaying::new(true, start);
        overlay.take(Message::Status(Box::new(capturing())), start);
        overlay.settle(&mut surface, &mut painter, 0.0, start);
        let hide_at = start + Duration::from_millis(20);
        overlay.take(Message::Visibility(Visibility::Hide), hide_at);
        overlay.settle(&mut surface, &mut painter, 0.0, hide_at);
        assert_eq!(surface.calls, ["show", "hide"]);
        assert!(
            hide_at.saturating_duration_since(start) < Duration::from_millis(100),
            "hide ran without the configure wait"
        );
        assert!(matches!(overlay.shown, Shown::Hidden));
        assert!(
            !overlay.cancels(&painter, 386.0, 17.0),
            "a press on an unplaced overlay cancels nothing"
        );
    }

    #[test]
    fn live_submit_shows_when_overlay_is_off_and_rejects_unshown_reviews() {
        use thalvi_desktop::engine::SubmitPreview;

        let start = Instant::now();
        let mut painter = Painter::new(1.0, Arc::new(defaults_of(Look::Gunmetal)), true).unwrap();
        let live_submit = |text: &str| Status::Pending {
            target: Some("foot".to_owned()),
            route: CaptureRoute::Submit,
            text: text.to_owned(),
            waiting_for_quiet: false,
            pause_ms: 5000,
            copies: false,
            final_confirmation: Some(SubmitPreview::default()),
        };

        // Short text fits: shown even with overlay and draft disabled.
        let mut overlay = Overlaying::new(false, start);
        overlay.take(Message::Draft(false), start);
        let mut surface = Fake::default();
        overlay.take(Message::Status(Box::new(live_submit("Run tests"))), start);
        assert!(matches!(
            overlay.settle(&mut surface, &mut painter, 0.0, start),
            Draw::Shown
        ));
        assert_eq!(surface.calls, ["show"]);

        // Text too long for the module's single-line ticker is rejected without showing.
        let mut overlay = Overlaying::new(false, start);
        let mut surface = Fake::default();
        let long = "The silver lantern burned through the winter storm and kept glowing all night";
        overlay.take(Message::Status(Box::new(live_submit(long))), start);
        assert!(matches!(
            overlay.settle(&mut surface, &mut painter, 0.0, start),
            Draw::Idle
        ));
        assert_eq!(surface.calls, [] as [&str; 0]);

        // A surface refusal fails the preview instead of letting Submit pass unseen.
        let mut overlay = Overlaying::new(false, start);
        let mut surface = Fake {
            refuse: true,
            ..Fake::default()
        };
        overlay.take(Message::Status(Box::new(live_submit("Run tests"))), start);
        assert!(matches!(
            overlay.settle(&mut surface, &mut painter, 0.0, start),
            Draw::Failed(_)
        ));
    }

    #[test]
    fn switching_the_overlay_off_or_on_takes_effect_at_once_without_a_new_status() {
        let start = Instant::now();
        let mut painter = Painter::new(1.0, Arc::new(defaults_of(Look::Gunmetal)), true).unwrap();
        let (mut surface, mut overlay) = (Fake::default(), Overlaying::new(false, start));
        overlay.take(Message::Status(Box::new(capturing())), start);
        overlay.settle(&mut surface, &mut painter, 0.0, start);
        assert_eq!(surface.calls, ["show"], "capturing shows the overlay");
        overlay.take(Message::Enabled(false), start);
        overlay.settle(&mut surface, &mut painter, 0.0, start);
        assert_eq!(
            surface.calls,
            ["show", "hide"],
            "switching off hides at once"
        );
        overlay.take(Message::Enabled(true), start);
        overlay.settle(&mut surface, &mut painter, 0.0, start);
        assert_eq!(
            surface.calls,
            ["show", "hide", "show"],
            "switching on shows again at once"
        );
        assert!(
            matches!(overlay.view.status, Status::Capturing { .. }),
            "no new status arrived"
        );
        // A finished delivery while disabled stays hidden, and switching on does not bring it back.
        overlay.take(Message::Enabled(false), start);
        overlay.settle(&mut surface, &mut painter, 0.0, start);
        let done = Box::new(Status::Done {
            target: None,
            route: CaptureRoute::Paste,
            notice: "Delivered.".to_owned(),
            wake_words: true,
        });
        overlay.take(Message::Status(done), start);
        overlay.settle(&mut surface, &mut painter, 0.0, start);
        overlay.take(Message::Enabled(true), start);
        overlay.settle(&mut surface, &mut painter, 0.0, start);
        assert_eq!(
            surface.calls,
            ["show", "hide", "show", "hide"],
            "a Done while disabled stays hidden, and switching on does not show it"
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

    /// A folder removed when the test ends. The recorder creates it on the first line it writes.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "thalvi-overlay-test-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |since| since.as_nanos())
            ));
            let _ = std::fs::remove_dir_all(&dir);
            Self(dir)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// One show, from the ask through the frame attaching. Returns the lines written under `root`.
    async fn shown_lines(
        overlay: &mut Overlaying,
        root: &std::path::Path,
    ) -> Vec<serde_json::Value> {
        let start = Instant::now();
        let mut painter = Painter::new(1.0, Arc::new(defaults_of(Look::Gunmetal)), true).unwrap();
        let mut surface = Fake {
            pending: true,
            ..Fake::default()
        };
        overlay.take(Message::Status(Box::new(capturing())), start);
        overlay.settle(&mut surface, &mut painter, 0.0, start);
        surface.pending = false;
        let placed = start + PLACE_AFTER;
        let drawn = overlay.settle(&mut surface, &mut painter, 0.0, placed);
        assert!(matches!(drawn, Draw::Shown), "the frame attached");
        // A redraw of the same show records nothing more.
        overlay.shown = Shown::Showing { stale: true };
        overlay.settle(
            &mut surface,
            &mut painter,
            0.0,
            placed + Duration::from_millis(40),
        );
        // finish joins the writer, so the lines below are everything this show queued.
        let recorder = std::mem::replace(&mut overlay.recorder, Recorder::disabled());
        recorder.finish().await;
        metric_lines(root)
    }

    fn metric_lines(root: &std::path::Path) -> Vec<serde_json::Value> {
        let path = root.join("metrics.jsonl");
        let Ok(text) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        text.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn a_show_records_how_long_it_took_to_place_the_first_frame() {
        let root = Scratch::new("shown");
        let recorder = Recorder::in_dir(
            RecordKinds {
                metrics: true,
                ..RecordKinds::default()
            },
            root.path(),
        )
        .unwrap();
        let mut overlay = Overlaying::with(false, Instant::now(), recorder);
        let lines = shown_lines(&mut overlay, root.path()).await;
        assert_eq!(lines.len(), 1, "one event per show");
        assert_eq!(lines[0]["event"], "desktop_overlay_shown");
        assert_eq!(lines[0]["outcome"], "ok");
        assert_eq!(lines[0]["duration_ms"], PLACE_AFTER.as_secs_f64() * 1000.0);
    }

    #[tokio::test]
    async fn a_recordings_change_decides_whether_the_next_show_is_written() {
        let root = Scratch::new("off");
        let recorder = Recorder::in_dir(
            RecordKinds {
                metrics: true,
                ..RecordKinds::default()
            },
            root.path(),
        )
        .unwrap();
        let mut overlay = Overlaying::with(false, Instant::now(), recorder);
        overlay.take(Message::Recordings(RecordKinds::default()), Instant::now());
        let none = shown_lines(&mut overlay, root.path()).await;
        assert!(
            none.is_empty(),
            "turning metrics off writes no line for the next show"
        );

        let root = Scratch::new("on");
        let recorder = Recorder::in_dir(RecordKinds::default(), root.path()).unwrap();
        let mut overlay = Overlaying::with(false, Instant::now(), recorder);
        overlay.take(
            Message::Recordings(RecordKinds {
                metrics: true,
                ..RecordKinds::default()
            }),
            Instant::now(),
        );
        let lines = shown_lines(&mut overlay, root.path()).await;
        assert_eq!(lines.len(), 1, "turning metrics on writes the next show");
        assert_eq!(lines[0]["event"], "desktop_overlay_shown");
    }

    #[test]
    fn each_preview_state_draws_its_own_frame_at_both_scales() {
        let style = Arc::new(defaults_of(Look::Holo));
        let states = [
            Sample::Live,
            Sample::Quiet,
            Sample::Working,
            Sample::Pending,
            Sample::Done,
            Sample::Failed,
        ];
        let mut frames = Vec::new();
        for state in states {
            let (width, height, rgba) =
                render_rgba(&View::sample(state), Arc::clone(&style), true, 1.0)
                    .expect("a preview frame");
            assert_eq!(
                (width, height, rgba.len()),
                (460, 200, 460 * 200 * 4),
                "{state:?}"
            );
            frames.push(rgba);
        }
        for (i, first) in frames.iter().enumerate() {
            for (j, second) in frames.iter().enumerate() {
                assert!(
                    i == j || first != second,
                    "states {i} and {j} draw the same frame"
                );
            }
        }
        let (width, height, rgba) = render_rgba(&View::sample(Sample::Live), style, true, 2.0)
            .expect("a scaled preview frame");
        assert_eq!((width, height, rgba.len()), (920, 400, 920 * 400 * 4));
    }

    #[test]
    fn one_renderer_reuses_its_painter_without_ever_showing_a_stale_look() {
        use thalvi_desktop::theme::{LookOverrides, Themes};

        let view = View::sample(Sample::Live);
        let themes = Themes::load(&serde_json::json!({
            "version": 1,
            "holo": {"colors": {"dark": {"accent": "#ff0000"}}},
        }));
        let changed = themes.of(Look::Holo);
        assert!(matches!(changed, Some(LookOverrides::Usable(_))));
        let accent = Arc::new(Style::resolve(Look::Holo, changed));
        let neon = Arc::new(defaults_of(Look::Neon));
        let cases = [
            (Arc::new(defaults_of(Look::Holo)), 1.0),
            (accent, 1.0),
            (Arc::clone(&neon), 1.0),
            (neon, 2.0),
        ];
        let mut renderer = Renderer::default();
        let mut previous = None;
        for (style, scale) in cases {
            let fresh = render_rgba(&view, Arc::clone(&style), true, scale).expect("a fresh frame");
            let reused = renderer
                .draw(&view, Arc::clone(&style), true, scale)
                .expect("a reused frame");
            assert_eq!(reused, fresh, "a stale look at scale {scale}");
            if (scale - 2.0).abs() < f32::EPSILON {
                assert_eq!((reused.0, reused.1), (920, 400));
            }
            if let Some(previous) = previous {
                assert_ne!(fresh, previous, "the cases must look different");
            }
            previous = Some(fresh);
        }
    }
}
