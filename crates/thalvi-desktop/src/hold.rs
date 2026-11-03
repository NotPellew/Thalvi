//! The hold-to-dictate key: its name in the config and the signal that it is down.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::host::Modifier;

/// The keys a hold key may be, as W3C `KeyboardEvent.code` names with their Windows scan codes.
const CODES: [(&str, u16); 49] = [
    ("Digit1", 0x02),
    ("Digit2", 0x03),
    ("Digit3", 0x04),
    ("Digit4", 0x05),
    ("Digit5", 0x06),
    ("Digit6", 0x07),
    ("Digit7", 0x08),
    ("Digit8", 0x09),
    ("Digit9", 0x0A),
    ("Digit0", 0x0B),
    ("KeyQ", 0x10),
    ("KeyW", 0x11),
    ("KeyE", 0x12),
    ("KeyR", 0x13),
    ("KeyT", 0x14),
    ("KeyY", 0x15),
    ("KeyU", 0x16),
    ("KeyI", 0x17),
    ("KeyO", 0x18),
    ("KeyP", 0x19),
    ("KeyA", 0x1E),
    ("KeyS", 0x1F),
    ("KeyD", 0x20),
    ("KeyF", 0x21),
    ("KeyG", 0x22),
    ("KeyH", 0x23),
    ("KeyJ", 0x24),
    ("KeyK", 0x25),
    ("KeyL", 0x26),
    ("KeyZ", 0x2C),
    ("KeyX", 0x2D),
    ("KeyC", 0x2E),
    ("KeyV", 0x2F),
    ("KeyB", 0x30),
    ("KeyN", 0x31),
    ("KeyM", 0x32),
    ("Space", 0x39),
    ("F1", 0x3B),
    ("F2", 0x3C),
    ("F3", 0x3D),
    ("F4", 0x3E),
    ("F5", 0x3F),
    ("F6", 0x40),
    ("F7", 0x41),
    ("F8", 0x42),
    ("F9", 0x43),
    ("F10", 0x44),
    ("F11", 0x57),
    ("F12", 0x58),
];

/// Where `KeyZ` is in [`CODES`].
const DEFAULT_CODE: usize = 29;

/// Why a hold key name was refused. Shown as written.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HoldKeyError {
    #[error(
        "The hold key \"{0}\" is not a key name such as Alt+KeyZ: modifiers Ctrl, Shift, Alt or Win, then a key such as KeyZ, Digit1, F9 or Space, joined by +."
    )]
    Unknown(String),
    #[error("The hold key \"{0}\" needs a modifier, or typing that key would start dictation.")]
    NeedsModifier(String),
}

/// The physical key held to dictate, such as `Alt+KeyZ`: where the key is on the keyboard, not
/// which letter it types, so one setting covers every layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoldKey {
    /// Distinct, in the order [`Modifier`] declares them.
    modifiers: Vec<Modifier>,
    code: usize,
}

/// The modifier names, in the order [`Modifier`] declares them.
const MODIFIERS: [(&str, Modifier); 4] = [
    ("Ctrl", Modifier::Ctrl),
    ("Shift", Modifier::Shift),
    ("Alt", Modifier::Alt),
    ("Win", Modifier::Logo),
];

impl HoldKey {
    /// # Errors
    /// [`HoldKeyError`] when `name` is not modifiers and a key from the list, or names a key that
    /// types a character without a modifier.
    pub fn parse(name: &str) -> Result<Self, HoldKeyError> {
        let unknown = || HoldKeyError::Unknown(name.to_owned());
        let mut parts = name.split('+').collect::<Vec<_>>();
        let key = parts.pop().ok_or_else(unknown)?;
        let code = CODES
            .iter()
            .position(|(code, _)| *code == key)
            .ok_or_else(unknown)?;
        let mut modifiers = Vec::new();
        for part in parts {
            let (_, modifier) = MODIFIERS
                .iter()
                .find(|(known, _)| *known == part)
                .ok_or_else(unknown)?;
            if modifiers.contains(modifier) {
                return Err(unknown());
            }
            modifiers.push(*modifier);
        }
        modifiers.sort();
        if modifiers.is_empty() && !key.starts_with('F') {
            return Err(HoldKeyError::NeedsModifier(name.to_owned()));
        }
        Ok(Self { modifiers, code })
    }

    /// The modifiers that must be down.
    #[must_use]
    pub fn modifiers(&self) -> &[Modifier] {
        &self.modifiers
    }

    /// The key's Windows scan code.
    #[must_use]
    pub fn scan_code(&self) -> u16 {
        CODES[self.code].1
    }
}

impl Default for HoldKey {
    /// Alt with the key left of X.
    fn default() -> Self {
        Self {
            modifiers: vec![Modifier::Alt],
            code: DEFAULT_CODE,
        }
    }
}

impl fmt::Display for HoldKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (name, modifier) in MODIFIERS {
            if self.modifiers.contains(&modifier) {
                write!(f, "{name}+")?;
            }
        }
        f.write_str(CODES[self.code].0)
    }
}

impl<'de> Deserialize<'de> for HoldKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Self::parse(&name).map_err(serde::de::Error::custom)
    }
}

impl Serialize for HoldKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// Whether the hold key is down, and how many times it has been pressed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyState {
    pub presses: u64,
    pub down: bool,
}

/// Follows the key so each press is acted on or refused once, however late it is looked at.
pub struct Presses {
    state: watch::Receiver<KeyState>,
    seen: u64,
}

impl Presses {
    #[must_use]
    pub fn new(state: watch::Receiver<KeyState>) -> Self {
        Self { state, seen: 0 }
    }

    /// Counts the presses so far as dealt with, and the state as seen.
    pub fn skip(&mut self) {
        self.seen = self.state.borrow_and_update().presses;
    }

    /// Resolves when the key changed since it was last looked at; an error once nothing can
    /// press it any more.
    ///
    /// # Errors
    /// [`watch::error::RecvError`] when the input is gone.
    pub async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        self.state.changed().await
    }

    /// The press that went down since the last one was taken, if the key is still down.
    pub fn take(&mut self) -> Option<u64> {
        let KeyState { presses, down } = *self.state.borrow_and_update();
        let fresh = down && presses > self.seen;
        self.seen = presses;
        fresh.then_some(presses)
    }

    /// Whether press number `press` is over: the key is up, or has been pressed again.
    pub fn is_over(&mut self, press: u64) -> bool {
        let state = *self.state.borrow_and_update();
        !state.down || state.presses != press
    }

    /// Resolves when [`is_over`](Self::is_over) is true; never when the input is gone.
    pub async fn over(&mut self, press: u64) {
        if self
            .state
            .wait_for(|state| !state.down || state.presses != press)
            .await
            .is_err()
        {
            std::future::pending::<()>().await;
        }
    }
}

/// Where the hold key's presses and releases come in, from a compositor bind or the keyboard hook.
#[derive(Clone)]
pub struct HoldInput(Arc<watch::Sender<KeyState>>);

impl HoldInput {
    #[must_use]
    pub fn new() -> (Self, watch::Receiver<KeyState>) {
        let (sender, receiver) = watch::channel(KeyState::default());
        (Self(Arc::new(sender)), receiver)
    }

    /// The key went down or up; a repeat of the state it is in changes nothing, so key
    /// auto-repeat starts nothing extra.
    pub fn set(&self, down: bool) {
        self.0.send_if_modified(|state| {
            if state.down == down {
                return false;
            }
            state.down = down;
            if down {
                state.presses += 1;
            }
            true
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hold_key_is_modifiers_and_a_physical_key_and_reads_back_as_written() {
        for (name, scan_code, alt) in [
            ("Alt+KeyZ", 0x2C, true),
            ("Alt+KeyX", 0x2D, true),
            ("Ctrl+Alt+Digit1", 0x02, true),
            ("Shift+Win+Space", 0x39, false),
            ("F9", 0x43, false),
        ] {
            let key = HoldKey::parse(name).unwrap();
            assert_eq!(key.to_string(), name);
            assert_eq!(key.scan_code(), scan_code, "{name}");
            assert_eq!(key.modifiers().contains(&Modifier::Alt), alt, "{name}");
        }
        assert_eq!(HoldKey::default().to_string(), "Alt+KeyZ");
    }

    #[test]
    fn a_hold_key_that_is_not_a_key_or_would_take_a_typing_key_is_refused() {
        for (name, needs_modifier) in [
            ("KeyZ", true),
            ("Digit1", true),
            ("Space", true),
            ("Shift+Hyper", false),
            ("Alt+", false),
            ("", false),
            ("Alt+Alt+KeyZ", false),
            ("alt+KeyZ", false),
            ("Alt+Z", false),
            ("Alt+KeyZ+Ctrl", false),
        ] {
            let error = HoldKey::parse(name).unwrap_err();
            assert_eq!(
                matches!(error, HoldKeyError::NeedsModifier(_)),
                needs_modifier,
                "{name}: {error}"
            );
        }
    }

    #[test]
    fn auto_repeat_is_not_another_press_and_each_release_allows_the_next() {
        let (input, state) = HoldInput::new();
        let seen = || *state.borrow();
        input.set(true);
        input.set(true);
        input.set(true);
        assert_eq!(
            seen(),
            KeyState {
                presses: 1,
                down: true
            }
        );
        input.set(false);
        input.set(false);
        assert_eq!(
            seen(),
            KeyState {
                presses: 1,
                down: false
            }
        );
        input.set(true);
        assert_eq!(
            seen(),
            KeyState {
                presses: 2,
                down: true
            }
        );
    }
}
