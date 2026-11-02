---
name: verify-thalvi
description: Launch and drive Thalvi's web client (typed and spoken conversation, conversation list, history search, voice picker) against local fake model, recognizer and speech endpoints, and capture evidence. Use to confirm a server or web change works the way a user would see it, beyond the unit tests.
---

# Verify Thalvi

Thalvi's main surface is the web client the Rust server serves at `/`. A user
types or talks in it, hears spoken replies, and manages saved conversations.
The other surfaces are not covered here. The desktop client (`desktop/`, wake
words, dictation into Hyprland windows) has its own fakes-based gate in
AGENTS.md. The `measure` binary is a CLI covered by `tests/measure_bin.rs`.

Every run uses local stand-ins for the three model endpoints:

- a Chat Completions SSE model;
- a Meta realtime ASR WebSocket;
- an OpenAI `/audio/speech` PCM engine.

A run never reaches Meta, Baseten or a GPU speech server, so it costs nothing
and needs no keys. The server starts with an empty environment from a working
directory under `/tmp`, so no `.env` file or shell variable can leak in.

All commands run from the repository root. `V` is the helper and `R` is the run
directory that `up` prints:

```sh
V=.agents/skills/verify-thalvi/scripts/thalvi-verify
```

## Launch

```sh
$V up
```

`up` creates `/tmp/thalvi-verify-<id>/` with an empty data directory, which is
the `R` for every later command. It then:

1. Runs `npm ci` if `web/node_modules` is missing.
2. Builds the web client into `R/web` without touching `web/dist`, then builds
   the debug server and copies it to `R/thalvi`.
3. Picks three free ports and starts the fakes and the server, each in its own
   process group.
4. Writes `R/run.env` with the URL, ports, process groups, browser session
   name and build identity, then runs doctor.

`up` prints `RUN_DIR=` first and `URL=` last. A cold build takes about 30 s
when `target/` is already warm.

The run is usable when doctor prints only `ok` lines and the page reaches
Idle:

```sh
$V ab $R open "$(. $R/run.env; echo $URL)"
$V state $R idle
```

Seed data:

- One learning, `Prefers metric units in recipes.`, in `R/data/learnings.json`.
- No conversations. Create them through the UI.
- Voices `fake_heart` (the default) and `fake_bella`.

Two runs can be up side by side. Each has its own ports, data directory and
agent-browser session (`thalvi-verify-<id>`). Drive only a run that `up`
created in this task. Never drive the developer's own server on :8080.

Teardown is `$V down $R` (see Cleanup).

## Doctor

```sh
$V doctor $R
```

Doctor is read-only and exits non-zero on any `FAIL`. It checks that:

- both process groups are alive;
- the app port and both fake ports are held by this run's process groups;
- `/` serves the Thalvi client;
- `/voices` lists the fake voices;
- the server's environment points the model, recognizer and speech at this
  run's fakes;
- the checkout's source hash (HEAD plus uncommitted diff under `crates/`,
  `web/` and `Cargo.lock`) still matches the build.

If the source hash fails, the code changed after launch. Run `down`, then `up`
again before claiming anything. Run doctor before the first drive and after any
failed step.

## Drive

Drive the browser through `$V ab $R <agent-browser command>`. It always uses
this run's session, plus Chromium flags that grant the microphone and feed it
`R/mic.wav`. That file is a looping 8.5 s clip: 1 s of silence, 1.6 s of
speech-like sound, then silence.

Handles that stay stable:

| What | Selector |
| --- | --- |
| Current state | `.orb-app` `data-state`: `connecting`, `idle`, `starting`, `listening`, `thinking`, `working`, `speaking`, `reconnecting`, `ended` |
| Talk / stop button | `button.o-orb`; its label is `Start talking` when idle, `Stop` while thinking, working or speaking, and `End voice` while listening |
| Voice toggle | `button.o-voice` (`aria-pressed`, text `Voice on` or `Voice off`) |
| Composer | `#o-message` (label `Message`), `button[aria-label="Send"]` |
| Turns | `.o-turns > .o-user`, `.o-turns > .o-assistant` |
| Failure notice | `.o-notice` (`role=alert`); hidden when there is none |
| Conversations sheet | `button[aria-label="Conversations"]`, dialog `.o-library` |
| Voice picker | `button.o-voice-chip`, `select[aria-label="Voice to try"]`, `.o-vp-preview`, `.o-vp-apply`, `.o-vp-note` |
| Speed | `.o-speed-chip`, `button[aria-label="Playback speed 1.5×"]` |

Waits:

- Wait for a finished reply with `$V reply $R "<expected text>"`. It returns
  once the last Thalvi turn contains the text and the state is `idle` or
  `listening`.
- Do not wait for `idle` right after Send. The page is still idle before the
  server answers, so the wait passes at once, and the next send cancels the
  turn.
- Use `$V state $R <state>` only for a state the page is not already in, such
  as `thinking` after Send. Wait on other content with
  `$V ab $R wait --fn '<js>'`. Never sleep for a fixed time.
- After `open`, `reload`, `New conversation` or reopening a conversation, run
  `$V state $R idle`. The page passes through `connecting` with the composer
  disabled, and agent-browser's `fill` and `click` still report success there
  while the message is silently lost.
- The Conversations sheet loads its list after it opens. Wait for a row's
  button (`$V ab $R wait 'button[aria-label="Delete <title>"]'`) before
  clicking it.
- `wait --text "Idle"` never matches, because CSS uppercases the label.

Read turns with
`$V ab $R eval 'document.querySelector(".o-turns").innerText'`, not with
`get text`, which returned stale partial text in practice.

The fake model's reply depends on the last user turn:

| Turn contains | Fake model does |
| --- | --- |
| `make it fail` | Answers HTTP 500 |
| `long story` | Streams about 15 s of text, slowly enough to stop or interrupt |
| `search for <words>` (tools offered) | Calls `search_history` with `<words>`, then reports the tool result |
| anything else | Replies `Fake reply to: <turn>. That is all.` |

The fake recognizer turns each burst of microphone speech into the next line
queued with `$V say $R "<words>"`. When the queue is empty, it ignores speech,
so queue a line before the user would speak. For example, voice on with one
queued line produces exactly one turn.

The feature recipes are in [features/README.md](features/README.md).

## Evidence

Evidence goes in `R/evidence/`, outside the repository. Never commit it.

- **UI state:** `$V ab $R screenshot $R/evidence/<feature>-<step>.png` and
  `$V ab $R snapshot -c > $R/evidence/<feature>-<step>.aria.txt`.
- **Flows:** wrap the action in `$V ab $R record start $R/evidence/<name>.webm`
  and `$V ab $R record stop`. A frame comes out with
  `ffmpeg -sseof -0.5 -i <webm> -frames:v 1 <png>`.
- **Side effects:** `R/fakes.jsonl` gets one JSON line for each of these:
  - model request (`llm_request`: user turn, tools offered, tool results, plan)
  - cancelled stream (`llm_cancelled`, `tts_cancelled`)
  - speech request (`tts_request`: voice, input)
  - recognizer event (`asr_open`, `asr_speech_start`, `asr_speech_complete`,
    `asr_ignored_speech`, `asr_close`)
- **Stored state:**
  - saved conversations: `R/data/sessions/*.json`
  - learnings: `R/data/learnings.json`
  - metric and text recordings: `R/data/recordings/` (`THALVI_RECORD=metrics,text`)
  - server log: `R/server.log`
  - the read-only API views: `curl <URL>sessions` and `curl <URL>learnings`

A claim needs the user action and the resulting state, plus the side effect
where one exists. For example, a stopped reply shows `tts_cancelled` and the
page back in `listening`.

The fakes stand in for real models, so a run proves the UI, the server's
session logic and the wire protocols. It does not prove any of these, so do not
report them as verified:

- real model quality or latency;
- Meta ASR's real endpointing or recognition;
- real TTS audio, or audio you can hear (headless Chromium plays to a null
  sink);
- physical microphone, echo or wake-word behavior;
- phone browsers and the `tailscale serve` path.

## Cleanup

```sh
$V down $R
```

`down` closes this run's agent-browser session and sends TERM to the two
process groups recorded in `R/run.env` (KILL after 20 s). It never matches
processes by name. `R/` stays with its evidence, data and logs. Confirm nothing
outlived the run:

```sh
. $R/run.env
ss -Hltn | grep -E ":($APP_PORT|$FAKE_HTTP_PORT|$FAKE_ASR_PORT)\b"
```

That should print nothing. After a failed `up`, run `down` on the printed
`RUN_DIR` too.

## Helpers

- `scripts/thalvi-verify`: `up`, `doctor R`, `say R "<words>"`,
  `ab R <agent-browser args>`, `state R <state>`, `reply R "<text>"`,
  `down R`. Run it with no
  arguments for usage.
- `scripts/fake_endpoints.py`: the fakes. `up` starts it with
  `uv run --no-project --with websockets==17.4`. `python3 -I
  scripts/fake_endpoints.py wav <path>` writes the microphone clip. It needs
  `uv`, `python3`, `node`/`npm`, `cargo`, `agent-browser`, `ss` and `curl` on
  `PATH`.
