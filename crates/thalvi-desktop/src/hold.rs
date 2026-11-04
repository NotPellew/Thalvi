//! The hold-to-dictate key: its name in the config and the signal that it is down.

use std::fmt;
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::frame::Order;
use crate::host::Modifier;
use crate::metrics::{Event, Recorder};

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
#[derive(Clone, Debug, Default)]
pub struct KeyState {
    pub presses: u64,
    pub down: bool,
    /// The latest press's release.
    pub release: Cutoff,
}

/// Where in the [`Order`] of queued frames one press of the key was first released. The first
/// release stays however many presses follow, and every clone sees it.
#[derive(Clone, Debug, Default)]
pub struct Cutoff(Arc<OnceLock<Order>>);

impl Cutoff {
    fn release(&self) {
        self.0.get_or_init(Order::next);
    }

    /// Whether the key was released before a frame queued at `order`.
    #[must_use]
    pub fn precedes(&self, order: Order) -> bool {
        self.0.get().is_some_and(|released| *released < order)
    }
}

/// A press that opens a hold.
#[derive(Clone, Debug)]
pub struct Press {
    pub number: u64,
    pub release: Cutoff,
}

/// Follows the key so each press is acted on or refused once, however late it is looked at.
pub struct Presses {
    state: watch::Receiver<KeyState>,
    seen: u64,
    lost: u64,
}

impl Presses {
    #[must_use]
    pub fn new(state: watch::Receiver<KeyState>) -> Self {
        Self {
            state,
            seen: 0,
            lost: 0,
        }
    }

    /// Counts the presses so far as dealt with, and the state as seen.
    pub fn skip(&mut self) {
        self.seen = self.state.borrow_and_update().presses;
        self.lost = 0;
    }

    /// Resolves when the key changed since it was last looked at; an error once nothing can
    /// press it any more.
    ///
    /// # Errors
    /// [`watch::error::RecvError`] when the input is gone.
    pub async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        self.state.changed().await
    }

    /// The press that went down since the last one was taken, if the key is still down. Earlier
    /// presses that were released or pressed again before anyone looked are counted by
    /// [`take_lost`](Self::take_lost).
    pub fn take(&mut self) -> Option<Press> {
        let state = self.state.borrow_and_update();
        let unseen = state.presses - self.seen;
        let fresh = state.down && unseen > 0;
        self.lost += unseen - u64::from(fresh);
        self.seen = state.presses;
        fresh.then(|| Press {
            number: state.presses,
            release: state.release.clone(),
        })
    }

    /// How many presses [`take`](Self::take) passed over since this was last asked.
    pub fn take_lost(&mut self) -> u64 {
        std::mem::take(&mut self.lost)
    }

    /// Whether press number `press` is over: the key is up, or has been pressed again.
    pub fn is_over(&mut self, press: u64) -> bool {
        let state = self.state.borrow_and_update();
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

/// Why a press of the hold key started nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Paused,
    MicrophoneClosed,
    Delivering,
    /// A dictation by wake word was already running.
    Capturing,
    /// The key came up before anyone looked: the press was over before it could start anything.
    TooQuick,
}

impl Refusal {
    /// How the metrics name it.
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::Paused => "paused",
            Self::MicrophoneClosed => "microphone_closed",
            Self::Delivering => "delivering",
            Self::Capturing => "capturing",
            Self::TooQuick => "too_quick",
        }
    }

    /// Says in plain words what happened.
    #[must_use]
    pub fn notice(self) -> &'static str {
        match self {
            Self::Paused => {
                "Thalvi is paused, so pressing the key started nothing. Resume listening first."
            }
            Self::MicrophoneClosed => {
                "The microphone is not open, so pressing the key started nothing."
            }
            Self::Delivering => {
                "Thalvi is still delivering the last dictation, so pressing the key started nothing."
            }
            Self::Capturing => {
                "A dictation is already running, so pressing the key started nothing."
            }
            Self::TooQuick => {
                "The key was pressed and released too quickly, so pressing it started nothing."
            }
        }
    }

    /// `notice` as one short line for the overlay.
    #[must_use]
    pub fn headline(self) -> &'static str {
        match self {
            Self::Paused => "Paused — key ignored",
            Self::MicrophoneClosed => "Microphone closed — key ignored",
            Self::Delivering => "Delivering — key ignored",
            Self::Capturing => "Dictating — key ignored",
            Self::TooQuick => "Key released too quickly",
        }
    }

    /// Records `count` presses refused for this reason.
    pub fn record(self, recorder: &Recorder, count: u64) {
        recorder.metric(
            Event::HoldStart,
            "refused",
            None,
            serde_json::json!({"reason": self.reason(), "count": count}),
        );
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
                state.release = Cutoff::default();
            } else {
                state.release.release();
            }
            true
        });
    }
}

/// One keyboard event as the Windows hook reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    pub vk: u16,
    pub scan_code: u16,
    /// The scan code has the extended prefix, as on the arrows and the numeric pad.
    pub extended: bool,
    pub down: bool,
    /// Another program sent it, as Thalvi's own typing does.
    pub injected: bool,
}

/// What the hook does with an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The focused program gets it.
    Pass,
    /// It goes no further: the focused program never sees the hold key.
    Swallow,
    /// This is the release of an Alt or Win key that only the hold key joined. The program gets a
    /// key nothing uses first, then the release, or it would take a lone Alt for the menu bar.
    Mask,
}

/// How many events a mask injects: the unused key down and up, and the replayed release.
pub const MASK_INPUTS: usize = 3;

impl Verdict {
    /// What becomes of a [`Verdict::Mask`] event once `injected` of its [`MASK_INPUTS`] events
    /// went in. The physical release is swallowed only when its replay did, or Windows would
    /// keep the modifier down; a refused injection, as toward an elevated window, lets it pass.
    #[must_use]
    pub fn after_mask(injected: usize) -> Self {
        if injected == MASK_INPUTS {
            Self::Swallow
        } else {
            Self::Pass
        }
    }
}

/// The hook's answer to one event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decision {
    pub verdict: Verdict,
    /// The hold key went down (`true`) or up (`false`).
    pub change: Option<bool>,
}

impl Decision {
    const PASS: Self = Self {
        verdict: Verdict::Pass,
        change: None,
    };

    fn swallow(change: Option<bool>) -> Self {
        Self {
            verdict: Verdict::Swallow,
            change,
        }
    }
}

/// Where the key itself is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Main {
    Up,
    /// Down, and the program got it, so its repeats are the program's too.
    Passed,
    /// Down and kept from the program.
    Swallowed,
}

/// Decides, event by event, which keys are the hold key. Pure: the hook asks and acts.
pub struct HoldHook {
    key: HoldKey,
    main: Main,
    /// Which sides of each modifier are down, as bits.
    down: [u8; 4],
    /// Alt or Win keys whose next release needs the mask.
    masks: Vec<Modifier>,
    /// The hold lasts from the press until the key and all its modifiers are up, in either order:
    /// text typed while a modifier is still down would reach the app as shortcuts.
    holding: bool,
}

impl HoldHook {
    #[must_use]
    pub fn new(key: HoldKey) -> Self {
        Self {
            key,
            main: Main::Up,
            down: [0; 4],
            masks: Vec::new(),
            holding: false,
        }
    }

    /// Ends the hold if nothing of it is down any more; true when it ended.
    fn end_if_clear(&mut self) -> bool {
        let clear = self.main != Main::Swallowed
            && !self
                .key
                .modifiers()
                .iter()
                .any(|modifier| self.down[*modifier as usize] != 0);
        if self.holding && clear {
            self.holding = false;
            return true;
        }
        false
    }

    /// The side-specific modifier keys, as Windows reports them.
    const SIDES: [(u16, Modifier, u8); 8] = [
        (0xA0, Modifier::Shift, 1),
        (0xA1, Modifier::Shift, 2),
        (0xA2, Modifier::Ctrl, 1),
        (0xA3, Modifier::Ctrl, 2),
        (0xA4, Modifier::Alt, 1),
        (0xA5, Modifier::Alt, 2),
        (0x5B, Modifier::Logo, 1),
        (0x5C, Modifier::Logo, 2),
    ];

    /// Sets the tracked modifiers to what the keyboard holds down now. A release the hook never
    /// saw, as on the lock screen, would otherwise keep a modifier down for good.
    fn reconcile(&mut self, physically_down: &impl Fn(u16) -> bool) {
        self.down = [0; 4];
        for (vk, modifier, side) in Self::SIDES {
            if physically_down(vk) {
                self.down[modifier as usize] |= side;
            }
        }
    }

    /// The modifier a virtual key is, and which side.
    fn modifier(vk: u16) -> Option<(Modifier, u8)> {
        Some(match vk {
            0x10 | 0xA0 => (Modifier::Shift, 1),
            0xA1 => (Modifier::Shift, 2),
            0x11 | 0xA2 => (Modifier::Ctrl, 1),
            0xA3 => (Modifier::Ctrl, 2),
            0x12 | 0xA4 => (Modifier::Alt, 1),
            0xA5 => (Modifier::Alt, 2),
            0x5B => (Modifier::Logo, 1),
            0x5C => (Modifier::Logo, 2),
            _ => return None,
        })
    }

    /// Whether exactly the key's modifiers are down: Alt+Shift+Z is another chord than Alt+Z.
    fn chord_down(&self) -> bool {
        [
            Modifier::Ctrl,
            Modifier::Shift,
            Modifier::Alt,
            Modifier::Logo,
        ]
        .into_iter()
        .all(|modifier| {
            (self.down[modifier as usize] != 0) == self.key.modifiers().contains(&modifier)
        })
    }

    /// Decides what to do with `event`. `physically_down` says whether the keyboard holds a
    /// modifier's virtual key down right now; the event itself is applied on top of that, since
    /// Windows may not have updated it for the key of this very event.
    #[must_use]
    pub fn on_event(&mut self, event: KeyEvent, physically_down: impl Fn(u16) -> bool) -> Decision {
        if event.injected {
            return Decision::PASS;
        }
        self.reconcile(&physically_down);
        // A release the hook never saw may have been the last thing the hold waited for.
        let lost = self.end_if_clear().then_some(false);
        let mut decision = if let Some((modifier, side)) = Self::modifier(event.vk) {
            self.on_modifier(modifier, side, event)
        } else if event.scan_code != self.key.scan_code() || event.extended {
            Decision::PASS
        } else {
            self.on_main(event.down)
        };
        decision.change = decision.change.or(lost);
        decision
    }

    fn on_main(&mut self, down: bool) -> Decision {
        match (down, self.main) {
            (true, Main::Swallowed) => Decision::swallow(None),
            (true, Main::Passed) | (false, Main::Up) => Decision::PASS,
            (true, Main::Up) if self.chord_down() => {
                self.main = Main::Swallowed;
                for modifier in self.key.modifiers() {
                    if matches!(modifier, Modifier::Alt | Modifier::Logo)
                        && !self.masks.contains(modifier)
                    {
                        self.masks.push(*modifier);
                    }
                }
                Decision::swallow((!std::mem::replace(&mut self.holding, true)).then_some(true))
            }
            (true, Main::Up) => {
                self.main = Main::Passed;
                Decision::PASS
            }
            (false, Main::Swallowed) => {
                self.main = Main::Up;
                Decision::swallow(self.end_if_clear().then_some(false))
            }
            (false, Main::Passed) => {
                self.main = Main::Up;
                Decision::PASS
            }
        }
    }

    fn on_modifier(&mut self, modifier: Modifier, side: u8, event: KeyEvent) -> Decision {
        let bits = &mut self.down[modifier as usize];
        if event.down {
            *bits |= side;
            return Decision::PASS;
        }
        *bits &= !side;
        let mut decision = Decision::PASS;
        decision.change = self.end_if_clear().then_some(false);
        if self.down[modifier as usize] == 0
            && let Some(at) = self.masks.iter().position(|known| *known == modifier)
        {
            self.masks.remove(at);
            decision.verdict = Verdict::Mask;
        }
        decision
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

    const ALT: u16 = 0xA4;
    const RIGHT_ALT: u16 = 0xA5;
    const SHIFT: u16 = 0xA0;
    const CTRL: u16 = 0xA2;
    const WIN: u16 = 0x5B;
    const Z: u16 = 0x5A;
    const X: u16 = 0x58;
    const F9: u16 = 0x78;

    fn scan_of(vk: u16) -> u16 {
        match vk {
            Z => 0x2C,
            X => 0x2D,
            F9 => 0x43,
            _ => 0x38,
        }
    }

    fn event(vk: u16, down: bool) -> KeyEvent {
        KeyEvent {
            vk,
            scan_code: scan_of(vk),
            extended: false,
            down,
            injected: false,
        }
    }

    fn decision(verdict: Verdict, change: Option<bool>) -> Decision {
        Decision { verdict, change }
    }

    const PASS: Decision = Decision::PASS;
    const SWALLOW: Decision = Decision {
        verdict: Verdict::Swallow,
        change: None,
    };

    /// Feeds `events`, `(vk, down)`, to a hook for `key`, with a keyboard that holds down what the
    /// events pressed, and returns what each one got.
    fn run(key: &str, events: &[(u16, bool)]) -> Vec<Decision> {
        let mut hook = HoldHook::new(HoldKey::parse(key).unwrap());
        let mut keyboard = std::collections::HashSet::new();
        events
            .iter()
            .map(|&(vk, down)| {
                if down {
                    keyboard.insert(vk);
                } else {
                    keyboard.remove(&vk);
                }
                hook.on_event(event(vk, down), |vk| keyboard.contains(&vk))
            })
            .collect()
    }

    #[test]
    fn the_hook_swallows_only_the_hold_keys_press_repeats_and_release_and_reports_each_change() {
        let pressed = decision(Verdict::Swallow, Some(true));
        // The hold ends with the last of the key and its modifiers, here the Alt.
        let released = decision(Verdict::Mask, Some(false));
        let got = run(
            "Alt+KeyZ",
            &[
                (ALT, true),
                (Z, true),
                (Z, true),
                (Z, true),
                (X, true),
                (X, false),
                (Z, false),
                (ALT, false),
                (ALT, true),
                (Z, true),
                (Z, false),
                (ALT, false),
            ],
        );
        assert_eq!(
            got,
            [
                PASS, pressed, SWALLOW, SWALLOW, PASS, PASS, SWALLOW, released, PASS, pressed,
                SWALLOW, released
            ]
        );
    }

    #[test]
    fn the_key_is_the_hold_key_only_with_exactly_its_modifiers_and_not_when_an_app_got_it_first() {
        // (events, what the last one gets)
        let cases: [(&[(u16, bool)], Decision); 5] = [
            (&[(Z, true)], PASS),
            (&[(ALT, true), (SHIFT, true), (Z, true)], PASS),
            (&[(CTRL, true), (Z, true)], PASS),
            // Z typed first: its repeats under a later Alt are still the program's.
            (&[(Z, true), (ALT, true), (Z, true)], PASS),
            (
                &[(RIGHT_ALT, true), (Z, true)],
                decision(Verdict::Swallow, Some(true)),
            ),
        ];
        for (events, expected) in cases {
            assert_eq!(
                run("Alt+KeyZ", events).last(),
                Some(&expected),
                "{events:?}"
            );
        }
    }

    #[test]
    fn events_another_program_sent_are_never_the_hold_key() {
        let mut hook = HoldHook::new(HoldKey::default());
        let alt_down = |vk| vk == ALT;
        assert_eq!(hook.on_event(event(ALT, true), alt_down), PASS);
        let injected = KeyEvent {
            injected: true,
            ..event(Z, true)
        };
        assert_eq!(hook.on_event(injected, alt_down), PASS);
        let extended = KeyEvent {
            extended: true,
            ..event(Z, true)
        };
        assert_eq!(
            hook.on_event(extended, alt_down),
            PASS,
            "the extended twin of a scan code is another key"
        );
    }

    #[test]
    fn an_alt_or_win_release_after_a_swallowed_press_is_masked_once() {
        let mask = decision(Verdict::Mask, None);
        // Z first, then Alt: the menu bar would open on a lone Alt.
        assert_eq!(
            run(
                "Alt+KeyZ",
                &[
                    (ALT, true),
                    (Z, true),
                    (Z, false),
                    (ALT, false),
                    (ALT, true),
                    (ALT, false)
                ]
            ),
            [
                PASS,
                decision(Verdict::Swallow, Some(true)),
                SWALLOW,
                decision(Verdict::Mask, Some(false)),
                PASS,
                PASS
            ]
        );
        // Ctrl needs no mask, and no press means no mask.
        assert_eq!(
            run(
                "Ctrl+KeyZ",
                &[(CTRL, true), (Z, true), (Z, false), (CTRL, false)]
            )
            .last(),
            Some(&decision(Verdict::Pass, Some(false)))
        );
        assert_eq!(
            run("Alt+KeyZ", &[(ALT, true), (ALT, false)]).last(),
            Some(&PASS)
        );
        // Win and Alt both took part: each release is masked.
        let got = run(
            "Alt+Win+KeyZ",
            &[
                (ALT, true),
                (0x5B, true),
                (Z, true),
                (Z, false),
                (0x5B, false),
                (ALT, false),
            ],
        );
        assert_eq!(&got[4..], [mask, decision(Verdict::Mask, Some(false))]);
    }

    #[test]
    fn a_hold_ends_only_when_the_key_and_all_its_modifiers_are_up_in_either_order() {
        let pressed = decision(Verdict::Swallow, Some(true));
        // Key first: the release comes with the Alt, which is still masked.
        assert_eq!(
            run(
                "Alt+KeyZ",
                &[(ALT, true), (Z, true), (Z, false), (ALT, false)]
            ),
            [PASS, pressed, SWALLOW, decision(Verdict::Mask, Some(false))]
        );
        // Alt first: the key's release ends it, and is still swallowed.
        assert_eq!(
            run(
                "Alt+KeyZ",
                &[(ALT, true), (Z, true), (ALT, false), (Z, false)]
            ),
            [
                PASS,
                pressed,
                decision(Verdict::Mask, None),
                decision(Verdict::Swallow, Some(false)),
            ]
        );
        // Pressing the key again while a modifier is still down is the same hold.
        assert_eq!(
            run(
                "Alt+KeyZ",
                &[
                    (ALT, true),
                    (Z, true),
                    (Z, false),
                    (Z, true),
                    (Z, false),
                    (ALT, false)
                ]
            ),
            [
                PASS,
                pressed,
                SWALLOW,
                SWALLOW,
                SWALLOW,
                decision(Verdict::Mask, Some(false)),
            ]
        );
        // A key without modifiers ends with its own release.
        assert_eq!(
            run("F9", &[(F9, true), (F9, false)]),
            [pressed, decision(Verdict::Swallow, Some(false))]
        );
    }

    #[test]
    fn the_modifier_release_ends_the_hold_even_while_windows_still_reports_the_modifier_down() {
        let mut hook = HoldHook::new(HoldKey::default());
        let alt = |vk| vk == ALT;
        let _ = hook.on_event(event(ALT, true), alt);
        let _ = hook.on_event(event(Z, true), alt);
        assert_eq!(hook.on_event(event(Z, false), alt), SWALLOW);
        // Windows has not yet cleared the Alt this very event releases.
        assert_eq!(
            hook.on_event(event(ALT, false), alt),
            decision(Verdict::Mask, Some(false))
        );
    }

    #[test]
    fn a_modifier_release_lost_while_the_key_is_up_still_ends_the_hold_on_the_next_event() {
        let mut hook = HoldHook::new(HoldKey::default());
        let alt = |vk| vk == ALT;
        assert_eq!(hook.on_event(event(ALT, true), alt), PASS);
        assert_eq!(
            hook.on_event(event(Z, true), alt),
            decision(Verdict::Swallow, Some(true))
        );
        assert_eq!(hook.on_event(event(Z, false), alt), SWALLOW);
        // The lock screen took the Alt release; the next key shows the keyboard has none down.
        assert_eq!(
            hook.on_event(event(X, true), |_| false),
            decision(Verdict::Pass, Some(false))
        );
    }

    #[test]
    fn a_modifier_release_the_hook_never_saw_does_not_keep_the_hold_key_from_working() {
        // Win+L: the lock screen takes Win's release, so the hook only saw it go down.
        let mut hook = HoldHook::new(HoldKey::default());
        assert_eq!(hook.on_event(event(WIN, true), |vk| vk == WIN), PASS);
        let keyboard = |vk| vk == ALT;
        assert_eq!(hook.on_event(event(ALT, true), keyboard), PASS);
        assert_eq!(
            hook.on_event(event(Z, true), keyboard),
            decision(Verdict::Swallow, Some(true))
        );
        // A modifier the keyboard really holds still spoils the chord.
        let mut hook = HoldHook::new(HoldKey::default());
        let with_win = |vk| vk == WIN || vk == ALT;
        assert_eq!(hook.on_event(event(ALT, true), with_win), PASS);
        assert_eq!(hook.on_event(event(Z, true), with_win), PASS);
    }

    #[test]
    fn a_mask_whose_replay_did_not_go_in_lets_the_physical_release_through() {
        for (injected, verdict) in [
            (MASK_INPUTS, Verdict::Swallow),
            (MASK_INPUTS - 1, Verdict::Pass),
            (0, Verdict::Pass),
        ] {
            assert_eq!(Verdict::after_mask(injected), verdict, "{injected}");
        }
    }

    #[test]
    fn auto_repeat_is_not_another_press_and_each_release_allows_the_next() {
        let (input, state) = HoldInput::new();
        let seen = || {
            let state = state.borrow();
            (state.presses, state.down)
        };
        input.set(true);
        input.set(true);
        input.set(true);
        assert_eq!(seen(), (1, true));
        input.set(false);
        input.set(false);
        assert_eq!(seen(), (1, false));
        input.set(true);
        assert_eq!(seen(), (2, true));
    }

    #[test]
    fn a_press_released_or_repeated_before_it_is_looked_at_is_counted_as_lost() {
        // (events since the last look, the press taken, presses lost)
        for (events, taken, lost) in [
            (vec![true], Some(1), 0),
            (vec![true, false], None, 1),
            (vec![true, false, true], Some(2), 1),
            (vec![true, false, true, false, true], Some(3), 2),
            (vec![], None, 0),
        ] {
            let (input, state) = HoldInput::new();
            let mut presses = Presses::new(state);
            for down in &events {
                input.set(*down);
            }
            assert_eq!(
                presses.take().map(|press| press.number),
                taken,
                "{events:?}"
            );
            assert_eq!(presses.take_lost(), lost, "{events:?}");
            assert_eq!(presses.take_lost(), 0, "{events:?}: asked once");
        }

        let (input, state) = HoldInput::new();
        let mut presses = Presses::new(state);
        input.set(true);
        input.set(false);
        presses.skip();
        assert!(presses.take().is_none());
        assert_eq!(presses.take_lost(), 0, "a press dealt with is not lost");
    }

    #[test]
    fn a_press_keeps_its_first_release_whatever_presses_follow_before_anyone_looks() {
        let (input, state) = HoldInput::new();
        let mut presses = Presses::new(state);
        input.set(true);
        let press = presses.take().expect("a fresh press");
        let while_down = Order::next();
        input.set(false);
        let after_release = Order::next();
        input.set(false);
        input.set(true);
        input.set(false);
        let after_tap = Order::next();
        assert!(!press.release.precedes(while_down), "no release while down");
        assert!(
            press.release.precedes(after_release),
            "a later tap's release does not move this press's"
        );
        assert!(press.release.precedes(after_tap));
        assert_eq!(presses.take_lost(), 0, "nothing was passed over yet");
        assert!(presses.take().is_none());
        assert_eq!(presses.take_lost(), 1, "the tap is counted once");
    }
}
