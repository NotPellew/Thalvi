# Native desktop client

This foreground client owns the microphone, local wake detection, and the
original target window. The Rust server still owns transcription and
conversation. `/transcription` returns text without asking the chat model;
`/session` keeps the existing conversational protocol.

The first native target is Hyprland. It needs `hyprctl`, `wtype`, `wl-copy`,
and `xdg-settings`. Other desktop adapters are not yet implemented or qualified.
Python 3.12.14 is pinned; use uv 0.12.15. No code is copied from Hex-Hands-free.

## Run

Start Thalvi normally, then from this directory:

```sh
uv sync --locked --extra audio
cp config.example.json ../data/desktop.json
uv run thalvi-desktop --config ../data/desktop.json run
```

Configure `wake.model_dir` to an existing sherpa-onnx
`kws-zipformer-gigaspeech-3.3M-2024-01-01` directory containing `tokens.txt`,
`bpe.model`, and the int8 encoder, decoder and joiner for epoch 12, average 2,
chunk 16, left context 64. Paths are relative to the config file. Assets are
never downloaded automatically. The model is from the [sherpa-onnx pretrained
keyword models](https://k2-fsa.github.io/sherpa/onnx/kws/pretrained_models/index.html);
its model-specific licence has not been established, so this configuration is
for private validation, not an asset redistribution claim.

The capture wake phrase is “Hey Thalvi”; configure its phonetic spelling as
`Hey Talvi` because the literal spelling did not detect in the local trial.
`conversation_phrase` defaults to `Hey Mycroft` for the demo and has not been
live qualified. The phrases must be distinct English-letter text supported by
the model tokenizer. Both use the same model and stay local until a wake.

Audio dependencies are optional so tests and replay need neither PortAudio nor
weights. `sherpa-onnx` runs local CPU keyword inference, `sentencepiece`
tokenizes configured phrases for its BPE model, `sounddevice` supplies native
audio, NumPy converts audio formats, and `websockets` speaks the local server
protocol. Keyword data is held in memory on Linux; model files are not modified.

Use headphones for native conversation: this prototype has no acoustic echo
cancellation. The phone/browser client retains its own media handling. The
native microphone stays open until Ctrl+C; audio is not saved. No audio is
sent to the server while waiting for a wake word. `--once` closes after one
capture or conversation; `--dry-run` previews desktop actions while still
using the configured transcription/conversation endpoints.

## Captures and tasks

After the capture wake word, say the text, then an ending phrase. Pauses can
produce separate transcription pieces but do not deliver or end the capture.
All ending phrases can be changed in the config.

| Ending | Result |
| --- | --- |
| paste that | Insert the collected text without Enter. |
| submit | Ask for confirmation, then insert and press Enter. |
| do that | Run the matching configured action. |
| scratch that | Discard this piece, or the preceding piece if spoken alone. |
| never mind | Discard the capture and return to wake listening. |

Submit initially confirms every target because Enter in an arbitrary window
can send a message or execute a command. Confirmation accepts `yes`,
`yes send it` or `confirm`; `no`, `never mind` or `cancel` stops it. A failed
confirmation does not submit. The original window identity remains bound
through confirmation. If it closes or cannot be focused, text is copied to
the clipboard instead of being typed elsewhere. A typing failure may be
partial, so it is reported and never retried automatically.

The example scripts open the project page or focus T3 Code and Discord.
App classes, input shortcuts, task phrases and URL destinations are config,
not shell commands. When several app windows match, Hyprland's recent-focus
history chooses the first. An unconfigured task preserves its text on the
clipboard. `tasks.decide` returns `OpenUrl`, `FocusApp`, or `DiscordMessage`; a future
model decision can return the same typed action to `tasks.execute`.

On Hyprland/Wayland, “open google.com do that” opens `https://google.com` in a
new tab of the default browser. Firefox, Chromium and Google Chrome desktop
entries are supported, including Firefox and Chromium Snap entries. Other
default browsers report that the action is unavailable and preserve the text.
URLs without a scheme use HTTPS; path and query case are preserved.

Preview this task without a microphone or browser action:

```sh
uv run thalvi-desktop --config config.example.json replay examples/open-browser.jsonl
```

### Discord Desktop

Live Discord typing and submission are disabled. A scripted Discord request
reports this and keeps the message on the clipboard without connecting to
Discord, navigating, inserting text or pressing Enter. Preview still shows the
proposed action. Channel and DM URLs remain accepted in configuration.

The experimental CDP adapter is retained for inert tests. Its trusted
`Input.insertText` updates the real Slate model, unlike `execCommand`, but the
input gate does not protect existing child documents or establish safety for
out-of-process frames. An existing-frame regression is a strict expected
failure. The later unresolved-input tombstone regression passes; a new arm or
disarm cannot erase that uncertainty before a reload.

The real-browser fixture establishes transport and Slate behavior, not safe
live Discord control. Re-enabling the action requires closing the frame input
boundary and separate live qualification. Discord acknowledgements and an
optimistically cleared editor still would not establish delivery.

The other wake word opens native conversation. Speak naturally and interrupt
by speaking; `stop conversation`, `end conversation` or `never mind` returns
to wake listening. This client uses the server's existing interruption logic.

Set `metrics` to a local path to opt into desktop action/flow timing and
outcomes, including each local debug discovery, composer check, text insert and
key press. Desktop metrics contain neither captured text, destinations, tokens
nor debugger messages. Status and confirmation text appear in the foreground
terminal.

Wake-model loading and each detector call record their host elapsed time and
outcome when metrics are enabled. Detector timings buffer at most 125 records
and flush at a wake or shutdown, avoiding a disk write on every audio frame.
These timings measure local call cost, not perceived wake latency.

## Tests

`uv run pytest -q` runs the adapter against local fake HTTP, WebSocket and
`/proc` fixtures; it needs no Discord, microphone or model. The actual composer
guard runs in the web toolchain against inert DOM fixtures (`npm test` in
`web/`), not against the user's Discord.

The real-browser gate is opt-in and uses Playwright's pinned Chromium plus a
local CONNECT/TLS proxy that serves an inert Slate/React fixture as
`https://discord.com`. It never reaches the network or real Discord:

```sh
cd desktop
npm ci --prefix browser && npm run build --prefix browser
uv run playwright install --with-deps chromium
THALVI_BROWSER_TESTS=1 uv run pytest -q tests/test_discord_browser.py
```

Playwright is a dev-only dependency used to launch the pinned Chromium and
observe the fixture. The fixture's React, Slate and esbuild packages are also
dev-only and lock to the versions in `browser/package-lock.json`.

## Replay without audio or app actions

```sh
uv sync --locked
uv run thalvi-desktop --config config.example.json replay examples/capture.jsonl
```

Replay does not use a microphone, wake model or endpoint. It previews actions
unless `--apply` is explicitly given; that flag really types into the focused
window. Live Discord requests remain disabled even with `--apply`. Use
`--apply` only for an explicitly authorized live rehearsal. Without `--apply`, replay and
preview never connect to Discord. Replay reads confirmation events from the
fixture rather than live speech. Live voice recognition quality and native
audio hardware need separate manual qualification; replay and stub-server
tests do not establish those.
