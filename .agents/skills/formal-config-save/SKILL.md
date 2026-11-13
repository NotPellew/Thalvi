---
name: formal-config-save
description: Check that concurrent saves of the desktop config (the Settings window, agent settings requests on their own threads, and the login or menu switches) leave desktop.json whole, the running app equal to the file, and each setting at its last reported save. Use on any change to crates/thalvi-app/src/settings.rs (Store::save, write_atomically), commands.rs (save), app.rs (AppState::remember, apply, serve_requests), agent.rs (settings_set) or the request servers' threads in crates/thalvi-desktop/src/host; update the source-mapped TLA+ model before rerunning when that code changes.
---

# Formal config save

## Property

Every save leaves `desktop.json` a whole config. Once all saves have returned,
the running app and the file hold the same settings, and each setting holds the
value of the last save that reported success for it.
Breaking this can leave a config the next start cannot load. It can also
silently run other settings than the file, undo a switch or a Settings change
the user was told was saved, or keep a change the user was told had failed.

Three invariants, each one sentence:

- `FileAlwaysWhole`: `desktop.json` always names a fully written config
  (`Store::save`: "replaces the file in one step").
- `RunningMatchesFile`: once every save has returned, `AppState::config`, the
  engine's last `Control::Apply` and the file agree.
- `EachFieldKeepsItsLastSave`: once every save has returned, each form setting
  is that of the last form save that set it and returned Ok (a save that sets
  one setting never undoes another's), and the switch is that of the last
  switch save that returned Ok. A Save never writes the
  switch, and a switch never writes the form (DECISIONS.md, "Packaged desktop
  with an OS-held login entry": the preference is "written by the switch alone,
  never by Save").

**Current result: PASS.** `SyncedConfig` in `app.rs` holds one lock from
reading the config through `Store::save` and `AppState::apply` (for a switch,
through setting the running config), so the writers cannot interleave and
share the one temporary name safely. An agent change reads the config, merges
its patch over it and validates it inside that lock (`settings_set` hands
`plan_over` to `SyncedConfig::save_form`), so it rebases on a Settings save
that finished first. The traces below are the runner's output from before the
fix, in the code's words.

`FileAlwaysWhole`, a Settings save and an agent change at once:

1. Both read `AppState::config` (`commands::save` → `state.config()`).
2. The Settings save's `write_private` removes `desktop.json.tmp`, creates it and writes it.
3. The agent's `write_private` removes that name (unlinking the Settings file) and creates its own, still empty.
4. The Settings save's `write_atomically` renames `desktop.json.tmp`, which is now the agent's empty file, onto `desktop.json`, and returns Ok.

Until the agent's `write_all` lands in that same file, `desktop.json` is empty.
If the app is killed then, the next start cannot load its config.

`RunningMatchesFile` and `EachFieldKeepsItsLastSave`, with a login switch
flipped by an agent request at the same time:

1. The Settings save, the agent change and `AppState::remember` each read the config.
2. All three remove `desktop.json.tmp`; the switch creates it.
3. The Settings save's `create_new` fails because the name exists. Its error path removes the name, the switch's file, and Settings shows that the save failed.
4. The agent creates its file and writes its settings; the switch writes into its own file, which is no longer linked.
5. The switch renames, moving the agent's file onto `desktop.json`, and stores its config in memory: Ok.
6. The agent's rename finds no `desktop.json.tmp` and returns an error, so the agent is told the change was refused.

Now the file holds the agent's refused change with the switch off, and the app
runs the old settings with the switch on. The next start silently loads
settings the agent was told were refused, and it forgets the switch the user
turned on.

An agent change that planned outside the lock lost a Settings save
(`AgentMergeOutsideLock` below, and the Rust test
`an_agent_change_keeps_a_settings_save_that_finished_while_it_was_planned`):

1. The agent's `settings_set` calls `plan`, which reads the config (look `gunmetal`) and merges `{"microphone": "USB Mic"}` into a whole form.
2. A Settings save sets the look to `holo` and returns Ok.
3. The agent's `save` takes the lock and writes its whole form, look `gunmetal` again, and returns Ok.

Both were told they saved; the file and the running app lost the new look.

A plain stale read breaks `EachFieldKeepsItsLastSave` too, without the shared
temporary name. `remember` reads the config, a Settings save completes, and then
`remember` writes the old form back with the new switch. The `SnapshotOutsideLock`
control below shows that trace.

Each overlap needs two saves within the few file-system calls of
`write_atomically`, a window of microseconds to milliseconds. An agent that
sends two settings requests at once reaches it without the user, because
`channel.rs` and `pipe.rs` answer each request on its own thread.

## Covers

Sync commit: `8218dba22ec6c24255c73aa3e073071fb4d76bd4`. [source.json](source.json)
records whole-file hashes, so a changed file cannot silently reuse this model.
Matching hashes establish synchronization only, not correctness of the model.

| Code | Model action and boundary |
| --- | --- |
| `crates/thalvi-app/src/commands.rs`: `save` (from `save_settings`, the Settings window) | Writer `S` on the main thread, one at a time with other main-thread writers. Via `AppState::save_form` into `SyncedConfig::save_form`: `Lock` before `Snap`, `Unlock` after `Send`. `Snap`: the config read for `Store::check`'s `current`, under the lock; the Settings save sets its field (the look). This abstracts any one Settings form field, including `update_channel`; that setting has no separate writer. `Store::write` runs through `AppState::take_effect`'s `write`, after any changed hold keys are watched; a desktop that refuses them ends the save before `Create`, like any refused save. |
| `crates/thalvi-app/src/agent.rs`: `settings_set`, `plan_over`, via `Live::save` in `app.rs` | Writer `A` on a request thread; the same `commands::save` path. `Snap` is `plan_over` on the config `SyncedConfig::save_form` read under the lock: the patch merged over the whole form, so it sets its own field (the microphone) and carries the others over. `settings check` (`plan`) writes nothing and is not modeled. |
| `crates/thalvi-app/src/app.rs`: `SyncedConfig::remember` (from `AppState::remember`, reached by `remember_login`, `remember_menu_entry` and `Live::set_switch`) | Writer `L` on a request thread. `Lock` before `Snap`: reads the whole config and sets the switch under the lock; `Store::save_switches` writes it; the running config is set before `Unlock`. |
| `crates/thalvi-app/src/settings.rs`: `Store::check` (form saves), `Store::save` (switches), `prepare` | The content each writer validates: a form save keeps `current`'s switches; a switch keeps the form it read. |
| Same file: `write_private` | `RmTmp` (`remove_file` of the shared name), `Create` (`create_new`; fails if the name exists), `Write` (`write_all` into the opened file). Under the save lock these steps cannot interleave with another writer's. |
| Same file: `write_atomically` | `Rename` (moves whatever `desktop.json.tmp` names now; fails if it names nothing). On any error it removes the shared name and returns Err. |
| `crates/thalvi-app/src/app.rs`: `AppState::apply` | `Apply`: `*current = config.clone()` under `AppState::config`'s lock (for `remember`, its own `*self.config.lock() = config`). `Send`: `Control::Apply` to the runner, which takes them in order. |
| `crates/thalvi-app/src/settings.rs`: `Store::start` (from `AppState::start` in `app.rs`'s `setup`) | Not modeled: a first start writes the defaults through `save_switches` before `setup` registers the state, serves requests or opens Settings, so no other writer exists yet. |
| `crates/thalvi-app/src/update.rs`: `Marker::write` and update-channel reads | Not a config writer: the updater's atomic write targets the separate `update.json` marker. The channel is read from `AppState::config`; changing it is covered by the Settings form writer above. |
| `crates/thalvi-desktop/src/host/channel.rs`: `accept`; `windows/pipe.rs`: `accept`, `answer` | Why `A` and `L` run concurrently: one `thalvi-request` thread per connection, up to `MAX_CONNECTIONS`. |

## Run

From the repository root, with Python 3.12+:

```sh
.agents/skills/formal-config-save/scripts/check.py
```

The command installs TLC's stable `v1.7.4` release artifact (TLC2 Version 2.19
of 08 August 2024) into `${XDG_CACHE_HOME:-~/.cache}/thalvi-formal/` and verifies
SHA-256 `936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88`, the
same pin as `formal-capture-cancel` ([upstream](https://github.com/tlaplus/tlaplus)).
It uses an existing `java` (11+); on Linux x86-64 without one it installs Temurin
JRE `21.0.9+10` in the same cache, verifying SHA-256
`aeab55d064a1a27a3744b0880b9b414077b4ed2b1790817eea3df60aec946431`. Nothing is
installed system-wide, and the network is needed on the first run only.

It checks each invariant in its own TLC run, so every broken one gets its own
shortest counterexample. That takes about 2.5 s warm, and each run has a 120 s
limit and a 512 MB heap. It prints `EVIDENCE=/var/tmp/thalvi-formal-config-...`
and keeps TLC's output there, outside the checkout. Each counterexample prints
as numbered steps naming the writer (Settings save, agent settings change, login
switch) and the action from the table. Exit 0 prints `PASS`; exit 1 prints
`COUNTEREXAMPLE <invariant>` for each broken one, then `FAIL`; exit 2 prints
`BLOCKED` for source drift, a tool error or a timeout. The current result is exit 0
(the finding above). Never turn a failure into a pass.

After changing the verification, check that the invariants can pass and fail:

```sh
.agents/skills/formal-config-save/scripts/check.py --self-test
```

It first runs `Serialized = TRUE`, one lock held from before `Snap` until
after `Send`, which must pass every invariant (131 distinct states). This shows
the finding comes from the missing serialization, not from the model. Then
four mutations of that design each must break their own invariant:

| Mutation constant | What it breaks in the serialized design | Must break |
| --- | --- | --- |
| `SnapshotOutsideLock` | the config is read before taking the lock | `EachFieldKeepsItsLastSave` |
| `ApplyOutsideLock` | the lock is released before `AppState::apply` | `RunningMatchesFile` |
| `AgentMergeOutsideLock` | the agent reads and merges its patch before taking the lock, as `plan` did before the repair | `EachFieldKeepsItsLastSave` |
| `RenameBeforeWrite` | the temporary file is renamed before it is written | `FileAlwaysWhole` |

`SELF-TEST PASS` verifies detection only; it is not a pass of the code as it is.

## Read a result

A pass holds only for the bounds and assumptions under Scope. For a failure,
replay each numbered step against the function the table maps its action to.
A step the code cannot take is a modeling error: fix the model from the code
and say what was wrong. If every step is possible, it is a bug: report the
steps in the code's words, with the harm. The traces above are this check's
current output.

## When the covered code changes

1. Read the diff and the commit messages since the sync commit for every covered
   file. Follow new writers of the config file or of `AppState::config`, and any
   lock or queue added around them. A shared atomic-write helper does not make a
   writer of another file a config writer; check the target path.
2. Update the model from the code **before** running TLC. A fix that serializes
   the writers changes the faithful configuration, for example by setting
   `Serialized = TRUE` if one lock spans the read, the file write and the apply.
   Model it as the code does it, not as the `Serialized` flag happens to.
   Keep each file-system call and each lock its own step.
3. Change an invariant only if the promise changed on purpose, with evidence (a
   doc comment, test, DECISIONS.md entry or commit message), and record the old
   wording, the new wording and that evidence here.
4. Update each changed file's SHA-256 in `source.json` (`sha256sum <path>`), and
   move `sync_commit` only after confirming that the model describes that commit.
   For an uncommitted change, keep the previous commit and say that its hashes
   describe the working tree.
5. Run the check and `--self-test`. When a fix lands, the faithful run must pass;
   update the finding above and keep a mutation that reverts the fix.

To prove this recipe, in a throwaway worktree make `write_private` stop
removing an existing temporary file: delete its `let _ = std::fs::remove_file(path);`.
The unchanged recipe must first report `SOURCE DRIFT`. Reading the diff, `RmTmp`
no longer exists, so remove `"rmtmp", ` from both step lists in the scratch
copy's `Flow` and refresh the hash. The check must still report
`COUNTEREXAMPLE RunningMatchesFile`, because a Settings save and an agent change
still collide on `create_new`. Remove the worktree and confirm that the
checkout is unchanged.

## Scope

Three writers that each save once: a Settings save on the main thread, and an
agent settings change and a switch flip on request threads. Every interleaving
of their file-system calls and locks. Content is abstract: a form of two fields, the look
that the Settings save sets and the microphone that the agent sets, each save
carrying the other field over from the config it read, and one switch. The
Settings window's own view of the fields it did not change is taken to be
current (its stale state is out of scope).

Assumed: each file-system call is atomic, and `write_all` of a config this size
lands in one step, so a half-written file is not modeled; validation in `load`
never fails; the runner applies `Control::Apply` messages in send order;
`rename` replaces the target atomically.

Not verified: two writers on the main thread at once (they cannot overlap); a
tray switch on the main thread racing an agent save (the same steps as `L`,
with `MainThread = {"S", "L"}`); more than three writers; partial writes, disk
full and crashes beyond the empty-file state; the Settings form's own stale
state (`settings.ts`, the "changed elsewhere" banner); the OS login entry that
`startup::set` changes before `remember`; Windows file-sharing errors on
`rename`.
