# App and settings

The running app: looks, settings, history and recordings, controls, errors and login.

- **Three looks for the desktop UI.** `look` is gunmetal (default), holo or neon. Each look's built-in colours and measures can be overridden under `themes` (`version: 1`), resolved as those defaults plus the sparse changes. The overlay hangs from the top of the usable area, one 460 × 200 size, and neon follows the system light or dark theme.
  Why: gunmetal is the developer's choice and the others stay; a damaged theme must not stop startup, so `themes` is the only key loaded leniently — an unreadable look keeps its JSON and draws the built-in look, and a save refuses a theme it changes into something unusable.
  Rejected: dropping the neon pill, y = 0 (a bar covers it), a size per look, rejecting the whole file when a theme is damaged, deleting customizations to recover.
- **Opt-in recordings.** Metrics, dictation text and mic audio (with ~2 s before each wake) are each
  written locally by settings switches, applied between captures. Why: to judge changes and
  misrecognitions on real use. Off by default: they are personal data.
- **Dictation history is on by default.** `history` keeps the text, time, target and outcome of the last 200
  captures (1 to 1000) in one file in the per-user data directory; switching it off stops writes and deletes nothing.
  Why: the user can recover what they dictated, and text alone never leaves the computer. Rejected: opt-in like the
  recordings (audio and metrics stay off), deleting entries when it is switched off.
- **The last dictation is an in-memory draft.** Settings → History & recordings holds the latest capture, also when it failed or was cancelled, to
  edit and Copy; a newer capture waits while the draft is edited. Why: fix a dictation without dictating it again, with nothing
  stored beyond the history; Windows refuses to bring the bound window to the front from Settings, so Paste could not work there.
  Rejected: persisting drafts, replacing edits, Paste into the bound window, retrying a partial delivery by itself.
- **Desktop recordings by wake.** `recordings/` holds only recordings: `metrics.jsonl` and a
  `<date>/<time>/` folder per wake (`wake.wav`, `speech.wav`, `text.txt`). Wake metrics: near misses
  (score from 0.15), wakes refused as too quiet, wakes that deliver nothing, one summary a minute
  (scoring latency, seconds with sound and quiet, switches between them). Why: one dictation in one place;
  95% of old lines were per frame. Rejected: day files linked by timestamp, a line per frame
  or per sound/quiet switch.
- **Resident controls and private request socket.** Commands reach the running app over a request/reply
  socket in the private runtime folder (a user-only pipe on Windows): one JSON line each way, size-bounded,
  known commands only. An agent can feed a WAV file in place of the microphone and set the app up (models, speech servers, word list, quit). Why: controls must not start a microphone, an agent needs an answer, and testing and setup must not need computer use.
  Rejected: single-instance relaunch (no answer), a keyboard listener, TCP, transcript-bearing replies (they stay rejected).
- **Packaged desktop with an OS-held login entry.** AppImage on Linux, per-user NSIS on Windows, released from `desktop-vX.Y.Z` tags that match the Cargo version (development builds add `-dev.N` at build time); login startup is the OS entry (`tauri-plugin-autostart`, run with `--login`), and the config keeps the preference (`start_at_login`, written by the switch alone, not with the settings form) so each start restores an entry an upgrade removed. Hyprland runs no XDG autostart unless `xdg-desktop-autostart.target` is up (uwsm starts it), so otherwise the row shows an `exec-once` line to copy and no entry is written. The AppImage's app menu entry is Thalvi's own file (`show_in_menu`, same pattern); a startup failure shows a GTK or `MessageBoxW` box when stderr is no terminal (`gtk` is Tauri's own).
  Updates (#137): `tauri-plugin-updater` follows `update_channel` (Stable or Development; signed packages only, never an older version); a download installs only when the engine thread itself finds Thalvi idle, and a one-shot `update.json` keeps the paused state across the restart and tells the next start what came of it.
  Why: both formats suit the updater, the plugin is Tauri's own, the switch reads the OS back, and the NSIS uninstall hook cannot tell an upgrade from an uninstall.
  Rejected: deb and MSI as well, a hand-written login `.desktop` and `Run` key writer, restoring over a Thalvi entry the user turned off in Windows (read from `StartupApproved` with `windows-registry`), idle read from a status snapshot.
- **Microphone changes apply like every setting.** The new device opens beside the open one and replaces it once it works, or after the running
  capture; one that can't open leaves the old one running and the audio status says why. Why: choosing a device must not need a pause, and a
  failure must not leave the user without a microphone. Rejected: pausing first, closing the old device before opening the new one.
- **Learn only the cleaned-up pasted span, behind an opt-in switch.** With **Learn from my corrections** on, a paste into a readable field is watched for five minutes; only what lies between the anchors around the pasted text is kept, beside the dictation in History when it is on and in the in-memory draft. Names the user corrected are offered as suggestions and added to the word list only on **Add**. Why: the fix belongs where the dictation is kept, nothing outside that span is remembered, and field text never reaches metrics, logs or recordings. Rejected: recording whole fields, keeping a rewrite or text typed outside the paste, adding words without the user's click, reading fields on Windows and GNOME until they expose them; on Hyprland the field is found from AT-SPI `object:text-changed` events because the desktop walk and AT-SPI pids do not identify it, and Chromium and Electron apps share text only when started with `ACCESSIBILITY_ENABLED=1` and `--force-renderer-accessibility`.
- **Errors stay in the tray and Settings, not on the overlay.** Each says what failed, why and which setting to fix; its short headline shows on the
  overlay for 5 s, and again when it refuses a hold key press. Copy (settings, tray, `copy-error`) puts a report (causes, version, system, no secrets)
  on the clipboard through the host. Why: on Hyprland the overlay takes no clicks, so a headline that stayed covered the screen with no way
  to close it. Rejected: keeping errors on the overlay, a focusable overlay, a duration per error, the WebView clipboard.
