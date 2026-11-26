# Thalvi

Speak instead of typing, in any window. Say **"Hey Thalvi"** (or hold a key), say what you mean, and let Thalvi paste, format, or send it.

Typing everything out is slow and breaks flow—whether you are writing messages, emails, notes, or prompts for coding agents. Speaking lets you get the whole thought out naturally, correct yourself as you go, and look at something else while you talk; your words still land in the window you started from.

Thalvi is a local-first desktop app for **Hyprland**, **Windows**, and **GNOME Wayland** (clipboard-only on GNOME). Speech recognition runs on your computer by default with no account or cloud required, and every part—wake words, speech recognition, and optional AI writing—can also be swapped or extended in Settings.

> Spoken conversation (with a separate server and web client) lived here up to [`conversation-final`](https://github.com/NotPellew/Thalvi/tree/conversation-final).

## How it works

Start a capture by saying **"Hey Thalvi"** or holding a key (**F9** on Hyprland, **Alt+Z** on Windows). A small non-focus-stealing overlay at the top of the screen shows live progress; the tray icon lets you pause, cancel, recover the **Last dictation…**, copy the last error, or open Settings.

### Spoken endings (wake-word mode)

| Say | What Thalvi does |
| --- | --- |
| `"paste that"` or `"finish dictation"` | Pastes the text into the window you started from |
| `"write that out"` | Rewrites the text via the chat model, then pastes it |
| `"submit"` | Pastes the text and presses **Enter** (after a pause to review) |
| `"do that"` | Runs a configured action (open a URL or focus an app) |
| `"scratch that"` | Drops the last piece |
| `"never mind"` | Cancels the whole capture |

### Hold keys (push-to-talk)

Hold **F9** (Hyprland) or **Alt+Z** (Windows) to dictate and release to paste immediately; hold **F10** / **Alt+X** to run the writing model on release. Both keys are customizable in **Settings → Listening** (including **AltGr** alone on Windows). Wake words can also be switched off for hold-key-only use.

### Terminal safety & platform support

Thalvi pastes via the clipboard (**Ctrl+V**, or **Ctrl+Shift+V** in known Linux terminals), keeping line breaks intact. In terminals, unconfirmed multi-line text stays on the clipboard so nothing executes unread; `"submit"` pastes after its confirmation pause (with a 250 ms delay before Enter on Hyprland).

| Desktop | Support |
| --- | --- |
| Hyprland | Full (auto-binds hold keys; targets window by PID) |
| Windows | Full (global hook for hold keys; `AltGr` supported) |
| GNOME Wayland | Clipboard only (wake word works; no hold keys or app styles; requires XWayland) |

## Install

Download the latest release from [Releases](https://github.com/NotPellew/Thalvi/releases/latest) (no Git, Rust, or Node needed):

- **Windows:** run `Thalvi_<version>_x64-setup.exe` (installs per-user into `%LOCALAPPDATA%\Programs\Thalvi` and handles WebView2).
- **Linux:** install `wtype` and `wl-copy`, make `Thalvi_<version>_amd64.AppImage` executable (`chmod +x`), and run it from a path without spaces.

On first launch, Thalvi writes default settings, opens Settings, and downloads the wake model and default speech model once. Built-in auto-updates follow **Stable** or **Development** builds (**Settings → App → Updates**).

## Features

### Speech recognition (**Settings → Recognition**)

- **On this computer (default):** runs offline via ONNX Runtime (`Nemotron Streaming` English default; `Nemotron Unified` 25 European languages with optional WebGPU on Linux; `Parakeet TDT` 25 European languages) or an app-managed `whisper.cpp` helper (`Whisper Turbo` CPU or Vulkan GPU, experimental), or your own ONNX model folder.
- **Streaming & after-you-stop servers:** connect a WebSocket streaming server (such as the self-hosted recognizer below) or any OpenAI-compatible `/v1/audio/transcriptions` or audio chat endpoint.
- **Final pass:** select one recognizer other than the live one. It re-transcribes the finished audio and replaces the live text when it answers within two seconds and agrees with the spoken ending. A slow, failed, or mismatched pass keeps the live text and shows a notice. Tasks, empty captures, and captures that used “scratch that” skip the final pass. Cancelled captures deliver nothing.
- **Extra recognizers & Word list:** enable extra recognizers to merge their text through a chat model, or switch **Merge with the chat model** off to paste the live text as soon as you stop and only record what each extra heard. Either recognizer kind can be the final pass, which needs no chat model. While a final pass is selected, the extras and merge pause and keep their settings. The word list sends names and terms as a hint to on-device models that support it, Whisper Turbo, realtime servers that support it, OpenAI-compatible and Meta servers (whether active, extra, or final pass), and the chat-model merge. Settings tells you when the active recognizer can't use it.

### Writing & cleanup (**Settings → Chat model**)

- **On this computer:** download **Qwen3 4B** (`Q4_K_M`, ~2.5 GB, Apache-2.0) and a pinned `llama.cpp` runtime (Vulkan GPU or CPU) directly from Settings—no server to manage.
- **Manual endpoint:** point at any OpenAI-compatible chat completions API (`llama.cpp`, Ollama, vLLM, etc.).
- **Prompts & cleanup:** customize the **Writing prompt**, **Opening phrases** (e.g., `"write me an email"`), **App styles** (per-app/window-title prompts on Hyprland and Windows), and opt-in **Clean up dictation** (removes filler words and fixes punctuation on normal pastes).
- **Write numbers as digits.** Thalvi puts spoken English and German numbers in digits by default in final dictation. “I have fifteen open issues” becomes “I have 15 open issues” and “zwei Komma fünf” becomes “2,5”. Expressions like “the other one” keep their words. The rules work offline and independently of cleanup. The switch is under **Settings → Chat model → Write numbers as digits**.

### Wake words, looks, history & privacy

- **Wake words** (**Settings → Listening**): choose **Hey Thalvi**, **Hey Mycroft**, **Hey Jarvis**, or a custom `.onnx` model, tune thresholds and capture limits, or turn wake words off.
- **Overlay & sounds** (**Settings → Overlay & sounds**): toggle the overlay pill, live draft preview, five audio cues, and pick between **Gunmetal** (default), **Holo**, and **Neon** looks (customizable under `themes` in `desktop.json`).
- **History & recordings** (**Settings → History & recordings**): **Last dictation** keeps the latest capture in memory (even if cancelled or failed) to edit and copy. Local **History** is on by default (last 200 entries, configurable 1–1000). Metrics, text, and audio recordings are all **off by default** and stay strictly on your computer (`~/.local/share/thalvi` on Linux, `%LOCALAPPDATA%\thalvi` on Windows).
- **Config:** stored in `desktop.json` under `~/.config/thalvi/` (Linux) or `%APPDATA%\thalvi\` (Windows).

## Self-hosted GPU recognizer (optional)

`speech/nemotron-asr.sh` installs a WebSocket streaming recognizer running NVIDIA's [Nemotron unified ASR 0.6B](https://huggingface.co/nvidia/nemotron-labs-unified-asr-0.6b) (25 European languages, punctuation, ~3 GB VRAM) on a Linux machine with an NVIDIA GPU and `uv`:

```sh
speech/nemotron-asr.sh
systemctl --user enable --now thalvi-asr
```

Add it under **Settings → Recognition → Streaming servers (+)** with address `ws://127.0.0.1:8012` (or your machine's tailnet IP; keep `HOST` private as it has no auth) and any model name.

## CLI, Hyprland & agent control

`thalvi-desktop` talks to the running app over a private local socket/pipe (commands never start the app or return dictated text) and exits `0` on `"ok": true`:

- **Listening & capture:** `toggle`, `pause`, `resume`, `cancel`, `hold-start`, `hold-end`, `write-start`, `write-end`, `copy-error`
- **Status & UI:** `status` (Waybar JSON), `state` (JSON phase/overlay/recognizer), `watch [--json]`, `show [page]`, `quit`
- **Setup & testing:** `models`, `model-download <wake|speech|chat> <name>`, `speech-check '<json>'`, `settings get`, `settings check '<json>'`, `settings set '<json>'`, `metrics summary`, `feed <wav-file>`

See [`integrations/hyprland/`](integrations/hyprland/) for Waybar and `hyprland.conf` binding examples.

## Build from source

Requires Rust (`rust-toolchain.toml`), Node (`.node-version`), plus:
- **Linux:** `libasound2-dev libwebkit2gtk-4.1-dev libayatana-appindicator3-dev` to build; `wtype` and `wl-copy` to run.
- **Windows:** MSVC build tools.

```sh
npm ci --prefix crates/thalvi-app/ui && npm run build --prefix crates/thalvi-app/ui
cargo run --release -p thalvi-app
```

Releases are built on `desktop-vX.Y.Z` tags on `main` (and `X.Y.Z-dev.N` prereleases on pushes to `main`), signed in the `release` environment for the Tauri updater (`plugins.updater.pubkey` in `crates/thalvi-app/tauri.conf.json`).

## Provenance & License

Thalvi began during the [Meta Global AI Developer Hackathon](https://cerebralvalley.ai/e/meta-ai-developer-hackathon) (October 26 – November 4, 2026); code up to [`hackathon-submission`](https://github.com/NotPellew/Thalvi/tree/hackathon-submission) was written during the hackathon, and spoken conversation lived here up to [`conversation-final`](https://github.com/NotPellew/Thalvi/tree/conversation-final). Later work continues in the same open history with merge commits on `main`, and commits made with coding agents carry `Co-Authored-By` trailers.

Licensed under [Apache License 2.0](LICENSE). Speech, wake, and chat models downloaded by the app keep their own licenses, shown beside each model in Settings.

