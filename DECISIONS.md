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
- **Rust desktop app with a Tauri UI.** One process runs engine, tray, a top overlay and a settings window; the overlay never takes
  focus (Hyprland window rules via the host, a non-activating window on Windows, an XWayland window on GNOME, where the app runs on GTK's X11 backend).
  Why: one TypeScript UI, no IPC; on GNOME's Mutter a Wayland window and `wl-copy` each take focus, an X11 window and clipboard do not (measured).
  Rejected: layer-shell webview (blank), GTK-only overlay, egui, native Wayland GTK on GNOME.
- **Three looks for the desktop UI.** `look` picks gunmetal (default), holo or neon for the overlay
  and settings window; the overlay hangs from the top of the usable screen area, one 460 × 200 size
  for all looks. Why: the developer chose gunmetal, the others stay as options; resizing re-centres it
  on Hyprland. Rejected: dropping the neon pill, y = 0 (a bar covers it), a window size per look.
- **Local recognition in the desktop app.** `stt.engine: local` runs a catalog model on `ort`, picked in Settings from a Language drop-down (filters only, not saved) and a Model drop-down: Nemotron Streaming (0.6B, 160 ms, default), Nemotron Unified (0.6B, buffered) or Parakeet TDT v3 (0.6B, offline, re-read every 640 ms). Why: offline; a model is listed for a real trade-off, not only for the fewest errors; word errors on
  LibriSpeech and the developer's takes 3.46/7.14% and 3.03/6.35%, Unified also 8.38% German, 7.09% Spanish; Parakeet about 0.15× real time against Unified's 1×, at 2.33/4.76% (Unified 2.25/4.76% on those clips), 12.44% German, 7.95% Spanish. Rejected: Moonshine (7.4%), Zipformer
  (28%), Nemotron 3.5 (15%), the 80 and 560 ms exports (no real choice; 3.62 and 3.46% on LibriSpeech).
- **Self-hosted recognition speaks Meta's ASR protocol.** `speech/asr` serves Nemotron unified ASR
  0.6B (NeMo, buffered, 0.56 s lookahead, language detected) behind `STT_URL`. Why: no server change;
  5.8–6.6% word errors on the developer's takes (Meta-free, en/de/es), fits next to TTS on an 8 GB
  RTX 2080. Rejected: Voxtral 4B Realtime (too big), Nemotron 3.5 streaming (15%), Qwen3-ASR (2 s chunks).
- **The desktop connects to its recognizer itself, from a list of speech servers.** `stt.entries` holds realtime
  ASR, Thalvi server, on-device and second-pass entries; one live entry is active, ticked second passes run in list
  order and the first that answers wins; `STT_URL` stays for web conversations. Why: switch recognizers with one click,
  no `.env` edit or restart. Rejected: one setting per recognizer, a silent fallback between entries, several live entries (#228).
- **The self-hosted recognizer computes in fp32.** Why: in fp16 its encoder overflows to NaN, decoded as `⁇`,
  when speech follows seconds of digital silence (#220); fp32 takes 1.25 GB more VRAM (7.7 of 8 GB beside TTS)
  and about 2 ms more per step on the RTX 2080. Rejected: bfloat16 (Turing has none), filling silence with
  noise (rescued one stretch of two), restarting streams.
- **The word list is a recognition hint, used locally and on the server.** Settings' list goes to the
  server (when a Thalvi server entry is active) and to a direct realtime recognizer at each connection, to
  recognizers that announce `phraseBoosting`, and to local dictation. Why: NeMo boosting fixed "Cybrex2" with no new errors; text replacement can't fix
  what was misheard. Rejected: a flat logit bonus (no fix, stray tokens in silence), sending it to Meta.
- **Local boosting fuses NeMo's phrase tree into the greedy search at alpha 2.** Why: "Cybrex2", digit
  and all, in 8 of 8 dictations, no new errors on 48 takes with a two-entry list, about 1 ms more per
  step. Rejected: a beam of 4 (0 of 8 at alpha 1, twice the step time), NeMo's alpha 1 (0 of 8).
- **Self-hosted; parts swap by endpoint.** Config sets each part's address on a
  standard protocol (Chat Completions, OpenAI speech, MCP); Meta's realtime ASR
  and Qwen3-TTS's framed PCM stream are the custom adapters. Why: any provider
  or local server plugs in without code. Rejected: a plugin system (no faster,
  much more code).
- **Same-site browser requests only.** The server refuses a request whose `Origin` names another
  host than its `Host`; native clients send none. Why: browsers skip cross-origin checks on
  WebSockets, so any page could open `/session`. Rejected for now: a token (no accounts), a host
  allowlist against DNS rebinding (a new setting).
- **Chat Completions first.** Self-hosted servers speak it. Why: one adapter
  covers Meta, Ollama and vLLM. Revisit: add Responses for Meta if its search
  grounding beats an MCP search tool on the road briefing.
- **Qwen3-TTS clone of af_heart; Kokoro on CPU.** vLLM-Omni serves it, qwentts.cpp
  on GPUs vLLM-Omni can't use. Why: the developer preferred its sound, and first
  audio takes ~50 ms where Kokoro synthesizes a whole sentence first. Rejected:
  Chatterbox (AGPL), Kyutai (no cloning), Orpheus and qwen3-tts-server (slower).
- **Opt-in recordings.** Metrics, turn, dictation and reply text, and mic audio (with ~2 s
  before each wake) are each written locally off the reply path: on the server by
  `THALVI_RECORD`, on the desktop by settings switches, applied between captures. Why: to judge
  changes and misrecognitions on real use. Off by default: they are personal data.
- **Dictation history is on by default.** `history` keeps the text, time, target and outcome of the last 200
  captures (1 to 1000) in one file in the per-user data directory; switching it off stops writes and deletes nothing.
  Why: the user can recover what they dictated, and text alone never leaves the computer. Rejected: opt-in like the
  recordings (audio and metrics stay off), deleting entries when it is switched off.
- **The last dictation is an in-memory draft.** Settings → History holds the latest capture, also when it failed or was cancelled, to
  edit and Copy; a newer capture waits while the draft is edited. Why: fix a dictation without dictating it again, with nothing
  stored beyond the history; Windows refuses to bring the bound window to the front from Settings, so Paste could not work there.
  Rejected: persisting drafts, replacing edits, Paste into the bound window, retrying a partial delivery by itself.
- **Desktop recordings by wake.** `recordings/` holds only recordings: `metrics.jsonl` and a
  `<date>/<time>/` folder per wake (`wake.wav`, `speech.wav`, `text.txt`). Wake metrics: near misses
  (score from 0.15), wakes refused as too quiet, wakes that deliver nothing, one summary a minute
  (scoring latency, seconds with sound and quiet, switches between them). Why: one dictation in one place;
  95% of old lines were per frame. Rejected: day files linked by timestamp, a line per frame
  or per sound/quiet switch.
- **Hands-free turns.** The recognizer runs in endpointing mode, and the user
  interrupts a reply by speaking while mic input is on. Why: hands and eyes are
  busy. Rejected: push-to-talk.
- **Input and reply audio are separate.** `/session` takes `input` (`text` or `mic`) and
  `reply_audio` (on or off); only `mic` opens a recognizer, only reply audio calls speech
  synthesis. Web: Voice sets both, Speak replies only reply audio. Why: typed or locally
  recognized input can get spoken replies. Rejected: one `voice` switch, a compat message.
- **Two wake entries, endings select the capture route.** One local inference pipeline
  runs separate trained capture and conversation wake heads; a wake capture whose whole text is
  an `open_url`/`focus_app` phrase or `open <host>` runs as a task with no ending. Why: defer the
  action choice until an ending, yet a ready command needs none. Rejected: one phrase for both flows.
- **Wake context.** Both trained heads use 16 embedding frames. Why: this
  supports first-frame inference with upstream startup and reset history.
  Rejected: accepting longer contexts that need additional warm-up history.
- **Quiet wake floor.** A wake whose last 1.28 s (the head's 16 frames) peaked below 1,500 as a
  sample magnitude starts nothing and is recorded as `desktop_wake_refused`. Why: heads score speech
  alike at any volume; real wakes peaked at 2,073 or more, faint false wakes at 1,152 or less, RMS never separated.
  Rejected: RMS, a floor per microphone (revisit if a quieter one appears), the last few frames.
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
  pauses the countdown, submit confirms its target, and a whole-text command waits the task pause.
  Why: paste is undoable; submit and tasks reach others. Rejected: one fixed pause, a second spoken yes.
- **A keyword spotter only warns about missed endings.** sherpa-onnx's 3.3M keyword model listens beside the recognizer; when the covering final lacks an ending it heard and the user then stays quiet for that ending's pause, the cancelled cue plays and a notice names the phrase. Nothing is cut, retried or acted on.
  Why: spotted times run 0.2–0.5 s late and finals carry no word times; a recheck by the same recognizer rescued 0 of 12 disputed hits in 62.8 min; the quiet gate keeps false alarms (16 in that time) silent.
  Rejected: cutting at the spotted time, a recheck, acting on the spotter alone, judging on a short timer (24 of 36 finals came late), quoting dictated words.
- **Check, then type.** Delivery re-verifies the bound window just before injecting
  input, which goes to whatever window is focused; a switch in those milliseconds
  can misdirect it. Why: neither Hyprland nor Windows can address input to one window.
  Rejected: per-desktop accessibility or input protocols (much code for a rare race).
- **Enter waits 250 ms.** On Hyprland, submit waits 250 ms between the text and Enter.
  Why: Muse Code 1.4.14 read a faster Enter as a new line while the overlay said sent;
  0 of 15 sent at up to 100 ms, 15 of 15 from 150 ms.
  Rejected: two wtype calls, a delay per application.
- **Musical earcons.** Five two- or three-note cues (wake, provisional ending, cancelled, accepted, error), each switchable under `cues` in the desktop config; the mic drops frames while a cue plays.
  Why: distinct contours tell capture state apart without looking, and the mic never hears them. Rejected: one beep for every state, and a post-delivery cue.
- **Second-pass transcription is explicitly opt-in.** A second-pass entry runs only
  when ticked (`active`), and unticking keeps its settings and key. Why: avoid uploading
  every dictation and adding latency without an explicit choice. Rejected:
  treating an endpoint's presence as enablement.
- **Resident controls and private request socket.** Commands reach the running app over a request/reply
  socket in the private runtime folder (a user-only pipe on Windows): one JSON line each way, size-bounded,
  known commands only. Why: controls must not start a microphone, and an agent needs an answer.
  Rejected: single-instance relaunch (no answer), a keyboard listener, TCP, transcript-bearing replies.
- **Hold to talk.** Holding a key dictates and releasing pastes: F9 without a modifier on Hyprland
  (compositor binds), Alt+KeyZ on Windows (`hold_key`, a swallowing keyboard hook that waits for the
  key and Alt both up); a second key, `write_key` (F10, Alt+KeyX), releases into the writing model instead; every word of a hold is text, and only `cancel` abandons one. Why: Hyprland 0.53.3 types held modifiers into the paste and drops the release when one comes up first, seen live; `RegisterHotKey` sees no release; the key already ends a dictation, and spoken endings cut off sentences that held them.
  Rejected: Alt+Y with binds, Ctrl+Z/Y, spoken endings during a hold.
- **Writing is one direct chat call, words only, paste only.** "write that out" or the write key sends the
  recognized words to an OpenAI-compatible endpoint from Settings and pastes what streams back; no context, no
  Enter, 15 s at most; any failure pastes nothing and puts the words on the clipboard. Why: dictation then needs
  no server, and text the user did not say is never sent unread. Rejected: the Thalvi server, app-aware tone, write-and-send.
- **The writing prompt is a stored override of one built-in default.** `writing.prompt` is the system message; it is
  saved only when it differs from the default, and a blank one is refused where the config loads. Why: users who never
  edited it get a better default later, and Settings, the agent surface and the file share one check. Rejected:
  storing the default text, a prompt without an endpoint, one prompt per opening phrase (#291).
- **Hold release flush.** At release a hold sends 800 ms of generated silence, kept out of the recording, and pastes on the next
  final or after 250 ms. Why: a streaming recognizer emits a word only after audio past it; measured headless on the server
  recognizer, no last word lost in 336 releases (75% before when released on the word's end). Rejected: a fixed 500 ms tail;
  an end-of-stream message through the server (11% lost at the word's end, a protocol change).
- **Packaged desktop with an OS-held login entry.** AppImage on Linux, per-user NSIS on Windows, released from `desktop-vX.Y.Z` tags that match the Cargo version; login startup is the OS entry (`tauri-plugin-autostart`, run with `--login`), and the config keeps the preference (`start_at_login`, written by the switch alone, never by Save) so each start restores an entry an upgrade removed. Hyprland runs no XDG autostart unless `xdg-desktop-autostart.target` is up (uwsm starts it), so otherwise the row shows an `exec-once` line to copy and no entry is written. The AppImage's app menu entry is Thalvi's own file (`show_in_menu`, same pattern); a startup failure shows a GTK or `MessageBoxW` box when stderr is no terminal (`gtk` is Tauri's own).
  Why: both formats suit the updater (#137), the plugin is Tauri's own, the switch reads the OS back, and the NSIS uninstall hook cannot tell an upgrade from an uninstall.
  Rejected: deb and MSI as well, a hand-written login `.desktop` and `Run` key writer, restoring over a Thalvi entry the user turned off in Windows (read from `StartupApproved` with `windows-registry`).
- **Pause before changing microphones.** Save device changes only while paused;
  enabling opens the selected device and status names the open stream separately.
  Why: choosing a microphone must not start recording or redirect a capture.
  Rejected: applying a device change automatically after an active capture.
- **Errors stay in the tray and Settings, not on the overlay.** Each says what failed, why and which setting to fix; its short headline shows on the
  overlay for 5 s, and again when it refuses a hold key press. Copy (settings, tray, `copy-error`) puts a report (causes, version, system, no secrets)
  on the clipboard through the host. Why: on Hyprland `no_focus` makes the overlay click-through, so a headline that stayed covered the screen with no way
  to close it. Rejected: keeping errors on the overlay, a focusable overlay, a duration per error, the WebView clipboard.
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
- **Reading a conversation never takes it over.** `GET /sessions/{id}` previews;
  only an explicit Continue here resumes, moving a live one from its device.
  Why: browsing must not cut off the device in use. Rejected: resume on open,
  shared ownership. Desktop conversation clients follow the same split.
- **Private conversations hold no store.** `/session?private=1` opens one with no store handle: never saved,
  listed, titled, retired or searched from others, metrics only whatever `THALVI_RECORD` says; closing ends it
  and a `private-` id can't be resumed. Desktop clients send the same flag and keep nothing derived locally.
  Why: a type that cannot write beats a flag that must not. Rejected: a private flag on saved sessions, reconnecting into a fresh saved one.
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
- **Work lands on `development`; releases merge it into `main`.** PRs target `development`; a release PR
  merges it into `main`, and only a tag on `main` publishes. Why: `main` stays what users run, and a
  release is one reviewable diff. Rejected: PRs straight to `main` (every merge a release candidate),
  changing GitHub's default branch during the hackathon freeze.
- **No review gate.** The `main` and `development` rulesets require PRs and passing checks but
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
- **Releases wait for approval.** `publish` runs in a `release` environment only the developer can approve.
  Why: agent tokens can push tags, and a tag builds and publishes to users. Rejected: a tag ruleset (the token
  acts as the developer, so any bypass for the developer also lets agents through).
- **Dependency and secret gates.** cargo-deny checks advisories, licences and sources; gitleaks scans
  history in CI and staged changes in a hook. Why: nothing else watched Rust advisories, the binaries link
  every crate, and on a public repo a pushed secret has leaked. Rejected: bans (nothing to ban), flagging
  unmaintained transitive crates (no upgrade exists), a hook that refuses when gitleaks is missing.
- **Rust caches are saved only on `development`.** CI runs on pushes to `development` and saves the Rust
  caches there; PRs, and mutation runs on the Linux one, only read them. Why: a PR can read only its own and its
  base branch's caches, so per-PR copies left every new PR cold and filled the 10 GB limit. Rejected: saving on
  every PR, saving on `main` (frozen, so its caches go stale and are evicted).
- **Mutation testing advises on the diff.** cargo-mutants reports survivors in a PR's changed Rust as
  annotations, on Linux. Why: agents write code and tests, and it found real gaps, but it can't tell those from
  equivalent mutants. Rejected: a required gate, whole-workspace runs (3,197 mutants, about 18 hours on a CI runner), Windows runs.
- **Property tests only where event order is the rule.** Today only the hold hook. Why: it found what tables
  missed there; on streaming and capture code it found nothing tables wouldn't. Rejected: properties everywhere.
