# Thalvi

Talk to your coding agents instead of typing to them. Say "Hey Thalvi" in any
window, say the whole thing, then decide: paste it, send it, or drop it.

Built with Muse Spark Open.

Typing a good prompt is a chore, so prompts end up short and leave out what the
agent needs. Speaking takes that barrier away, most of all for long prompts. You
can correct yourself as you go and look at something else while you talk, and
the words still land in the window you started from. The agent workflow is
tested with Muse Code on Hyprland.

The same voice helps whenever typing isn't an option:

- **Cooking:** pick a recipe together and get walked through it step by step.
- **On the road:** a briefing on the topics you care about, then follow-up
  questions in plain conversation.
- **Messages on the go:** dictate a reply and confirm it out loud before
  anything is sent.

Thalvi has two parts:

- **The desktop app** (Hyprland, Windows, and GNOME Wayland with dictation
  copied to the clipboard) listens for "Hey Thalvi" and pastes what you dictate
  into the window you are using.
- **The server and its web client** hold the spoken conversation. Open it in a
  browser, on your phone for example, and talk over it to interrupt.

**Download the desktop app:** the
[latest release](https://github.com/NotPellew/Thalvi/releases/latest) has the
Windows installer and the Linux AppImage; see [Install](#install).

It is built to self-host. By default it uses Meta's Muse Voice Transcribe for
live speech and Muse Spark for reasoning and tool use, and speaks with a voice
engine on your own machine. Each model is an endpoint in `.env`, so any
compatible server can replace it, such as Ollama or vLLM. Camera input (SAM
3.1) is not built yet.

## Server and web client

### Run it

1. Start a speech engine (see [Speech](#speech)). On an NVIDIA GPU with 9 GB
   of free VRAM:

   ```sh
   hf download Qwen/Qwen3-TTS-12Hz-1.7B-Base --local-dir ~/thalvi-tts/models/qwen3-base
   docker compose --profile vllm-omni up -d --wait
   ```

   Without one: `docker compose --profile kokoro up -d --wait`.
2. `cp .env.example .env`, then fill in the API keys and the speech lines for
   the engine you started. `.env.example` describes every setting.
3. `npm ci --prefix web && npm run build --prefix web`, then `cargo run --release`.
4. `tailscale serve --bg 8080` and open the printed HTTPS address on your phone.

There is no authentication: anyone who can reach the server can read and
delete every conversation, so keep it on your tailnet.

### Use it

**Voice** turns the microphone on and off. **Speak replies** chooses whether
replies are spoken; it follows Voice, and you can flip it on its own to type and
listen, or to talk and read. Talking while Thalvi speaks interrupts it. The
**1×** chip sets the playback speed, and the **Voice** chip picks the voice (see
[Voices](#voices)).

An error stays on screen until you dismiss it or it is fixed, and **Copy** puts
its text on the clipboard.

With `LLM_AUDIO_INPUT=on`, the language model hears each spoken turn instead of
reading its transcript: the turn's audio, padded by 300 ms on each side, goes
as an OpenAI `input_audio` WAV part, so the model must accept those. Captions,
the saved conversation and earlier turns still use the transcript, and typed
turns are always text.

For a model that hears speech itself,
[Glimmer projector v3](https://github.com/NotPellew/Thalvi/releases/tag/glimmer-projector-v3)
is an experimental audio projector for Muse Glimmer 30B, with its training code
on the
[`feat/glimmer-adapter`](https://github.com/NotPellew/Thalvi/tree/feat/glimmer-adapter)
branch. It is not part of the app.

### Conversations

Every conversation is saved under `$THALVI_DATA_DIR/sessions/` (default
`data/sessions/`, one JSON file each) until you delete it from **Conversations**.
A reload or a dropped connection rejoins the same conversation. Opening a
conversation that is live on another device shows it read-only; **Continue
here** moves it to this device.

- **Titles** start as your first line. With `BG_LLM_BASE_URL`, `BG_LLM_MODEL`
  and `BG_LLM_API_KEY` set (all three or none), a background model rewrites each
  title in three to six words after the first reply. A title you typed is never
  overwritten.
- **History search:** Thalvi can call the read-only `search_history` tool for
  short, timestamped matches in past conversations. The app shows **Working**
  while it runs; **Task steps** opens the details.
- **Retirement:** with `THALVI_RETIRE_DAYS=90`, conversations inactive for 90
  days are replaced by a summary on startup and then daily. Lasting preferences
  and facts go to `$THALVI_DATA_DIR/learnings.json`, which you can read and
  edit under **Conversations → Learnings**. A failed model call or write leaves
  the transcript untouched. Unset or `off` (the default) changes nothing.

### Speech

`compose.yaml` runs the engine in Docker on 127.0.0.1, with weights and voices
under `$SPEECH_DIR` (default `~/thalvi-tts`). To switch engine, change the
`TTS_*` lines in `.env` and restart Thalvi.

| Engine | First audio | Cost | Needs |
| --- | --- | --- | --- |
| Kokoro | ~0.15 s short sentence, ~1 s long one | CPU, ~1 GB RAM | nothing |
| Qwen3-TTS clone of `af_heart`, vLLM-Omni | ~50 ms | ~8 GB VRAM, ~8 GB RAM, ~2.5 min to start | NVIDIA GPU, Ampere or newer |
| Qwen3-TTS clone of `af_heart`, qwentts.cpp | ~60 ms (RTX 2080), ~4.8× real time | ~4 GB VRAM, ~3 GB RAM, starts in seconds | any NVIDIA GPU, CUDA toolkit to build |

**Kokoro** (`--profile kokoro`) takes any
[Kokoro voice](https://huggingface.co/hexgrad/Kokoro-82M/blob/main/VOICES.md)
as `TTS_VOICE`.

```sh
TTS_API=openai
TTS_BASE_URL=http://127.0.0.1:8880/v1
TTS_MODEL=kokoro
TTS_VOICE=af_heart
```

**Qwen3-TTS on vLLM-Omni** (`--profile vllm-omni`). The first start also runs
Kokoro, which reads the reference line for the `af_heart` clone and then stays
up as the CPU fallback.

```sh
TTS_API=openai
TTS_BASE_URL=http://127.0.0.1:8091/v1
TTS_MODEL=Qwen/Qwen3-TTS-12Hz-1.7B-Base
TTS_VOICE=af_heart
```

To use another voice, register a few seconds of clean speech with its
transcript, then put the name in `TTS_VOICE`:

```sh
curl 127.0.0.1:8091/v1/audio/voices -F audio_sample=@me.wav -F consent=mine \
  -F name=me -F "ref_text=What the clip says"
```

A misspelled voice fails with "does not support task_type='CustomVoice'".
Qwen3's built-in presets (such as `vivian`) need
`Qwen/Qwen3-TTS-12Hz-1.7B-CustomVoice` instead of the Base checkpoint: download
it, then change the model path and `--served-model-name` in `compose.yaml` and
`TTS_MODEL`.

**Qwen3-TTS on older GPUs** (before Ampere, such as an RTX 2080) runs
[qwentts.cpp](https://github.com/ServeurpersoCom/qwentts.cpp) as a systemd user
service, built without root from a pinned
[fork](https://github.com/NotPellew/qwentts.cpp) that adds Thalvi's streaming
route (offered upstream in
[#45](https://github.com/ServeurpersoCom/qwentts.cpp/pull/45)).

```sh
docker compose --profile kokoro up -d --wait   # or set KOKORO_URL to a running Kokoro
speech/qwentts.sh
systemctl --user enable --now thalvi-qwentts
```

It needs `git`, `cmake`, a C++ compiler and `nvcc`. `CUDA_ARCH` targets
another GPU than the detected one (`75` for an RTX 2080); `SPEECH_DIR`, `HOST`
and `PORT` (default `8011`) set where it lives and listens. It has no
authentication, so keep `HOST` at `127.0.0.1`. Rerun the script to resume or
upgrade, then restart the service. It prints the `.env` lines:

```sh
TTS_API=qwen3
TTS_BASE_URL=http://127.0.0.1:8011/v1
TTS_VOICE=af_heart
```

**Speech recognition on your own GPU** replaces Meta's recognizer for
dictation through the server and for conversations. It runs NVIDIA's
[Nemotron unified ASR 0.6B](https://huggingface.co/nvidia/nemotron-labs-unified-asr-0.6b)
(OpenMDW licence) live with about 0.56 s of lookahead, finds the language
itself (English, German, Spanish and 22 more European languages) and
punctuates. On an RTX 2080 it uses about 3 GB of VRAM next to qwentts.cpp
and takes about 75 ms for each 0.16 s of audio. Run it from a checkout on the GPU
machine:

```sh
speech/nemotron-asr.sh
systemctl --user enable --now thalvi-asr
```

It needs `uv`, `curl` and an NVIDIA driver, and downloads about 2.5 GB of
weights plus NeMo. `SPEECH_DIR`, `HOST`, `PORT` (default `8012`) and
`ENDPOINT_MS` (silence that ends a stretch of speech, default 600) set where it
lives, listens and how it cuts. It has no authentication, so keep `HOST` at
`127.0.0.1` or a private address such as the machine's tailnet IP. It prints
the `.env` lines:

```sh
STT_URL=ws://127.0.0.1:8012
STT_MODEL=nemotron-labs-unified-asr-0.6b
STT_API_KEY=
```

With it, Settings → Speech shows a **Word list**: names and terms, one per
line, whose spelling recognition should favour (such as "Cybrex2"). Saving
sends the list to the Thalvi server in use, which keeps it and uses it for
dictation and conversations; a realtime speech server entry gets it at each
connection. Meta's recognizer doesn't support it, so the field is hidden
then. Dictation on this computer always uses the list, so the field shows
whenever speech is recognized there.

### Voices

`TTS_VOICES` lists the voices the app offers, comma-separated; `TTS_VOICE` is
the default and always comes first. The names must be ones the running engine
knows.

```sh
TTS_VOICE=af_heart
TTS_VOICES=af_heart,af_bella,sohee
```

In the app, the **Voice** chip opens the picker. **Preview** speaks a short
phrase on that device without interrupting a reply. **Apply** saves the choice
in that browser only and uses it in the conversation from then on. A saved
voice the server no longer offers falls back to `TTS_VOICE`.

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
so line breaks arrive as line breaks and nothing is sent by them; the text stays on
the clipboard. The shortcut is Ctrl+V, and Ctrl+Shift+V in a known terminal on
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
first start writes the default config, opens Settings on Speech, and downloads
the wake models and, once, the speech model, so the first "Hey
Thalvi" works with no setup. If Thalvi cannot start, for example because its
config is broken, a message box names the problem and the file.

### Speech recognition

Settings → Speech keeps a list of speech servers. A fresh install has one,
**On this computer**, with Nemotron Streaming, so it recognizes without a
server; add a server to use one. **Live dictation** uses exactly one of them, and **Use** switches to another at once; it applies from the next
capture, and a capture already running keeps its recognizer. At the top of
Live dictation, **Next dictation** names the entry (and, on this computer, the
model) that will handle the next dictation and whether it is ready. The entry
in use is marked **In use** and has no **Use** button. IDs and addresses are
under each entry's collapsed **Details**. The kinds are:

- **Realtime speech server:** a recognizer that speaks Meta's realtime ASR
  protocol, such as the self-hosted one below, which the app connects to itself
  (address, model, API key), with no Thalvi server running.
- **Through a Thalvi server:** streams to the server's `/transcription`
  (by default `ws://127.0.0.1:8080`), which uses the recognizer in its `.env`.
- **On this computer:** dictates without the server or a network; the default.
  Use **Show models for** to filter the list of models (each model recognizes the
  languages on its card), then pick a model.

Web conversations always use the server's own `STT_URL`. An existing config
with `stt.engine`, `stt.model`, `stt.secondary` and `server` becomes entries
the first time Thalvi reads it, with nothing to enter again. An API key is shown
as saved and never again; leave it blank to keep it. A server that cannot be
reached is named in the error, and Thalvi never falls back to another one.

For the on-device model, Thalvi downloads a missing model when it starts (once:
about 650 MB, or 1.3 GB for Nemotron Unified's GPU files), and Next dictation
shows the download's percent until it is ready. While it
downloads or if it fails, **Use a speech server instead** opens the form to add
a server, and after a failure **Retry** fetches the model again; Thalvi never
switches recognizers by itself. After that it works offline. A model you pick applies from the next capture, and
a capture already running keeps the model it started with. Settings shows each
model's description, languages and size, whether it is **Selected** and
**Downloaded** or **Not downloaded**, and can download it; its ID, source and
license are under the card's collapsed **Details**. Or you can point to your
own model folder and check it before saving.
Word errors below are on LibriSpeech (120 clips) and on 48 of the developer's
own takes, int8 on the CPU, language detected automatically. The unified model
also made 8.38% on German and 7.09% on Spanish (80 Multilingual LibriSpeech clips
each).

| Drop-down entry (config name) | Model | Languages | LibriSpeech | Developer's takes | Notes |
|---|---|---|---|---|---|
| Nemotron Streaming (`nemotron_160ms`, default) | Nemotron Speech Streaming 0.6B | English | 3.46% | 7.14% | Lightest on the CPU; text appears soonest. Mostly without punctuation. |
| Nemotron Unified (`unified_160ms`) | Nemotron Unified ASR 0.6B | 25 European languages | 3.03% | 6.35% | Punctuation and capitals. Re-reads the last 6 s every 160 ms: about 70 ms of work per step on 4 cores of a Ryzen, so on the CPU it needs a fast CPU. On Linux with a discrete GPU it runs there instead, through WebGPU and an fp16 export (about 30 ms a step on an RTX 5070 Ti, a 1.3 GB download); Settings says which it uses. Until the GPU files are downloaded it runs on the CPU, and Settings offers the download. Text appears about 0.5 s later. Our own export, under OpenMDW-1.1. |
| Parakeet TDT (`parakeet_tdt_v3`) | Parakeet TDT 0.6B v3 | 25 European languages | 2.33%* | 4.76%* | Punctuation and capitals. Light on the CPU: about a seventh of real time on 4 cores, against about one for Unified. Not a streaming model: it re-reads the last 2.6 s every 640 ms, so words may change while you speak, and the text is final about a second after you pause. Under CC BY 4.0. |

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

**Second-pass transcription** (Settings → Speech → Second pass, none ticked by
default) sends each accepted capture again to every ticked OpenAI-style or Meta
transcription API. The first ticked one in list order (move them with the
arrows) that answers replaces the live text, and Thalvi pastes as soon as that
one is known: an earlier entry that fails or disagrees is waited for, a later
one never. The others keep running and are only recorded, in the capture's
`text.txt` and the metrics, for comparing later; one that fails says so in a
notice. Unticking keeps the address and key.

### Microphone

Settings → Microphone has **Listening**, which pauses and resumes right away,
and the saved device. To change the device, pause first, then choose and Save;
listening stays paused until you resume it. **Active microphone** names the
device actually open. If the saved device is missing, Thalvi reports it
instead of using another one.

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
(saved with Save): **Stable** (the default) follows released versions;
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

Settings → App has a switch each for metrics, text and audio recordings, all
off by default; Save applies them between captures. They stay on this computer.
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
| `state` | `{"ok": true, "state": "listening", "paused": false, "copy_error": false, "feeding": false, "overlay": {"state": "hidden"}, "recognizer": {"name": "On this computer", "kind": "on_device", "model": "nemotron_160ms", "status": "ready"}}`. `state` is `paused`, `loading`, `listening`, `keyboard` (listening with wake words off), `capturing`, `pending`, `transcribing`, `delivering` or `failed`; `failed` adds `"failure": {"headline", "reason"}` (`reason` is left out for a delivery failure, whose notice can hold the dictated text), and `awaiting_confirmation: true` marks a submit that waits for you. `overlay.state` is what the overlay shows: `hidden` (also when the overlay is off), `listening`, `quiet`, `countdown`, `writing`, `working`, `delivered`, `error` (with the `headline` shown) or `loading`. `recognizer` is the speech entry that handles the next dictation (`model` only for a catalog model) and whether it is `ready`, `loading`, `failed` or `paused`, as the Speech page says. Nothing that was said is in it. |
| `feed <wav-file>` | Plays the file's audio in place of the microphone, at the pace of speech, through the same wake word, recognition, endings and delivery as spoken audio; the microphone is ignored while it plays. Replies when the file has been fed: `{"ok": true, "took_effect": true, "state": "…"}`, with the state then; a capture it started carries on. Refused with `"Thalvi is paused."`, `"A capture is running."`, or the reason for a file that is missing, unreadable, not a mono or stereo 16-bit PCM WAV, or longer than the capture limit. Needs an absolute path (the command makes it absolute). `cancel` stops the feed and any capture it started. Capture metrics record `"source": "file"`. |
| `models` | `{"ok": true, "models": [{"kind": "speech", "name", "title", "size", "downloaded", "progress_percent", "runs_on"}, …]}`: the wake and speech catalog models, `progress_percent` only while one downloads, `runs_on` (the Speech page's hardware line) only for speech models. |
| `model-download <wake\|speech> <name>` | Downloads the model; replies when it is ready, `{"ok": true, "downloaded": true}`, or has failed, `{"ok": false, "downloaded": false, "error": "…"}`. `models` shows the progress meanwhile. |
| `speech-check '<json>'` | Checks one speech server entry exactly as Settings does before saving it: `{"ok": true, "ready": true}`, or `{"ok": false, "ready": false, "error": "…"}`. Saves nothing; keys are never repeated. |
| `word-list-push` | Gives the active Thalvi server the stored word list, as Settings does: `{"ok": true, "pushed": true}`; `"pushed": false` with a message when the active speech entry is not a Thalvi server. |
| `show [page]` | Opens Settings, at `speech`, `microphone`, `capture`, `feedback`, `tasks`, `history` or `app` when one is named: `{"ok": true, "took_effect": true}`. |
| `quit` | Quits as the tray's Quit does: `{"ok": true, "took_effect": true}`. |
| `settings get` | `{"ok": true, "settings": {…}}`, every setting Settings offers plus `start_at_login` and `show_in_menu`. Speech server keys appear only as `"has_key": true/false`. |
| `settings check '<json>'` | `{"ok": true, "valid": true}`, or `{"ok": false, "valid": false, "error": "…"}` with the message Settings shows. Saves nothing. |
| `settings set '<json>'` | `{"ok": true, "changed": true, "applies": "now"}`, or `"next_capture"` while a capture runs. A refusal is `{"ok": false, "error": "…", "changed": false}` and changes nothing. A word list the Thalvi server could not take comes back as `"warning"`. |
| `pause`, `resume`, `toggle`, `cancel`, `hold-start`, `hold-end`, `write-start`, `write-end`, `copy-error` | `{"ok": true, "took_effect": true, "state": "…"}`, or `{"ok": false, "took_effect": false, "error": "…", "state": "…"}`, such as when Thalvi is already paused. `hold-start` starts a dictation that `hold-end` or `cancel` ends; `write-start` and `write-end` do the same with writing on release. |
| `metrics summary` | `{"ok": true, "enabled": true, "window_hours": 24, "stages": {"connect", "partial_lag", "final", "delivery"}, "captures": {"ok": 3, …}}`; each stage has `count`, `outcomes`, `p50_ms` and `p95_ms`. `"enabled": false` and a message when metrics are off. |

The change given to `settings check` and `settings set` is a JSON merge patch
over what `settings get` returns: objects merge, `null` removes a key, and a
list such as `stt.entries` or `stt.phrases` is replaced whole, so read it first
and send it back with your change. Give `-` instead of the JSON to read it from
standard input. A speech server entry takes `api_key` to set a key, `""` to
clear it, and keeps the saved key when `api_key` is absent. The microphone
changes only while Thalvi is paused.

A confirmation stays with you: no command answers one, and a submit that waits
for it shows as `awaiting_confirmation` in `state`. Any other failure of a
command is `{"ok": false, "error": "…"}`; a command with no running app says so
and exits 1, and one that is not a command exits 2.

### Wake words off

Settings → Capture → **Wake words** is a switch, on by default. Off, Thalvi keeps
listening for the hold keys and the microphone stays open, but no wake word starts
anything, plays a cue or delivers, and the wake model scores nothing. It applies at
once, without Save or a restart, survives restarts and pause and resume, and is
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

Choose the keys under **Settings → Capture → Hold keys**: **Change…** records the
next combination you press in the Settings window (Escape cancels), and **Restore
default** goes back to the desktop's own key. A key applies at once, with no Save.
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

The model is any OpenAI-compatible chat completions endpoint (Glimmer, Baseten, a
llama.cpp server, Meta), set under **Settings → Speech → Writing** as `writing`
in the config: `url`, `model` and `api_key`. The model gets the **Writing prompt**
(`prompt`) as its instructions and the recognized words as the message: no window,
app or other context. The default prompt follows what you ask for (an email, a tone,
a wording), invents nothing and keeps line breaks to where the text needs them, such as an
email; edit it in Settings or restore the default there.
How closely a model follows it depends on the model. Saving never calls the model.
If it is not set up, fails, answers with nothing, or takes longer than 15 seconds, nothing is pasted,
Thalvi says what happened, and your own words go on the clipboard to paste with
Ctrl+V.

A model that reasons before it answers, such as Qwen3, shows "Thinking…" until its
text starts; what it reasons is never shown or pasted. The 15 seconds include the
thinking, and it often takes longer, so turn thinking off on the model server: for a
llama.cpp server, start `llama-server` with `--reasoning-budget 0`. Thalvi does not
ask the model itself, because some endpoints refuse a request with a setting they
don't know.

**Opening phrases** (`writing.openings`, under the Writing prompt in Settings) give
a kind of text its own prompt, such as "write me an email" or "write a message".
When the words you said begin with a phrase, whole words only and ignoring case and
punctuation, that rule's prompt is used instead of the Writing prompt, and the
longest matching phrase wins. The words are sent as said, opening included. No
match uses the Writing prompt. Rules apply only when you write out with "write that
out" or the write key. A new rule starts as a copy of the Writing prompt, and a
rule edited in Settings applies from the next capture.

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

## Measure a setup

`cargo run --release --bin measure -- --setup data/measure/a.env --setup data/measure/b.env`
times and grades the same prompts on each setup and prints the numbers side by
side. A setup is your `.env` plus an override file whose lines win over it.
Keep override files under `data/` (git-ignored), since they may hold keys:

```sh
# data/measure/spark12.env
LLM_MODEL=muse-spark-1.2
LLM_REASONING_EFFORT=low
```

Setups run interleaved, so they share the same network noise, on the prompts
in `crates/thalvi-server/measure/cases.json`. Flags: `--setup <file>` (repeat;
none measures the plain `.env`), `--runs <n>` (default 3), `--cases <file>`,
`--only <id>` (repeat) and `--no-judge`.

Replies run through Thalvi's own chat, sentence and speech code. The table
gives median (min-max) seconds to the first token, first sentence, first and
last audio byte, plus failures (an error or over 120 s) and the judge's mean
score. The wait a user feels adds endpointing (about 0.5-0.75 s). Every run is
saved to `data/measure/run-<time>.json`. The judge scores each answer 1 to 5
(correct, fitting length, speakable) with any OpenAI-compatible model set in
`JUDGE_BASE_URL`, `JUDGE_MODEL` and `JUDGE_API_KEY`; without them quality is
"not judged".

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
