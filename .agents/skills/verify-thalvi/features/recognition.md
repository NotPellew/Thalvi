# Recognition

The Recognition page chooses the **Live recognizer** and manages speech
entries in four groups; when the chosen server cannot be reached, a capture
fails at once and says so in plain words with where to fix it.

Spec: none — no runner drives the real app; `crates/thalvi-app/ui/src/settings.test.ts` (Vitest, jsdom, run by PR CI) covers the page's logic

## Sub-features

- `recognition-live` **Live recognizer** picks the entry that shows words and hears endings; **Next dictation:** states the choice and its status.
- `recognition-entries` groups **Streaming servers**, **After-you-stop servers**, **On this computer**, **Extra-only servers**, each with **+**, and rows with **Edit** and **Live** or an **Extra** switch.
- `recognition-editor` **Edit** opens **Editing “NAME”** with **Name**, **Address**, **API key**, **Model**, **Cancel** and **Save**; fields are drafts until **Save**.
- `recognition-unreachable` a capture through an unreachable server fails with **FAILED — Can't reach speech server — Settings**.

## How to get to it (user POV)

- Tray **Settings…** → **Recognition** (the page Settings opens on).
- `thalvi-desktop show recognition`.

## Driving it with the rig

Preconditions:

- Baseline from [README.md](README.md). Nothing listens on `127.0.0.1:1`.

- **Check an entry.** `$H ctl $R speech-check '{"kind":"realtime","id":"rig","name":"Rig","active":true,"address":"ws://127.0.0.1:1","model":"test","api_key":""}'` replies `{"ok":true,"ready":true}`: the fields are valid. It does not connect.
- **Apply it.** `$H ctl $R settings set '{"stt":{"entries":[<that entry>,{"kind":"on_device","id":"dev","name":"On this computer","active":false,"model":"nemotron_160ms"}]}}'` replies `applies: now`. `ctl state` names `Rig` with status `ready`, which is not a connection either.
- **See the page.** `$H ctl $R show recognition`; the drop-down reads **Rig (streaming)** and **Streaming servers** lists `Rig 127.0.0.1:1` with **Live** and **Edit**.
- **Capture.** `$H ctl $R hold-start`, `$H burst $R rt 15 0.03`, `$H ctl $R hold-end` (replies `No capture is being taken.`: it already failed).
- **Proof.** The overlay shows **FAILED** / **Can't reach speech server — Settings**; the log says `Thalvi could not reach the speech server "Rig" at ws://127.0.0.1:1 (connection refused). Check that it is running, or fix its address under Settings → Recognition.`; the error bar shows the same; `$MET` has `desktop_transcription_ready` `outcome: error` and `desktop_capture_flow` `outcome: failed`, `reason: recognizer`; `ctl state` overlay `{"headline":"Can't reach speech server — Settings","state":"error"}`.
- **Editor.** Focus Settings, `Escape`, then 12 Tabs reach **Edit** for Rig (on a 400×680 window; check the ring). `Return` opens **Editing “Rig”** with focus in **Name** within a frame. Its **Cancel** closes it without saving.
- **Restore.** `ctl settings set` with the on-device entry active; `ctl state` shows it `loading`, then `ready`.

## Layout and timing

Window sizes 400×680, 780×680 and 1280×900, with `settings-boxes` (README,
Layout checks) and a real-window screenshot at 400×680.

| Screen (checked when it first appears) | Layout: elements checked | Wait: action → awaited element, normal, limit | Fallback signal |
|---|---|---|---|
| Recognition page (See the page) | `heading=Recognition`, `combobox=Live recognizer`, `button=Add a streaming server`, `button=Edit Realtime transcription (WebSocket)`, `switch=Extra: Realtime transcription (WebSocket)`, `button=Add an on-device model`, `button=Edit On this computer`, `button=Add an extra-only server` (names from the fake backend) | none: the page is already loaded when Settings is | none |
| Error overlay (Capture) | the overlay lies wholly inside the screen at top centre, by screenshot, at 1920×1080 only | `ctl hold-start` → red **FAILED** in the overlay (more than 30 pixels with red > 180, green and blue < 120 in x 760–1160, y 0–110), 0.16 s, 0.65 s | none: this is itself the failure path; a capture that stays `capturing` instead is the defect |

Measured 2026-11-20: error overlay 0.16, 0.10, 0.08 s. Layout, 2026-11-23:
400×680, 560×680, 780×680 and 1280×900 pass. Below 561 px the Live recognizer
row is 87 px tall in Gunmetal and Holo and 92 px in Neon, with the drop-down
under its description; at 780 px it is 95 px.

## Gotchas

- `speech-check` and `state`'s `status: ready` prove only valid fields; only a capture's `desktop_transcription_ready` `ok` proves a connection.
- The rig's names differ from the fake backend's: the real page lists what the run's config holds.
- The editor's fields do not save on blur; only **Save** applies them, and **Clear saved key** clears the key only on **Save**.
- An active entry cannot be removed; one with a saved key asks before removal.
