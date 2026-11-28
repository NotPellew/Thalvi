//! Draws a view in the chosen look and theme into frames for the overlay's surface.

use std::sync::Arc;
use std::time::Duration;

use thalvi_desktop::config::Look;
use thalvi_desktop::host::OverlayFrame;
use thalvi_desktop::theme::{Style, overlay};

use super::module::{Finish, Module};
use super::neon::Neon;
use super::paint::{Canvas, Text};
use super::view::{CANCEL, HEIGHT, View, WIDTH};

enum Skin {
    Module(Module),
    Neon(Neon),
}

impl Skin {
    fn new(style: &Style, dark: bool, scale: f32) -> Option<Self> {
        Some(match style.look() {
            Look::Gunmetal => Self::Module(Module::new(
                Finish::Gunmetal,
                overlay::Module::resolve(style),
                scale,
            )?),
            Look::Holo => Self::Module(Module::new(
                Finish::Holo,
                overlay::Module::resolve(style),
                scale,
            )?),
            Look::Neon => Self::Neon(Neon::new(overlay::Neon::resolve(style, dark))),
        })
    }
}

pub(super) struct Painter {
    text: Text,
    canvas: Canvas,
    frame: OverlayFrame,
    scale: f32,
    style: Arc<Style>,
    dark: bool,
    skin: Skin,
    #[cfg(test)]
    restyles: usize,
}

impl Painter {
    /// Reads the system's fonts and draws the look's still parts at `scale` pixels per point.
    pub(super) fn new(scale: f32, style: Arc<Style>, dark: bool) -> Option<Self> {
        let mut text = Text::new();
        let scheme = style.scheme(dark);
        text.set_families(&scheme.font_family, &scheme.mono_family);
        Some(Self {
            text,
            canvas: Canvas::new(WIDTH, HEIGHT, scale)?,
            frame: OverlayFrame::default(),
            scale,
            skin: Skin::new(&style, dark, scale)?,
            style,
            dark,
            #[cfg(test)]
            restyles: 0,
        })
    }

    /// Applies a new active palette on the next frame. An inactive palette can change in place.
    pub(super) fn restyle(&mut self, style: &Arc<Style>, dark: bool) -> bool {
        let scheme = style.scheme(dark);
        if self.style.look() != style.look() || self.style.scheme(self.dark) != scheme {
            let Some(skin) = Skin::new(style, dark, self.scale) else {
                return false;
            };
            self.text
                .set_families(&scheme.font_family, &scheme.mono_family);
            self.skin = skin;
            #[cfg(test)]
            {
                self.restyles += 1;
            }
        }
        self.style = Arc::clone(style);
        self.dark = dark;
        true
    }

    /// Returns the frame and whether the draft fits in the active theme's text area.
    pub(super) fn render(&mut self, view: &View, clock: f64) -> (&OverlayFrame, bool) {
        let complete = self.paint(view, clock);
        self.canvas.frame_into(&mut self.frame);
        (&self.frame, complete)
    }

    #[cfg(test)]
    pub(super) fn draw(&mut self, view: &View, clock: f64) -> &OverlayFrame {
        self.render(view, clock).0
    }

    pub(super) fn rgba(&mut self, view: &View) -> (u32, u32, Vec<u8>) {
        self.paint(view, view.clock);
        self.canvas.rgba()
    }

    fn paint(&mut self, view: &View, clock: f64) -> bool {
        match &mut self.skin {
            Skin::Module(module) => module.draw(&mut self.canvas, &mut self.text, view, clock),
            Skin::Neon(neon) => neon.draw(&mut self.canvas, &mut self.text, view),
        }
    }

    /// How long until the look moves on its own while the view is still.
    pub(super) fn motion(&self, clock: f64) -> Option<Duration> {
        match &self.skin {
            Skin::Module(module) => module.motion(clock),
            Skin::Neon(_) => None,
        }
    }

    /// Whether a press at `(x, y)` points from the overlay's corner hits its cancel button.
    pub(super) fn cancels(&self, view: &View, x: f32, y: f32) -> bool {
        view.cancellable()
            && match &self.skin {
                Skin::Module(_) => {
                    let (left, top, width, height) = CANCEL;
                    (left..left + width).contains(&x) && (top..top + height).contains(&y)
                }
                Skin::Neon(neon) => neon.cancels(x, y),
            }
    }
}

#[cfg(test)]
mod tests {
    use thalvi_desktop::capture::{CaptureRoute, EndingPauses};

    use super::*;

    fn painter(scale: f32, look: Look, dark: bool) -> Painter {
        Painter::new(scale, Arc::new(Style::of(look)), dark).unwrap()
    }
    use crate::status::{FailureKind, Status};

    fn capturing() -> Status {
        Status::Capturing {
            target: Some("kitty".to_owned()),
            committed: "hello".to_owned(),
            partial: None,
            notice: None,
        }
    }

    fn view(status: &Status, takes_clicks: bool) -> View {
        let mut view = View::new(takes_clicks);
        view.set_status(status.clone(), 0.0);
        view
    }

    /// The frame's colour at `(x, y)` points, at scale 1, as RGB.
    fn rgb(frame: &OverlayFrame, (x, y): (f32, f32)) -> [u8; 3] {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "test probes are nonnegative points inside the 460 by 200 scale-1 frame"
        )]
        let at = (y as usize * frame.width as usize + x as usize) * 4;
        [frame.bgra[at + 2], frame.bgra[at + 1], frame.bgra[at]]
    }

    fn near(got: [u8; 3], want: [u8; 3]) -> bool {
        got.iter()
            .zip(want)
            .all(|(got, want)| got.abs_diff(want) < 48)
    }

    fn submit_preview(text: &str) -> Status {
        use crate::status::{Mode, View as StatusView};
        use thalvi_desktop::engine::{EngineState, SubmitPreview};

        StatusView {
            mode: Mode::Idle,
            engine: EngineState::ConfirmingSubmit {
                text: text.to_owned(),
                pause: Duration::from_millis(2500),
                preview: SubmitPreview::default(),
            },
            target: None,
            copies: false,
            pauses: EndingPauses::default(),
            loading: None,
            wake_words: true,
            rebuilding: false,
            offer: None,
        }
        .status()
    }

    #[test]
    fn final_submit_text_is_visible_even_with_the_overlay_and_draft_disabled() {
        use crate::overlay::Visibility;

        for (look, dark) in LOOKS {
            let mut painter = painter(1.0, look, dark);
            let mut preview = |text: &str| {
                let status = submit_preview(text);
                assert_eq!(Visibility::of(&status, false), Visibility::Show);
                let mut view = view(&status, false);
                view.set_draft(false);
                let (frame, complete) = painter.render(&view, 0.0);
                assert!(complete, "{look:?} dark {dark}: {text}");
                frame.bgra.clone()
            };
            assert_ne!(
                preview("Send thirty dollars"),
                preview("Send two dollars"),
                "{look:?}"
            );
            let long = format!("Transfer 9000 dollars {}", "More words. ".repeat(80));
            assert!(!painter.render(&view(&submit_preview(&long), false), 0.0).1);
        }
    }

    #[test]
    fn each_look_shows_each_state_in_its_tone() {
        let pending = Status::Pending {
            target: None,
            route: CaptureRoute::Paste,
            text: "hello".to_owned(),
            waiting_for_quiet: false,
            pause_ms: 2500,
            copies: false,
            final_confirmation: None,
        };
        let done = Status::Done {
            target: None,
            route: CaptureRoute::Paste,
            notice: "Delivered.".to_owned(),
            wake_words: true,
        };
        let failed = Status::Failed {
            kind: FailureKind::Other,
            offer: None,
            notice: String::new(),
            headline: "No words heard".to_owned(),
            causes: Vec::new(),
        };
        // The module looks light the middle of their bottom line, neon the top of its pill.
        let line = (WIDTH / 2.0, 83.0);
        let pill = (WIDTH / 2.0, 26.5);
        let cases = [
            (
                Look::Gunmetal,
                true,
                line,
                [
                    [107, 248, 255],
                    [255, 194, 75],
                    [43, 255, 136],
                    [255, 107, 94],
                ],
            ),
            (
                Look::Holo,
                true,
                line,
                [
                    [107, 248, 255],
                    [255, 194, 75],
                    [43, 255, 136],
                    [255, 107, 94],
                ],
            ),
            (
                Look::Neon,
                true,
                pill,
                [
                    [0x12, 0x31, 0x5e],
                    [0xff, 0xc2, 0x4b],
                    [0x2b, 0xff, 0x88],
                    [0xff, 0x6b, 0x5e],
                ],
            ),
            (
                Look::Neon,
                false,
                pill,
                [
                    [0xb9, 0xcb, 0xdd],
                    [0x8a, 0x5a, 0x00],
                    [0x00, 0x7a, 0x43],
                    [0xb4, 0x23, 0x18],
                ],
            ),
        ];
        for (look, dark, at, tones) in cases {
            let mut painter = painter(1.0, look, dark);
            for (status, want) in [capturing(), pending.clone(), done.clone(), failed.clone()]
                .iter()
                .zip(tones)
            {
                let got = rgb(painter.draw(&view(status, false), 0.0), at);
                assert!(
                    near(got, want),
                    "{look:?} dark {dark}: {status:?} drew {got:?}, not {want:?}"
                );
            }
        }
    }

    #[test]
    fn the_module_looks_draw_a_silent_wave_and_every_look_moves_with_the_voice() {
        let differing = |a: &[u8], b: &[u8]| {
            a.as_chunks::<4>()
                .0
                .iter()
                .zip(b.as_chunks::<4>().0)
                .filter(|(a, b)| a.iter().zip(*b).any(|(a, b)| a.abs_diff(*b) > 24))
                .count()
        };
        for (look, dark) in [
            (Look::Gunmetal, true),
            (Look::Holo, true),
            (Look::Neon, true),
            (Look::Neon, false),
        ] {
            let [without, quiet, loud] = [None, Some(0.0), Some(1.0)].map(|level| {
                let mut painter = painter(1.0, look, dark);
                let mut shown = view(&capturing(), false);
                for step in 0..15 {
                    let clock = f64::from(step) * 0.033;
                    shown.poll(clock, level.unwrap_or(0.0));
                    if level.is_none() {
                        shown.wave = None;
                    }
                    painter.draw(&shown, clock);
                }
                painter.frame.bgra.clone()
            });
            let silent = differing(&without, &quiet);
            let flat_bars_either_way = look == Look::Neon;
            assert!(
                flat_bars_either_way || silent > 150,
                "{look:?}: the silent wave drew {silent} pixels"
            );
            let moved = differing(&quiet, &loud);
            assert!(moved > 150, "{look:?} dark {dark}: {moved} pixels moved");
        }
    }

    #[test]
    fn a_new_look_or_system_scheme_shows_on_the_next_frame() {
        let line = (WIDTH / 2.0, 83.0);
        let pill = (WIDTH / 2.0, 26.5);
        let shown = view(&capturing(), false);
        let mut painter = painter(1.0, Look::Gunmetal, true);
        for (look, dark, at, want) in [
            (Look::Neon, false, pill, [0xb9, 0xcb, 0xdd]),
            (Look::Neon, true, pill, [0x12, 0x31, 0x5e]),
            (Look::Gunmetal, true, line, [107, 248, 255]),
        ] {
            assert!(painter.restyle(&Arc::new(Style::of(look)), dark));
            let got = rgb(painter.draw(&shown, 0.0), at);
            assert!(near(got, want), "{look:?} dark {dark} drew {got:?}");
        }
    }

    #[test]
    fn only_holo_moves_on_its_own_and_only_while_its_shine_passes() {
        let holo = painter(1.0, Look::Holo, true);
        let wait = |clock: f64| holo.motion(clock).map(|wait| wait.as_secs_f64());
        for (clock, until) in [
            (1.0, 3.9),
            (8.0, 3.9),
            (5.0, super::super::view::FRAME.as_secs_f64()),
        ] {
            let got = wait(clock).unwrap();
            assert!((got - until).abs() < 1e-6, "at {clock} s: {got}");
        }
        for look in [Look::Gunmetal, Look::Neon] {
            assert_eq!(painter(1.0, look, true).motion(1.0), None, "{look:?}");
        }
    }

    const LOOKS: [(Look, bool); 4] = [
        (Look::Gunmetal, true),
        (Look::Holo, true),
        (Look::Neon, true),
        (Look::Neon, false),
    ];

    fn pending() -> Status {
        Status::Pending {
            target: None,
            route: CaptureRoute::Paste,
            text: "hello".to_owned(),
            waiting_for_quiet: false,
            pause_ms: 2000,
            copies: false,
            final_confirmation: None,
        }
    }

    #[test]
    fn the_countdown_bar_shrinks_as_its_time_runs_out_in_every_look() {
        for ((look, dark), tone) in LOOKS.into_iter().zip([
            [255, 194, 75],
            [255, 194, 75],
            [0xff, 0xc2, 0x4b],
            [0x8a, 0x5a, 0x00],
        ]) {
            let mut painter = painter(1.0, look, dark);
            let mut shown = view(&pending(), false);
            let mut toned = |clock: f64| {
                shown.poll(clock, 0.0);
                painter
                    .draw(&shown, clock)
                    .bgra
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .filter(|[b, g, r, _]| near([*r, *g, *b], tone))
                    .count()
            };
            let (full, half) = (toned(0.0), toned(1.0));
            assert!(
                half * 10 < full * 8 && half * 10 > full * 2,
                "{look:?} dark {dark}: {full} then {half}"
            );
        }
    }

    #[test]
    fn flipping_the_draft_switch_redraws_a_pending_countdown_in_every_look() {
        for (look, dark) in LOOKS {
            let mut painter = painter(1.0, look, dark);
            let mut shown = view(&pending(), false);
            shown.poll(1.0, 0.0);
            let with = painter.draw(&shown, 1.0).bgra.clone();
            shown.set_draft(false);
            let without = painter.draw(&shown, 1.0).bgra.clone();
            assert_ne!(with, without, "{look:?} dark {dark}: off");
            shown.set_draft(true);
            assert_eq!(
                painter.draw(&shown, 1.0).bgra,
                with,
                "{look:?} dark {dark}: on"
            );
        }
    }

    #[test]
    fn the_cancel_button_shows_only_on_a_capture_the_desktop_lets_be_clicked() {
        for status in [capturing(), Status::CleaningUp { target: None }] {
            for (look, dark) in LOOKS {
                let mut painter = painter(1.0, look, dark);
                let with = painter.draw(&view(&status, true), 0.0).bgra.clone();
                let without = painter.draw(&view(&status, false), 0.0).bgra.clone();
                let differing = with
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .zip(without.as_chunks::<4>().0)
                    .filter(|(a, b)| a != b)
                    .count();
                assert!(
                    differing > 40,
                    "{status:?} {look:?} dark {dark}: {differing}"
                );
            }
        }
    }

    #[test]
    fn neons_card_grows_with_the_draft_up_to_three_lines() {
        let bottom = |committed: &str| {
            let mut painter = painter(1.0, Look::Neon, true);
            let status = Status::Capturing {
                target: None,
                committed: committed.to_owned(),
                partial: None,
                notice: None,
            };
            let frame = painter.draw(&view(&status, false), 0.0);
            let width = frame.width as usize;
            (0..frame.height as usize)
                .rfind(|y| (0..width).any(|x| frame.bgra[(y * width + x) * 4 + 3] > 30))
                .unwrap()
        };
        let sentence = "one more sentence that runs on ";
        let (one, three, many) = (
            bottom("hello"),
            bottom(&sentence.repeat(5)),
            bottom(&sentence.repeat(9)),
        );
        assert!(three > one + 30, "{one} {three}");
        assert_eq!(three, many, "the card stops at three lines");
    }

    #[test]
    fn only_a_capture_where_the_desktop_takes_clicks_is_cancelled_by_its_button() {
        let done = Status::Done {
            target: None,
            route: CaptureRoute::Paste,
            notice: String::new(),
            wake_words: true,
        };
        for look in [Look::Gunmetal, Look::Holo, Look::Neon] {
            let mut painter = painter(1.0, look, true);
            let clickable = view(&capturing(), true);
            painter.draw(&clickable, 0.0);
            let button = match &painter.skin {
                Skin::Module(_) => (CANCEL.0 + CANCEL.2 / 2.0, CANCEL.1 + CANCEL.3 / 2.0),
                Skin::Neon(neon) => neon
                    .cancel
                    .map(|(x, y, w, h)| (x + w / 2.0, y + h / 2.0))
                    .unwrap(),
            };
            assert!(painter.cancels(&clickable, button.0, button.1), "{look:?}");
            for (x, y) in [
                (WIDTH / 2.0, 150.0),
                (button.0, 150.0),
                (10.0, button.1),
                (button.0 + 14.0, button.1),
                (button.0, button.1 + 12.0),
            ] {
                assert!(!painter.cancels(&clickable, x, y), "{look:?} at ({x}, {y})");
            }
            let unclickable = view(&capturing(), false);
            painter.draw(&unclickable, 0.0);
            assert!(
                !painter.cancels(&unclickable, button.0, button.1),
                "{look:?} without clicks"
            );
            let finished = view(&done, true);
            painter.draw(&finished, 0.0);
            assert!(
                !painter.cancels(&finished, button.0, button.1),
                "{look:?} once done"
            );
        }
    }
    fn calm_view(status: Status) -> View {
        let mut shown = View::new(true);
        shown.set_status(status, 5.4);
        for step in 0..16 {
            shown.poll(5.4 + f64::from(step) * 0.033, 0.6);
        }
        shown
    }

    #[test]
    fn the_largest_font_and_spacing_show_the_full_draft() {
        let words = "Gypsy jumps";
        for (look, dark) in LOOKS {
            let mut style = Style::of(look);
            for scheme in std::iter::once(&mut style.dark).chain(style.light.iter_mut()) {
                scheme.font_scale = 1.25;
                scheme.spacing = 1.5;
            }
            let mut painter = Painter::new(1.0, Arc::new(style), dark).unwrap();
            let draw = |painter: &mut Painter, committed: &str| {
                let shown = calm_view(Status::Capturing {
                    target: None,
                    committed: committed.to_owned(),
                    partial: None,
                    notice: None,
                });
                painter.draw(&shown, shown.clock).bgra.clone()
            };
            let blank = draw(&mut painter, " ");
            let draft = draw(&mut painter, words);
            let extent = |indices: Vec<usize>| {
                let xs = indices.iter().map(|at| at % 460);
                let ys = indices.iter().map(|at| at / 460);
                (
                    xs.clone().max().unwrap() - xs.min().unwrap(),
                    ys.clone().max().unwrap() - ys.min().unwrap(),
                )
            };
            let changed = draft
                .as_chunks::<4>()
                .0
                .iter()
                .zip(blank.as_chunks::<4>().0)
                .enumerate()
                .filter_map(|(at, (ink, none))| (ink != none).then_some(at))
                .collect();
            let measured = extent(changed);
            let style = if look == Look::Neon {
                overlay::DRAFT
            } else {
                overlay::TICKER
            };
            let scale = 1.25;
            let scaled = overlay::TextStyle {
                size: style.size * scale,
                line_height: style.line_height * scale,
                ..style
            };
            let mut fixture = Canvas::new(WIDTH, HEIGHT, 1.0).unwrap();
            painter.text.draw(
                &mut fixture,
                &[(words, overlay::WHITE)],
                scaled,
                super::super::paint::Layout::Line,
                (40.0, 40.0),
            );
            let mut frame = OverlayFrame::default();
            fixture.frame_into(&mut frame);
            let expected = extent(
                frame
                    .bgra
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .enumerate()
                    .filter_map(|(at, &[_b, _g, _r, a])| (a > 0).then_some(at))
                    .collect(),
            );
            assert!(
                measured.0.abs_diff(expected.0) <= 2 && measured.1.abs_diff(expected.1) <= 2,
                "{look:?} dark={dark}: visible ink {measured:?}, full glyphs {expected:?}"
            );
        }
    }



    #[test]
    fn skin_rebuilds_once_when_the_visible_look_changes() {
        let mut painter = painter(1.0, Look::Neon, true);
        let changed = Arc::new(Style::of(Look::Gunmetal));
        let view = calm_view(capturing());
        let original = painter.draw(&view, view.clock).bgra.clone();
        assert!(painter.restyle(&changed, true));
        assert_eq!(painter.restyles, 1);
        for step in 0..8 {
            if step == 4 {
                assert!(painter.restyle(&Arc::new(Style::of(Look::Gunmetal)), true));
            }
            painter.draw(&view, view.clock);
        }
        assert_eq!(painter.restyles, 1, "drawing and sending equal styles");
        assert_ne!(painter.frame.bgra, original);
    }

    #[test]
    fn a_missing_family_falls_back_and_the_preview_uses_the_live_renderer() {
        let shown = calm_view(capturing());
        let look = Look::Holo;
        let baseline = Arc::new(Style::of(look));
        let mut missing = Style::of(look);
        for scheme in std::iter::once(&mut missing.dark).chain(missing.light.iter_mut()) {
            scheme.font_family = "Thalvi Missing Font".to_owned();
        }
        let missing = Arc::new(missing);
        let before = super::super::render_rgba(&shown, baseline, true, 1.0).unwrap();
        let after = super::super::render_rgba(&shown, Arc::clone(&missing), true, 1.0).unwrap();
        assert_eq!(before, after);
        assert_eq!((after.0, after.1, after.2.len()), (460, 200, 460 * 200 * 4));
        let mut painter = Painter::new(1.0, missing, true).unwrap();
        let bgra = &painter.draw(&shown, shown.clock).bgra;
        for ([r, g, b, a], [bb, gg, rr, aa]) in after
            .2
            .as_chunks::<4>()
            .0
            .iter()
            .zip(bgra.as_chunks::<4>().0)
        {
            assert_eq!(a, aa);
            for (straight, packed) in [(*r, *rr), (*g, *gg), (*b, *bb)] {
                let restored = (u16::from(straight) * u16::from(*a) + 127) / 255;
                assert_eq!(restored, u16::from(packed));
            }
        }
    }

    #[test]
    fn renderer_uses_named_theme_colors_and_measures() {
        for (renderer, code) in [
            ("module", include_str!("module.rs")),
            ("neon", include_str!("neon.rs")),
            ("paint", include_str!("paint.rs")),
        ] {
            let production = code.split("#[cfg(test)]").next().unwrap();
            for (line, text) in production.lines().enumerate() {
                let code = text.split("//").next().unwrap();
                assert!(
                    !code.contains("Rgba::rgb(") && !code.contains("Rgba {")
                        || code.trim_end().ends_with("-> Rgba {"),
                    "{renderer}:{} has a color literal",
                    line + 1
                );
                if renderer == "paint" {
                    continue;
                }
                let literal = code
                    .split(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '_')
                    .filter_map(|token| {
                        token
                            .contains('.')
                            .then(|| token.trim_end_matches("f32").parse::<f32>().ok())
                            .flatten()
                    })
                    .find(|number| ![0.0, 0.5, 1.0, 2.0].contains(number));
                assert!(
                    literal.is_none(),
                    "{renderer}:{} has an unnamed measure {literal:?}",
                    line + 1
                );
            }
        }
    }
}
