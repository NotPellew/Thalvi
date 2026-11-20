//! The styles the settings window wears: the look model resolved into CSS custom properties.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};
use thalvi_desktop::config::{DesktopConfig, Look};
use thalvi_desktop::theme::{self, Color, Scheme, Style};

/// The property descriptors and every look's resolved custom properties.
#[must_use]
pub fn response(config: &DesktopConfig) -> Value {
    let mut looks = Map::new();
    for look in [Look::Neon, Look::Holo, Look::Gunmetal] {
        looks.insert(
            look.name().to_owned(),
            Value::Object(tokens(look, config).into_iter().collect()),
        );
    }
    json!({ "properties": properties(), "looks": looks })
}

fn properties() -> Vec<Value> {
    theme::descriptors()
        .iter()
        .map(|property| {
            json!({
                "id": property.id,
                "label": property.label,
                "category": property.category,
                "unit": property.unit,
                "min": property.min,
                "max": property.max,
                "step": property.step,
                "looks": property.looks.iter().map(|look| look.name()).collect::<Vec<_>>(),
                "surfaces": property.surfaces,
            })
        })
        .collect()
}

/// One look's value for each of the three looks.
fn pick<T>(look: Look, neon: T, holo: T, gunmetal: T) -> T {
    match look {
        Look::Neon => neon,
        Look::Holo => holo,
        Look::Gunmetal => gunmetal,
    }
}

/// Every custom property the settings stylesheets read, resolved for one look.
fn tokens(look: Look, config: &DesktopConfig) -> BTreeMap<String, Value> {
    let theme = SettingsStyle {
        look,
        style: Style::resolve(look, config.themes.of(look)),
        defaults: theme::defaults_of(look),
    };
    let mut tokens = Tokens::default();
    theme.palette(&mut tokens);
    theme.fonts(&mut tokens);
    theme.shape(&mut tokens);
    theme.spacing(&mut tokens);
    theme.chrome_geometry(&mut tokens);
    theme.control_geometry(&mut tokens);
    SettingsStyle::decoration_geometry(&mut tokens);
    theme.decorations(&mut tokens);
    theme.metal(&mut tokens);
    theme.frame(&mut tokens);
    theme.navigation(&mut tokens);
    theme.headings(&mut tokens);
    theme.group(&mut tokens);
    theme.fields(&mut tokens);
    theme.buttons(&mut tokens);
    theme.primary_buttons(&mut tokens);
    theme.badges(&mut tokens);
    theme.cards(&mut tokens);
    theme.bar(&mut tokens);
    theme.picker(&mut tokens);
    theme.checkbox(&mut tokens);
    theme.checkbox_knob(&mut tokens);
    theme.range(&mut tokens);
    tokens
        .0
        .into_iter()
        .map(|(name, value)| (name, Value::String(value)))
        .collect()
}

#[derive(Default)]
struct Tokens(BTreeMap<String, String>);

impl Tokens {
    fn set(&mut self, name: &str, value: impl Into<String>) {
        self.0.insert(name.to_owned(), value.into());
    }
}

struct SettingsStyle {
    look: Look,
    style: Style,
    defaults: Style,
}

impl SettingsStyle {
    fn accent_tint(&self, alpha: f32) -> String {
        rgba(self.style.dark.accent, self.glow_alpha(alpha))
    }

    fn border_tint(&self, alpha: f32) -> String {
        rgba(self.style.dark.border, self.glow_alpha(alpha))
    }

    fn glow_alpha(&self, alpha: f32) -> f32 {
        (alpha * self.style.dark.glow / self.defaults.dark.glow).clamp(0.0, 1.0)
    }

    fn palette(&self, tokens: &mut Tokens) {
        let s = &self.style.dark;
        let style = &self.style;
        let defaults = &self.defaults;
        tokens.set("--scheme", scheme(style));
        tokens.set("--bg", palette(style, |scheme| scheme.background.to_hex()));
        tokens.set("--fg", palette(style, |scheme| scheme.text.to_hex()));
        tokens.set("--mut", palette(style, |scheme| scheme.muted.to_hex()));
        tokens.set("--line", palette(style, |scheme| scheme.border.to_hex()));
        tokens.set("--surf", surface_pair(style, defaults));
        tokens.set("--neon", palette(style, |scheme| scheme.accent.to_hex()));
        tokens.set("--blue", palette(style, |scheme| scheme.secondary.to_hex()));
        tokens.set("--deep", palette(style, |scheme| scheme.raised.to_hex()));
        tokens.set("--warn", palette(style, |scheme| scheme.failed.to_hex()));
        tokens.set("--amber", palette(style, |scheme| scheme.pending.to_hex()));
        tokens.set(
            "--on-accent",
            palette(style, |scheme| scheme.on_accent.to_hex()),
        );
        tokens.set("--ok-solid", s.done.to_hex());
        tokens.set(
            "--applied",
            palette_against_defaults(style, defaults, |scheme, default| {
                if scheme.done == default.done {
                    scheme.muted.to_hex()
                } else {
                    scheme.done.to_hex()
                }
            }),
        );
        tokens.set(
            "--caret",
            palette_against_defaults(style, defaults, |scheme, default| {
                if scheme.secondary == default.secondary {
                    "currentcolor".to_owned()
                } else {
                    scheme.secondary.to_hex()
                }
            }),
        );
        tokens.set(
            "--selection",
            palette_against_defaults(style, defaults, |scheme, default| {
                if scheme.secondary == default.secondary {
                    "Highlight".to_owned()
                } else {
                    scheme.secondary.to_hex()
                }
            }),
        );
    }

    fn fonts(&self, tokens: &mut Tokens) {
        let look = self.look;
        let s = &self.style.dark;
        let d = &self.defaults.dark;
        let neon = self.look == Look::Neon;
        let scale = s.font_scale;
        let track = |baseline: f32| baseline / d.tracking * s.tracking;
        tokens.set("--sans", s.font_family.clone());
        tokens.set("--mono", s.mono_family.clone());
        tokens.set("--line-height", "1.45");

        for (name, baseline) in [
            ("--fs-body", 14.0),
            ("--fs-13", 13.0),
            ("--fs-12", 12.0),
            ("--fs-11-5", 11.5),
            ("--fs-11", 11.0),
            ("--fs-10-5", 10.5),
            ("--fs-10", 10.0),
            ("--fs-narrow", 11.0),
            ("--brand-size", pick(look, 12.0, 13.0, 13.0)),
            ("--h4-size", pick(look, 12.0, 10.0, 10.0)),
        ] {
            tokens.set(name, px(baseline * scale));
        }
        tokens.set(
            "--h4-family",
            if neon { "var(--sans)" } else { "var(--mono)" }.to_owned(),
        );
        // Neon draws its small headings heavier than its labels; the others draw both at the label weight.
        let label_weight = i32::from(s.font_weight) - i32::from(d.font_weight);
        tokens.set("--weight-label", s.font_weight.to_string());
        tokens.set(
            "--h4-weight",
            pick(
                look,
                600 + label_weight,
                i32::from(s.font_weight),
                i32::from(s.font_weight),
            )
            .to_string(),
        );
        tokens.set("--track-brand", em(track(pick(look, 0.18, 0.24, 0.24))));
        tokens.set("--track-page", em(track(pick(look, 0.12, 0.18, 0.18))));
        tokens.set("--track-section", em(track(pick(look, 0.06, 0.16, 0.16))));
        tokens.set("--track-choice", em(track(0.14)));
        tokens.set("--track-button", em(track(0.12)));
    }

    fn shape(&self, tokens: &mut Tokens) {
        let look = self.look;
        let s = &self.style.dark;
        let d = &self.defaults.dark;
        let radius =
            |baseline: f32| (baseline + f32::from(s.radius) - f32::from(d.radius)).max(0.0);
        let (input, button, badge, nav_button, card, code, check, meter, lk, entry) = pick(
            look,
            (6.0, 6.0, 6.0, 6.0, 8.0, 6.0, 10.0, 3.0, 6.0, 8.0),
            (6.0, 0.0, 0.0, 0.0, 0.0, 6.0, 0.0, 0.0, 0.0, 0.0),
            (2.0, 2.0, 2.0, 0.0, 0.0, 6.0, 3.0, 0.0, 0.0, 0.0),
        );
        for (name, baseline) in [
            ("--radius-input", input),
            ("--radius-button", button),
            ("--radius-badge", badge),
            ("--radius-nav-button", nav_button),
            ("--radius-card", card),
            ("--radius-editor", pick(look, 8.0, 0.0, 0.0)),
            ("--radius-entry", entry),
            ("--radius-code", code),
            ("--radius-model", 8.0),
            ("--radius-own", 8.0),
            ("--radius-check", check),
            ("--radius-meter", meter),
            ("--radius-lk", lk),
        ] {
            tokens.set(name, px(radius(baseline)));
        }
        tokens.set("--radius-pill", "99px");
        tokens.set(
            "--radius-thumb",
            if look == Look::Gunmetal { "2px" } else { "0px" }.to_owned(),
        );
        tokens.set("--chamfer", px(f32::from(s.chamfer)));
        tokens.set("--lk-chamfer", px(8.0));
    }

    fn spacing(&self, tokens: &mut Tokens) {
        let look = self.look;
        let s = &self.style.dark;
        let d = &self.defaults.dark;
        let length = |baseline: f32| px(baseline * s.spacing / d.spacing);
        for (name, baseline) in [
            ("--nav-gap", 8.0),
            ("--nav-pad-y", 14.0),
            ("--nav-pad-x", 10.0),
            ("--nav-narrow-pad-y", 8.0),
            ("--items-gap", 2.0),
            ("--nav-button-pad-y", 6.0),
            ("--nav-button-pad-x", pick(look, 10.0, 12.0, 12.0)),
            ("--brand-pad-top", pick(look, 0.0, 2.0, 2.0)),
            ("--brand-pad-x", 10.0),
            ("--brand-pad-bottom", pick(look, 2.0, 4.0, 4.0)),
            ("--field-pad-y", pick(look, 4.0, 0.0, 0.0)),
            ("--field-pad-x", 10.0),
            ("--button-pad-y", pick(look, 4.0, 0.0, 0.0)),
            ("--button-pad-x", 12.0),
            ("--pages-pad-y", 14.0),
            ("--pages-pad-x", 22.0),
            ("--pages-pad-narrow", 10.0),
            ("--pages-pad-narrowest", 6.0),
            ("--group-pad-narrow", 8.0),
            ("--select-narrow-pad", 6.0),
            ("--row-gap", 16.0),
            ("--row-pad-y", 8.0),
            ("--inline-gap", 8.0),
            ("--stack-gap", 6.0),
            ("--entry-gap-y", 4.0),
            ("--entry-gap-x", 10.0),
            ("--entry-head-gap-y", 2.0),
            ("--entry-head-gap-x", 10.0),
            ("--fields-gap-y", 6.0),
            ("--fields-gap-x", 12.0),
            ("--card-gap-y", 6.0),
            ("--card-gap-x", 10.0),
            ("--card-pad-y", 8.0),
            ("--card-pad-x", 10.0),
            ("--badge-pad-y", 2.0),
            ("--badge-pad-x", 10.0),
            ("--editor-h4-margin-top", 12.0),
            ("--editor-about-margin-top", 4.0),
            ("--editor-about-margin-bottom", 6.0),
            ("--editor-pad-x", 10.0),
            ("--editor-pad-bottom", 10.0),
            ("--editor-actions-margin-top", 10.0),
            ("--fold-indent", 8.0),
            ("--fold-margin", 6.0),
            ("--note-margin-top", 6.0),
            ("--why-margin-top", 4.0),
            ("--code-pad-y", 6.0),
            ("--code-pad-x", 8.0),
            ("--model-pad-y", 8.0),
            ("--model-pad-x", 10.0),
            ("--model-gap", 4.0),
            ("--own-pad-y", 8.0),
            ("--own-pad-x", 10.0),
            ("--own-gap", 6.0),
            ("--h3-margin-bottom", pick(look, 2.0, 4.0, 4.0)),
            ("--h3-pad-bottom", pick(look, 0.0, 6.0, 6.0)),
            ("--h4-margin-top", pick(look, 16.0, 6.0, 6.0)),
            ("--h4-margin-bottom", pick(look, 4.0, 2.0, 2.0)),
            ("--textarea-pad-y", pick(look, 4.0, 6.0, 6.0)),
            ("--textarea-pad-x", 10.0),
            ("--bar-pad-x", 22.0),
            ("--bar-pad-bottom", 10.0),
            ("--group-margin-top", pick(look, 0.0, 10.0, 10.0)),
        ] {
            tokens.set(name, length(baseline));
        }
    }

    fn chrome_geometry(&self, tokens: &mut Tokens) {
        let look = self.look;
        let spacing = self.style.dark.spacing / self.defaults.dark.spacing;
        let padding = |inset: f32, edge: f32| px((inset * spacing - edge).max(0.0));
        let width = f32::from(self.style.dark.border_width);
        let app_bezel = width.min(5.0);
        let nav_bezel = pick(look, width, width, (width - 1.0).max(0.0));
        let group_bezel = pick(look, 0.0, width, (width - 2.0).max(0.0));
        tokens.set("--app-bezel", px(pick(look, 0.0, app_bezel, app_bezel)));
        tokens.set(
            "--app-pad",
            px(pick(look, 0.0, 5.0 - app_bezel, 5.0 - app_bezel)),
        );
        tokens.set("--nav-bezel", px(nav_bezel));
        tokens.set(
            "--nav-pad-right",
            padding(pick(look, 11.0, 14.0, 14.0), nav_bezel),
        );
        tokens.set("--group-bezel", px(group_bezel));
        tokens.set(
            "--group-pad-y",
            padding(pick(look, 0.0, 7.0, 7.0), group_bezel),
        );
        tokens.set(
            "--group-pad-x",
            padding(pick(look, 0.0, 17.0, 17.0), group_bezel),
        );
        let bar_bezel = pick(look, width, width, group_bezel);
        tokens.set("--bar-bezel", px(bar_bezel));
        tokens.set(
            "--bar-pad-top",
            padding(pick(look, 11.0, 13.0, 13.0), bar_bezel),
        );
        tokens.set("--button-bezel", px(1.0));
        tokens.set("--button-side-bezel", px(pick(look, 0.0, 2.0, 2.0)));
    }

    fn control_geometry(&self, tokens: &mut Tokens) {
        let look = self.look;
        tokens.set("--nav-width", px(190.0));
        tokens.set(
            "--field-height",
            pick(look, "auto", "26px", "26px").to_owned(),
        );
        tokens.set(
            "--button-height",
            pick(look, "auto", "26px", "26px").to_owned(),
        );
        tokens.set(
            "--field-number-width",
            pick(look, "6.5em", "7em", "7em").to_owned(),
        );
        tokens.set("--select-arrow-pad", px(26.0));
        tokens.set("--check-w", px(pick(look, 36.0, 40.0, 40.0)));
        tokens.set("--check-h", px(20.0));
        tokens.set("--check-knob-left", px(pick(look, 2.0, 3.0, 3.0)));
        tokens.set("--check-knob-top", px(pick(look, 2.0, 3.0, 3.0)));
        tokens.set("--check-knob-w", px(pick(look, 16.0, 14.0, 14.0)));
        tokens.set("--check-knob-h", px(pick(look, 16.0, 12.0, 12.0)));
        tokens.set(
            "--check-knob-radius",
            pick(look, "50%", "0px", "2px").to_owned(),
        );
        tokens.set("--check-travel", px(pick(look, 16.0, 18.0, 18.0)));
        tokens.set("--check-bezel", px(pick(look, 0.0, 1.0, 1.0)));
        tokens.set("--meter-w", px(pick(look, 150.0, 160.0, 160.0)));
        tokens.set("--meter-h", px(6.0));
        tokens.set("--slider-w", px(150.0));
        tokens.set("--range-w", px(160.0));
        tokens.set("--range-h", px(16.0));
        tokens.set("--range-thumb-w", px(pick(look, 0.0, 10.0, 9.0)));
        tokens.set("--range-thumb-h", px(pick(look, 0.0, 10.0, 16.0)));
        tokens.set("--range-thumb-margin", px(pick(look, 0.0, 3.0, 0.0)));
        tokens.set("--lk-width", px(104.0));
        tokens.set("--lk-pad", px(6.0));
        tokens.set("--lk-mini-height", px(34.0));
        tokens.set("--lk-mini-gap", px(5.0));
        tokens.set("--lk-mini-line-h", px(2.0));
    }

    fn decoration_geometry(tokens: &mut Tokens) {
        for (name, value) in [
            ("--clear", "transparent"),
            ("--hairline", "1px"),
            ("--focus-width", "2px"),
            ("--focus-offset", "2px"),
            ("--look-gap", "10px"),
            ("--look-swatch-inset", "18%"),
            ("--look-swatch-top", "45%"),
            ("--look-swatch-outline-inset", "-1px"),
            ("--heading-gap", "8px"),
            ("--heading-marker", "7px"),
            ("--module-row-height", "30px"),
            ("--select-arrow-x", "9px"),
            ("--holo-bracket-reach", "7px"),
            ("--holo-bracket-width", "2px"),
            ("--holo-bracket-glow", "3px"),
            ("--holo-marker-angle", "45deg"),
            ("--holo-marker-scale", "0.8"),
            ("--holo-corner-box", "10px"),
            ("--holo-corner-inset", "-1px"),
            ("--holo-corner-width", "2px"),
            ("--holo-focus-inset", "-3px"),
            ("--holo-switch-angle", "-14deg"),
            ("--range-division", "16px"),
            ("--range-start", "5px"),
            ("--range-clearance", "10px"),
            ("--range-filled-height", "2px"),
            ("--range-track-height", "4px"),
            ("--range-holo-track-y", "7px"),
            ("--range-holo-ruler-y", "11px"),
            ("--range-metal-track-y", "6px"),
            ("--metal-marker-inset", "4px"),
            ("--metal-marker-width", "3px"),
            ("--round", "50%"),
            ("--check-dot-left", "8px"),
            ("--check-dot-top", "7px"),
            ("--check-dot-size", "4px"),
            ("--check-dot-shadow-y", "1px"),
            ("--check-dot-blur", "1px"),
            ("--metal-thumb-stripe-size", "2px"),
            ("--metal-thumb-stripe-end", "3px"),
            ("--metal-thumb-pattern-x", "2px"),
            ("--metal-thumb-pattern-y", "4px"),
            ("--metal-thumb-pattern-width", "5px"),
            ("--metal-thumb-pattern-height", "8px"),
            ("--control-text-max", "22em"),
            ("--entry-add-max", "16em"),
            ("--legend-inset", "6px"),
            ("--phrase-basis", "10em"),
            ("--field-gap", "4px"),
            ("--table-row-inset", "2px"),
            ("--server-unit-basis", "12em"),
            ("--empty-inset", "2px"),
            ("--chat-notice-inset", "4px"),
            ("--recognizer-label-basis", "16em"),
            ("--entries-max", "24em"),
            ("--entry-note-pad-y", "4px"),
            ("--entry-note-pad-x", "6px"),
            ("--mini-gap", "2px"),
            ("--server-kind-basis", "26em"),
            ("--feedback-gap", "10px"),
            ("--progress-min", "4em"),
            ("--chip-gap", "2px"),
            ("--chip-right", "2px"),
            ("--chip-left", "6px"),
            ("--feedback-max", "6.5em"),
        ] {
            tokens.set(name, value);
        }
    }

    fn decorations(&self, tokens: &mut Tokens) {
        let look = self.look;
        let s = &self.style.dark;
        let d = &self.defaults.dark;
        let neon = self.look == Look::Neon;
        let style = &self.style;
        let neon_defaults = theme::defaults_of(Look::Neon);
        let preview_style = if neon { style } else { &neon_defaults };
        let holo_defaults = theme::defaults_of(Look::Holo);
        tokens.set(
            "--editor-bg",
            if neon { "var(--surf)" } else { "#101418" }.to_owned(),
        );
        tokens.set("--lk-mini-bg", surface_pair(preview_style, &neon_defaults));
        tokens.set(
            "--lk-mini-border",
            palette(preview_style, |scheme| scheme.border.to_hex()),
        );
        let nav_divider = |scheme: &Scheme| {
            if !neon && s.border != d.border {
                s.border.to_hex()
            } else {
                scheme.border.to_hex()
            }
        };
        let light = preview_style.light.as_ref().unwrap_or(&preview_style.dark);
        tokens.set("--nav-divider-light", nav_divider(light));
        tokens.set("--nav-divider-dark", nav_divider(&preview_style.dark));
        tokens.set("--corner-glow", self.accent_tint(0.8));
        tokens.set("--lk-module-accent", holo_defaults.dark.accent.to_hex());
        tokens.set("--lk-module-glow", "0 0 6px var(--lk-module-accent)");
        tokens.set(
            "--heading-marker-glow",
            format!("0 0 6px {}", self.accent_tint(1.0)),
        );
        tokens.set(
            "--nav-marker-glow",
            format!("0 0 8px {}", self.accent_tint(1.0)),
        );
        tokens.set(
            "--check-indicator-glow",
            format!("0 0 5px {}", rgba(s.done, self.glow_alpha(1.0))),
        );

        tokens.set("--lk-holo-outline", holo_defaults.dark.border.to_hex());
        tokens.set("--arrow", ARROW.to_owned());
        tokens.set(
            "--hex",
            if neon {
                "none".to_owned()
            } else {
                // Settings drew its mesh at five percent; a look's own mesh only moves it from there.
                mesh((0.05 + s.mesh - d.mesh).clamp(0.0, 1.0))
            },
        );
        tokens.set(
            "--rowline",
            pick(
                look,
                "transparent".to_owned(),
                self.border_tint(0.18),
                "rgba(255, 255, 255, 0.06)".to_owned(),
            ),
        );
        tokens.set(
            "--corner-display",
            if look == Look::Holo && s.corners {
                "block"
            } else {
                "none"
            }
            .to_owned(),
        );
        tokens.set(
            "--control-motion",
            if s.pulse { "0.15s" } else { "0s" }.to_owned(),
        );
        tokens.set(
            "--focus-glow",
            if neon && (s.glow - d.glow).abs() > f32::EPSILON {
                format!(
                    "0 0 {} {}",
                    px(4.0 + 12.0 * s.glow),
                    rgba(s.accent, (0.15 + 0.5 * (s.glow - d.glow)).clamp(0.0, 1.0))
                )
            } else {
                "none".to_owned()
            },
        );
    }

    fn metal(&self, tokens: &mut Tokens) {
        let look = self.look;
        let mut metal = |name: &str, value: &str| {
            tokens.set(
                name,
                if look == Look::Gunmetal {
                    value
                } else {
                    "transparent"
                }
                .to_owned(),
            );
        };
        metal("--gm-metal-1", "#6a6a6e");
        metal("--gm-metal-2", "#3c3c3f");
        metal("--gm-metal-3", "#2a2a2c");
        metal("--gm-metal-4", "#55555a");
        tokens.set("--edge",
            if look == Look::Gunmetal {
                "var(--hex) border-box, linear-gradient(180deg, var(--gm-metal-1), var(--gm-metal-2) 45%, var(--gm-metal-3) 70%, var(--gm-metal-4)) border-box".to_owned()
            } else {
                "none".to_owned()
            },
        );
    }

    fn frame(&self, tokens: &mut Tokens) {
        let look = self.look;
        let s = &self.style.dark;
        let d = &self.defaults.dark;
        let extra_edge = if s.border_width > 5 {
            "center / calc(100% - 2px) calc(100% - 2px) no-repeat "
        } else {
            ""
        };
        tokens.set(
            "--app-edge",
            pick(
                look,
                "transparent".to_owned(),
                self.border_tint(0.8),
                "transparent".to_owned(),
            ),
        );
        tokens.set(
            "--app-bg",
            pick(
                look,
                "none".to_owned(),
                format!(
                    "var(--hex), linear-gradient(180deg, {}, {})",
                    s.background.to_hex(),
                    tint(s.background, d.background, [7, 12, 17], 1.0)
                ),
                format!(
                    "linear-gradient(180deg, {}, {}) {extra_edge}padding-box, var(--edge)",
                    s.background.to_hex(),
                    shade(s.background, 2.0 / 3.0)
                ),
            ),
        );
        tokens.set(
            "--app-shadow",
            pick(
                look,
                "none".to_owned(),
                format!("inset 0 0 30px {}", self.border_tint(0.12)),
                "0 0 0 1px #000000, inset 0 0 0 1px #000000".to_owned(),
            ),
        );
        tokens.set(
            "--nav-edge",
            pick(
                look,
                "var(--line)".to_owned(),
                self.border_tint(0.45),
                "transparent".to_owned(),
            ),
        );
        tokens.set(
            "--nav-bg",
            pick(
                look,
                "none".to_owned(),
                format!(
                    "linear-gradient(180deg, {}, {})",
                    tint(s.surface, d.surface, [15, 37, 48], 0.6),
                    tint(s.surface, d.surface, [8, 20, 25], 0.4)
                ),
                format!(
                    "linear-gradient(180deg, {}, {}) padding-box, var(--edge)",
                    shade(s.surface, 0.9),
                    shade(s.background, 0.89)
                ),
            ),
        );
    }

    fn navigation(&self, tokens: &mut Tokens) {
        let look = self.look;
        tokens.set(
            "--nav-hover-bg",
            pick(look, "var(--surf)", "none", "none").to_owned(),
        );
        tokens.set("--nav-text", pick(look, "var(--fg)", "#a9bfcc", "#a7acb1"));
        tokens.set(
            "--nav-text-hover",
            pick(look, "var(--fg)", "#e6f6fb", "#e4e8eb"),
        );
        tokens.set(
            "--nav-current-text",
            pick(look, "var(--neon)", "#effeff", "#effeff"),
        );
        tokens.set(
            "--nav-current-bg",
            pick(
                look,
                "var(--deep)".to_owned(),
                self.accent_tint(0.08),
                "#070b0f".to_owned(),
            ),
        );
        tokens.set(
            "--nav-current-glow",
            pick(
                look,
                "transparent".to_owned(),
                self.accent_tint(0.35),
                "transparent".to_owned(),
            ),
        );
        tokens.set(
            "--nav-current-shadow",
            pick(
                look,
                "none",
                "none",
                "inset 0 2px 6px #000000, inset 0 0 0 1px #2a2a2d",
            ),
        );
        tokens.set(
            "--search-bg",
            pick(look, "var(--surf)", "rgba(3, 9, 13, 0.85)", "#05080b"),
        );
        tokens.set(
            "--search-text",
            pick(look, "var(--mut)", "var(--fg)", "var(--fg)").to_owned(),
        );
        tokens.set(
            "--search-edge",
            pick(
                look,
                "var(--line)".to_owned(),
                self.border_tint(0.65),
                "#2a2b2e".to_owned(),
            ),
        );
        tokens.set(
            "--search-edge-bottom",
            pick(
                look,
                "var(--line)".to_owned(),
                self.border_tint(0.65),
                "#4a4a4e".to_owned(),
            ),
        );
    }

    fn headings(&self, tokens: &mut Tokens) {
        let look = self.look;
        let s = &self.style.dark;
        tokens.set(
            "--brand-color",
            pick(
                look,
                "var(--neon)".to_owned(),
                s.accent.to_hex(),
                "#e2e6ea".to_owned(),
            ),
        );
        tokens.set(
            "--brand-shadow",
            pick(
                look,
                "none".to_owned(),
                format!("0 0 8px {}", self.accent_tint(0.6)),
                "0 1px 0 #000000".to_owned(),
            ),
        );
        tokens.set(
            "--h3-color",
            pick(
                look,
                "var(--neon)".to_owned(),
                s.accent.to_hex(),
                "#e6eaee".to_owned(),
            ),
        );
        tokens.set(
            "--h3-shadow",
            pick(
                look,
                "none".to_owned(),
                format!("0 0 8px {}", self.accent_tint(0.45)),
                "0 1px 0 #000000".to_owned(),
            ),
        );
        tokens.set(
            "--h3-rule",
            pick(
                look,
                "none".to_owned(),
                format!(
                    "linear-gradient(90deg, {} 0 48px, {} 48px) bottom / 100% 1px no-repeat",
                    s.accent.to_hex(),
                    self.border_tint(0.5)
                ),
                "none".to_owned(),
            ),
        );
        tokens.set("--h4-color", pick(look, "var(--mut)", "#7f9cad", "#a3a8ad"));
        tokens.set(
            "--hint-color",
            pick(look, "var(--mut)", "#86a0ae", "#9aa0a5"),
        );
    }

    fn group(&self, tokens: &mut Tokens) {
        let look = self.look;
        let s = &self.style.dark;
        tokens.set(
            "--group-bg",
            pick(
                look,
                "none".to_owned(),
                rgba(s.surface, s.opacity.clamp(0.0, 1.0)),
                format!(
                    "linear-gradient(180deg, {}, {}) padding-box, var(--edge)",
                    rgba(s.surface, s.opacity.clamp(0.0, 1.0)),
                    shade(s.surface, 0.81)
                ),
            ),
        );
        tokens.set(
            "--group-edge",
            pick(
                look,
                "transparent".to_owned(),
                self.border_tint(0.3),
                "transparent".to_owned(),
            ),
        );
        tokens.set(
            "--group-shadow",
            pick(look, "none", "none", "0 2px 8px rgba(0, 0, 0, 0.6)"),
        );
        tokens.set("--group-clip",
            if look == Look::Gunmetal {
                "polygon(var(--chamfer) 0, 100% 0, 100% calc(100% - var(--chamfer)), calc(100% - var(--chamfer)) 100%, 0 100%, 0 var(--chamfer))".to_owned()
            } else {
                "none".to_owned()
            },
        );
    }

    fn fields(&self, tokens: &mut Tokens) {
        let look = self.look;
        tokens.set(
            "--field-bg",
            pick(look, "var(--surf)", "rgba(3, 9, 13, 0.85)", "#05080b"),
        );
        tokens.set(
            "--field-edge",
            pick(
                look,
                "var(--line)".to_owned(),
                self.border_tint(0.65),
                "#2a2b2e".to_owned(),
            ),
        );
        tokens.set(
            "--field-edge-bottom",
            pick(
                look,
                "var(--line)".to_owned(),
                self.border_tint(0.65),
                "#4a4a4e".to_owned(),
            ),
        );
        tokens.set(
            "--field-text",
            pick(look, "var(--fg)", "#e4f2f8", "#e4f2f8"),
        );
        tokens.set(
            "--field-shadow",
            pick(look, "none", "none", "inset 0 2px 5px #000000"),
        );
        tokens.set(
            "--placeholder",
            pick(look, "var(--mut)", "#5f7886", "#5f7886"),
        );
    }

    fn buttons(&self, tokens: &mut Tokens) {
        let look = self.look;
        tokens.set(
            "--button-text",
            pick(look, "var(--fg)", "#cfe6ef", "#e1e4e7"),
        );
        tokens.set(
            "--button-bg",
            pick(look, "var(--surf)", "var(--deep)", "none"),
        );
        tokens.set(
            "--button-edge",
            pick(
                look,
                "var(--line)".to_owned(),
                self.border_tint(0.6),
                "#6e6e72".to_owned(),
            ),
        );
        tokens.set(
            "--button-hover-bg",
            pick(look, "var(--surf)", "#16303b", "none"),
        );
        tokens.set(
            "--button-hover-edge",
            pick(
                look,
                "var(--neon)".to_owned(),
                self.border_tint(0.9),
                "#6e6e72".to_owned(),
            ),
        );
    }

    fn primary_buttons(&self, tokens: &mut Tokens) {
        let look = self.look;
        let s = &self.style.dark;
        tokens.set(
            "--button-1",
            pick(
                look,
                "var(--surf)".to_owned(),
                "var(--deep)".to_owned(),
                s.raised.to_hex(),
            ),
        );
        tokens.set(
            "--button-2",
            pick(
                look,
                "var(--surf)".to_owned(),
                "var(--deep)".to_owned(),
                s.border.to_hex(),
            ),
        );
        tokens.set(
            "--button-hover-1",
            pick(look, "var(--surf)", "#16303b", "#6a6a6f"),
        );
        tokens.set(
            "--button-hover-2",
            pick(look, "var(--surf)", "#16303b", "#46464a"),
        );
        tokens.set(
            "--button-shadow",
            pick(
                look,
                "none",
                "none",
                "inset 0 1px 0 rgba(255, 255, 255, 0.15), 0 1px 2px #000000",
            ),
        );
        tokens.set(
            "--button-quiet",
            pick(look, "var(--mut)", "var(--mut)", "#b8bcc0"),
        );
        tokens.set(
            "--primary-edge",
            pick(
                look,
                "var(--neon)".to_owned(),
                "var(--neon)".to_owned(),
                rgba(s.accent, 0.6),
            ),
        );
        tokens.set(
            "--primary-shadow",
            pick(
                look,
                "none".to_owned(),
                format!("0 0 12px {}", self.accent_tint(0.45)),
                "none".to_owned(),
            ),
        );
        tokens.set(
            "--primary-text-shadow",
            pick(
                look,
                "none".to_owned(),
                "none".to_owned(),
                format!("0 0 6px {}", self.accent_tint(0.7)),
            ),
        );
    }

    fn badges(&self, tokens: &mut Tokens) {
        let look = self.look;
        tokens.set(
            "--badge-text",
            pick(look, "var(--fg)", "#cfe6ef", "#cfe6ef"),
        );
        tokens.set(
            "--badge-bg",
            pick(
                look,
                "transparent".to_owned(),
                self.accent_tint(0.05),
                "#0a141a".to_owned(),
            ),
        );
        tokens.set(
            "--badge-edge",
            pick(
                look,
                "var(--line)".to_owned(),
                self.border_tint(0.55),
                "#1a2329".to_owned(),
            ),
        );
        tokens.set("--badge-on-color", "var(--neon)");
        tokens.set(
            "--badge-on-edge",
            pick(
                look,
                "var(--neon)".to_owned(),
                self.border_tint(0.55),
                "#1a2329".to_owned(),
            ),
        );
        tokens.set(
            "--code-bg",
            pick(look, "var(--surf)", "rgba(3, 9, 13, 0.85)", "#05080b"),
        );
        tokens.set(
            "--code-edge",
            pick(
                look,
                "var(--line)".to_owned(),
                self.border_tint(0.65),
                "#2a2b2e".to_owned(),
            ),
        );
        tokens.set(
            "--badge-shadow",
            pick(look, "none", "none", "inset 0 1px 3px #000000"),
        );
    }

    fn cards(&self, tokens: &mut Tokens) {
        let look = self.look;
        tokens.set(
            "--card-edge",
            pick(
                look,
                "var(--line)".to_owned(),
                self.border_tint(0.4),
                "#2a2b2e".to_owned(),
            ),
        );
        tokens.set(
            "--card-bg",
            pick(
                look,
                "transparent".to_owned(),
                self.accent_tint(0.03),
                "#0a0b0d".to_owned(),
            ),
        );
        tokens.set(
            "--card-shadow",
            pick(look, "none", "none", "inset 0 1px 3px #000000"),
        );
        tokens.set(
            "--meter-bg",
            pick(
                look,
                "var(--line)".to_owned(),
                self.border_tint(0.25),
                "#05090d".to_owned(),
            ),
        );
        tokens.set(
            "--meter-shadow",
            pick(look, "none", "none", "inset 0 1px 2px #000000"),
        );
    }

    fn bar(&self, tokens: &mut Tokens) {
        let look = self.look;
        let s = &self.style.dark;
        let d = &self.defaults.dark;
        tokens.set(
            "--bar-edge",
            pick(
                look,
                "var(--line)".to_owned(),
                self.border_tint(0.45),
                "transparent".to_owned(),
            ),
        );
        tokens.set(
            "--bar-bg",
            pick(
                look,
                "var(--surf)".to_owned(),
                tint(s.surface, d.surface, [8, 20, 25], 0.6),
                format!(
                    "linear-gradient(180deg, {}, {}) padding-box, var(--edge)",
                    shade(s.surface, 0.9),
                    shade(s.background, 0.94)
                ),
            ),
        );
    }

    fn picker(&self, tokens: &mut Tokens) {
        let look = self.look;
        let s = &self.style.dark;
        tokens.set(
            "--lk-bg",
            pick(look, "transparent", "transparent", "#101113"),
        );
        tokens.set(
            "--lk-edge",
            pick(
                look,
                "var(--line)".to_owned(),
                self.border_tint(0.45),
                s.border.to_hex(),
            ),
        );
        tokens.set("--lk-text", pick(look, "var(--mut)", "#9fb8c6", "#b8bcc0"));
        tokens.set(
            "--lk-on-edge",
            pick(
                look,
                "var(--neon)".to_owned(),
                "var(--neon)".to_owned(),
                rgba(s.accent, 0.6),
            ),
        );
        tokens.set(
            "--lk-on-shadow",
            pick(
                look,
                "none".to_owned(),
                format!("0 0 10px {}", self.accent_tint(0.25)),
                "none".to_owned(),
            ),
        );
        tokens.set("--lk-holo-1", "#0f2530");
        tokens.set("--lk-holo-2", "#081419");
        tokens.set("--lk-gm-1", "#3a3a3a");
        tokens.set("--lk-gm-2", "#2a2a2a");
        tokens.set(
            "--lk-gm-shadow",
            "inset 0 0 0 4px #4a4a4a, inset 0 0 0 30px #06111a",
        );
    }

    fn checkbox(&self, tokens: &mut Tokens) {
        let look = self.look;
        let s = &self.style.dark;
        tokens.set(
            "--check-bg",
            pick(
                look,
                "var(--line)",
                "#040a0e",
                "linear-gradient(180deg, #05090d, #0d161c)",
            ),
        );
        tokens.set(
            "--check-bg-on",
            pick(
                look,
                "var(--neon)".to_owned(),
                self.accent_tint(0.12),
                "linear-gradient(180deg, #05090d, #0d161c)".to_owned(),
            ),
        );
        tokens.set(
            "--check-edge",
            pick(
                look,
                "transparent".to_owned(),
                self.border_tint(0.7),
                "#1a1d20".to_owned(),
            ),
        );
        tokens.set(
            "--check-edge-left",
            pick(look, "transparent", "#1a1d20", "#4a4d52").to_owned(),
        );
        tokens.set(
            "--check-edge-bottom",
            pick(look, "transparent", "#1a1d20", "#666666").to_owned(),
        );
        tokens.set(
            "--check-edge-on",
            pick(look, "transparent", "var(--neon)", "transparent"),
        );
        tokens.set(
            "--check-edge-right-on",
            pick(
                look,
                "transparent".to_owned(),
                "#1a1d20".to_owned(),
                rgba(s.done, 0.75),
            ),
        );
    }

    fn checkbox_knob(&self, tokens: &mut Tokens) {
        let look = self.look;
        let s = &self.style.dark;
        let knob = "linear-gradient(180deg, #9a9a9a, #555555)".to_owned();
        tokens.set(
            "--check-knob",
            pick(
                look,
                "var(--fg)".to_owned(),
                "#5d7685".to_owned(),
                knob.clone(),
            ),
        );
        tokens.set(
            "--check-knob-on",
            pick(
                look,
                "var(--on-accent)".to_owned(),
                "var(--neon)".to_owned(),
                knob,
            ),
        );
        tokens.set(
            "--check-knob-shadow",
            pick(look, "none", "none", "0 1px 2px #000000"),
        );
        tokens.set(
            "--check-knob-on-shadow",
            pick(
                look,
                "none".to_owned(),
                format!("0 0 8px {}", s.accent.to_hex()),
                format!("0 1px 2px #000000, 1px 0 2px {}", rgba(s.done, 0.35)),
            ),
        );
        tokens.set(
            "--check-shadow",
            pick(look, "none", "none", "inset 0 2px 4px #000000"),
        );
        tokens.set(
            "--check-shadow-on",
            pick(
                look,
                "none".to_owned(),
                "none".to_owned(),
                format!(
                    "inset -3px 0 5px -1px {}, 1px 0 4px {}, inset 0 2px 4px #000000",
                    rgba(s.done, 0.5),
                    rgba(s.done, 0.3)
                ),
            ),
        );
        tokens.set(
            "--check-dot",
            pick(look, "transparent", "transparent", "#3c4045").to_owned(),
        );
        tokens.set(
            "--check-dot-shadow",
            pick(look, "transparent", "transparent", "#000000").to_owned(),
        );
    }

    fn range(&self, tokens: &mut Tokens) {
        let look = self.look;
        let s = &self.style.dark;
        tokens.set(
            "--range-rule",
            pick(
                look,
                "transparent".to_owned(),
                self.border_tint(0.45),
                "transparent".to_owned(),
            ),
        );
        tokens.set(
            "--range-base",
            pick(
                look,
                "transparent".to_owned(),
                self.border_tint(0.5),
                "#05090d".to_owned(),
            ),
        );
        tokens.set(
            "--thumb",
            pick(look, "var(--neon)", "#dffcff", "var(--neon)").to_owned(),
        );
        tokens.set(
            "--thumb-stripe",
            pick(look, "transparent", "transparent", "rgba(0, 0, 0, 0.35)").to_owned(),
        );
        tokens.set(
            "--thumb-1",
            pick(look, "var(--neon)", "var(--neon)", "#b5b5b5").to_owned(),
        );
        tokens.set(
            "--thumb-2",
            pick(look, "var(--neon)", "var(--neon)", "#5a5a5a").to_owned(),
        );
        tokens.set(
            "--thumb-shadow",
            pick(
                look,
                "none".to_owned(),
                format!("0 0 8px {}", s.accent.to_hex()),
                "0 1px 3px #000000".to_owned(),
            ),
        );
    }
}

/// The arrow a look's selects draw, in the muted metal every look shares.
const ARROW: &str = "url(\"data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='10' height='6'%3E%3Cpath d='M0 0L5 6L10 0Z' fill='%2386a0ae'/%3E%3C/svg%3E\")";

fn scheme(style: &Style) -> String {
    if style.light.is_some() {
        "light dark"
    } else {
        "dark"
    }
    .to_owned()
}

/// Neon's surface stays opaque, as Settings always drew it; its own opacity only moves it.
fn surface_pair(style: &Style, defaults: &Style) -> String {
    match (&style.light, &defaults.light) {
        (Some(light), Some(base)) => format!(
            "light-dark({}, {})",
            surface(light, base),
            surface(&style.dark, &defaults.dark)
        ),
        _ => surface(&style.dark, &defaults.dark),
    }
}

fn surface(scheme: &Scheme, model: &Scheme) -> String {
    let alpha = (1.0 + (scheme.opacity - model.opacity)).clamp(0.0, 1.0);
    if alpha >= 0.999 {
        scheme.surface.to_hex()
    } else {
        rgba(scheme.surface, alpha)
    }
}

fn palette(style: &Style, paint: impl Fn(&Scheme) -> String) -> String {
    match &style.light {
        Some(light) => format!("light-dark({}, {})", paint(light), paint(&style.dark)),
        None => paint(&style.dark),
    }
}

fn palette_against_defaults(
    style: &Style,
    defaults: &Style,
    paint: impl Fn(&Scheme, &Scheme) -> String,
) -> String {
    let dark = paint(&style.dark, &defaults.dark);
    match (&style.light, &defaults.light) {
        (Some(light), Some(base)) => format!("light-dark({}, {dark})", paint(light, base)),
        _ => dark,
    }
}

fn rgba(color: Color, alpha: f32) -> String {
    let [r, g, b, _] = color.channels();
    format!("rgba({r}, {g}, {b}, {})", number(alpha))
}

/// Preserves the built-in tint while following RGB changes to its model colour.
fn tint(color: Color, default: Color, [r, g, b]: [u8; 3], alpha: f32) -> String {
    let [red, green, blue, _] = color.channels();
    let [base_red, base_green, base_blue, _] = default.channels();
    let channel =
        |value, base, tint| (i16::from(value) - i16::from(base) + i16::from(tint)).clamp(0, 255);
    format!(
        "rgba({}, {}, {}, {})",
        channel(red, base_red, r),
        channel(green, base_green, g),
        channel(blue, base_blue, b),
        number(alpha)
    )
}

/// A colour darkened toward black, so a gradient's far stop follows the colour it starts from.
fn shade(color: Color, factor: f32) -> String {
    let [r, g, b, _] = color.channels();
    let part = |channel: u8| format!("{:.0}", (f32::from(channel) * factor).clamp(0.0, 255.0));
    format!("rgb({}, {}, {})", part(r), part(g), part(b))
}

fn px(value: f32) -> String {
    format!("{}px", number(value))
}

fn em(value: f32) -> String {
    format!("{}em", number(value))
}

fn number(value: f32) -> String {
    let rounded = (value * 1000.0).round() / 1000.0;
    let mut text = format!("{rounded:.3}");
    while text.contains('.') && (text.ends_with('0') || text.ends_with('.')) {
        text.pop();
    }
    text
}

fn mesh(mesh: f32) -> String {
    format!(
        "url(\"data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='16' height='27.7'%3E%3Cpath d='M8 0L16 4.62V13.86L8 18.47L0 13.86V4.62ZM8 18.47V27.7' fill='none' stroke='white' stroke-opacity='{}'/%3E%3C/svg%3E\")",
        number(mesh)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_committed_fixture_matches_what_the_window_reads() {
        let resolved = response(&config("{}"));
        // `THALVI_WRITE_LOOK_FIXTURE=1 cargo test -p thalvi-app look_styles` regenerates the fixture.
        if std::env::var_os("THALVI_WRITE_LOOK_FIXTURE").is_some() {
            let text = serde_json::to_string_pretty(&resolved).unwrap();
            std::fs::write("ui/src/look-styles.json", format!("{text}\n")).unwrap();
        }
        let fixture: Value =
            serde_json::from_str(include_str!("../ui/src/look-styles.json")).unwrap();
        assert_eq!(resolved, fixture, "ui/src/look-styles.json is stale");
    }

    fn config(json: &str) -> DesktopConfig {
        DesktopConfig::from_json_str(json).unwrap()
    }

    #[test]
    fn every_look_gets_its_colours_and_neon_pairs_light_and_dark() {
        let value = response(&config("{}"));
        assert_eq!(value["looks"]["holo"]["--neon"], "#6bf8ff");
        assert_eq!(value["looks"]["gunmetal"]["--deep"], "#5c5c60");
        assert_eq!(
            value["looks"]["neon"]["--bg"],
            "light-dark(#e8eff7, #020617)"
        );
        for look in ["holo", "gunmetal"] {
            assert_eq!(value["looks"][look]["--editor-bg"], "#101418");
        }
    }

    #[test]
    fn an_override_reaches_its_look_and_a_suspended_or_unreadable_one_keeps_the_built_in_look() {
        let value = response(&config(
            r##"{"themes":{"version":1,
                "holo":{"colors":{"dark":{"accent":"#ffffff"}}},
                "gunmetal":{"suspended":true,"colors":{"dark":{"accent":"#ffffff"}}},
                "neon":{"colors":{"dark":{"accent":"red"}}}}}"##,
        ));
        assert_eq!(value["looks"]["holo"]["--neon"], "#ffffff");
        assert_eq!(value["looks"]["gunmetal"]["--neon"], "#6bf8ff");
        assert_eq!(
            value["looks"]["neon"]["--neon"],
            "light-dark(#007a43, #2bff88)"
        );
    }

    #[test]
    fn the_properties_describe_what_a_look_offers_settings() {
        let value = response(&config("{}"));
        let properties = value["properties"].as_array().unwrap();
        assert_eq!(properties.len(), 32);
        let accent = properties
            .iter()
            .find(|property| property["id"] == "colors.accent")
            .unwrap();
        assert_eq!(accent["label"], "Accent");
        assert_eq!(accent["unit"], "color");
        assert_eq!(accent["looks"], json!(["neon", "holo", "gunmetal"]));
        assert_eq!(accent["surfaces"], json!(["settings", "overlay"]));
        assert!(accent.get("min").is_some_and(Value::is_null));
    }

    #[test]
    fn every_custom_property_the_stylesheets_use_is_emitted_for_every_look() {
        let value = response(&config("{}"));
        let css = concat!(
            include_str!("../ui/src/theme.css"),
            include_str!("../ui/src/settings.css"),
            include_str!("../ui/src/settings-looks.css"),
        );
        // `--p` is the slider's own progress, which the page sets on the element as it is dragged.
        let mut used: Vec<&str> = css
            .match_indices("var(--")
            .filter_map(|(at, _)| {
                let rest = &css[at + "var(--".len()..];
                let end = rest.find(')')?;
                Some(&rest[..end])
            })
            .filter(|name| *name != "p")
            .collect();
        used.sort_unstable();
        used.dedup();
        assert!(!used.is_empty(), "the stylesheets use no custom properties");
        for look in ["neon", "holo", "gunmetal"] {
            for name in &used {
                assert!(
                    value["looks"][look]
                        .get(format!("--{name}"))
                        .is_some_and(|value| value.as_str().is_some_and(|text| !text.is_empty())),
                    "{look} has no --{name}"
                );
            }
        }
    }

    #[test]
    fn every_settings_property_changes_what_the_window_reads_for_its_looks() {
        let base = response(&config("{}"));
        let cases: &[(&str, &str)] = &[
            ("neon", r##"{"colors":{"dark":{"background":"#010203"}}}"##),
            ("neon", r##"{"colors":{"dark":{"surface":"#010203"}}}"##),
            ("neon", r##"{"colors":{"dark":{"raised":"#010203"}}}"##),
            ("neon", r##"{"colors":{"dark":{"text":"#f1f2f3"}}}"##),
            ("neon", r##"{"colors":{"dark":{"muted":"#010203"}}}"##),
            ("neon", r##"{"colors":{"dark":{"border":"#010203"}}}"##),
            ("neon", r##"{"colors":{"dark":{"accent":"#ffffff"}}}"##),
            ("neon", r##"{"colors":{"dark":{"on_accent":"#010203"}}}"##),
            ("neon", r##"{"colors":{"dark":{"secondary":"#010203"}}}"##),
            (
                "neon",
                r##"{"colors":{"dark":{"state":{"pending":"#010203"}}}}"##,
            ),
            (
                "neon",
                r##"{"colors":{"dark":{"state":{"done":"#010203"}}}}"##,
            ),
            (
                "neon",
                r##"{"colors":{"dark":{"state":{"failed":"#010203"}}}}"##,
            ),
            ("neon", r#"{"font":{"family":"Iowan Old Style"}}"#),
            ("neon", r#"{"font":{"mono_family":"Iowan Old Style"}}"#),
            ("neon", r#"{"font":{"scale":1.1}}"#),
            ("neon", r#"{"font":{"weight":800}}"#),
            ("neon", r#"{"font":{"tracking":0.2}}"#),
            ("neon", r#"{"spacing":{"scale":1.2}}"#),
            ("neon", r#"{"shape":{"radius":10}}"#),
            ("holo", r#"{"shape":{"chamfer":2}}"#),
            ("neon", r#"{"border":{"width":2}}"#),
            ("holo", r#"{"border":{"width":2}}"#),
            ("gunmetal", r#"{"border":{"width":2}}"#),
            ("neon", r#"{"surface":{"opacity":0.7}}"#),
            ("neon", r#"{"glow":{"strength":1}}"#),
            ("holo", r#"{"decoration":{"mesh":0.1}}"#),
            ("holo", r#"{"decoration":{"corners":false}}"#),
            ("neon", r#"{"motion":{"pulse":false}}"#),
        ];
        for (look, overrides) in cases {
            let themed = response(&config(&format!(
                r#"{{"themes":{{"version":1,"{look}":{overrides}}}}}"#
            )));
            assert_ne!(
                themed["looks"][look], base["looks"][look],
                "{look} {overrides} changed nothing the settings window reads"
            );
        }
    }
}
