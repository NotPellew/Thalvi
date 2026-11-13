---
name: formal-conversation-takeover
description: Check that when a saved conversation is reopened on another device, or deleted, no older connection's save lands afterwards, no turn is lost in the handoff, and a deleted conversation never comes back. Use on any change to crates/thalvi-server/src/store.rs (attach, release, save, delete, the writer's Remove) or to how crates/thalvi-server/src/session.rs saves and releases its conversation; update the source-mapped TLA+ model before rerunning when that code changes.
---

# Formal conversation takeover

## Property

When a device reopens a saved conversation, every older connection's last turns
are stored before the new one reads the history, none of them can save after
that, and a delete revokes every connection still able to save.
Breaking this silently drops turns, lets an old device overwrite the new one's
conversation, or brings back a conversation the user deleted.

Four invariants, each one sentence:

- `NoStaleSave`: once a takeover's `attach` has returned, no connection that
  attached to that conversation earlier gets a save through.
- `NoLostUpdate`: a save never replaces stored turns its connection had not read.
- `NoResurrection`: after a delete, the conversation is never in the store again.
- `HandoffKeepsTurns`: a connection that closed with turns unsaved lost them only
  to a delete, or to a takeover that stopped waiting for it after `HANDOFF_WAIT`.

## Covers

Sync commit: `8218dba22ec6c24255c73aa3e073071fb4d76bd4`. [source.json](source.json)
records whole-file hashes, so a changed file cannot silently reuse this model.
Matching hashes establish synchronization only, not correctness of the model.

| Code | Model action and boundary |
| --- | --- |
| `crates/thalvi-server/src/store.rs`: `SessionStore::attach`, first `self.lock()` block | `Claim`: a missing id gets a fresh conversation (`elsewhere`); otherwise the old `Live` entry is removed and the new one inserted with `predecessors` = old predecessors + old owner. One lock. |
| Same function: `previous.detach.send(Detach::Moved)` and `timeout(HANDOFF_WAIT, previous.released)` | `WaitReleased` (the old connection called `release`, or was already gone) or `WaitTimeout`. A takeover that started earlier has an earlier deadline, so a timeout also marks those earlier waits as passed (`expired`). |
| Same function: the second lock, marking `predecessors` dead and clearing them if still the owner | `Revoke`. One lock. |
| Same function: the third lock, reading `sessions.get(&id)` into the history | `Read`, then the attach returns. Empty history if the conversation was deleted meanwhile. One lock, separate from `Revoke`. |
| `SessionStore::save` | `Sync` and `FinalSave` through `DoSave`: one lock; a dead attachment changes nothing, otherwise the entry is created or replaced (`entry().or_insert_with`). |
| `SessionStore::release` | `Release`: removes the `Live` entry only if this connection still owns it, then fires `released`. One lock. |
| `write_loop`, `DiskOp::Remove` (from `SessionStore::delete`) | `Delete`: one lock across the file removal; marks the owner and every predecessor dead. |
| `crates/thalvi-server/src/session.rs`: `run_session` loop top, `Persistence::sync`, `Saved::sync` | `Turn` and `Sync`: a save happens only when the history grew. |
| Same file: recognizer retry state in `31a2eeb` | Not a persistence step: this changes input readiness and browser status/error messages only; the `Turn`, `Sync`, `Close`, `FinalSave` and `Release` ordering remains unchanged. |
| Same file: `run_session` loop exit on `persistence.detached()`, the client leaving, or a browser send timing out or failing, then `close_session` → `Persistence::finish` | `Close`, `FinalSave`, `Release`: the last sync, then release, before `ended` is sent. |

Not covered in these files: `retire`, `rename`, `set_model_title`, `snapshot`,
`search_history`, the private conversation path, and the order of disk writes in
`write_loop` (one writer thread serializes them).

## Run

From the repository root, with Python 3.12+:

```sh
.agents/skills/formal-conversation-takeover/scripts/check.py
```

The command installs TLC's stable `v1.7.4` release artifact (TLC2 Version 2.19
of 08 August 2024) into `${XDG_CACHE_HOME:-~/.cache}/thalvi-formal/` and verifies
SHA-256 `936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88`, the
same pin as `formal-capture-cancel`. The `v1.8.0` pre-release is re-uploaded
upstream, so its checksum does not hold
([upstream](https://github.com/tlaplus/tlaplus)). It uses an existing `java`
(11+); on Linux x86-64 without one it installs Temurin JRE `21.0.9+10` in the
same cache, verifying SHA-256
`aeab55d064a1a27a3744b0880b9b414077b4ed2b1790817eea3df60aec946431`. Nothing is
installed system-wide, and the network is needed on the first run only. The
cache is shared with `formal-capture-cancel`.

A warm run takes about 3 s here: TLC explores 371,796 distinct states (1,022,433
generated) in about 2 s. Each TLC run has a 120 s limit and a 512 MB heap. State
counts are receipts for this model, not acceptance criteria for a changed one.

It prints `EVIDENCE=/var/tmp/thalvi-formal-takeover-...` and keeps TLC's output,
the copied model and the trace there, outside the checkout. Exit 0 prints
`PASS`; exit 1 prints `COUNTEREXAMPLE <invariant>` and `FAIL`; exit 2 prints
`BLOCKED` for source drift, a tool error or a timeout. The current faithful
result is exit 0. Never turn a failure into a pass.

After changing the verification, check that each invariant can still fail:

```sh
.agents/skills/formal-conversation-takeover/scripts/check.py --self-test
```

It runs four mutations, each with only its own invariant enabled, and each must
report a counterexample for that invariant:

| Mutation constant | What it puts back | Must break |
| --- | --- | --- |
| `RevokeOnlyPrevious` | `attach` marks only the previous owner dead (the code before 3293431) | `NoStaleSave` |
| `DeleteSparesPredecessors` | delete marks only the owner dead (the code before 3293431) | `NoResurrection` |
| `ReadBeforeRevoke` | `attach` reads the history before marking predecessors dead | `NoLostUpdate` |
| `ReleaseBeforeLastSave` | `Persistence::finish` releases before its last sync | `HandoffKeepsTurns` |

`SELF-TEST PASS` verifies detection only; it is not a pass of the faithful
model. With every invariant enabled, `ReleaseBeforeLastSave` also breaks
`NoResurrection`: a released connection is no longer revoked by a delete.

## Read a result

A pass holds only for the bounds and assumptions under Scope. For a failure,
open the printed `tlc.txt` and replay each state's action against the function
the table maps it to. A step the code cannot take is a modeling error: fix the
model from the code and say what was wrong. If every step is possible, it is a
bug: report it as steps in the code's words, with the harm.

The `RevokeOnlyPrevious` counterexample, which is the takeover chain that
3293431 fixed, shows how a trace reads:

1. `c1`'s `attach` claims and reads the conversation; a turn is added but not yet synced (`Claim`, `Read`, `Turn`).
2. `c2` opens the same conversation: its `attach` takes the `Live` entry and waits for `c1` (`Claim`).
3. `c3` opens it too, carrying `c1` and `c2` as predecessors, and waits for `c2` (`Claim`).
4. `c3`'s `HANDOFF_WAIT` passes first, since timers can fire in either order (`WaitTimeout`).
5. `c3` marks only `c2` dead, reads the history and returns (`Revoke`, `Read`).
6. `c1`'s `handle_session` loop syncs, and `SessionStore::save` stores its turn after `c3` returned (`Sync`): an old device has written over the conversation the newest one holds.

## When the covered code changes

1. Read the diff and the commit messages since the sync commit for both files.
   Follow moved or renamed functions and any new code that touches `State::live`,
   an `Attachment`'s `dead` flag, `released`, or `sessions` for an open
   conversation. Read the functions even if the hash change looks cosmetic.
2. Update the actions from the code **before** running TLC. Keep each
   `self.lock()` block one action and each await its own step. Do not merge
   `Revoke` and `Read`: they are separate locks.
3. Change an invariant only if the promise changed on purpose, with evidence (a
   doc comment, test, DECISIONS.md entry or commit message), and record the old
   wording, the new wording and that evidence here. Keep the bounds; report any
   reduction. Never tune the model to hide a counterexample.
4. Update each changed file's SHA-256 in `source.json` (`sha256sum <path>`), and
   move `sync_commit` only after confirming that the model describes that
   commit. For an uncommitted change, keep the previous commit and say that its
   hashes describe the working tree; move it once the commit exists.
5. Run the check and `--self-test`. If the code change adds a guard or a step
   order worth keeping, add a mutation for it that breaks one invariant. Replay
   any counterexample before calling it a bug.

To prove this recipe, in a throwaway worktree delete the
`for older in &live.predecessors` loop from the `DiskOp::Remove` arm of
`write_loop`. The unchanged recipe must first report `SOURCE DRIFT`. Reading the
diff, the scratch copy's `Delete` now marks only the owner, so set
`DeleteSparesPredecessors = TRUE` in the scratch `.cfg` and refresh the hash.
The check must then fail with `COUNTEREXAMPLE NoResurrection`. Remove the
worktree, and confirm the original checkout is unchanged and still passes.

## Scope

One saved conversation; three connections that each open it once, in any
order; at most one new turn each; any number of deletes. Each connection's
saves can happen at any point while it is open, which over-approximates the
`handle_session` loop. `HANDOFF_WAIT` can pass at any moment of a wait, and
every takeover waits the same length, so deadlines pass in the order the waits
began.

Assumed: `Mutex<State>` excludes other lock holders; oneshot and watch channels
deliver; the store's `Arc<AtomicBool>` stores are `SeqCst` and made under the
lock; an `attach` future is not dropped mid-handoff, which no production caller
does (`Retention::open` awaits it, and the HTTP handlers attach without one).
Not verified: more than three connections or more turns, liveness (a takeover
eventually returns), what the client is told (`send_ended`), recordings,
titles, retirement, renames, disk contents and crash recovery, and the
websocket protocol around the store.
