//! Every bounded sequence of two key rows' events, in order and with overtaken taps, against the
//! real `HoldInput` and `Presses`, for each of the four modes; and every bounded sequence of events
//! of an AltGr key that waits to be told from a chord, beside a plain Hold key.

use thalvi_desktop::hold::{Action, HoldChange, HoldInput, KeyBind, KeyMode, KeyName, Presses};

/// Longest physical sequence looked at after every arrival, as the engine's `scan` does.
const EAGER_EVENTS: usize = 8;
/// Longest physical sequence looked at after any subset of arrivals.
const LATE_EVENTS: usize = 7;
/// The modes a row can work in, each checked with the same event bounds.
const MODES: [KeyMode; 4] = [
    KeyMode::Hold,
    KeyMode::Toggle,
    KeyMode::Start,
    KeyMode::Stop,
];

/// The two rows every family drives: the dictate row in `mode`, and a plain Hold write row.
fn binds(mode: KeyMode) -> [KeyBind; 2] {
    [
        KeyBind::new(Action::Dictate, mode, KeyName::f9()),
        KeyBind::new(Action::Write, KeyMode::Hold, KeyName::f10()),
    ]
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Key {
    Hold,
    Write,
}

fn key_of(action: Action) -> Key {
    match action {
        Action::Dictate => Key::Hold,
        Action::Write => Key::Write,
    }
}

#[derive(Clone, Copy, Debug)]
struct Event {
    key: Key,
    down: bool,
}

/// Every physical history of up to `events` key changes, both keys starting up.
fn histories(events: usize) -> Vec<Vec<Event>> {
    let mut all = Vec::new();
    for length in 1..=events {
        for choice in 0..(1u32 << length) {
            let mut down = [false, false];
            let history = (0..length)
                .map(|step| {
                    let key = if choice >> step & 1 == 0 { Key::Hold } else { Key::Write };
                    let slot = key as usize;
                    down[slot] = !down[slot];
                    Event { key, down: down[slot] }
                })
                .collect();
            all.push(history);
        }
    }
    all
}

/// The orders the app can receive `history` in: as it happened, or with any quick tap's release
/// arriving before its press, as two Hyprland bind processes can.
fn arrivals(history: &[Event]) -> Vec<Vec<usize>> {
    let taps: Vec<usize> = (0..history.len().saturating_sub(1))
        .filter(|&at| {
            history[at].down && !history[at + 1].down && history[at].key == history[at + 1].key
        })
        .collect();
    (0..(1u32 << taps.len()))
        .map(|swapped| {
            let mut order: Vec<usize> = (0..history.len()).collect();
            for (bit, &at) in taps.iter().enumerate() {
                if swapped >> bit & 1 == 1 {
                    order.swap(at, at + 1);
                }
            }
            order
        })
        .collect()
}

fn describe(history: &[Event], order: &[usize], looks: u32, upto: usize, mode: KeyMode) -> String {
    let mut text = format!("the dictate row is {mode:?}");
    for (step, &at) in order[..=upto].iter().enumerate() {
        let event = history[at];
        let key = match event.key {
            Key::Hold => "dictate key",
            Key::Write => "write key",
        };
        let moved = if event.down { "down" } else { "up" };
        let late = if !event.down && at != step { " (overtook its press)" } else { "" };
        let look = if looks >> step & 1 == 1 { ", then the engine looks" } else { "" };
        text.push_str(&format!("\n{}. {key} {moved}{late}{look}", step + 1));
    }
    text
}

/// Plays one arrival order, looking where `looks` says; the first broken property and its step.
fn play(
    mode: KeyMode,
    history: &[Event],
    order: &[usize],
    looks: u32,
    eager: bool,
) -> Option<(&'static str, usize, String)> {
    let rows = binds(mode);
    let (input, state) = HoldInput::new();
    let counted = state.clone();
    let mut presses = Presses::new(state);
    let mut happened = 0;
    let (mut taken, mut lost) = (0u64, 0u64);
    let mut hold: Option<(Key, u64)> = None;
    for (step, &at) in order.iter().enumerate() {
        let event = history[at];
        let row = event.key as usize;
        input.key(row, &rows[row], event.down);
        happened = happened.max(at + 1);
        let down = |key: Key| {
            history[..happened].iter().rev().find(|e| e.key == key).is_some_and(|e| e.down)
        };
        if looks >> step & 1 == 0 {
            continue;
        }
        if let Some((key, number)) = hold {
            let over = presses.is_over(number);
            if eager && over == down(key) {
                let what = if over { "the hold ended while its key was still down" } else { "the hold stayed open after its key came up" };
                return Some(("HoldFollowsItsKey", step, what.to_owned()));
            }
            if over {
                hold = None;
            }
        }
        let free = hold.is_none();
        let mut waiting = Vec::new();
        while let Some(press) = presses.take() {
            taken += 1;
            waiting.push(press);
        }
        let press = waiting.into_iter().next();
        let refused = presses.take_lost();
        lost += refused;
        let total = counted.borrow().presses;
        if taken + lost != total {
            return Some((
                "EachPressCountedOnce",
                step,
                format!("{total} presses, but {taken} taken and {lost} refused"),
            ));
        }
        if let Some(press) = &press {
            if press.mode == KeyMode::Hold {
                let key = key_of(press.action);
                if eager && !down(key) {
                    return Some(("HoldFollowsItsKey", step, "a hold started while its key was up".to_owned()));
                }
                if !free {
                    return Some(("HoldFollowsItsKey", step, "a second hold started during a hold".to_owned()));
                }
                hold = Some((key, press.number));
            }
        }
        let mode_of = |key: Key| if key == Key::Hold { mode } else { KeyMode::Hold };
        let other = match event.key {
            Key::Hold => Key::Write,
            Key::Write => Key::Hold,
        };
        if eager && event.down && at == step {
            // A Hold row waits for the other Hold key to be up; every other mode reports each press
            // even during a hold, since a press of it ends the capture running.
            let wanted = match mode_of(event.key) {
                KeyMode::Hold => free && !down(other),
                KeyMode::Toggle | KeyMode::Start | KeyMode::Stop => true,
            };
            let started = press.as_ref().map(|press| key_of(press.action));
            if wanted && started != Some(event.key) {
                return Some((
                    "FreeKeyStartsCapture",
                    step,
                    format!("the key went down and the engine got {started:?} and {refused} refused"),
                ));
            }
        }
    }
    None
}

/// One arrival at `HoldInput` when the dictate key is AltGr alone, which waits to be told from a
/// chord, and the write key is a plain key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// AltGr goes down with nothing else down: the hook reports `Maybe`.
    Press,
    /// Another key goes down while AltGr is down: the hook reports `Chord`.
    Chord,
    /// The wait of the press that is down, or was last down, runs out.
    Confirm,
    /// The wait of an earlier press runs out after a later press began.
    ConfirmOld,
    /// AltGr comes up: the hook reports `Up`.
    Release,
    WriteDown,
    WriteUp,
}

/// What a correct `HoldInput` has to make of the steps so far.
#[derive(Clone, Copy, Default)]
struct Model {
    down: bool,
    /// The wait of the press that is down ran out with AltGr still alone.
    confirmed: bool,
    chord_sent: bool,
    /// The wait of the latest press has not run out.
    current: bool,
    /// Waits of earlier presses that have not run out.
    old: u32,
    write: bool,
}

impl Model {
    fn legal(&self, step: Step) -> bool {
        match step {
            Step::Press => !self.down,
            Step::Chord => self.down && !self.chord_sent,
            Step::Confirm => self.current,
            Step::ConfirmOld => self.old > 0,
            Step::Release => self.down,
            Step::WriteDown => !self.write,
            Step::WriteUp => self.write,
        }
    }

    /// Whether the step makes AltGr's capture start: its wait ran out with the key alone.
    fn starts(&self, step: Step) -> bool {
        step == Step::Confirm && self.down && !self.chord_sent
    }

    fn apply(&mut self, step: Step) {
        match step {
            Step::Press => {
                self.old += u32::from(self.current);
                *self = Self { down: true, current: true, old: self.old, write: self.write, ..Self::default() };
            }
            Step::Chord => self.chord_sent = true,
            Step::Confirm => {
                self.confirmed |= self.starts(step);
                self.current = false;
            }
            Step::ConfirmOld => self.old -= 1,
            Step::Release => {
                self.down = false;
                self.confirmed = false;
            }
            Step::WriteDown => self.write = true,
            Step::WriteUp => self.write = false,
        }
    }

    /// Whether a Hold row is down and its wait ran out.
    fn holding(&self) -> bool {
        self.down && self.confirmed
    }
}

const STEPS: [Step; 7] =
    [Step::Press, Step::Chord, Step::Confirm, Step::ConfirmOld, Step::Release, Step::WriteDown, Step::WriteUp];

/// Every legal sequence of up to `events` steps.
fn step_histories(events: usize) -> Vec<Vec<Step>> {
    let mut all = Vec::new();
    let mut frontier = vec![(Vec::new(), Model::default())];
    for _ in 0..events {
        let mut next = Vec::new();
        for (history, model) in &frontier {
            for step in STEPS {
                if model.legal(step) {
                    let mut moved = *model;
                    moved.apply(step);
                    let mut longer = history.clone();
                    longer.push(step);
                    all.push(longer.clone());
                    next.push((longer, moved));
                }
            }
        }
        frontier = next;
    }
    all
}

fn describe_steps(history: &[Step], looks: u32, upto: usize, mode: KeyMode) -> String {
    let mut text = format!("the dictate row is {mode:?}");
    for (step, event) in history[..=upto].iter().enumerate() {
        let what = match event {
            Step::Press => "AltGr down alone, the hook reports Maybe",
            Step::Chord => "another key joins AltGr, the hook reports Chord",
            Step::Confirm => "the wait of the latest press runs out",
            Step::ConfirmOld => "the wait of an earlier press runs out",
            Step::Release => "AltGr up",
            Step::WriteDown => "write key down",
            Step::WriteUp => "write key up",
        };
        let look = if looks >> step & 1 == 1 { ", then the engine looks" } else { "" };
        text.push_str(&format!("\n{}. {what}{look}", step + 1));
    }
    text
}

/// Plays one sequence of the AltGr family; the first broken property and its step.
fn play_alone(
    mode: KeyMode,
    history: &[Step],
    looks: u32,
    eager: bool,
) -> Option<(&'static str, usize, String)> {
    let rows = binds(mode);
    let altgr = &rows[0];
    let (input, state) = HoldInput::new();
    let counted = state.clone();
    let mut presses = Presses::new(state);
    let mut model = Model::default();
    let mut waits: Vec<u64> = Vec::new();
    let mut unfired: Vec<u64> = Vec::new();
    let (mut taken, mut lost) = (0u64, 0u64);
    let mut hold: Option<(Key, u64)> = None;
    for (step, &event) in history.iter().enumerate() {
        let starts = model.starts(event);
        let write_before = model.write;
        match event {
            Step::Press => {
                let wait = input.press_alone(0);
                waits.push(wait);
                unfired.push(wait);
            }
            Step::Chord => input.apply(0, altgr, HoldChange::Chord),
            Step::Confirm => {
                let wait = *waits.last().expect("a press came first");
                unfired.retain(|known| *known != wait);
                input.confirm_alone(0, altgr, wait);
            }
            Step::ConfirmOld => {
                let latest = *waits.last().expect("a press came first");
                let wait = *unfired.iter().find(|known| **known != latest).expect("an old wait is left");
                unfired.retain(|known| *known != wait);
                input.confirm_alone(0, altgr, wait);
            }
            Step::Release => input.apply(0, altgr, HoldChange::Up),
            Step::WriteDown => input.key(1, &rows[1], true),
            Step::WriteUp => input.key(1, &rows[1], false),
        }
        model.apply(event);
        if looks >> step & 1 == 0 {
            continue;
        }
        let expected_open = |key: Key| match key {
            Key::Hold => mode == KeyMode::Hold && model.holding(),
            Key::Write => model.write,
        };
        if let Some((key, number)) = hold {
            let over = presses.is_over(number);
            if eager && over == expected_open(key) {
                let what = if over { "the hold ended while its key was still down" } else { "the hold stayed open after its key came up" };
                return Some(("HoldFollowsItsKey", step, what.to_owned()));
            }
            if over {
                hold = None;
            }
        }
        let free = hold.is_none();
        let mut waiting = Vec::new();
        while let Some(press) = presses.take() {
            taken += 1;
            waiting.push(press);
        }
        let press = waiting.into_iter().next();
        let refused = presses.take_lost();
        lost += refused;
        let total = counted.borrow().presses;
        if taken + lost != total {
            return Some(("EachPressCountedOnce", step, format!("{total} presses, but {taken} taken and {lost} refused")));
        }
        if let Some(press) = &press
            && press.mode == KeyMode::Hold
        {
            let key = key_of(press.action);
            if eager && !expected_open(key) {
                let property = if key == Key::Hold { "ChordNeverHolds" } else { "HoldFollowsItsKey" };
                return Some((property, step, "a hold started while its key was not held alone".to_owned()));
            }
            if !free {
                return Some(("HoldFollowsItsKey", step, "a second hold started during a hold".to_owned()));
            }
            hold = Some((key, press.number));
        }
        if eager {
            let started = press.as_ref().map(|press| (key_of(press.action), press.mode));
            let wanted = starts && (mode != KeyMode::Hold || !write_before);
            if wanted && started != Some((Key::Hold, mode)) {
                return Some((
                    "FreeKeyStartsCapture",
                    step,
                    format!("AltGr stayed alone until its wait ran out with no hold open, and the engine got {started:?} and {refused} refused"),
                ));
            }
            if event == Step::WriteDown && !model.holding() && started != Some((Key::Write, KeyMode::Hold)) {
                return Some((
                    "FreeKeyStartsCapture",
                    step,
                    format!("the write key went down with no hold open, and the engine got {started:?} and {refused} refused"),
                ));
            }
        }
    }
    None
}

#[test]
fn hold_keys_keep_their_promises_for_every_bounded_sequence() {
    let mut found: Vec<(&'static str, String)> = Vec::new();
    let mut scenarios = 0u64;
    for mode in MODES {
        let record = |found: &mut Vec<(&'static str, String)>, history: &[Event], order: &[usize], looks: u32, eager: bool| {
            if let Some((property, step, what)) = play(mode, history, order, looks, eager)
                && !found.iter().any(|(known, _)| *known == property)
            {
                found.push((property, format!("{}\n=> {what}", describe(history, order, looks, step, mode))));
            }
        };
        for history in histories(EAGER_EVENTS) {
            for order in arrivals(&history) {
                scenarios += 1;
                record(&mut found, &history, &order, u32::MAX, true);
            }
        }
        for history in histories(LATE_EVENTS) {
            for order in arrivals(&history) {
                for looks in 0..(1u32 << history.len()) {
                    scenarios += 1;
                    record(&mut found, &history, &order, looks, false);
                }
            }
        }
        for history in step_histories(EAGER_EVENTS) {
            scenarios += 1;
            if let Some((property, step, what)) = play_alone(mode, &history, u32::MAX, true)
                && !found.iter().any(|(known, _)| *known == property)
            {
                found.push((property, format!("{}\n=> {what}", describe_steps(&history, u32::MAX, step, mode))));
            }
        }
        for history in step_histories(LATE_EVENTS) {
            for looks in 0..(1u32 << history.len()) {
                scenarios += 1;
                if let Some((property, step, what)) = play_alone(mode, &history, looks, false)
                    && !found.iter().any(|(known, _)| *known == property)
                {
                    found.push((property, format!("{}\n=> {what}", describe_steps(&history, looks, step, mode))));
                }
            }
        }
    }
    println!("SCENARIOS {scenarios}");
    for property in ["EachPressCountedOnce", "HoldFollowsItsKey", "FreeKeyStartsCapture", "ChordNeverHolds"] {
        match found.iter().find(|(known, _)| *known == property) {
            Some((_, trace)) => println!("COUNTEREXAMPLE {property}\n{trace}"),
            None => println!("HOLDS {property}"),
        }
    }
    assert!(found.is_empty(), "a key property is broken; see the counterexamples above");
}
