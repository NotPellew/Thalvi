//! The hold-to-dictate key: its name in the config and the signal that it is down.

use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::frame::Order;
use crate::host::Modifier;
use crate::metrics::{Event, Recorder};

/// The keys a hold key may be, as W3C `KeyboardEvent.code` names with their Windows scan codes.
const CODES: [(&str, u16); 50] = [
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
    ("AltRight", 0x38),
];

/// Where `KeyZ`, `KeyX`, `F9`, `F10` and `AltRight` are in [`CODES`].
const KEY_Z: usize = 29;
const KEY_X: usize = 30;
const F9: usize = 45;
const F10: usize = 46;
const ALT_RIGHT: usize = 49;

/// Why a hold key name was refused. Shown as written.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HoldKeyError {
    #[error(
        "The key \"{0}\" is not a key name such as Alt+KeyZ: modifiers Ctrl, Shift, Alt or Win, then a key such as KeyZ, Digit1, F9 or Space, joined by +."
    )]
    Unknown(String),
    #[error("The key \"{0}\" needs a modifier, or typing it would start a capture.")]
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
        // AltGr is a hold key only alone; with another modifier it is a chord the app types.
        if code == ALT_RIGHT && !modifiers.is_empty() {
            return Err(unknown());
        }
        if modifiers.is_empty() && !key.starts_with('F') && code != ALT_RIGHT {
            return Err(HoldKeyError::NeedsModifier(name.to_owned()));
        }
        Ok(Self { modifiers, code })
    }

    /// The modifiers that must be down.
    #[must_use]
    pub fn modifiers(&self) -> &[Modifier] {
        &self.modifiers
    }

    /// Whether this is `AltGr` (right Alt) held alone, which is also a modifier of the characters it
    /// types: its hold cannot start before the next key says it is not a chord.
    #[must_use]
    pub fn is_altgr(&self) -> bool {
        self.code == ALT_RIGHT
    }

    /// The key's Windows scan code.
    #[must_use]
    pub fn scan_code(&self) -> u16 {
        CODES[self.code].1
    }
}

impl HoldKey {
    /// Alt with the key left of X: what every config stored for the hold key before defaults were
    /// set per desktop, and the default on Windows.
    #[must_use]
    pub fn alt_z() -> Self {
        Self {
            modifiers: vec![Modifier::Alt],
            code: KEY_Z,
        }
    }

    /// Alt with X: what every config stored for the write key before defaults were set per
    /// desktop, and the default on Windows.
    #[must_use]
    pub fn alt_x() -> Self {
        Self {
            modifiers: vec![Modifier::Alt],
            code: KEY_X,
        }
    }

    /// F9 alone.
    #[must_use]
    pub fn f9() -> Self {
        Self {
            modifiers: Vec::new(),
            code: F9,
        }
    }

    /// F10 alone.
    #[must_use]
    pub fn f10() -> Self {
        Self {
            modifiers: Vec::new(),
            code: F10,
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

/// Which of the two hold keys a hold came from, and so what its release does with the words.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HoldKind {
    /// The hold key: the words are pasted as dictated.
    #[default]
    Dictate,
    /// The write key: the words are written out as proper text, then pasted.
    Write,
}

/// Whether a hold key is down, which one, and how many times a key has been pressed.
#[derive(Clone, Debug, Default)]
pub struct KeyState {
    pub presses: u64,
    pub down: bool,
    /// The key of the latest press.
    pub kind: HoldKind,
    /// The latest press's release.
    pub release: Cutoff,
    /// Where the latest press happened among the queued frames, when it waited to be told from a
    /// chord: the frames since then are the hold's.
    pub since: Option<Order>,
    /// Each key's own phase, so only a key that is up can hold a release that overtook a press.
    phases: [KeyPhase; 2],
    /// Each key's own wait to be told from a chord.
    altgr: [AltGrWait; 2],
    /// Counts the waits, so a late answer to one never settles another.
    waits: u64,
}

/// Where an `AltGr` press is between the press and the answer whether it is a hold.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum AltGrWait {
    #[default]
    Idle,
    /// Down and waiting: another key may still turn it into a chord.
    Waiting { wait: u64, since: Order },
    /// Another key came first: a chord that types, not a hold.
    Chord,
    /// Nothing came first: it started a hold.
    Holding,
}

/// Where one hold key is. A key that is down has no press left for a release to overtake, so only
/// an up key carries the credit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum KeyPhase {
    /// Up, with no release waiting for the press it overtook.
    #[default]
    Up,
    /// Down, whether or not it started the hold.
    Down,
    /// A release arrived while the key was up: the press it overtook is still to come.
    Overtaken(Instant),
}

/// How long after a release that overtook its press that press still counts as released. The
/// Hyprland binds start one process per press and release, and on a quick tap the release's
/// process can reach the app first.
const OVERTAKEN_PRESS: Duration = Duration::from_secs(1);

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
    pub kind: HoldKind,
    pub release: Cutoff,
    /// Where the key went down among the queued frames, for a key that waited to be told from a
    /// chord; the audio since then is kept for the hold.
    pub since: Option<Order>,
}

/// Follows the key so each press is acted on or refused once, however late it is looked at.
#[derive(Clone)]
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
            kind: state.kind,
            release: state.release.clone(),
            since: state.since,
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

/// What the keyboard hook reports about a hold key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoldChange {
    /// The key went down and starts a hold at once.
    Down,
    /// The key came up.
    Up,
    /// `AltGr` went down alone: a hold if no other key joins it within [`ALONE_AFTER`].
    Maybe,
    /// Another key joined `AltGr`: a chord that types.
    Chord,
}

/// How long `AltGr` must be down alone before it starts a hold. `AltGr` is also how the layout types
/// `@`, `€` and `{`, so a press may be the start of a chord for as long as the next key takes.
/// Chords overlap by well under 200 ms; a longer wait would only delay the cue, since the audio
/// from the press on is kept, and 200 ms stays under the shortest hold that dictates.
pub const ALONE_AFTER: Duration = Duration::from_millis(200);

/// Where the hold keys' presses and releases come in, from a compositor bind or the keyboard hook.
#[derive(Clone)]
pub struct HoldInput(Arc<watch::Sender<KeyState>>);

impl HoldInput {
    #[must_use]
    pub fn new() -> (Self, watch::Receiver<KeyState>) {
        let (sender, receiver) = watch::channel(KeyState::default());
        (Self(Arc::new(sender)), receiver)
    }

    /// The hold key went down or up; a repeated press changes nothing, so key auto-repeat starts
    /// nothing extra. A release while up is the next press's, which it overtook.
    pub fn set(&self, down: bool) {
        self.change(HoldKind::Dictate, down);
    }

    /// The write key went down or up, as [`set`](Self::set) for the hold key.
    pub fn set_write(&self, down: bool) {
        self.change(HoldKind::Write, down);
    }

    /// Takes what the keyboard hook reports for `kind`. A key that waited to be told from a chord
    /// starts its hold [`ALONE_AFTER`] after the press, on a thread of its own, unless another key
    /// or its release comes first.
    pub fn apply(&self, kind: HoldKind, change: HoldChange) {
        match change {
            HoldChange::Down => self.change(kind, true),
            HoldChange::Up => self.release(kind),
            HoldChange::Chord => self.chord(kind),
            HoldChange::Maybe => {
                let wait = self.press_alone(kind);
                let input = self.clone();
                let thread = std::thread::Builder::new()
                    .name("thalvi-alone".to_owned())
                    .spawn(move || {
                        std::thread::sleep(ALONE_AFTER);
                        input.confirm_alone(kind, wait);
                    });
                if thread.is_err() {
                    self.confirm_alone(kind, wait);
                }
            }
        }
    }

    /// `AltGr` went down alone: starts its wait, which [`apply`](Self::apply) ends after
    /// [`ALONE_AFTER`], and says which wait it is.
    #[must_use]
    pub fn press_alone(&self, kind: HoldKind) -> u64 {
        let mut wait = 0;
        self.0.send_if_modified(|state| {
            // A release the hook never saw ends the earlier press first.
            let ended = state.altgr[kind as usize] == AltGrWait::Holding
                && Self::step(state, kind, false, None);
            state.waits += 1;
            wait = state.waits;
            state.altgr[kind as usize] = AltGrWait::Waiting {
                wait,
                since: Order::next(),
            };
            ended
        });
        wait
    }

    /// The wait `wait` of `kind` ran out with `AltGr` still alone: its hold starts, with the frames
    /// since the press.
    pub fn confirm_alone(&self, kind: HoldKind, wait: u64) {
        self.0.send_if_modified(|state| {
            let AltGrWait::Waiting {
                wait: current,
                since,
            } = state.altgr[kind as usize]
            else {
                return false;
            };
            if current != wait {
                return false;
            }
            state.altgr[kind as usize] = AltGrWait::Holding;
            Self::step(state, kind, true, Some(since))
        });
    }

    /// Another key joined `AltGr`, which makes a chord unless the hold started already.
    fn chord(&self, kind: HoldKind) {
        self.0.send_if_modified(|state| {
            if matches!(state.altgr[kind as usize], AltGrWait::Waiting { .. }) {
                state.altgr[kind as usize] = AltGrWait::Chord;
            }
            false
        });
    }

    /// The key came up: the end of a hold, or of a press that never became one.
    fn release(&self, kind: HoldKind) {
        self.0.send_if_modified(|state| {
            let index = kind as usize;
            match std::mem::take(&mut state.altgr[index]) {
                AltGrWait::Idle | AltGrWait::Holding => Self::step(state, kind, false, None),
                AltGrWait::Waiting { .. } | AltGrWait::Chord => false,
            }
        });
    }

    fn change(&self, kind: HoldKind, down: bool) {
        self.0
            .send_if_modified(|state| Self::step(state, kind, down, None));
    }

    /// Applies one event of `kind` to `state` and says whether the state changed for the engine.
    /// `since` is where a press that waited went down among the queued frames.
    fn step(state: &mut KeyState, kind: HoldKind, down: bool, since: Option<Order>) -> bool {
        let index = kind as usize;
        let phase = state.phases[index];
        // A press of a tap whose release came first is over already, so it opens no hold.
        let overtaken = matches!(phase, KeyPhase::Overtaken(at) if at.elapsed() < OVERTAKEN_PRESS);
        // Every key follows its own events, even while the other key's hold ignores them.
        state.phases[index] = if down {
            if overtaken {
                KeyPhase::Up
            } else {
                KeyPhase::Down
            }
        } else if phase == KeyPhase::Down {
            KeyPhase::Up
        } else {
            KeyPhase::Overtaken(Instant::now())
        };
        // One hold at a time: the other key means nothing until this one is up.
        if state.down && state.kind != kind {
            return false;
        }
        if state.down == down {
            return false;
        }
        if down {
            state.presses += 1;
            state.kind = kind;
            state.since = since;
            state.release = Cutoff::default();
            if !overtaken {
                state.down = true;
                return true;
            }
            state.release.release();
        } else {
            state.release.release();
            state.down = false;
        }
        true
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
    /// Windows' time stamp of the event in milliseconds; the key events of one keystroke share it.
    pub time: u32,
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
    /// `AltGr` went down alone, or another key joined it.
    pub alone: Option<Alone>,
}

/// What the hook knows about an `AltGr` hold key that is down: whether another key has joined it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alone {
    /// `AltGr` went down with nothing else down.
    Maybe,
    /// Another key went down while it was.
    Chord,
}

impl Decision {
    const PASS: Self = Self {
        verdict: Verdict::Pass,
        change: None,
        alone: None,
    };

    fn swallow(change: Option<bool>) -> Self {
        Self {
            verdict: Verdict::Swallow,
            change,
            alone: None,
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
    /// Where `AltGr` is, for a key that is `AltGr` alone.
    altgr: AltGrPhase,
    /// The left Ctrl down now is the one Windows adds to `AltGr`, not the user's.
    altgr_ctrl: bool,
    /// The event before this one.
    previous: Option<KeyEvent>,
}

/// Where an `AltGr` hold key is, as the hook sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AltGrPhase {
    Up,
    /// Down with no other key down: a hold unless another key joins; whether one did.
    Alone {
        chorded: bool,
    },
    /// Down with another key already down: a chord from the start.
    Chord,
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
            altgr: AltGrPhase::Up,
            altgr_ctrl: false,
            previous: None,
        }
    }

    const LEFT_CTRL: u16 = 0xA2;
    const RIGHT_ALT_KEY: u16 = 0xA5;

    /// A key that is `AltGr` alone: unlike every other hold key it is down while the user may still
    /// be typing a chord with it, so it passes every event, tells the app apart from a chord by
    /// the next key, and ignores the left Ctrl that Windows adds to `AltGr`.
    fn on_altgr_key(
        &mut self,
        event: KeyEvent,
        physically_down: &impl Fn(u16) -> bool,
    ) -> Decision {
        let previous = self.previous.replace(event);
        let follows = |vk: fn(u16) -> bool, down: bool| {
            previous.is_some_and(|before| {
                before.down == down && vk(before.vk) && before.time == event.time
            })
        };
        let is_ctrl = |vk: u16| vk == Self::LEFT_CTRL;
        let is_altgr = |vk: u16| vk == Self::RIGHT_ALT_KEY;
        let mut decision = Decision::PASS;
        if is_altgr(event.vk) {
            match (event.down, self.altgr) {
                (true, AltGrPhase::Up) => {
                    self.altgr_ctrl = follows(|vk| vk == Self::LEFT_CTRL, true);
                    let others = Self::SIDES.iter().any(|(vk, _, _)| {
                        *vk != Self::RIGHT_ALT_KEY
                            && !(self.altgr_ctrl && is_ctrl(*vk))
                            && physically_down(*vk)
                    });
                    if others {
                        self.altgr = AltGrPhase::Chord;
                    } else {
                        self.altgr = AltGrPhase::Alone { chorded: false };
                        decision.alone = Some(Alone::Maybe);
                    }
                }
                (true, _) => {}
                (false, phase) => {
                    self.altgr = AltGrPhase::Up;
                    self.altgr_ctrl = false;
                    if let AltGrPhase::Alone { chorded } = phase {
                        decision.change = Some(false);
                        // A lone Alt press and release opens the app's menu bar; a chord does not.
                        if !chorded {
                            decision.verdict = Verdict::Mask;
                        }
                    }
                }
            }
        } else if event.down
            && let AltGrPhase::Alone { chorded: false } = self.altgr
        {
            let added = is_ctrl(event.vk)
                && (self.altgr_ctrl || follows(|vk| vk == Self::RIGHT_ALT_KEY, true));
            if added {
                self.altgr_ctrl = true;
            } else {
                self.altgr = AltGrPhase::Alone { chorded: true };
                decision.alone = Some(Alone::Chord);
            }
        }
        decision
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
        if self.key.is_altgr() {
            return self.on_altgr_key(event, &physically_down);
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

/// Decides, event by event, which keys are the hold keys, one [`HoldHook`] per key. Pure.
pub struct HoldHooks(Vec<(HoldKind, HoldHook)>);

impl HoldHooks {
    #[must_use]
    pub fn new(keys: Vec<(HoldKind, HoldKey)>) -> Self {
        Self(
            keys.into_iter()
                .map(|(kind, key)| (kind, HoldHook::new(key)))
                .collect(),
        )
    }

    /// What every key makes of `event`: the strongest verdict, and the keys that went down or up.
    #[must_use]
    pub fn on_event(
        &mut self,
        event: KeyEvent,
        physically_down: impl Fn(u16) -> bool,
    ) -> (Verdict, Vec<(HoldKind, HoldChange)>) {
        let mut verdict = Verdict::Pass;
        let mut changes = Vec::new();
        for (kind, hook) in &mut self.0 {
            let decision = hook.on_event(event, &physically_down);
            verdict = match (verdict, decision.verdict) {
                (Verdict::Swallow, _) | (_, Verdict::Swallow) => Verdict::Swallow,
                (Verdict::Mask, _) | (_, Verdict::Mask) => Verdict::Mask,
                _ => Verdict::Pass,
            };
            changes.extend(decision.change.map(|down| {
                (
                    *kind,
                    if down {
                        HoldChange::Down
                    } else {
                        HoldChange::Up
                    },
                )
            }));
            changes.extend(decision.alone.map(|alone| {
                (
                    *kind,
                    match alone {
                        Alone::Maybe => HoldChange::Maybe,
                        Alone::Chord => HoldChange::Chord,
                    },
                )
            }));
        }
        (verdict, changes)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::{Just, Strategy};

    use super::*;

    #[test]
    fn a_hold_key_is_modifiers_and_a_physical_key_and_reads_back_as_written() {
        for (name, scan_code, alt) in [
            ("Alt+KeyZ", 0x2C, true),
            ("Alt+KeyX", 0x2D, true),
            ("Ctrl+Alt+Digit1", 0x02, true),
            ("Shift+Win+Space", 0x39, false),
            ("F9", 0x43, false),
            ("AltRight", 0x38, false),
        ] {
            let key = HoldKey::parse(name).unwrap();
            assert_eq!(key.to_string(), name);
            assert_eq!(key.is_altgr(), name == "AltRight", "{name}");
            assert_eq!(key.scan_code(), scan_code, "{name}");
            assert_eq!(key.modifiers().contains(&Modifier::Alt), alt, "{name}");
        }
        for (key, name) in [
            (HoldKey::alt_z(), "Alt+KeyZ"),
            (HoldKey::alt_x(), "Alt+KeyX"),
            (HoldKey::f9(), "F9"),
            (HoldKey::f10(), "F10"),
        ] {
            assert_eq!(key, HoldKey::parse(name).unwrap(), "{name}");
        }
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
            ("Alt+AltRight", false),
            ("Ctrl+AltRight", false),
        ] {
            let error = HoldKey::parse(name).unwrap_err();
            assert_eq!(
                matches!(error, HoldKeyError::NeedsModifier(_)),
                needs_modifier,
                "{name}: {error}"
            );
        }
        assert_eq!(
            HoldKey::parse("KeyZ").unwrap_err().to_string(),
            "The key \"KeyZ\" needs a modifier, or typing it would start a capture."
        );
    }

    const ALT: u16 = 0xA4;
    const RIGHT_ALT: u16 = 0xA5;
    const SHIFT: u16 = 0xA0;
    const RIGHT_SHIFT: u16 = 0xA1;
    const CTRL: u16 = 0xA2;
    const RIGHT_CTRL: u16 = 0xA3;
    const WIN: u16 = 0x5B;
    const RIGHT_WIN: u16 = 0x5C;
    const Z: u16 = 0x5A;
    const X: u16 = 0x58;
    const F9: u16 = 0x78;
    /// A hold key with the keys that can start, prolong or spoil it: picking from every key would
    /// rarely put down an exact chord.
    const KEYS_OF_A_HOLD: [(&str, &[u16]); 4] = [
        ("F9", &[F9, ALT, Z]),
        ("Alt+KeyZ", &[ALT, RIGHT_ALT, SHIFT, Z, X]),
        ("Ctrl+Alt+KeyZ", &[CTRL, RIGHT_CTRL, ALT, Z]),
        ("Shift+Win+KeyX", &[SHIFT, RIGHT_SHIFT, WIN, RIGHT_WIN, X]),
    ];

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
            time: 0,
        }
    }

    fn decision(verdict: Verdict, change: Option<bool>) -> Decision {
        Decision {
            verdict,
            change,
            alone: None,
        }
    }

    const PASS: Decision = Decision::PASS;
    const SWALLOW: Decision = Decision {
        verdict: Verdict::Swallow,
        change: None,
        alone: None,
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
        let mut hook = HoldHook::new(HoldKey::alt_z());
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
        let mut hook = HoldHook::new(HoldKey::alt_z());
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
        let mut hook = HoldHook::new(HoldKey::alt_z());
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
        let mut hook = HoldHook::new(HoldKey::alt_z());
        assert_eq!(hook.on_event(event(WIN, true), |vk| vk == WIN), PASS);
        let keyboard = |vk| vk == ALT;
        assert_eq!(hook.on_event(event(ALT, true), keyboard), PASS);
        assert_eq!(
            hook.on_event(event(Z, true), keyboard),
            decision(Verdict::Swallow, Some(true))
        );
        // A modifier the keyboard really holds still spoils the chord.
        let mut hook = HoldHook::new(HoldKey::alt_z());
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
        assert_eq!(seen(), (1, false));
        input.set(true);
        assert_eq!(seen(), (2, true));
    }

    #[test]
    fn a_press_says_which_key_it_was_and_the_other_key_means_nothing_during_a_hold() {
        let (input, state) = HoldInput::new();
        let mut presses = Presses::new(state);
        let kind = |presses: &mut Presses| presses.take().map(|press| press.kind);
        input.set_write(true);
        assert_eq!(kind(&mut presses), Some(HoldKind::Write));
        // The hold key is no hold while the write key is down, and neither is its release.
        input.set(true);
        input.set(false);
        assert!(!presses.is_over(1), "the write hold goes on");
        assert_eq!(kind(&mut presses), None);
        assert_eq!(presses.take_lost(), 0);
        input.set_write(false);
        assert!(presses.is_over(1));
        input.set(true);
        assert_eq!(kind(&mut presses), Some(HoldKind::Dictate));
    }

    #[tokio::test(start_paused = true)]
    async fn a_release_that_overtook_its_press_belongs_to_the_same_key() {
        let (input, state) = HoldInput::new();
        let mut presses = Presses::new(state);
        input.set_write(false);
        input.set(true);
        assert_eq!(
            presses.take().map(|press| press.kind),
            Some(HoldKind::Dictate),
            "another key's early release does not cancel this press"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_key_released_after_the_other_keys_hold_starts_the_next_hold() {
        // (the key held first, the key pressed during it and released after it)
        for (held, touched) in [
            (HoldKind::Write, HoldKind::Dictate),
            (HoldKind::Dictate, HoldKind::Write),
        ] {
            let (input, state) = HoldInput::new();
            let mut presses = Presses::new(state);
            let set = |kind: HoldKind, down: bool| match kind {
                HoldKind::Dictate => input.set(down),
                HoldKind::Write => input.set_write(down),
            };
            set(held, true);
            assert_eq!(
                presses.take().map(|press| press.kind),
                Some(held),
                "{held:?}"
            );
            set(touched, true);
            assert_eq!(presses.take().map(|press| press.kind), None, "{held:?}");
            set(held, false);
            assert!(presses.is_over(1), "{held:?}: its release ends the hold");
            set(touched, false);
            assert_eq!(presses.take().map(|press| press.kind), None, "{held:?}");
            assert_eq!(presses.take_lost(), 0, "{held:?}");
            tokio::time::advance(Duration::from_millis(999)).await;
            set(touched, true);
            let press = presses.take().expect("the key starts a hold");
            assert_eq!(press.kind, touched, "{held:?}");
            assert_eq!(presses.take_lost(), 0, "{held:?}");
            assert!(
                !presses.is_over(press.number),
                "{held:?}: the hold lasts while the key is down"
            );
            set(touched, false);
            assert!(
                presses.is_over(press.number),
                "{held:?}: and ends with its release"
            );
        }
    }

    #[test]
    fn two_keys_are_two_holds_and_the_strongest_verdict_wins() {
        let mut hooks = HoldHooks::new(vec![
            (HoldKind::Dictate, HoldKey::alt_z()),
            (HoldKind::Write, HoldKey::alt_x()),
        ]);
        let with_alt = |vk| vk == ALT;
        let step = |hooks: &mut HoldHooks, vk, down, keyboard: &dyn Fn(u16) -> bool| {
            hooks.on_event(event(vk, down), keyboard)
        };
        assert_eq!(
            step(&mut hooks, ALT, true, &with_alt),
            (Verdict::Pass, vec![])
        );
        assert_eq!(
            step(&mut hooks, X, true, &with_alt),
            (Verdict::Swallow, vec![(HoldKind::Write, HoldChange::Down)])
        );
        assert_eq!(
            step(&mut hooks, X, false, &with_alt),
            (Verdict::Swallow, vec![]),
            "the write hold lasts until Alt is up too"
        );
        assert_eq!(
            step(&mut hooks, Z, true, &with_alt),
            (
                Verdict::Swallow,
                vec![(HoldKind::Dictate, HoldChange::Down)]
            )
        );
        assert_eq!(
            step(&mut hooks, ALT, false, &|_| false),
            (Verdict::Mask, vec![(HoldKind::Write, HoldChange::Up)]),
            "one mask for both keys, and only the write hold is over"
        );
        assert_eq!(
            step(&mut hooks, Z, false, &|_| false),
            (Verdict::Swallow, vec![(HoldKind::Dictate, HoldChange::Up)])
        );
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

    #[tokio::test(start_paused = true)]
    async fn a_press_that_arrives_after_its_own_release_opens_nothing() {
        // (time between the release and the press, whether the press is still down)
        for (gap, down) in [
            (Duration::ZERO, false),
            (Duration::from_millis(999), false),
            (OVERTAKEN_PRESS, true),
        ] {
            let (input, state) = HoldInput::new();
            let mut presses = Presses::new(state);
            input.set(false);
            tokio::time::advance(gap).await;
            input.set(true);
            let taken = presses.take();
            assert_eq!(taken.is_some(), down, "{gap:?}");
            assert_eq!(presses.take_lost(), u64::from(!down), "{gap:?}");
            if !down {
                input.set(true);
                assert!(presses.take().is_some(), "{gap:?}: the next press opens");
            }
        }
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

    proptest::proptest! {
        #[test]
        fn any_key_order_leaves_one_hold_at_a_time_and_the_program_with_balanced_keys(
            (key, steps) in proptest::sample::select(&KEYS_OF_A_HOLD[..]).prop_flat_map(|(key, keys)| {
                let step = (proptest::sample::select(keys), proptest::bool::ANY);
                (Just(key), proptest::collection::vec(step, 0..40))
            }),
            windows_lags in proptest::bool::ANY,
        ) {
            let mut hook = HoldHook::new(HoldKey::parse(key).unwrap());
            let mut keyboard = std::collections::BTreeSet::new();
            let mut program = std::collections::BTreeSet::new();
            let mut holding = false;
            let mut left_down = std::collections::BTreeSet::new();
            let mut events = Vec::new();
            for (vk, repeats) in steps {
                let is_down = left_down.contains(&vk);
                if !repeats && is_down {
                    left_down.remove(&vk);
                    events.push((vk, false));
                } else if !repeats || is_down {
                    left_down.insert(vk);
                    events.push((vk, true));
                }
            }
            events.extend(left_down.into_iter().map(|vk| (vk, false)));
            for (vk, down) in events {
                let before = keyboard.clone();
                if down {
                    keyboard.insert(vk);
                } else {
                    keyboard.remove(&vk);
                }
                let seen = if windows_lags { &before } else { &keyboard };
                let decision = hook.on_event(event(vk, down), |vk| seen.contains(&vk));
                if let Some(change) = decision.change {
                    proptest::prop_assert_ne!(change, holding, "a hold starts and ends in turn");
                    holding = change;
                }
                if decision.verdict == Verdict::Swallow {
                    proptest::prop_assert!(!program.contains(&vk), "the program saw {vk:#x} go down, so it sees it go up");
                } else if down {
                    program.insert(vk);
                } else {
                    proptest::prop_assert!(program.remove(&vk), "the program never saw {vk:#x} go down");
                }
            }
            proptest::prop_assert!(!holding, "every key is up, so the hold is over");
        }
    }

    const LEFT_CTRL: u16 = 0xA2;
    const Q: u16 = 0x51;

    /// Feeds `events`, `(vk, down, time)`, to a hook for `AltGr` alone, with a keyboard that holds
    /// down what the events pressed, and returns what each one got.
    fn run_altgr(events: &[(u16, bool, u32)]) -> Vec<Decision> {
        let mut hook = HoldHook::new(HoldKey::parse("AltRight").unwrap());
        let mut keyboard = std::collections::HashSet::new();
        events
            .iter()
            .map(|&(vk, down, time)| {
                if down {
                    keyboard.insert(vk);
                } else {
                    keyboard.remove(&vk);
                }
                let event = KeyEvent {
                    extended: matches!(vk, RIGHT_ALT | RIGHT_CTRL),
                    time,
                    ..event(vk, down)
                };
                hook.on_event(event, |vk| keyboard.contains(&vk))
            })
            .collect()
    }

    type AltGrCase<'a> = (&'a str, &'a [(u16, bool, u32)], Vec<Decision>);

    #[test]
    fn altgr_alone_starts_a_wait_and_a_key_joining_it_makes_a_chord_that_types() {
        let maybe = Decision {
            alone: Some(Alone::Maybe),
            ..PASS
        };
        let chord = Decision {
            alone: Some(Alone::Chord),
            ..PASS
        };
        // The hook never swallows an event, so the chord still types: only the release of a lone
        // press is masked, and every release of a press that waited tells the input.
        let lone_release = decision(Verdict::Mask, Some(false));
        let chord_release = decision(Verdict::Pass, Some(false));
        // (events as (key, down, time), what each gets)
        let cases: [AltGrCase; 8] = [
            (
                "US layout: right Alt alone",
                &[(RIGHT_ALT, true, 100), (RIGHT_ALT, false, 900)],
                vec![maybe, lone_release],
            ),
            (
                "German layout: the Ctrl Windows adds is not a modifier, and its repeats are not keys",
                &[
                    (LEFT_CTRL, true, 100),
                    (RIGHT_ALT, true, 100),
                    (LEFT_CTRL, true, 700),
                    (RIGHT_ALT, true, 700),
                    (LEFT_CTRL, false, 900),
                    (RIGHT_ALT, false, 900),
                ],
                vec![PASS, maybe, PASS, PASS, PASS, lone_release],
            ),
            (
                "AltGr+Q types @",
                &[
                    (LEFT_CTRL, true, 100),
                    (RIGHT_ALT, true, 100),
                    (Q, true, 160),
                    (Q, false, 220),
                    (RIGHT_ALT, false, 300),
                ],
                vec![PASS, maybe, chord, PASS, chord_release],
            ),
            (
                "another key before AltGr is a chord from the start",
                &[
                    (SHIFT, true, 50),
                    (RIGHT_ALT, true, 100),
                    (RIGHT_ALT, false, 300),
                ],
                vec![PASS, PASS, PASS],
            ),
            (
                "a real Ctrl held before AltGr is not the Ctrl Windows adds",
                &[
                    (LEFT_CTRL, true, 50),
                    (RIGHT_ALT, true, 100),
                    (RIGHT_ALT, false, 300),
                ],
                vec![PASS, PASS, PASS],
            ),
            (
                "left Alt with right Alt is a chord from the start",
                &[
                    (ALT, true, 50),
                    (RIGHT_ALT, true, 100),
                    (RIGHT_ALT, false, 300),
                ],
                vec![PASS, PASS, PASS],
            ),
            (
                "a modifier pressed after AltGr joins it",
                &[
                    (RIGHT_ALT, true, 100),
                    (RIGHT_SHIFT, true, 160),
                    (RIGHT_ALT, false, 300),
                ],
                vec![maybe, chord, chord_release],
            ),
            (
                "the key before the Ctrl and the right Alt does not pair them",
                &[
                    (Z, true, 90),
                    (LEFT_CTRL, true, 100),
                    (RIGHT_ALT, true, 150),
                ],
                vec![PASS, PASS, PASS],
            ),
        ];
        for (name, events, expected) in cases {
            assert_eq!(run_altgr(events), expected, "{name}");
        }
    }

    #[test]
    fn altgr_starts_its_hold_only_if_nothing_joined_or_released_it_before_the_wait_ran_out() {
        #[derive(Clone, Copy)]
        enum Step {
            /// `AltGr` goes down alone, starting a wait.
            Wait,
            /// The wait with this number runs out.
            Confirm(usize),
            Chord,
            Release,
        }
        use Step::{Chord, Confirm, Release, Wait};
        // (steps, whether a hold is open after them)
        let cases: [(&[Step], bool); 6] = [
            (&[Wait, Confirm(0)], true),
            (&[Wait, Chord, Confirm(0)], false),
            (&[Wait, Release, Confirm(0)], false),
            // The wait of an earlier press never starts the hold of a later one.
            (&[Wait, Release, Wait, Confirm(0)], false),
            (&[Wait, Release, Wait, Confirm(1)], true),
            // A key pressed once the hold started leaves the hold open.
            (&[Wait, Confirm(0), Chord], true),
        ];
        for (index, (steps, open)) in cases.into_iter().enumerate() {
            let (input, state) = HoldInput::new();
            let mut presses = Presses::new(state);
            let mut waits = Vec::new();
            let mut down = false;
            for step in steps {
                match step {
                    Wait => {
                        waits.push(input.press_alone(HoldKind::Dictate));
                        down = true;
                    }
                    Confirm(wait) => input.confirm_alone(HoldKind::Dictate, waits[*wait]),
                    Chord => input.apply(HoldKind::Dictate, HoldChange::Chord),
                    Release => {
                        input.apply(HoldKind::Dictate, HoldChange::Up);
                        down = false;
                    }
                }
            }
            let press = presses.take();
            assert_eq!(press.is_some(), open, "case {index}");
            assert_eq!(
                press.is_some_and(|press| press.since.is_some()),
                open,
                "a hold that waited keeps where the key went down, case {index}"
            );
            assert_eq!(presses.take_lost(), 0, "case {index}");

            // Whatever became of the press, its release leaves nothing behind that spoils the
            // next press of the key.
            if down {
                input.apply(HoldKind::Dictate, HoldChange::Up);
            }
            input.set(true);
            assert!(
                presses.take().is_some_and(|press| press.since.is_none()),
                "the next press opens a hold at once, case {index}"
            );
        }
    }
}
