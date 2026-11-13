//! The gunmetal and holo looks: a 370 × 84 module hanging from the top edge, a mic wave over one
//! line of text, and a glowing line along its bottom.

use std::f32::consts::{PI, TAU};
use std::time::Duration;

use super::paint::{Canvas, Fill, Layout, Rgba, Style, TRANSPARENT, Text, TextBounds};
use super::view::{FRAME, HEIGHT, Line, Tone, View, WAVE_H, WAVE_W, WIDTH, done_message, spaced};
use crate::status::Status;

const MODULE_W: f32 = 370.0;
const MODULE_H: f32 = 84.0;
const MODULE_X: f32 = (WIDTH - MODULE_W) / 2.0;
const SHOULDER: f32 = 16.0;
/// The bezel and slot are inset by this much on each side, and slant in by `SLANT`.
const SLOT_X: f32 = 14.0;
const SLANT: f32 = 13.0;
const SLOT_W: f32 = MODULE_W - 2.0 * SLOT_X;
const SLOT_H: f32 = MODULE_H - 6.0;
/// The slot's top-left corner, in points from the overlay's.
const SLOT: (f32, f32) = (MODULE_X + SLOT_X, 2.0);
/// The cancel button, in points from the overlay's top-left corner.
pub(super) const CANCEL: (f32, f32, f32, f32) = (SLOT.0 + SLOT_W - 24.0, SLOT.1 + 6.0, 18.0, 18.0);
/// Room around a glow for its blur.
const GLOW_PAD: f32 = 24.0;
/// Holo's shine crosses the panel in the last 30 % of each cycle.
const SHINE_CYCLE: f64 = 7.0;
const SHINE_FROM: f64 = 4.9;

const CYAN: Rgba = Rgba::rgb(107, 248, 255);
const QUIET: Rgba = Rgba::rgb(143, 179, 196);
const INK: Rgba = Rgba::rgb(234, 255, 243);
const BLACK: Rgba = Rgba::rgb(0, 0, 0);
/// A delivery's confirmation, centred in the slot.
const CONFIRMATION: Style = Style {
    size: 13.0,
    line_height: 17.0,
    weight: 600,
    mono: false,
    tracking: 0.0,
};
/// The ticker's line of words or notice.
const TICKER: Style = Style {
    size: 14.0,
    line_height: 22.0,
    weight: 400,
    mono: false,
    tracking: 0.0,
};
const WHITE: Rgba = Rgba::rgb(255, 255, 255);

/// Which of the two module looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Finish {
    /// A thin hex-mesh metal bezel around a dark glass slot.
    Gunmetal,
    /// A steel-blue frame line, a teal scanline field over a hex mesh, glowing corner pieces.
    Holo,
}

fn color(tone: Tone) -> Rgba {
    match tone {
        Tone::Live | Tone::Working => CYAN,
        Tone::Quiet => QUIET,
        Tone::Pending => Rgba::rgb(255, 194, 75),
        Tone::Done => Rgba::rgb(43, 255, 136),
        Tone::Failed => Rgba::rgb(255, 107, 94),
    }
}

/// A module look's still parts, drawn once per scale.
pub(super) struct Module {
    finish: Finish,
    background: Canvas,
    glows: Vec<(Tone, Canvas)>,
}

impl Module {
    pub(super) fn new(finish: Finish, scale: f32) -> Option<Self> {
        Some(Self {
            finish,
            background: background(finish, scale)?,
            glows: Vec::new(),
        })
    }

    /// How long until the look itself moves again while the view is still; holo's shine.
    pub(super) fn motion(&self, clock: f64) -> Option<Duration> {
        if self.finish == Finish::Gunmetal {
            return None;
        }
        let at = clock % SHINE_CYCLE;
        Some(if at >= SHINE_FROM {
            FRAME
        } else {
            Duration::from_secs_f64(SHINE_FROM - at)
        })
    }

    pub(super) fn draw(&mut self, canvas: &mut Canvas, text: &mut Text, view: &View, clock: f64) {
        canvas.copy(&self.background);
        if self.finish == Finish::Holo {
            shine(canvas, clock);
        }
        canvas.clip(Some((SLOT.0, SLOT.1, SLOT_W, SLOT_H)));
        let tone_color = color(view.tone);
        let has_text = view.line != Line::None;
        let wave_top = SLOT.1 + if has_text { 2.0 } else { 17.0 };
        #[expect(clippy::cast_possible_truncation, reason = "a clock in seconds")]
        let seconds = view.clock as f32;
        if let Some(wave) = &view.wave {
            if let Some(glow) = glow(&mut self.glows, view.tone, canvas.scale_of()) {
                halo(canvas, glow, wave_top, wave.smoothed);
            }
            let strands = if view.tone == Tone::Quiet {
                QUIET
            } else {
                CYAN
            };
            draw_wave(canvas, view, strands, seconds, (SLOT.0 + 5.0, wave_top));
        }
        if let Some(label) = &view.label {
            tag(canvas, text, label, tone_color, wave_top);
        }
        if let Status::Done {
            target,
            route,
            notice,
        } = &view.status
        {
            let message = done_message(*route, target.as_deref(), notice);
            let style = CONFIRMATION;
            let layout = Layout::Centred {
                width: SLOT_W - 16.0,
            };
            let spans = [(message.as_str(), tone_color)];
            let (_, height) = text.measure(&spans, style, layout, canvas.scale_of());
            text.draw(
                canvas,
                &spans,
                style,
                layout,
                (SLOT.0 + 8.0, SLOT.1 + (SLOT_H - height) / 2.0),
            );
        } else if has_text {
            rule(canvas);
            ticker(canvas, text, &view.line, view.caret_on());
        }
        if view.cancellable() {
            cancel_button(canvas, text, self.finish);
        }
        canvas.clip(None);
        line_strip(canvas, view, tone_color, self.finish);
    }
}

/// The tone's glow behind the wave, as strong as the voice is loud.
fn halo(canvas: &mut Canvas, glow: &Canvas, wave_top: f32, loudness: f32) {
    canvas.draw(
        glow,
        (
            SLOT.0 + 5.0 + (WAVE_W - 160.0) / 2.0 - GLOW_PAD,
            wave_top + 7.0 - GLOW_PAD,
        ),
        (loudness * 1.3).min(1.0),
    );
}

/// The faint rule above the ticker.
fn rule(canvas: &mut Canvas) {
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
}

/// The module's shadow and frame, with the finish's own panel, bezel and slot.
fn background(finish: Finish, scale: f32) -> Option<Canvas> {
    let mut canvas = Canvas::new(WIDTH, HEIGHT, scale)?;
    let mut shadow = Canvas::new(WIDTH, HEIGHT, scale)?;
    shadow.polygon(
        &trapezoid([MODULE_X, 6.0, MODULE_W, MODULE_H], SHOULDER),
        &Fill::Solid(BLACK.alpha(0.6)),
    );
    shadow.blur(6.0);
    canvas.draw(&shadow, (0.0, 0.0), 1.0);
    let outer = [MODULE_X, 0.0, MODULE_W, MODULE_H];
    let panel = [MODULE_X + 1.0, 0.0, MODULE_W - 2.0, MODULE_H - 1.0];
    match finish {
        Finish::Gunmetal => {
            let metal = [
                (0.0, Rgba::rgb(0x3a, 0x3a, 0x3a)),
                (0.8, Rgba::rgb(0x5a, 0x5a, 0x5a)),
                (1.0, Rgba::rgb(0x9a, 0x9a, 0x9a)),
            ];
            shade(&mut canvas, outer, SHOULDER, &metal);
            let face = [
                (0.0, Rgba::rgb(0x1c, 0x1c, 0x1c)),
                (0.6, Rgba::rgb(0x2c, 0x2c, 0x2c)),
                (1.0, Rgba::rgb(0x3c, 0x3c, 0x3c)),
            ];
            shade(&mut canvas, panel, SHOULDER, &face);
            hexes(&mut canvas, panel, SHOULDER, 0.06);
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
        }
        Finish::Holo => {
            canvas.polygon(
                &trapezoid(outer, SHOULDER),
                &Fill::Solid(Rgba::rgb(0x5d, 0x92, 0xb4)),
            );
            let field = [
                (0.0, Rgba::rgb(0x0f, 0x25, 0x30)),
                (0.55, Rgba::rgb(0x08, 0x14, 0x19)),
                (1.0, Rgba::rgb(0x0b, 0x1a, 0x21)),
            ];
            shade(&mut canvas, panel, SHOULDER, &field);
            hexes(&mut canvas, panel, SHOULDER, 0.042);
            canvas.clip_to(&trapezoid(panel, SHOULDER));
            let mut y = panel[1];
            while y < panel[1] + panel[3] {
                canvas.rect(
                    (panel[0], y, panel[2], 1.0),
                    0.0,
                    &Fill::Solid(CYAN.alpha(0.045)),
                );
                y += 3.0;
            }
            let mut vignette = Canvas::new(WIDTH, HEIGHT, scale)?;
            vignette.stroke(
                &closed(&trapezoid(panel, SHOULDER)),
                18.0,
                &Fill::Solid(Rgba::rgb(93, 146, 180).alpha(0.3)),
            );
            vignette.blur(9.0);
            canvas.draw(&vignette, (0.0, 0.0), 1.0);
            canvas.clip(None);
            accents(&mut canvas, scale)?;
        }
    }
    Some(canvas)
}

/// Holo's corner pieces and slanted end caps, glowing.
fn accents(canvas: &mut Canvas, scale: f32) -> Option<()> {
    let reach = 20.0;
    let lean = (SHOULDER / MODULE_H).atan();
    let (dx, dy) = (lean.sin() * reach, lean.cos() * reach);
    let mut pieces: Vec<Vec<(f32, f32)>> = Vec::new();
    for side in [1.0f32, -1.0] {
        let edge = if side > 0.0 {
            SHOULDER
        } else {
            MODULE_W - SHOULDER
        };
        pieces.push(vec![
            (edge + side * dx, MODULE_H - dy),
            (edge, MODULE_H),
            (edge + side * reach, MODULE_H),
        ]);
        let x = |share: f32| {
            if side > 0.0 {
                SHOULDER * share + 9.0
            } else {
                MODULE_W - SHOULDER * share - 9.0
            }
        };
        pieces.push(vec![(x(0.35), MODULE_H * 0.3), (x(0.65), MODULE_H * 0.7)]);
    }
    let pieces: Vec<Vec<(f32, f32)>> = pieces
        .into_iter()
        .map(|piece| piece.into_iter().map(|(x, y)| (MODULE_X + x, y)).collect())
        .collect();
    let mut glow = Canvas::new(WIDTH, HEIGHT, scale)?;
    for piece in &pieces {
        glow.stroke(piece, 2.0, &Fill::Solid(CYAN.alpha(0.9)));
    }
    glow.blur(3.0);
    canvas.draw(&glow, (0.0, 0.0), 1.0);
    for piece in &pieces {
        canvas.stroke(piece, 2.0, &Fill::Solid(CYAN));
    }
    Some(())
}

/// Holo's sheen crossing the panel once every few seconds.
fn shine(canvas: &mut Canvas, clock: f64) {
    let at = clock % SHINE_CYCLE;
    if at < SHINE_FROM {
        return;
    }
    #[expect(clippy::cast_possible_truncation, reason = "a share of one cycle")]
    let travel = ((at - SHINE_FROM) / (SHINE_CYCLE - SHINE_FROM)) as f32;
    let panel = [MODULE_X + 1.0, 0.0, MODULE_W - 2.0, MODULE_H - 1.0];
    let x = panel[0] - 80.0 + 720.0 * travel;
    let stops = [
        (0.0, TRANSPARENT),
        (0.5, WHITE.alpha(0.07)),
        (1.0, TRANSPARENT),
    ];
    canvas.clip_to(&trapezoid(panel, SHOULDER));
    canvas.rect(
        (x, panel[1], 70.0, panel[3]),
        0.0,
        &Fill::Linear {
            from: (x, 0.0),
            to: (x + 70.0, 0.0),
            stops: &stops,
        },
    );
    canvas.clip(None);
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

fn closed(points: &[(f32, f32)]) -> Vec<(f32, f32)> {
    points.iter().chain(points.first()).copied().collect()
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

/// The hex mesh on the panel in white at `alpha`, kept where it lies inside the trapezoid.
fn hexes(canvas: &mut Canvas, [x, y, width, height]: [f32; 4], inset: f32, alpha: f32) {
    let inside = |p: (f32, f32)| {
        let down = (p.1 - y) / height;
        (0.0..=1.0).contains(&down) && p.0 >= x + inset * down && p.0 <= x + width - inset * down
    };
    let line = Fill::Solid(WHITE.alpha(alpha));
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
            &Fill::Solid(color(tone).alpha(0.2)),
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
fn draw_wave(canvas: &mut Canvas, view: &View, color: Rgba, seconds: f32, origin: (f32, f32)) {
    const STEP: f32 = 4.0;
    let Some(wave) = &view.wave else {
        return;
    };
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
    let style = TICKER;
    let (left, top, width) = (SLOT.0 + 24.0, SLOT.1 + 52.0, SLOT_W - 48.0);
    let right = left + width;
    let scale = canvas.scale_of();
    match line {
        Line::None => {}
        Line::Notice(notice) => {
            let spans = [(notice.as_str(), INK)];
            let (used, _) = text.measure(&spans, style, Layout::Line, scale);
            canvas.set_text_bounds(Some(TextBounds {
                left,
                right,
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
            let committed = spaced(committed, partial);
            let spans = [(committed.as_str(), INK), (partial.as_str(), QUIET)];
            let (used, _) = text.measure(&spans, style, Layout::Line, scale);
            // A line too long to fit ends 3 points short of the right edge, leaving room for the caret.
            let start = (right - 3.0 - used).min(left);
            let long = start < left;
            canvas.set_text_bounds(Some(TextBounds {
                left,
                right,
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

/// The button's face in `finish`; the colour its × is drawn in.
fn button_face(canvas: &mut Canvas, finish: Finish) -> Rgba {
    let (_, y, _, height) = CANCEL;
    match finish {
        Finish::Gunmetal => {
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
            Rgba::rgb(0xe1, 0xe4, 0xe7)
        }
        Finish::Holo => {
            canvas.rect(CANCEL, 2.0, &Fill::Solid(BLACK.alpha(0.4)));
            canvas.outline(CANCEL, 2.0, QUIET.alpha(0.4));
            QUIET
        }
    }
}

fn cancel_button(canvas: &mut Canvas, text: &mut Text, finish: Finish) {
    let (x, y, width, height) = CANCEL;
    let glyph = button_face(canvas, finish);
    let style = Style {
        size: 14.0,
        line_height: height,
        weight: 400,
        mono: false,
        tracking: 0.0,
    };
    let spans = [("×", glyph)];
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
fn line_strip(canvas: &mut Canvas, view: &View, color: Rgba, finish: Finish) {
    let inset = match finish {
        Finish::Gunmetal => SHOULDER + 4.0,
        Finish::Holo => SHOULDER + 22.0,
    };
    let full = MODULE_W - 2.0 * inset;
    let left = MODULE_X + inset;
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

#[cfg(test)]
mod tests {
    use thalvi_desktop::capture::CaptureRoute;
    use thalvi_desktop::host::OverlayFrame;

    use super::*;

    thread_local! {
        /// The fonts one test draws with; loading the system's fonts is slow, above all on Windows.
        static TEXT: std::cell::RefCell<Text> = std::cell::RefCell::new(Text::new());
    }

    fn with_text<R>(use_text: impl FnOnce(&mut Text) -> R) -> R {
        TEXT.with_borrow_mut(use_text)
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the overlay's size in pixels at scale 1"
    )]
    const COLUMNS: usize = WIDTH as usize;

    /// The ink of `line` along the ticker's row: the strongest alpha in each column.
    fn ink(line: &Line, blink_on: bool) -> Vec<u8> {
        let mut canvas = Canvas::new(WIDTH, HEIGHT, 1.0).unwrap();
        with_text(|text| ticker(&mut canvas, text, line, blink_on));
        let mut frame = OverlayFrame::default();
        canvas.frame_into(&mut frame);
        let width = frame.width as usize;
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the slot's rows"
        )]
        let rows = (SLOT.1 + 52.0) as usize..(SLOT.1 + 52.0 + 22.0) as usize;
        (0..width)
            .map(|x| {
                rows.clone()
                    .map(|y| frame.bgra[(y * width + x) * 4 + 3])
                    .max()
                    .unwrap_or(0)
            })
            .collect()
    }

    fn words(committed: &str, caret: bool) -> Line {
        Line::Words {
            committed: committed.to_owned(),
            partial: String::new(),
            caret,
        }
    }

    #[test]
    fn the_ticker_starts_a_short_line_at_the_left_and_keeps_a_long_lines_newest_words_in_view() {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the slot's columns"
        )]
        let (left, right) = ((SLOT.0 + 24.0) as usize, (SLOT.0 + SLOT_W - 24.0) as usize);
        let inked = |ink: &[u8]| {
            let columns: Vec<usize> = (0..ink.len()).filter(|x| ink[*x] > 0).collect();
            (columns[0], columns[columns.len() - 1])
        };
        let short = ink(&words("hello", false), true);
        let (first, last) = inked(&short);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a width in pixels"
        )]
        let used = with_text(|text| text.measure(&[("hello", INK)], TICKER, Layout::Line, 1.0))
            .0
            .ceil() as usize;
        assert!(
            (left..=left + 5).contains(&first) && last <= left + used + 1,
            "{first}..{last} for {used}"
        );
        let long = ink(
            &words("the newest words stay in view ".repeat(6).trim_end(), false),
            true,
        );
        let (first, last) = inked(&long);
        assert!(
            first >= left && last + 3 <= right && last + 12 >= right,
            "the newest word ends just short of the right edge: {first}..{last}"
        );
        let strongest =
            |range: std::ops::Range<usize>| long[range].iter().copied().max().unwrap_or(0);
        assert!(
            strongest(left..left + 6) < strongest(left + 100..left + 160) / 2,
            "the cut-off start fades"
        );
        assert_ne!(
            ink(&words("hello", true), true),
            ink(&words("hello", true), false),
            "the caret blinks"
        );
        assert_eq!(
            ink(&words("hello", false), true),
            short,
            "no caret without one"
        );
        let notice = ink(
            &Line::Notice("a notice long enough to run past the end ".repeat(3)),
            true,
        );
        let shown = notice[left + 80..left + 160]
            .iter()
            .copied()
            .max()
            .unwrap_or(0);
        let end = notice[right - 8..right].iter().copied().max().unwrap_or(0);
        assert!(
            shown > 200 && end < shown / 2,
            "a long notice shows and fades at its end: {shown} {end}"
        );
        let fits = with_text(|text| {
            let mut fits = "a".to_owned();
            while text
                .measure(&[(fits.as_str(), INK)], TICKER, Layout::Line, 1.0)
                .0
                < SLOT_W - 48.0 - 20.0
            {
                fits.push_str(" a");
            }
            fits
        });
        let fitting = ink(&Line::Notice(fits), true);
        let (_, end) = inked(&fitting);
        assert!(
            fitting[end - 6..=end].iter().any(|alpha| *alpha >= 200),
            "a notice that fits keeps its end"
        );
        assert!(ink(&Line::None, true).iter().all(|alpha| *alpha == 0));
    }

    /// The pixels `draw` puts on a blank canvas, as premultiplied BGRA.
    fn painted(draw: impl FnOnce(&mut Canvas, &mut Text)) -> Vec<[u8; 4]> {
        let mut canvas = Canvas::new(WIDTH, HEIGHT, 1.0).unwrap();
        with_text(|text| draw(&mut canvas, text));
        let mut frame = OverlayFrame::default();
        canvas.frame_into(&mut frame);
        frame.bgra.as_chunks::<4>().0.to_vec()
    }

    /// The alpha-weighted centre of the pixels where `a` and `b` differ, and their row span.
    fn changed(a: &[[u8; 4]], b: &[[u8; 4]]) -> ((f32, f32), (usize, usize)) {
        let width = COLUMNS;
        let (mut sum, mut x, mut y, mut rows) = (0.0, 0.0, 0.0, (usize::MAX, 0));
        for (i, (p, q)) in a.iter().zip(b).enumerate() {
            let weight = f32::from(
                p.iter()
                    .zip(q)
                    .map(|(p, q)| p.abs_diff(*q))
                    .max()
                    .unwrap_or(0),
            );
            if weight > 0.0 {
                #[expect(clippy::cast_precision_loss, reason = "pixel positions")]
                let (px, py) = ((i % width) as f32, (i / width) as f32);
                (sum, x, y) = (sum + weight, x + px * weight, y + py * weight);
                rows = (rows.0.min(i / width), rows.1.max(i / width));
            }
        }
        ((x / sum, y / sum), rows)
    }

    fn drawn(finish: Finish, view: &View) -> Vec<[u8; 4]> {
        let mut module = Module::new(finish, 1.0).unwrap();
        painted(|canvas, text| module.draw(canvas, text, view, view.clock))
    }

    fn shown(status: Status, draft: bool, clock: f64, level: f32) -> View {
        let mut view = View::new(false);
        view.set_draft(draft);
        view.set_status(status, 0.0);
        for frame in 0..=12 {
            view.poll(clock * f64::from(frame) / 12.0, level);
        }
        view
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
    fn only_holo_shines_as_time_passes() {
        let done = Status::Done {
            target: None,
            route: CaptureRoute::Paste,
            notice: "Delivered.".to_owned(),
        };
        for (finish, shines) in [(Finish::Gunmetal, false), (Finish::Holo, true)] {
            let mut view = shown(done.clone(), false, 0.0, 0.0);
            let still = drawn(finish, &view);
            view.clock = 5.5;
            assert_eq!(drawn(finish, &view) != still, shines, "{finish:?}");
        }
    }

    #[test]
    fn the_wave_sits_centred_in_the_slot_rising_above_a_line_of_text_and_quiet_coloured_while_waiting()
     {
        let quiet = Status::Pending {
            target: None,
            route: CaptureRoute::Paste,
            text: String::new(),
            waiting_for_quiet: true,
            pause_ms: 2000,
            copies: false,
        };
        let centre = |status: &Status, draft| {
            let loud = drawn(Finish::Gunmetal, &shown(status.clone(), draft, 1.0, 1.0));
            let silent = drawn(Finish::Gunmetal, &shown(status.clone(), draft, 1.0, 0.0));
            changed(&loud, &silent).0
        };
        let (bare, worded) = (centre(&capturing(), false), centre(&capturing(), true));
        assert!(
            (bare.0 - (SLOT.0 + 5.0 + WAVE_W / 2.0)).abs() <= 2.0,
            "{bare:?}"
        );
        assert!(
            (bare.1 - worded.1 - 15.0).abs() <= 1.5,
            "{bare:?} {worded:?}"
        );
        let cyan = |status: &Status| {
            drawn(Finish::Gunmetal, &shown(status.clone(), false, 1.0, 1.0))
                .iter()
                .filter(|[b, g, r, _]| {
                    r.abs_diff(107) < 30 && g.abs_diff(248) < 30 && b.abs_diff(255) < 30
                })
                .count()
        };
        assert!(
            cyan(&capturing()) > 50 && cyan(&quiet) == 0,
            "{} {}",
            cyan(&capturing()),
            cyan(&quiet)
        );
    }

    #[test]
    fn the_caret_blinks_once_a_second() {
        let ticker = |clock: f64| {
            let mut view = shown(capturing(), true, 0.0, 0.0);
            view.clock = clock;
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "the ticker's rows"
            )]
            let rows = (SLOT.1 + 52.0) as usize * COLUMNS..(SLOT.1 + 74.0) as usize * COLUMNS;
            drawn(Finish::Gunmetal, &view)[rows].to_vec()
        };
        let cyan = |pixels: &[[u8; 4]]| {
            pixels
                .iter()
                .filter(|[b, g, r, _]| *r < 140 && *g > 200 && *b > 200)
                .count()
        };
        let (on, off) = (ticker(0.2), ticker(0.7));
        assert!(cyan(&on) > cyan(&off), "on at the start of each second");
        assert_eq!(ticker(1.2), on);
    }

    #[test]
    fn the_caret_sits_just_after_the_last_word() {
        let used = with_text(|text| text.measure(&[("hello", INK)], TICKER, Layout::Line, 1.0)).0;
        let on = painted(|canvas, text| ticker(canvas, text, &words("hello", true), true));
        let off = painted(|canvas, text| ticker(canvas, text, &words("hello", true), false));
        let ((x, _), (top, _)) = changed(&on, &off);
        assert!(
            (x - (SLOT.0 + 24.0 + used + 2.0)).abs() <= 1.0,
            "{x} after {used}"
        );
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a row"
        )]
        let row = (SLOT.1 + 52.0 + 4.0) as usize;
        assert_eq!(top, row);
    }

    #[test]
    fn a_confirmation_is_centred_in_the_slot_and_wraps_inside_it() {
        let done = Status::Done {
            target: Some("a-window-with-a-rather-long-name".to_owned()),
            route: CaptureRoute::Submit,
            notice: "Delivered, and the answer is on its way.".to_owned(),
        };
        let view = shown(done, false, 0.0, 0.0);
        let width = COLUMNS;
        let green: Vec<(usize, usize)> = drawn(Finish::Gunmetal, &view)
            .iter()
            .enumerate()
            .filter(|(i, [b, g, r, _])| {
                #[expect(clippy::cast_precision_loss, reason = "a row")]
                let in_slot = ((i / width) as f32) < SLOT.1 + SLOT_H;
                in_slot && *r < 120 && *g > 200 && *b > 100 && *b < 180
            })
            .map(|(i, _)| (i % width, i / width))
            .collect();
        #[expect(clippy::cast_precision_loss, reason = "pixel positions")]
        let span = |pick: fn(&(usize, usize)) -> usize| {
            let values = green.iter().map(pick);
            (
                values.clone().min().unwrap() as f32,
                values.max().unwrap() as f32,
            )
        };
        let ((left, right), (top, bottom)) = (span(|p| p.0), span(|p| p.1));
        assert!(
            left >= SLOT.0 + 8.0 && right <= SLOT.0 + SLOT_W - 8.0,
            "{left}..{right}"
        );
        assert!(
            (left.midpoint(right) - (SLOT.0 + SLOT_W / 2.0)).abs() <= 4.0,
            "{left}..{right}"
        );
        assert!(
            (top.midpoint(bottom) - (SLOT.1 + SLOT_H / 2.0)).abs() <= 3.0,
            "{top}..{bottom}"
        );
        let message =
            "✓ Sent to a-window-with-a-rather-long-name. Delivered, and the answer is on its way.";
        let (_, height) = with_text(|text| {
            text.measure(
                &[(message, INK)],
                CONFIRMATION,
                Layout::Centred {
                    width: SLOT_W - 16.0,
                },
                1.0,
            )
        });
        assert!(
            height >= 2.0 * CONFIRMATION.line_height
                && bottom - top > height - CONFIRMATION.line_height
                && bottom - top < height + 2.0,
            "wrapped to {height} points, inked {top}..{bottom}"
        );
    }

    #[test]
    fn the_countdown_tag_is_centred_on_the_wave_between_two_lines() {
        let top = SLOT.1 + 2.0;
        let amber = Rgba::rgb(255, 194, 75);
        let tagged = painted(|canvas, text| tag(canvas, text, "PASTE IN 3 s", amber, top));
        let width = COLUMNS;
        let inked: Vec<(usize, usize)> = tagged
            .iter()
            .enumerate()
            .filter(|(_, p)| p[3] > 60)
            .map(|(i, _)| (i % width, i / width))
            .collect();
        let (left, right) = (
            inked.iter().map(|p| p.0).min().unwrap(),
            inked.iter().map(|p| p.0).max().unwrap(),
        );
        #[expect(clippy::cast_precision_loss, reason = "pixel columns")]
        let middle_x = (left + right) as f32 / 2.0;
        assert!(
            (middle_x - (SLOT.0 + 5.0 + WAVE_W / 2.0)).abs() <= 1.5,
            "{left}..{right}"
        );
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a row"
        )]
        let middle = (top + WAVE_H / 2.0) as usize;
        let at = |x: usize, y: usize| tagged[y * width + x][3];
        for x in [left + 20, right - 20] {
            assert!(
                at(x, middle) > 60 && at(x, middle - 3) == 0 && at(x, middle + 3) == 0,
                "a line at {x}"
            );
        }
        let words: Vec<usize> = inked
            .iter()
            .filter(|p| p.0.abs_diff(left + (right - left) / 2) < 30)
            .map(|p| p.1)
            .collect();
        #[expect(clippy::cast_precision_loss, reason = "pixel rows")]
        let words_middle = (words.iter().min().unwrap() + words.iter().max().unwrap()) as f32 / 2.0;
        #[expect(clippy::cast_precision_loss, reason = "a row")]
        let middle = middle as f32;
        assert!(
            (words_middle - middle).abs() <= 3.0,
            "{words_middle} on {middle}"
        );
    }

    #[test]
    fn the_cancel_glyph_is_centred_in_its_button() {
        for finish in [Finish::Gunmetal, Finish::Holo] {
            let face = painted(|canvas, _| {
                button_face(canvas, finish);
            });
            let button = painted(|canvas, text| cancel_button(canvas, text, finish));
            let ((x, _), _) = changed(&button, &face);
            assert!(
                (x - (CANCEL.0 + CANCEL.2 / 2.0)).abs() <= 1.0,
                "{finish:?}: {x}"
            );
        }
    }
}
