# DECISIONS.md

Decisions in force, grouped by area. Each area stands alone; read only the
areas your change touches. Editing rules are in
[AGENTS.md](AGENTS.md#docs).

## Architecture

- **Rust server with desktop and web clients.** Rust owns sessions and model
  calls; the desktop client owns native audio, wake words and window
  delivery; TypeScript owns browser media and UI. Why: use native wake/audio
  libraries without moving model orchestration. Rejected: a WASM UI.
- **Two products, one repo.** Desktop voice control and the conversational
  assistant are separate tracks, joined only by `/transcription` and
  `/session`. Why: different platforms, thin boundary. Rejected for now:
  phone-to-PC dictation; separate repos until desktop needs its own releases.
- **Desktop on Hyprland, GNOME Wayland and Windows.** GNOME gets dictation on
  the clipboard only: it exposes neither the focused window nor focus by ID.
  Why: the desktops we can qualify. Rejected: KDE, X11 and macOS until needed,
  and claiming untested platform parity.
- **Rust desktop app with a Tauri UI.** One process runs engine, tray, a top
  overlay and a settings window; the overlay never takes focus (Hyprland window
  rules via the host, a non-activating window on Windows). Why: one TypeScript
  UI, no IPC. Rejected: layer-shell webview (blank), GTK-only overlay, egui.
- **Three looks for the desktop UI.** `look` picks classic (default), holo or gunmetal for the overlay
  and settings window; the overlay hangs from the top of the usable screen area, one 460 × 200 size
  for all looks. Why: the new designs ship as options, today's UI stays; resizing re-centres it on
  Hyprland. Rejected: replacing classic, y = 0 (a bar covers it), a window size per look.
- **Local recognition in the desktop app.** `stt.engine: local` runs Nemotron Speech
  Streaming 0.6B (int8 ONNX) on `ort` behind the recognizer interface. Why: offline; 6.0% word
  errors on 57 clips, 0.8 s p95 partial lag on 4 cores. Rejected: Moonshine (7.4%, p95 2.6 s on
  the Windows PC), Zipformer (28%), hosting it behind the server.
- **Self-hosted; parts swap by endpoint.** Config sets each part's address on a
  standard protocol (Chat Completions, OpenAI speech, MCP); Meta's realtime ASR
  and Qwen3-TTS's framed PCM stream are the custom adapters. Why: any provider
  or local server plugs in without code. Rejected: a plugin system (no faster,
  much more code).
- **Chat Completions first.** Self-hosted servers speak it. Why: one adapter
  covers Meta, Ollama and vLLM. Revisit: add Responses for Meta if its search
  grounding beats an MCP search tool on the road briefing.
- **Qwen3-TTS clone of af_heart; Kokoro on CPU.** vLLM-Omni serves it, qwentts.cpp
  on GPUs vLLM-Omni can't use. Why: the developer preferred its sound, and first
  audio takes ~50 ms where Kokoro synthesizes a whole sentence first. Rejected:
  Chatterbox (AGPL), Kyutai (no cloning), Orpheus and qwen3-tts-server (slower).
- **Opt-in recordings.** `THALVI_RECORD` turns on metrics, turn, dictation and reply
  text, and mic audio (with ~2 s before each wake), each separately, written locally
  off the reply path. Why: to judge product, model and endpoint changes, misrecognitions
  and false wakes on real use. Off by default: they are personal data.
- **Hands-free turns.** The recognizer runs in endpointing mode, and the user
  interrupts a reply by speaking. Why: hands and eyes are busy. Rejected:
  push-to-talk.
- **Two wake entries, endings select the capture route.** One local inference pipeline
  runs separate trained capture and conversation wake heads. Why: defer the action
  choice until an ending without delaying conversation. Rejected: one phrase
  for both flows.
- **Wake context.** Both trained heads use 16 embedding frames. Why: this
  supports first-frame inference with upstream startup and reset history.
  Rejected: accepting longer contexts that need additional warm-up history.
- **ONNX Runtime from ort's prebuilt static binaries.** The build downloads a pinned
  ONNX Runtime and links it in, the same on Linux and Windows. Why: one way to
  provide it, nothing to ship or find at run time. Rejected: loading a system
  or bundled dynamic library (a version and a file per OS).
- **Wake heads chosen by name or path, downloaded pinned.** Each head is a catalog
  name or a local `.onnx` path with its own threshold; catalog files download on first
  use into the user data directory and must match their SHA-256. Why: users change a
  phrase without a rebuild, and we never re-host CC BY-NC-SA heads. Rejected: bundled models.
- **Capture starts after the wake cue.** Dictation plays the wake cue at once and sends
  only later speech, buffered while the recognizer connects; confirmations wait for ready,
  then the cue. Why: wake audio leaked the phrase into 1 in 5 live transcripts even with
  stripping. Rejected: pre-wake audio, cutting it at the wake, and stripping the phrase.
- **Silence commits capture.** Paste waits 0.7 s of speech silence, submit and tasks 2.5 s by
  default, each settable from 0 to 5 s; recognized speech cancels the pending ending, local speech
  pauses the countdown, and submit confirms its target meanwhile. Why: paste is undoable and should
  feel fast; submit and tasks reach others. Rejected: one fixed pause for all, a second spoken yes.
- **Check, then type.** Delivery re-verifies the bound window just before injecting
  input, which goes to whatever window is focused; a switch in those milliseconds
  can misdirect it. Why: neither Hyprland nor Windows can address input to one window.
  Rejected: per-desktop accessibility or input protocols (much code for a rare race).
- **Musical earcons.** Five two- or three-note cues (wake, provisional ending, cancelled, accepted, error), each switchable under `cues` in the desktop config; the mic drops frames while a cue plays.
  Why: distinct contours tell capture state apart without looking, and the mic never hears them. Rejected: one beep for every state, and a post-delivery cue.
- **Scripted decisions, typed actions.** The demo matches configured phrases to
  browser and app actions; execution receives a typed action. Why: a later model
  can choose the same actions. Rejected for now: a plugin runtime, speculative
  interfaces and open-ended computer use before the scripted flow works.
- **Live Discord actions are disabled.** `discord_message` is refused before any host
  call and the message stays on the clipboard. Why: trusted input can reach an existing
  child editor outside the gate, and a key acknowledgement is no delivery receipt.
  Rejected: shipping the unsafe path, channel webhooks and private REST.
- **Spoken acknowledgement.** With `THALVI_ACK=on`, a slow reply first plays a
  canned line (rotated, synthesized once and cached per session) when no first
  sentence arrives within 500 ms. Why: it covers the model's thinking gap with
  no extra model call, and recordings report its first audio separately from
  the real answer's. Rejected: a small model writing the line (adds the
  latency it should hide), and on by default (no off baseline to compare).
- **Measure setups with an LLM judge on a fixed prompt set.** `measure` times
  setups interleaved, then a judge model grades answers 1 to 5 against a rubric.
  Why: setups compare in minutes, with numbers, repeatably. Rejected: rating by
  hand (slow), and a router or fast-model role before the numbers ask for one.

- **Audio input is the current turn only.** With `LLM_AUDIO_INPUT=on`, a
  spoken turn reaches the model as its audio alone, with no transcript beside
  it, while earlier turns stay text. Why: the model was trained on audio only,
  and old clips would grow every request. Rejected: audio plus transcript
  (off-distribution), audio for the whole history.
- **Heard guard.** For audio-model echoes, reject empty captions or >50% word
  disagreement, replace the answer with "Sorry, I didn't catch that.", and omit
  the turn from history. Allow matching one-word replies. Why: avoid invented
  requests without blocking short answers. Rejected: a two-word minimum.
- **Keyword history search.** `search_history` searches the current and past
  sessions and returns short excerpts as data; task steps stay collapsed. Why:
  recover earlier details without whole-history context. Rejected: embeddings
  and code mode before a keyword tool proves useful.
- **Opt-in retirement with editable learnings.** `THALVI_RETIRE_DAYS` replaces
  old transcripts with summaries and keeps durable facts in a separate editable
  store. Why: bound retention without losing useful context. Rejected:
  automatic deletion by default or hiding learnings inside summaries.
- **Per-device voices on the configured engine.** `TTS_VOICES` offers names;
  browsers preview and save their own choice. Why: simple personalization
  without shared-device surprises. Rejected: runtime engine switching and a
  server-wide voice preference.

## Repository

- **Merge commits; only `main` is fixed.** PRs land as merge commits, and PR
  branches may be rewritten before merge. Why: the commits show the work being
  iterated, which is enough; no full audit trail. Rejected: squash merge (hides
  the iteration), a force-push block on all branches (more friction).
- **No review gate.** The `main` ruleset requires PRs and passing checks but
  no approvals. Why: Thalvi has one author, and GitHub does not let authors
  approve their own PRs. Revisit if a second contributor joins.
- **Issues are optional.** Why: in a 9-day solo build, PRs and the commit
  history are record enough. Rejected: an issue for every change.
- **`unsafe` only for Win32.** The workspace denies `unsafe`; only the Windows
  host module opts out, each block with `// SAFETY:`. Why: typing and window
  checks need raw Win32 calls. Rejected: `forbid` with wrapper crates (none
  covers the checks), clipboard-only Windows, `unsafe` allowed everywhere.
- **Commit checks run on `pull_request`.** They need only read access, and
  changes to them take effect in the PR that makes them. Rejected:
  `pull_request_target`, which runs the base branch's workflow definition.
