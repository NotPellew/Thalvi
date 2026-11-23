//! The gunmetal and holo looks: a 370 × 84 module hanging from the top edge, a mic wave over one
//! line of text, and a glowing line along its bottom.

use std::f32::consts::{PI, TAU};
use std::time::Duration;

use thalvi_desktop::theme::overlay::{
    CONFIRMATION, MODULE_CANCEL, Module as Theme, TAG, TICKER, TRANSPARENT, WAVE_H, WAVE_W, WHITE,
    module as art,
};

use super::paint::{Canvas, Fill, Layout, Rgba, Text, TextBounds};
use super::view::{
    CANCEL, FRAME, HEIGHT, Line, MODULE_H, MODULE_W, MODULE_X, SLOT, SLOT_H, SLOT_W, Tone, View,
    WIDTH, done_message, spaced,
};
use crate::status::Status;

/// Which of the two module looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Finish {
    /// A thin hex-mesh metal bezel around a dark glass slot.
    Gunmetal,
    /// A steel-blue frame line, a teal scanline field over a hex mesh, glowing corner pieces.
    Holo,
}

fn color(theme: &Theme, tone: Tone) -> Rgba {
    match tone {
        Tone::Live => theme.state.live,
        Tone::Working => theme.state.working,
        Tone::Quiet => theme.state.quiet,
        Tone::Pending => theme.state.pending,
        Tone::Done => theme.state.done,
        Tone::Failed => theme.state.failed,
    }
}

/// A module look's still parts, drawn once per scale.
pub(super) struct Module {
    finish: Finish,
    background: Canvas,
    theme: Theme,
    glows: Vec<(Tone, Canvas)>,
}

impl Module {
    pub(super) fn new(finish: Finish, theme: Theme, scale: f32) -> Option<Self> {
        Some(Self {
            finish,
            background: background(finish, &theme, scale)?,
            theme,
            glows: Vec::new(),
        })
    }

    /// How long until the look itself moves again while the view is still; holo's shine.
    pub(super) fn motion(&self, clock: f64) -> Option<Duration> {
        if !self.theme.shine {
            return None;
        }
        let at = clock % art::SHINE_CYCLE;
        Some(if at >= art::SHINE_FROM {
            FRAME
        } else {
            Duration::from_secs_f64(art::SHINE_FROM - at)
        })
    }

    pub(super) fn draw(&mut self, canvas: &mut Canvas, text: &mut Text, view: &View, clock: f64) {
        canvas.copy(&self.background);
        let theme = &self.theme;
        if theme.shine {
            shine(canvas, theme, clock);
        }
        canvas.clip(Some((SLOT.0, SLOT.1, SLOT_W, SLOT_H)));
        let tone_color = color(theme, view.tone);
        let has_text = view.line != Line::None;
        let wave_top = SLOT.1
            + if has_text {
                art::WAVE_WITH_TEXT
            } else {
                art::WAVE_ALONE
            } * theme.measures.spacing;
        #[expect(clippy::cast_possible_truncation, reason = "a clock in seconds")]
        let seconds = view.clock as f32;
        if let Some(wave) = &view.wave {
            if let Some(glow) = glow(
                &mut self.glows,
                view.tone,
                tone_color,
                theme.measures.glow_ratio,
                canvas.scale_of(),
            ) {
                halo(canvas, glow, wave_top, wave.smoothed);
            }
            let strands = if view.tone == Tone::Quiet {
                theme.state.quiet
            } else {
                theme.state.live
            };
            draw_wave(
                canvas,
                view,
                strands,
                seconds,
                (SLOT.0 + art::WAVE_LEFT, wave_top),
                theme.measures.pulse,
            );
        }
        if let Some(label) = &view.label {
            tag(canvas, text, label, tone_color, wave_top, theme);
        }
        if let Status::Done {
            target,
            route,
            notice,
        } = &view.status
        {
            let message = done_message(*route, target.as_deref(), notice);
            let style = theme.measures.text(CONFIRMATION);
            let inset = art::CONFIRMATION_LEFT * theme.measures.spacing;
            let layout = Layout::Centred {
                width: SLOT_W - 2.0 * inset,
            };
            let spans = [(message.as_str(), tone_color)];
            let (_, height) = text.measure(&spans, style, layout, canvas.scale_of());
            text.draw(
                canvas,
                &spans,
                style,
                layout,
                (SLOT.0 + inset, SLOT.1 + (SLOT_H - height) / 2.0),
            );
        } else if has_text {
            rule(canvas, theme);
            ticker(canvas, text, &view.line, view.caret_on(), theme);
        }
        if view.cancellable() {
            cancel_button(canvas, text, self.finish, theme);
        }
        canvas.clip(None);
        line_strip(canvas, view, tone_color, self.finish, theme);
    }
}

/// The tone's glow behind the wave, as strong as the voice is loud.
fn halo(canvas: &mut Canvas, glow: &Canvas, wave_top: f32, loudness: f32) {
    canvas.draw(
        glow,
        (
            SLOT.0 + art::WAVE_LEFT + (WAVE_W - art::WAVE_HALO.0) / 2.0 - art::GLOW_PAD,
            wave_top + art::HALO_OFFSET - art::GLOW_PAD,
        ),
        (loudness * art::HALO_GAIN).min(1.0),
    );
}

/// The faint rule above the ticker.
fn rule(canvas: &mut Canvas, theme: &Theme) {
    let stops = [
        (0.0, TRANSPARENT),
        (0.5, theme.muted.faded(art::RULE_ALPHA)),
        (1.0, TRANSPARENT),
    ];
    let inset = art::TICKER_LEFT * theme.measures.spacing;
    let (left, right) = (SLOT.0 + inset, SLOT.0 + SLOT_W - inset);
    canvas.rect(
        (
            left,
            SLOT.1 + art::RULE_TOP.min(ticker_top(theme) - art::RULE_WIDTH),
            right - left,
            art::RULE_WIDTH,
        ),
        0.0,
        &Fill::Linear {
            from: (left, 0.0),
            to: (right, 0.0),
            stops: &stops,
        },
    );
}

/// The module's shadow and frame, with the finish's own panel, bezel and slot.
fn background(finish: Finish, theme: &Theme, scale: f32) -> Option<Canvas> {
    let mut canvas = Canvas::new(WIDTH, HEIGHT, scale)?;
    let mut shadow = Canvas::new(WIDTH, HEIGHT, scale)?;
    let shoulder = art::SHOULDER + theme.measures.chamfer_delta;
    let slant = art::SLANT + theme.measures.chamfer_delta;
    let radius = if theme.measures.radius_delta == 0.0 {
        0.0
    } else {
        theme.measures.radius
    };
    shadow.rounded_polygon(
        &trapezoid([MODULE_X, art::SHADOW_OFFSET, MODULE_W, MODULE_H], shoulder),
        radius,
        &Fill::Solid(theme.shadow),
    );
    shadow.blur(art::SHADOW_BLUR);
    canvas.draw(&shadow, (0.0, 0.0), 1.0);
    let outer = [MODULE_X, 0.0, MODULE_W, MODULE_H];
    let width = match finish {
        Finish::Gunmetal => art::FRAME_EDGE,
        Finish::Holo => theme.measures.border,
    };
    let panel = panel(width)?;
    match finish {
        Finish::Gunmetal => {
            shade(&mut canvas, outer, shoulder, radius, &theme.metal);
            shade(&mut canvas, panel, shoulder, radius, &theme.face);
            hexes(&mut canvas, panel, shoulder, theme.mesh);
            let bezel = art::FRAME_EDGE + theme.measures.border_delta * art::BEZEL_SHARE;
            shade(
                &mut canvas,
                [SLOT.0 - bezel, SLOT.1, SLOT_W + 2.0 * bezel, SLOT_H + bezel],
                slant,
                radius,
                &theme.bezel,
            );
            shade(
                &mut canvas,
                [SLOT.0, SLOT.1, SLOT_W, SLOT_H],
                slant,
                radius,
                &theme.glass,
            );
            let inset = [(0.0, art::SLOT_SHADE), (1.0, TRANSPARENT)];
            canvas.clip(Some((SLOT.0, SLOT.1, SLOT_W, SLOT_H)));
            canvas.rect(
                (SLOT.0, SLOT.1, SLOT_W, art::INSET_SHADE_HEIGHT),
                0.0,
                &Fill::Linear {
                    from: (0.0, SLOT.1),
                    to: (0.0, SLOT.1 + art::INSET_SHADE_HEIGHT),
                    stops: &inset,
                },
            );
            canvas.clip(None);
        }
        Finish::Holo => {
            if width > 0.0 {
                canvas.rounded_polygon(
                    &trapezoid(outer, shoulder),
                    radius,
                    &Fill::Solid(theme.rim),
                );
            }
            shade(&mut canvas, panel, shoulder, radius, &theme.field);
            hexes(&mut canvas, panel, shoulder, theme.mesh);
            canvas.clip_to(&trapezoid(panel, shoulder));
            if theme.scanlines {
                let (stroke, step, opacity) = art::SCANLINE;
                let mut y = panel[1];
                while y < panel[1] + panel[3] {
                    canvas.rect(
                        (panel[0], y, panel[2], stroke),
                        0.0,
                        &Fill::Solid(theme.accent.faded(opacity)),
                    );
                    y += step;
                }
            }
            let (stroke, blur, opacity) = art::VIGNETTE;
            let mut vignette = Canvas::new(WIDTH, HEIGHT, scale)?;
            vignette.stroke(
                &closed(&trapezoid(panel, shoulder)),
                stroke,
                &Fill::Solid(theme.rim.faded(opacity)),
            );
            vignette.blur(blur);
            canvas.draw(&vignette, (0.0, 0.0), 1.0);
            canvas.clip(None);
            if theme.corners {
                accents(&mut canvas, scale, shoulder, theme)?;
            }
        }
    }
    Some(canvas)
}

/// Holo's corner pieces and slanted end caps, glowing.
fn accents(canvas: &mut Canvas, scale: f32, shoulder: f32, theme: &Theme) -> Option<()> {
    let reach = art::BRACKET_REACH;
    let lean = (shoulder / MODULE_H).atan();
    let (dx, dy) = (lean.sin() * reach, lean.cos() * reach);
    let mut pieces: Vec<Vec<(f32, f32)>> = Vec::new();
    for side in [1.0f32, -1.0] {
        let edge = if side > 0.0 {
            shoulder
        } else {
            MODULE_W - shoulder
        };
        pieces.push(vec![
            (edge + side * dx, MODULE_H - dy),
            (edge, MODULE_H),
            (edge + side * reach, MODULE_H),
        ]);
        let x = |share: f32| {
            if side > 0.0 {
                shoulder * share + art::BRACKET_OFFSET
            } else {
                MODULE_W - shoulder * share - art::BRACKET_OFFSET
            }
        };
        pieces.push(vec![
            (x(art::BRACKET_SLOPE.0), MODULE_H * art::BRACKET_SPAN.0),
            (x(art::BRACKET_SLOPE.1), MODULE_H * art::BRACKET_SPAN.1),
        ]);
    }
    let pieces: Vec<Vec<(f32, f32)>> = pieces
        .into_iter()
        .map(|piece| piece.into_iter().map(|(x, y)| (MODULE_X + x, y)).collect())
        .collect();
    let mut glow = Canvas::new(WIDTH, HEIGHT, scale)?;
    for piece in &pieces {
        glow.stroke(
            piece,
            art::BRACKET_WIDTH,
            &Fill::Solid(
                theme
                    .accent
                    .faded(art::BRACKET_ALPHA * theme.measures.glow_ratio.max(1.0)),
            ),
        );
    }
    glow.blur(art::BRACKET_BLUR);
    canvas.draw(&glow, (0.0, 0.0), theme.measures.glow_ratio.min(1.0));
    for piece in &pieces {
        canvas.stroke(piece, art::BRACKET_WIDTH, &Fill::Solid(theme.accent));
    }
    Some(())
}

/// Holo's sheen crossing the panel once every few seconds.
fn shine(canvas: &mut Canvas, theme: &Theme, clock: f64) {
    let at = clock % art::SHINE_CYCLE;
    if at < art::SHINE_FROM {
        return;
    }
    #[expect(clippy::cast_possible_truncation, reason = "a share of one cycle")]
    let travel = ((at - art::SHINE_FROM) / (art::SHINE_CYCLE - art::SHINE_FROM)) as f32;
    let Some(panel) = panel(theme.measures.border) else {
        return;
    };
    let (offscreen, distance, width) = art::SHINE_RANGE;
    let x = panel[0] - offscreen + distance * travel;
    let stops = [
        (0.0, TRANSPARENT),
        (0.5, art::SHINE_COLOR),
        (1.0, TRANSPARENT),
    ];
    canvas.clip_to(&trapezoid(
        panel,
        art::SHOULDER + theme.measures.chamfer_delta,
    ));
    canvas.rect(
        (x, panel[1], width, panel[3]),
        0.0,
        &Fill::Linear {
            from: (x, 0.0),
            to: (x + width, 0.0),
            stops: &stops,
        },
    );
    canvas.clip(None);
}

fn panel(border: f32) -> Option<[f32; 4]> {
    let height = MODULE_H - border;
    // The mesh and scanline loops walk the panel, so it must be a height the module holds.
    (0.0..=MODULE_H).contains(&height).then_some([
        MODULE_X + border,
        0.0,
        MODULE_W - 2.0 * border,
        height,
    ])
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
fn shade(canvas: &mut Canvas, rect: [f32; 4], inset: f32, radius: f32, stops: &[(f32, Rgba)]) {
    canvas.rounded_polygon(
        &trapezoid(rect, inset),
        radius,
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
    let (cell_w, cell_h) = art::HEX_SIZE;
    let mut top = y;
    while top < y + height {
        let mut left = x;
        while left < x + width {
            let hex = art::HEX_CORNERS.map(|(x, y)| (left + x, top + y));
            for i in 0..hex.len() {
                let (a, b) = (hex[i], hex[(i + 1) % hex.len()]);
                if inside(a) && inside(b) {
                    canvas.stroke(&[a, b], art::HEX_STROKE, &line);
                }
            }
            let (a, b) = (hex[3], (hex[3].0, top + cell_h));
            if inside(a) && inside(b) {
                canvas.stroke(&[a, b], art::HEX_STROKE, &line);
            }
            left += cell_w;
        }
        top += cell_h;
    }
}

/// The soft glow behind the wave in `tone`'s colour, drawn once per tone.
fn glow(
    glows: &mut Vec<(Tone, Canvas)>,
    tone: Tone,
    color: Rgba,
    strength: f32,
    scale: f32,
) -> Option<&Canvas> {
    if !glows.iter().any(|(drawn, _)| *drawn == tone) {
        let (w, h) = art::WAVE_HALO;
        let mut canvas = Canvas::new(w + 2.0 * art::GLOW_PAD, h + 2.0 * art::GLOW_PAD, scale)?;
        canvas.rect(
            (art::GLOW_PAD, art::GLOW_PAD, w, h),
            h / 2.0,
            &Fill::Solid(color.faded(art::HALO_ALPHA * strength)),
        );
        canvas.blur(art::HALO_BLUR);
        glows.push((tone, canvas));
    }
    glows
        .iter()
        .find(|(drawn, _)| *drawn == tone)
        .map(|(_, canvas)| canvas)
}

/// Nine twisting strands, faded at both ends, with their top-left at `origin`.
fn draw_wave(
    canvas: &mut Canvas,
    view: &View,
    color: Rgba,
    seconds: f32,
    origin: (f32, f32),
    pulse: bool,
) {
    let Some(wave) = &view.wave else {
        return;
    };
    let (seconds, phase) = if pulse {
        (seconds, wave.phase)
    } else {
        (0.0, 0.0)
    };
    let [first, second, third, fourth] = art::STRAND_FREQUENCIES;
    let strand_a = |x: f32, p: f32| {
        (TAU * first.0 * x + p).sin() * first.2
            + (TAU * second.0 * x - p * second.1 + 1.0).sin() * second.2
    };
    let strand_b = |x: f32, p: f32| {
        (TAU * third.0 * x - p * third.1 + 2.0).sin() * third.2
            + (TAU * fourth.0 * x + p * fourth.1).sin() * fourth.2
    };
    let middle = origin.1 + WAVE_H / 2.0;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "332 / 4"
    )]
    let count = (WAVE_W / art::STRAND_STEP) as usize;
    for (width, alpha) in art::STRAND_LAYERS {
        for strand in 0..=2 * art::STRAND_SIDES {
            let side = (f32::from(strand) / f32::from(2 * art::STRAND_SIDES)) * 2.0 - 1.0;
            let (twist_sin, twist_cos) = (side * art::STRAND_TWIST).sin_cos();
            let strength =
                alpha * (art::STRAND_EDGE + (1.0 - art::STRAND_EDGE) * (1.0 - side.abs()));
            #[expect(clippy::cast_precision_loss, reason = "a few hundred points")]
            let points: Vec<(f32, f32)> = (0..=count)
                .map(|i| {
                    let px = i as f32 * art::STRAND_STEP;
                    let x = px / WAVE_W;
                    let offset = strand_a(x, phase) * twist_cos + strand_b(x, phase) * twist_sin;
                    (origin.0 + px, middle + wave.amplitude(x, seconds) * offset)
                })
                .collect();
            let stops =
                art::STRAND_FALLOFF.map(|(at, opacity)| (at, color.faded(opacity * strength)));
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
fn tag(canvas: &mut Canvas, text: &mut Text, label: &str, color: Rgba, top: f32, theme: &Theme) {
    let style = theme.measures.label(TAG);
    let line = art::TAG_LINE * theme.measures.spacing;
    let gap = art::TAG_GAP * theme.measures.spacing;
    let spans = [(label, color)];
    let (width, _) = text.measure(&spans, style, Layout::Line, canvas.scale_of());
    let total = line + gap + width + gap + line;
    let left = SLOT.0 + art::WAVE_LEFT + (WAVE_W - total) / 2.0;
    let middle = top + WAVE_H / 2.0;
    for (x, from, to) in [
        (left, TRANSPARENT, color),
        (left + line + gap + width + gap, color, TRANSPARENT),
    ] {
        let stops = [(0.0, from), (1.0, to)];
        canvas.rect(
            (x, middle - art::TAG_STROKE / 2.0, line, art::TAG_STROKE),
            art::TAG_RADIUS,
            &Fill::Linear {
                from: (x, 0.0),
                to: (x + line, 0.0),
                stops: &stops,
            },
        );
    }
    text.draw(
        canvas,
        &spans,
        style,
        Layout::Line,
        (left + line + gap, middle - style.line_height / 2.0),
    );
}

/// One line that never wraps. Words that overflow it keep their newest end in view and fade out
/// at the start; a notice that overflows fades out at its end.
fn ticker_top(theme: &Theme) -> f32 {
    (art::TICKER_TOP + (theme.measures.spacing - 1.0) * art::TICKER_TRAVEL)
        .min(SLOT_H - theme.measures.text(TICKER).line_height - art::TICKER_BOTTOM)
}

fn ticker(canvas: &mut Canvas, text: &mut Text, line: &Line, blink_on: bool, theme: &Theme) {
    let style = theme.measures.text(TICKER);
    let inset = art::TICKER_LEFT * theme.measures.spacing;
    let (left, top, width) = (
        SLOT.0 + inset,
        SLOT.1 + ticker_top(theme),
        SLOT_W - 2.0 * inset,
    );
    let right = left + width;
    let scale = canvas.scale_of();
    match line {
        Line::None => {}
        Line::Notice(notice) => {
            let spans = [(notice.as_str(), theme.text)];
            let (used, _) = text.measure(&spans, style, Layout::Line, scale);
            canvas.set_text_bounds(Some(TextBounds {
                left,
                right,
                fade_left: 0.0,
                fade_right: if used > width { art::TICKER_FADE } else { 0.0 },
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
            let spans = [
                (committed.as_str(), theme.text),
                (partial.as_str(), theme.muted),
            ];
            let (used, _) = text.measure(&spans, style, Layout::Line, scale);
            // A line too long to fit ends 3 points short of the right edge, leaving room for the caret.
            let start = (right - art::CARET_INSETS.0 - art::CARET_WIDTH - used).min(left);
            let long = start < left;
            canvas.set_text_bounds(Some(TextBounds {
                left,
                right,
                fade_left: if long { art::TICKER_FADE } else { 0.0 },
                fade_right: 0.0,
            }));
            text.draw(canvas, &spans, style, Layout::Line, (start, top));
            canvas.set_text_bounds(None);
            if *caret && blink_on {
                canvas.rect(
                    (
                        start + used + art::CARET_INSETS.0,
                        top + art::CARET_INSETS.1,
                        art::CARET_WIDTH,
                        style.size,
                    ),
                    0.0,
                    &Fill::Solid(theme.accent),
                );
            }
        }
    }
}

/// The button's face in `finish`; the colour its × is drawn in.
fn button_face(canvas: &mut Canvas, finish: Finish, theme: &Theme) {
    let (_, y, _, height) = CANCEL;
    let radius = (art::BUTTON_RADIUS + theme.measures.radius_delta).max(0.0);
    let fill = match finish {
        Finish::Gunmetal => Fill::Linear {
            from: (0.0, y),
            to: (0.0, y + height),
            stops: &theme.button,
        },
        Finish::Holo => Fill::Solid(art::BUTTON_GLASS),
    };
    canvas.rect(CANCEL, radius, &fill);
    canvas.outline(
        CANCEL,
        radius,
        theme.button_outline,
        (art::BUTTON_RIM.0 + theme.measures.border_delta).clamp(0.0, art::BUTTON_RIM.1),
    );
}

fn cancel_button(canvas: &mut Canvas, text: &mut Text, finish: Finish, theme: &Theme) {
    let (x, y, width, height) = CANCEL;
    button_face(canvas, finish, theme);
    let glyph = theme.button_glyph;
    let style = theme.measures.text(MODULE_CANCEL);
    let spans = [("×", glyph)];
    let (used, _) = text.measure(&spans, style, Layout::Line, canvas.scale_of());
    text.draw(
        canvas,
        &spans,
        style,
        Layout::Line,
        (
            x + (width - used) / 2.0,
            y + (height - style.line_height) / 2.0,
        ),
    );
}

/// The glowing line along the module's bottom edge: full while listening, shrinking towards its
/// middle through a countdown, a sweep while working.
fn line_strip(canvas: &mut Canvas, view: &View, color: Rgba, finish: Finish, theme: &Theme) {
    let inset = match finish {
        Finish::Gunmetal => art::SHOULDER + art::STRIP_INSETS.0,
        Finish::Holo => art::SHOULDER + art::STRIP_INSETS.1,
    };
    let inset = inset + theme.measures.chamfer_delta;
    let full = MODULE_W - 2.0 * inset;
    let left = MODULE_X + inset;
    let y = MODULE_H - art::STRIP_WIDTH;
    if view.sweep {
        #[expect(clippy::cast_possible_truncation, reason = "a phase in 0..2")]
        let phase = if theme.measures.pulse {
            ((view.clock / art::SWEEP.0) % 2.0) as f32
        } else {
            0.5
        };
        let travel = if phase <= 1.0 { phase } else { 2.0 - phase };
        let eased = 0.5 - 0.5 * (PI * travel).cos();
        let width = full * art::SWEEP.1;
        canvas.rect(
            (left, y, full, art::STRIP_WIDTH),
            0.0,
            &Fill::Solid(color.faded(art::SWEEP.2)),
        );
        canvas.clip(Some((
            left,
            y - art::STRIP_RADIUS,
            full,
            2.0 * art::STRIP_WIDTH,
        )));
        let x = left + eased * width * art::SWEEP.3;
        let stops = [(0.0, TRANSPARENT), (0.5, color), (1.0, TRANSPARENT)];
        canvas.rect(
            (x, y, width, art::STRIP_WIDTH),
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
    let glows = art::STRIP_GLOWS.map(|(spread, alpha)| (spread, alpha * theme.measures.glow_ratio));
    for (spread, alpha) in glows.into_iter().chain([(0.0, 1.0)]) {
        let stops = [
            (0.0, TRANSPARENT),
            (art::STRIP_FADE.0, color.faded(alpha)),
            (art::STRIP_FADE.1, color.faded(alpha)),
            (1.0, TRANSPARENT),
        ];
        canvas.rect(
            (x, y - spread, width, art::STRIP_WIDTH + 2.0 * spread),
            (art::STRIP_RADIUS + spread + theme.measures.radius_delta).max(0.0),
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
    use thalvi_desktop::{config::Look, theme::defaults_of};

    fn built_in(finish: Finish) -> Theme {
        let look = match finish {
            Finish::Gunmetal => Look::Gunmetal,
            Finish::Holo => Look::Holo,
        };
        Theme::resolve(&defaults_of(look))
    }

    /// The holo look with `width`'s border, as Settings' border control sets it.
    fn holo_with_border(width: u8) -> Theme {
        let mut style = defaults_of(Look::Holo);
        style.dark.border_width = width;
        Theme::resolve(&style)
    }

    fn ticker(canvas: &mut Canvas, text: &mut Text, line: &Line, blink_on: bool) {
        super::ticker(canvas, text, line, blink_on, &built_in(Finish::Gunmetal));
    }

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
        let (left, right) = edges();
        let short = ink(&words("hello", false), true);
        let (first, last) = inked(&short);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a width in pixels"
        )]
        let used = with_text(|text| {
            text.measure(
                &[("hello", built_in(Finish::Gunmetal).text)],
                TICKER,
                Layout::Line,
                1.0,
            )
        })
        .0
        .ceil() as usize;
        assert!(
            (left..=left + 5).contains(&first) && last <= left + used + 1,
            "{first}..{last} for {used}"
        );
        // A line well over a third of the width that still fits, whatever the font.
        let medium = with_text(|text| {
            let mut medium = "hello".to_owned();
            while text
                .measure(
                    &[(medium.as_str(), built_in(Finish::Gunmetal).text)],
                    TICKER,
                    Layout::Line,
                    1.0,
                )
                .0
                < (SLOT_W - 48.0) / 2.0
            {
                medium.push_str(" and");
            }
            medium
        });
        let medium = ink(&words(&medium, false), true);
        let (first, _) = inked(&medium);
        let (short_first, _) = inked(&short);
        let from = first.min(short_first);
        let strongest = |ink: &[u8]| ink[from..from + 10].iter().copied().max().unwrap_or(0);
        assert!(
            first.abs_diff(short_first) <= 4
                && strongest(&short) >= 150
                && u16::from(strongest(&medium)) * 10 >= u16::from(strongest(&short)) * 8,
            "lines that fit start at the left unfaded: {first} {short_first} {}",
            strongest(&short)
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
    }

    /// The ticker's left and right edge, in pixels.
    fn edges() -> (usize, usize) {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the slot's columns"
        )]
        let edges = ((SLOT.0 + 24.0) as usize, (SLOT.0 + SLOT_W - 24.0) as usize);
        edges
    }

    /// The first and last inked column.
    fn inked(ink: &[u8]) -> (usize, usize) {
        let columns: Vec<usize> = (0..ink.len()).filter(|x| ink[*x] > 0).collect();
        (columns[0], columns[columns.len() - 1])
    }

    #[test]
    fn a_notice_fades_at_its_end_only_when_it_runs_past_it() {
        let (left, right) = edges();
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
                .measure(
                    &[(fits.as_str(), built_in(Finish::Gunmetal).text)],
                    TICKER,
                    Layout::Line,
                    1.0,
                )
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
        drawn_with(finish, built_in(finish), view)
    }

    fn drawn_with(finish: Finish, theme: Theme, view: &View) -> Vec<[u8; 4]> {
        let mut module = Module::new(finish, theme, 1.0).expect("a module to draw");
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
    fn a_holo_module_with_no_border_keeps_its_panel_to_the_module_edges() {
        let view = shown(capturing(), false, 0.0, 0.0);
        let bare = drawn_with(Finish::Holo, holo_with_border(0), &view);
        let rimmed = drawn_with(Finish::Holo, holo_with_border(1), &view);
        let (_, rows) = changed(&bare, &rimmed);
        assert_eq!(rows, (0, 83), "no border reaches the module's last row");
        let (bare, rimmed) = (bare[45], rimmed[45]);
        assert!(
            bare[3] > 200 && bare != rimmed,
            "the panel sits where the rim was: {bare:?} against {rimmed:?}"
        );
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
        let used = with_text(|text| {
            text.measure(
                &[("hello", built_in(Finish::Gunmetal).text)],
                TICKER,
                Layout::Line,
                1.0,
            )
        })
        .0;
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
        let long = words(&"the newest words stay in view ".repeat(6), true);
        let on = painted(|canvas, text| ticker(canvas, text, &long, true));
        let off = painted(|canvas, text| ticker(canvas, text, &long, false));
        let ((x, _), _) = changed(&on, &off);
        assert!(
            (x - (SLOT.0 + SLOT_W - 24.0 - 2.0 + 1.0)).abs() <= 1.0,
            "after a long line the caret stays 2 points inside the right edge: {x}"
        );
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
                &[(message, built_in(Finish::Gunmetal).text)],
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
        let tagged = painted(|canvas, text| {
            tag(
                canvas,
                text,
                "PASTE IN 3 s",
                amber,
                top,
                &built_in(Finish::Gunmetal),
            );
        });
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
                button_face(canvas, finish, &built_in(finish));
            });
            let button =
                painted(|canvas, text| cancel_button(canvas, text, finish, &built_in(finish)));
            let ((x, _), _) = changed(&button, &face);
            assert!(
                (x - (CANCEL.0 + CANCEL.2 / 2.0)).abs() <= 1.0,
                "{finish:?}: {x}"
            );
        }
    }
}
