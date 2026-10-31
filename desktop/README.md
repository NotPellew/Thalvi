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
App classes, input shortcuts, task phrases and destinations are config, not
shell commands. When several app windows match, Hyprland's recent-focus
history chooses the first. An unconfigured task preserves its text on the
clipboard. `tasks.decide` returns `OpenUrl`, `FocusApp`, or `DiscordMessage`;
a future model decision can return the same typed action.

### Discord demo delivery

Say “message the demo channel saying <message> do that”. Thalvi shows the
destination and message, asks for a separate spoken confirmation, then sends
once through the incoming webhook for **Thalvi Demo / #demo**. Saying no sends
nothing. Nothing is typed into Discord Desktop.

The webhook is a secret. Keep it out of Git and this config. Create an
incoming webhook in the demo channel (Channel settings → Integrations →
Webhooks) and either export `THALVI_DISCORD_WEBHOOK` before starting the
client, or put the URL in a local file, `chmod 600` it, and set `webhook_file`
in the script. `config.example.json` reads the environment variable. Discord
shows the message under the webhook's name, not your account.

Before posting, Thalvi reads the webhook's metadata and refuses unless it
points at guild `1566030981687677039` and channel `1566034064333340732`.
The URL must be a plain `https://discord.com/api/webhooks/<id>/<token>` URL;
other hosts, ports, query strings, and redirects are refused. It posts with
`wait=true`, mentions disabled, and reports success only when Discord returns
the created message for that channel. A rejected request changes nothing and
keeps the captured text; a timeout or lost response is reported as unconfirmed
and never retried automatically, so check the channel first. The webhook token
never appears in errors, notices, or metrics. `--dry-run` and `replay` preview
the confirmation without reading credentials or contacting Discord.

No automated test or CI run sends a real Discord message; live delivery needs
separate manual qualification.

The other wake word opens native conversation. Speak naturally and interrupt
by speaking; `stop conversation`, `end conversation` or `never mind` returns
to wake listening. This client uses the server's existing interruption logic.

Set `metrics` to a local path to opt into desktop action/flow timing and
outcomes. Desktop metrics contain neither captured text, destinations, nor
webhook tokens.
Status and confirmation text appear in the foreground terminal.

## Replay without audio or app actions

```sh
uv sync --locked
uv run thalvi-desktop --config config.example.json replay examples/capture.jsonl
```

Replay does not use a microphone, wake model or endpoint. It previews actions
unless `--apply` is explicitly given; that flag really types into the focused
window and sends configured Discord messages. The submit fixture includes an
explicit confirmation event. Live
voice recognition quality and native audio hardware need separate manual
qualification; replay and stub-server tests do not establish those.
