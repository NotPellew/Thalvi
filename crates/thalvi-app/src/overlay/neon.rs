//! The neon look: a glass pill with a dot, nine level bars, a label and the target, and under it a
//! card with the last lines of the draft; in the system's light or dark colours.

use std::f32::consts::PI;

use thalvi_desktop::capture::CaptureRoute;

use thalvi_desktop::theme::overlay::{
    CONFIRMATION, DRAFT, LABEL, NEON_CANCEL, Neon as Theme, TARGET, neon as art,
};

use super::paint::{Canvas, Fill, Layout, Rgba, Style, Text, TextBounds};
use super::view::{
    HEIGHT, SUBMIT_DRAFT_SURFACE, SUBMIT_DRAFT_TEXT, Tone, View, WIDTH, copies_text, done_message,
    spaced,
};
use crate::status::Status;

/// The neon look's state between frames: its theme, the bars' eased level, and where it last
/// drew its cancel button.
pub(super) struct Neon {
    theme: Theme,
    shown: f32,
    last: Option<f64>,
    pub(super) cancel: Option<(f32, f32, f32, f32)>,
    built_in_cancel: Option<CancelAnchor>,
}

impl Neon {
    pub(super) fn new(theme: Theme) -> Self {
        Self {
            theme,
            shown: 0.0,
            last: None,
            cancel: None,
            built_in_cancel: None,
        }
    }

    /// Whether `(x, y)` points hits the cancel button as last drawn.
    pub(super) fn cancels(&self, x: f32, y: f32) -> bool {
        self.cancel.is_some_and(|(left, top, width, height)| {
            (left..left + width).contains(&x) && (top..top + height).contains(&y)
        })
    }

    pub(super) fn draw(&mut self, canvas: &mut Canvas, text: &mut Text, view: &View) -> bool {
        canvas.clear();
        let elapsed = self.last.map_or(0.0, |last| {
            (view.clock - last).clamp(0.0, art::BAR_EASING.2)
        });
        self.last = Some(view.clock);
        let moving = matches!(view.tone, Tone::Live | Tone::Quiet) && view.wave.is_some();
        if moving {
            let rate = if view.level > self.shown {
                art::BAR_EASING.0
            } else {
                art::BAR_EASING.1
            };
            #[expect(clippy::cast_possible_truncation, reason = "a fraction of a second")]
            let pace = 1.0 - (1.0f32 - rate).powf(elapsed as f32 * art::BAR_RATE);
            self.shown += (view.level - self.shown) * pace;
        } else {
            self.shown = 0.0;
        }
        let pill = self.pill(canvas, text, view, moving);
        draft(view).is_some_and(|words| {
            card(
                canvas,
                text,
                view,
                &self.theme,
                &words,
                pill.1 + pill.3 + art::GAP * self.theme.measures.spacing,
            )
        })
    }

    /// Draws the pill; its rectangle.
    fn pill(
        &mut self,
        canvas: &mut Canvas,
        text: &mut Text,
        view: &View,
        moving: bool,
    ) -> (f32, f32, f32, f32) {
        let theme = &self.theme;
        let style = theme.measures;
        let scale = canvas.scale_of();
        let (tone, words, target) = pill_words(view);
        let ends = matches!(tone, Tone::Done | Tone::Failed);
        let (label_style, label_color) = pill_label(theme, tone);
        let target_style = style.text(TARGET);
        let cancellable = view.cancellable();
        let custom_geometry = label_style != LABEL && !ends
            || target_style != TARGET
            || style.spacing.to_bits() != 1.0f32.to_bits()
            || text.has_custom_families();
        let [left_inset, right_inset] =
            [art::PILL_INSETS.0, art::PILL_INSETS.1].map(|inset| inset * style.spacing);
        let gap = art::PART_GAP * style.spacing;
        let fixed_marks = art::DOT + art::STRIP;
        let mut fixed = left_inset + right_inset + 2.0 * art::BASE_EDGE;
        let mut parts = 0.0;
        if !ends {
            fixed += fixed_marks;
            parts += 2.0;
        }
        if cancellable {
            fixed += art::CANCEL_SIZE;
            parts += 1.0;
        }
        let target_words = target.map(|to| format!("→ {to}"));
        let label = (words.as_str(), label_color);
        let target = target_words.as_deref().map(|words| (words, theme.muted));
        if target.is_some() {
            parts += 1.0;
        }
        let gaps = gap * parts;
        let original_cancel = (cancellable && custom_geometry).then(|| {
            CancelAnchor::resolve(&mut self.built_in_cancel, text, label, target, parts, scale)
        });
        let right = original_cancel.map_or(WIDTH - art::SIDE, |x| {
            x + art::CANCEL_SIZE + right_inset + art::BASE_EDGE
        });
        let room = right - art::SIDE;
        let available = (room - fixed - gaps).max(art::MIN_LABEL_ROOM);
        let label_layout = if ends {
            Layout::Wrapped { width: available }
        } else {
            Layout::Line
        };
        let label = PillText::measure(text, label, label_style, label_layout, scale);
        let mut target =
            target.map(|span| PillText::measure(text, span, target_style, Layout::Line, scale));
        if custom_geometry && let Some(target) = &mut target {
            target.width = target.width.min((available - label.width).max(0.0));
        }
        let target_width = target.as_ref().map_or(0.0, |target| target.width);
        let label = PillText {
            width: label
                .width
                .min((available - target_width).max(art::MIN_LABEL_ROOM)),
            ..label
        };
        let inner = if ends { label.height } else { art::BAR_H };
        let vertical = if cancellable {
            art::PILL_VERTICAL
        } else {
            art::PILL_VERTICAL * style.spacing
        };
        let height = inner + 2.0 * vertical + 2.0 * art::BASE_EDGE;
        let width = (fixed + gaps + label.width + target_width).min(room);
        let left = original_cancel.map_or((WIDTH - width) / 2.0, |_| right - width);
        let top = art::TOP;
        let rect = (left, top, width, height);
        pill_surface(canvas, theme, tone, rect);
        let middle = top + height / 2.0;
        let mut x = left + art::BASE_EDGE + left_inset;
        if !ends {
            self.marks(canvas, view, tone, moving, (x, middle));
            x += art::DOT + gap + art::STRIP + gap;
        }
        label.draw(canvas, text, (x, middle), custom_geometry && !ends);
        x += label.width + gap;
        if let Some(target) = target {
            PillText {
                height: target.style.line_height,
                ..target
            }
            .draw(canvas, text, (x, middle), custom_geometry);
            x += target.width + gap;
        }
        canvas.set_text_bounds(None);
        let x = original_cancel.unwrap_or(x);
        self.cancel = cancellable.then_some((
            x,
            middle - art::CANCEL_SIZE / 2.0,
            art::CANCEL_SIZE,
            art::CANCEL_SIZE,
        ));
        if cancellable {
            cancel_mark(canvas, text, theme, (x, middle));
        }
        rect
    }

    /// The dot and the nine level bars, starting at `x` around `middle`.
    fn marks(
        &self,
        canvas: &mut Canvas,
        view: &View,
        tone: Tone,
        moving: bool,
        (x, middle): (f32, f32),
    ) {
        let theme = &self.theme;
        let dot = match tone {
            Tone::Quiet => theme.state.quiet,
            Tone::Pending => theme.state.pending,
            Tone::Working if theme.measures.pulse => {
                #[expect(clippy::cast_possible_truncation, reason = "a phase in seconds")]
                let phase = (view.clock % art::DOT_PULSE.0) as f32;
                theme
                    .blue
                    .faded(1.0 - art::DOT_PULSE.1 * (0.5 - 0.5 * (2.0 * PI * phase).cos()))
            }
            Tone::Working => theme.blue,
            _ => theme.accent,
        };
        if !matches!(tone, Tone::Quiet | Tone::Working) {
            let (left, radius, opacity) = art::DOT_HALO;
            canvas.rect(
                (x - left, middle - radius, 2.0 * radius, 2.0 * radius),
                radius,
                &Fill::Solid(
                    theme
                        .dot_halo
                        .unwrap_or(dot)
                        .faded(opacity * theme.measures.glow_ratio),
                ),
            );
        }
        canvas.rect(
            (x, middle - art::DOT / 2.0, art::DOT, art::DOT),
            art::DOT / 2.0,
            &Fill::Solid(dot),
        );
        let bars = x + art::DOT + art::PART_GAP * theme.measures.spacing;
        let bar_color = match tone {
            Tone::Live => theme.accent,
            Tone::Pending => theme.state.pending,
            Tone::Working => theme.state.working,
            Tone::Quiet => theme.state.quiet,
            _ => theme.muted,
        };
        #[expect(
            clippy::cast_possible_truncation,
            reason = "monotonic Duration milliseconds fit f32; only a rounded bar phase is needed"
        )]
        let now_ms = (view.clock * art::MILLIS_PER_SECOND) as f32;
        for (index, envelope) in (0u8..).zip(art::ENVELOPE) {
            let share = if moving {
                let wobble = if theme.measures.pulse {
                    let (base, amplitude, period, phase) = art::BAR_WOBBLE;
                    base + amplitude * (now_ms / period + f32::from(index) * phase).sin()
                } else {
                    1.0
                };
                art::FLAT
                    + (1.0 - art::FLAT) * (self.shown * envelope * wobble * art::BAR_GAIN).min(1.0)
            } else {
                art::FLAT
            };
            let bar = art::BAR_H * share;
            canvas.rect(
                (
                    bars + f32::from(index) * 2.0 * art::BAR_W,
                    middle - bar / 2.0,
                    art::BAR_W,
                    bar,
                ),
                art::BAR_RADIUS,
                &Fill::Solid(bar_color),
            );
        }
    }
}

fn pill_label(theme: &Theme, tone: Tone) -> (Style, Rgba) {
    let label = theme.measures.label(LABEL);
    let message = theme.measures.text(CONFIRMATION);
    match tone {
        Tone::Done => (message, theme.state.done),
        Tone::Failed => (message, theme.state.failed),
        Tone::Pending => (label, theme.state.pending),
        _ => (label, theme.muted),
    }
}

fn pill_surface(canvas: &mut Canvas, theme: &Theme, tone: Tone, rect: (f32, f32, f32, f32)) {
    let border = match tone {
        Tone::Live => theme.state.live,
        Tone::Pending => theme.state.pending,
        Tone::Done => theme.state.done,
        Tone::Failed => theme.state.failed,
        _ => theme.line,
    };
    let radius = (rect.3 / 2.0 + theme.measures.radius_delta).max(0.0);
    shadow(canvas, rect, radius, theme);
    canvas.rect(rect, radius, &Fill::Solid(theme.glass));
    canvas.outline(rect, radius, border, theme.measures.border);
}

struct PillText<'a> {
    span: (&'a str, Rgba),
    style: Style,
    layout: Layout,
    width: f32,
    height: f32,
}

impl<'a> PillText<'a> {
    fn measure(
        text: &mut Text,
        span: (&'a str, Rgba),
        style: Style,
        layout: Layout,
        scale: f32,
    ) -> Self {
        let (width, height) = text.measure(&[span], style, layout, scale);
        Self {
            span,
            style,
            layout,
            width,
            height,
        }
    }

    fn draw(&self, canvas: &mut Canvas, text: &mut Text, (x, middle): (f32, f32), clip: bool) {
        if clip {
            text_width(canvas, x, self.width);
        }
        text.draw(
            canvas,
            &[self.span],
            self.style,
            self.layout,
            (x, middle - self.height / 2.0),
        );
    }
}

struct CancelAnchor {
    label: String,
    target: Option<String>,
    x: f32,
}

impl CancelAnchor {
    fn resolve(
        held: &mut Option<Self>,
        text: &mut Text,
        label: (&str, Rgba),
        target: Option<(&str, Rgba)>,
        parts: f32,
        scale: f32,
    ) -> f32 {
        let name = target.map(|(name, _)| name);
        if let Some(held) = held
            .as_ref()
            .filter(|held| held.label == label.0 && held.target.as_deref() == name)
        {
            return held.x;
        }
        let room = WIDTH - 2.0 * art::SIDE;
        let target_width = target.map_or(0.0, |span| text.built_in_width(&[span], TARGET, scale));
        let label_width = text.built_in_width(&[label], LABEL, scale);
        let fixed = art::PILL_INSETS.0
            + art::PILL_INSETS.1
            + 2.0 * art::BASE_EDGE
            + (art::DOT + art::STRIP)
            + art::CANCEL_SIZE;
        let gaps = art::PART_GAP * parts;
        let label_width =
            label_width.min((room - fixed - gaps - target_width).max(art::MIN_LABEL_ROOM));
        let width = (fixed + gaps + label_width + target_width).min(room);
        let mut x = (WIDTH - width) / 2.0 + art::BASE_EDGE + art::PILL_INSETS.0;
        x += art::DOT + art::PART_GAP + art::STRIP + art::PART_GAP;
        x += label_width + art::PART_GAP;
        if name.is_some() {
            x += target_width + art::PART_GAP;
        }
        *held = Some(Self {
            label: label.0.to_owned(),
            target: name.map(str::to_owned),
            x,
        });
        x
    }
}

fn text_width(canvas: &mut Canvas, left: f32, width: f32) {
    canvas.set_text_bounds(Some(TextBounds {
        left,
        right: left + width,
        fade_left: 0.0,
        fade_right: 0.0,
    }));
}

/// Draws the glyph centred inside the unchanged cancel area.
fn cancel_mark(canvas: &mut Canvas, text: &mut Text, theme: &Theme, (x, middle): (f32, f32)) {
    let style = theme.measures.text(NEON_CANCEL);
    let spans = [("×", theme.muted)];
    let (used, _) = text.measure(&spans, style, Layout::Line, canvas.scale_of());
    text.draw(
        canvas,
        &spans,
        style,
        Layout::Line,
        (
            x + (art::CANCEL_SIZE - used) / 2.0,
            middle - style.line_height / 2.0,
        ),
    );
}

/// The pill's tone, its words and the target it names.
fn pill_words(view: &View) -> (Tone, String, Option<&str>) {
    let words = |text: &str| text.to_owned();
    match &view.status {
        Status::Starting {
            target,
            loading: true,
        } => (Tone::Working, words("LOADING"), target.as_deref()),
        Status::Starting { target, .. } | Status::Capturing { target, .. } => {
            (Tone::Live, words("LISTENING"), target.as_deref())
        }
        Status::Pending {
            target,
            waiting_for_quiet: true,
            ..
        } => (Tone::Quiet, words("QUIET"), target.as_deref()),
        Status::Pending {
            target,
            route,
            copies,
            pause_ms,
            ..
        } => {
            let left = view
                .countdown
                .as_ref()
                .map_or(*pause_ms, |running| view.left_ms(running));
            (
                Tone::Pending,
                countdown(*route, *copies, left),
                target.as_deref(),
            )
        }
        Status::Writing { target, .. } => (Tone::Working, words("WRITING…"), target.as_deref()),
        Status::Thinking { target } => (Tone::Working, words("THINKING…"), target.as_deref()),
        Status::StartingChat { target, again } => (
            Tone::Working,
            words(if *again {
                "RESTARTING MODEL…"
            } else {
                "STARTING MODEL…"
            }),
            target.as_deref(),
        ),
        Status::Transcribing { target, .. } => {
            (Tone::Working, words("TRANSCRIBING…"), target.as_deref())
        }
        Status::Finishing { target, .. } => (Tone::Working, words("FINISHING…"), target.as_deref()),
        Status::CleaningUp { target } => (Tone::Working, words("CLEANING UP…"), target.as_deref()),
        Status::Working { target, .. } => (Tone::Working, words("WORKING"), target.as_deref()),
        Status::Done {
            target,
            route,
            notice,
            ..
        } => (
            Tone::Done,
            done_message(*route, target.as_deref(), notice),
            None,
        ),
        Status::Failed { headline, .. } => (Tone::Failed, words(headline), None),
        Status::Loading { notice, .. } => (Tone::Working, notice.clone(), None),
        Status::Paused | Status::Listening | Status::KeyboardOnly => {
            (Tone::Live, String::new(), None)
        }
    }
}

/// The pill's countdown: the action and whole seconds left, rounded down but at least one.
fn countdown(route: CaptureRoute, copies: bool, left_ms: u64) -> String {
    let action = if copies_text(route, copies) {
        "COPY"
    } else {
        match route {
            CaptureRoute::Paste => "PASTE",
            CaptureRoute::Write => "WRITE",
            CaptureRoute::Submit => "SUBMIT",
            CaptureRoute::Task => "TASK",
        }
    };
    format!("{action} IN {} s", (left_ms / 1000).max(1))
}

/// What the card shows, when it shows.
struct Draft {
    committed: String,
    partial: String,
    notice: bool,
    caret: bool,
    /// The ending's route word and the countdown's share left, while one counts down.
    ending: Option<(&'static str, f32, bool)>,
}

fn draft(view: &View) -> Option<Draft> {
    let words = |committed: &str, partial: &str, caret: bool| Draft {
        committed: committed.to_owned(),
        partial: partial.to_owned(),
        notice: false,
        caret,
        ending: None,
    };
    match &view.status {
        Status::Capturing {
            notice: Some(notice),
            ..
        } => Some(Draft {
            notice: true,
            ..words(notice, "", false)
        }),
        _ if !view.show_draft && !view.status.requires_final_preview() => None,
        Status::Capturing {
            committed, partial, ..
        } => Some(words(
            committed,
            partial.as_deref().unwrap_or_default(),
            true,
        )),
        Status::Pending {
            text,
            route,
            waiting_for_quiet,
            ..
        } => {
            #[expect(
                clippy::cast_precision_loss,
                reason = "left and total are at most 5000 ms, exactly representable in f32"
            )]
            let left = view.countdown.as_ref().map_or(1.0, |running| {
                view.left_ms(running) as f32 / running.total_ms.max(1) as f32
            });
            let word = match route {
                CaptureRoute::Paste => "paste",
                CaptureRoute::Write => "write",
                CaptureRoute::Submit => "submit",
                CaptureRoute::Task => "task",
            };
            Some(Draft {
                ending: Some((word, left, !*waiting_for_quiet)),
                ..words(text, "", false)
            })
        }
        Status::Writing { text, .. } => Some(words(text, "", true)),
        _ => None,
    }
}

/// The card under the pill: the draft's last lines, the ending and the countdown bar.
fn card(
    canvas: &mut Canvas,
    text: &mut Text,
    view: &View,
    theme: &Theme,
    words: &Draft,
    top: f32,
) -> bool {
    let measures = theme.measures;
    let style = measures.text(DRAFT);
    let (inset_x, inset_y) = (
        art::DRAFT_INSETS.0 * measures.spacing,
        art::DRAFT_INSETS.1 * measures.spacing,
    );
    let radius = (art::CARD_RADIUS + measures.radius_delta).max(0.0);
    let scale = canvas.scale_of();
    let width = (WIDTH - 2.0 * art::SIDE) * art::CARD_SHARE;
    let left = (WIDTH - width) / 2.0;
    let inner = width - 2.0 * art::BASE_EDGE - 2.0 * inset_x;
    let (surface, committed_color, ending_color) =
        match (view.status.requires_final_preview(), words.notice) {
            (true, _) => (SUBMIT_DRAFT_SURFACE, SUBMIT_DRAFT_TEXT, SUBMIT_DRAFT_TEXT),
            (false, true) => (theme.card, theme.state.pending, theme.state.pending),
            (false, false) => (theme.card, theme.fg, theme.state.pending),
        };
    let committed = spaced(&words.committed, &words.partial);
    let spans = [
        (committed.as_str(), committed_color),
        (words.partial.as_str(), theme.muted),
    ];
    let layout = Layout::Wrapped { width: inner };
    let lines = text.line_widths(&spans, style, layout, scale);
    let last = lines.last().copied().unwrap_or(0.0);
    #[expect(
        clippy::cast_precision_loss,
        reason = "rounding starts above 2^24 lines, where the count still exceeds the three-line cap"
    )]
    let mut count = lines.len().max(1) as f32;
    // The ending sits after the last word, or on a line of its own when it does not fit.
    let (ending_gap, ending_pad, ..) = art::ENDING;
    let ending_width = words.ending.map_or(0.0, |(word, ..)| {
        text.measure(&[(word, ending_color)], style, Layout::Line, scale)
            .0
            + 2.0 * ending_pad
    });
    let mut end_x = last + ending_gap;
    if words.ending.is_some() && end_x + ending_width > inner {
        count += 1.0;
        end_x = 0.0;
    }
    let shown = count.min(art::DRAFT_LINES);
    let text_height = shown * style.line_height;
    let counting = words.ending.is_some();
    let (bar_gap, bar_height, ..) = art::COUNTDOWN;
    let height = 2.0 * art::BASE_EDGE
        + 2.0 * inset_y
        + text_height
        + if counting { bar_gap + bar_height } else { 0.0 };
    shadow(canvas, (left, top, width, height), radius, theme);
    canvas.rect((left, top, width, height), radius, &Fill::Solid(surface));
    canvas.outline(
        (left, top, width, height),
        radius,
        theme.line,
        measures.border,
    );
    let text_left = left + art::BASE_EDGE + inset_x;
    let text_top = top + art::BASE_EDGE + inset_y;
    let scrolled = (count - shown) * style.line_height;
    canvas.clip(Some((text_left, text_top, inner, text_height)));
    let origin = text_top - scrolled;
    text.draw(canvas, &spans, style, layout, (text_left, origin));
    let last_top = origin + (count - 1.0) * style.line_height;
    if let Some((word, ..)) = words.ending {
        ending_badge(
            canvas,
            text,
            (word, ending_color),
            style,
            (text_left + end_x, last_top, ending_width),
        );
    }
    if words.caret && view.caret_on() {
        let (left, top, width, height) = art::CARET;
        canvas.rect(
            (
                text_left + last + left,
                last_top + top,
                width,
                height * measures.font_scale,
            ),
            0.0,
            &Fill::Solid(theme.accent),
        );
    }
    canvas.clip(None);
    if let Some((_, share, running)) = words.ending {
        card_countdown(
            canvas,
            theme,
            (text_left, text_top + text_height + bar_gap, inner),
            share,
            running,
        );
    }
    count <= art::DRAFT_LINES
        && text_top + text_height <= HEIGHT
        && lines.iter().all(|width| *width <= inner)
}

fn card_countdown(
    canvas: &mut Canvas,
    theme: &Theme,
    (left, top, width): (f32, f32, f32),
    share: f32,
    running: bool,
) {
    let (_, height, radius, waiting) = art::COUNTDOWN;
    canvas.rect((left, top, width, height), radius, &Fill::Solid(theme.line));
    let bar = theme
        .state
        .pending
        .faded(if running { 1.0 } else { waiting });
    canvas.rect(
        (left, top, width * share, height),
        radius,
        &Fill::Solid(bar),
    );
}

fn ending_badge(
    canvas: &mut Canvas,
    text: &mut Text,
    (word, color): (&str, Rgba),
    style: Style,
    (x, top, width): (f32, f32, f32),
) {
    let (_, pad, radius, inset, opacity) = art::ENDING;
    canvas.rect(
        (x, top + inset, width, style.line_height - 2.0 * inset),
        radius,
        &Fill::Solid(color.faded(opacity)),
    );
    text.draw(
        canvas,
        &[(word, color)],
        style,
        Layout::Line,
        (x + pad, top),
    );
}

/// A soft shadow 6 points under a rounded rectangle.
fn shadow(
    canvas: &mut Canvas,
    (x, y, width, height): (f32, f32, f32, f32),
    radius: f32,
    theme: &Theme,
) {
    for (spread, alpha) in art::SHADOW_LAYERS {
        canvas.rect(
            (
                x - spread,
                y + art::SHADOW_OFFSET - spread / 2.0,
                width + 2.0 * spread,
                height + spread,
            ),
            radius + spread,
            &Fill::Solid(theme.shadow.faded(alpha * theme.measures.glow_ratio)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thalvi_desktop::{config::Look, theme::defaults_of};

    fn dark() -> Theme {
        Theme::resolve(&defaults_of(Look::Neon), true)
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the overlay's size in pixels at scale 1"
    )]
    const COLUMNS: usize = WIDTH as usize;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the overlay's size in pixels at scale 1"
    )]
    const ROWS: usize = HEIGHT as usize;
    use crate::overlay::view::HEIGHT;

    thread_local! {
        /// The fonts one test draws with; loading the system's fonts is slow, above all on Windows.
        static TEXT: std::cell::RefCell<Text> = std::cell::RefCell::new(Text::new());
    }

    fn with_text<R>(use_text: impl FnOnce(&mut Text) -> R) -> R {
        TEXT.with_borrow_mut(use_text)
    }

    fn words(committed: &str, caret: bool, ending: Option<(&'static str, f32, bool)>) -> Draft {
        Draft {
            committed: committed.to_owned(),
            partial: String::new(),
            notice: false,
            caret,
            ending,
        }
    }

    /// The card for `words`, drawn alone at blink time `clock`.
    fn drawn(words: &Draft, clock: f64) -> Canvas {
        let mut canvas = Canvas::new(WIDTH, HEIGHT, 1.0).unwrap();
        let mut view = View::new(false);
        view.clock = clock;
        with_text(|text| card(&mut canvas, text, &view, &dark(), words, 10.0));
        canvas
    }

    fn pixels(canvas: &Canvas) -> Vec<[u8; 4]> {
        let mut frame = thalvi_desktop::host::OverlayFrame::default();
        canvas.frame_into(&mut frame);
        frame.bgra.as_chunks::<4>().0.to_vec()
    }

    /// The last row the card's body covers.
    fn bottom(canvas: &Canvas) -> usize {
        let width = COLUMNS;
        let pixels = pixels(canvas);
        (0..ROWS)
            .rfind(|y| {
                pixels[y * width..(y + 1) * width]
                    .iter()
                    .any(|p| p[3] >= 200)
            })
            .unwrap()
    }

    /// Where the blinking caret is: the leftmost column and the top row that change with the blink.
    fn caret(words: &Draft) -> Option<(usize, usize)> {
        let (on, off) = (pixels(&drawn(words, 0.0)), pixels(&drawn(words, 0.6)));
        let width = COLUMNS;
        let changed: Vec<usize> = (0..on.len()).filter(|i| on[*i] != off[*i]).collect();
        let left = changed.iter().map(|i| i % width).min()?;
        Some((left, changed[0] / width))
    }

    #[test]
    fn neons_card_puts_the_ending_after_the_last_word_or_on_its_own_line_and_counts_under_it() {
        let ending = Some(("paste", 1.0, true));
        let plain = bottom(&drawn(&words("hello", false, None), 0.0));
        let counting = bottom(&drawn(&words("hello", false, ending), 0.0));
        assert_eq!(counting - plain, 12, "room for the countdown bar");
        let mut committed = "hello".to_owned();
        let grown = (0..200)
            .find_map(|_| {
                committed.push_str(" ab");
                let height = bottom(&drawn(&words(&committed, false, ending), 0.0));
                (height > counting).then_some(height)
            })
            .expect("200 more words grow the card");
        assert_eq!(
            bottom(&drawn(&words(&committed, false, None), 0.0)),
            plain,
            "the words alone still fit one line"
        );
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a line's height"
        )]
        let line = DRAFT.line_height.round() as usize;
        assert!(grown.abs_diff(counting + line) <= 1, "{grown} {counting}");
    }

    #[test]
    fn neons_caret_follows_the_last_word_onto_new_lines_and_stays_in_view_as_the_card_scrolls() {
        let width = |words: &str| {
            with_text(|text| text.measure(&[(words, dark().fg)], DRAFT, Layout::Line, 1.0)).0
        };
        let (short, long) = ("hi", "hello there");
        let (at_short, row) = caret(&words(short, true, None)).unwrap();
        let (at_long, same_row) = caret(&words(long, true, None)).unwrap();
        #[expect(
            clippy::cast_precision_loss,
            reason = "the two scale-1 caret columns differ by less than 460, exact in f32"
        )]
        let moved = (at_long - at_short) as f32;
        assert!(
            (moved - (width(long) - width(short))).abs() <= 1.5,
            "{moved}"
        );
        assert_eq!(row, same_row);
        assert_eq!(
            caret(&words(short, false, None)),
            None,
            "no caret without one"
        );
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a line's height"
        )]
        let line = DRAFT.line_height.round() as usize;
        // Three words at a time, far less than a line, the caret drops one line at each wrap until
        // the card shows three lines.
        let mut draft = "word".to_owned();
        let mut rows = vec![row];
        for _ in 0..27 {
            draft.push_str(" word word word");
            let (_, now) = caret(&words(&draft, true, None)).unwrap();
            if now != *rows.last().unwrap() {
                rows.push(now);
            }
        }
        let expected: Vec<usize> = (0..3).map(|n| row + n * line).collect();
        assert!(
            rows.len() == 3
                && rows
                    .iter()
                    .zip(&expected)
                    .all(|(got, want)| got.abs_diff(*want) <= 1),
            "80 words fill more than three lines, and the newest shows last: {rows:?}"
        );
    }

    #[test]
    fn neons_countdown_bar_fills_by_its_share_and_fades_while_waiting_for_quiet() {
        let amber = |share: f32, running: bool| {
            pixels(&drawn(
                &words("hello", false, Some(("paste", share, running))),
                0.0,
            ))
            .iter()
            .filter(|[b, g, r, _]| {
                r.abs_diff(0xff) < 24 && g.abs_diff(0xc2) < 24 && b.abs_diff(0x4b) < 24
            })
            .count()
        };
        let (full, half) = (amber(1.0, true), amber(0.5, true));
        assert!(
            half * 10 > full * 4 && half * 10 < full * 6,
            "{full} {half}"
        );
        assert!(
            amber(1.0, false) * 4 < full,
            "faded while waiting for quiet"
        );
    }

    #[test]
    fn neons_card_shows_a_notice_always_and_the_draft_only_when_asked() {
        let capturing = |notice: Option<&str>| Status::Capturing {
            target: None,
            committed: "hello".to_owned(),
            partial: Some("wor".to_owned()),
            notice: notice.map(str::to_owned),
        };
        let pending = |waiting_for_quiet| Status::Pending {
            target: None,
            route: CaptureRoute::Submit,
            text: "hello".to_owned(),
            waiting_for_quiet,
            pause_ms: 2000,
            copies: false,
            final_confirmation: None,
        };
        let writing = Status::Writing {
            target: None,
            text: "hello".to_owned(),
        };
        let cases = [
            (
                capturing(Some("Speak up")),
                false,
                Some(("Speak up", "", true, false, None)),
            ),
            (capturing(None), false, None),
            (
                capturing(None),
                true,
                Some(("hello", "wor", false, true, None)),
            ),
            (
                pending(false),
                true,
                Some(("hello", "", false, false, Some(("submit", true)))),
            ),
            (
                pending(true),
                true,
                Some(("hello", "", false, false, Some(("submit", false)))),
            ),
            (pending(false), false, None),
            (writing, true, Some(("hello", "", false, true, None))),
            (Status::Listening, true, None),
        ];
        for (status, show_draft, want) in cases {
            let mut view = View::new(false);
            view.set_status(status.clone(), 0.0);
            view.set_draft(show_draft);
            let got = draft(&view).map(|d| {
                (
                    d.committed,
                    d.partial,
                    d.notice,
                    d.caret,
                    d.ending.map(|(word, _, running)| (word, running)),
                )
            });
            let want = want.map(|(committed, partial, notice, caret, ending)| {
                (
                    committed.to_owned(),
                    partial.to_owned(),
                    notice,
                    caret,
                    ending,
                )
            });
            assert_eq!(got, want, "{status:?} draft {show_draft}");
        }
    }

    #[test]
    fn neons_pill_shows_when_a_live_capture_is_transcribing() {
        let mut view = View::new(false);
        view.set_status(
            Status::Transcribing {
                target: Some("Editor".to_owned()),
                route: CaptureRoute::Paste,
                cancellable: true,
            },
            0.0,
        );

        assert_eq!(
            pill_words(&view),
            (Tone::Working, "TRANSCRIBING…".to_owned(), Some("Editor"))
        );
        assert_eq!(view.label.as_deref(), Some("TRANSCRIBING…"));
    }

    #[test]
    fn neons_pill_counts_down_whole_seconds_rounded_down_but_never_below_one() {
        for (route, copies, left_ms, want) in [
            (CaptureRoute::Paste, false, 2500, "PASTE IN 2 s"),
            (CaptureRoute::Write, false, 1000, "WRITE IN 1 s"),
            (CaptureRoute::Submit, false, 999, "SUBMIT IN 1 s"),
            (CaptureRoute::Paste, true, 3000, "COPY IN 3 s"),
            (CaptureRoute::Task, true, 2000, "TASK IN 2 s"),
        ] {
            assert_eq!(countdown(route, copies, left_ms), want);
        }
    }

    #[test]
    fn neons_cancel_mark_is_centred_on_its_place() {
        let mut canvas = Canvas::new(WIDTH, HEIGHT, 1.0).unwrap();
        with_text(|text| cancel_mark(&mut canvas, text, &dark(), (100.0, 50.0)));
        let width = COLUMNS;
        let inked: Vec<(usize, usize)> = pixels(&canvas)
            .iter()
            .enumerate()
            .filter(|(_, p)| p[3] > 60)
            .map(|(i, _)| (i % width, i / width))
            .collect();
        assert!(!inked.is_empty(), "a mark is drawn");
        let span = |pick: fn(&(usize, usize)) -> usize| {
            let values = inked.iter().map(pick);
            values.clone().min().unwrap() + values.max().unwrap()
        };
        assert!(span(|p| p.0).abs_diff(2 * 110) <= 3, "centred on x + 10");
        assert!(span(|p| p.1).abs_diff(2 * 50) <= 5, "centred on the middle");
    }

    /// Neon's whole drawing of `status`, with the draft card off.
    fn look(status: Status) -> Vec<[u8; 4]> {
        let mut canvas = Canvas::new(WIDTH, HEIGHT, 1.0).unwrap();
        let mut view = View::new(false);
        view.set_draft(false);
        view.set_status(status, 0.0);
        with_text(|text| Neon::new(dark()).draw(&mut canvas, text, &view));
        pixels(&canvas)
    }

    /// Whether `pixel` is `color` laid over the pill's glass at a third of its strength or more:
    /// thin or faint glyphs never reach the colour itself.
    fn tinted([b, g, r, _]: [u8; 4], color: Rgba) -> bool {
        let glass = dark().glass;
        let from = |value: u8, base: u8| f32::from(value) - f32::from(base) * glass.a;
        let seen = [from(r, glass.r), from(g, glass.g), from(b, glass.b)];
        let full = [
            from(color.r, glass.r),
            from(color.g, glass.g),
            from(color.b, glass.b),
        ];
        let along = seen.iter().zip(&full).map(|(s, f)| s * f).sum::<f32>()
            / full.iter().map(|f| f * f).sum::<f32>();
        let off = seen
            .iter()
            .zip(&full)
            .map(|(s, f)| (s - along * f).powi(2))
            .sum::<f32>()
            .sqrt();
        along > 0.33 && off < 24.0
    }

    /// The pill's left and right edge and its middle row.
    fn pill(pixels: &[[u8; 4]]) -> (usize, usize, usize) {
        let solid: Vec<usize> = (0..60 * COLUMNS).filter(|i| pixels[*i][3] > 200).collect();
        let columns = solid.iter().map(|i| i % COLUMNS);
        let rows = solid.iter().map(|i| i / COLUMNS);
        (
            columns.clone().min().unwrap(),
            columns.max().unwrap(),
            usize::midpoint(rows.clone().min().unwrap(), rows.max().unwrap()),
        )
    }

    /// How many of `columns` hold a pixel of `color` in the band through the pill's middle, where
    /// its border touches only the outermost columns.
    fn count(pixels: &[[u8; 4]], color: Rgba, columns: std::ops::Range<usize>) -> usize {
        let (_, _, middle) = pill(pixels);
        columns
            .filter(|x| (middle - 5..=middle + 5).any(|y| tinted(pixels[y * COLUMNS + x], color)))
            .count()
    }

    fn capturing() -> Status {
        Status::Capturing {
            target: None,
            committed: String::new(),
            partial: None,
            notice: None,
        }
    }

    #[test]
    fn neons_dot_and_bars_take_the_tone_and_its_ending_words_their_own() {
        let pending = |waiting_for_quiet| Status::Pending {
            target: None,
            route: CaptureRoute::Paste,
            text: String::new(),
            waiting_for_quiet,
            pause_ms: 2000,
            copies: false,
            final_confirmation: None,
        };
        let working = Status::Working {
            target: None,
            route: CaptureRoute::Paste,
            cancellable: false,
        };
        let done = Status::Done {
            target: None,
            route: CaptureRoute::Paste,
            notice: "Delivered.".to_owned(),
            wake_words: true,
        };
        let failed = Status::Failed {
            kind: crate::status::FailureKind::Other,
            offer: None,
            notice: String::new(),
            headline: "No words heard".to_owned(),
            causes: Vec::new(),
        };
        // The dot spans about 10 columns; the nine bars add 27 more.
        // The bars start 31 points into the pill and span 51; the label sits inside its border.
        let bars = |pixels: &[[u8; 4]]| {
            let (left, _, _) = pill(pixels);
            left + 31..left + 31 + 51
        };
        let label = |pixels: &[[u8; 4]]| {
            let (left, right, _) = pill(pixels);
            left + 4..right - 4
        };
        let all = 0..COLUMNS;
        let (live, waiting, counting, busy) = (
            look(capturing()),
            look(pending(true)),
            look(pending(false)),
            look(working),
        );
        let (delivered, refused) = (look(done), look(failed));
        for (state, pixels, tinted, at, plain) in [
            (
                "live",
                &live,
                Some(dark().accent),
                bars(&live),
                dark().state.pending,
            ),
            (
                "counting down",
                &counting,
                Some(dark().state.pending),
                bars(&counting),
                dark().accent,
            ),
            (
                "waiting for quiet",
                &waiting,
                None,
                all.clone(),
                dark().accent,
            ),
            (
                "working",
                &busy,
                Some(dark().blue),
                all.clone(),
                dark().accent,
            ),
            (
                "delivered",
                &delivered,
                Some(dark().accent),
                label(&delivered),
                dark().state.pending,
            ),
            (
                "failed",
                &refused,
                Some(dark().state.failed),
                label(&refused),
                dark().accent,
            ),
        ] {
            if let Some(tint) = tinted {
                let tinted = count(pixels, tint, at.clone());
                assert!(tinted > 5, "{state}: {tinted} tinted columns");
            }
            assert_eq!(count(pixels, plain, all.clone()), 0, "{state}");
        }
        assert_eq!(
            count(&waiting, dark().state.pending, all),
            0,
            "waiting for quiet"
        );
    }

    #[test]
    fn a_listening_pill_makes_room_for_its_bars_and_a_finished_one_drops_them() {
        let width = |pixels: Vec<[u8; 4]>| {
            let columns: Vec<usize> = (0..pixels.len())
                .filter(|i| i / COLUMNS < 60 && pixels[*i][3] > 200)
                .map(|i| i % COLUMNS)
                .collect();
            columns.iter().max().unwrap() - columns.iter().min().unwrap()
        };
        let done = Status::Done {
            target: None,
            route: CaptureRoute::Paste,
            notice: "Delivered.".to_owned(),
            wake_words: true,
        };
        let (listening, finished) = (width(look(capturing())), width(look(done)));
        assert!(listening > finished + 20, "{listening} {finished}");
    }
}
