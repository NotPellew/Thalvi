//! The fixed palettes the three looks draw with.

use crate::config::Look;

pub mod overlay;

/// `#rrggbb` or `#rrggbbaa`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color([u8; 4]);

impl Color {
    fn parse(text: &str) -> Result<Self, String> {
        let hex = text
            .strip_prefix('#')
            .filter(|hex| hex.len() == 6 || hex.len() == 8)
            .ok_or_else(|| format!("use a colour like #6bf8ff, not {text}"))?;
        if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!("use a colour like #6bf8ff, not {text}"));
        }
        let byte = |pair: usize| u8::from_str_radix(&hex[pair..pair + 2], 16).unwrap_or(0);
        let alpha = if hex.len() == 8 { byte(6) } else { 0xff };
        Ok(Self([byte(0), byte(2), byte(4), alpha]))
    }

    /// The colour as `#rrggbb` or `#rrggbbaa`.
    #[must_use]
    pub fn to_hex(self) -> String {
        let [r, g, b, a] = self.0;
        if a == 0xff {
            format!("#{r:02x}{g:02x}{b:02x}")
        } else {
            format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
        }
    }

    /// Red, green, blue and alpha, each 0–255.
    #[must_use]
    pub const fn channels(self) -> [u8; 4] {
        self.0
    }
}

/// One scheme of a look.
#[derive(Clone, Debug, PartialEq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one flag per fixed decoration or effect"
)]
pub struct Scheme {
    pub background: Color,
    pub surface: Color,
    pub raised: Color,
    pub text: Color,
    pub muted: Color,
    pub border: Color,
    pub accent: Color,
    pub on_accent: Color,
    pub secondary: Color,
    pub pending: Color,
    pub done: Color,
    pub failed: Color,
    pub live: Color,
    pub working: Color,
    pub quiet: Color,
    pub font_family: String,
    pub mono_family: String,
    pub font_scale: f32,
    pub font_weight: u16,
    pub tracking: f32,
    pub spacing: f32,
    pub radius: u8,
    pub chamfer: u8,
    pub border_width: u8,
    pub opacity: f32,
    pub glow: f32,
    pub mesh: f32,
    pub scanlines: bool,
    pub corners: bool,
    pub shine: bool,
    pub pulse: bool,
}

/// A look's fixed palette, with a light scheme only for Neon.
#[derive(Clone, Debug, PartialEq)]
pub struct Style {
    look: Look,
    pub dark: Scheme,
    pub light: Option<Scheme>,
}

impl Style {
    /// The look this palette draws.
    #[must_use]
    pub const fn look(&self) -> Look {
        self.look
    }

    /// The active scheme, falling back to the look's only scheme.
    #[must_use]
    pub fn scheme(&self, dark: bool) -> &Scheme {
        if dark {
            &self.dark
        } else {
            self.light.as_ref().unwrap_or(&self.dark)
        }
    }

    /// The palette of `look`.
    #[must_use]
    pub fn of(look: Look) -> Self {
        match look {
            Look::Neon => Self {
                look,
                dark: neon_dark(),
                light: Some(neon_light()),
            },
            Look::Holo => Self {
                look,
                dark: holo(),
                light: None,
            },
            Look::Gunmetal => Self {
                look,
                dark: gunmetal(),
                light: None,
            },
        }
    }
}

const SANS: &str = "system-ui, -apple-system, \"Segoe UI\", sans-serif";
const MONO: &str = "ui-monospace, \"SF Mono\", SFMono-Regular, Menlo, Consolas, monospace";

fn with_fonts(mut scheme: Scheme) -> Scheme {
    SANS.clone_into(&mut scheme.font_family);
    MONO.clone_into(&mut scheme.mono_family);
    scheme
}

fn neon_dark() -> Scheme {
    with_fonts(Scheme {
        background: must("#020617"),
        surface: must("#04122e"),
        raised: must("#061a3d"),
        text: must("#eafff3"),
        muted: must("#9cc7b4"),
        border: must("#12315e"),
        accent: must("#2bff88"),
        on_accent: must("#02120a"),
        secondary: must("#3b82f6"),
        pending: must("#ffc24b"),
        done: must("#2bff88"),
        failed: must("#ff6b5e"),
        live: must("#12315e"),
        working: must("#9cc7b499"),
        quiet: must("#9cc7b4"),
        chamfer: 0,
        border_width: 1,
        opacity: 0.92,
        glow: 0.25,
        mesh: 0.0,
        scanlines: false,
        corners: false,
        shine: false,
        font_family: String::new(),
        mono_family: String::new(),
        font_scale: 1.0,
        font_weight: 700,
        tracking: 0.12,
        spacing: 1.0,
        radius: 6,
        pulse: true,
    })
}

fn neon_light() -> Scheme {
    let mut scheme = neon_dark();
    scheme.background = must("#e8eff7");
    scheme.surface = must("#f7fbff");
    scheme.raised = must("#d3e2f3");
    scheme.text = must("#07182e");
    scheme.muted = must("#3d5a73");
    scheme.border = must("#b9cbdd");
    scheme.accent = must("#007a43");
    scheme.on_accent = must("#ffffff");
    scheme.secondary = must("#0b3d91");
    scheme.pending = must("#8a5a00");
    scheme.done = must("#007a43");
    scheme.failed = must("#b42318");
    scheme.live = must("#b9cbdd");
    scheme.working = must("#3d5a7399");
    scheme.quiet = must("#3d5a73");
    scheme.opacity = 0.94;
    scheme
}

fn holo() -> Scheme {
    with_fonts(Scheme {
        background: must("#0c1820"),
        surface: must("#0a1c2473"),
        raised: must("#0f1a22"),
        text: must("#dfe9ef"),
        muted: must("#8ea7b5"),
        border: must("#5d92b4"),
        accent: module_accent(),
        on_accent: must("#04141a"),
        secondary: must("#6bf8ff"),
        pending: must("#ffc24b"),
        done: must("#2bff88"),
        failed: must("#ff6b5e"),
        live: must("#6bf8ff"),
        working: must("#6bf8ff"),
        quiet: must("#8fb3c4"),
        chamfer: 7,
        border_width: 1,
        opacity: 0.45,
        glow: 0.6,
        mesh: 0.05,
        scanlines: true,
        corners: true,
        shine: true,
        font_family: String::new(),
        mono_family: String::new(),
        font_scale: 1.0,
        font_weight: 700,
        tracking: 0.18,
        spacing: 1.0,
        radius: 0,
        pulse: true,
    })
}

fn gunmetal() -> Scheme {
    with_fonts(Scheme {
        background: must("#0f1012"),
        surface: must("#141517"),
        raised: must("#5c5c60"),
        text: must("#dfe9ef"),
        muted: must("#8ea7b5"),
        border: must("#3a3a3d"),
        accent: module_accent(),
        on_accent: must("#6bf8ff"),
        secondary: must("#6bf8ff"),
        pending: must("#ffc24b"),
        done: must("#2bff88"),
        failed: must("#ff6b5e"),
        live: must("#6bf8ff"),
        working: must("#6bf8ff"),
        quiet: must("#8fb3c4"),
        chamfer: 9,
        border_width: 5,
        opacity: 1.0,
        glow: 0.6,
        mesh: 0.06,
        scanlines: false,
        corners: false,
        shine: false,
        font_family: String::new(),
        mono_family: String::new(),
        font_scale: 1.0,
        font_weight: 700,
        tracking: 0.18,
        spacing: 1.0,
        radius: 2,
        pulse: true,
    })
}

fn must(hex: &str) -> Color {
    Color::parse(hex).unwrap_or(Color([0, 0, 0, 0xff]))
}

fn module_accent() -> Color {
    must("#6bf8ff")
}

impl Look {
    /// The name the file uses.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Neon => "neon",
            Self::Holo => "holo",
            Self::Gunmetal => "gunmetal",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_look_and_scheme_draws_the_captured_built_in_palette() {
        // Captured from the base's resolved style with no `themes` key.
        let cases: &[(&str, bool, &str, &str)] = &[
            (
                "neon",
                true,
                "dark",
                r#"Scheme { background: Color([2, 6, 23, 255]), surface: Color([4, 18, 46, 255]), raised: Color([6, 26, 61, 255]), text: Color([234, 255, 243, 255]), muted: Color([156, 199, 180, 255]), border: Color([18, 49, 94, 255]), accent: Color([43, 255, 136, 255]), on_accent: Color([2, 18, 10, 255]), secondary: Color([59, 130, 246, 255]), pending: Color([255, 194, 75, 255]), done: Color([43, 255, 136, 255]), failed: Color([255, 107, 94, 255]), live: Color([18, 49, 94, 255]), working: Color([156, 199, 180, 153]), quiet: Color([156, 199, 180, 255]), font_family: "system-ui, -apple-system, \"Segoe UI\", sans-serif", mono_family: "ui-monospace, \"SF Mono\", SFMono-Regular, Menlo, Consolas, monospace", font_scale: 1.0, font_weight: 700, tracking: 0.12, spacing: 1.0, radius: 6, chamfer: 0, border_width: 1, opacity: 0.92, glow: 0.25, mesh: 0.0, scanlines: false, corners: false, shine: false, pulse: true }"#,
            ),
            (
                "neon",
                false,
                "light",
                r#"Scheme { background: Color([232, 239, 247, 255]), surface: Color([247, 251, 255, 255]), raised: Color([211, 226, 243, 255]), text: Color([7, 24, 46, 255]), muted: Color([61, 90, 115, 255]), border: Color([185, 203, 221, 255]), accent: Color([0, 122, 67, 255]), on_accent: Color([255, 255, 255, 255]), secondary: Color([11, 61, 145, 255]), pending: Color([138, 90, 0, 255]), done: Color([0, 122, 67, 255]), failed: Color([180, 35, 24, 255]), live: Color([185, 203, 221, 255]), working: Color([61, 90, 115, 153]), quiet: Color([61, 90, 115, 255]), font_family: "system-ui, -apple-system, \"Segoe UI\", sans-serif", mono_family: "ui-monospace, \"SF Mono\", SFMono-Regular, Menlo, Consolas, monospace", font_scale: 1.0, font_weight: 700, tracking: 0.12, spacing: 1.0, radius: 6, chamfer: 0, border_width: 1, opacity: 0.94, glow: 0.25, mesh: 0.0, scanlines: false, corners: false, shine: false, pulse: true }"#,
            ),
            (
                "holo",
                true,
                "dark",
                r#"Scheme { background: Color([12, 24, 32, 255]), surface: Color([10, 28, 36, 115]), raised: Color([15, 26, 34, 255]), text: Color([223, 233, 239, 255]), muted: Color([142, 167, 181, 255]), border: Color([93, 146, 180, 255]), accent: Color([107, 248, 255, 255]), on_accent: Color([4, 20, 26, 255]), secondary: Color([107, 248, 255, 255]), pending: Color([255, 194, 75, 255]), done: Color([43, 255, 136, 255]), failed: Color([255, 107, 94, 255]), live: Color([107, 248, 255, 255]), working: Color([107, 248, 255, 255]), quiet: Color([143, 179, 196, 255]), font_family: "system-ui, -apple-system, \"Segoe UI\", sans-serif", mono_family: "ui-monospace, \"SF Mono\", SFMono-Regular, Menlo, Consolas, monospace", font_scale: 1.0, font_weight: 700, tracking: 0.18, spacing: 1.0, radius: 0, chamfer: 7, border_width: 1, opacity: 0.45, glow: 0.6, mesh: 0.05, scanlines: true, corners: true, shine: true, pulse: true }"#,
            ),
            (
                "gunmetal",
                true,
                "dark",
                r#"Scheme { background: Color([15, 16, 18, 255]), surface: Color([20, 21, 23, 255]), raised: Color([92, 92, 96, 255]), text: Color([223, 233, 239, 255]), muted: Color([142, 167, 181, 255]), border: Color([58, 58, 61, 255]), accent: Color([107, 248, 255, 255]), on_accent: Color([107, 248, 255, 255]), secondary: Color([107, 248, 255, 255]), pending: Color([255, 194, 75, 255]), done: Color([43, 255, 136, 255]), failed: Color([255, 107, 94, 255]), live: Color([107, 248, 255, 255]), working: Color([107, 248, 255, 255]), quiet: Color([143, 179, 196, 255]), font_family: "system-ui, -apple-system, \"Segoe UI\", sans-serif", mono_family: "ui-monospace, \"SF Mono\", SFMono-Regular, Menlo, Consolas, monospace", font_scale: 1.0, font_weight: 700, tracking: 0.18, spacing: 1.0, radius: 2, chamfer: 9, border_width: 5, opacity: 1.0, glow: 0.6, mesh: 0.06, scanlines: false, corners: false, shine: false, pulse: true }"#,
            ),
        ];
        for (name, dark, scheme_name, expected) in cases {
            let look = match *name {
                "neon" => Look::Neon,
                "holo" => Look::Holo,
                _ => Look::Gunmetal,
            };
            let style = Style::of(look);
            assert_eq!(
                format!("{:?}", style.scheme(*dark)),
                *expected,
                "{name} {scheme_name}"
            );
            assert_eq!(style.light.is_some(), *name == "neon", "{name}");
        }
    }
}
