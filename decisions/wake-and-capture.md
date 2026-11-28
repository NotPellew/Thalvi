# Wake and capture

Starting and ending a capture: wake words, hold keys, cues and silence.

- **Two wake entries, endings select the capture route.** Trained capture and conversation heads; the
  desktop runs only capture, and the saved conversation head stays unused now that conversations left
  Thalvi. A capture whose whole text is an `open_url`/`focus_app` phrase or `open <host>` runs as a task
  with no ending. Why: a ready command needs no ending; a head with no handler dropped wakes.
  Rejected: one phrase; hiding it in Settings.
- **Wake context.** Both trained heads use 16 embedding frames. Why: this
  supports first-frame inference with upstream startup and reset history.
  Rejected: accepting longer contexts that need additional warm-up history.
- **Quiet wake floor.** A wake whose last 1.28 s (the head's 16 frames) peaked below 1,500 as a
  sample magnitude starts nothing and is recorded as `desktop_wake_refused`. Why: heads score speech
  alike at any volume; real wakes peaked at 2,073 or more, faint false wakes at 1,152 or less, RMS never separated.
  Rejected: RMS, a floor per microphone (revisit if a quieter one appears), the last few frames.
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
- **Musical earcons.** Five switchable cues share one output stream, paused between cues and rebuilt on device change or error; the mic drops frames only during playback, except a hold's start cue. Accepted delivery waits for final text, while its cue keeps playing; submit and task pauses and confirmation stay.
  Why: distinct contours explain capture state, opening output delays cues, and an undoable paste need not wait for sound. Rejected: running silence (blocks sleep), one beep for every state, a post-delivery cue, muting a hold's start cue.
- **Keybind modes.** Settings → Keys is a list of keybinds, each an action (Dictate, or Write to
  write the words out first), a mode and a key; several rows can work one action. Hold dictates
  while the key is down and pastes on release, F9 and F10 alone on Hyprland and Alt+KeyZ and
  Alt+KeyX on Windows by default; Toggle starts on a press, or ends and pastes when a capture runs;
  Start only starts and Stop only ends. One key capture runs at a time: a Hold or Start press while
  one runs and a Stop press with none are refused with the overlay notice, and a Toggle or Stop
  press ends the capture running, delivered as the action it started with. Every capture a key
  started ends only by a key or `cancel`: no spoken or pause ending, so every word is text. Each
  immediate start keeps up to 480 ms (6 frames) of pre-roll from the listening ring, and an AltGr-only press
  keeps its audio from the press while it waits 0.2 s to be told from a chord. The rows are stored
  only when they differ from the desktop's two Hold rows, and a config's `hold_key`/`write_key`
  become its Hold rows once on load; then the old fields are gone. A key appears in one row, and an
  action in the list needs a row that can start it and one that can end it. Hyprland adds Hold
  rows as `bindrti` release binds and the others as press binds at runtime through `hyprctl`,
  removed on quit and re-added on `configreloaded`; Windows swallows each matched key's press,
  repeats and release, acting only on the press for the other modes. Why: Hyprland 0.53.3 types
  held modifiers into the paste and drops the release when one comes up first, seen live, and drops
  a plain `bindr` when another key is pressed or a modifier is down during the hold, so a Hold's
  release is `bindrti`; `RegisterHotKey` sees no release; a long hold means holding a key for
  minutes, which Discord's keybinds spare the user; binds in the user's config go stale when the
  executable moves; a 450 ms pre-roll kept first words that shorter pre-roll lost in Hex's test.
  Rejected: one key and one mode per action, spoken endings during a key capture, a start-only
  action without a stop key, Ctrl+Z/Y, a managed `thalvi-binds.conf` the user sources, hand-written
  binds, a modifier checklist with a key drop-down (Settings records the combination the user
  presses).
- **Wake words are a switch apart from listening.** `wake.enabled` (stored only when off) turns spoken activation off while listening stays on: the engine skips the wake model and the hold keys work, the switch applies as a retune with no rebuild, and keyboard-only is its own status (`keyboardOnly`, tray icon and `state` word `keyboard`). Why: pausing also refuses the hold keys, and a user who wants only the keyboard has no other way to stop wake words from starting captures. Rejected: pausing as the way to turn wake words off, unloading the wake model (re-enabling would need a rebuild).
- **AltGr alone may be a keybind key on Windows.** `AltRight` bound alone in any mode works; AltGr with any other key is a chord that types, so the hook passes every event, ignores the Ctrl Windows adds, and starts the keybind only after 0.2 s alone, keeping the audio from the press. Why: AltGr types `@`, `€` and `{` on layouts that have it, and a swallowed AltGr would break typing for them. Reversible: a capture started by AltGr and a second key stays a capture.
  Rejected: swallowing AltGr, starting the hold at the press and cancelling on a chord (cue and overlay would flash on every `@`).
- **Hold release flush.** At release a hold sends 800 ms of generated silence (kept out of the recording) to streaming servers that have no end-of-input message and to streaming extras, while local sessions skip those frames and flush their right-context tail in one pass on `input_ended`; the hold pastes on the next final or after a session-reported wait: 250 ms, or 400 ms only for a decoder whose final re-hears the end
  of the stretch (Parakeet's, measured at 235-397 ms loaded); a tentative decoding that commits frames without a token while the voice
  detector still hears speech replays its whole stretch. The session is told where the hold's audio ends, so only audio sent before the
  release is owed a result: a session is behind only while one of those frames has none, and a final ends the wait only once none is left,
  so a final covering one stretch of a longer hold keeps it waiting. A decoder whose last window's words come out only once the silence
  after them has been decoded (Nemotron Unified's) stays behind until it has answered the end of the input, while one whose partials
  already carry them (Nemotron Streaming, and Parakeet within its wait) is caught up at release. Only a session still behind past that wait shows "Finishing…"; it keeps the hold waiting up to
  15 s, then the hold pastes what it has and says the end may be missing. The wait is recorded apart from the model's latency. Why: a
  streaming recognizer emits a word only after audio past it, an early final used to cut a lagging hold to the audio heard so far; a busy
  machine (Settings opening during a hold) made the recognizer fall seconds behind, so the 250 ms paste lost the end while saying it
  worked; Nemotron Unified keeps its last word until its stretch is finished, and feeding it 800 ms of release silence forced 5-6 full
  616-frame GPU passes (400-1,835 ms on Radeon 8060S) instead of one right-context lookahead pass (~100 ms); showing Finishing
  from the release flashed it on ordinary releases. Measured headless on the server recognizer, no last word lost in 336 releases (75%
  before when released on the word's end). Rejected: one longer wait for every recognizer; a fixed 500 ms tail; an end-of-stream message
  through the server (11% lost at the word's end, a protocol change); pasting at the wait and only warning; more left context or a global
  mel over the capture (both stall alike); counting the flush as owed for every decoder (ordinary releases would wait on silence and
  flash Finishing).
