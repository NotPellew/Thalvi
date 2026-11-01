# Native desktop client

This resident client owns the microphone, local wake detection, and the
original target window. The Rust server owns transcription and conversation.
`/transcription` returns text without asking the chat model; `/session` keeps
the conversational protocol. Capture works in the application you are using;
there is no dedicated dictation editor.

The first native target is Hyprland. It needs `hyprctl`, `wtype`, `wl-copy`,
and `xdg-settings`. Other desktop adapters are not yet implemented or qualified.
Python 3.11.16 is pinned; use uv 0.12.15. No code is copied from Hex-Hands-free.

## Run

On Ubuntu, the resident interface uses the system GTK bindings and tray:

```sh
sudo apt install python3-gi gir1.2-gtk-3.0 gir1.2-ayatanaappindicator3-0.1
```

Start the Rust server normally, then from this directory:

```sh
uv sync --locked --extra audio
cp config.example.json ../data/desktop.json
uv run thalvi-desktop --config ../data/desktop.json app
```

Enable listening once in the panel or tray, then click in an input in another
application and say your wake phrase. The tray and notifications report capture,
pending actions and failures without opening a window over your input. Closing
the panel hides it; Quit stops the worker. Pause closes the microphone, and
Cancel capture discards the current capture and returns to wake listening.
The status panel keeps the most recent notice in memory; notifications do not
include dictated text. When the tray is unavailable, the panel stays accessible.

The Thalvi T icon carries a distinct shape for paused, wake listening, capturing,
pending, working and error states. Its accessible description and tray title
include the current state and pending seconds, without dictated text. Use the
tray's Open status / settings entry, or middle-click the icon, to reopen the
panel. The tray host controls how its title appears as a tooltip.

Python 3.11 is required. Configure `wake.model_dir` with existing
`melspectrogram.onnx`, `embedding_model.onnx`, and `hey_mycroft_v0.1.onnx`,
and `wake.capture_model` with the dedicated trained Hey Thalvi ONNX head.
Both heads must accept float32 `[batch, 16, 96]` features and return a single
float32 probability `[batch, 1]`. Longer contexts are rejected before opening
audio because upstream startup and reset history cannot satisfy arbitrary lengths.
Paths are relative to the config file; assets are never downloaded automatically.
The example thresholds are provisional and require calibration. The trained head,
backbone provenance, threshold and live performance are not yet qualified. Verify
model and training data licenses separately before distribution or use.

Audio dependencies are optional for replay and tests. openWakeWord and ONNX Runtime
run local CPU inference; sounddevice supplies audio and NumPy converts frames.
WebRTC VAD checks speech locally so an ending's quiet interval does not depend
only on delayed transcription updates.

Use headphones for native conversation: this prototype has no acoustic echo
cancellation. The phone/browser client retains its own media handling. The
native microphone stays open while listening is enabled; audio is not saved. No audio is
sent to the server while waiting for a wake word. `--once` closes after one
capture or conversation; `--dry-run` previews desktop actions while still
using the configured transcription/conversation endpoints.

## Captures and tasks

After the capture wake word, keep speaking your text and ending phrase; initial
capture plays no cue, and speech that follows the wake phrase without a pause is
kept. Microphone blocks captured during a later cue are discarded. An anticipatory cue marks a provisional ending; a descending
cue marks cancellation or continued speech. A resolution chime acknowledges
an accepted ending before delivery begins; it does not mean delivery succeeded.
A dissonant cue signals an action or playback failure.
Audio is buffered while the target window is read and the recognizer connects;
startup must complete within eight seconds. Ordinary pauses can produce separate transcription pieces
but do not end the capture without an ending command. All ending phrases can be
changed in the config. `ending_pause` defaults to 2.5 seconds and accepts values
from 2 to 3 seconds.

| Ending | Result |
| --- | --- |
| paste that / finish dictation | After the quiet interval, insert the text without Enter. |
| submit | Show target confirmation during the quiet interval, then insert and press Enter. |
| do that | After the quiet interval, run the matching configured action. |
| scratch that | Discard this piece, or the preceding piece if spoken alone. |
| never mind | Discard the capture and return to wake listening. |

An ending is provisional while the countdown runs. Trailing punctuation and up
to two recognized filler or non-speech markers are removed with the ending.
Microphone audio is withheld from the recognizer while an ending is pending,
and a stricter speech detector filters trailing noise. Three consecutive speech
frames (240 ms) are buffered and relayed so the recognizer can distinguish real
continuation without clipping its beginning. Only meaningful recognizer
continuations cancel the ending; the ending words then remain part of your
dictation. Empty and artifact-only recognition results neither restart the
quiet interval nor reactivate a canceled ending. The accepted cue plays after
the quiet interval is accepted. The speech detector's trailing activity delays
the start of the quiet interval.
Empty partial updates do not cancel an ending. Opt-in metrics record pending,
quiet, cancellation and acceptance states.
For submit, the pending notification asks you to confirm the destination by
pausing; no second spoken yes is needed. Cancel or keep speaking to stop it.

The original window identity remains bound through the countdown and delivery.
Keep the intended input selected while dictating: switching between fields
inside one window is not tracked. If the original window closes or cannot be
focused, text is kept on the clipboard instead of being typed elsewhere.
Typing can be partial on failure or interruption; it is never retried
automatically.

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
nor debugger messages. Each record includes its UTC creation time. New logs rotate
at 5 MiB and retain two numbered backups; new log files are private to the user.
Existing oversized logs are preserved until they age out of those backups.
Status and confirmation text appear in the foreground terminal.

Wake-model loading, the target-window lookup and each detector call record
their host elapsed time and outcome when metrics are enabled. Detector outcomes
are `none`, `capture`, `conversation`, `error` or `cancelled`. These timings
measure local call cost, not perceived wake latency.

With metrics enabled, the client also tracks peaks across every wake frame,
reporting the first frame and then two-second windows as a coarse `quiet` or
`activity` label. The terminal announces the first label and later changes.
`quiet` means below the diagnostic threshold, not absent input. The level is
signal strength for diagnosis: it is not voice-activity detection, does not
prove speech, and never changes the wake decision. Microphone audio,
transcripts, destinations and device paths are never recorded, and audio is
not saved.

Detector and level timings write in background batches of 125 records, with at
most one batch writing and one waiting. A stalled writer can drop optional
timings with a warning; it does not delay capture. Shutdown closes the
microphone before waiting for the remaining metrics.

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

To opt in to a second transcription after dictation finishes, add this to the
desktop JSON configuration (the API key may be omitted):

```json
"stt": {
  "secondary": {
    "url": "http://127.0.0.1:9000/v1",
    "model": "whisper-large-v3",
    "api_key": "optional"
  }
}
```

The configured endpoint receives the complete dictation audio after its ending
is accepted. If it fails, Thalvi uses the live transcript.

For [Meta's recording API](https://dev.meta.ai/docs/speech-to-text), set
`stt.secondary` to:

```json
{
  "api": "meta",
  "url": "https://api.meta.ai/v1",
  "model": "muse-voice-transcribe-1.0",
  "api_key": "your-meta-api-key"
}
```

This uses `/v1/asr/transcribe`; omitting `api` uses the OpenAI-compatible
`/audio/transcriptions` route. Meta's recording and realtime endpoints run the
same model, so a second pass does not guarantee improved accuracy.
