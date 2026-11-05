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

- **The desktop app** (Hyprland and Windows) listens for "Hey Thalvi" and
  types what you dictate into the window you are using.
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
| "paste that" or "finish dictation" | types the text into the window you were in |
| "submit" | types it and presses Enter |
| "do that" | runs a configured action, such as opening a site or focusing an app |
| "scratch that" | drops the last piece |
| "never mind" | drops the whole capture |

Delivered text is also put on the clipboard. An overlay at the top of the
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
  without spaces. To update, replace the file; settings and models stay.

Start it once and it stays in the tray; starting it again opens Settings. The
first start downloads the wake models. If Thalvi cannot start, for example
because its config is broken, a message box names the problem and the file.

### Speech recognition

By default the app streams your speech to the server's `/transcription` at
`ws://127.0.0.1:8080` (Settings → Speech → Server address). To dictate without
the server or a network, choose **On this computer** under Speech. The first
use downloads about 650 MB; after that it works offline. Its text comes mostly
without punctuation. Settings shows whether the model is downloaded and can
download it, or you can point to your own model folder and check it before
saving. The model is NVIDIA Nemotron Speech Streaming under the
[NVIDIA Open Model License](https://www.nvidia.com/en-us/agreements/enterprise-software/nvidia-open-model-license/).

**Second-pass transcription** (Settings → Speech, off by default) sends each
accepted capture again to the endpoint you set there. Turning it off keeps the
endpoint and key.

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

An error stays on the overlay until the next capture, in a short headline.
Settings shows the full text with **Copy** and **Dismiss**. Copy, the tray's
**Copy last error** and `thalvi-desktop copy-error` all copy the same report:
the error, its technical cause, time, version, system and relevant settings,
never API keys.

### Commands and Hyprland

`thalvi-desktop toggle`, `pause`, `resume`, `cancel` and `copy-error` control
the running app (with the AppImage, run it with that word). `status` prints
Waybar JSON. These commands work on Hyprland; Windows uses the tray. The
examples in [`integrations/hyprland/`](integrations/hyprland/) add a Waybar
microphone button (left click toggles listening without moving focus, right
click opens Settings) and key bindings in `hyprland-binds.conf`, such as
`bind = SUPER, E, exec, thalvi-desktop copy-error`. Pausing cancels the
current capture and closes the microphone.

### Hold a key to dictate

Hold a key, speak, and let go: the text is pasted into the window focused when
you pressed, without the pause countdown and without Enter. A hold shorter than
a third of a second, or with no words, pastes nothing and says so. Saying "never
mind" cancels it; any other spoken ending works as after the wake word.

- **Hyprland:** F9, with no modifier. Add the `hold-start` and `hold-end` binds
  from `hyprland-binds.conf`. Hyprland types any modifier you still hold into the
  pasted text and skips the release when a modifier comes up first, so the key
  must be a single key.
- **Windows:** Alt and the key left of X (Z on a QWERTY keyboard, Y on QWERTZ).
  The app watches the keyboard itself and keeps the key from the focused app; the
  hold ends when the key and Alt are both up. Set another as `hold_key` in the
  config, such as `Ctrl+Shift+F9`, named by its place on the keyboard (`Alt+KeyZ`,
  `Digit1`, `Space`, `F9`). While an administrator window is focused, Windows
  shows Thalvi no keys and lets it type nothing, so the key reaches that window
  as usual and no dictation starts.

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

All code in this repository was written during the hackathon; the commit
history is the record. Pull requests land as merge commits, so the individual
commits behind each change stay visible, and `main` is never rewritten.

The idea draws on lessons from an earlier, private voice-assistant prototype by
the same author. Nothing is copied from it or from any other repository;
third-party code enters only as declared dependencies.

AI assistance is used in this project and disclosed: Claude Code orchestrates
design, implementation and review, with other models (such as Meta's Muse) as
workers. Commits made with an agent carry its `Co-Authored-By` trailer.

## License

[Apache License 2.0](LICENSE). The Glimmer projector weights are released
separately under the Muse Model License, attached to their release.
