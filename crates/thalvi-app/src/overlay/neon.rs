//! The neon look: a glass pill with a dot, nine level bars, a label and the target, and under it a
//! card with the last lines of the draft; in the system's light or dark colours.

use std::f32::consts::PI;

use thalvi_desktop::capture::CaptureRoute;

use super::paint::{Canvas, Fill, Layout, Rgba, Style, Text};
use super::view::{Tone, View, WIDTH, copies_text, done_message};
use crate::status::Status;

/// Space around the pill and the card inside the overlay.
const TOP: f32 = 26.0;
const SIDE: f32 = 12.0;
const GAP: f32 = 6.0;
const BARS: usize = 9;
const BAR_W: f32 = 3.0;
/// The nine bars with a bar's width between each two.
const STRIP: f32 = 17.0 * BAR_W;
const BAR_H: f32 = 22.0;
/// A bar's height while nothing moves, as a share of its full height.
const FLAT: f32 = 4.0 / 22.0;
const ENVELOPE: [f32; BARS] = [0.45, 0.65, 0.82, 0.95, 1.0, 0.95, 0.82, 0.65, 0.45];
const DRAFT: Style = Style {
    size: 13.0,
    line_height: 13.0 * 1.45,
    weight: 400,
    mono: false,
    tracking: 0.0,
};
/// The pill's label, its message once a capture ends, and the target it names.
const LABEL: Style = Style {
    size: 11.0,
    line_height: 14.0,
    weight: 700,
    mono: true,
    tracking: 1.32,
};
const MESSAGE: Style = Style {
    size: 13.0,
    line_height: 17.0,
    weight: 600,
    mono: false,
    tracking: 0.0,
};
const TARGET: Style = Style {
    size: 12.0,
    line_height: 15.0,
    weight: 400,
    mono: true,
    tracking: 0.0,
};
/// The card shows the draft's last lines; earlier ones scroll out of its top.
const DRAFT_LINES: f32 = 3.0;

/// The look's colours in one system theme.
struct Palette {
    glass: Rgba,
    card: Rgba,
    line: Rgba,
    fg: Rgba,
    muted: Rgba,
    neon: Rgba,
    blue: Rgba,
    warn: Rgba,
    amber: Rgba,
}

const DARK: Palette = Palette {
    glass: Rgba::rgb(2, 6, 23).alpha(0.92),
    card: Rgba::rgb(4, 18, 46).alpha(0.95),
    line: Rgba::rgb(0x12, 0x31, 0x5e),
    fg: Rgba::rgb(0xea, 0xff, 0xf3),
    muted: Rgba::rgb(0x9c, 0xc7, 0xb4),
    neon: Rgba::rgb(0x2b, 0xff, 0x88),
    blue: Rgba::rgb(0x3b, 0x82, 0xf6),
    warn: Rgba::rgb(0xff, 0x6b, 0x5e),
    amber: Rgba::rgb(0xff, 0xc2, 0x4b),
};

const LIGHT: Palette = Palette {
    glass: Rgba::rgb(247, 251, 255).alpha(0.94),
    card: Rgba::rgb(255, 255, 255).alpha(0.96),
    line: Rgba::rgb(0xb9, 0xcb, 0xdd),
    fg: Rgba::rgb(0x07, 0x18, 0x2e),
    muted: Rgba::rgb(0x3d, 0x5a, 0x73),
    neon: Rgba::rgb(0x00, 0x7a, 0x43),
    blue: Rgba::rgb(0x0b, 0x3d, 0x91),
    warn: Rgba::rgb(0xb4, 0x23, 0x18),
    amber: Rgba::rgb(0x8a, 0x5a, 0x00),
};

/// The neon look's state between frames: its theme, the bars' eased level, and where it last
/// drew its cancel button.
pub(super) struct Neon {
    dark: bool,
    shown: f32,
    last: Option<f64>,
    pub(super) cancel: Option<(f32, f32, f32, f32)>,
}

impl Neon {
    pub(super) fn new(dark: bool) -> Self {
        Self {
            dark,
            shown: 0.0,
            last: None,
            cancel: None,
        }
    }

    /// Whether `(x, y)` points hits the cancel button as last drawn.
    pub(super) fn cancels(&self, x: f32, y: f32) -> bool {
        self.cancel.is_some_and(|(left, top, width, height)| {
            (left..left + width).contains(&x) && (top..top + height).contains(&y)
        })
    }

    pub(super) fn draw(&mut self, canvas: &mut Canvas, text: &mut Text, view: &View) {
        let colors = if self.dark { &DARK } else { &LIGHT };
        canvas.clear();
        let elapsed = self
            .last
            .map_or(0.0, |last| (view.clock - last).clamp(0.0, 0.1));
        self.last = Some(view.clock);
        let moving = matches!(view.tone, Tone::Live | Tone::Quiet) && view.wave.is_some();
        if moving {
            let rate = if view.level > self.shown { 0.6 } else { 0.2 };
            #[expect(clippy::cast_possible_truncation, reason = "a fraction of a second")]
            let pace = 1.0 - (1.0f32 - rate).powf(elapsed as f32 * 60.0);
            self.shown += (view.level - self.shown) * pace;
        } else {
            self.shown = 0.0;
        }
        let pill = self.pill(canvas, text, view, colors, moving);
        if let Some(words) = draft(view) {
            card(canvas, text, view, colors, &words, pill.1 + pill.3 + GAP);
        }
    }

    /// Draws the pill; its rectangle.
    fn pill(
        &mut self,
        canvas: &mut Canvas,
        text: &mut Text,
        view: &View,
        colors: &Palette,
        moving: bool,
    ) -> (f32, f32, f32, f32) {
        let scale = canvas.scale_of();
        let (tone, words, target) = pill_words(view);
        let ends = matches!(tone, Tone::Done | Tone::Failed);
        let tint = match tone {
            Tone::Live | Tone::Done => colors.neon,
            Tone::Quiet | Tone::Working => colors.muted,
            Tone::Pending => colors.amber,
            Tone::Failed => colors.warn,
        };
        let label_style = if ends { MESSAGE } else { LABEL };
        let label_color = match tone {
            Tone::Pending | Tone::Done | Tone::Failed => tint,
            _ => colors.muted,
        };
        let target_style = TARGET;
        let cancellable = view.cancellable();
        let mut fixed = 10.0 + 14.0 + 2.0;
        let mut parts = 0.0;
        if !ends {
            fixed += 10.0 + STRIP;
            parts += 2.0;
        }
        if cancellable {
            fixed += 20.0;
            parts += 1.0;
        }
        let room = WIDTH - 2.0 * SIDE;
        let target_words = target.map(|to| format!("→ {to}"));
        let target_width = target_words.as_deref().map_or(0.0, |words| {
            text.measure(&[(words, colors.muted)], target_style, Layout::Line, scale)
                .0
        });
        if target_words.is_some() {
            parts += 1.0;
        }
        let label_spans = [(words.as_str(), label_color)];
        let gaps = 10.0 * parts;
        let label_room = (room - fixed - gaps - target_width).max(40.0);
        let label_layout = if ends {
            Layout::Wrapped { width: label_room }
        } else {
            Layout::Line
        };
        let (label_width, label_height) =
            text.measure(&label_spans, label_style, label_layout, scale);
        let label_width = label_width.min(label_room);
        let inner = if ends { label_height } else { BAR_H };
        let height = inner + 12.0 + 2.0;
        let width = (fixed + gaps + label_width + target_width).min(room);
        let (left, top) = ((WIDTH - width) / 2.0, TOP);
        let border = match tone {
            Tone::Pending => colors.amber,
            Tone::Done => colors.neon,
            Tone::Failed => colors.warn,
            _ => colors.line,
        };
        shadow(canvas, (left, top, width, height), height / 2.0);
        canvas.rect(
            (left, top, width, height),
            height / 2.0,
            &Fill::Solid(colors.glass),
        );
        canvas.outline((left, top, width, height), height / 2.0, border);
        let middle = top + height / 2.0;
        let mut x = left + 1.0 + 10.0;
        if !ends {
            self.marks(canvas, view, colors, tone, moving, (x, middle));
            x += 10.0 + 10.0 + STRIP + 10.0;
        }
        text.draw(
            canvas,
            &label_spans,
            label_style,
            label_layout,
            (x, middle - label_height / 2.0),
        );
        x += label_width + 10.0;
        if let Some(words) = &target_words {
            text.draw(
                canvas,
                &[(words.as_str(), colors.muted)],
                target_style,
                Layout::Line,
                (x, middle - target_style.line_height / 2.0),
            );
            x += target_width + 10.0;
        }
        self.cancel = cancellable.then_some((x, middle - 10.0, 20.0, 20.0));
        if cancellable {
            cancel_mark(canvas, text, colors, (x, middle));
        }
        (left, top, width, height)
    }

    /// The dot and the nine level bars, starting at `x` around `middle`.
    fn marks(
        &self,
        canvas: &mut Canvas,
        view: &View,
        colors: &Palette,
        tone: Tone,
        moving: bool,
        (x, middle): (f32, f32),
    ) {
        let dot = match tone {
            Tone::Quiet => colors.muted,
            Tone::Pending => colors.amber,
            Tone::Working => {
                #[expect(clippy::cast_possible_truncation, reason = "a phase in seconds")]
                let phase = (view.clock % 1.0) as f32;
                colors
                    .blue
                    .alpha(1.0 - 0.65 * (0.5 - 0.5 * (2.0 * PI * phase).cos()))
            }
            _ => colors.neon,
        };
        if !matches!(tone, Tone::Quiet | Tone::Working) {
            canvas.rect(
                (x - 4.0, middle - 9.0, 18.0, 18.0),
                9.0,
                &Fill::Solid(dot.alpha(0.25)),
            );
        }
        canvas.rect((x, middle - 5.0, 10.0, 10.0), 5.0, &Fill::Solid(dot));
        let bars = x + 20.0;
        let bar_color = match tone {
            Tone::Live => colors.neon,
            Tone::Pending => colors.amber,
            Tone::Working => colors.muted.alpha(0.6),
            _ => colors.muted,
        };
        #[expect(clippy::cast_possible_truncation, reason = "a clock in seconds")]
        let now_ms = (view.clock * 1000.0) as f32;
        for (index, envelope) in (0u8..).zip(ENVELOPE) {
            let share = if moving {
                let wobble = 0.75 + 0.25 * (now_ms / 110.0 + f32::from(index) * 1.3).sin();
                FLAT + (1.0 - FLAT) * (self.shown * envelope * wobble * 1.2).min(1.0)
            } else {
                FLAT
            };
            let bar = BAR_H * share;
            canvas.rect(
                (
                    bars + f32::from(index) * 2.0 * BAR_W,
                    middle - bar / 2.0,
                    BAR_W,
                    bar,
                ),
                1.5,
                &Fill::Solid(bar_color),
            );
        }
    }
}

/// The 20-point cancel button at `x`, centred on `middle`.
fn cancel_mark(canvas: &mut Canvas, text: &mut Text, colors: &Palette, (x, middle): (f32, f32)) {
    let style = Style {
        size: 16.0,
        line_height: 20.0,
        weight: 400,
        mono: false,
        tracking: 0.0,
    };
    let spans = [("×", colors.muted)];
    let (used, _) = text.measure(&spans, style, Layout::Line, canvas.scale_of());
    text.draw(
        canvas,
        &spans,
        style,
        Layout::Line,
        (x + (20.0 - used) / 2.0, middle - 10.0),
    );
}

/// The pill's tone, its words and the target it names.
fn pill_words(view: &View) -> (Tone, String, Option<&str>) {
    let words = |text: &str| text.to_owned();
    match &view.status {
        Status::Starting { target } | Status::Capturing { target, .. } => {
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
        Status::Working { target, .. } => (Tone::Working, words("WORKING"), target.as_deref()),
        Status::Done {
            target,
            route,
            notice,
        } => (
            Tone::Done,
            done_message(*route, target.as_deref(), notice),
            None,
        ),
        Status::Failed { headline, .. } => (Tone::Failed, words(headline), None),
        Status::Loading { notice, .. } => (Tone::Working, notice.clone(), None),
        Status::Paused | Status::Listening => (Tone::Live, String::new(), None),
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
        _ if !view.show_draft => None,
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
            #[expect(clippy::cast_precision_loss, reason = "milliseconds of a pause")]
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
    colors: &Palette,
    words: &Draft,
    top: f32,
) {
    let scale = canvas.scale_of();
    let width = (WIDTH - 2.0 * SIDE) * 0.92;
    let left = (WIDTH - width) / 2.0;
    let inner = width - 2.0 - 26.0;
    let committed_color = if words.notice {
        colors.amber
    } else {
        colors.fg
    };
    let gap = if words.committed.is_empty() || words.partial.is_empty() {
        ""
    } else {
        " "
    };
    let committed = format!("{}{gap}", words.committed);
    let spans = [
        (committed.as_str(), committed_color),
        (words.partial.as_str(), colors.muted),
    ];
    let layout = Layout::Wrapped { width: inner };
    let lines = text.line_widths(&spans, DRAFT, layout, scale);
    let last = lines.last().copied().unwrap_or(0.0);
    #[expect(clippy::cast_precision_loss, reason = "a few lines")]
    let mut count = lines.len().max(1) as f32;
    // The ending sits after the last word, or on a line of its own when it does not fit.
    let ending_width = words.ending.map_or(0.0, |(word, ..)| {
        text.measure(&[(word, colors.amber)], DRAFT, Layout::Line, scale)
            .0
            + 8.0
    });
    let caret_width = if words.caret { 3.0 } else { 0.0 };
    let mut end_x = last + 5.2;
    if words.ending.is_some() && end_x + ending_width > inner {
        count += 1.0;
        end_x = 0.0;
    }
    let shown = count.min(DRAFT_LINES);
    let text_height = shown * DRAFT.line_height;
    let counting = words.ending.is_some();
    let height = 2.0 + 18.0 + text_height + if counting { 12.0 } else { 0.0 };
    shadow(canvas, (left, top, width, height), 12.0);
    canvas.rect((left, top, width, height), 12.0, &Fill::Solid(colors.card));
    canvas.outline((left, top, width, height), 12.0, colors.line);
    let text_left = left + 1.0 + 13.0;
    let text_top = top + 1.0 + 9.0;
    let scrolled = (count - shown) * DRAFT.line_height;
    canvas.clip(Some((text_left, text_top, inner, text_height)));
    let origin = text_top - scrolled;
    text.draw(canvas, &spans, DRAFT, layout, (text_left, origin));
    let last_top = origin + (count - 1.0) * DRAFT.line_height;
    if let Some((word, ..)) = words.ending {
        let x = text_left + end_x;
        canvas.rect(
            (x, last_top + 1.0, ending_width, DRAFT.line_height - 2.0),
            4.0,
            &Fill::Solid(colors.amber.alpha(0.22)),
        );
        text.draw(
            canvas,
            &[(word, colors.amber)],
            DRAFT,
            Layout::Line,
            (x + 4.0, last_top),
        );
    }
    if words.caret && view.clock % 1.0 < 0.5 {
        canvas.rect(
            (
                text_left + last + 1.0,
                last_top + 3.0,
                caret_width - 1.0,
                14.0,
            ),
            0.0,
            &Fill::Solid(colors.neon),
        );
    }
    canvas.clip(None);
    if let Some((_, share, running)) = words.ending {
        let y = text_top + text_height + 8.0;
        canvas.rect((text_left, y, inner, 4.0), 2.0, &Fill::Solid(colors.line));
        let bar = colors.amber.alpha(if running { 1.0 } else { 0.35 });
        canvas.rect((text_left, y, inner * share, 4.0), 2.0, &Fill::Solid(bar));
    }
}

/// A soft shadow 6 points under a rounded rectangle.
fn shadow(canvas: &mut Canvas, (x, y, width, height): (f32, f32, f32, f32), radius: f32) {
    for (spread, alpha) in [(12.0, 0.05), (8.0, 0.08), (4.0, 0.12)] {
        canvas.rect(
            (
                x - spread,
                y + 6.0 - spread / 2.0,
                width + 2.0 * spread,
                height + spread,
            ),
            radius + spread,
            &Fill::Solid(Rgba::rgb(0, 0, 0).alpha(alpha)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        card(&mut canvas, &mut Text::new(), &view, &DARK, words, 10.0);
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
        let grown = loop {
            committed.push_str(" ab");
            let height = bottom(&drawn(&words(&committed, false, ending), 0.0));
            if height > counting {
                break height;
            }
        };
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
        let mut text = Text::new();
        let mut width = |words: &str| {
            text.measure(&[(words, DARK.fg)], DRAFT, Layout::Line, 1.0)
                .0
        };
        let (short, long) = ("hi", "hello there");
        let (at_short, row) = caret(&words(short, true, None)).unwrap();
        let (at_long, same_row) = caret(&words(long, true, None)).unwrap();
        #[expect(clippy::cast_precision_loss, reason = "pixel columns")]
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
        // Word by word, the caret drops one line at each wrap until the card shows three lines.
        let mut draft = "word".to_owned();
        let mut rows = vec![row];
        for _ in 0..80 {
            draft.push_str(" word");
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
        cancel_mark(&mut canvas, &mut Text::new(), &DARK, (100.0, 50.0));
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
}
