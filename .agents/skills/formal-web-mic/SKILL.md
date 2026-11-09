---
name: formal-web-mic
description: Check that the web client never leaves the microphone open when a conversation ends (moved, deleted, or a private one lost) while Voice is still starting, and that voice is on exactly while a working microphone is held. Use on any change to web/src/main.ts (startVoice, acquireAudio, releaseUnused), web/src/mic.ts (openMic), web/src/controller.ts (setVoice, micStopped, message) or the voice and ended handling in web/src/session.ts and view.ts; update the source-mapped TLA+ model before rerunning when that code changes.
---

# Formal web mic

## Property

A microphone the web client opened can always be stopped by the app, and once
no start is running, a working microphone is held exactly while voice is on and
the conversation has not ended.
Breaking this leaves the browser recording from the microphone with no control
in the page that can stop it, or keeps Voice on in a conversation that has ended.

Three invariants, each one sentence:

- `NoOrphanMic`: every live microphone stream is held by `micInput` or by the
  `openMic` call still running.
- `MicOnlyWithVoice`: when no `startVoice` is running, `micInput` holds a live
  stream exactly while `controller.state.voice` is on.
- `NoMicAfterEnd`: once the conversation has ended (`link === "ended"`) and no
  `startVoice` is running, `micInput` holds nothing.

**Current result: FINDING.** `NoOrphanMic` and `NoMicAfterEnd` are broken.

`NoOrphanMic`, the conversation ends while Voice is starting:

1. The user taps Voice: `startVoice` runs `acquireAudio`, and `openMic` awaits `getUserMedia` (the permission prompt may be open).
2. The prompt is granted, and `openMic` awaits `ctx.audioWorklet.addModule`. The conversation's `ended` can also arrive before the grant.
3. The server's `ended` arrives (moved to another device, deleted, or a private conversation's link lost). `Controller.message` turns voice and speak off and calls `releaseUnused`. `micInput` is still null, so no microphone is stopped. The `AudioContext` is closed.
4. `addModule` resolves on the closed context. `new AudioWorkletNode` throws `InvalidStateError`, outside `openMic`'s `try`, so the stream's tracks are never stopped. `startVoice`'s catch shows "The microphone could not be started."

The browser keeps recording from the microphone: its indicator stays on, and no
control in the page can stop it until the page is reloaded. Steps 2 to 4 were
measured with `scripts/closed-context-probe.html` in HeadlessChrome 153: the
track's `readyState` stays `live`.

`NoMicAfterEnd`, the conversation ends during `await ctx.resume()`:

1. The user taps Voice; `acquireAudio` creates a context and awaits `ctx.resume()`. `audio` is still null.
2. `ended` arrives; `releaseUnused` has no `audio` to close.
3. `resume` returns, and `openMic` succeeds on the open context. `startVoice` sets `micInput` and calls `setVoice(true)`, which turns voice on although the link has ended.

The microphone stays open with Voice on, and `view.ts` disables the Voice button
in an ended conversation, so the user cannot turn it off. This trace comes from
the model; it was not reproduced in a browser. `ctx.resume()` inside a tap
usually returns within milliseconds, so this window is far narrower than the
prompt in the first trace.

## Covers

Sync commit: `261bb2e0d87119034947afd68ef7844bebf0d381`. [source.json](source.json)
records whole-file hashes, so a changed file cannot silently reuse this model.
Matching hashes establish synchronization only, not correctness of the model.

JavaScript runs one task at a time, so each action is the code between two
awaits, or one event:

| Code | Model action |
| --- | --- |
| `web/src/main.ts`: `view.onVoice` → `startVoice`, `acquireAudio` up to `await ctx.resume()` | `Tap` (only while not busy, voice off and the link not ended, as `view.ts` enables the button) |
| `acquireAudio` after `resume`, `openMic` up to `await getUserMedia` | `Resumed` |
| `openMic` (`web/src/mic.ts`) after `getUserMedia` resolves, up to `await addModule` | `Granted`; a rejection runs `startVoice`'s catch: `Denied` |
| `openMic` after `addModule` settles, through node creation and return, then `startVoice` setting `micInput` and calling `Controller.setVoice(true)`, or its catch with `releaseUnused` | `Opened`, one step because no await follows `addModule` |
| `web/src/controller.ts`: `Controller.message` with `ended` (also `lost` for a private conversation via `session.ts` `endedState`), then `releaseUnused` | `Ended` |
| `Controller.message` with `input` from `text` while voice is on | `ServerTextInput` |
| `view.onVoice` while voice is on: `Controller.setVoice(false)` | `VoiceOff` |
| `openMic`'s track `onended` → `Controller.micStopped` | `TrackEnded` |
| `main.ts`: `releaseUnused` | `Release`: stops `micInput` unless voice is on; closes the context unless voice or speak is. |

Browser behaviour enters as two constants, measured by the probe:
`ClosedAddModuleRejects = FALSE` and `ClosedNodeThrows = TRUE` (HeadlessChrome 153).

## Run

From the repository root, with Python 3.12+:

```sh
.agents/skills/formal-web-mic/scripts/check.py
```

The command installs TLC's stable `v1.7.4` release artifact (TLC2 Version 2.19
of 08 August 2024) into `${XDG_CACHE_HOME:-~/.cache}/thalvi-formal/` and verifies
SHA-256 `936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88`, the
same pin as `formal-capture-cancel` ([upstream](https://github.com/tlaplus/tlaplus)).
It uses an existing `java` (11+); on Linux x86-64 without one it installs Temurin
JRE `21.0.9+10` in the same cache, verifying SHA-256
`aeab55d064a1a27a3744b0880b9b414077b4ed2b1790817eea3df60aec946431`. Nothing is
installed system-wide, and the network is needed on the first run only.

It checks each invariant in its own TLC run, in about 2.5 s warm; each run has
a 120 s limit and a 512 MB heap. It prints `EVIDENCE=/var/tmp/thalvi-formal-web-mic-...`
and keeps TLC's output there. Each counterexample prints as numbered actions
from the table. Exit 0 prints `PASS`; exit 1 prints `COUNTEREXAMPLE <invariant>`
for each broken one, then `FAIL`; exit 2 prints `BLOCKED` for source drift, a tool
error or a timeout. The current result is exit 1, with `NoOrphanMic` and
`NoMicAfterEnd` broken (the finding above). Never turn a failure into a pass.

After changing the verification:

```sh
.agents/skills/formal-web-mic/scripts/check.py --self-test
```

It first runs `GuardedOpen = TRUE`, which must pass every invariant (47
distinct states). In that design, `openMic` stops its tracks on any failure and
`startVoice` drops a microphone that arrives after the link ended. This shows
the finding comes from those two gaps, not from the model. Then each mutation
must break its invariant:

| Mutation constant | What it puts back | Must break |
| --- | --- | --- |
| `ReleaseKeepsMic` | `releaseUnused` no longer stops `micInput` (694a725 added this release when the server turns voice off) | `MicOnlyWithVoice` |
| `MicStoppedKeepsVoice` | a dead track leaves voice on | `MicOnlyWithVoice` |

`SELF-TEST PASS` verifies detection only; it is not a pass of the code as it is.

To measure the browser constants again, for a new browser or version, serve the
probe from localhost (a secure context) and open it in that browser:

```sh
python3 -m http.server 8765 --bind 127.0.0.1 --directory .agents/skills/formal-web-mic/scripts
```

Open `http://127.0.0.1:8765/closed-context-probe.html` with a microphone, or in
Chromium with `--use-fake-device-for-media-stream --use-fake-ui-for-media-stream`,
and click **go**. HeadlessChrome 153 prints `addModule resolved | openMic threw
InvalidStateError | tracks live`. If `addModule` rejects, set
`ClosedAddModuleRejects = TRUE`; if the node is built without an error, set
`ClosedNodeThrows = FALSE`. A browser that differs gets its own configuration,
not a change to the Chromium one.

## Read a result

A pass holds only for the bounds and assumptions under Scope. For a failure,
replay each numbered action against the code in the table. A step the code
cannot take is a modeling error: fix the model and say what was wrong. While
this skill was written, `addModule` settling and node creation were first
modeled as two steps, which let `Ended` run between them. The code has no await
there, so they are now one step (`Opened`). If every step is possible, it is a
bug: report the steps in the code's words, with the harm.

## When the covered code changes

1. Read the diff and the commit messages since the sync commit for every covered
   file. Follow every await between a tap and `micInput` being set, every caller
   of `releaseUnused`, and every message that turns voice off or ends the link.
2. Update the model from the code **before** running TLC. A new await is a new
   pending state that other events can interleave with; code with no await
   between stays one action. A fix that matches `GuardedOpen` should be modeled
   from the code itself, not by turning the flag on.
3. Change an invariant only if the promise changed on purpose, with evidence (a
   doc comment, test, DECISIONS.md entry or commit message), and record the old
   wording, the new wording and that evidence here.
4. Update each changed file's SHA-256 in `source.json` (`sha256sum <path>`), and
   move `sync_commit` only after confirming the model describes that commit. For
   an uncommitted change, keep the previous commit and say that its hashes
   describe the working tree.
5. Run the check and `--self-test`. When a fix lands, the faithful run must pass;
   update the finding above, and add a mutation that reverts the fix.

To prove this recipe, in a throwaway worktree delete `micInput.mic.stop();` from
`releaseUnused` in `web/src/main.ts`. The unchanged recipe must first report
`SOURCE DRIFT`. Reading the diff, `releaseUnused` now keeps the stream running,
which is `ReleaseKeepsMic = TRUE` in the scratch `.cfg`; refresh the hash. The
check must then also report `COUNTEREXAMPLE MicOnlyWithVoice`, which holds on
the code as it is. Remove the worktree and confirm that the checkout is unchanged.

## Scope

One page and up to two taps of Voice; any order of the server's `ended` or text
input, the user's Voice off, and the device ending the track; the permission
granted or denied. Speak follows voice (`setVoice` sets both), so
`startSpeak` is not modeled.

Assumed: the browser behaviour the constants state; JavaScript's one task at a
time, with each `await` a point where a WebSocket message can run; `ended`
stops the connection, so nothing reconnects.

Not verified: `startSpeak`, switching conversations (`switchTo`, which keeps
voice as it is), reconnection, the playback path, the wake lock, more than two
taps, other browsers than the one measured, and what the server does with the
`input` message after `ended`.
