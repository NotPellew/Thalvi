---
name: formal-capture-cancel
description: Check that desktop cancellation, pause, and quit prevent a capture from entering delivery. Use on any change to crates/thalvi-app/src/runner.rs or crates/thalvi-desktop/src/engine.rs, recognizer.rs, or delivery.rs; update the source-mapped TLA+ model before rerunning when code changes.
---

# Formal capture cancellation

## Property

An ending control queued before the capture's final control drain empties must
prevent that capture's accepted result from entering delivery, even if a later
control resumes listening. The result's text is still kept once, as cancelled.
Breaking this promise can type text, press Enter, or execute a configured task
after the user cancels or pauses.

## Covers

Sync commit: `8c3a4871da96b6c8e55445667689d1d2ade9d35a`. The recorded file hashes
match the covered files at that commit. Since the model was last updated, holds
stopped honouring spoken endings (a hold's secondary transcript is agreed as
plain text, and cancellation is still polled first), the quiet wake floor was
added in `engine.rs`, the upsampling filter and word list changed in
`recognizer.rs`, and `runner.rs` hands the word list to the local recognizer and
notifies followers of every refused press, and `load_wake` publishes a wake model
download while `Runner::build` loads the wake engine. A released hold sends silence through
the same cancel-first send before it waits for the final. `Runner::apply` now
accounts for presses during a rebuild and refuses them afterwards; its former
body is `Runner::rebuild`. A dropped listener keeps its hold cursor until it
resumes. Closing the microphone ends that interval and hands pending presses to
Apply, which returns the accounted cursor to the live engine so a newer press
cannot be skipped. Speech servers became a list of entries: `Engine::reconcile`
runs every ticked second-pass entry (none runs when none is ticked, as before
when the switch was off), still polling cancellation first, and the recognizer
errors name their entry. `drive` times the recognizer's first words and each final
result against the speech heard (`Heard`) for the metrics. None of these touch the mapped functions' cancellation
behaviour, so the model is unchanged and only the hashes moved.
The GNOME host (#267) changed `delivery.rs` and `runner.rs` without changing that:
`Delivery::paste_in` (formerly `type_in`) returns `Delivered::InWindow` or `Delivered::OnClipboard` from the
same `Delivery::finish` admission, so a clipboard-only desktop is another admitted host
action and a cancelled capture still never reaches it. `Runner::capture` now calls
`Publisher::woke` before `drive`, which only sets the view's `copies` and `target`.
A wake capture whose whole text is a task now arms the task ending itself; it
reaches `Engine::conclude` through the same `Done::Accepted`, so that is unchanged too.
The offline-model fix (#272) added a `RecognizerError::Model` variant, raised while the
local model's files are fetched and so before any `RecognizerSession` exists.
Writing a dictation out ("write that out", the write key) adds one stage after
`reconcile`: `Engine::write` streams a chat reply and returns Accepted (a paste of
the written text), Failed (the user's words are kept for the clipboard) or
Cancelled. It polls cancellation first, through `bounded`, at every await, so it
is another case of the model's `secondary` stage: a post-accept step that sees a
pending cancel before it can return Accepted. Its Failed result is not an
accepted result, and an ended capture skips `Delivery::finish`, so the clipboard
copy of the user's words is gated by `ended` like every other host effect. The
model now keeps second-pass completion and writing completion as separate
cancellation-first awaits; neither admits a result into delivery. `delivery.rs`
adds a `Write` arm that pastes.
Delivery by pasting (#308) renames `type_in` to `paste_in` and drops its post-delivery `copy`, since the host
now leaves the text on the clipboard; admission through `Delivery::finish`, the `Write` arm and every await
boundary are unchanged, and `engine.rs` and `runner.rs` changed only in message text and a test.
The ending spotter adds synchronous bookkeeping in `conclude` before the cue and
recognizer release, and a `drive` branch that only plays a cue and shows a notice.
`Runner::build` joins the spotter load with the wake load; capture notices are set
synchronously. These changes preserve the mapped await boundaries, control drains
and delivery admission, so the cancellation model is unchanged.
The spotter loader now accepts its load future, like the wake loader, so tests
exercise download status and both load outcomes. The future is still awaited at
the same pre-capture boundary; the mapped cancellation functions are unchanged.
`SpotStats::record` now owns the same synchronous capture-summary metric calculation,
so exact durations and the zero-frame case can be tested. `close_spotting` still
records it before the ending cue; this metric extraction preserves the control drains and await boundaries.
An idle failure now picks `NoEnding` or `NoWords` from the kept text; it only chooses the error and leaves every await, control drain and delivery admission as mapped.
`Engine::reconcile` now stops waiting at the earliest ticked entry, in list order, whose
answer agrees, and leaves the later requests running as tasks in the engine's `recorded_only`
sets, which `scan`, `drive` and `reconcile` collect results from and only record. Its one await is still a
cancellation-first select that also takes the answers (its own requests are aborted when it
drops them), and it returns without another
await once the entry is known, so the model's `secondary` stage is unchanged: a pending
cancel wins that select before an answer can become Accepted. Recorded-only requests
never feed a capture's outcome and a later cancel does not touch them, so they are
outside the modeled delivery path.
`FakeControl::connect_again` in `recognizer.rs` only lets the test fake connect twice; the mapped
`RecognizerSession` and `ServerSession::close` are untouched.
The faithful check **passes**. It used to fail: cancellation was lost while the
recognizer closed after the accepted cue finished, because the runner kept only
the last drain's decision. `Requests::ended` now keeps every ending decision
through the whole `drive` call, and `Settling::settle` delivers nothing once it
is set. The full model's counterexample, the shortest trace and the first
replay failure are kept in "Read a result" as the history of the fix.

All paths below are repository-relative. [source.json](source.json) records
whole-file hashes so a changed implementation cannot silently reuse this model.
Matching hashes establish synchronization only, not correctness of the model.

| Code | Model actions and boundary |
| --- | --- |
| `crates/thalvi-app/src/runner.rs`: `Control`, `Requests::take`, `Requests::read_queued` | `Send`, `DrainOne`, `NextOff`, `Ends`; one channel send or one receive plus runner-local state update. Every receive and drain accumulates ending decisions with OR into `Requests::ended`. `Apply` has no cancellation effect. |
| Same file: `drive` | `PreEmpty`, `Receive`, `FinishEmpty`; pre-start drain, cancellation callback while capture is Pending, and completion drain. Biased polling starts the capture before a later control branch can run. `ended` is never reset inside the call, so a control consumed by `Receive` still counts at `FinishEmpty`. |
| Same file: `Runner::capture`, `Settling::settle` | `FinishEmpty` admits an Accepted result only if `ended` is clear. This is delivery admission at the last empty receive, not an atomic physical host effect. An ended capture skips `Delivery::finish` and is kept as cancelled. |
| `crates/thalvi-desktop/src/engine.rs`: `EngineHandle::cancel`, `Engine::capture` | `cancelPending` represents the watch version change; capture entry consumes the old version. `Accept` and `ActiveCancelled` abstract capture work before the ending. |
| Same file: `Engine::conclude`, `Engine::end_cue`, `Engine::end_with` | `CueReturns`, `CloseReturns`; the cue's cancellation-biased select and recognizer release are separate futures joined together. A completed cue retains its bool while close is still Pending. Neither completion is a transaction encompassing the other. |
| Same file: `Engine::reconcile`, `Engine::write`, `bounded` | `secondary`, `writing`, `SecondaryReturns`, `WritingReturns`; disabled stages are skipped, each enabled stage polls cancellation first before handing the result to the next stage. |
| `crates/thalvi-desktop/src/recognizer.rs`: `RecognizerSession`, `ServerSession::close` | `CloseReturns` may occur before or after the cue. Network close is awaited under a timeout, so it can remain Pending. |
| `crates/thalvi-desktop/src/delivery.rs`: `Delivery::finish`, `paste_in`, `call` | `delivered` abstracts admission to a valid configured host action. Actual host execution is outside the proof; once admitted it can run via `spawn_blocking`. Other changes to this file are not covered by the model. |

## Run

From the repository root, with Python 3.12+:

```sh
.agents/skills/formal-capture-cancel/scripts/check.py
```

The command installs TLC's `v1.8.0` release artifact into
`${XDG_CACHE_HOME:-~/.cache}/thalvi-formal/` and verifies SHA-256
`c57d4d7bbee3ed2c89c32cb9c60cac728f751e80e2570bc4b7368ae39291fc21`.
The artifact identifies itself as TLC `2026.11.02.164701`, revision `dc6a944`;
the checksum, rather than a moving release URL, pins its contents.
[Upstream tool and download instructions](https://github.com/tlaplus/tlaplus).

Use an existing `java` (11+). On Linux x86-64 without Java, the command installs
Temurin JRE `21.0.9+10` in the same user cache, verifying archive SHA-256
`aeab55d064a1a27a3744b0880b9b414077b4ed2b1790817eea3df60aec946431`.
Other platforms need a user-installed Java on PATH. No system package install
is performed. Downloads require network access only on the first run.

A faithful run here explores 15,896 distinct states (44,390 generated) with no
error; the passing focused control explores 13,896.
Each TLC invocation has a 120 s limit and 512 MB heap limit. State counts are
receipts for this model, not acceptance criteria for a changed model.

The command prints `EVIDENCE=/var/tmp/thalvi-formal-...`, retaining TLC output,
trace, copied model/config and generated states there. Exit 0 prints `PASS`;
exit 1 prints `COUNTEREXAMPLE` and `FAIL`; exit 2 prints `BLOCKED` for source
drift, a tool error, or timeout. The current faithful result is exit 0.
Never turn a failure into a success verdict.

Check the negative controls after changing the verification:

```sh
.agents/skills/formal-capture-cancel/scripts/check.py --self-test
```

This runs a focused `Scenario = "fence"` control that disallows consuming a
control after the ending cue returns (the shutdown ordering of the fixed bug).
It passes, then both mutations must fail the **same** `NoDeliveryAfterStop`
invariant: removing the `ended` guard from admission, and replacing OR
accumulation with the last message's decision. Both mutations also fail under
`Scenario = "all"`. `SELF-TEST PASS` verifies detection only; it
is not a pass of the full model. The production configuration remains
`Scenario = "all"`, both mutation constants FALSE, `MaxControls = 3`.

For a deterministic CPU replay against the real engine and delivery, plus
byte-for-byte extracted `Control`, `Requests`, and `drive` from the current
runner:

```sh
.agents/skills/formal-capture-cancel/scripts/repro.py
```

This copies tracked Rust sources to a new `/var/tmp/thalvi-cancel-repro-...`
directory, runs the workspace's pinned Rust toolchain and locked Cargo
dependencies offline, and builds only the desktop library test in a dedicated
user-cache target directory. It needs cached Cargo dependencies and native
build prerequisites from AGENTS.md. It uses immediate fake cues, an event-gated
fake recognizer close and a recording host; no audio, native host, model, or
service is used. The runner's `Control`, `Settling`, `while_delivering`,
`Requests` and `drive` are extracted byte for byte, so the replay settles the
real engine's outcome through the real admission code.
A finding prints `FINDING` and exits 1; a resolved replay exits 0; a compile,
setup, or 300 s timeout failure exits 2. Raw evidence stays outside Git.
Before the fix the replay failed with `CANCEL_REACHED_DELIVERY`, Accepted
output and `delivered=true`. Now it observes Accepted output with `ended=true`,
no host call, and one cancelled history entry and draft carrying the text.
The second replay gates the same shutdown without Cancel, presses and releases
the hold key while close is pending, and checks one refusal and successful delivery.
The settings replay extracts `Runner::apply` and `refuse_unheard` byte for byte,
gates a fake rebuild, and checks one refusal for a tap or held press just before
Apply or during the rebuild, through both
success and failure. It also replays identical and retune-only Apply handoffs with the real engine,
accepting a held press or reporting a completed tap. It checks the Apply wiring,
not model loading or native input.
The replay respects an existing `TMPDIR` and `CARGO_TARGET_DIR` for managed jobs.

## Read a result

A pass holds only for the bounds and assumptions below. For a failure, open
the printed `tlc.txt`; replay each named action against the mapped function.
An impossible step is a modeling error: correct it from the code and explain
what was wrong. If every step is possible, report the bug and its harm.

Shortest trace of the fixed bug (secondary disabled), kept to read the model:

1. `runner.rs::drive` finishes its empty pre-start drain (`PreEmpty`).
2. `EngineLink::send` queues Cancel (`Send`); capture polling can win before the runner receives it.
3. `engine.rs::conclude` reaches Accepted and starts `end_with` (`Accept`).
4. `engine.rs::end_cue` finishes successfully while recognizer close remains Pending (`CueReturns`).
5. `runner.rs::drive` receives Cancel, `Requests::take` returns true, and `EngineHandle::cancel` changes the watch version (`Receive`).
6. `recognizer.rs::ServerSession::close` completes or times out; `engine.rs::end_with` returns the cue's earlier true, and disabled `reconcile` returns Accepted (`CloseReturns`).
7. `runner.rs::drive` finds an empty completion queue because it consumed Cancel already; `Runner::capture` admits Accepted to `Delivery::finish` (`FinishEmpty`).

Cancel enqueued after step 4 reached the same bug; the CPU replay uses that
order. Step 7 now finds `ended` set by step 5 and admits nothing.

## When the covered code changes

1. Read the diff and commit messages since the sync commit for every covered
   file, following moved functions and new writes to cancellation or delivery
   state. Read the functions even if a hash change looks cosmetic.
2. Update actions from the code **before** running TLC. Preserve separate await
   boundaries and queue iterations. Do not model `end_with` as one indivisible
   operation. Do not assume that invoking the cancellation callback guarantees
   a cancelled outcome.
3. Change the property only if the product promise deliberately changed, with
   evidence from a decision, contract, test or commit. Preserve bounds; report
   any necessary reduction. Never tune the model to hide a counterexample.
4. Update each affected SHA-256 in `source.json` from the inspected working
   file (`sha256sum <path>`), and move `sync_commit` to the inspected commit
   only after confirming the model describes it. During an uncommitted change,
   retain the prior commit and state that its file hashes describe the working
   change; refresh the commit when it exists. Hash updates are not a substitute
   for step 2.
5. Run the full check and negative controls. Replay any new counterexample;
   preserve a real failure. The known failure may be resolved only with code
   evidence and a faithful full-model pass, plus the real-code replay.

When proving this recipe, make a temporary product mutation in a throwaway
worktree: change `Requests::read_queued` from `ends |= self.take(...)` to
`ends = self.take(...)`. The unchanged recipe must first report `SOURCE DRIFT`.
Read that diff, set `ForgetEarlierStop = TRUE` in the scratch model's config,
and refresh its source hash. For a diagnostic comparison, use the already
specified `Scenario = "fence"` to isolate this new break from the known bug;
TLC must report a counterexample where Cancel followed by Resume revives the
capture. Restore the full configuration and remove only the scratch worktree.
Confirm the original checkout's product files and status are unchanged by the
proof, and rerun the full check to restore its recorded result.

## Scope

One capture, initially listening, one logical FIFO producer (representing
channel send order), and zero to three controls from Cancel, pause, resume,
toggle, Apply and Quit. Every combination of enabled and disabled/skipped
second-pass transcription and writing is explored. Each queue send, each receive, and each awaited
completion is a separate transition; synchronous runner-local updates are
combined where no other actor reads them. The final empty receive is the
admission fence. Sends after that fence belong to the next flow and are not
covered; native delivery need not already have occurred at the fence.

The pre-accept capture pipeline is abstracted as an accepted result or an
observed cancellation. Recognition and silence correctness, watch wraparound,
channel disconnection, thread/process failure, repeated captures, more than
three controls, liveness/fair scheduling, config application, history writes,
real sockets/audio/cue devices, OS focus, clipboard, key injection and native
platform behavior are not verified. `Apply` stands for any settings payload
because payload content does not change `Requests::take`'s ending decision.
The Rust replay covers two concrete shutdown orderings, not these exclusions.
