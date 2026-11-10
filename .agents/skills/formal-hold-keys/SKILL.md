---
name: formal-hold-keys
description: Check that the hold and write keys start one hold per press, that a hold lasts exactly while its own key is down, and that each press is taken or refused once, for every sequence of key events up to a bound, run against the real HoldInput and Presses. Use on any change to crates/thalvi-desktop/src/hold.rs above the Windows hook (KeyState, Cutoff, Presses, HoldInput); the harness runs the real code, but its model of key events must be checked against the change first.
---

# Formal hold keys

## Property

A press of the hold key or the write key starts a hold when no other hold is
down, the hold lasts exactly while that key is physically down, and every press
is either taken or refused once. Breaking this leaves the microphone open after
the key is up, cuts a dictation short, or answers a key the user is holding with
"pressed and released too quickly".

Three properties, each checked on the real code:

- `EachPressCountedOnce`: whenever the engine looks, the presses `Presses::take`
  returned plus those `Presses::take_lost` refused equal `KeyState::presses`,
  however late it looks.
- `HoldFollowsItsKey`: `take` returns a press only for a key that is physically
  down, never during another hold, and `Presses::is_over` turns true exactly at
  that key's own release.
- `FreeKeyStartsHold`: a key that goes down in order while no hold is open and
  the other key is up starts a hold of that key.

**Current result: PASS.** All three properties hold on the real code. Until #326,
`FreeKeyStartsHold` was broken:

1. The write key goes down: `HoldInput::change` starts a write hold, and the engine takes it.
2. The hold key goes down: `change` ignores it ("one hold at a time").
3. The write key comes up: the write hold ends.
4. The hold key comes up while no hold is open: `change` took it for a release that overtook its own press and stored an early-release credit for the hold key.
5. Within `OVERTAKEN_PRESS` (1 s) the hold key goes down again: `change` counted the press and set its `Cutoff` at once, without setting `down`. `take` returned `None` and `take_lost` 1, so the engine refused it as `Refusal::TooQuick`.

In `HoldInput` either key could be the first one. Harm: a user who touched the
other key during a hold and pressed it again within a second was told "The key
was pressed and released too quickly" while holding it, and nothing was
dictated.

The fix records each key's own phase (`KeyPhase`: up, down, or up with a release
that overtook a press) on every event, before the "one hold at a time" check, so
an ignored press is still a press whose release is its own, and a release can
only be an overtake while that key is up. The early-release credit came with
3ad1240, and the second key with 1913ec5, which made an ignored press's release
look like an overtaken one. On Windows, `HoldHooks` ends both holds when the
shared modifier comes up and reports them in the order `app.rs` builds the
hooks, Dictate then Write, so a write key touched during a dictation hold reached
the old step 4 there, but not the reverse; on Hyprland each bind reports on its
own, so either order could (not tried on a live session).

## Covers

Sync commit: `a5060554df34d8b99071e7f379376d4e69466f41`. [source.json](source.json)
records the whole-file hash of `crates/thalvi-desktop/src/hold.rs`. Matching
hashes establish synchronization only, not that the event model still fits. The
recorded hash also covers the #326 fix, whose own commit cannot name itself, so
`sync_commit` stays at the last commit the check ran against before it.

| Code | In the harness |
| --- | --- |
| `hold.rs`: `HoldInput::set`, `HoldInput::set_write`, `HoldInput::change`, `KeyState`, `KeyPhase`, `OVERTAKEN_PRESS` | Called for each arriving key event, in arrival order. Every scenario runs within microseconds, so every early release is inside the 1 s window: the worst case for overtake credit. |
| `hold.rs`: `Presses::take`, `Presses::take_lost`, `Presses::is_over`, `Cutoff` | A "look" calls `is_over` on the open hold, then `take` and `take_lost`, as `Engine::scan` and `Engine::on_key` do after each change of the watch. |
| Hyprland binds (one process per press and per release) | Every `hold-start`, `hold-end`, `write-start` and `write-end` request reaches `HoldInput` through `Running::hold` (`agent.rs`), refused or not, as the Windows hook delivers every key event, so one arrival sequence covers both. An arrival order may swap a quick tap's press and release (`arrivals` in `harness.rs`). |
| `Engine::scan`'s biased `self.key.changed()` branch | Eager mode: a look after every arrival, up to 8 events. |
| A watch whose changes are seen late (coalesced, or a cursor not polled) | Late mode: a look after any subset of arrivals, up to 7 events; only `EachPressCountedOnce` is judged there. |

Not covered: `HoldHook` and `HoldHooks` (the Windows hook, which the
`any_key_order_leaves_one_hold_at_a_time…` property test in `hold.rs` covers),
`Presses::skip` and the hand-offs between the engine's and the runner's cursors
in `crates/thalvi-app/src/runner.rs` (`formal-capture-cancel`'s `repro.py` replays
the shutdown, reload and hand-off cases), `Cutoff::precedes` against queued
frames, and `Refusal`'s wording.

## Run

From the repository root, with Python 3.12+ and the workspace's Rust toolchain:

```sh
.agents/skills/formal-hold-keys/scripts/check.py
```

It copies the tracked `crates/` and the workspace manifests into a new
`/var/tmp/thalvi-formal-hold-...` directory, adds `harness.rs` there as
`crates/thalvi-desktop/tests/formal_hold_keys.rs`, and runs
`cargo test --offline --locked -p thalvi-desktop --test formal_hold_keys`.
The pinned toolchain (`rust-toolchain.toml`) and `Cargo.lock` pin every version.
Build output goes to `${XDG_CACHE_HOME:-~/.cache}/thalvi-formal/hold-keys-target`,
never into the checkout. It needs the Cargo dependencies already fetched and the
Linux build prerequisites from AGENTS.md. Nothing is installed.

It runs 85,770 scenarios in about 25 s with a warm Cargo cache, most of that
compiling; each run has a 600 s limit. It prints `EVIDENCE=...`; `harness.txt` there
holds the full output. Exit 0 prints `PASS`; exit 1 prints each broken
property's shortest counterexample as numbered steps, then `FAIL`; exit 2 prints
`BLOCKED` for source drift, a build failure or a timeout. The current result is
exit 0 with `PASS`. Never turn a failure into a pass.

After changing the harness, check that each property can still fail:

```sh
.agents/skills/formal-hold-keys/scripts/check.py --self-test
```

It applies one mutation at a time to the snapshot's `hold.rs` and each must
break its property:

| Mutation | What it puts back | Must break |
| --- | --- | --- |
| `NoOvertakeCredit` | no credit for a release while the key is up: the code before 3ad1240, where a tap whose release arrived first left a hold open | `HoldFollowsItsKey` |
| `NoOtherKeyGuard` | no "one hold at a time" check: the other key's release ends the hold | `HoldFollowsItsKey` |
| `UncountedLoss` | `Presses::take` stops counting presses it passes over | `EachPressCountedOnce` |
| `ForgetIgnoredKey` | the other key's press and release leave no phase behind while a hold ignores them: the #326 bug | `FreeKeyStartsHold` |

A fix for the finding must turn `FreeKeyStartsHold` to `HOLDS` without breaking
the other two, and `ForgetIgnoredKey` is that fix's reversal, so the self-test
keeps showing the property can fail. A mutation whose text no longer matches
`hold.rs` stops the self-test with `BLOCKED`; update it from the code.

## Read a result

A pass holds only for the bounds and assumptions under Scope. For a
counterexample, replay the numbered steps against `HoldInput::change` and
`Presses` in `hold.rs`, using the snapshot under `EVIDENCE` if the checkout has
moved. A step the app cannot receive (for example, a release overtaking a press
that is not part of a quick tap) is a modeling error: fix `arrivals` or `play` in
`harness.rs` and say what was wrong. If every step can happen, it is a bug:
report the steps and the harm. Never narrow the sequences or skip a property
to make a run pass.

## When the covered code changes

1. Read the `hold.rs` diff and commit messages since the sync commit. Check
   whether the change alters what the harness assumes: the two keys and their
   `set`/`set_write` entry points, a look being `is_over` then `take` then
   `take_lost`, the 1 s overtake window, and which arrivals Hyprland or the
   hook can produce. Read the engine's and runner's callers in
   `crates/thalvi-desktop/src/engine.rs` (`scan`, `on_key`, `capture`) and
   `crates/thalvi-app/src/runner.rs` if the way presses are looked at changed.
2. Update `harness.rs` from the code **before** rerunning: a new key, a new
   window, or a new way of looking at presses. Keep the bounds; report any
   reduction. Update each `MUTATIONS` entry whose text moved.
3. Change a property only for a promise that changed on purpose, with evidence
   (a doc comment, test, DECISIONS.md entry or commit message), and record the
   old wording, the new wording and that evidence here.
4. Update the SHA-256 in `source.json` (`sha256sum crates/thalvi-desktop/src/hold.rs`),
   and move `sync_commit` only after confirming the harness describes that
   commit; for an uncommitted change, keep the previous commit and say the
   hash describes the working tree.
5. Run the check and `--self-test`. A fix for the finding must turn
   `FreeKeyStartsHold` to `HOLDS` without breaking the other two; update the
   finding text here and add its mutation.

To prove this recipe, in a throwaway worktree make a release while the key is
up leave the phase `Up` instead of `Overtaken(Instant::now())` in
`HoldInput::change` (the `NoOvertakeCredit` mutation). The unchanged recipe must
first report `SOURCE DRIFT`. The diff changes no assumption of the harness, so
refresh the hash and rerun: the check must report
`COUNTEREXAMPLE HoldFollowsItsKey` with a tap whose release overtook its press.
Remove the worktree and confirm the checkout is unchanged.

## Scope

Two keys, both up at the start; physical histories of up to 8 key changes,
looked at after every arrival, and up to 7 looked at after any subset of
arrivals; every quick tap may arrive release first; every event within 1 s of
the others. One `Presses` cursor that is never skipped.

Not verified: histories longer than the bounds, events more than a second apart
(an expired early release), a key already down when the app starts, other
cursors and `skip`, the Windows hook's own event handling, the engine's
reaction to a press beyond `take` (cues, capture, delivery), and real input
devices. The harness drives the real `HoldInput` and `Presses` on one thread;
real producers and the engine run on different threads, and the watch channel
makes each `change` atomic.
