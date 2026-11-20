# Listening controls

The user can pause and resume listening, start a capture with a hold key, and
cancel any capture; the overlay at the top of the screen shows a capture while
it runs and hides when Thalvi is idle or paused.

Spec: none — no runner drives the real app; the hold-key state machine has the `formal-hold-keys` harness and capture cancellation the `formal-capture-cancel` model

## Sub-features

- `controls-pause` pause closes the microphone and drops a running capture; resume reopens it; toggle flips between them.
- `controls-hold` holding the dictate (F9) or write (F10) key captures while it is down.
- `controls-cancel` cancel ends a capture without delivering anything.
- `controls-overlay` the overlay shows the live wave during a capture, a label while loading or working, and **FAILED** or **NOT SENT** with a reason on failure.

## How to get to it (user POV)

- Tray: **Pause listening** / **Resume listening**, **Cancel capture**, **Settings…**, **Quit**. The rig has no tray; `ctl` sends the same requests.
- Hold keys: F9 and F10 on Hyprland, Alt+Z and Alt+X on Windows; Settings → **Listening** → **Hold keys**.
- Settings → **Listening** → **Right now** → **Listening** switch.
- Spoken ending **never mind** cancels a wake capture.

## Driving it with the rig

Preconditions:

- Baseline from [README.md](README.md); `ctl state` shows `listening`, `paused: false`, overlay `hidden`.

- **Pause.** `$H ctl $R pause` replies `took_effect: true`; `ctl state` shows `state: paused`, `paused: true`, overlay `hidden`. A second `pause` replies `"error":"Thalvi is already paused."`, `took_effect: false`.
- **Toggle back.** `$H ctl $R toggle` replies `took_effect: true` (its `state` can read `loading` for a moment); `ctl state` then shows `listening`.
- **Hold.** Take `$H shot $R base`, count `desktop_transcription_ready` in `$MET`, then `$H ctl $R hold-start` and `$H burst $R hold 12 0.03`. Wait under `timeout 10` for a new ready event; `ctl state` shows `capturing`, overlay `listening`, and the frames show the overlay at top centre.
- **Cancel.** `$H ctl $R cancel` replies `took_effect: true`, then **always** `$H ctl $R hold-end` (it replies `No capture is being taken.` and still releases the simulated key).
- **Proof.** A new `desktop_capture_flow` with `outcome: cancelled`, `source: microphone`; `$R/out/typed.txt` unchanged; `ctl state` back to `listening`, overlay `hidden`.

## Layout and timing

| Screen (checked when it first appears) | Layout: elements checked | Wait: action → awaited element, normal, limit | Fallback signal |
|---|---|---|---|
| Overlay during a hold (Hold) | the overlay lies wholly inside the screen at top centre, by screenshot, at 1920×1080 only | `ctl hold-start` → the overlay's interior differs from `base` (crop about x 800–1120, y 30–85 on a 1920×1080 run), 0.33 s, 1.3 s | overlay **Speech model not ready — Settings** and `desktop_capture_flow` `reason: not_ready` |

Measured 2026-11-20: 0.33 s for the first hold after start, 0.15 and 0.16 s
after. Smaller screens are unverified for the overlay.

## Gotchas

- Compare only the overlay's interior: a crop that includes a window border changes when focus moves and gives a false early hit.
- Pair every `hold-start` with `hold-end`, also after `cancel` or a failure; otherwise the next start looks like a repeated key-down and nothing starts.
- `ctl status` gives the tray's short class; `ctl state` gives the phase, `paused`, overlay and recognizer. Neither contains dictated text.
- Pause, resume and toggle wait up to 3 s for the change to take effect and then reply with a no-effect error.
