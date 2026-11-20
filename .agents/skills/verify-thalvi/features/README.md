# Thalvi verification map

One recipe per user-facing feature of the desktop app, driven in the sandboxed
Hyprland rig of [desktop.md](../desktop.md). Read that guide's Build, Launch
and Doctor first; every recipe starts from a run it describes.

## Baseline preconditions

- `H=.agents/skills/verify-thalvi/scripts/thalvi-desktop-rig`, run from the
  repository root, and `R` is a run this task started with `$H up`.
- The app is this task's build, started with `app-start ... --hosted` and a
  config with on-device Nemotron and `"recordings":{"metrics":true,"text":true,"audio":false}`.
- The model files a start needs are copied in (desktop.md, Launch), so nothing
  downloads unless the recipe says so.
- `MET=$R/home/.local/share/thalvi/recordings/metrics.jsonl`.
- Never drive an app or compositor this task did not start.

## Driving conventions

- Settings has no pointer: `$H sh $R wtype -k Tab` moves focus, `Return`
  activates, `space` flips a switch, `Down` changes a drop-down. Focus Settings
  first with `$H hypr $R dispatch focuswindow "title:^Thalvi settings\$"`.
- Take a screenshot (`$H shot $R NAME`) after each key and read the focus ring
  before the next one; Tab counts below are what a fresh run needed, not a
  contract.
- Wait for an observable event (a metric line, `ctl state`, a changed frame)
  under `timeout`, never a fixed sleep.
- Time a wait with `$H burst $R NAME N 0.03` started right after the action,
  stamped with `$EPOCHREALTIME` before it; compare each frame's crop with a
  frame taken before the action.

## Proof and skip reporting

- Pair each action with its result: screenshot, `ctl` reply, metric line, or
  `$R/out/typed.txt`.
- A settings change is proven by the screen, `ctl settings get` and the run's
  own `$R/home/.config/thalvi/desktop.json`, then again after a restart.
- Report a step that could not run with the command and the missing
  precondition; never report it as covered by another path.

## Layout checks

The real window (WebKitGTK) exposes no DOM, so boxes are measured in Chromium
with the UI's fake backend:

```sh
.agents/skills/verify-thalvi/scripts/settings-boxes PAGE 400x680,780x680,1280x900 OUT.json ROLE=NAME...
```

It needs `npm ci` in `crates/thalvi-app/ui` and the headless shell
(`npm exec --prefix crates/thalvi-app/ui -- playwright-core install --only-shell chromium`).
Each element is scrolled into view vertically only; it must be visible,
unoccluded and wholly inside the window, and no two listed elements may
overlap. Exit 1 names each failure. Then check the same page in the real
window at 400×680 by screenshot (desktop.md, Drive: Window size).
400×680 is the narrowest size the guides use; 780×680 is the window's opening
size; nothing sets a minimum.

## Feature entry contract

Each file: an H1 and one paragraph, a `Spec:` line, then `Sub-features`,
`How to get to it (user POV)`, `Driving it with the rig` (starting with
`Preconditions:`), `Layout and timing` where something is visible, and
`Gotchas`. Keep implementation details out; name user paths, commands and proof.

## Features

- [Settings](settings.md): open, keyboard navigation, autosave with **Applied.**, persistence across restarts.
- [Dictation](dictation.md): wake word or file feed, recognition, spoken ending, delivery into the focused window.
- [Listening controls](controls.md): pause, resume, toggle, hold, cancel and the overlay that shows them.
- [Recognition](recognition.md): the Recognition page, streaming servers and what a capture does when one is unreachable.
- [Model downloads](downloads.md): the wake model Download button, its notices, failure and success through the proxy.
