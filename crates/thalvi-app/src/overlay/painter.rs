//! Draws a view in the chosen look and theme into frames for the overlay's surface.

use std::time::Duration;

use thalvi_desktop::config::Look;
use thalvi_desktop::host::OverlayFrame;

use super::module::{CANCEL, Finish, Module};
use super::neon::Neon;
use super::paint::{Canvas, Text};
use super::view::{HEIGHT, View, WIDTH};

enum Skin {
    Module(Module),
    Neon(Neon),
}

impl Skin {
    fn new(look: Look, dark: bool, scale: f32) -> Option<Self> {
        Some(match look {
            Look::Gunmetal => Self::Module(Module::new(Finish::Gunmetal, scale)?),
            Look::Holo => Self::Module(Module::new(Finish::Holo, scale)?),
            Look::Neon => Self::Neon(Neon::new(dark)),
        })
    }
}

pub(super) struct Painter {
    text: Text,
    canvas: Canvas,
    frame: OverlayFrame,
    scale: f32,
    look: Look,
    dark: bool,
    skin: Skin,
}

impl Painter {
    /// Reads the system's fonts and draws the look's still parts at `scale` pixels per point.
    pub(super) fn new(scale: f32, look: Look, dark: bool) -> Option<Self> {
        Some(Self {
            text: Text::new(),
            canvas: Canvas::new(WIDTH, HEIGHT, scale)?,
            frame: OverlayFrame::default(),
            scale,
            look,
            dark,
            skin: Skin::new(look, dark, scale)?,
        })
    }

    /// Switches to `look` in the `dark` or light theme; the next frame shows it.
    pub(super) fn restyle(&mut self, look: Look, dark: bool) {
        if (look, dark) == (self.look, self.dark) {
            return;
        }
        if let Some(skin) = Skin::new(look, dark, self.scale) {
            (self.look, self.dark, self.skin) = (look, dark, skin);
        }
    }

    pub(super) fn draw(&mut self, view: &View, clock: f64) -> &OverlayFrame {
        match &mut self.skin {
            Skin::Module(module) => module.draw(&mut self.canvas, &mut self.text, view, clock),
            Skin::Neon(neon) => neon.draw(&mut self.canvas, &mut self.text, view),
        }
        self.canvas.frame_into(&mut self.frame);
        &self.frame
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
    use thalvi_desktop::capture::CaptureRoute;

    use super::*;
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
            reason = "a test point"
        )]
        let at = (y as usize * frame.width as usize + x as usize) * 4;
        [frame.bgra[at + 2], frame.bgra[at + 1], frame.bgra[at]]
    }

    fn near(got: [u8; 3], want: [u8; 3]) -> bool {
        got.iter()
            .zip(want)
            .all(|(got, want)| got.abs_diff(want) < 48)
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
            let mut painter = Painter::new(1.0, look, dark).unwrap();
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
                let mut painter = Painter::new(1.0, look, dark).unwrap();
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
    fn a_new_look_or_theme_shows_on_the_next_frame() {
        let line = (WIDTH / 2.0, 83.0);
        let pill = (WIDTH / 2.0, 26.5);
        let shown = view(&capturing(), false);
        let mut painter = Painter::new(1.0, Look::Gunmetal, true).unwrap();
        for (look, dark, at, want) in [
            (Look::Neon, false, pill, [0xb9, 0xcb, 0xdd]),
            (Look::Neon, true, pill, [0x12, 0x31, 0x5e]),
            (Look::Gunmetal, true, line, [107, 248, 255]),
        ] {
            painter.restyle(look, dark);
            let got = rgb(painter.draw(&shown, 0.0), at);
            assert!(near(got, want), "{look:?} dark {dark} drew {got:?}");
        }
    }

    #[test]
    fn only_holo_moves_on_its_own_and_only_while_its_shine_passes() {
        let holo = Painter::new(1.0, Look::Holo, true).unwrap();
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
            assert_eq!(
                Painter::new(1.0, look, true).unwrap().motion(1.0),
                None,
                "{look:?}"
            );
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
            let mut painter = Painter::new(1.0, look, dark).unwrap();
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
            let mut painter = Painter::new(1.0, look, dark).unwrap();
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
        for (look, dark) in LOOKS {
            let mut painter = Painter::new(1.0, look, dark).unwrap();
            let with = painter.draw(&view(&capturing(), true), 0.0).bgra.clone();
            let without = painter.draw(&view(&capturing(), false), 0.0).bgra.clone();
            let differing = with
                .as_chunks::<4>()
                .0
                .iter()
                .zip(without.as_chunks::<4>().0)
                .filter(|(a, b)| a != b)
                .count();
            assert!(differing > 40, "{look:?} dark {dark}: {differing}");
        }
    }

    #[test]
    fn neons_card_grows_with_the_draft_up_to_three_lines() {
        let bottom = |committed: &str| {
            let mut painter = Painter::new(1.0, Look::Neon, true).unwrap();
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
        };
        for look in [Look::Gunmetal, Look::Holo, Look::Neon] {
            let mut painter = Painter::new(1.0, look, true).unwrap();
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
}
