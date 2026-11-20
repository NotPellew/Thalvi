# Dictation

The user says the wake phrase, dictates, and ends with a spoken ending such as
**Finish dictation**; Thalvi recognizes the words and delivers them, without
the ending, into the window that had focus when the capture began.

Spec: none — no runner drives the real app; recognition, endings and delivery are unit-tested in `crates/thalvi-desktop`

## Sub-features

- `dictation-wake` the wake phrase starts a capture (a hold key does too: [controls.md](controls.md)).
- `dictation-recognize` the on-device model turns speech into text as it arrives.
- `dictation-ending` **Finish dictation** ends the capture after a short quiet and is removed from the text.
- `dictation-deliver` the text is pasted into the focused window (GNOME: copied, with **Copied — press Ctrl+V to paste**).

## How to get to it (user POV)

- Say **Hey Thalvi**, speak, say **Finish dictation**.
- Hold the dictate key (F9 on Hyprland), speak, release.
- Settings → **Commands** → **Endings** lists the ending phrases.

## Driving it with the rig

Preconditions:

- Baseline from [README.md](README.md); `ctl state` shows `listening` and recognizer `ready`.
- A fixture as desktop.md (Feed a file) describes, made from Piper `en_US-ryan-high`: `Hey Thalvi.`, 1 s silence, a distinctive sentence, 0.8 s silence, `Finish dictation.` synthesized with `--length-scale 1.25`, 1.5 s silence; 16 kHz mono PCM16 under `/var/tmp`. Record its SHA256.

- **Give it a target.** `$H victim $R`, wait for `title: victim` in `$H hypr $R clients`, then `$H hypr $R dispatch focuswindow "title:^victim\$"`.
- **Dictate.** Note the count of `"call":"deliver"` lines in `$MET`, then `$H ctl $R feed /var/tmp/thalvi-rig/<fixture>.wav`. It replies when the last frame is fed, with `took_effect: true`.
- **Wait for delivery.** Under `timeout 20`, wait for the deliver count to rise.
- **Proof.** `$R/out/typed.txt` ends with the sentence and no ending phrase. `$MET` has a new `desktop_transcription_ready` (`ok`), `desktop_capture_flow` with `outcome: ok`, `route: paste`, `source: file`, and `desktop_host_call` `call: deliver` `ok`. `$R/home/.local/share/thalvi/recordings/<date>/<time>/text.txt` holds what was heard.

## Layout and timing

| Screen (checked when it first appears) | Layout: elements checked | Wait: action → awaited element, normal, limit | Fallback signal |
|---|---|---|---|
| Overlay while capturing (Dictate) | the overlay lies wholly inside the screen at top centre, by screenshot, at 1920×1080 only | end of the spoken ending → text in `typed.txt` (the deliver metric's `ts_ms` minus (feed start + the ending's end offset in the clip)), 1.63 s, 6.5 s | `desktop_capture_flow` `outcome: failed`; overlay **Speech model not ready — Settings** (`reason: not_ready` after its 8 s readiness bound) |

Measured 2026-11-20: 1.63, 1.53, 1.51 s; the clip's ending ends 6.07 s in.
Smaller screens are unverified for the overlay.

## Gotchas

- At normal speed the Ryan voice's `Finish dictation.` was heard as `Finish vacation` (ending spotter: 0 hits) and the capture stayed open; `--length-scale 1.25` ended it 7 of 7 times. Close a capture that never ends with `ctl cancel`.
- `ctl state` shows `copy_error: true` after any capture in the rig: the cue sound fails on the null audio device (`Audio feedback failed. Check the output device. The capture continues.`). It is not a delivery failure.
- `desktop_ready_cue` and `desktop_end_cue` report `outcome: error` for the same reason.
- The text has no trailing space; repeated runs run together in `typed.txt`. Compare the tail.
- `--hosted` is required for delivery: Thalvi finds the target window by PID.
