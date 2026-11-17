# Thalvi

Talk to your coding agents instead of typing to them. Say "Hey Thalvi" in any
window, say the whole thing, then decide: paste it, send it, or drop it.

Built with Muse Spark Open.

Typing a good prompt is a chore, so prompts end up short and leave out what the
agent needs. Speaking takes that barrier away, most of all for long prompts. You
can correct yourself as you go and look at something else while you talk, and
the words still land in the window you started from. The agent workflow is
tested with Muse Code on Hyprland.

Thalvi is a desktop app for Hyprland, Windows, and GNOME Wayland, where
dictation is copied to the clipboard. It listens for "Hey Thalvi" and pastes
what you dictate into the window you are using. It keeps nothing about you
beyond its settings, recordings you opt into and its dictation history.

**Download the desktop app:** the
[latest release](https://github.com/NotPellew/Thalvi/releases/latest) has the
Windows installer and the Linux AppImage; see [Install](#install).

It recognizes speech on this computer by default. Any speech server that
speaks Meta's realtime ASR protocol or OpenAI transcriptions can replace it,
including the [self-hosted recognizer](#a-self-hosted-recognizer) in this
repository. Spoken conversation, with a server and web client, lived here up to
the tag
[`conversation-final`](https://github.com/NotPellew/Thalvi/tree/conversation-final).

## Desktop app

Say "Hey Thalvi" in any window, dictate, then end with a phrase:

| Say | Thalvi |
| --- | --- |
| "paste that" or "finish dictation" | pastes the text into the window you were in |
| "write that out" | has the writing model turn the words into proper text, then pastes that ([Write it out](#write-a-dictation-out)) |
| "submit" | pastes it and presses Enter |
| "do that" | runs a configured action, such as opening a site or focusing an app; the action also runs without "do that" when the whole capture is the command, for example "open google.com" |
| "scratch that" | drops the last piece |
| "never mind" | drops the whole capture |

Thalvi puts the text on the clipboard and presses the paste shortcut in the window,
so line breaks arrive as line breaks; the text stays on the clipboard. A terminal can
run each pasted line, so there Thalvi pastes one line only: text with line breaks
stays on the clipboard for you to paste once you have read it, and "submit" pastes
it after showing it to you. The shortcut is Ctrl+V, and Ctrl+Shift+V in a known terminal on
Hyprland (kitty, foot, Alacritty, WezTerm, Ghostty, GNOME Console and Terminal,
Konsole, Ptyxis, xterm). In another terminal Ctrl+V pastes nothing; paste yourself.
An overlay at the top of the
screen shows what Thalvi is doing and never takes focus. The tray pauses,
cancels, copies the last error and opens Settings.

| Desktop | Support |
| --- | --- |
| Hyprland | Full |
| Windows | Full |
| GNOME | Not yet ([#101](https://github.com/NotPellew/Thalvi/issues/101)) |

### Install

Download the latest `desktop-v*` release from
[Releases](https://github.com/NotPellew/Thalvi/releases). No Git, Rust or Node
is needed.

- **Windows:** run `Thalvi_<version>_x64-setup.exe`. It installs for your user
  into `%LOCALAPPDATA%\Programs\Thalvi`, adds a Start menu entry, and downloads
  WebView2 if missing. Uninstall from Settings → Apps; your config, models and
  recordings stay.
- **Linux:** install `wtype` and `wl-copy`, then
  `chmod +x Thalvi_<version>_amd64.AppImage` and run it. Keep it on a path
  without spaces. Thalvi updates the AppImage in place (see
  [Updates](#updates)); settings and models stay.

Start it once and it stays in the tray; starting it again opens Settings. The
first start writes the default config, opens Settings on Recognition, and downloads
the wake models and, once, the speech model, so the first "Hey
Thalvi" works with no setup. If Thalvi cannot start, for example because its
config is broken, a message box names the problem and the file.

### Speech recognition

Settings → Recognition keeps a list of speech servers. A fresh install has one,
**On this computer**, with Nemotron Streaming, so it recognizes without a
server; add a server to use one. **Live dictation** uses exactly one of them, and **Use** switches to another at once; it applies from the next
capture, and a capture already running keeps its recognizer. At the top of
Live dictation, **Next dictation** names the entry (and, on this computer, the
model) that will handle the next dictation and whether it is ready. The entry
in use is the live recognizer, marked **Live**: it shows your words and hears the
ending phrases, and it has no **Use** button. IDs and addresses are
under each entry's collapsed **Details**. The kinds are:

- **Live as you speak — Meta realtime ASR:** shows your words as you speak,
  through a recognizer that speaks Meta's realtime ASR protocol, such as the
  self-hosted one below, which the app connects to itself (address, model, API
  key).
- **After you stop — OpenAI transcriptions:** sends each capture to an
  OpenAI-compatible `/v1/audio/transcriptions` endpoint when you release the
  dictation key, and the text streams back once the recording is sent. A
  wake-word capture ends after a pause in speech (address, model, API key). Set
  **Request** to Chat with audio for a server that takes the recording only on
  `/v1/chat/completions` (no streaming); set **Language** to a two-letter code
  such as `de` to stop the server guessing it.
- **On this computer:** dictates without a speech server or a network; the default.
  Use **Show models for** to filter the list of models (each model recognizes the
  languages on its card), then pick a model.

An existing config
with `stt.engine`, `stt.model`, `stt.secondary` and `server` becomes entries
the first time Thalvi reads it; a saved Thalvi server speech entry is dropped,
and if active, On this computer becomes active with a one-time notice. An API key is shown
as saved and never again; leave it blank to keep it. A server that cannot be
reached is named in the error, and Thalvi never falls back to another one.

For the on-device model, Thalvi downloads a missing model when it starts (once:
575 to 671 MB depending on the model, or 1.3 GB for Nemotron Unified's GPU files; the notice names the size), and Next dictation
shows the download's percent until it is ready. While it
downloads or if it fails, **Use a speech server instead** opens the form to add
a server, and after a failure **Retry** fetches the model again; Thalvi never
switches recognizers by itself. After that it works offline. A model you pick applies from the next capture, and
a capture already running keeps the model it started with. Settings shows each
model's description, languages and size, whether it is **Selected** and
**Downloaded** or **Not downloaded**, and can download it; its ID, source and
license are under the card's collapsed **Details**. Or you can point to your
own model folder and check it before it is used. That folder sits under the speech
model's **Details**: it holds `encoder.int8.onnx`, `decoder.int8.onnx`,
`joiner.int8.onnx` and `tokens.txt`.
Word errors below are on LibriSpeech (120 clips) and on 48 of the developer's
own takes, int8 on the CPU, language detected automatically. The unified model
also made 8.38% on German and 7.09% on Spanish (80 Multilingual LibriSpeech clips
each).

| Drop-down entry (config name) | Model | Languages | LibriSpeech | Developer's takes | Notes |
|---|---|---|---|---|---|
| Nemotron Streaming (`nemotron_160ms`, default) | Nemotron Speech Streaming 0.6B | English | 3.46% | 7.14% | Lightest on the CPU; text appears soonest. Mostly without punctuation. |
| Nemotron Unified (`unified_160ms`) | Nemotron Unified ASR 0.6B | 25 European languages | 3.03% | 6.35% | Punctuation and capitals. Re-reads the last 6 s every 160 ms: about 70 ms of work per step on 4 cores of a Ryzen, so on the CPU it needs a fast CPU. On Linux with a discrete GPU it runs there instead, through WebGPU and an fp16 export (about 30 ms a step on an RTX 5070 Ti, a 1.3 GB download); Settings says which it uses. Until the GPU files are downloaded it runs on the CPU, and Settings offers the download. Text appears about 0.5 s later. Our own export, under OpenMDW-1.1. |
| Parakeet TDT (`parakeet_tdt_v3`) | Parakeet TDT 0.6B v3 | 25 European languages | 2.33%* | 4.76%* | Punctuation and capitals. Light on the CPU: about a seventh of real time on 4 cores, against about one for Unified. Not a streaming model: it re-reads the last 2.6 s every 640 ms, so words may change while you speak, and the text is final about a second after you pause. At the pause it re-reads the whole stretch once, so the final comes later after a long unbroken stretch: that re-read took about 1 s after 30 s of speech and 3.5 s after 90 s, on 4 cores. Under CC BY 4.0. |
| Whisper Turbo · CPU (`whisper_turbo_cpu`), experimental | Whisper large-v3-turbo, q5_0, in whisper.cpp | Many; detected per capture (tried: English, German) | – | – | Text appears only after you stop speaking. Runs in a whisper.cpp helper that Thalvi ships, starts and stops itself; no server or console. Slow: about real time or slower on 8 cores of a Ryzen 9800X3D (an 8 s capture took 9–15 s). Ignores the word list, so custom names may be misheard. 575 MB, MIT. |
| Whisper Turbo · Vulkan GPU (`whisper_turbo_vulkan`), experimental | the same files | the same | – | – | The same helper built with Vulkan, on the first GPU it finds. While you speak, it re-reads everything said so far on the GPU and shows it as provisional text, which the final replaces after you stop; only the final is pasted. Settings names the GPU, and says so when there is none or the driver lacks Vulkan, without falling back to the CPU. On an RTX 5070 Ti an 8 s capture took about 0.1 s after a 4 s first warm-up; a small integrated GPU was slower than the CPU. Untested on other GPUs. |

Nemotron Speech Streaming at 80 ms and 560 ms (3.62% and 3.46% on LibriSpeech)
are left out because they offer no real choice over 160 ms; either works as your
own model folder. \*Parakeet was measured on 60 of those LibriSpeech clips and 24
of the takes, where Unified made 2.25% and 4.76%; on 40 clips each it made 12.44% on
German (Unified 8.32%), 7.95% on Spanish (6.91%) and 9.28% on a German and an
English speaker joined (12.32%). Nemotron
3.5 multilingual was rejected because it made 15% word errors on the takes. Nemotron Speech
Streaming is under the
[NVIDIA Open Model License](https://www.nvidia.com/en-us/agreements/enterprise-software/nvidia-open-model-license/).
The unified model is NVIDIA's
[Nemotron unified ASR](https://huggingface.co/nvidia/nemotron-labs-unified-asr-0.6b)
under OpenMDW-1.1, exported to ONNX by Thalvi. Parakeet is NVIDIA's
[Parakeet TDT 0.6B v3](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3) under
[CC BY 4.0](https://creativecommons.org/licenses/by/4.0/), in sherpa-onnx's int8 export.

**Extra recognizers** (none switched on by default) hear each accepted capture
besides the recognizer you chose. Any recognizer can be one: a realtime speech
server, an OpenAI-compatible endpoint or the model on this computer, switched on with
"Also transcribe with" on its row (any entry but the one in use), or an OpenAI-style
or Meta transcription API listed under Extra recognizers. The recognizer you chose
stays live: it shows the words, hears
the ending phrases and decides what happens to the text (paste, submit, task,
write). A live-kind extra hears the same audio from the start of the capture, so its
text is ready about when the capture ends; its partials are never shown and its text
decides nothing. A second on-device model stays loaded and adds its memory and
processing. If an extra cannot connect, load or keep up, only it is left out of that
capture's merge, with a notice; the live capture is never slowed or stopped.
Each extra has a switch and says where
its audio goes ("Audio goes to" the address's host), and the group says "Off. Dictation
pastes the live text." until one is on, then which are on. Every extra hears the same capture, and the chat model of **Writing**
(same `url`, `model` and key, its own fixed prompt) merges the live text and the
extras' texts into one, choosing between what the recognizers heard; the overlay says
"Merging…" meanwhile, and cancel works as ever. The merged text is pasted only if
every word of it appears in at least one recognizer's text. Otherwise, or when
fewer than two texts arrive, an extra or the chat call fails, or the eight seconds
from the end of the capture run out (the extras get five of them), the live text is
pasted and the overlay says why. An extra that answers too late is left out of the
merge and still recorded, in the capture's `text.txt` and the metrics. "Write that
out" and the write key write out the merged text, with a second chat call.
Extras can be switched on only while Writing has an endpoint, and the endpoint
cannot be removed while one is on; without one, the group says so and **Go to the
endpoint setting** moves to it. Switching one off keeps its address and key.
An upload extra is stored as an entry of kind `extra`; an old `second_pass` entry loads as
one, and is written as `extra` the next time the settings are saved. A live-kind
entry is an extra with `"extra": true`, which the active entry can't be.

### A self-hosted recognizer

`speech/nemotron-asr.sh` installs a live recognizer on your own GPU, which
the app reaches as a Meta realtime ASR entry. It runs NVIDIA's
[Nemotron unified ASR 0.6B](https://huggingface.co/nvidia/nemotron-labs-unified-asr-0.6b)
(OpenMDW licence) live with about 0.56 s of lookahead, finds the language
itself (English, German, Spanish and 22 more European languages) and
punctuates. On an RTX 2080 it uses about 3 GB of VRAM and takes about 75 ms
for each 0.16 s of audio. Run it from a checkout on the GPU machine:

```sh
speech/nemotron-asr.sh
systemctl --user enable --now thalvi-asr
```

It needs `uv`, `curl` and an NVIDIA driver, and downloads about 2.5 GB of
weights plus NeMo. `SPEECH_DIR`, `HOST`, `PORT` (default `8012`) and
`ENDPOINT_MS` (silence that ends a stretch of speech, default 600) set where it
lives, listens and how it cuts. It has no authentication, so keep `HOST` at
`127.0.0.1` or a private address such as the machine's tailnet IP. It prints
what to enter in Settings → Recognition: a **Live as you speak** entry with
address `ws://127.0.0.1:8012`, model `nemotron-labs-unified-asr-0.6b` and no
API key.

With it, Settings → Recognition shows a **Word list**: names and terms, one per
line, whose spelling recognition should favour (such as "Cybrex2"). The list
is used by local recognition and sent to direct realtime speech servers at
each connection. A model that ignores the list says so on the row.

### Microphone

Settings → Listening has **Listening**, which pauses and resumes right away,
and the **Microphone** to use. Choosing a device applies it like every other
setting: while Thalvi is listening it opens the new device first and closes the
old one once that works, or after the capture that is running; while paused, the
device is used when listening resumes. **Active microphone** names the device
actually open. If the chosen device can't be opened, the reason shows under the
choice, with the device that stays open instead; Thalvi never opens another one
by itself.

### Start at login and app menu

Settings → App → Startup, both off by default and applied right away:

- **Start Thalvi when I log in** adds a login entry that starts Thalvi in the
  tray. Thalvi remembers the choice and restores the entry each time it starts,
  so an upgrade or a moved AppImage is fixed by starting it once. If you turn
  Thalvi off in the system's startup list, it stays off. Hyprland runs login
  entries only under a session manager such as `uwsm`; without one, the row
  shows the `exec-once = … --login` line to add to `hyprland.conf`.
- **Show Thalvi in the app menu** (AppImage) adds a menu entry and icon under
  `~/.local/share`. On Windows the installer owns the Start menu entry.

### Updates

Thalvi keeps itself on one update channel, chosen in Settings → App → Updates
(applied as you choose it): **Stable** (the default) follows released versions;
**Development** also follows the builds the developer approves for testing.
Stable never installs a development build, and neither channel ever installs an
older version, so switching from Development to Stable keeps the development
build until a newer stable release exists.

- **Supported installs:** the Windows installer's per-user folder, and the Linux
  AppImage in a folder you can write to. A copy that cannot replace itself, such
  as a build from source or an AppImage in a read-only folder, says so under
  Updates and in words you can follow; it does not keep trying.
- **How it runs:** a check a few seconds after Thalvi is usable, then at most
  once a day while it runs; **Check for updates** checks at once, and a failed
  check says so and is retried by the button only. The download is verified
  against the public key built into the app before anything is installed. A
  check sends only what is needed to find the right version and is counted, with
  its time and outcome, in the opt-in metrics.
- **Restart:** a downloaded update installs, and Thalvi restarts, only while it
  is idle: no capture, key held, ending countdown or delivery in progress. Until
  then the tray and Settings show that it is waiting. Nothing is pasted or sent
  by an update, a paused microphone stays paused, and no window takes focus. The
  tray and Settings say "Updated to X" once afterwards.
- **If it fails:** a download, signature or installer failure leaves the
  installed version working and says what happened under Updates and in the
  tray. If an install did not take, the next start says so. To recover, download
  the version you want from [Releases](https://github.com/NotPellew/Thalvi/releases)
  and install it as in [Install](#install); settings and models stay.

### Recordings

Settings → History & recordings → Recordings has a switch each for metrics, text and audio recordings, all
off by default; a switch applies between captures. Metrics record when each wake,
capture and model call happened, with no words or audio. Audio recordings include
what the microphone heard and about two seconds before a wake. They stay on this
computer and are never uploaded. History keeps what you dictate, when, where it
went and how it ended, never the audio. Turning history off stops new entries and
deletes none.
**Open folder** opens `recordings/` in the data folder (`~/.local/share/thalvi`
on Linux, `%LOCALAPPDATA%\thalvi` on Windows):

- `<date>/<time>/` per wake, in local time: `wake.wav` (about 2 s before the
  wake), `speech.wav`, and `text.txt` with what was heard and what was
  delivered.
- `metrics.jsonl`: timing and outcome of each step, near misses of the wake
  word, wakes that delivered nothing, and one summary line a minute for wake
  detection.

### Errors

An error shows on the overlay for 5 seconds in a short headline, and again if
it refuses a press of the hold key. The tray icon and Settings keep it until the
next capture; Settings shows the full text with **Copy** and **Dismiss**. Copy, the tray's
**Copy last error** and `thalvi-desktop copy-error` all copy the same report:
the error, its technical cause, time, version, system and relevant settings,
never API keys.

### Commands and Hyprland

`thalvi-desktop toggle`, `pause`, `resume`, `cancel` and `copy-error` control
the running app (with the AppImage, run it with that word). `status` prints
Waybar JSON, with the class `keyboard` while wake words are off. They work on Hyprland, GNOME and Windows, and start nothing when
Thalvi is not running. The
examples in [`integrations/hyprland/`](integrations/hyprland/) add a Waybar
microphone button (left click toggles listening without moving focus, right
click opens Settings) and key bindings in `hyprland-binds.conf`, such as
`bind = SUPER, E, exec, thalvi-desktop copy-error`. Pausing cancels the
current capture and closes the microphone.

### For agents

A program you run, such as a coding agent, can set Thalvi up, read and change the
settings, feed it audio from a file in place of the microphone, see what Thalvi is
doing and what its overlay shows, and control it with `thalvi-desktop` commands.
Nothing that was dictated is ever in an answer. Each prints
one JSON object and exits 0, or non-zero when the answer is `"ok": false`.
They talk to the running app over a private socket in your runtime folder on
Linux, or a named pipe only your user can open on Windows; nothing listens on
the network, and a command never starts the app or the microphone.

| Command | Answer |
| --- | --- |
| `state` | `{"ok": true, "state": "listening", "paused": false, "copy_error": false, "feeding": false, "overlay": {"state": "hidden"}, "recognizer": {"name": "On this computer", "kind": "on_device", "model": "nemotron_160ms", "status": "ready"}}`. `state` is `paused`, `loading`, `listening`, `keyboard` (listening with wake words off), `capturing`, `pending`, `transcribing`, `merging`, `delivering` or `failed`; `failed` adds `"failure": {"headline", "reason"}` (`reason` is left out for a delivery failure, whose notice can hold the dictated text), and `awaiting_confirmation: true` marks a submit that waits for you. `overlay.state` is what the overlay shows: `hidden` (also when the overlay is off), `listening`, `quiet`, `countdown`, `writing`, `merging`, `working`, `delivered`, `error` (with the `headline` shown) or `loading`. `recognizer` is the speech entry that handles the next dictation (`model` only for a catalog model) and whether it is `ready`, `loading`, `failed` or `paused`, as the Recognition page says. Nothing that was said is in it. |
| `watch [--json]` | Follows the running app and prints again only when the view changes. In a terminal it redraws in place: the phase, the overlay and any error headline, the next recognizer, the latest dictation's route and outcome, and the recent window's stage medians. `--json` prints one JSON line per change, the `state` fields plus that dictation as `capture`. Anywhere else it prints one plain line per change. When Thalvi is not running, or quits, it prints `Thalvi is not running. Waiting for it…` (or `{"running": false}`) and keeps waiting. Ctrl+C ends it. Nothing that was said is in it. |
| `feed <wav-file>` | Plays the file's audio in place of the microphone, at the pace of speech, through the same wake word, recognition, endings and delivery as spoken audio; the microphone is ignored while it plays. Replies when the file has been fed: `{"ok": true, "took_effect": true, "state": "…"}`, with the state then; a capture it started carries on. Refused with `"Thalvi is paused."`, `"A capture is running."`, or the reason for a file that is missing, unreadable, not a mono or stereo 16-bit PCM WAV, or longer than the capture limit. Needs an absolute path (the command makes it absolute). `cancel` stops the feed and any capture it started. Capture metrics record `"source": "file"`. |
| `models` | `{"ok": true, "models": [{"kind": "speech", "name", "title", "size", "downloaded", "progress_percent", "runs_on"}, …]}`: the wake, speech and local chat models, `progress_percent` only while one downloads, `runs_on` (the Recognition page's hardware line) only for speech models. |
| `model-download <wake\|speech\|chat> <name>` | Downloads the model (`chat chat` for the local chat model); replies when it is ready, `{"ok": true, "downloaded": true}`, or has failed, `{"ok": false, "downloaded": false, "error": "…"}`. `models` shows the progress meanwhile. |
| `speech-check '<json>'` | Checks one speech server entry exactly as Settings does before saving it: `{"ok": true, "ready": true}`, or `{"ok": false, "ready": false, "error": "…"}`. Saves nothing; keys are never repeated. |
| `show [page]` | Opens Settings, at `listening`, `recognition`, `commands`, `chat`, `overlay`, `history` or `app` when one is named: `{"ok": true, "took_effect": true}`. |
| `quit` | Quits as the tray's Quit does: `{"ok": true, "took_effect": true}`. |
| `settings get` | `{"ok": true, "settings": {…}}`, every setting Settings offers plus `start_at_login` and `show_in_menu`. Speech server keys appear only as `"has_key": true/false`. |
| `settings check '<json>'` | `{"ok": true, "valid": true}`, or `{"ok": false, "valid": false, "error": "…"}` with the message Settings shows. Saves nothing. |
| `settings set '<json>'` | `{"ok": true, "changed": true, "applies": "now"}`, or `"next_capture"` while a capture runs. A refusal is `{"ok": false, "error": "…", "changed": false}` and changes nothing. |
| `pause`, `resume`, `toggle`, `cancel`, `hold-start`, `hold-end`, `write-start`, `write-end`, `copy-error` | `{"ok": true, "took_effect": true, "state": "…"}`, or `{"ok": false, "took_effect": false, "error": "…", "state": "…"}`, such as when Thalvi is already paused. `hold-start` starts a dictation that `hold-end` or `cancel` ends; `write-start` and `write-end` do the same with writing on release. |
| `metrics summary` | `{"ok": true, "enabled": true, "window_hours": 24, "stages": {"connect", "partial_lag", "final", "delivery"}, "captures": {"ok": 3, …}, "recent": [{"outcome": "ok", "source": "file", "route": "paste"}, …]}`; each stage has `count`, `outcomes`, `p50_ms` and `p95_ms`. `recent` is the last few dictations in the window, oldest first, with `route` only when one was delivered. `"enabled": false` and a message when metrics are off; `"ok": false` and an `error` when a metrics log exists but can't be read. Nothing that was said is in it. |

The change given to `settings check` and `settings set` is a JSON merge patch
over what `settings get` returns: objects merge, `null` removes a key, and a
list such as `stt.entries` or `stt.phrases` is replaced whole, so read it first
and send it back with your change. Give `-` instead of the JSON to read it from
standard input. A speech server entry takes `api_key` to set a key, `""` to
clear it, and keeps the saved key when `api_key` is absent. A changed
microphone opens beside the open one, as in Settings.

A confirmation stays with you: no command answers one, and a submit that waits
for it shows as `awaiting_confirmation` in `state`. Any other failure of a
command is `{"ok": false, "error": "…"}`; a command with no running app says so
and exits 1, and one that is not a command exits 2.

### Wake words off

Settings → Listening → **Wake words** is a switch, on by default. **Threshold**
is under **Details**: higher wakes less often by mistake, lower catches quieter
speech. The catalog phrases start at 0.7 (Hey Thalvi) or 0.5 (the others); a
file of your own starts at 0.5. **Capture limits**, also under **Details**,
stop a capture that hears no words (10 to 600 s, starting at 60) or that runs
too long (60 to 3600 s, starting at 300). Off, Thalvi keeps
listening for the hold keys and the microphone stays open, but no wake word starts
anything, plays a cue or delivers, and the wake model scores nothing. It applies at
once, without a restart, survives restarts and pause and resume, and is
stored as `"wake": {"enabled": false}` only when it is off. The tray, `state` and
Settings tell the three states apart: listening with wake words, listening for the
hold keys only (the tray tooltip says "hold keys only"), and paused. Pausing still
stops the hold keys too.

### Hold a key to dictate

Hold a key, speak, and let go: the text is pasted into the window focused when
you pressed, without the pause countdown and without Enter. A hold shorter than
a third of a second, or with no words, pastes nothing and says so. No spoken
phrase ends or cancels a hold: words such as "never mind" are dictated as text.
To abandon a hold, use the `cancel` command: the tray's **Cancel**, or a key
bound to `thalvi-desktop cancel` (the Hyprland examples bind SUPER+Escape).

**Settings → Commands → Quiet before acting** is under **Details**: time to keep
talking or cancel, 0 to 5 s, starting at 0.7 s before a paste and 2.5 s before
paste-and-Enter or a task. An app's window classes, executables and keys, and a
script's label, are under that entry's **Details**. For Discord, the words
after the phrase are the message.

Choose the keys under **Settings → Listening → Hold keys**: **Change…** records the
next combination you press in the Settings window (Escape cancels), and **Restore
default** goes back to the desktop's own key. A key applies at once.
Thalvi refuses a key that is not a key it can use, the same key for both actions,
and one the desktop will not bind, says why under the row, and keeps the keys that
worked. A key other than the default is stored as `hold_key` in the config. GNOME
shows why it has no hold key and no controls.

- **Hyprland:** F9, with no modifier. Thalvi binds the key in Hyprland itself while
  it runs and binds it again after `hyprctl reload`, so `hyprland.conf` needs no
  lines for it. Pick another of F1 to F12. Hyprland types
  any modifier you still hold into the pasted text and skips the release when a
  modifier comes up first, so the key must be a single key. If another bind already
  uses the key, or `hold-start` lines from an older `hyprland-binds.conf` are still in
  `hyprland.conf`, Thalvi says so and binds nothing until you remove them or pick
  another key; a key that cannot be bound keeps the previous one working.
- **Windows:** Alt and the key left of X (Z on a QWERTY keyboard, Y on QWERTZ).
  The app watches the keyboard itself and keeps the key from the focused app; the
  hold ends when the key and Alt are both up. Pick another, such as `Ctrl+Shift+F9`;
  the config names it by its place on the keyboard (`Alt+KeyZ`, `Digit1`, `Space`,
  `F9`). While an administrator window is focused, Windows
  shows Thalvi no keys and lets it send no keys, so the key reaches that window
  as usual and no dictation starts.
  **AltGr alone** (`AltRight`, the right Alt key) can be the dictate or write key
  too. It is never kept from the focused app, so AltGr with another key still
  types its character, as `@`, `€` and `{` do on a German keyboard, with no cue or
  overlay. The hold starts when AltGr has been down for 0.2 s with no other key; the
  words said meanwhile are kept. Hyprland refuses it.

### Write a dictation out

Say "tell Anna I'm late, sorry, write that out", or hold the write key the way
you hold the hold key, and Thalvi sends the words you said to a model that turns
them into proper text in the language you spoke. The overlay says "Writing…" and
shows the text as it streams in; when the stream ends the text is pasted like any
other dictation, and **Ctrl+Z** undoes it. Cancel as usual before then and nothing
is pasted. Without "write that out" or the write key, the words are pasted as
recognized. There is no write-and-send: press Enter yourself after reading it.

Under **Settings → Chat model → Writing**, choose **On this computer** and
**Download chat model** to install Qwen3 4B (Q4_K_M, about 2.5 GB, Apache-2.0;
English, German and Spanish). Settings shows the total size before downloading.
Thalvi downloads a pinned, checksum-verified prebuilt llama.cpp runtime: Vulkan
when it finds a usable GPU, otherwise CPU. You can select CPU explicitly.
Nothing is compiled and no service needs managing. The first writing or merge
loads the model with a visible notice; that load is separate from the 15-second
writing limit. The child listens only on loopback with a fresh private key and
thinking disabled, and stops when Thalvi quits. A crash restarts it once on the
next use. Updating Thalvi keeps the download; **Remove chat model** asks before
deleting the model and keeps the runtime.

Choose **Manual endpoint** instead for any OpenAI-compatible chat completions
endpoint (Glimmer, Baseten, llama.cpp, Meta): `writing.url`, `model` and `api_key`
in the config. Switching sources keeps the saved endpoint and downloaded model.
The address, model and key of a manual endpoint are under **Details**, which stays
open until both the address and the model are set; a saved key is shown only as
"key set". A speech server's address, model and key fold the same way.
The model gets the **Writing prompt**
(`prompt`) as its instructions and the recognized words as the message: no window,
app or other context. The default prompt follows what you ask for (an email, a tone,
a wording), invents nothing and keeps line breaks to where the text needs them, such as an
email; edit it in Settings or restore the default there.
How closely a model follows it depends on the model. Changing the prompt never
calls the model.
If it is not set up, fails, answers with nothing, or takes longer than 15 seconds, nothing is pasted,
Thalvi says what happened, and your own words go on the clipboard to paste with
Ctrl+V.

A model that reasons before it answers, such as Qwen3, shows "Thinking…" until its
text starts; what it reasons is never shown or pasted. The 15 seconds include the
thinking, and it often takes longer, so turn thinking off on the model server: for a
llama.cpp server, use `--reasoning-budget 0 --jinja --chat-template-kwargs '{"enable_thinking":false}'`. Thalvi does not
ask the model itself, because some endpoints refuse a request with a setting they
don't know.

**Opening phrases** (`writing.openings`, under the Writing prompt in Settings) give
a kind of text its own prompt, such as "write me an email" or "write a message".
When the words you said begin with a phrase, whole words only and ignoring case and
punctuation, that rule's prompt is used instead of the Writing prompt, and the
longest matching phrase wins. The words are sent as said, opening included. No
match uses the Writing prompt. Rules apply only when you write out with "write that
out" or the write key. A new rule starts as a copy of the Writing prompt, and a
rule edited in Settings applies from the next capture. Adding or editing a rule
never calls the model.

**App styles** (`writing.apps`) use a prompt for the app you write into, optionally
matching part of its window title. App names match exactly, ignoring case, and the
first matching style wins. The app name and title are matched only on this computer
and are never sent to the writing model. A spoken opening phrase wins over an app
style. A new style starts as a copy of the Writing prompt. Changing one never calls
the model.

- **Hyprland:** F10, with no modifier, bound by Thalvi like the hold key.
- **Windows:** Alt and X, kept from the focused app like the hold key.

Change the write key in the **Write key** row of the same **Hold keys** group
(`write_key` in the config). It must differ from the dictate key.

### Config

Settings edit `desktop.json` in `thalvi/` under the user config folder
(`~/.config` on Linux, `%APPDATA%` on Windows); without one the defaults apply.

### Build from source

Needs Rust (`rust-toolchain.toml`) and Node (`.node-version`), plus:

- **Linux:** `libasound2-dev libwebkit2gtk-4.1-dev libayatana-appindicator3-dev`
  to build; `wtype` and `wl-copy` to run.
- **Windows:** the MSVC build tools.

```sh
npm ci --prefix crates/thalvi-app/ui && npm run build --prefix crates/thalvi-app/ui
cargo run --release -p thalvi-app   # or: cargo install --path crates/thalvi-app
```

### Release

A `desktop-vX.Y.Z` tag on `main` builds both packages; after the developer
approves the `release` environment, `publish` signs them for the updater and
publishes them with their `.sig` files and `latest.json`, which
`releases/latest/download/latest.json` serves to Stable.

Each push to `main` (or running the workflow on it) builds a
development build numbered `X.Y.Z-dev.N` from the version in `Cargo.toml`; a
newer push replaces a build still waiting. Once approved, it publishes as a
prerelease and the `desktop-dev` release's `latest.json`, the Development feed,
names it, or the latest release when that is newer. The last five development
builds are kept. Since `X.Y.Z-dev.N` sorts below `X.Y.Z`, bump `main`
to the next version right after a release.

- **Key:** made once with `npm exec --prefix crates/thalvi-app/ui -- tauri
  signer generate` and a password. The public key is `plugins.updater.pubkey`
  in `crates/thalvi-app/tauri.conf.json`; the private key and its password are
  the `release` environment's `TAURI_SIGNING_PRIVATE_KEY` and
  `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` secrets, with an offline backup.
  Without either secret, or with a key that does not match the public key, the
  release fails and publishes nothing.
- **Lost or leaked key:** make a new pair, commit the new public key and replace
  the secrets. Installed apps cannot verify releases signed with the new key, so
  their users reinstall once from the release page.

## Status

Built for the
[Meta Global AI Developer Hackathon](https://cerebralvalley.ai/e/meta-ai-developer-hackathon)
(October 26 – November 4, 2026).

## Hackathon provenance

All code up to the tag
[`hackathon-submission`](https://github.com/NotPellew/Thalvi/tree/hackathon-submission)
was written during the hackathon (October 26 – November 4, 2026); later work
continues in the same open history, and the commit history is the record.
Pull requests land as merge commits, so the individual commits behind each
change stay visible, and `main` is never rewritten.

The idea draws on lessons from an earlier, private voice-assistant prototype by
the same author. Nothing is copied from it or from any other repository;
third-party code enters only as declared dependencies.

AI assistance is used in this project and disclosed: Claude Code orchestrates
design, implementation and review, with other models (such as Meta's Muse) as
workers. Commits made with an agent carry its `Co-Authored-By` trailer.

## License

[Apache License 2.0](LICENSE). The Glimmer projector weights are released
separately under the Muse Model License, attached to their release.
