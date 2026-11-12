//! Every bounded sequence of the two hold keys' events, in order and with overtaken taps, against
//! the real `HoldInput` and `Presses`; and every bounded sequence of events of an AltGr hold key
//! that waits to be told from a chord, beside a plain write key.

use thalvi_desktop::hold::{HoldChange, HoldInput, HoldKind, Presses};

/// Longest physical sequence looked at after every arrival, as the engine's `scan` does.
const EAGER_EVENTS: usize = 8;
/// Longest physical sequence looked at after any subset of arrivals.
const LATE_EVENTS: usize = 7;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Key {
    Hold,
    Write,
}

#[derive(Clone, Copy, Debug)]
struct Event {
    key: Key,
    down: bool,
}

fn key_of(kind: HoldKind) -> Key {
    match kind {
        HoldKind::Dictate => Key::Hold,
        HoldKind::Write => Key::Write,
    }
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

fn describe(history: &[Event], order: &[usize], looks: u32, upto: usize) -> String {
    order[..=upto]
        .iter()
        .enumerate()
        .map(|(step, &at)| {
            let event = history[at];
            let key = match event.key {
                Key::Hold => "hold key",
                Key::Write => "write key",
            };
            let moved = if event.down { "down" } else { "up" };
            let late = if !event.down && at != step { " (overtook its press)" } else { "" };
            let look = if looks >> step & 1 == 1 { ", then the engine looks" } else { "" };
            format!("{}. {key} {moved}{late}{look}", step + 1)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Plays one arrival order, looking where `looks` says; the first broken property and its step.
fn play(history: &[Event], order: &[usize], looks: u32, eager: bool) -> Option<(&'static str, usize, String)> {
    let (input, state) = HoldInput::new();
    let counted = state.clone();
    let mut presses = Presses::new(state);
    let mut happened = 0;
    let (mut taken, mut lost) = (0u64, 0u64);
    let mut hold: Option<(Key, u64)> = None;
    for (step, &at) in order.iter().enumerate() {
        let event = history[at];
        match event.key {
            Key::Hold => input.set(event.down),
            Key::Write => input.set_write(event.down),
        }
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
        let press = presses.take();
        let refused = presses.take_lost();
        taken += u64::from(press.is_some());
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
            let key = key_of(press.kind);
            if eager && !down(key) {
                return Some(("HoldFollowsItsKey", step, "a hold started while its key was up".to_owned()));
            }
            if !free {
                return Some(("HoldFollowsItsKey", step, "a second hold started during a hold".to_owned()));
            }
            hold = Some((key, press.number));
        }
        let other = match event.key {
            Key::Hold => Key::Write,
            Key::Write => Key::Hold,
        };
        if eager && event.down && at == step && free && !down(other) {
            let started = press.as_ref().map(|press| key_of(press.kind));
            if started != Some(event.key) {
                return Some((
                    "FreeKeyStartsHold",
                    step,
                    format!("the key went down with no hold open, and the engine got {started:?} and {refused} refused"),
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

    /// Whether the step makes AltGr's hold start: its wait ran out with the key alone.
    fn starts_hold(&self, step: Step) -> bool {
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
                self.confirmed |= self.starts_hold(step);
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

fn describe_steps(history: &[Step], looks: u32, upto: usize) -> String {
    history[..=upto]
        .iter()
        .enumerate()
        .map(|(step, event)| {
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
            format!("{}. {what}{look}", step + 1)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Plays one sequence of the AltGr family; the first broken property and its step.
fn play_alone(history: &[Step], looks: u32, eager: bool) -> Option<(&'static str, usize, String)> {
    let (input, state) = HoldInput::new();
    let counted = state.clone();
    let mut presses = Presses::new(state);
    let mut model = Model::default();
    let mut waits: Vec<u64> = Vec::new();
    let mut unfired: Vec<u64> = Vec::new();
    let (mut taken, mut lost) = (0u64, 0u64);
    let mut hold: Option<(HoldKind, u64)> = None;
    for (step, &event) in history.iter().enumerate() {
        let starts = model.starts_hold(event);
        let write_before = model.write;
        match event {
            Step::Press => {
                let wait = input.press_alone(HoldKind::Dictate);
                waits.push(wait);
                unfired.push(wait);
            }
            Step::Chord => input.apply(HoldKind::Dictate, HoldChange::Chord),
            Step::Confirm => {
                let wait = *waits.last().expect("a press came first");
                unfired.retain(|known| *known != wait);
                input.confirm_alone(HoldKind::Dictate, wait);
            }
            Step::ConfirmOld => {
                let latest = *waits.last().expect("a press came first");
                let wait = *unfired.iter().find(|known| **known != latest).expect("an old wait is left");
                unfired.retain(|known| *known != wait);
                input.confirm_alone(HoldKind::Dictate, wait);
            }
            Step::Release => input.apply(HoldKind::Dictate, HoldChange::Up),
            Step::WriteDown => input.set_write(true),
            Step::WriteUp => input.set_write(false),
        }
        model.apply(event);
        if looks >> step & 1 == 0 {
            continue;
        }
        let expected_open = |kind: HoldKind| match kind {
            HoldKind::Dictate => model.holding(),
            HoldKind::Write => model.write,
        };
        if let Some((kind, number)) = hold {
            let over = presses.is_over(number);
            if eager && over == expected_open(kind) {
                let what = if over { "the hold ended while its key was still down" } else { "the hold stayed open after its key came up" };
                return Some(("HoldFollowsItsKey", step, what.to_owned()));
            }
            if over {
                hold = None;
            }
        }
        let free = hold.is_none();
        let press = presses.take();
        let refused = presses.take_lost();
        taken += u64::from(press.is_some());
        lost += refused;
        let total = counted.borrow().presses;
        if taken + lost != total {
            return Some(("EachPressCountedOnce", step, format!("{total} presses, but {taken} taken and {lost} refused")));
        }
        if let Some(press) = &press {
            if eager && !expected_open(press.kind) {
                let property = if press.kind == HoldKind::Dictate { "ChordNeverHolds" } else { "HoldFollowsItsKey" };
                return Some((property, step, "a hold started while its key was not held alone".to_owned()));
            }
            if !free {
                return Some(("HoldFollowsItsKey", step, "a second hold started during a hold".to_owned()));
            }
            hold = Some((press.kind, press.number));
        }
        if eager && free {
            let started = press.as_ref().map(|press| press.kind);
            if starts && !write_before && started != Some(HoldKind::Dictate) {
                return Some((
                    "FreeKeyStartsHold",
                    step,
                    format!("AltGr stayed alone until its wait ran out with no hold open, and the engine got {started:?} and {refused} refused"),
                ));
            }
            if event == Step::WriteDown && !model.holding() && started != Some(HoldKind::Write) {
                return Some((
                    "FreeKeyStartsHold",
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
    let record = |found: &mut Vec<(&'static str, String)>, history: &[Event], order: &[usize], looks: u32, eager: bool| {
        if let Some((property, step, what)) = play(history, order, looks, eager)
            && !found.iter().any(|(known, _)| *known == property)
        {
            found.push((property, format!("{}\n=> {what}", describe(history, order, looks, step))));
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
        if let Some((property, step, what)) = play_alone(&history, u32::MAX, true)
            && !found.iter().any(|(known, _)| *known == property)
        {
            found.push((property, format!("{}\n=> {what}", describe_steps(&history, u32::MAX, step))));
        }
    }
    for history in step_histories(LATE_EVENTS) {
        for looks in 0..(1u32 << history.len()) {
            scenarios += 1;
            if let Some((property, step, what)) = play_alone(&history, looks, false)
                && !found.iter().any(|(known, _)| *known == property)
            {
                found.push((property, format!("{}\n=> {what}", describe_steps(&history, looks, step))));
            }
        }
    }
    println!("SCENARIOS {scenarios}");
    for property in ["EachPressCountedOnce", "HoldFollowsItsKey", "FreeKeyStartsHold", "ChordNeverHolds"] {
        match found.iter().find(|(known, _)| *known == property) {
            Some((_, trace)) => println!("COUNTEREXAMPLE {property}\n{trace}"),
            None => println!("HOLDS {property}"),
        }
    }
    assert!(found.is_empty(), "a hold key property is broken; see the counterexamples above");
}
