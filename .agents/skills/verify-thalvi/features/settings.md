# Settings

The Settings window lists seven pages in a sidebar; a switch, drop-down or
text field saves itself when changed, says **Applied.** beside it, and the
value survives a restart.

Spec: none — no runner drives the real app; `crates/thalvi-app/ui/src/settings.test.ts` (Vitest, jsdom, run by PR CI) covers the page logic, and `npm run explore` (Playwright, fake backend) explores focus and progress

## Sub-features

- `settings-open` opens on first start, from the tray or `ctl show [page]`; closing it hides it.
- `settings-pages` sidebar order: Listening, Recognition, Commands, Chat model, Overlay & sounds, History & recordings, App.
- `settings-autosave` a change saves at once and shows **Applied.**, or **Applied. From the next dictation.** during a capture.
- `settings-persist` the saved value is in `desktop.json` and returns after a restart.

## How to get to it (user POV)

- Tray **Settings…**; tray **Last dictation…** opens History & recordings.
- Starting Thalvi again while it runs opens the window.
- `thalvi-desktop show overlay` (any page: `listening`, `recognition`, `commands`, `chat`, `overlay`, `history`, `app`).

## Driving it with the rig

Preconditions:

- Baseline from [README.md](README.md); a fresh config (Wake sound on).

- **Open a page.** `$H ctl $R show overlay`. Reply `{"ok":true,"took_effect":true}`; the screenshot shows the **Overlay & sounds** heading and its rows. The reply alone does not prove the page rendered.
- **Reach a switch.** Focus Settings, then `$H sh $R wtype -k Tab` eleven times: search box, seven sidebar buttons, **Show the overlay**, **Show the draft**, the **Look** group (one stop). One more Tab focuses **Wake sound**; confirm its ring in a screenshot.
- **Change it.** `$H sh $R wtype -k space`. The switch turns off and **Applied.** appears under it within a frame.
- **Read it back.** `$H ctl $R settings get` shows `settings.cues.wake: false`, and `$R/home/.config/thalvi/desktop.json` has `cues.wake: false`. Read only that key.
- **Restart.** `$H ctl $R quit` (log ends `Stopped.`), `$H app-stop $R`, then `app-start` the same binary **without** `--config`, which would overwrite the file. Wait for `ctl status` to leave `unavailable`/`loading`.
- **Proof.** `$H ctl $R show overlay`; the screenshot shows Wake sound off, and `settings get` still says `false`.

## Layout and timing

Window sizes 400×680, 780×680 and 1280×900, with `settings-boxes` (README,
Layout checks) and a real-window screenshot at 400×680.

| Screen (checked when it first appears) | Layout: elements checked | Wait: action → awaited element, normal, limit | Fallback signal |
|---|---|---|---|
| Overlay & sounds (Open a page) | `searchbox=Search settings`, `button=Listening`, `button=App`, `heading=Overlay & sounds`, `switch=Show the overlay`, `switch=Show the draft`, `radio=Gunmetal`, `radio=Neon`, `switch=Wake sound`, `switch=Error sound` | `ctl show overlay` on a hidden window after a restart → the settled page (the last burst frame), 0.19 s, 0.75 s | the error bar (`role=alert`) at the window's foot; `Thalvi did not answer: timed out.` from `ctl` |
| Wake sound row after the change (Change it) | none beyond the page | `space` → switch flipped and **Applied.**, 0.18 s (first burst frame in each run), 0.75 s | a reason in the row's alert instead of **Applied.**, and the switch flips back |

Measured 2026-11-20 on the developer's PC (Radeon render node, 1920×1080 rig):
open 0.185, 0.129, 0.130 s; toggle visible in the first frame each time
(≤0.18 s, ≤0.05 s, ≤0.05 s). All three sizes pass.

## Gotchas

- The window opens by itself on a first start, so the first `show` is not a cold open; time opens after a restart.
- A drop-down or switch saves on change; a text field saves when it loses focus. Fields in a speech-entry editor are drafts until its **Save** (see [recognition.md](recognition.md)).
- Hyprland tiles Settings beside other windows; float and resize it for a size check (desktop.md, Drive), and crop by the window's `at`/`size` from `hypr clients`.
- `ctl settings set` returns before the page re-renders; take the screenshot after the change appears.
