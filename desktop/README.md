# Native desktop client

This foreground client owns the microphone, local wake detection, and the
original target window. The Rust server still owns transcription and
conversation. `/transcription` returns text without asking the chat model;
`/session` keeps the existing conversational protocol.

The first native target is Hyprland. It needs `hyprctl`, `wtype`, `wl-copy`,
and `xdg-open`. Other desktop adapters are not yet implemented or qualified.
Python 3.12.14 is pinned; use uv 0.12.15. No code is copied from Hex-Hands-free.

## Run

Start Thalvi normally, then from this directory:

```sh
uv sync --locked --extra audio
cp config.example.json ../data/desktop.json
uv run thalvi-desktop --config ../data/desktop.json run
```

Configure existing ONNX files for the two wake models and their
`melspectrogram.onnx` and `embedding_model.onnx` feature models. Paths are
relative to the config file. Nothing downloads weights automatically.
The example names refer to openWakeWord models; the words they detect come
from those weights, not an editable text label. Check each model's licence
before choosing it; the pretrained Jarvis model is non-commercial. See the
[upstream project](https://github.com/dscripka/openWakeWord).

Audio dependencies are optional so tests and the replay command need neither
PortAudio nor model weights. `openwakeword` supplies local ONNX wake inference,
`sounddevice` supplies native audio on the platform prototypes, NumPy converts
48 kHz microphone frames to the wake and recognizer formats, and `websockets`
speaks the local server protocol. The uv override excludes the unused TFLite
runtime from openWakeWord's metadata; inference explicitly selects ONNX.

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

Say “message the demo channel saying <message> do that” to use the opt-in
Discord Desktop path below. The destination accepts Discord channel and DM URLs
such as `https://discord.com/channels/<guild-or-@me>/<channel>`; there is no
per-channel allowlist. No test sends a real message.

### Discord Desktop

This is a local-debugger adapter, not a bot, webhook or private endpoint. Start
Discord Desktop with its DevTools port open and accessibility enabled. Fully
quit the existing instance before launching with these flags:

```sh
discord --force-renderer-accessibility --remote-debugging-port=9223
```

Set `discord.debug_port` in the config to the same port (9223 in the example).
The client only talks to `127.0.0.1`, and checks that the listening socket, its
executable, the Discord window, and the user ID all belong to this Discord
process. Any other owner refuses the action. The debug port gives local
processes full control of Discord, so open it only while testing and restart
Discord normally afterwards.

With the configured channel open and its message box empty, the task verifies
the exact route and a bounded composer baseline: one focused message box, no
draft, attachment, reply, edit, menu, dialog or unfamiliar control. It opens
the exact channel if needed, inserts the text, shows the destination and exact
body, and asks for the normal standalone voice confirmation. Immediately after
yes it repeats the route, draft, region, and focus checks, then presses Enter
once. A failed check presses nothing and keeps the body. Cancelling leaves the
inserted draft in Discord for review; the client never clears a draft it cannot
prove it owns. Preflight inspects the composer; inserting the draft changes
Discord before the confirmation.

Discord's acknowledgement and the editor clearing are not delivery receipts.
Every completed attempt reports **submission attempted; delivery unconfirmed**.
There is no "sent" result until a defined Discord receipt is observed. The
initial read-only guard was exercised against Linux Discord Desktop 1.0.168.
Live insertion and submission need separate qualification; an unfamiliar form
or state refuses instead of guessing. The tests use inert local HTTP,
WebSocket and DOM fixtures and never connect to real Discord.

The other wake word opens native conversation. Speak naturally and interrupt
by speaking; `stop conversation`, `end conversation` or `never mind` returns
to wake listening. This client uses the server's existing interruption logic.

Set `metrics` to a local path to opt into desktop action/flow timing and
outcomes, including each local debug discovery, composer check, text insert and
key press. Desktop metrics contain neither captured text, destinations, tokens
nor debugger messages. Status and confirmation text appear in the foreground
terminal.

## Tests

`uv run pytest -q` runs the adapter against local fake HTTP, WebSocket and
`/proc` fixtures; it needs no Discord, microphone or model. The actual composer
guard runs in the web toolchain against inert DOM fixtures (`npm test` in
`web/`), not against the user's Discord.

## Replay without audio or app actions

```sh
uv sync --locked
uv run thalvi-desktop --config config.example.json replay examples/capture.jsonl
```

Replay does not use a microphone, wake model or endpoint. It previews actions
unless `--apply` is explicitly given; that flag really types into the focused
window and, for a crafted Discord replay fixture whose confirmation is true,
really uses the local Discord debugger and can press Enter. Use `--apply` only
for an explicitly authorized live rehearsal. Without `--apply`, replay and
preview never connect to Discord. Replay reads confirmation events from the
fixture rather than live speech. Live voice recognition quality and native
audio hardware need separate manual qualification; replay and stub-server
tests do not establish those.
