# DECISIONS.md

Decisions in force, grouped by area. Each area stands alone; read only the
areas your change touches. Editing rules are in
[AGENTS.md](AGENTS.md#docs).

## Architecture

- **Rust server, TypeScript web client.** The Rust server owns sessions, model
  calls, and tools; the TypeScript client owns microphone, camera, playback,
  and UI. Why: browser media APIs are first-class in TypeScript and awkward
  through `web-sys`. Rejected: full Rust with a WASM UI (Leptos, Dioxus).
- **Phone browser first.** The client is a mobile web app served over HTTPS,
  which mobile browsers require for microphone and camera access. Why: no
  app-store builds or signing in a 9-day hackathon. Rejected for now: a
  native app; revisit once the web client works.
- **Self-hosted; parts swap by endpoint.** Config sets each part's address on a
  standard protocol (Chat Completions, OpenAI speech, MCP); Meta's realtime ASR
  is the one custom adapter. Why: any provider or local server plugs in without
  code. Rejected: a plugin system (no faster, much more code).
- **Chat Completions first.** Self-hosted servers speak it. Why: one adapter
  covers Meta, Ollama and vLLM. Revisit: add Responses for Meta if its search
  grounding beats an MCP search tool on the road briefing.
- **Kokoro for local speech.** Kokoro-FastAPI streams 24 kHz PCM over the
  OpenAI speech API. Why: Meta has no TTS; Kokoro runs 12x real time on CPU. Revisit
  for a more expressive voice.
- **Hands-free turns.** The recognizer runs in endpointing mode, and the user
  interrupts a reply by speaking. Why: hands and eyes are busy. Rejected:
  push-to-talk.

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
