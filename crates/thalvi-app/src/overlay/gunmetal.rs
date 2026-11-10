//! The gunmetal overlay drawn with Rune: a module hanging from the top edge with a mic wave over one line of text.

use std::f32::consts::{PI, TAU};
use std::sync::Arc;
use std::time::Duration;

use rune::tok::TextStyle;
use rune::{Color, Cx, El, LineCap, Shape, Tx, canvas, el, row, text};
use thalvi_desktop::capture::CaptureRoute;

use crate::status::{FailureKind, Status};

/// The overlay surface, in points; the module hangs centred from its top.
pub const WIDTH: f32 = 460.0;
pub const HEIGHT: f32 = 200.0;

const MODULE_W: f32 = 370.0;
const MODULE_H: f32 = 84.0;
const MODULE_X: f32 = (WIDTH - MODULE_W) / 2.0;
const SHOULDER: f32 = 16.0;
/// The bezel and slot are inset by this much on each side, and slant in by `SLANT`.
const SLOT_X: f32 = 14.0;
const SLANT: f32 = 13.0;
const SLOT_W: f32 = MODULE_W - 2.0 * SLOT_X;
const SLOT_H: f32 = MODULE_H - 6.0;
const WAVE_W: f32 = 332.0;
const WAVE_H: f32 = 44.0;
const LINE_INSET: f32 = SHOULDER + 4.0;
/// Longest target the countdown label spells out.
const MAX_TARGET: usize = 10;

const HISTORY: usize = 40;
const SAMPLE_S: f64 = 0.08;
const FRAME: Duration = Duration::from_millis(33);
const SETTLED_FRAME: Duration = Duration::from_millis(100);
const SETTLED_BELOW: f32 = 0.015;

fn srgb(r: u8, g: u8, b: u8) -> Color {
    let decode = |c: u8| rune::color::srgb_decode(f32::from(c) / 255.0);
    Color {
        r: decode(r),
        g: decode(g),
        b: decode(b),
        a: 1.0,
    }
}

fn cyan() -> Color {
    srgb(107, 248, 255)
}

fn quiet() -> Color {
    srgb(143, 179, 196)
}

fn ink() -> Color {
    srgb(234, 255, 243)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tone {
    Live,
    Quiet,
    Pending,
    Working,
    Done,
    Failed,
}

impl Tone {
    fn color(self) -> Color {
        match self {
            Self::Live | Self::Working => cyan(),
            Self::Quiet => quiet(),
            Self::Pending => srgb(255, 194, 75),
            Self::Done => srgb(43, 255, 136),
            Self::Failed => srgb(255, 107, 94),
        }
    }
}

/// What the one line of text shows.
#[derive(Clone, Debug, Default, PartialEq)]
enum Line {
    #[default]
    None,
    Words {
        committed: String,
        partial: String,
        caret: bool,
    },
    Notice(String),
}

/// A running countdown to an ending's action.
#[derive(Clone, Debug, PartialEq)]
struct Countdown {
    route: CaptureRoute,
    target: Option<String>,
    copies: bool,
    total_ms: u64,
    /// When it started, on the overlay's clock.
    started: f64,
}

/// The mic wave's recent loudness and motion, stepped by the overlay's poll.
#[derive(Clone, Debug)]
struct Wave {
    history: [f32; HISTORY],
    smoothed: f32,
    phase: f32,
    last_frame: Option<f64>,
    last_sample: Option<f64>,
}

impl Default for Wave {
    fn default() -> Self {
        Self {
            history: [0.0; HISTORY],
            smoothed: 0.0,
            phase: 0.0,
            last_frame: None,
            last_sample: None,
        }
    }
}

impl Wave {
    fn settled(&self, level: f32) -> bool {
        self.smoothed < SETTLED_BELOW
            && level < SETTLED_BELOW
            && self.history.iter().all(|sample| *sample < SETTLED_BELOW)
    }

    /// Advances to `now` (seconds); the time to wait for the next frame.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "small positive counts"
    )]
    fn step(&mut self, now: f64, level: f32) -> Duration {
        let elapsed = self
            .last_frame
            .map_or(0.0, |last| (now - last).clamp(0.0, 0.05)) as f32;
        self.last_frame = Some(now);
        let due = self
            .last_sample
            .map_or(HISTORY, |last| ((now - last) / SAMPLE_S).floor() as usize);
        if due > 0 {
            let fresh = due.min(HISTORY);
            self.history.rotate_left(fresh);
            for sample in &mut self.history[HISTORY - fresh..] {
                *sample = level;
            }
            self.last_sample = Some(match self.last_sample {
                Some(last) if due <= HISTORY => last + due as f64 * SAMPLE_S,
                _ => now,
            });
        }
        let pace = |rate: f32| 1.0 - (1.0 - rate).powf(elapsed * 60.0);
        let rate = if level > self.smoothed { 0.35 } else { 0.08 };
        self.smoothed += (level - self.smoothed) * pace(rate);
        self.phase += elapsed * (1.1 + 3.2 * self.smoothed);
        if self.settled(level) {
            SETTLED_FRAME
        } else {
            FRAME
        }
    }

    fn amplitude(&self, x: f32, seconds: f32) -> f32 {
        let distance = (x - 0.5).abs() * 2.0;
        #[expect(clippy::cast_precision_loss, reason = "HISTORY is 40")]
        let at = (HISTORY - 1) as f32 - distance * 14.0;
        let index = at.floor();
        let share = at - index;
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "index is within 25..39"
        )]
        let index = index as usize;
        let sample = |i: usize| self.history.get(i).copied().unwrap_or(0.0);
        let recent = sample(index) * (1.0 - share) + sample(index + 1) * share;
        let breathing = 0.035 + 0.012 * (seconds * 2.2).sin();
        let envelope = (-((x - 0.5) / 0.27).powi(2)).exp();
        WAVE_H
            * 0.47
            * envelope
            * (breathing + 0.95 * (self.smoothed * (1.0 - distance * 0.6)).max(recent))
    }

    /// Nine twisting strands, faded at both ends; each strand is cut into pieces whose alpha
    /// follows the fade, since a Rune stroke has one colour.
    fn shape(&self, color: Color, seconds: f32) -> Shape {
        const PIECES: usize = 12;
        const STEP: f32 = 4.0;
        let strand_a = |x: f32, p: f32| {
            (TAU * 3.1 * x + p).sin() * 0.62 + (TAU * 5.2 * x - p * 1.35 + 1.0).sin() * 0.38
        };
        let strand_b = |x: f32, p: f32| {
            (TAU * 2.5 * x - p * 0.8 + 2.0).sin() * 0.55 + (TAU * 4.3 * x + p * 1.1).sin() * 0.45
        };
        let fade = |x: f32| {
            let stops = [(0.0, 0.0), (0.18, 0.6), (0.5, 1.0), (0.82, 0.6), (1.0, 0.0)];
            stops
                .windows(2)
                .find(|pair| x <= pair[1].0)
                .map_or(0.0, |pair| {
                    let ((x0, a0), (x1, a1)) = (pair[0], pair[1]);
                    a0 + (a1 - a0) * (x - x0) / (x1 - x0)
                })
        };
        let middle = WAVE_H / 2.0;
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "332 / 4"
        )]
        let count = (WAVE_W / STEP) as usize;
        let mut shape = Shape::new();
        for (width, alpha) in [(3.2, 0.07), (1.0, 0.55)] {
            for strand in 0..9 {
                #[expect(clippy::cast_precision_loss, reason = "strand < 9")]
                let side = (strand as f32 / 8.0) * 2.0 - 1.0;
                let (twist_sin, twist_cos) = (side * 1.25).sin_cos();
                let strength = alpha * (0.45 + 0.55 * (1.0 - side.abs()));
                #[expect(clippy::cast_precision_loss, reason = "a few hundred points")]
                let points: Vec<[f32; 2]> = (0..=count)
                    .map(|i| {
                        let px = i as f32 * STEP;
                        let x = px / WAVE_W;
                        let offset = strand_a(x, self.phase) * twist_cos
                            + strand_b(x, self.phase) * twist_sin;
                        [px, middle + self.amplitude(x, seconds) * offset]
                    })
                    .collect();
                let per = count.div_ceil(PIECES);
                for from in (0..count).step_by(per) {
                    let to = (from + per).min(count);
                    #[expect(clippy::cast_precision_loss, reason = "a few hundred points")]
                    let centre = (from + to) as f32 / 2.0 / count as f32;
                    shape = shape.stroke_with(
                        points[from..=to].to_vec(),
                        false,
                        width,
                        LineCap::Butt,
                        color.alpha(strength * fade(centre)),
                    );
                }
            }
        }
        shape
    }
}

/// What the overlay shows, fed from the engine's status, its level and the settings.
pub struct View {
    status: Status,
    tone: Tone,
    label: Option<String>,
    line: Line,
    show_draft: bool,
    takes_clicks: bool,
    on_cancel: Option<Arc<dyn Fn() + Send + Sync>>,
    level: f32,
    wave: Option<Wave>,
    countdown: Option<Countdown>,
    sweep: bool,
    clock: f64,
}

impl View {
    pub fn new(takes_clicks: bool) -> Self {
        Self {
            status: Status::Paused,
            tone: Tone::Live,
            label: None,
            line: Line::None,
            show_draft: true,
            takes_clicks,
            on_cancel: None,
            level: 0.0,
            wave: None,
            countdown: None,
            sweep: false,
            clock: 0.0,
        }
    }

    pub fn set_draft(&mut self, show: bool) {
        self.show_draft = show;
    }

    pub fn set_level(&mut self, level: f32) {
        self.level = level.clamp(0.0, 1.0);
    }

    pub fn on_cancel(&mut self, cancel: Arc<dyn Fn() + Send + Sync>) {
        self.on_cancel = Some(cancel);
    }

    /// The tone, label and line `next` shows; whether the wave runs, and the countdown if one does.
    fn take(&mut self, next: &Status, was_counting: bool, now: f64) -> (bool, Option<Countdown>) {
        let mut wave = false;
        let mut countdown = None;
        match next {
            Status::Paused | Status::Listening => {
                self.line = Line::None;
                self.tone = Tone::Live;
            }
            Status::Starting { .. } => {
                self.line = Line::None;
                self.tone = Tone::Live;
                wave = true;
            }
            Status::Capturing {
                committed,
                partial,
                notice,
                ..
            } => {
                self.line = match notice {
                    Some(notice) => Line::Notice(notice.clone()),
                    None if self.show_draft => Line::Words {
                        committed: committed.clone(),
                        partial: partial.clone().unwrap_or_default(),
                        caret: true,
                    },
                    None => Line::None,
                };
                self.tone = Tone::Live;
                wave = true;
            }
            Status::Pending {
                target,
                route,
                text: words,
                waiting_for_quiet,
                pause_ms,
                copies,
            } => {
                self.line = self.words(words, false);
                if *waiting_for_quiet {
                    self.tone = Tone::Quiet;
                    wave = true;
                } else {
                    self.tone = Tone::Pending;
                    let started = match &self.countdown {
                        Some(running) if was_counting => running.started,
                        _ => now,
                    };
                    countdown = Some(Countdown {
                        route: *route,
                        target: target.clone(),
                        copies: *copies,
                        total_ms: *pause_ms,
                        started,
                    });
                }
            }
            Status::Writing { text: words, .. } => {
                self.line = self.words(words, true);
                self.tone = Tone::Working;
                self.label = Some("WRITING…".to_owned());
                self.sweep = true;
            }
            Status::Thinking { .. } => {
                self.line = Line::None;
                self.tone = Tone::Working;
                self.label = Some("THINKING…".to_owned());
                self.sweep = true;
            }
            Status::Working { .. } => {
                self.tone = Tone::Working;
                self.label = Some("WORKING".to_owned());
                self.sweep = true;
            }
            Status::Loading { notice, .. } => {
                self.line = Line::Notice(notice.clone());
                self.tone = Tone::Working;
                self.label = Some("LOADING".to_owned());
                self.sweep = true;
            }
            Status::Done { .. } => {
                self.tone = Tone::Done;
            }
            Status::Failed { kind, headline, .. } => {
                self.line = Line::Notice((*headline).to_owned());
                self.tone = Tone::Failed;
                self.label = Some(
                    match kind {
                        FailureKind::Delivery => "NOT SENT",
                        FailureKind::Other => "FAILED",
                    }
                    .to_owned(),
                );
            }
        }
        (wave, countdown)
    }

    /// Takes in a new status at `now` on the overlay's clock.
    pub fn set_status(&mut self, next: Status, now: f64) {
        let was_counting = matches!(
            self.status,
            Status::Pending {
                waiting_for_quiet: false,
                ..
            }
        );
        self.clock = now;
        self.label = None;
        self.sweep = false;
        let (wave, countdown) = self.take(&next, was_counting, now);
        self.wave = match (wave, self.wave.take()) {
            (true, Some(running)) => Some(running),
            (true, None) => Some(Wave::default()),
            (false, _) => None,
        };
        if !wave {
            self.level = 0.0;
        }
        self.countdown = countdown;
        self.status = next;
        if let Some(running) = &self.countdown {
            self.label = Some(countdown_label(running, self.left_ms(running)));
        }
    }

    fn words(&self, words: &str, caret: bool) -> Line {
        if self.show_draft {
            Line::Words {
                committed: words.to_owned(),
                partial: String::new(),
                caret,
            }
        } else {
            Line::None
        }
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "milliseconds of a pause"
    )]
    fn left_ms(&self, countdown: &Countdown) -> u64 {
        let elapsed = ((self.clock - countdown.started) * 1000.0).max(0.0) as u64;
        countdown
            .total_ms
            .saturating_sub(elapsed)
            .min(countdown.total_ms)
    }

    fn cancellable(&self) -> bool {
        self.takes_clicks
            && matches!(
                self.status,
                Status::Starting { .. }
                    | Status::Capturing { .. }
                    | Status::Pending { .. }
                    | Status::Writing { .. }
                    | Status::Thinking { .. }
                    | Status::Working {
                        cancellable: true,
                        ..
                    }
            )
    }

    fn animating(&self) -> bool {
        self.wave.is_some() || self.countdown.is_some() || self.sweep || self.caret()
    }

    fn caret(&self) -> bool {
        matches!(self.line, Line::Words { caret: true, .. })
    }
}

/// Steps the wave, the countdown and the sweep while any runs; `true` when the view must redraw.
pub fn poll(view: &mut View, clock: f64, tx: &mut Tx) -> bool {
    if !view.animating() {
        return false;
    }
    view.clock = clock;
    let level = view.level;
    let next = view
        .wave
        .as_mut()
        .map_or(FRAME, |wave| wave.step(clock, level));
    if let Some(running) = view.countdown.clone() {
        view.label = Some(countdown_label(&running, view.left_ms(&running)));
    }
    tx.settle();
    tx.wake_after(next);
    true
}

fn tone_text(size: f32, weight: f32) -> TextStyle {
    TextStyle::new(size, weight)
}

/// A trapezoid `w` wide and `h` tall at `(x, y)` whose bottom edge is `inset` in from each side,
/// filled with vertical gradient `stops` as opaque horizontal bands, since a Rune fill has one
/// colour; each band reaches a point into the next so no antialiased seam shows between them.
fn trapezoid(mut shape: Shape, rect: [f32; 4], inset: f32, stops: &[(f32, Color)]) -> Shape {
    const BANDS: u16 = 24;
    let [x, y, width, height] = rect;
    let colour = |at: f32| {
        stops
            .windows(2)
            .find(|pair| at <= pair[1].0)
            .map_or(stops[stops.len() - 1].1, |pair| {
                pair[0]
                    .1
                    .mix(pair[1].1, (at - pair[0].0) / (pair[1].0 - pair[0].0))
            })
    };
    for band in 0..BANDS {
        let (top, bottom) = (
            f32::from(band) / f32::from(BANDS),
            f32::from(band + 1) / f32::from(BANDS),
        );
        let edge = |share: f32| inset * share;
        let reach = if band + 1 == BANDS {
            bottom
        } else {
            (bottom + 1.0 / height).min(1.0)
        };
        shape = shape.fill(
            vec![
                [x + edge(top), y + height * top],
                [x + width - edge(top), y + height * top],
                [x + width - edge(reach), y + height * reach],
                [x + edge(reach), y + height * reach],
            ],
            colour(f32::midpoint(top, bottom)),
        );
    }
    shape
}

/// The hex mesh on the metal, kept where it lies inside the trapezoid.
fn hexes(mut shape: Shape, rect: [f32; 4], inset: f32) -> Shape {
    let [x, y, width, height] = rect;
    let inside = |p: [f32; 2]| {
        let down = (p[1] - y) / height;
        (0.0..=1.0).contains(&down) && p[0] >= x + inset * down && p[0] <= x + width - inset * down
    };
    let line = Color::WHITE.alpha(0.06);
    let (cell_w, cell_h) = (16.0, 27.7);
    let mut top = y;
    while top < y + height {
        let mut left = x;
        while left < x + width {
            let hex = [
                [left + 8.0, top],
                [left + 16.0, top + 4.62],
                [left + 16.0, top + 13.86],
                [left + 8.0, top + 18.47],
                [left, top + 13.86],
                [left, top + 4.62],
            ];
            for i in 0..6 {
                let (a, b) = (hex[i], hex[(i + 1) % 6]);
                if inside(a) && inside(b) {
                    shape = shape.line(a, b, 1.0, LineCap::Butt, line);
                }
            }
            let (a, b) = ([left + 8.0, top + 18.47], [left + 8.0, top + cell_h]);
            if inside(a) && inside(b) {
                shape = shape.line(a, b, 1.0, LineCap::Butt, line);
            }
            left += cell_w;
        }
        top += cell_h;
    }
    shape
}

/// The metal frame, the bezel and the dark glass slot.
fn frame() -> Shape {
    let shape = trapezoid(
        Shape::new(),
        [0.0, 0.0, MODULE_W, MODULE_H],
        SHOULDER,
        &[
            (0.0, srgb(0x3a, 0x3a, 0x3a)),
            (0.8, srgb(0x5a, 0x5a, 0x5a)),
            (1.0, srgb(0x9a, 0x9a, 0x9a)),
        ],
    );
    let panel = [1.0, 0.0, MODULE_W - 2.0, MODULE_H - 1.0];
    let shape = trapezoid(
        shape,
        panel,
        SHOULDER,
        &[
            (0.0, srgb(0x1c, 0x1c, 0x1c)),
            (0.6, srgb(0x2c, 0x2c, 0x2c)),
            (1.0, srgb(0x3c, 0x3c, 0x3c)),
        ],
    );
    let shape = hexes(shape, panel, SHOULDER);
    let shape = trapezoid(
        shape,
        [SLOT_X - 1.0, 2.0, SLOT_W + 2.0, MODULE_H - 5.0],
        SLANT,
        &[(0.0, srgb(0x55, 0x55, 0x55)), (1.0, srgb(0x88, 0x88, 0x88))],
    );
    trapezoid(
        shape,
        [SLOT_X, 2.0, SLOT_W, SLOT_H],
        SLANT,
        &[(0.0, srgb(0x03, 0x08, 0x0d)), (1.0, srgb(0x0a, 0x18, 0x20))],
    )
}

fn module_outline() -> Shape {
    Shape::new().fill(
        vec![
            [0.0, 0.0],
            [MODULE_W, 0.0],
            [MODULE_W - SHOULDER, MODULE_H],
            [SHOULDER, MODULE_H],
        ],
        Color::BLACK.alpha(0.6),
    )
}

fn stub(color: Color, towards_label: bool) -> El {
    let (from, to) = if towards_label {
        (Color::TRANSPARENT, color)
    } else {
        (color, Color::TRANSPARENT)
    };
    el().size(34.0, 1.5)
        .radius(1.0)
        .linear_gradient(PI / 2.0, [(0.0, from), (1.0, to)])
}

fn tag(label: &str, color: Color, top: f32) -> El {
    row()
        .key("tag")
        .size(WAVE_W, WAVE_H)
        .pin(Some(5.0), Some(top), None, None)
        .center()
        .gap(12.0)
        .child(stub(color, true))
        .child(
            text(label.to_owned())
                .key("label")
                .font(tone_text(11.0, 700.0).mono().tracking(1.54))
                .color(color),
        )
        .child(stub(color, false))
}

/// One line that never wraps; once it is longer than the line, the newest words stay in view.
fn ticker(line: &Line, blink_on: bool) -> El {
    let style = tone_text(14.0, 400.0).lh(22.0);
    let body = row()
        .key("tick")
        .size(SLOT_W - 48.0, 22.0)
        .pin(Some(24.0), Some(52.0), None, None)
        .clip_rounded(0.0);
    match line {
        Line::None => body,
        Line::Notice(notice) => body.child(
            text(notice.clone())
                .key("notice")
                .font(style)
                .color(ink())
                .truncate_fade(),
        ),
        Line::Words {
            committed,
            partial,
            caret,
        } => {
            // Rune gives a view no text width; about 40 characters of 14 px text fill the line.
            let long = committed.chars().count() + partial.chars().count() > 40;
            let gap = if committed.is_empty() || partial.is_empty() {
                ""
            } else {
                " "
            };
            let words = row()
                .key("words")
                .h(22.0)
                .center_y()
                .child(
                    text(format!("{committed}{gap}"))
                        .key("committed")
                        .font(style)
                        .color(ink()),
                )
                .child(
                    text(partial.clone())
                        .key("partial")
                        .font(style)
                        .color(quiet()),
                )
                .child(
                    el().key("caret")
                        .size(2.0, 14.0)
                        .ml(1.0)
                        .fill(cyan())
                        .opacity(if *caret && blink_on { 1.0 } else { 0.0 }),
                );
            let body = body
                .justify_end()
                .child(words)
                .child(el().key("rest").grow(1.0));
            if long {
                body.clip_fade(36.0, 0.0, 0.0, 0.0)
            } else {
                body
            }
        }
    }
}

fn line_strip(view: &View) -> El {
    let color = view.tone.color();
    let full = MODULE_W - 2.0 * LINE_INSET;
    let base = el()
        .key("line")
        .h(2.0)
        .pin(None, Some(MODULE_H - 2.0), None, None);
    if view.sweep {
        #[expect(clippy::cast_possible_truncation, reason = "a phase in 0..1")]
        let phase = ((view.clock / 1.3) % 2.0) as f32;
        let travel = if phase <= 1.0 { phase } else { 2.0 - phase };
        let eased = 0.5 - 0.5 * (PI * travel).cos();
        let width = full * 0.3;
        return base
            .w(full)
            .pin(
                Some(MODULE_X + LINE_INSET),
                Some(MODULE_H - 2.0),
                None,
                None,
            )
            .fill(color.alpha(0.2))
            .clip_rounded(0.0)
            .child(
                el().size(width, 2.0)
                    .pin(Some(eased * width * 2.33), Some(0.0), None, None)
                    .linear_gradient(
                        PI / 2.0,
                        [
                            (0.0, Color::TRANSPARENT),
                            (0.5, color),
                            (1.0, Color::TRANSPARENT),
                        ],
                    ),
            );
    }
    #[expect(clippy::cast_precision_loss, reason = "milliseconds of a pause")]
    let share = view.countdown.as_ref().map_or(1.0, |running| {
        view.left_ms(running) as f32 / running.total_ms.max(1) as f32
    });
    let width = full * share;
    base.w(width)
        .pin(
            Some(MODULE_X + LINE_INSET + (full - width) / 2.0),
            Some(MODULE_H - 2.0),
            None,
            None,
        )
        .linear_gradient(
            PI / 2.0,
            [
                (0.0, Color::TRANSPARENT),
                (0.2, color),
                (0.8, color),
                (1.0, Color::TRANSPARENT),
            ],
        )
        .shadow(0.0, 6.0, color.alpha(0.9))
}

fn cancel_button(view: &View) -> El {
    let cancel = view.on_cancel.clone();
    row()
        .key("cancel")
        .role("button")
        .label("Cancel capture")
        .size(18.0, 18.0)
        .pin(None, Some(6.0), Some(6.0), None)
        .center()
        .radius(2.0)
        .gradient(srgb(0x5c, 0x5c, 0x60), srgb(0x3a, 0x3a, 0x3d))
        .border(1.0, srgb(0x6e, 0x6e, 0x72))
        .on_press(move |_: &mut View| {
            if let Some(cancel) = &cancel {
                cancel();
            }
        })
        .child(
            text("×")
                .font(tone_text(14.0, 400.0))
                .color(srgb(0xe1, 0xe4, 0xe7)),
        )
}

/// The dark glass slot: the wave, its label, and the line of text or the delivery message.
fn slot(view: &View) -> El {
    let color = view.tone.color();
    let has_text = view.line != Line::None;
    let wave_top = if has_text { 2.0 } else { 17.0 };
    #[expect(clippy::cast_possible_truncation, reason = "a clock in seconds")]
    let seconds = view.clock as f32;
    let blink_on = view.clock % 1.0 < 0.5;
    let mut slot = el()
        .key("slot")
        .size(SLOT_W, SLOT_H)
        .pin(Some(MODULE_X + SLOT_X), Some(2.0), None, None)
        .clip_rounded(0.0)
        .child(
            el().key("inset")
                .size(SLOT_W, 14.0)
                .pin(Some(0.0), Some(0.0), None, None)
                .gradient(Color::BLACK.alpha(0.85), Color::TRANSPARENT),
        );
    if let Some(wave) = &view.wave {
        let wave_color = if view.tone == Tone::Quiet {
            quiet()
        } else {
            cyan()
        };
        slot = slot
            .child(
                el().key("glow")
                    .size(160.0, 30.0)
                    .pin(
                        Some(5.0 + (WAVE_W - 160.0) / 2.0),
                        Some(wave_top + 7.0),
                        None,
                        None,
                    )
                    .radius(15.0)
                    .fill(color.alpha(0.2))
                    .blur(10.0)
                    .opacity((wave.smoothed * 1.3).min(1.0)),
            )
            .child(
                canvas(wave.shape(wave_color, seconds))
                    .key("wave")
                    .size(WAVE_W, WAVE_H)
                    .pin(Some(5.0), Some(wave_top), None, None),
            );
    }
    if let Some(label) = &view.label {
        slot = slot.child(tag(label, color, wave_top));
    }
    if let Status::Done {
        target,
        route,
        notice,
    } = &view.status
    {
        slot = slot.child(
            row()
                .key("message")
                .size(SLOT_W, SLOT_H)
                .pin(Some(0.0), Some(0.0), None, None)
                .center()
                .padding(8.0)
                .child(
                    text(done_message(*route, target.as_deref(), notice))
                        .key("done")
                        .font(tone_text(13.0, 600.0).lh(17.0))
                        .color(color)
                        .align(rune::Align::Center)
                        .wrap(),
                ),
        );
    } else if has_text {
        slot = slot
            .child(
                el().key("sep")
                    .size(SLOT_W - 48.0, 1.0)
                    .pin(Some(24.0), Some(48.0), None, None)
                    .linear_gradient(
                        PI / 2.0,
                        [
                            (0.0, Color::TRANSPARENT),
                            (0.5, quiet().alpha(0.25)),
                            (1.0, Color::TRANSPARENT),
                        ],
                    ),
            )
            .child(ticker(&view.line, blink_on));
    }
    if view.cancellable() {
        slot = slot.child(cancel_button(view));
    }
    slot
}

/// The overlay's view; everything it leaves clear stays transparent.
pub fn view(view: &View, _: &mut Cx<View>) -> El {
    el().key("overlay")
        .w_full()
        .h_full()
        .child(
            canvas(module_outline())
                .key("shadow")
                .size(MODULE_W, MODULE_H)
                .pin(Some(MODULE_X), Some(6.0), None, None)
                .blur(6.0),
        )
        .child(canvas(frame()).key("frame").size(MODULE_W, MODULE_H).pin(
            Some(MODULE_X),
            Some(0.0),
            None,
            None,
        ))
        .child(slot(view))
        .child(line_strip(view))
}

fn copies_text(route: CaptureRoute, copies: bool) -> bool {
    copies && route != CaptureRoute::Task
}

/// The action, its target, and whole seconds rounded up.
fn countdown_label(countdown: &Countdown, left_ms: u64) -> String {
    let seconds = left_ms.div_ceil(1000).max(1);
    let to = countdown.target.as_deref().map(|target| {
        let named = target.to_uppercase();
        if named.chars().count() > MAX_TARGET {
            format!(
                "{}…",
                named.chars().take(MAX_TARGET - 1).collect::<String>()
            )
        } else {
            named
        }
    });
    let action = if copies_text(countdown.route, countdown.copies) {
        "COPY".to_owned()
    } else {
        match (countdown.route, to) {
            (CaptureRoute::Paste, None) => "PASTE".to_owned(),
            (CaptureRoute::Paste, Some(to)) => format!("PASTE INTO {to}"),
            (CaptureRoute::Write, None) => "WRITE".to_owned(),
            (CaptureRoute::Write, Some(to)) => format!("WRITE FOR {to}"),
            (CaptureRoute::Submit, None) => "SEND".to_owned(),
            (CaptureRoute::Submit, Some(to)) => format!("SEND TO {to}"),
            (CaptureRoute::Task, _) => "TASK".to_owned(),
        }
    };
    format!("{action} IN {seconds} s")
}

/// What the slot says once a delivery finished.
fn done_message(route: CaptureRoute, target: Option<&str>, notice: &str) -> String {
    match target {
        Some(target) if route != CaptureRoute::Task => {
            let verb = if matches!(route, CaptureRoute::Paste | CaptureRoute::Write) {
                "Pasted into"
            } else {
                "Sent to"
            };
            format!("✓ {verb} {target}. {notice}")
        }
        _ => format!("✓ {notice}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counting(route: CaptureRoute, target: Option<&str>, copies: bool) -> Countdown {
        Countdown {
            route,
            target: target.map(str::to_owned),
            copies,
            total_ms: 2500,
            started: 0.0,
        }
    }

    #[test]
    fn the_countdown_names_the_action_and_its_target_in_whole_seconds_rounded_up() {
        let cases = [
            (
                CaptureRoute::Paste,
                Some("kitty"),
                false,
                2500,
                "PASTE INTO KITTY IN 3 s",
            ),
            (
                CaptureRoute::Submit,
                Some("a-very-long-window"),
                false,
                1000,
                "SEND TO A-VERY-LO… IN 1 s",
            ),
            (CaptureRoute::Write, None, false, 1, "WRITE IN 1 s"),
            (CaptureRoute::Paste, Some("kitty"), true, 0, "COPY IN 1 s"),
            (CaptureRoute::Task, Some("kitty"), true, 2000, "TASK IN 2 s"),
        ];
        for (route, target, copies, left, expected) in cases {
            assert_eq!(
                countdown_label(&counting(route, target, copies), left),
                expected
            );
        }
    }

    #[test]
    fn a_countdown_keeps_its_start_while_the_pending_status_updates() {
        let pending = |quiet: bool| Status::Pending {
            target: None,
            route: CaptureRoute::Paste,
            text: "hello".to_owned(),
            waiting_for_quiet: quiet,
            pause_ms: 2500,
            copies: false,
        };
        let mut view = View::new(false);
        view.set_status(pending(false), 1.0);
        view.set_status(pending(false), 2.0);
        assert_eq!(view.label.as_deref(), Some("PASTE IN 2 s"));
        view.set_status(pending(true), 3.0);
        view.set_status(pending(false), 4.0);
        assert_eq!(view.label.as_deref(), Some("PASTE IN 3 s"));
    }

    #[test]
    fn a_delivery_names_where_the_words_went() {
        assert_eq!(
            done_message(CaptureRoute::Write, Some("kitty"), "Done."),
            "✓ Pasted into kitty. Done."
        );
        assert_eq!(
            done_message(CaptureRoute::Submit, Some("kitty"), "Done."),
            "✓ Sent to kitty. Done."
        );
        assert_eq!(
            done_message(CaptureRoute::Task, Some("kitty"), "Opened."),
            "✓ Opened."
        );
    }
}
