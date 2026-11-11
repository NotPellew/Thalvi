//! The gunmetal overlay: a module hanging from the top edge with a mic wave over one line of text,
//! drawn on the CPU.

use std::f32::consts::{PI, TAU};
use std::time::Duration;

use thalvi_desktop::capture::CaptureRoute;
use thalvi_desktop::host::OverlayFrame;

use super::paint::{Canvas, Fill, Layout, Rgba, Style, TRANSPARENT, Text, TextBounds};
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

const CYAN: Rgba = Rgba::rgb(107, 248, 255);
const QUIET: Rgba = Rgba::rgb(143, 179, 196);
const INK: Rgba = Rgba::rgb(234, 255, 243);
const BLACK: Rgba = Rgba::rgb(0, 0, 0);
const WHITE: Rgba = Rgba::rgb(255, 255, 255);

/// The slot's top-left corner, in points from the overlay's.
const SLOT: (f32, f32) = (MODULE_X + SLOT_X, 2.0);
/// The cancel button, in points from the overlay's top-left corner.
const CANCEL: (f32, f32, f32, f32) = (SLOT.0 + SLOT_W - 24.0, SLOT.1 + 6.0, 18.0, 18.0);
/// Room around a glow for its blur.
const GLOW_PAD: f32 = 24.0;
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
    fn color(self) -> Rgba {
        match self {
            Self::Live | Self::Working => CYAN,
            Self::Quiet => QUIET,
            Self::Pending => Rgba::rgb(255, 194, 75),
            Self::Done => Rgba::rgb(43, 255, 136),
            Self::Failed => Rgba::rgb(255, 107, 94),
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
}
/// What the overlay shows, fed from the engine's status, its level and the settings.
pub struct View {
    status: Status,
    tone: Tone,
    label: Option<String>,
    line: Line,
    show_draft: bool,
    takes_clicks: bool,
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

    pub fn cancellable(&self) -> bool {
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

    pub fn animating(&self) -> bool {
        self.wave.is_some() || self.countdown.is_some() || self.sweep || self.caret()
    }

    fn caret(&self) -> bool {
        matches!(self.line, Line::Words { caret: true, .. })
    }
}

impl View {
    /// Steps the wave, the countdown and the sweep to `clock` with the mic at `level`; how long
    /// until the next frame, or `None` while nothing moves.
    pub fn poll(&mut self, clock: f64, level: f32) -> Option<Duration> {
        if !self.animating() {
            return None;
        }
        self.clock = clock;
        let level = if self.wave.is_some() {
            level.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.level = level;
        let next = self
            .wave
            .as_mut()
            .map_or(FRAME, |wave| wave.step(clock, level));
        if let Some(running) = self.countdown.clone() {
            self.label = Some(countdown_label(&running, self.left_ms(&running)));
        }
        Some(next)
    }

    /// Whether a press at `(x, y)` points from the overlay's corner hits the cancel button.
    pub fn cancels(&self, x: f32, y: f32) -> bool {
        let (left, top, width, height) = CANCEL;
        self.cancellable() && (left..left + width).contains(&x) && (top..top + height).contains(&y)
    }
}

/// Draws views into frames: the still parts once per scale, the rest each frame.
pub struct Painter {
    text: Text,
    background: Canvas,
    canvas: Canvas,
    glows: Vec<(Tone, Canvas)>,
    frame: OverlayFrame,
}

impl Painter {
    /// Reads the system's fonts and draws the still parts at `scale` pixels per point.
    pub fn new(scale: f32) -> Option<Self> {
        Some(Self {
            text: Text::new(),
            background: background(scale)?,
            canvas: Canvas::new(WIDTH, HEIGHT, scale)?,
            glows: Vec::new(),
            frame: OverlayFrame::default(),
        })
    }

    pub fn draw(&mut self, view: &View) -> &OverlayFrame {
        let canvas = &mut self.canvas;
        canvas.copy(&self.background);
        canvas.clip(Some((SLOT.0, SLOT.1, SLOT_W, SLOT_H)));
        let color = view.tone.color();
        let has_text = view.line != Line::None;
        let wave_top = SLOT.1 + if has_text { 2.0 } else { 17.0 };
        #[expect(clippy::cast_possible_truncation, reason = "a clock in seconds")]
        let seconds = view.clock as f32;
        if let Some(wave) = &view.wave {
            let glow = glow(&mut self.glows, view.tone, canvas.scale_of());
            if let Some(glow) = glow {
                canvas.draw(
                    glow,
                    (
                        SLOT.0 + 5.0 + (WAVE_W - 160.0) / 2.0 - GLOW_PAD,
                        wave_top + 7.0 - GLOW_PAD,
                    ),
                    (wave.smoothed * 1.3).min(1.0),
                );
            }
            let strands = if view.tone == Tone::Quiet {
                QUIET
            } else {
                CYAN
            };
            draw_wave(canvas, wave, strands, seconds, (SLOT.0 + 5.0, wave_top));
        }
        if let Some(label) = &view.label {
            tag(canvas, &mut self.text, label, color, wave_top);
        }
        if let Status::Done {
            target,
            route,
            notice,
        } = &view.status
        {
            let message = done_message(*route, target.as_deref(), notice);
            let style = Style {
                size: 13.0,
                line_height: 17.0,
                weight: 600,
                mono: false,
                tracking: 0.0,
            };
            let layout = Layout::Centred {
                width: SLOT_W - 16.0,
            };
            let spans = [(message.as_str(), color)];
            let (_, height) = self.text.measure(&spans, style, layout, canvas.scale_of());
            self.text.draw(
                canvas,
                &spans,
                style,
                layout,
                (SLOT.0 + 8.0, SLOT.1 + (SLOT_H - height) / 2.0),
            );
        } else if has_text {
            let stops = [
                (0.0, TRANSPARENT),
                (0.5, QUIET.alpha(0.25)),
                (1.0, TRANSPARENT),
            ];
            let (left, right) = (SLOT.0 + 24.0, SLOT.0 + SLOT_W - 24.0);
            canvas.rect(
                (left, SLOT.1 + 48.0, right - left, 1.0),
                0.0,
                &Fill::Linear {
                    from: (left, 0.0),
                    to: (right, 0.0),
                    stops: &stops,
                },
            );
            ticker(canvas, &mut self.text, &view.line, view.clock % 1.0 < 0.5);
        }
        if view.cancellable() {
            cancel_button(canvas, &mut self.text);
        }
        canvas.clip(None);
        line_strip(canvas, view, color);
        canvas.frame_into(&mut self.frame);
        &self.frame
    }
}

/// The module's shadow, the metal frame with its hex mesh, the bezel and the dark glass slot.
fn background(scale: f32) -> Option<Canvas> {
    let mut canvas = Canvas::new(WIDTH, HEIGHT, scale)?;
    let mut shadow = Canvas::new(WIDTH, HEIGHT, scale)?;
    shadow.polygon(
        &trapezoid([MODULE_X, 6.0, MODULE_W, MODULE_H], SHOULDER),
        &Fill::Solid(BLACK.alpha(0.6)),
    );
    shadow.blur(6.0);
    canvas.draw(&shadow, (0.0, 0.0), 1.0);
    let metal = [
        (0.0, Rgba::rgb(0x3a, 0x3a, 0x3a)),
        (0.8, Rgba::rgb(0x5a, 0x5a, 0x5a)),
        (1.0, Rgba::rgb(0x9a, 0x9a, 0x9a)),
    ];
    shade(
        &mut canvas,
        [MODULE_X, 0.0, MODULE_W, MODULE_H],
        SHOULDER,
        &metal,
    );
    let panel = [MODULE_X + 1.0, 0.0, MODULE_W - 2.0, MODULE_H - 1.0];
    let face = [
        (0.0, Rgba::rgb(0x1c, 0x1c, 0x1c)),
        (0.6, Rgba::rgb(0x2c, 0x2c, 0x2c)),
        (1.0, Rgba::rgb(0x3c, 0x3c, 0x3c)),
    ];
    shade(&mut canvas, panel, SHOULDER, &face);
    hexes(&mut canvas, panel, SHOULDER);
    let bezel = [
        (0.0, Rgba::rgb(0x55, 0x55, 0x55)),
        (1.0, Rgba::rgb(0x88, 0x88, 0x88)),
    ];
    shade(
        &mut canvas,
        [SLOT.0 - 1.0, 2.0, SLOT_W + 2.0, MODULE_H - 5.0],
        SLANT,
        &bezel,
    );
    let glass = [
        (0.0, Rgba::rgb(0x03, 0x08, 0x0d)),
        (1.0, Rgba::rgb(0x0a, 0x18, 0x20)),
    ];
    shade(&mut canvas, [SLOT.0, SLOT.1, SLOT_W, SLOT_H], SLANT, &glass);
    let inset = [(0.0, BLACK.alpha(0.85)), (1.0, TRANSPARENT)];
    canvas.clip(Some((SLOT.0, SLOT.1, SLOT_W, SLOT_H)));
    canvas.rect(
        (SLOT.0, SLOT.1, SLOT_W, 14.0),
        0.0,
        &Fill::Linear {
            from: (0.0, SLOT.1),
            to: (0.0, SLOT.1 + 14.0),
            stops: &inset,
        },
    );
    canvas.clip(None);
    Some(canvas)
}

/// The corners of a trapezoid `[x, y, width, height]` whose bottom edge is `inset` in from each side.
fn trapezoid([x, y, width, height]: [f32; 4], inset: f32) -> [(f32, f32); 4] {
    [
        (x, y),
        (x + width, y),
        (x + width - inset, y + height),
        (x + inset, y + height),
    ]
}

/// Fills a trapezoid with a vertical gradient through `stops`.
fn shade(canvas: &mut Canvas, rect: [f32; 4], inset: f32, stops: &[(f32, Rgba)]) {
    canvas.polygon(
        &trapezoid(rect, inset),
        &Fill::Linear {
            from: (0.0, rect[1]),
            to: (0.0, rect[1] + rect[3]),
            stops,
        },
    );
}

/// The hex mesh on the metal, kept where it lies inside the trapezoid.
fn hexes(canvas: &mut Canvas, [x, y, width, height]: [f32; 4], inset: f32) {
    let inside = |p: (f32, f32)| {
        let down = (p.1 - y) / height;
        (0.0..=1.0).contains(&down) && p.0 >= x + inset * down && p.0 <= x + width - inset * down
    };
    let line = Fill::Solid(WHITE.alpha(0.06));
    let (cell_w, cell_h) = (16.0, 27.7);
    let mut top = y;
    while top < y + height {
        let mut left = x;
        while left < x + width {
            let hex = [
                (left + 8.0, top),
                (left + 16.0, top + 4.62),
                (left + 16.0, top + 13.86),
                (left + 8.0, top + 18.47),
                (left, top + 13.86),
                (left, top + 4.62),
            ];
            for i in 0..6 {
                let (a, b) = (hex[i], hex[(i + 1) % 6]);
                if inside(a) && inside(b) {
                    canvas.stroke(&[a, b], 1.0, &line);
                }
            }
            let (a, b) = ((left + 8.0, top + 18.47), (left + 8.0, top + cell_h));
            if inside(a) && inside(b) {
                canvas.stroke(&[a, b], 1.0, &line);
            }
            left += cell_w;
        }
        top += cell_h;
    }
}

/// The soft glow behind the wave in `tone`'s colour, drawn once per tone.
fn glow(glows: &mut Vec<(Tone, Canvas)>, tone: Tone, scale: f32) -> Option<&Canvas> {
    if !glows.iter().any(|(drawn, _)| *drawn == tone) {
        let mut canvas = Canvas::new(160.0 + 2.0 * GLOW_PAD, 30.0 + 2.0 * GLOW_PAD, scale)?;
        canvas.rect(
            (GLOW_PAD, GLOW_PAD, 160.0, 30.0),
            15.0,
            &Fill::Solid(tone.color().alpha(0.2)),
        );
        canvas.blur(10.0);
        glows.push((tone, canvas));
    }
    glows
        .iter()
        .find(|(drawn, _)| *drawn == tone)
        .map(|(_, canvas)| canvas)
}

/// Nine twisting strands, faded at both ends, with their top-left at `origin`.
fn draw_wave(canvas: &mut Canvas, wave: &Wave, color: Rgba, seconds: f32, origin: (f32, f32)) {
    const STEP: f32 = 4.0;
    let strand_a = |x: f32, p: f32| {
        (TAU * 3.1 * x + p).sin() * 0.62 + (TAU * 5.2 * x - p * 1.35 + 1.0).sin() * 0.38
    };
    let strand_b = |x: f32, p: f32| {
        (TAU * 2.5 * x - p * 0.8 + 2.0).sin() * 0.55 + (TAU * 4.3 * x + p * 1.1).sin() * 0.45
    };
    let middle = origin.1 + WAVE_H / 2.0;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "332 / 4"
    )]
    let count = (WAVE_W / STEP) as usize;
    for (width, alpha) in [(3.2, 0.07), (1.0, 0.55)] {
        for strand in 0..9u8 {
            let side = (f32::from(strand) / 8.0) * 2.0 - 1.0;
            let (twist_sin, twist_cos) = (side * 1.25).sin_cos();
            let strength = alpha * (0.45 + 0.55 * (1.0 - side.abs()));
            #[expect(clippy::cast_precision_loss, reason = "a few hundred points")]
            let points: Vec<(f32, f32)> = (0..=count)
                .map(|i| {
                    let px = i as f32 * STEP;
                    let x = px / WAVE_W;
                    let offset =
                        strand_a(x, wave.phase) * twist_cos + strand_b(x, wave.phase) * twist_sin;
                    (origin.0 + px, middle + wave.amplitude(x, seconds) * offset)
                })
                .collect();
            let stops = [
                (0.0, color.alpha(0.0)),
                (0.18, color.alpha(0.6 * strength)),
                (0.5, color.alpha(strength)),
                (0.82, color.alpha(0.6 * strength)),
                (1.0, color.alpha(0.0)),
            ];
            canvas.stroke(
                &points,
                width,
                &Fill::Linear {
                    from: (origin.0, 0.0),
                    to: (origin.0 + WAVE_W, 0.0),
                    stops: &stops,
                },
            );
        }
    }
}

/// The label between two short lines that fade towards it, centred over the wave.
fn tag(canvas: &mut Canvas, text: &mut Text, label: &str, color: Rgba, top: f32) {
    let style = Style {
        size: 11.0,
        line_height: 14.0,
        weight: 700,
        mono: true,
        tracking: 1.54,
    };
    let spans = [(label, color)];
    let (width, _) = text.measure(&spans, style, Layout::Line, canvas.scale_of());
    let total = 34.0 + 12.0 + width + 12.0 + 34.0;
    let left = SLOT.0 + 5.0 + (WAVE_W - total) / 2.0;
    let middle = top + WAVE_H / 2.0;
    for (x, from, to) in [
        (left, TRANSPARENT, color),
        (left + 34.0 + 12.0 + width + 12.0, color, TRANSPARENT),
    ] {
        let stops = [(0.0, from), (1.0, to)];
        canvas.rect(
            (x, middle - 0.75, 34.0, 1.5),
            1.0,
            &Fill::Linear {
                from: (x, 0.0),
                to: (x + 34.0, 0.0),
                stops: &stops,
            },
        );
    }
    text.draw(
        canvas,
        &spans,
        style,
        Layout::Line,
        (left + 46.0, middle - style.line_height / 2.0),
    );
}

/// One line that never wraps. Words that overflow it keep their newest end in view and fade out
/// at the start; a notice that overflows fades out at its end.
fn ticker(canvas: &mut Canvas, text: &mut Text, line: &Line, blink_on: bool) {
    let style = Style {
        size: 14.0,
        line_height: 22.0,
        weight: 400,
        mono: false,
        tracking: 0.0,
    };
    let (left, top, width) = (SLOT.0 + 24.0, SLOT.1 + 52.0, SLOT_W - 48.0);
    let scale = canvas.scale_of();
    match line {
        Line::None => {}
        Line::Notice(notice) => {
            let spans = [(notice.as_str(), INK)];
            let (used, _) = text.measure(&spans, style, Layout::Line, scale);
            canvas.set_text_bounds(Some(TextBounds {
                left,
                right: left + width,
                fade_left: 0.0,
                fade_right: if used > width { 36.0 } else { 0.0 },
            }));
            text.draw(canvas, &spans, style, Layout::Line, (left, top));
            canvas.set_text_bounds(None);
        }
        Line::Words {
            committed,
            partial,
            caret,
        } => {
            let gap = if committed.is_empty() || partial.is_empty() {
                ""
            } else {
                " "
            };
            let committed = format!("{committed}{gap}");
            let spans = [(committed.as_str(), INK), (partial.as_str(), QUIET)];
            let (used, _) = text.measure(&spans, style, Layout::Line, scale);
            let long = used + 3.0 > width;
            let start = if long {
                left + width - used - 3.0
            } else {
                left
            };
            canvas.set_text_bounds(Some(TextBounds {
                left,
                right: left + width,
                fade_left: if long { 36.0 } else { 0.0 },
                fade_right: 0.0,
            }));
            text.draw(canvas, &spans, style, Layout::Line, (start, top));
            canvas.set_text_bounds(None);
            if *caret && blink_on {
                canvas.rect(
                    (start + used + 1.0, top + 4.0, 2.0, 14.0),
                    0.0,
                    &Fill::Solid(CYAN),
                );
            }
        }
    }
}

fn cancel_button(canvas: &mut Canvas, text: &mut Text) {
    let (x, y, width, height) = CANCEL;
    let stops = [
        (0.0, Rgba::rgb(0x5c, 0x5c, 0x60)),
        (1.0, Rgba::rgb(0x3a, 0x3a, 0x3d)),
    ];
    canvas.rect(
        CANCEL,
        2.0,
        &Fill::Linear {
            from: (0.0, y),
            to: (0.0, y + height),
            stops: &stops,
        },
    );
    canvas.outline(CANCEL, 2.0, Rgba::rgb(0x6e, 0x6e, 0x72));
    let style = Style {
        size: 14.0,
        line_height: height,
        weight: 400,
        mono: false,
        tracking: 0.0,
    };
    let spans = [("×", Rgba::rgb(0xe1, 0xe4, 0xe7))];
    let (used, _) = text.measure(&spans, style, Layout::Line, canvas.scale_of());
    text.draw(
        canvas,
        &spans,
        style,
        Layout::Line,
        (x + (width - used) / 2.0, y),
    );
}

/// The glowing line along the module's bottom edge: full while listening, shrinking towards its
/// middle through a countdown, a sweep while working.
fn line_strip(canvas: &mut Canvas, view: &View, color: Rgba) {
    let full = MODULE_W - 2.0 * LINE_INSET;
    let left = MODULE_X + LINE_INSET;
    let y = MODULE_H - 2.0;
    if view.sweep {
        #[expect(clippy::cast_possible_truncation, reason = "a phase in 0..2")]
        let phase = ((view.clock / 1.3) % 2.0) as f32;
        let travel = if phase <= 1.0 { phase } else { 2.0 - phase };
        let eased = 0.5 - 0.5 * (PI * travel).cos();
        let width = full * 0.3;
        canvas.rect((left, y, full, 2.0), 0.0, &Fill::Solid(color.alpha(0.2)));
        canvas.clip(Some((left, y - 1.0, full, 4.0)));
        let x = left + eased * width * 2.33;
        let stops = [(0.0, TRANSPARENT), (0.5, color), (1.0, TRANSPARENT)];
        canvas.rect(
            (x, y, width, 2.0),
            0.0,
            &Fill::Linear {
                from: (x, 0.0),
                to: (x + width, 0.0),
                stops: &stops,
            },
        );
        canvas.clip(None);
        return;
    }
    #[expect(clippy::cast_precision_loss, reason = "milliseconds of a pause")]
    let share = view.countdown.as_ref().map_or(1.0, |running| {
        view.left_ms(running) as f32 / running.total_ms.max(1) as f32
    });
    let width = full * share;
    let x = left + (full - width) / 2.0;
    // A soft glow under the line, then the line itself.
    for (spread, alpha) in [(5.0, 0.12), (3.0, 0.2), (0.0, 1.0)] {
        let stops = [
            (0.0, TRANSPARENT),
            (0.2, color.alpha(alpha)),
            (0.8, color.alpha(alpha)),
            (1.0, TRANSPARENT),
        ];
        canvas.rect(
            (x, y - spread, width, 2.0 + 2.0 * spread),
            1.0 + spread,
            &Fill::Linear {
                from: (x, 0.0),
                to: (x + width, 0.0),
                stops: &stops,
            },
        );
    }
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

    /// The frame's colour at `(x, y)` points, at scale 1, as RGB.
    fn rgb(frame: &OverlayFrame, x: f32, y: f32) -> [u8; 3] {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a test point"
        )]
        let at = (y as usize * frame.width as usize + x as usize) * 4;
        [frame.bgra[at + 2], frame.bgra[at + 1], frame.bgra[at]]
    }

    #[test]
    fn each_state_lights_the_bottom_line_in_its_tone() {
        let capturing = Status::Capturing {
            target: None,
            committed: "hello".to_owned(),
            partial: None,
            notice: None,
        };
        let pending = Status::Pending {
            target: None,
            route: CaptureRoute::Paste,
            text: "hello".to_owned(),
            waiting_for_quiet: false,
            pause_ms: 2500,
            copies: false,
        };
        let done = Status::Done {
            target: None,
            route: CaptureRoute::Paste,
            notice: "Delivered.".to_owned(),
        };
        let failed = Status::Failed {
            kind: FailureKind::Other,
            notice: String::new(),
            headline: "No words heard",
            causes: Vec::new(),
        };
        let mut painter = Painter::new(1.0).unwrap();
        for (status, tone) in [
            (capturing, Tone::Live),
            (pending, Tone::Pending),
            (done, Tone::Done),
            (failed, Tone::Failed),
        ] {
            let mut view = View::new(false);
            view.set_status(status.clone(), 0.0);
            let [r, g, b] = rgb(painter.draw(&view), WIDTH / 2.0, MODULE_H - 1.0);
            let want = tone.color();
            let near = |got: u8, want: u8| got.abs_diff(want) < 40;
            assert!(
                near(r, want.r) && near(g, want.g) && near(b, want.b),
                "{status:?} drew {:?}",
                [r, g, b]
            );
        }
    }

    #[test]
    fn only_a_capture_on_a_desktop_that_takes_clicks_is_cancelled_by_its_button() {
        let capturing = Status::Capturing {
            target: None,
            committed: String::new(),
            partial: None,
            notice: None,
        };
        let done = Status::Done {
            target: None,
            route: CaptureRoute::Paste,
            notice: String::new(),
        };
        let button = (CANCEL.0 + CANCEL.2 / 2.0, CANCEL.1 + CANCEL.3 / 2.0);
        for (takes_clicks, status, at, cancels) in [
            (true, capturing.clone(), button, true),
            (true, capturing.clone(), (WIDTH / 2.0, 40.0), false),
            (false, capturing, button, false),
            (true, done, button, false),
        ] {
            let mut view = View::new(takes_clicks);
            view.set_status(status.clone(), 0.0);
            assert_eq!(
                view.cancels(at.0, at.1),
                cancels,
                "{takes_clicks} {status:?} {at:?}"
            );
        }
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
