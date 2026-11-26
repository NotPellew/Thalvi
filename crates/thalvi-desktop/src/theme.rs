//! Per-look theme overrides: validated once when the config loads, resolved against built-in defaults.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::config::Look;

pub mod overlay;

/// The theme schema this build reads and writes.
pub const VERSION: u32 = 1;

/// Text on background or surface must reach this contrast.
const TEXT_CONTRAST: f32 = 4.5;

/// Accent on raised, and the focus ring (the accent) on background, must reach this contrast.
const ACCENT_CONTRAST: f32 = 3.0;

/// One look's customizations, or the original JSON when this build cannot use them.
#[derive(Clone, Debug, PartialEq)]
#[expect(
    clippy::large_enum_variant,
    reason = "a usable look is the common case and stays unboxed"
)]
pub enum LookOverrides {
    /// Changes this build can apply.
    Usable(Overrides),
    /// Left as stored. The look draws its built-in defaults.
    Unreadable { raw: Value, reason: String },
}

/// The changes a user made to one look. An absent field takes the built-in default at resolve time.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Overrides {
    /// Draw the built-in look and keep these changes.
    pub suspended: bool,
    pub colors: ColorOverrides,
    pub font: FontOverrides,
    pub spacing: SpacingOverrides,
    pub shape: ShapeOverrides,
    pub border: BorderOverrides,
    pub surface: SurfaceOverrides,
    pub glow: GlowOverrides,
    pub decoration: DecorationOverrides,
    pub motion: MotionOverrides,
    /// Keys a newer build stored inside this look, kept verbatim.
    pub unknown: Map<String, Value>,
}

/// Colour changes for one scheme.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SchemeColors {
    pub background: Option<Color>,
    pub surface: Option<Color>,
    pub raised: Option<Color>,
    pub text: Option<Color>,
    pub muted: Option<Color>,
    pub border: Option<Color>,
    pub accent: Option<Color>,
    pub on_accent: Option<Color>,
    pub secondary: Option<Color>,
    pub pending: Option<Color>,
    pub done: Option<Color>,
    pub failed: Option<Color>,
    pub live: Option<Color>,
    pub working: Option<Color>,
    pub quiet: Option<Color>,
}

/// Per-scheme colour changes. Neon has a light scheme; Holo and Gunmetal do not.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ColorOverrides {
    pub dark: SchemeColors,
    pub light: Option<SchemeColors>,
}

/// Font changes. Families are free text; a missing family falls back to the system stack.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FontOverrides {
    pub family: Option<FontFamily>,
    pub mono_family: Option<FontFamily>,
    pub scale: Option<Scale>,
    pub weight: Option<Weight>,
    pub tracking: Option<Tracking>,
}

/// Spacing changes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpacingOverrides {
    pub scale: Option<Scale>,
}

/// Corner changes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ShapeOverrides {
    pub radius: Option<Px>,
    pub chamfer: Option<Px>,
}

/// Border changes. Gunmetal's bezel uses its own bound.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BorderOverrides {
    pub width: Option<Px>,
}

/// Surface changes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SurfaceOverrides {
    pub opacity: Option<Opacity>,
}

/// Glow changes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GlowOverrides {
    pub strength: Option<Strength>,
}

/// Decoration changes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DecorationOverrides {
    pub mesh: Option<Mesh>,
    pub scanlines: Option<bool>,
    pub corners: Option<bool>,
}

/// Motion changes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MotionOverrides {
    pub shine: Option<bool>,
    pub pulse: Option<bool>,
}

/// `#rrggbb` or `#rrggbbaa`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color([u8; 4]);

impl Color {
    /// Parses a colour, or why it is not one.
    ///
    /// # Errors
    /// The value is not `#rrggbb` or `#rrggbbaa`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let hex = text
            .strip_prefix('#')
            .filter(|hex| hex.len() == 6 || hex.len() == 8)
            .ok_or_else(|| colour_hint(text))?;
        if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(colour_hint(text));
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

fn colour_hint(text: &str) -> String {
    format!("use a colour like #6bf8ff, not {text}")
}

/// A font family: 1–64 printable characters, with none of the characters that would leave a CSS string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontFamily(String);

impl FontFamily {
    /// Parses a family, or why it cannot be used.
    ///
    /// # Errors
    /// The text is empty, longer than 64 characters, or contains a quote, backslash, semicolon or brace.
    pub fn parse(text: &str) -> Result<Self, String> {
        let printable = !text.is_empty()
            && text.chars().count() <= 64
            && text.chars().all(|c| {
                !c.is_control()
                    && c != '"'
                    && c != '\''
                    && c != '\\'
                    && c != ';'
                    && c != '{'
                    && c != '}'
            });
        if printable {
            Ok(Self(text.to_owned()))
        } else {
            Err("use 1–64 characters, with no quotes, backslash, semicolon or braces".to_owned())
        }
    }

    /// The family, ready to place inside CSS quotes.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A multiplier inside a closed range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scale(f32);

impl Scale {
    fn parse(value: f64, min: f32, max: f32) -> Result<Self, String> {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "an f64 past the f32 range becomes infinite, which the finite check rejects"
        )]
        let value = value as f32;
        if value.is_finite() && (min..=max).contains(&value) {
            Ok(Self(value))
        } else {
            Err(format!("use a number from {min} to {max}"))
        }
    }

    /// The multiplier.
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }
}

/// A size in pixels inside a closed range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Px(u8);

impl Px {
    fn parse(value: u64, min: u8, max: u8) -> Result<Self, String> {
        if (u64::from(min)..=u64::from(max)).contains(&value) {
            Ok(Self(u8::try_from(value).unwrap_or(max)))
        } else {
            Err(format!("use a whole number from {min} to {max}"))
        }
    }

    /// The size in pixels.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

/// Label and heading weight, 400–900.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Weight(u16);

impl Weight {
    fn parse(value: u64) -> Result<Self, String> {
        if (400..=900).contains(&value) && value.is_multiple_of(100) {
            Ok(Self(u16::try_from(value).unwrap_or(700)))
        } else {
            Err("use a weight from 400 to 900, in steps of 100".to_owned())
        }
    }

    /// The CSS font weight.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// Label letter-spacing in em, 0–0.4.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tracking(f32);

impl Tracking {
    fn parse(value: f64) -> Result<Self, String> {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "an f64 past the f32 range becomes infinite, which the finite check rejects"
        )]
        let value = value as f32;
        if value.is_finite() && (0.0..=0.4).contains(&value) {
            Ok(Self(value))
        } else {
            Err("use a number from 0 to 0.4".to_owned())
        }
    }

    /// The letter-spacing in em.
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }
}

/// A group or glass opacity, 0.6–1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Opacity(f32);

impl Opacity {
    fn parse(value: f64) -> Result<Self, String> {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "an f64 past the f32 range becomes infinite, which the finite check rejects"
        )]
        let value = value as f32;
        if value.is_finite() && (0.6..=1.0).contains(&value) {
            Ok(Self(value))
        } else {
            Err("use a number from 0.6 to 1".to_owned())
        }
    }

    /// The opacity.
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }
}

/// Glow strength, 0–1.5. Zero is off.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Strength(f32);

impl Strength {
    fn parse(value: f64) -> Result<Self, String> {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "an f64 past the f32 range becomes infinite, which the finite check rejects"
        )]
        let value = value as f32;
        if value.is_finite() && (0.0..=1.5).contains(&value) {
            Ok(Self(value))
        } else {
            Err("use a number from 0 to 1.5".to_owned())
        }
    }

    /// The strength.
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }
}

/// Hex-mesh opacity, 0–0.15.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mesh(f32);

impl Mesh {
    fn parse(value: f64) -> Result<Self, String> {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "an f64 past the f32 range becomes infinite, which the finite check rejects"
        )]
        let value = value as f32;
        if value.is_finite() && (0.0..=0.15).contains(&value) {
            Ok(Self(value))
        } else {
            Err("use a number from 0 to 0.15".to_owned())
        }
    }

    /// The mesh opacity.
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }
}

/// Every look's customizations, as stored under `themes`.
#[derive(Clone, Debug, PartialEq)]
pub struct Themes {
    version: u32,
    looks: BTreeMap<Look, LookOverrides>,
    /// Keys beside `version` and the look names, kept verbatim.
    unknown: Map<String, Value>,
    /// The whole `themes` value when this build must write it back exactly: a newer version, or a
    /// value that is not an object. `reason` is reported when no look notice already names it.
    preserved: Option<(Value, String)>,
}

impl Default for Themes {
    fn default() -> Self {
        Self {
            version: VERSION,
            looks: BTreeMap::new(),
            unknown: Map::new(),
            preserved: None,
        }
    }
}

impl Themes {
    /// No customizations. A preserved value is never empty: it is written back as read.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.preserved.is_none()
            && self.looks.is_empty()
            && self.unknown.is_empty()
            && self.version == VERSION
    }

    /// One look's customizations.
    #[must_use]
    pub fn of(&self, look: Look) -> Option<&LookOverrides> {
        self.looks.get(&look)
    }

    /// Turns the active look's customizations off without deleting them.
    pub fn suspend(&mut self, look: Look) {
        match self.looks.entry(look) {
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                if let LookOverrides::Usable(overrides) = entry.get_mut() {
                    overrides.suspended = true;
                }
            }
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(LookOverrides::Usable(Overrides {
                    suspended: true,
                    ..Overrides::default()
                }));
            }
        }
    }

    /// The plain notice for each look this build could not read. A preserved value is one notice
    /// that names no look.
    #[must_use]
    pub fn notices(&self) -> Vec<String> {
        if let Some((_, reason)) = &self.preserved {
            return vec![preserved_notice(reason)];
        }
        self.looks
            .iter()
            .filter_map(|(look, overrides)| match overrides {
                LookOverrides::Unreadable { reason, .. } => {
                    Some(unreadable_notice(look.name(), reason))
                }
                LookOverrides::Usable(_) => None,
            })
            .collect()
    }

    /// The file form. Empty themes are omitted by the caller. A preserved value is returned unchanged.
    #[must_use]
    pub fn to_value(&self) -> Value {
        if let Some((preserved, _)) = &self.preserved {
            return preserved.clone();
        }
        let mut object = Map::new();
        object.insert("version".to_owned(), Value::from(self.version));
        for (key, value) in &self.unknown {
            object.insert(key.clone(), value.clone());
        }
        for (look, overrides) in &self.looks {
            object.insert(look.name().to_owned(), look_to_value(overrides));
        }
        Value::Object(object)
    }

    /// Reads `themes` without ever failing the config. A look this build cannot use keeps its JSON.
    /// A newer version, or a value that is not an object, is kept whole.
    #[must_use]
    pub fn load(value: &Value) -> Self {
        let Some(object) = value.as_object() else {
            return preserved(value.clone(), "themes must be an object");
        };
        if object.is_empty() {
            return Self::default();
        }
        let version = object.get("version").and_then(Value::as_u64);
        let Some(version) = version else {
            return unreadable_all(object, "themes.version must be 1");
        };
        if version > u64::from(VERSION) {
            return preserved(
                value.clone(),
                &format!("themes.version {version} is newer than this Thalvi"),
            );
        }
        if version != u64::from(VERSION) {
            return unreadable_all(object, &format!("themes.version must be {VERSION}"));
        }
        let mut themes = Self {
            version: VERSION,
            looks: BTreeMap::new(),
            unknown: Map::new(),
            preserved: None,
        };
        for (key, value) in object {
            if key == "version" {
                continue;
            }
            match Look::parse(key) {
                Some(look) => {
                    themes
                        .looks
                        .insert(look, read_look(look, value, &format!("themes.{key}")));
                }
                None => {
                    themes.unknown.insert(key.clone(), value.clone());
                }
            }
        }
        themes
    }
}

/// Keeps `value` exactly. Every look draws its built-in defaults.
fn preserved(value: Value, reason: &str) -> Themes {
    Themes {
        version: VERSION,
        looks: BTreeMap::new(),
        unknown: Map::new(),
        preserved: Some((value, reason.to_owned())),
    }
}

fn unreadable_all(object: &Map<String, Value>, reason: &str) -> Themes {
    let mut looks = BTreeMap::new();
    let mut unknown = Map::new();
    for (key, value) in object {
        if key == "version" {
            continue;
        }
        match Look::parse(key) {
            Some(look) => {
                looks.insert(
                    look,
                    LookOverrides::Unreadable {
                        raw: value.clone(),
                        reason: reason.to_owned(),
                    },
                );
            }
            None => {
                unknown.insert(key.clone(), value.clone());
            }
        }
    }
    Themes {
        version: VERSION,
        looks,
        unknown,
        preserved: None,
    }
}

/// The sentence shown when the whole `themes` value cannot be read. It names no look.
#[must_use]
pub fn preserved_notice(reason: &str) -> String {
    format!("Your look customizations could not be read ({reason}).")
}

/// The sentence shown when a look's customizations cannot be read.
#[must_use]
pub fn unreadable_notice(look: &str, reason: &str) -> String {
    let name = look_title(look);
    format!(
        "Your {name} customizations could not be read ({reason}). Thalvi uses built-in {name}. The data is kept in desktop.json until you reset {name}."
    )
}

fn look_title(name: &str) -> &str {
    match name {
        "holo" => "Holo",
        "neon" => "Neon",
        "gunmetal" => "Gunmetal",
        other => other,
    }
}

/// A property a later part can offer, with the bounds copied from nowhere else.
#[derive(Clone, Debug, PartialEq)]
pub struct Descriptor {
    /// Dotted id, such as `colors.accent`.
    pub id: &'static str,
    /// What Settings calls it.
    pub label: &'static str,
    /// `colors`, `font`, `spacing`, `shape`, `border`, `surface`, `glow`, `decoration` or `motion`.
    pub category: &'static str,
    /// `color`, `text`, `number` or `toggle`.
    pub unit: &'static str,
    pub min: Option<f32>,
    pub max: Option<f32>,
    pub step: Option<f32>,
    /// Looks the property applies to.
    pub looks: &'static [Look],
    /// `settings`, `overlay`, or both.
    pub surfaces: &'static [&'static str],
}

const ALL: &[Look] = &[Look::Neon, Look::Holo, Look::Gunmetal];
const HOLO: &[Look] = &[Look::Holo];
const MODULES: &[Look] = &[Look::Holo, Look::Gunmetal];

/// Every property a user can set, in file order.
#[must_use]
pub const fn descriptors() -> &'static [Descriptor] {
    DESCRIPTORS
}

const DESCRIPTORS: &[Descriptor] = &[
    color_desc("colors.background", "Background", &["settings", "overlay"]),
    color_desc("colors.surface", "Surface", &["settings", "overlay"]),
    color_desc("colors.raised", "Raised", &["settings", "overlay"]),
    color_desc("colors.text", "Text", &["settings", "overlay"]),
    color_desc("colors.muted", "Muted text", &["settings", "overlay"]),
    color_desc("colors.border", "Border", &["settings", "overlay"]),
    color_desc("colors.accent", "Accent", &["settings", "overlay"]),
    color_desc("colors.on_accent", "On accent", &["settings"]),
    Descriptor {
        id: "colors.secondary",
        label: "Secondary",
        category: "colors",
        unit: "color",
        min: None,
        max: None,
        step: None,
        looks: &[Look::Neon],
        surfaces: &["settings", "overlay"],
    },
    color_desc("colors.state.pending", "Pending", &["settings", "overlay"]),
    color_desc("colors.state.done", "Done", &["settings", "overlay"]),
    color_desc("colors.state.failed", "Failed", &["settings", "overlay"]),
    Descriptor {
        id: "colors.state.live",
        label: "Live",
        category: "colors",
        unit: "color",
        min: None,
        max: None,
        step: None,
        looks: ALL,
        surfaces: &["overlay"],
    },
    Descriptor {
        id: "colors.state.working",
        label: "Working",
        category: "colors",
        unit: "color",
        min: None,
        max: None,
        step: None,
        looks: ALL,
        surfaces: &["overlay"],
    },
    Descriptor {
        id: "colors.state.quiet",
        label: "Quiet",
        category: "colors",
        unit: "color",
        min: None,
        max: None,
        step: None,
        looks: ALL,
        surfaces: &["overlay"],
    },
    Descriptor {
        id: "font.family",
        label: "Font",
        category: "font",
        unit: "text",
        min: Some(1.0),
        max: Some(64.0),
        step: None,
        looks: ALL,
        surfaces: &["settings", "overlay"],
    },
    Descriptor {
        id: "font.mono_family",
        label: "Monospace font",
        category: "font",
        unit: "text",
        min: Some(1.0),
        max: Some(64.0),
        step: None,
        looks: ALL,
        surfaces: &["settings", "overlay"],
    },
    number_desc(
        "font.scale",
        "Font scale",
        "font",
        0.85,
        1.25,
        0.05,
        ALL,
        &["settings", "overlay"],
    ),
    number_desc(
        "font.weight",
        "Label weight",
        "font",
        400.0,
        900.0,
        100.0,
        ALL,
        &["settings", "overlay"],
    ),
    number_desc(
        "font.tracking",
        "Label spacing",
        "font",
        0.0,
        0.4,
        0.01,
        ALL,
        &["settings", "overlay"],
    ),
    number_desc(
        "spacing.scale",
        "Spacing",
        "spacing",
        0.75,
        1.5,
        0.05,
        ALL,
        &["settings", "overlay"],
    ),
    number_desc(
        "shape.radius",
        "Corner radius",
        "shape",
        0.0,
        12.0,
        1.0,
        ALL,
        &["settings", "overlay"],
    ),
    number_desc(
        "shape.chamfer",
        "Chamfer",
        "shape",
        0.0,
        12.0,
        1.0,
        MODULES,
        &["settings", "overlay"],
    ),
    number_desc(
        "border.width",
        "Border width",
        "border",
        0.0,
        3.0,
        1.0,
        &[Look::Neon, Look::Holo],
        &["settings", "overlay"],
    ),
    Descriptor {
        id: "border.width",
        label: "Bezel width",
        category: "border",
        unit: "number",
        min: Some(2.0),
        max: Some(6.0),
        step: Some(1.0),
        looks: &[Look::Gunmetal],
        surfaces: &["settings", "overlay"],
    },
    number_desc(
        "surface.opacity",
        "Surface opacity",
        "surface",
        0.6,
        1.0,
        0.05,
        ALL,
        &["settings", "overlay"],
    ),
    number_desc(
        "glow.strength",
        "Glow",
        "glow",
        0.0,
        1.5,
        0.05,
        ALL,
        &["settings", "overlay"],
    ),
    number_desc(
        "decoration.mesh",
        "Hex mesh",
        "decoration",
        0.0,
        0.15,
        0.01,
        MODULES,
        &["settings", "overlay"],
    ),
    toggle_desc("decoration.scanlines", "Scanlines", HOLO, &["overlay"]),
    toggle_desc(
        "decoration.corners",
        "Corner brackets",
        HOLO,
        &["settings", "overlay"],
    ),
    toggle_desc("motion.shine", "Shine", HOLO, &["overlay"]),
    toggle_desc("motion.pulse", "Pulse", ALL, &["settings", "overlay"]),
];

const fn color_desc(
    id: &'static str,
    label: &'static str,
    surfaces: &'static [&'static str],
) -> Descriptor {
    Descriptor {
        id,
        label,
        category: "colors",
        unit: "color",
        min: None,
        max: None,
        step: None,
        looks: ALL,
        surfaces,
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "one argument per descriptor field"
)]
const fn number_desc(
    id: &'static str,
    label: &'static str,
    category: &'static str,
    min: f32,
    max: f32,
    step: f32,
    looks: &'static [Look],
    surfaces: &'static [&'static str],
) -> Descriptor {
    Descriptor {
        id,
        label,
        category,
        unit: "number",
        min: Some(min),
        max: Some(max),
        step: Some(step),
        looks,
        surfaces,
    }
}

const fn toggle_desc(
    id: &'static str,
    label: &'static str,
    looks: &'static [Look],
    surfaces: &'static [&'static str],
) -> Descriptor {
    Descriptor {
        id,
        label,
        category: match id.as_bytes()[0] {
            b'd' => "decoration",
            _ => "motion",
        },
        unit: "toggle",
        min: None,
        max: None,
        step: None,
        looks,
        surfaces,
    }
}

/// One scheme of a look, fully resolved.
#[derive(Clone, Debug, PartialEq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is its own theme property"
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

/// A look resolved for the schemes it has.
#[derive(Clone, Debug, PartialEq)]
pub struct Style {
    look: Look,
    pub dark: Scheme,
    pub light: Option<Scheme>,
}

impl Style {
    #[must_use]
    pub const fn look(&self) -> Look {
        self.look
    }

    /// The active palette, falling back to the look's only scheme.
    #[must_use]
    pub fn scheme(&self, dark: bool) -> &Scheme {
        if dark {
            &self.dark
        } else {
            self.light.as_ref().unwrap_or(&self.dark)
        }
    }

    /// Built-in defaults with `overrides` on top. Suspended or unreadable changes draw the defaults.
    #[must_use]
    pub fn resolve(look: Look, overrides: Option<&LookOverrides>) -> Self {
        let usable = match overrides {
            Some(LookOverrides::Usable(overrides)) if !overrides.suspended => Some(overrides),
            _ => None,
        };
        let defaults = defaults_of(look);
        Self {
            look,
            dark: apply(&defaults.dark, usable.map(|item| &item.colors.dark), usable),
            light: defaults.light.as_ref().map(|light| {
                apply(
                    light,
                    usable.and_then(|item| item.colors.light.as_ref()),
                    usable,
                )
            }),
        }
    }
}

fn apply(base: &Scheme, colors: Option<&SchemeColors>, rest: Option<&Overrides>) -> Scheme {
    let pick = |chosen: Option<Color>, fallback: Color| chosen.unwrap_or(fallback);
    let colors = colors.cloned().unwrap_or_default();
    let mut scheme = Scheme {
        background: pick(colors.background, base.background),
        surface: pick(colors.surface, base.surface),
        raised: pick(colors.raised, base.raised),
        text: pick(colors.text, base.text),
        muted: pick(colors.muted, base.muted),
        border: pick(colors.border, base.border),
        accent: pick(colors.accent, base.accent),
        on_accent: pick(colors.on_accent, base.on_accent),
        secondary: pick(colors.secondary, base.secondary),
        pending: pick(colors.pending, base.pending),
        done: pick(colors.done, base.done),
        failed: pick(colors.failed, base.failed),
        live: pick(colors.live, base.live),
        working: pick(colors.working, base.working),
        quiet: pick(colors.quiet, base.quiet),
        ..base.clone()
    };
    if let Some(rest) = rest {
        if let Some(family) = &rest.font.family {
            family.as_str().clone_into(&mut scheme.font_family);
        }
        if let Some(family) = &rest.font.mono_family {
            family.as_str().clone_into(&mut scheme.mono_family);
        }
        if let Some(scale) = rest.font.scale {
            scheme.font_scale = scale.get();
        }
        if let Some(weight) = rest.font.weight {
            scheme.font_weight = weight.get();
        }
        if let Some(tracking) = rest.font.tracking {
            scheme.tracking = tracking.get();
        }
        if let Some(scale) = rest.spacing.scale {
            scheme.spacing = scale.get();
        }
        if let Some(radius) = rest.shape.radius {
            scheme.radius = radius.get();
        }
        if let Some(chamfer) = rest.shape.chamfer {
            scheme.chamfer = chamfer.get();
        }
        if let Some(width) = rest.border.width {
            scheme.border_width = width.get();
        }
        if let Some(opacity) = rest.surface.opacity {
            scheme.opacity = opacity.get();
        }
        if let Some(strength) = rest.glow.strength {
            scheme.glow = strength.get();
        }
        if let Some(mesh) = rest.decoration.mesh {
            scheme.mesh = mesh.get();
        }
        if let Some(on) = rest.decoration.scanlines {
            scheme.scanlines = on;
        }
        if let Some(on) = rest.decoration.corners {
            scheme.corners = on;
        }
        if let Some(on) = rest.motion.shine {
            scheme.shine = on;
        }
        if let Some(on) = rest.motion.pulse {
            scheme.pulse = on;
        }
    }
    scheme
}

/// Built-in colours and measures for `look`, copied from the overlay and Settings literals.
#[must_use]
pub fn defaults_of(look: Look) -> Style {
    match look {
        Look::Neon => Style {
            look,
            dark: neon_dark(),
            light: Some(neon_light()),
        },
        Look::Holo => Style {
            look,
            dark: holo(),
            light: None,
        },
        Look::Gunmetal => Style {
            look,
            dark: gunmetal(),
            light: None,
        },
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
        accent: must("#6bf8ff"),
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
        accent: must("#6bf8ff"),
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

/// Reads one look. A property that does not apply, or a leaf out of range, keeps the original JSON.
fn read_look(look: Look, value: &Value, path: &str) -> LookOverrides {
    match read_usable(look, value, path) {
        Ok(overrides) => LookOverrides::Usable(overrides),
        Err(reason) => LookOverrides::Unreadable {
            raw: value.clone(),
            reason,
        },
    }
}

fn read_usable(look: Look, value: &Value, path: &str) -> Result<Overrides, String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{path}: use an object"))?;
    let mut overrides = Overrides::default();
    for (key, value) in object {
        let here = format!("{path}.{key}");
        match key.as_str() {
            "suspended" => overrides.suspended = flag(value, &here)?,
            "colors" => overrides.colors = read_colors(look, value, &here)?,
            "font" => overrides.font = read_font(value, &here)?,
            "spacing" => overrides.spacing = read_spacing(value, &here)?,
            "shape" => overrides.shape = read_shape(look, value, &here)?,
            "border" => overrides.border = read_border(look, value, &here)?,
            "surface" => overrides.surface = read_surface(value, &here)?,
            "glow" => overrides.glow = read_glow(value, &here)?,
            "decoration" => overrides.decoration = read_decoration(look, value, &here)?,
            "motion" => overrides.motion = read_motion(look, value, &here)?,
            _ => {
                overrides.unknown.insert(key.clone(), value.clone());
            }
        }
    }
    check_contrast(look, &overrides)?;
    Ok(overrides)
}

fn flag(value: &Value, path: &str) -> Result<bool, String> {
    value
        .as_bool()
        .ok_or_else(|| format!("{path}: use true or false"))
}

fn object<'a>(value: &'a Value, path: &str) -> Result<&'a Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{path}: use an object"))
}

fn read_colors(look: Look, value: &Value, path: &str) -> Result<ColorOverrides, String> {
    let mut colors = ColorOverrides::default();
    for (key, value) in object(value, path)? {
        let here = format!("{path}.{key}");
        match key.as_str() {
            "dark" => colors.dark = read_scheme(look, value, &here)?,
            "light" => {
                if look != Look::Neon {
                    return Err(format!(
                        "{here}: {look_name} has no light scheme",
                        look_name = title(look)
                    ));
                }
                colors.light = Some(read_scheme(look, value, &here)?);
            }
            _ => return Err(format!("{here}: unknown colour group")),
        }
    }
    Ok(colors)
}

fn title(look: Look) -> &'static str {
    look_title(look.name())
}

fn read_scheme(look: Look, value: &Value, path: &str) -> Result<SchemeColors, String> {
    let mut scheme = SchemeColors::default();
    for (key, value) in object(value, path)? {
        match key.as_str() {
            "state" => read_state(value, &mut scheme, &format!("{path}.state"))?,
            "background" => {
                scheme.background = Some(leaf_color(value, &format!("{path}.background"))?);
            }
            "surface" => scheme.surface = Some(leaf_color(value, &format!("{path}.surface"))?),
            "raised" => scheme.raised = Some(leaf_color(value, &format!("{path}.raised"))?),
            "text" => scheme.text = Some(leaf_color(value, &format!("{path}.text"))?),
            "muted" => scheme.muted = Some(leaf_color(value, &format!("{path}.muted"))?),
            "border" => scheme.border = Some(leaf_color(value, &format!("{path}.border"))?),
            "accent" => scheme.accent = Some(leaf_color(value, &format!("{path}.accent"))?),
            "on_accent" => {
                scheme.on_accent = Some(leaf_color(value, &format!("{path}.on_accent"))?);
            }
            "secondary" => {
                if look != Look::Neon {
                    return Err(format!(
                        "{path}.secondary: {look_name} has no secondary colour",
                        look_name = title(look)
                    ));
                }
                scheme.secondary = Some(leaf_color(value, &format!("{path}.secondary"))?);
            }
            _ => {
                return Err(format!("{path}.{key}: unknown colour"));
            }
        }
    }
    Ok(scheme)
}

fn read_state(value: &Value, scheme: &mut SchemeColors, path: &str) -> Result<(), String> {
    for (key, value) in object(value, path)? {
        let here = format!("{path}.{key}");
        let color = leaf_color(value, &here)?;
        match key.as_str() {
            "pending" => scheme.pending = Some(color),
            "done" => scheme.done = Some(color),
            "failed" => scheme.failed = Some(color),
            "live" => scheme.live = Some(color),
            "working" => scheme.working = Some(color),
            "quiet" => scheme.quiet = Some(color),
            _ => return Err(format!("{here}: unknown state colour")),
        }
    }
    Ok(())
}

fn leaf_color(value: &Value, path: &str) -> Result<Color, String> {
    let text = value
        .as_str()
        .ok_or_else(|| format!("{path}: use a colour like #6bf8ff"))?;
    Color::parse(text).map_err(|reason| format!("{path}: {reason}"))
}

fn read_font(value: &Value, path: &str) -> Result<FontOverrides, String> {
    let mut font = FontOverrides::default();
    for (key, value) in object(value, path)? {
        let here = format!("{path}.{key}");
        match key.as_str() {
            "family" => font.family = Some(leaf_family(value, &here)?),
            "mono_family" => font.mono_family = Some(leaf_family(value, &here)?),
            "scale" => font.scale = Some(leaf_scale(value, &here, 0.85, 1.25)?),
            "weight" => font.weight = Some(leaf_weight(value, &here)?),
            "tracking" => font.tracking = Some(leaf_tracking(value, &here)?),
            _ => return Err(format!("{here}: unknown font property")),
        }
    }
    Ok(font)
}

fn leaf_family(value: &Value, path: &str) -> Result<FontFamily, String> {
    let text = value.as_str().ok_or_else(|| {
        format!("{path}: use 1–64 characters, with no quotes, backslash, semicolon or braces")
    })?;
    FontFamily::parse(text).map_err(|reason| format!("{path}: {reason}"))
}

fn number(value: &Value, path: &str) -> Result<f64, String> {
    value
        .as_f64()
        .filter(|value| value.is_finite())
        .ok_or_else(|| format!("{path}: use a number"))
}

fn leaf_scale(value: &Value, path: &str, min: f32, max: f32) -> Result<Scale, String> {
    Scale::parse(number(value, path)?, min, max).map_err(|reason| format!("{path}: {reason}"))
}

fn leaf_weight(value: &Value, path: &str) -> Result<Weight, String> {
    let value = number(value, path)?;
    if value.fract() != 0.0 || value < 0.0 {
        return Err(format!(
            "{path}: use a weight from 400 to 900, in steps of 100"
        ));
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "checked non-negative and integral; a value past u64 saturates, which parse rejects"
    )]
    #[expect(clippy::cast_sign_loss, reason = "checked non-negative")]
    Weight::parse(value as u64).map_err(|reason| format!("{path}: {reason}"))
}

fn leaf_tracking(value: &Value, path: &str) -> Result<Tracking, String> {
    Tracking::parse(number(value, path)?).map_err(|reason| format!("{path}: {reason}"))
}

fn read_spacing(value: &Value, path: &str) -> Result<SpacingOverrides, String> {
    let mut spacing = SpacingOverrides::default();
    for (key, value) in object(value, path)? {
        let here = format!("{path}.{key}");
        if key == "scale" {
            spacing.scale = Some(leaf_scale(value, &here, 0.75, 1.5)?);
        } else {
            return Err(format!("{here}: unknown spacing property"));
        }
    }
    Ok(spacing)
}

fn read_shape(look: Look, value: &Value, path: &str) -> Result<ShapeOverrides, String> {
    let mut shape = ShapeOverrides::default();
    for (key, value) in object(value, path)? {
        let here = format!("{path}.{key}");
        match key.as_str() {
            "radius" => shape.radius = Some(leaf_px(value, &here, 0, 12)?),
            "chamfer" => {
                if look == Look::Neon {
                    return Err(format!("{here}: Neon has no chamfer"));
                }
                shape.chamfer = Some(leaf_px(value, &here, 0, 12)?);
            }
            _ => return Err(format!("{here}: unknown shape property")),
        }
    }
    Ok(shape)
}

fn leaf_px(value: &Value, path: &str, min: u8, max: u8) -> Result<Px, String> {
    let value = number(value, path)?;
    if value.fract() != 0.0 || value < 0.0 {
        return Err(format!("{path}: use a whole number from {min} to {max}"));
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "checked non-negative and integral; a value past u64 saturates, which parse rejects"
    )]
    #[expect(clippy::cast_sign_loss, reason = "checked non-negative")]
    Px::parse(value as u64, min, max).map_err(|reason| format!("{path}: {reason}"))
}

fn read_border(look: Look, value: &Value, path: &str) -> Result<BorderOverrides, String> {
    let mut border = BorderOverrides::default();
    let (min, max) = if look == Look::Gunmetal {
        (2, 6)
    } else {
        (0, 3)
    };
    for (key, value) in object(value, path)? {
        let here = format!("{path}.{key}");
        if key == "width" {
            border.width = Some(leaf_px(value, &here, min, max)?);
        } else {
            return Err(format!("{here}: unknown border property"));
        }
    }
    Ok(border)
}

fn read_surface(value: &Value, path: &str) -> Result<SurfaceOverrides, String> {
    let mut surface = SurfaceOverrides::default();
    for (key, value) in object(value, path)? {
        let here = format!("{path}.{key}");
        if key == "opacity" {
            surface.opacity = Some(
                Opacity::parse(number(value, &here)?)
                    .map_err(|reason| format!("{here}: {reason}"))?,
            );
        } else {
            return Err(format!("{here}: unknown surface property"));
        }
    }
    Ok(surface)
}

fn read_glow(value: &Value, path: &str) -> Result<GlowOverrides, String> {
    let mut glow = GlowOverrides::default();
    for (key, value) in object(value, path)? {
        let here = format!("{path}.{key}");
        if key == "strength" {
            glow.strength = Some(
                Strength::parse(number(value, &here)?)
                    .map_err(|reason| format!("{here}: {reason}"))?,
            );
        } else {
            return Err(format!("{here}: unknown glow property"));
        }
    }
    Ok(glow)
}

fn read_decoration(look: Look, value: &Value, path: &str) -> Result<DecorationOverrides, String> {
    let mut decoration = DecorationOverrides::default();
    for (key, value) in object(value, path)? {
        let here = format!("{path}.{key}");
        match key.as_str() {
            "mesh" => {
                if look == Look::Neon {
                    return Err(format!("{here}: Neon has no hex mesh"));
                }
                decoration.mesh = Some(
                    Mesh::parse(number(value, &here)?)
                        .map_err(|reason| format!("{here}: {reason}"))?,
                );
            }
            "scanlines" => {
                if look != Look::Holo {
                    return Err(format!("{here}: only Holo has scanlines"));
                }
                decoration.scanlines = Some(flag(value, &here)?);
            }
            "corners" => {
                if look != Look::Holo {
                    return Err(format!("{here}: only Holo has corner brackets"));
                }
                decoration.corners = Some(flag(value, &here)?);
            }
            _ => return Err(format!("{here}: unknown decoration")),
        }
    }
    Ok(decoration)
}

fn read_motion(look: Look, value: &Value, path: &str) -> Result<MotionOverrides, String> {
    let mut motion = MotionOverrides::default();
    for (key, value) in object(value, path)? {
        let here = format!("{path}.{key}");
        match key.as_str() {
            "shine" => {
                if look != Look::Holo {
                    return Err(format!("{here}: only Holo has a shine"));
                }
                motion.shine = Some(flag(value, &here)?);
            }
            "pulse" => motion.pulse = Some(flag(value, &here)?),
            _ => return Err(format!("{here}: unknown motion")),
        }
    }
    Ok(motion)
}

/// Refuses a scheme whose text or accent falls below the readability floor.
fn check_contrast(look: Look, overrides: &Overrides) -> Result<(), String> {
    let defaults = defaults_of(look);
    for (name, base, colors) in [
        ("dark", &defaults.dark, Some(&overrides.colors.dark)),
        (
            "light",
            defaults.light.as_ref().unwrap_or(&defaults.dark),
            overrides.colors.light.as_ref(),
        ),
    ] {
        if colors.is_none() && name == "light" {
            continue;
        }
        let scheme = apply(base, colors, Some(overrides));
        let path = format!("themes.{}.colors.{name}", look.name());
        contrast_at(
            &path,
            "text on background",
            scheme.text,
            scheme.background,
            TEXT_CONTRAST,
        )?;
        contrast_at(
            &path,
            "text on surface",
            scheme.text,
            scheme.surface,
            TEXT_CONTRAST,
        )?;
        contrast_at(
            &path,
            "accent on raised",
            scheme.accent,
            scheme.raised,
            ACCENT_CONTRAST,
        )?;
        contrast_at(
            &path,
            "focus on background",
            scheme.accent,
            scheme.background,
            ACCENT_CONTRAST,
        )?;
    }
    Ok(())
}

fn contrast_at(
    path: &str,
    what: &str,
    foreground: Color,
    background: Color,
    floor: f32,
) -> Result<(), String> {
    let ratio = contrast(foreground, background);
    if ratio + 0.001 < floor {
        Err(format!(
            "{path}: {what} is {ratio:.2}:1, below the {floor:.1}:1 minimum"
        ))
    } else {
        Ok(())
    }
}

/// WCAG contrast of the two colours blended over black, ignoring alpha's pair and using each colour's own alpha.
fn contrast(foreground: Color, background: Color) -> f32 {
    let lighter = luminance(foreground).max(luminance(background));
    let darker = luminance(foreground).min(luminance(background));
    (lighter + 0.05) / (darker + 0.05)
}

fn luminance(color: Color) -> f32 {
    let [r, g, b, a] = color.channels();
    let alpha = f32::from(a) / 255.0;
    let channel = |value: u8| {
        let value = f32::from(value) / 255.0 * alpha;
        if value <= 0.03928 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}

fn look_to_value(overrides: &LookOverrides) -> Value {
    match overrides {
        LookOverrides::Unreadable { raw, .. } => raw.clone(),
        LookOverrides::Usable(overrides) => usable_to_value(overrides),
    }
}

fn usable_to_value(overrides: &Overrides) -> Value {
    let mut object = Map::new();
    if overrides.suspended {
        object.insert("suspended".to_owned(), Value::Bool(true));
    }
    insert_colors(&mut object, &overrides.colors);
    insert_font(&mut object, &overrides.font);
    if let Some(scale) = overrides.spacing.scale {
        object.insert(
            "spacing".to_owned(),
            Value::Object(Map::from_iter([(
                "scale".to_owned(),
                json_number(f64::from(scale.get())),
            )])),
        );
    }
    insert_shape(&mut object, &overrides.shape);
    if let Some(width) = overrides.border.width {
        object.insert(
            "border".to_owned(),
            Value::Object(Map::from_iter([(
                "width".to_owned(),
                Value::from(u64::from(width.get())),
            )])),
        );
    }
    if let Some(opacity) = overrides.surface.opacity {
        object.insert(
            "surface".to_owned(),
            Value::Object(Map::from_iter([(
                "opacity".to_owned(),
                json_number(f64::from(opacity.get())),
            )])),
        );
    }
    if let Some(strength) = overrides.glow.strength {
        object.insert(
            "glow".to_owned(),
            Value::Object(Map::from_iter([(
                "strength".to_owned(),
                json_number(f64::from(strength.get())),
            )])),
        );
    }
    insert_decoration(&mut object, &overrides.decoration);
    insert_motion(&mut object, &overrides.motion);
    for (key, value) in &overrides.unknown {
        object.insert(key.clone(), value.clone());
    }
    Value::Object(object)
}

fn json_number(value: f64) -> Value {
    serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
}

fn insert_colors(object: &mut Map<String, Value>, colors: &ColorOverrides) {
    let mut schemes = Map::new();
    let dark = scheme_value(&colors.dark);
    if !dark.as_object().is_some_and(Map::is_empty) {
        schemes.insert("dark".to_owned(), dark);
    }
    if let Some(light) = &colors.light {
        let light = scheme_value(light);
        if !light.as_object().is_some_and(Map::is_empty) {
            schemes.insert("light".to_owned(), light);
        }
    }
    if !schemes.is_empty() {
        object.insert("colors".to_owned(), Value::Object(schemes));
    }
}

fn scheme_value(scheme: &SchemeColors) -> Value {
    let mut object = Map::new();
    let mut put = |key: &str, color: Option<Color>| {
        if let Some(color) = color {
            object.insert(key.to_owned(), Value::String(color.to_hex()));
        }
    };
    put("background", scheme.background);
    put("surface", scheme.surface);
    put("raised", scheme.raised);
    put("text", scheme.text);
    put("muted", scheme.muted);
    put("border", scheme.border);
    put("accent", scheme.accent);
    put("on_accent", scheme.on_accent);
    put("secondary", scheme.secondary);
    let mut state = Map::new();
    let mut put_state = |key: &str, color: Option<Color>| {
        if let Some(color) = color {
            state.insert(key.to_owned(), Value::String(color.to_hex()));
        }
    };
    put_state("pending", scheme.pending);
    put_state("done", scheme.done);
    put_state("failed", scheme.failed);
    put_state("live", scheme.live);
    put_state("working", scheme.working);
    put_state("quiet", scheme.quiet);
    if !state.is_empty() {
        object.insert("state".to_owned(), Value::Object(state));
    }
    Value::Object(object)
}

fn insert_font(object: &mut Map<String, Value>, font: &FontOverrides) {
    let mut stored = Map::new();
    if let Some(family) = &font.family {
        stored.insert(
            "family".to_owned(),
            Value::String(family.as_str().to_owned()),
        );
    }
    if let Some(family) = &font.mono_family {
        stored.insert(
            "mono_family".to_owned(),
            Value::String(family.as_str().to_owned()),
        );
    }
    if let Some(scale) = font.scale {
        stored.insert("scale".to_owned(), json_number(f64::from(scale.get())));
    }
    if let Some(weight) = font.weight {
        stored.insert("weight".to_owned(), Value::from(u64::from(weight.get())));
    }
    if let Some(tracking) = font.tracking {
        stored.insert(
            "tracking".to_owned(),
            json_number(f64::from(tracking.get())),
        );
    }
    if !stored.is_empty() {
        object.insert("font".to_owned(), Value::Object(stored));
    }
}

fn insert_shape(object: &mut Map<String, Value>, shape: &ShapeOverrides) {
    let mut stored = Map::new();
    if let Some(radius) = shape.radius {
        stored.insert("radius".to_owned(), Value::from(u64::from(radius.get())));
    }
    if let Some(chamfer) = shape.chamfer {
        stored.insert("chamfer".to_owned(), Value::from(u64::from(chamfer.get())));
    }
    if !stored.is_empty() {
        object.insert("shape".to_owned(), Value::Object(stored));
    }
}

fn insert_decoration(object: &mut Map<String, Value>, decoration: &DecorationOverrides) {
    let mut stored = Map::new();
    if let Some(mesh) = decoration.mesh {
        stored.insert("mesh".to_owned(), json_number(f64::from(mesh.get())));
    }
    if let Some(on) = decoration.scanlines {
        stored.insert("scanlines".to_owned(), Value::Bool(on));
    }
    if let Some(on) = decoration.corners {
        stored.insert("corners".to_owned(), Value::Bool(on));
    }
    if !stored.is_empty() {
        object.insert("decoration".to_owned(), Value::Object(stored));
    }
}

fn insert_motion(object: &mut Map<String, Value>, motion: &MotionOverrides) {
    let mut stored = Map::new();
    if let Some(on) = motion.shine {
        stored.insert("shine".to_owned(), Value::Bool(on));
    }
    if let Some(on) = motion.pulse {
        stored.insert("pulse".to_owned(), Value::Bool(on));
    }
    if !stored.is_empty() {
        object.insert("motion".to_owned(), Value::Object(stored));
    }
}

/// The settings form of every look: usable changes as stored, unreadable ones as `{ "unreadable": reason }`.
/// A preserved value is the original JSON, so a save that does not edit it writes that JSON back.
#[must_use]
pub fn form_of_themes(themes: &Themes) -> Value {
    if let Some((preserved, _)) = &themes.preserved {
        return preserved.clone();
    }
    let mut object = Map::new();
    object.insert("version".to_owned(), Value::from(themes.version));
    for (key, value) in &themes.unknown {
        object.insert(key.clone(), value.clone());
    }
    for (look, overrides) in &themes.looks {
        object.insert(look.name().to_owned(), form_of(overrides));
    }
    Value::Object(object)
}

/// The settings form of one look: usable changes as stored, or `{ "unreadable": reason }` with the raw JSON withheld.
#[must_use]
pub fn form_of(overrides: &LookOverrides) -> Value {
    match overrides {
        LookOverrides::Usable(overrides) => usable_to_value(overrides),
        LookOverrides::Unreadable { reason, .. } => Value::Object(Map::from_iter([(
            "unreadable".to_owned(),
            Value::String(reason.clone()),
        )])),
    }
}

/// Refuses `themes` when it differs from `current` and a look is not usable.
///
/// # Errors
/// The path and the plain reason, when a changed look cannot be used.
pub fn refuse_changed(current: &Themes, next: &Value) -> Result<(), String> {
    if current.to_value() == *next {
        return Ok(());
    }
    let Some(object) = next.as_object() else {
        return Err("themes must be an object".to_owned());
    };
    match object.get("version").and_then(Value::as_u64) {
        Some(version) if version == u64::from(VERSION) => {}
        Some(_) | None => return Err(format!("themes.version must be {VERSION}")),
    }
    let loaded = Themes::load(next);
    if loaded.preserved.is_some() {
        return Err(loaded
            .notices()
            .into_iter()
            .next()
            .unwrap_or_else(|| format!("themes.version must be {VERSION}")));
    }
    for (look, overrides) in &loaded.looks {
        if let LookOverrides::Unreadable { reason, .. } = overrides {
            let previous = current.of(*look);
            let same = match previous {
                Some(LookOverrides::Unreadable { raw, .. }) => {
                    next.get(look.name()).is_some_and(|value| value == raw)
                }
                _ => false,
            };
            if !same {
                return Err(reason.clone());
            }
        }
    }
    Ok(())
}

/// Rewrites a v1 themes object as v2 would, for the evolution fixture. Production stays at v1.
#[must_use]
pub fn migrate_v1(value: &Value) -> Value {
    let mut next = value.clone();
    if let Some(object) = next.as_object_mut() {
        object.insert("version".to_owned(), Value::from(2_u64));
        // v2 adds `glow.softness`. A stored glow stays; the new property is not invented.
    }
    next
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

    fn parse(name: &str) -> Option<Self> {
        match name {
            "neon" => Some(Self::Neon),
            "holo" => Some(Self::Holo),
            "gunmetal" => Some(Self::Gunmetal),
            _ => None,
        }
    }
}

#[cfg(test)]
#[expect(clippy::too_many_lines, reason = "one assertion per theme case")]
mod tests {
    use super::*;

    fn themes(json: &str) -> Themes {
        Themes::load(&serde_json::from_str(json).unwrap())
    }

    #[test]
    fn built_in_defaults_match_the_overlay_and_settings_literals() {
        let neon = defaults_of(Look::Neon);
        assert_eq!(neon.dark.background.to_hex(), "#020617");
        assert_eq!(neon.light.as_ref().unwrap().background.to_hex(), "#e8eff7");
        assert_eq!(neon.dark.accent.to_hex(), "#2bff88");
        assert_eq!(neon.light.as_ref().unwrap().on_accent.to_hex(), "#ffffff");
        let holo = defaults_of(Look::Holo);
        assert_eq!(holo.dark.accent.to_hex(), "#6bf8ff");
        assert_eq!(holo.dark.border.to_hex(), "#5d92b4");
        assert_eq!(holo.dark.quiet.to_hex(), "#8fb3c4");
        assert!(holo.dark.scanlines && holo.dark.corners && holo.dark.shine);
        assert_eq!(holo.dark.chamfer, 7);
        let gunmetal = defaults_of(Look::Gunmetal);
        assert_eq!(gunmetal.dark.raised.to_hex(), "#5c5c60");
        assert_eq!(gunmetal.dark.border_width, 5);
        assert_eq!(gunmetal.dark.chamfer, 9);
        assert_eq!(gunmetal.dark.mesh, 0.06);
    }

    fn usable(look: Look, json: &str) -> Overrides {
        match themes(json).of(look) {
            Some(LookOverrides::Usable(overrides)) => overrides.clone(),
            other => panic!("expected usable {look:?}, got {other:?} from {json}"),
        }
    }

    fn refused(look: Look, json: &str) -> String {
        match themes(json).of(look) {
            Some(LookOverrides::Unreadable { reason, .. }) => reason.clone(),
            other => panic!("expected unreadable {look:?}, got {other:?} from {json}"),
        }
    }

    #[test]
    fn leaves_accept_their_bounds_and_refuse_injection_and_what_does_not_apply() {
        let cases: &[(&str, Look, bool, &str)] = &[
            (
                r##"{"version":1,"holo":{"colors":{"dark":{"accent":"#6bf8ff"}}}}"##,
                Look::Holo,
                true,
                "",
            ),
            (
                r##"{"version":1,"holo":{"colors":{"dark":{"accent":"#6bf8ff80"}}}}"##,
                Look::Holo,
                true,
                "",
            ),
            (
                r#"{"version":1,"holo":{"colors":{"dark":{"accent":"red"}}}}"#,
                Look::Holo,
                false,
                "themes.holo.colors.dark.accent: use a colour like #6bf8ff",
            ),
            (
                r##"{"version":1,"holo":{"colors":{"dark":{"accent":"#6bf8ff;background:url(x)"}}}}"##,
                Look::Holo,
                false,
                "themes.holo.colors.dark.accent: use a colour like #6bf8ff",
            ),
            (
                r#"{"version":1,"neon":{"font":{"family":"Segoe UI"}}}"#,
                Look::Neon,
                true,
                "",
            ),
            (
                r#"{"version":1,"neon":{"font":{"family":"Segoe UI\";}body{background:url(x)"}}}"#,
                Look::Neon,
                false,
                "themes.neon.font.family: use 1–64 characters",
            ),
            (
                r#"{"version":1,"neon":{"font":{"family":""}}}"#,
                Look::Neon,
                false,
                "themes.neon.font.family: use 1–64 characters",
            ),
            (
                r#"{"version":1,"neon":{"font":{"scale":0.85}}}"#,
                Look::Neon,
                true,
                "",
            ),
            (
                r#"{"version":1,"neon":{"font":{"scale":1.25}}}"#,
                Look::Neon,
                true,
                "",
            ),
            (
                r#"{"version":1,"neon":{"font":{"scale":1.26}}}"#,
                Look::Neon,
                false,
                "themes.neon.font.scale: use a number from 0.85 to 1.25",
            ),
            (
                r#"{"version":1,"neon":{"font":{"scale":0.84}}}"#,
                Look::Neon,
                false,
                "themes.neon.font.scale: use a number from 0.85 to 1.25",
            ),
            (
                r#"{"version":1,"holo":{"spacing":{"scale":0.75}}}"#,
                Look::Holo,
                true,
                "",
            ),
            (
                r#"{"version":1,"holo":{"spacing":{"scale":1.51}}}"#,
                Look::Holo,
                false,
                "themes.holo.spacing.scale: use a number from 0.75 to 1.5",
            ),
            (
                r#"{"version":1,"neon":{"shape":{"radius":0}}}"#,
                Look::Neon,
                true,
                "",
            ),
            (
                r#"{"version":1,"neon":{"shape":{"radius":13}}}"#,
                Look::Neon,
                false,
                "themes.neon.shape.radius: use a whole number from 0 to 12",
            ),
            (
                r#"{"version":1,"neon":{"shape":{"chamfer":4}}}"#,
                Look::Neon,
                false,
                "themes.neon.shape.chamfer: Neon has no chamfer",
            ),
            (
                r#"{"version":1,"gunmetal":{"shape":{"chamfer":12}}}"#,
                Look::Gunmetal,
                true,
                "",
            ),
            (
                r#"{"version":1,"holo":{"border":{"width":3}}}"#,
                Look::Holo,
                true,
                "",
            ),
            (
                r#"{"version":1,"holo":{"border":{"width":4}}}"#,
                Look::Holo,
                false,
                "themes.holo.border.width: use a whole number from 0 to 3",
            ),
            (
                r#"{"version":1,"gunmetal":{"border":{"width":2}}}"#,
                Look::Gunmetal,
                true,
                "",
            ),
            (
                r#"{"version":1,"gunmetal":{"border":{"width":6}}}"#,
                Look::Gunmetal,
                true,
                "",
            ),
            (
                r#"{"version":1,"gunmetal":{"border":{"width":1}}}"#,
                Look::Gunmetal,
                false,
                "themes.gunmetal.border.width: use a whole number from 2 to 6",
            ),
            (
                r#"{"version":1,"holo":{"surface":{"opacity":0.6}}}"#,
                Look::Holo,
                true,
                "",
            ),
            (
                r#"{"version":1,"holo":{"surface":{"opacity":0.59}}}"#,
                Look::Holo,
                false,
                "themes.holo.surface.opacity: use a number from 0.6 to 1",
            ),
            (
                r#"{"version":1,"holo":{"glow":{"strength":0}}}"#,
                Look::Holo,
                true,
                "",
            ),
            (
                r#"{"version":1,"holo":{"glow":{"strength":1.5}}}"#,
                Look::Holo,
                true,
                "",
            ),
            (
                r#"{"version":1,"holo":{"glow":{"strength":1.51}}}"#,
                Look::Holo,
                false,
                "themes.holo.glow.strength: use a number from 0 to 1.5",
            ),
            (
                r#"{"version":1,"holo":{"decoration":{"mesh":0.15}}}"#,
                Look::Holo,
                true,
                "",
            ),
            (
                r#"{"version":1,"neon":{"decoration":{"mesh":0.05}}}"#,
                Look::Neon,
                false,
                "themes.neon.decoration.mesh: Neon has no hex mesh",
            ),
            (
                r#"{"version":1,"holo":{"decoration":{"scanlines":false}}}"#,
                Look::Holo,
                true,
                "",
            ),
            (
                r#"{"version":1,"gunmetal":{"decoration":{"scanlines":true}}}"#,
                Look::Gunmetal,
                false,
                "themes.gunmetal.decoration.scanlines: only Holo has scanlines",
            ),
            (
                r##"{"version":1,"gunmetal":{"colors":{"light":{"accent":"#6bf8ff"}}}}"##,
                Look::Gunmetal,
                false,
                "themes.gunmetal.colors.light: Gunmetal has no light scheme",
            ),
            (
                r##"{"version":1,"neon":{"colors":{"light":{"accent":"#007a43"}}}}"##,
                Look::Neon,
                true,
                "",
            ),
            (
                r##"{"version":1,"holo":{"colors":{"dark":{"text":"#111111","background":"#222222"}}}}"##,
                Look::Holo,
                false,
                "text on background",
            ),
            (
                r#"{"version":1,"neon":{"font":{"family":"a;color:red"}}}"#,
                Look::Neon,
                false,
                "semicolon",
            ),
        ];
        for (json, look, ok, needle) in cases {
            let loaded = themes(json);
            match loaded.of(*look) {
                Some(LookOverrides::Usable(_)) => assert!(ok, "{json} should be refused"),
                Some(LookOverrides::Unreadable { reason, .. }) => {
                    assert!(!ok, "{json} should be usable, got {reason}");
                    assert!(reason.contains(needle), "{json}: {reason}");
                }
                None => panic!("{json} produced no look"),
            }
        }
    }

    #[test]
    fn an_absent_property_takes_the_built_in_default_and_an_explicit_one_does_not() {
        let style = Style::resolve(Look::Holo, None);
        assert_eq!(style.dark.accent.to_hex(), "#6bf8ff");
        assert_eq!(style.dark.font_scale, 1.0);
        assert!(style.light.is_none());

        let overrides = usable(
            Look::Holo,
            r##"{"version":1,"holo":{"colors":{"dark":{"accent":"#ffffff"}},"font":{"scale":1.1}}}"##,
        );
        let style = Style::resolve(Look::Holo, Some(&LookOverrides::Usable(overrides)));
        assert_eq!(style.dark.accent.to_hex(), "#ffffff");
        assert_eq!(style.dark.font_scale, 1.1);
        assert_eq!(
            style.dark.text.to_hex(),
            "#dfe9ef",
            "unset text stays the default"
        );
    }

    #[test]
    fn an_unreadable_themes_value_is_kept_and_names_no_look() {
        for (json, notice) in [
            (
                r##"{"version":9,"holo":{"colors":{"dark":{"accent":"#ffffff"}},"future":true}}"##,
                "Your look customizations could not be read (themes.version 9 is newer than this Thalvi).",
            ),
            (
                r#""bad""#,
                "Your look customizations could not be read (themes must be an object).",
            ),
            (
                "[]",
                "Your look customizations could not be read (themes must be an object).",
            ),
            (
                "1",
                "Your look customizations could not be read (themes must be an object).",
            ),
            (
                "true",
                "Your look customizations could not be read (themes must be an object).",
            ),
        ] {
            let loaded = themes(json);
            assert!(loaded.of(Look::Gunmetal).is_none(), "{json}");
            assert!(
                loaded.of(Look::Holo).is_none(),
                "{json}: no look is invented"
            );
            assert!(loaded.of(Look::Neon).is_none(), "{json}");
            assert_eq!(
                loaded.to_value(),
                serde_json::from_str::<Value>(json).unwrap(),
                "{json}"
            );
            assert!(!loaded.is_empty(), "{json}");
            assert_eq!(loaded.notices().len(), 1, "{json}");
            assert_eq!(loaded.notices()[0], notice, "{json}");
            assert!(!loaded.notices()[0].contains("Holo"), "{json}");
            for look in [Look::Neon, Look::Holo, Look::Gunmetal] {
                let style = Style::resolve(look, loaded.of(look));
                assert_eq!(style, Style::resolve(look, None), "{json}");
            }
            let holo = Style::resolve(Look::Holo, loaded.of(Look::Holo));
            assert_eq!(holo.dark.accent.to_hex(), "#6bf8ff", "{json}");
        }
    }

    #[test]
    fn an_empty_themes_object_is_empty() {
        let loaded = themes("{}");
        assert!(loaded.is_empty());
        assert_eq!(loaded.notices(), Vec::<String>::new());
        assert!(loaded.of(Look::Gunmetal).is_none());
    }

    #[test]
    fn a_changed_theme_whose_version_is_not_1_is_refused() {
        let current = themes(r#"{"version":1,"holo":{"font":{"scale":1.1}}}"#);
        for next in [
            r#"{"version":0,"holo":{"font":{"scale":0.85}}}"#,
            r#"{"version":2,"holo":{"font":{"scale":0.85}}}"#,
            r#"{"holo":{"font":{"scale":0.85}}}"#,
        ] {
            let error = refuse_changed(&current, &serde_json::from_str(next).unwrap()).unwrap_err();
            assert_eq!(error, "themes.version must be 1", "{next}");
        }
        let same = current.to_value();
        assert!(refuse_changed(&current, &same).is_ok());
    }

    #[test]
    fn unknown_keys_inside_a_usable_look_are_kept_verbatim() {
        let loaded = themes(
            r##"{"version":1,"holo":{"colors":{"dark":{"accent":"#6bf8ff"}},"from_later":{"x":1}},"note":"kept"}"##,
        );
        let overrides = match loaded.of(Look::Holo) {
            Some(LookOverrides::Usable(overrides)) => overrides,
            other => panic!("{other:?}"),
        };
        assert_eq!(overrides.unknown["from_later"]["x"], 1);
        let written = loaded.to_value();
        assert_eq!(written["holo"]["from_later"]["x"], 1);
        assert_eq!(written["note"], "kept");
    }

    #[test]
    fn a_suspended_look_resolves_to_built_in_defaults_and_keeps_its_overrides() {
        let overrides = usable(
            Look::Holo,
            r##"{"version":1,"holo":{"suspended":true,"colors":{"dark":{"accent":"#ffffff"}}}}"##,
        );
        assert!(overrides.suspended);
        assert_eq!(overrides.colors.dark.accent.unwrap().to_hex(), "#ffffff");
        let style = Style::resolve(Look::Holo, Some(&LookOverrides::Usable(overrides)));
        assert_eq!(style.dark.accent.to_hex(), "#6bf8ff");
    }

    #[test]
    fn migration_keeps_v1_values_and_an_absent_new_property_takes_the_new_default() {
        let v1: Value = serde_json::from_str(
            r##"{"version":1,"holo":{"colors":{"dark":{"accent":"#ffffff"}},"font":{"scale":1.1}}}"##,
        )
        .unwrap();
        let v2 = migrate_v1(&v1);
        assert_eq!(v2["version"], 2);
        assert_eq!(v2["holo"]["colors"]["dark"]["accent"], "#ffffff");
        assert_eq!(v2["holo"]["font"]["scale"], 1.1);
        assert!(
            v2["holo"].get("glow").is_none(),
            "an absent property is not invented"
        );

        // A changed built-in default reaches an unset property and not an explicit one.
        let mut changed = holo();
        changed.glow = 0.9;
        let unset = usable(
            Look::Holo,
            r##"{"version":1,"holo":{"colors":{"dark":{"accent":"#ffffff"}},"font":{"scale":1.1}}}"##,
        );
        let explicit = usable(
            Look::Holo,
            r#"{"version":1,"holo":{"glow":{"strength":0.6},"font":{"scale":1.1}}}"#,
        );
        let reached = apply(&changed, Some(&unset.colors.dark), Some(&unset));
        let kept = apply(&changed, None, Some(&explicit));
        assert_eq!(reached.glow, 0.9, "an unset property takes the new default");
        assert_eq!(reached.font_scale, 1.1, "a stored value is kept");
        assert_eq!(reached.accent.to_hex(), "#ffffff");
        assert_eq!(
            kept.glow, 0.6,
            "an explicit value is not replaced by the new default"
        );
    }

    #[test]
    fn the_notice_names_the_look_and_the_reason() {
        let reason = refused(
            Look::Holo,
            r#"{"version":1,"holo":{"colors":{"dark":{"accent":"red"}}}}"#,
        );
        let notice = unreadable_notice("holo", &reason);
        assert_eq!(
            notice,
            format!(
                "Your Holo customizations could not be read ({reason}). Thalvi uses built-in Holo. The data is kept in desktop.json until you reset Holo."
            )
        );
    }
}
