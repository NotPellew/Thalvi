//! Every bounded sequence of the two hold keys' events, in order and with overtaken taps, against
//! the real `HoldInput` and `Presses`.

use thalvi_desktop::hold::{HoldInput, HoldKind, Presses};

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
    println!("SCENARIOS {scenarios}");
    for property in ["EachPressCountedOnce", "HoldFollowsItsKey", "FreeKeyStartsHold"] {
        match found.iter().find(|(known, _)| *known == property) {
            Some((_, trace)) => println!("COUNTEREXAMPLE {property}\n{trace}"),
            None => println!("HOLDS {property}"),
        }
    }
    assert!(found.is_empty(), "a hold key property is broken; see the counterexamples above");
}
