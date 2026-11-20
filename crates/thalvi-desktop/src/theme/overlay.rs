//! The overlay's built-in artwork and the resolved values used to draw it.

use super::{Color, Scheme, Style, defaults_of};
use crate::config::Look;

/// A colour with straight alpha.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: f32,
}

impl Rgba {
    #[must_use]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 1.0 }
    }

    #[must_use]
    pub const fn alpha(self, a: f32) -> Self {
        Self { a, ..self }
    }

    #[must_use]
    pub fn faded(self, share: f32) -> Self {
        self.alpha(self.a * share)
    }
}

impl From<Color> for Rgba {
    fn from(color: Color) -> Self {
        let [r, g, b, a] = color.channels();
        Self::rgb(r, g, b).alpha(f32::from(a) / 255.0)
    }
}

pub const WHITE: Rgba = Rgba::rgb(255, 255, 255);
pub const BLACK: Rgba = Rgba::rgb(0, 0, 0);
pub const TRANSPARENT: Rgba = BLACK.alpha(0.0);

/// Built-in typography for one role, before user font scaling.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextStyle {
    pub size: f32,
    pub line_height: f32,
    pub weight: u16,
    pub mono: bool,
    pub tracking: f32,
}

const fn text(size: f32, line_height: f32, weight: u16, mono: bool, tracking: f32) -> TextStyle {
    TextStyle {
        size,
        line_height,
        weight,
        mono,
        tracking,
    }
}

pub const CONFIRMATION: TextStyle = text(13.0, 17.0, 600, false, 0.0);
pub const TICKER: TextStyle = text(14.0, 22.0, 400, false, 0.0);
pub const TAG: TextStyle = text(11.0, 14.0, 700, true, 1.54);
pub const MODULE_CANCEL: TextStyle = text(14.0, 18.0, 400, false, 0.0);
pub const DRAFT: TextStyle = text(13.0, 13.0 * 1.45, 400, false, 0.0);
pub const LABEL: TextStyle = text(11.0, 14.0, 700, true, 1.32);
pub const TARGET: TextStyle = text(12.0, 15.0, 400, true, 0.0);
pub const NEON_CANCEL: TextStyle = text(16.0, 20.0, 400, false, 0.0);

/// User typography and spacing relative to the built-in artwork.
#[derive(Clone, Copy, Debug)]
pub struct Measures {
    pub font_scale: f32,
    pub spacing: f32,
    label_weight: u16,
    tracking: Option<f32>,
    pub radius: f32,
    pub radius_delta: f32,
    pub border: f32,
    pub border_delta: f32,
    pub chamfer_delta: f32,
    pub glow_ratio: f32,
    pub pulse: bool,
}

impl Measures {
    fn resolve(scheme: &Scheme, base: &Scheme) -> Self {
        Self {
            font_scale: scheme.font_scale,
            spacing: scheme.spacing,
            label_weight: scheme.font_weight,
            tracking: (scheme.tracking.to_bits() != base.tracking.to_bits())
                .then_some(scheme.tracking),
            radius: f32::from(scheme.radius),
            radius_delta: f32::from(scheme.radius) - f32::from(base.radius),
            border: f32::from(scheme.border_width),
            border_delta: f32::from(scheme.border_width) - f32::from(base.border_width),
            chamfer_delta: f32::from(scheme.chamfer) - f32::from(base.chamfer),
            glow_ratio: scheme.glow / base.glow,
            pulse: scheme.pulse,
        }
    }

    #[must_use]
    pub fn text(self, base: TextStyle) -> TextStyle {
        TextStyle {
            size: base.size * self.font_scale,
            line_height: base.line_height * self.font_scale,
            tracking: base.tracking * self.font_scale,
            ..base
        }
    }

    #[must_use]
    pub fn label(self, base: TextStyle) -> TextStyle {
        let mut style = self.text(base);
        style.weight = self.label_weight;
        style.tracking = self
            .tracking
            .map_or(base.tracking, |tracking| base.size * tracking)
            * self.font_scale;
        style
    }
}

/// Maps a shared token onto the overlay's artwork. Some artwork defaults use a different
/// shade from Settings, or a different shade at each stop of a gradient.
fn tint(chosen: Color, base: Color, artwork: Rgba) -> Rgba {
    if chosen == base {
        artwork
    } else {
        Rgba::from(chosen).faded(artwork.a)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct StateColors {
    pub live: Rgba,
    pub quiet: Rgba,
    pub working: Rgba,
    pub pending: Rgba,
    pub done: Rgba,
    pub failed: Rgba,
}

impl StateColors {
    fn resolve(scheme: &Scheme, base: &Scheme, live: Rgba, working: Rgba) -> Self {
        Self {
            live: if scheme.live == base.live {
                live
            } else {
                scheme.live.into()
            },
            quiet: scheme.quiet.into(),
            working: if scheme.working == base.working {
                working
            } else {
                scheme.working.into()
            },
            pending: scheme.pending.into(),
            done: scheme.done.into(),
            failed: scheme.failed.into(),
        }
    }
}

const MODULE_ACCENT: Rgba = Rgba::rgb(107, 248, 255);
const MODULE_MUTED: Rgba = Rgba::rgb(143, 179, 196);
const MODULE_TEXT: Rgba = Rgba::rgb(234, 255, 243);
type Gradient<const N: usize> = [(f32, Rgba); N];

const METAL: Gradient<3> = [
    (0.0, Rgba::rgb(0x3a, 0x3a, 0x3a)),
    (0.8, Rgba::rgb(0x5a, 0x5a, 0x5a)),
    (1.0, Rgba::rgb(0x9a, 0x9a, 0x9a)),
];
const FACE: Gradient<3> = [
    (0.0, Rgba::rgb(0x1c, 0x1c, 0x1c)),
    (0.6, Rgba::rgb(0x2c, 0x2c, 0x2c)),
    (1.0, Rgba::rgb(0x3c, 0x3c, 0x3c)),
];
const BEZEL: Gradient<2> = [
    (0.0, Rgba::rgb(0x55, 0x55, 0x55)),
    (1.0, Rgba::rgb(0x88, 0x88, 0x88)),
];
const GLASS: Gradient<2> = [
    (0.0, Rgba::rgb(0x03, 0x08, 0x0d)),
    (1.0, Rgba::rgb(0x0a, 0x18, 0x20)),
];
const FIELD: Gradient<3> = [
    (0.0, Rgba::rgb(0x0f, 0x25, 0x30)),
    (0.55, Rgba::rgb(0x08, 0x14, 0x19)),
    (1.0, Rgba::rgb(0x0b, 0x1a, 0x21)),
];
const BUTTON: Gradient<2> = [
    (0.0, Rgba::rgb(0x5c, 0x5c, 0x60)),
    (1.0, Rgba::rgb(0x3a, 0x3a, 0x3d)),
];

fn gradient<const N: usize>(
    art: Gradient<N>,
    chosen: Color,
    base: Color,
    opacity: f32,
) -> Gradient<N> {
    art.map(|(at, color)| (at, tint(chosen, base, color).faded(opacity)))
}

/// Colours and measures for Gunmetal and Holo's shared module.
#[derive(Debug)]
pub struct Module {
    pub measures: Measures,
    pub state: StateColors,
    pub accent: Rgba,
    pub text: Rgba,
    pub muted: Rgba,
    pub shadow: Rgba,
    pub metal: Gradient<3>,
    pub face: Gradient<3>,
    pub bezel: Gradient<2>,
    pub glass: Gradient<2>,
    pub field: Gradient<3>,
    pub rim: Rgba,
    pub button: Gradient<2>,
    pub button_outline: Rgba,
    pub button_glyph: Rgba,
    pub mesh: f32,
    pub scanlines: bool,
    pub corners: bool,
    pub shine: bool,
}

impl Module {
    #[must_use]
    pub fn resolve(style: &Style) -> Self {
        let built_in = defaults_of(style.look());
        let base = &built_in.dark;
        let scheme = &style.dark;
        let opacity = if scheme.opacity.to_bits() == base.opacity.to_bits() {
            1.0
        } else {
            scheme.opacity
        };
        let accent = tint(scheme.accent, base.accent, MODULE_ACCENT);
        let rim = Rgba::from(scheme.border);
        let holo = style.look() == Look::Holo;
        let mut state = StateColors::resolve(scheme, base, accent, accent);
        state.quiet = tint(scheme.quiet, base.quiet, MODULE_MUTED);
        Self {
            measures: Measures::resolve(scheme, base),
            state,
            accent,
            text: tint(scheme.text, base.text, MODULE_TEXT),
            muted: tint(scheme.muted, base.muted, MODULE_MUTED),
            shadow: tint(scheme.accent, base.accent, BLACK.alpha(0.6)),
            metal: gradient(METAL, scheme.border, base.border, opacity),
            face: gradient(FACE, scheme.background, base.background, opacity),
            bezel: gradient(BEZEL, scheme.raised, base.raised, opacity),
            glass: gradient(GLASS, scheme.surface, base.surface, opacity),
            field: [
                (
                    FIELD[0].0,
                    tint(scheme.raised, base.raised, FIELD[0].1).faded(opacity),
                ),
                (
                    FIELD[1].0,
                    tint(scheme.background, base.background, FIELD[1].1).faded(opacity),
                ),
                (
                    FIELD[2].0,
                    tint(scheme.surface, base.surface, FIELD[2].1).faded(opacity),
                ),
            ],
            rim: rim.faded(opacity),
            button: gradient(BUTTON, scheme.raised, base.raised, 1.0),
            button_outline: if holo {
                tint(scheme.border, base.border, MODULE_MUTED.alpha(0.4))
            } else {
                tint(scheme.border, base.border, Rgba::rgb(0x6e, 0x6e, 0x72))
            },
            button_glyph: tint(
                scheme.text,
                base.text,
                if holo {
                    MODULE_MUTED
                } else {
                    Rgba::rgb(0xe1, 0xe4, 0xe7)
                },
            ),
            mesh: if holo && scheme.mesh.to_bits() == base.mesh.to_bits() {
                0.042
            } else {
                scheme.mesh
            },
            scanlines: scheme.scanlines,
            corners: scheme.corners,
            shine: scheme.shine,
        }
    }
}

/// Colours and measures for Neon in the requested system scheme.
#[derive(Debug)]
pub struct Neon {
    pub measures: Measures,
    pub state: StateColors,
    pub glass: Rgba,
    pub card: Rgba,
    pub line: Rgba,
    pub fg: Rgba,
    pub muted: Rgba,
    pub accent: Rgba,
    pub blue: Rgba,
    pub dot_halo: Option<Rgba>,
    pub shadow: Rgba,
}

impl Neon {
    #[must_use]
    pub fn resolve(style: &Style, dark: bool) -> Self {
        let built_in = defaults_of(Look::Neon);
        let base = built_in.scheme(dark);
        let scheme = style.scheme(dark);
        let glass = if dark {
            base.background.into()
        } else {
            base.surface.into()
        };
        let card = if dark { base.surface.into() } else { WHITE };
        let glass = tint(scheme.background, base.background, glass).faded(scheme.opacity);
        let card_opacity = if scheme.opacity.to_bits() != base.opacity.to_bits() {
            scheme.opacity
        } else if dark {
            0.95
        } else {
            0.96
        };
        let card = tint(scheme.surface, base.surface, card).faded(card_opacity);
        let line = Rgba::from(scheme.border);
        let mut state =
            StateColors::resolve(scheme, base, line, Rgba::from(scheme.muted).alpha(0.6));
        if scheme.working != base.working {
            state.working = scheme.working.into();
        }
        Self {
            measures: Measures::resolve(scheme, base),
            state,
            glass,
            card,
            line,
            fg: scheme.text.into(),
            muted: scheme.muted.into(),
            accent: scheme.accent.into(),
            blue: scheme.secondary.into(),
            dot_halo: (scheme.raised != base.raised).then(|| scheme.raised.into()),
            shadow: tint(scheme.accent, base.accent, BLACK),
        }
    }
}

pub const WAVE_W: f32 = 332.0;
pub const WAVE_H: f32 = 44.0;

pub mod module {
    //! The module's geometry and effects at the built-in scale.

    use super::{BLACK, Rgba, WHITE};

    pub const SHOULDER: f32 = 16.0;
    pub const SLANT: f32 = 13.0;
    pub const SLOT_X: f32 = 14.0;
    pub const SLOT_TOP: f32 = 2.0;
    pub const SLOT_BOTTOM: f32 = 4.0;
    pub const GLOW_PAD: f32 = 24.0;
    pub const SHINE_CYCLE: f64 = 7.0;
    pub const SHINE_FROM: f64 = 4.9;
    pub const BUTTON_RADIUS: f32 = 2.0;
    pub const BUTTON_GLASS: Rgba = BLACK.alpha(0.4);
    pub const SLOT_SHADE: Rgba = BLACK.alpha(0.85);
    pub const SHINE_COLOR: Rgba = WHITE.alpha(0.07);
    pub const WAVE_LEFT: f32 = 5.0;
    pub const WAVE_WITH_TEXT: f32 = 2.0;
    pub const WAVE_ALONE: f32 = 17.0;
    pub const TICKER_LEFT: f32 = 24.0;
    pub const TICKER_TOP: f32 = 52.0;
    pub const TICKER_FADE: f32 = 36.0;
    pub const RULE_TOP: f32 = 48.0;
    pub const CONFIRMATION_LEFT: f32 = 8.0;
    pub const TAG_LINE: f32 = 34.0;
    pub const TAG_GAP: f32 = 12.0;

    pub const WAVE_HALO: (f32, f32) = (160.0, 30.0);
    pub const HALO_OFFSET: f32 = 7.0;
    pub const HALO_GAIN: f32 = 1.3;
    pub const HALO_ALPHA: f32 = 0.2;
    pub const HALO_BLUR: f32 = 10.0;
    pub const RULE_ALPHA: f32 = 0.25;
    pub const RULE_WIDTH: f32 = 1.0;
    pub const SHADOW_OFFSET: f32 = 6.0;
    pub const SHADOW_BLUR: f32 = 6.0;
    pub const FRAME_EDGE: f32 = 1.0;
    pub const BEZEL_SHARE: f32 = 0.2;
    pub const INSET_SHADE_HEIGHT: f32 = 14.0;
    pub const SCANLINE: (f32, f32, f32) = (1.0, 3.0, 0.045);
    pub const VIGNETTE: (f32, f32, f32) = (18.0, 9.0, 0.3);
    pub const BRACKET_REACH: f32 = 20.0;
    pub const BRACKET_OFFSET: f32 = 9.0;
    pub const BRACKET_WIDTH: f32 = 2.0;
    pub const BRACKET_BLUR: f32 = 3.0;
    pub const BRACKET_ALPHA: f32 = 0.9;
    pub const BRACKET_SLOPE: (f32, f32) = (0.35, 0.65);
    pub const BRACKET_SPAN: (f32, f32) = (0.3, 0.7);
    pub const SHINE_RANGE: (f32, f32, f32) = (80.0, 720.0, 70.0);
    pub const HEX_SIZE: (f32, f32) = (16.0, 27.7);
    pub const HEX_CORNERS: [(f32, f32); 6] = [
        (8.0, 0.0),
        (16.0, 4.62),
        (16.0, 13.86),
        (8.0, 18.47),
        (0.0, 13.86),
        (0.0, 4.62),
    ];
    pub const HEX_STROKE: f32 = 1.0;
    pub const STRAND_STEP: f32 = 4.0;
    pub const STRAND_LAYERS: [(f32, f32); 2] = [(3.2, 0.07), (1.0, 0.55)];
    pub const STRAND_FREQUENCIES: [(f32, f32, f32); 4] = [
        (3.1, 1.0, 0.62),
        (5.2, 1.35, 0.38),
        (2.5, 0.8, 0.55),
        (4.3, 1.1, 0.45),
    ];
    pub const STRAND_TWIST: f32 = 1.25;
    pub const STRAND_SIDES: u8 = 4;
    pub const STRAND_EDGE: f32 = 0.45;
    pub const STRAND_FALLOFF: [(f32, f32); 5] =
        [(0.0, 0.0), (0.18, 0.6), (0.5, 1.0), (0.82, 0.6), (1.0, 0.0)];
    pub const TAG_STROKE: f32 = 1.5;
    pub const TAG_RADIUS: f32 = 1.0;
    pub const TICKER_TRAVEL: f32 = 6.0;
    pub const TICKER_BOTTOM: f32 = 1.0;
    pub const CARET_INSETS: (f32, f32) = (1.0, 4.0);
    pub const CARET_WIDTH: f32 = 2.0;
    pub const BUTTON_RIM: (f32, f32) = (1.0, 3.0);
    pub const STRIP_INSETS: (f32, f32) = (4.0, 22.0);
    pub const STRIP_WIDTH: f32 = 2.0;
    pub const STRIP_RADIUS: f32 = 1.0;
    pub const STRIP_GLOWS: [(f32, f32); 2] = [(5.0, 0.12), (3.0, 0.2)];
    pub const STRIP_FADE: (f32, f32) = (0.2, 0.8);
    pub const SWEEP: (f64, f32, f32, f32) = (1.3, 0.3, 0.2, 2.33);
}

pub mod neon {
    //! The pill, draft card and microphone marks at the built-in scale.

    pub const TOP: f32 = 26.0;
    pub const SIDE: f32 = 12.0;
    pub const GAP: f32 = 6.0;
    pub const PART_GAP: f32 = 10.0;
    pub const PILL_VERTICAL: f32 = 6.0;
    pub const PILL_INSETS: (f32, f32) = (10.0, 14.0);
    pub const BAR_W: f32 = 3.0;
    pub const BAR_H: f32 = 22.0;
    pub const DOT: f32 = 10.0;
    pub const DRAFT_INSETS: (f32, f32) = (13.0, 9.0);
    pub const CARD_RADIUS: f32 = 12.0;
    pub const CANCEL_SIZE: f32 = 20.0;

    pub const BARS: usize = 9;
    pub const STRIP: f32 = 17.0 * BAR_W;
    pub const FLAT: f32 = 4.0 / BAR_H;
    pub const ENVELOPE: [f32; BARS] = [0.45, 0.65, 0.82, 0.95, 1.0, 0.95, 0.82, 0.65, 0.45];
    pub const DRAFT_LINES: f32 = 3.0;
    pub const MILLIS_PER_SECOND: f64 = 1000.0;
    pub const BAR_RATE: f32 = 60.0;
    pub const BAR_RADIUS: f32 = 1.5;
    pub const BAR_GAIN: f32 = 1.2;
    pub const BAR_WOBBLE: (f32, f32, f32, f32) = (0.75, 0.25, 110.0, 1.3);
    pub const BAR_EASING: (f32, f32, f64) = (0.6, 0.2, 0.1);
    pub const DOT_PULSE: (f64, f32) = (1.0, 0.65);
    pub const DOT_HALO: (f32, f32, f32) = (4.0, 9.0, 0.25);
    pub const BASE_EDGE: f32 = 1.0;
    pub const MIN_LABEL_ROOM: f32 = 40.0;
    pub const CARD_SHARE: f32 = 0.92;
    pub const CARET: (f32, f32, f32, f32) = (1.0, 3.0, 2.0, 14.0);
    pub const ENDING: (f32, f32, f32, f32, f32) = (5.2, 4.0, 4.0, 1.0, 0.22);
    pub const COUNTDOWN: (f32, f32, f32, f32) = (8.0, 4.0, 2.0, 0.35);
    pub const SHADOW_OFFSET: f32 = 6.0;
    pub const SHADOW_LAYERS: [(f32, f32); 3] = [(12.0, 0.05), (8.0, 0.08), (4.0, 0.12)];
}

/// The desktops' UI fonts, the first installed of which sets the overlay's text; Windows' and the
/// Linux desktops' names together, since each system has only its own.
pub const SANS: [&str; 6] = [
    "Segoe UI",
    "Adwaita Sans",
    "Cantarell",
    "Noto Sans",
    "DejaVu Sans",
    "Arial",
];
pub const MONO: [&str; 6] = [
    "Cascadia Mono",
    "Consolas",
    "Adwaita Mono",
    "Noto Sans Mono",
    "DejaVu Sans Mono",
    "Liberation Mono",
];
