# DECISIONS.md

Decisions in force, grouped by area. Each area stands alone; read only the
areas your change touches. Editing rules are in
[AGENTS.md](AGENTS.md#docs).

## Architecture

- **Rust server with desktop and web clients.** Rust owns sessions and model
  calls; the Python desktop client owns native audio, wake words and window
  delivery; TypeScript owns browser media and UI. Why: use native wake/audio
  libraries without moving model orchestration. Rejected: a WASM UI.
- **Desktop demo first; phone conversation.** The server runs on the user's PC.
  Hyprland is the first native target; GNOME, macOS and Windows need prototypes.
  Why: hands-free desktop work is the short-term demo. Rejected for now:
  phone-to-PC dictation and claiming untested platform parity.
- **Resident desktop UI.** System GTK 3 owns the tray and status panel; the
  pinned Python environment owns wake/audio work. Why: keep platform GTK
  bindings separate from model dependencies and leave the input focused.
  Rejected: using a dedicated test editor as the desktop interface.
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
- **Opt-in recordings.** `THALVI_RECORD` turns on metrics, turn and reply text,
  and mic audio, each separately, written locally off the reply path. Why: to judge
  product, model and endpoint changes on real use. Off by default: they are
  personal data.
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
- **Capture starts at a local cue.** Buffer fresh post-cue speech during the
  eight-second recognizer startup budget; explicit spoken confirmations wait
  for ready plus their own cue. Why: keep dictation and confirmations fresh.
  Rejected: transcribing pre-cue backlog or waiting on the network to speak.
- **Silence commits capture.** Paste, submit and tasks wait 2.5 seconds of
  speech silence; continued speech cancels the pending ending. Submit asks
  for target confirmation during that pause. Why: hands-free control with
  time to interrupt. Rejected: immediate dispatch and a second spoken yes.
- **Musical earcons.** Five two- or three-note cues (wake, provisional ending, cancelled, accepted, error), each switchable under `cues` in the desktop config; the mic drops frames while one plays.
  Why: distinct contours tell capture state apart without looking, and the mic never hears them. Rejected: one beep for every state, and a post-delivery cue.
- **Scripted decisions, typed actions.** The demo matches configured phrases to
  browser and app actions; execution receives a typed action. Why: a later model
  can choose the same actions. Rejected for now: a plugin runtime, speculative
  interfaces and open-ended computer use before the scripted flow works.
- **Live Discord actions are disabled.** Refuse before debugger discovery and
  preserve the message on the clipboard. Why: trusted input can reach an
  existing child editor outside the gate. Rejected: shipping the unsafe path,
  channel webhooks and private REST.
- **Experimental Discord input stays in inert tests.** Retain the CDP/Slate
  fixture and the strict existing-frame failure as evidence. Why: passing
  main-document tests does not establish frame safety. Rejected: calling the
  adapter qualified or expanding target coordination before the browser demo.
- **Discord submissions stay unconfirmed.** A key acknowledgement or optimistic
  UI is not a delivery receipt. Why: submission and delivery are different
  events. Rejected: reporting "sent" without a defined receipt.
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
- **Commit checks run on `pull_request`.** They need only read access, and
  changes to them take effect in the PR that makes them. Rejected:
  `pull_request_target`, which runs the base branch's workflow definition.
