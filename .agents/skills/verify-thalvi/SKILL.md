---
name: verify-thalvi
description: Launch and drive Thalvi's web client (typed and spoken conversation, conversation list, history search, voice picker) against local fake endpoints, or a self-hosted model and speech engine, and capture evidence. Use to confirm a server or web change works the way a user would see it, beyond the unit tests.
---

# Verify Thalvi

Thalvi's main surface is the web client the Rust server serves at `/`. A user
types or talks in it, hears spoken replies, and manages saved conversations.
The other surfaces are not covered here. The desktop app (`crates/thalvi-app`,
wake words, dictation into other windows) is covered by the Rust and desktop UI
gates in AGENTS.md. The `measure` binary is a CLI covered by `tests/measure_bin.rs`.

A run has one of two modes. Neither reaches a paid service or needs a key.

| Endpoint | `up` (fake mode) | `up --local` |
| --- | --- | --- |
| Chat model | Scripted fake | Your self-hosted Chat Completions server (Ollama by default) |
| Speech | Fake tone | Your self-hosted speech engine (the repo's Kokoro by default) |
| Recognizer | Scripted fake | Scripted fake. Thalvi only speaks Meta's realtime ASR protocol, and no self-hosted server speaks it. |

- Use fake mode for UI, session logic and error paths. It is deterministic, and
  its scripted replies drive failures, long replies and tool calls on demand.
- Use `--local` when the claim needs real model replies, real tool calls, real
  synthesized audio, or real latency.

The server starts with an empty environment from a working directory under
`/tmp`, so no `.env` file or shell variable can leak in. In `--local` mode,
every endpoint must resolve to loopback, a private LAN or a tailnet, and no API
key is sent.

All commands run from the repository root. `V` is the helper and `R` is the run
directory that `up` prints:

```sh
V=.agents/skills/verify-thalvi/scripts/thalvi-verify
```

## Launch

```sh
$V up            # fake mode
$V up --local    # self-hosted model and speech
```

`up` creates `/tmp/thalvi-verify-<id>/` with an empty data directory, which is
the `R` for every later command. It then:

1. With `--local`, checks the self-hosted endpoints first. It fails fast with
   the command that fixes a missing service.
2. Runs `npm ci` if `web/node_modules` is missing.
3. Builds the web client into `R/web` without touching `web/dist`, then builds
   the debug server and copies it to `R/thalvi`.
4. Picks three free ports and starts the fakes and the server, each in its own
   process group.
5. Writes `R/run.env` with the mode, endpoints, URL, ports, process groups,
   browser session name and build identity, then runs doctor.

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
- Voices: `fake_heart` (the default) and `fake_bella` in fake mode; the
  configured voices with `--local`.

Two runs can be up side by side. Each has its own ports, data directory and
agent-browser session (`thalvi-verify-<id>`). With `--local`, runs share the
developer's model and speech servers, which hold no per-run state. Drive only a
run that `up` created in this task. Never drive the developer's own server on
:8080.

Teardown is `$V down $R` (see Cleanup).

### Self-hosted endpoints for `--local`

Each setting comes from the environment, else `.env.verify` at the repository
root (git-ignored, one `KEY=value` per line), else the default:

| Setting | Default | Notes |
| --- | --- | --- |
| `VERIFY_LLM_BASE_URL` | `http://127.0.0.1:11434/v1` | Any Chat Completions server: Ollama, vLLM, llama.cpp |
| `VERIFY_LLM_MODEL` | `qwen3:4b-instruct-2507-q4_K_M` | Must stream tool calls; about 2.5 GB, fits CPU or GPU |
| `VERIFY_TTS_API` | `openai` | `openai` or `qwen3` (qwentts.cpp) |
| `VERIFY_TTS_BASE_URL` | `http://127.0.0.1:8880/v1` | `docker compose --profile kokoro up -d --wait` |
| `VERIFY_TTS_MODEL` | `kokoro` | Read for `openai` only |
| `VERIFY_TTS_VOICE` | `af_heart` | The default voice |
| `VERIFY_TTS_VOICES` | `af_heart,af_bella` | Offered in the voice picker; each must synthesize |

For example, a developer whose qwentts.cpp runs on another tailnet machine
puts these lines in `.env.verify`:

```sh
VERIFY_TTS_API=qwen3
VERIFY_TTS_BASE_URL=http://gpu-box.example.ts.net:8011/v1
VERIFY_TTS_VOICE=af_heart
VERIFY_TTS_VOICES=af_heart
```

`up --local` never starts, stops or downloads anything on the developer's
machine. When a service is missing it prints the command that fixes it, for
example `ollama pull <model>` or the Kokoro compose line above. Ollama also
runs from its Docker image with `-p 127.0.0.1:11434:11434 -v
~/.ollama:/root/.ollama`.

## Doctor

```sh
$V doctor $R
```

Doctor exits non-zero on any `FAIL`. It checks that:

- both process groups are alive;
- the app port and both fake ports are held by this run's process groups;
- `/` serves the Thalvi client;
- `/voices` lists the run's voices;
- the server's environment holds the run's model and speech endpoints, and the
  recognizer is this run's fake;
- with `--local`, the model answers a one-token reply and every voice
  synthesizes a word;
- the checkout's source hash (HEAD plus uncommitted diff under `crates/`,
  `web/` and `Cargo.lock`) still matches the build.

Doctor changes no state. In `--local` mode it makes those small model and
speech calls.

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
- When the reply text is unknown, as with `--local`, use
  `$V reply $R --after "<user text>"`. It waits until the last turn is
  Thalvi's, right after a user turn containing that text.
- `reply` allows 25 s, or 125 s with `--local`.
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
- Never set `AGENT_BROWSER_*` variables on a single command. agent-browser
  restarts its browser when they change, and the page is lost.

Read turns with
`$V ab $R eval 'document.querySelector(".o-turns").innerText'`, not with
`get text`, which returned stale partial text in practice.

In fake mode, the fake model's reply depends on the last user turn:

| Turn contains | Fake model does |
| --- | --- |
| `make it fail` | Answers HTTP 500 |
| `long story` | Streams about 15 s of text, slowly enough to stop or interrupt |
| `search for <words>` (tools offered) | Calls `search_history` with `<words>`, then reports the tool result |
| anything else | Replies `Fake reply to: <turn>. That is all.` |

With `--local`, these phrases are ordinary questions to a real model. Assert
the shape of the result rather than its text:

- a settled reply (`reply --after`);
- `Task steps` for a question that should make the model search;
- the recording events listed under Evidence.

The fake recognizer turns each burst of microphone speech into the next line
queued with `$V say $R "<words>"`. When the queue is empty, it ignores speech,
so queue a line before the user would speak. For example, voice on with one
queued line produces exactly one turn. This works the same in both modes.

The feature recipes are in [features/README.md](features/README.md). They are
written for fake mode.

## Evidence

Evidence goes in `R/evidence/`, outside the repository. Never commit it.

- **UI state:** `$V ab $R screenshot $R/evidence/<feature>-<step>.png` and
  `$V ab $R snapshot -c > $R/evidence/<feature>-<step>.aria.txt`.
- **Flows:** start the recording with the page URL, before any turn:
  `$V ab $R record start $R/evidence/<name>.webm "$(. $R/run.env; echo $URL)"`,
  then `$V state $R idle`, and finish with `$V ab $R record stop`.
  - `record start` moves the page into a new browser context. Started
    mid-conversation, it closes the old page, and the server cancels the reply
    in flight.
  - A frame comes out with `ffmpeg -sseof -0.5 -i <webm> -frames:v 1 <png>`.
- **Model and speech calls, both modes:**
  `R/data/recordings/<date>/<session>.jsonl` holds the server's own metrics:
  - `model_call`: round, outcome, `total_ms`, error;
  - `tts_sentence`: voice, `first_byte_ms`, `audio_ms`;
  - `reply`: outcome, `first_token_ms`, `first_audio_ms`, text;
  - `turn`, `stt_connect`.

  With `--local`, these are the real latencies of the self-hosted model and
  speech engine. Report them as that, never as Meta's latency.
- **Fake side effects, fake mode:** `R/fakes.jsonl` gets one JSON line for each
  of these:
  - model request (`llm_request`: user turn, tools offered, tool results, plan)
  - cancelled stream (`llm_cancelled`, `tts_cancelled`)
  - speech request (`tts_request`: voice, input)

  Recognizer events (`asr_open`, `asr_speech_start`, `asr_speech_complete`,
  `asr_ignored_speech`, `asr_close`) are logged in both modes.
- **Stored state:**
  - saved conversations: `R/data/sessions/*.json`
  - learnings: `R/data/learnings.json`
  - server log: `R/server.log`
  - the read-only API views: `curl <URL>sessions` and `curl <URL>learnings`

A claim needs the user action and the resulting state, plus the side effect
where one exists. For example, a stopped reply shows `tts_cancelled` and the
page back in `listening`.

Do not report any of these as verified in either mode:

- Muse Spark's answers or latency (`--local` uses another model);
- Meta ASR's real endpointing or recognition;
- audio you can hear (headless Chromium plays to a null sink);
- physical microphone, echo or wake-word behavior;
- phone browsers and the `tailscale serve` path.

Fake mode also proves no model quality, tool choice, synthesized audio or
latency.

## Cleanup

```sh
$V down $R
```

`down` closes this run's agent-browser session and sends TERM to the two
process groups recorded in `R/run.env` (KILL after 20 s). It never matches
processes by name, and it never touches the developer's model or speech
servers. `R/` stays with its evidence, data and logs. Confirm nothing outlived
the run:

```sh
. $R/run.env
ss -Hltn | grep -E ":($APP_PORT|$FAKE_HTTP_PORT|$FAKE_ASR_PORT)\b"
```

That should print nothing. After a failed `up`, run `down` on the printed
`RUN_DIR` too.

## Helpers

All helpers need `uv`, `python3`, `node`/`npm`, `cargo`, `agent-browser`, `ss`
and `curl` on `PATH`.

- `scripts/thalvi-verify`: `up [--local]`, `doctor R`, `say R "<words>"`,
  `ab R <agent-browser args>`, `state R <state>`,
  `reply R "<text>"` or `reply R --after "<user text>"`, `down R`. Run it with
  no arguments for usage.
- `scripts/fake_endpoints.py`: the fakes. `up` starts it with
  `uv run --no-project --with websockets==17.4`.
  `python3 -I scripts/fake_endpoints.py wav <path>` writes the microphone clip.
- `scripts/local_endpoints.py`: `resolve <repo>` picks and checks the
  `--local` settings; `check <env file>` probes them.
