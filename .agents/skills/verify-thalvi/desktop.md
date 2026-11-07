# Verify the desktop app in a sandbox

Runs the real `thalvi-desktop` (a chosen commit's debug build) on a real
Hyprland, nested in a headless compositor, inside `bwrap`. Nothing of the
developer's desktop, microphone, audio, config, model cache or running Thalvi is
reachable, so an agent can use it with no developer involved. The web client has
its own recipes (`SKILL.md`); this file is the desktop counterpart.

It covers what shows on screen and in stdout: the overlay, the Settings window at
any size and look, model downloads, pause and resume. It has no microphone (audio
is a null device), no real tray (use the CLI below) and no Windows. Report what a
run covered with those limits.

`H` is the helper, run from the repository root:

```sh
H=.agents/skills/verify-thalvi/scripts/thalvi-desktop-rig
```

Needs `bwrap`, `kwin_wayland`, `Hyprland`, `hyprctl`, `grim`, `wtype`, python3
with Pillow (for image checks), and a non-NVIDIA render node (`/dev/dri`). Run
`$H help` for the commands.

## Build the commit

Build from your own detached copy, never from a checkout someone else uses. Use
`dev-job` so scratch is managed; copy the binary out before the job ends.

```sh
job=$(dev-job new thalvi-desktop-build)
dev-job run --job "$job" -- bash -c '
  git worktree add --detach "$DEV_JOB_SCRATCH/src" <sha> &&
  (cd "$DEV_JOB_SCRATCH/src/crates/thalvi-app/ui" && npm ci && npm run build) &&
  (cd "$DEV_JOB_SCRATCH/src" && cargo build -p thalvi-app --locked) &&
  mkdir -p /var/tmp/thalvi-rig/bin/<name> &&
  cp "$CARGO_TARGET_DIR/debug/thalvi-desktop" /var/tmp/thalvi-rig/bin/<name>/'
dev-job finish "$job"
```

A warm target builds in about a minute. Evidence and binaries live under
`/var/tmp/thalvi-rig/` (`THALVI_RIG_BASE` changes it); never commit them.

## Launch

```sh
R=$($H up | sed -n 's/^RUN_DIR=//p')                      # compositors; add --size 1920x1080 to change
$H proof $R                                              # isolation proof, saved as $R/out/isolation.txt
$H app-start $R /var/tmp/thalvi-rig/bin/<name>/thalvi-desktop --config cfg.json --tag case1 \
    --env HTTPS_PROXY=http://127.0.0.1:18081
```

- `up` is one run; several `app-start`/`app-stop` cycles can follow. Reset state
  between cases by deleting `$R/home/.local/share/thalvi` (that is the app's
  private data directory, models included) and rewriting
  `$R/home/.config/thalvi/desktop.json` (`--config` copies a file there).
- Without `--config` the defaults apply, and the on-device speech model
  downloads 650 MB at start. Always set `server` to an unreachable address
  (`ws://127.0.0.1:1`, never the developer's server) and, when the on-device
  engine is chosen, `HTTPS_PROXY=http://127.0.0.1:1` unless the download is the
  point. Copy model files read-only into `$R/home/.local/share/thalvi/models/`
  to skip a download.
- App output is `$R/out/app-<tag>.log`, every line stamped with its arrival time.
  The app's own state: `$H ctl $R status`.

## Drive

| Need | Command |
| --- | --- |
| Tray actions | `$H ctl $R pause`, `resume`, `toggle`, `cancel`, `status` (the relaunch path the tray and key binds use) |
| Open Settings | `$H ctl $R` with no argument |
| Keys into the focused window | `$H sh $R wtype -k Tab` (also `-M shift -k Tab -m shift`, `-k Return`, `-k Down`, `-k Next` for Page Down) |
| Window size | `$H hypr $R dispatch setfloating "class:^thalvi-desktop\$"`, then `$H hypr $R dispatch resizewindowpixel "exact 400 680,class:^thalvi-desktop\$"`; read it back with `$H hypr $R clients` |
| Screenshot | `$H shot $R name` (whole screen, `$R/out/name.png`); crop with Pillow, view with the image reader |
| Frame burst | `$H burst $R name 100 0.15` writes `name-001.png...` and `name-times.txt` (time of each frame, same clock as the log) |
| Download proxy | `scripts/throttle-proxy.py PORT 250000` slows a real download so the notice stays on screen; with rate `0` it answers 502 after 2 s |
| Anything else inside | `$H sh $R <command>` |

- There is no pointer, so Settings is keyboard-driven. The first `Tab` focuses the
  search box, then the sidebar pages in order (Speech, Microphone, Capture,
  Feedback and appearance, Apps & tasks, History, App); `Return` opens the page.
  The page's fields follow, and `Shift+Tab` from the search box wraps to the
  footer. Take a screenshot after each step to see where the focus ring is
  rather than counting: the footer's order is Copy, ×, Revert, Save, and a long
  page needs about ten `Shift+Tab` presses from the sidebar to reach Save.
- In a narrow window the whole page scrolls: focus a sidebar page and press
  `Next` (Page Down) to scroll it, `Home` to return.
- The overlay is a pill hanging at the top centre of the screen. Move the
  Settings window away from it (`movewindowpixel`) or float it small, and look at
  frames, not the log: a log line and a visible notice differ by a frame or two.
  A download that fails at once (a refused connection) never shows its notice
  long enough to catch; use `throttle-proxy.py PORT 0` for a late failure.
- Change the proxy a running app uses by moving what listens on its port, since
  an app's environment is fixed when it starts.
- `Hyprland` only renders a window that is mapped; open Settings before taking
  screenshots of it.

## Isolation, and its limits

`$H proof $R` records what the sandbox cannot reach. A passing run shows: a PID
namespace that differs from the host's, no `/dev/snd`, only one render node, a
private `/run/user/<uid>` holding only the run's sockets (no PipeWire, Pulse or
real Wayland or Hyprland socket), `/home/<you>` absent with a private `HOME`,
no `DISPLAY`, and an ALSA null device. The network namespace is shared on
purpose, so downloads are real and local proxies work; it also means abstract
unix sockets are shared with the host, which is why X11 is off and the app is
forced onto Wayland.

## Evidence

Keep screenshots, burst frames, logs and `isolation.txt` under
`$R/out/`. Name frames by case. For a timeline, read `name-times.txt` beside the
log's stamps and state what frame first shows a notice and which first does not.

## Teardown

```sh
$H app-stop $R     # the app only
$H down $R         # app and compositors; $R/out stays
```

Both stop by recorded PID (the holder shell of each sandbox); nothing is
matched by name. Check with `ps -eo pid,etime,cmd | grep -E 'bwrap|/tmp/thalvi'`
that nothing of this run is left. Stop proxies you started by their recorded
PID. Never run `thalvi-desktop` outside the sandbox: even `--help` starts the
app against the real session.

## Why it is built this way

- Hyprland needs a Wayland host that offers `xdg_wm_base` version 6. `sway`
  1.11 offers 5 (Hyprland exits with `CBackend::create() failed`), headless
  Hyprland cannot start without a host, so the host is `kwin_wayland --virtual`.
- NVIDIA's EGL fails inside the sandbox (no device nodes), so only an AMD or
  Intel render node is bound, with Mesa forced.
- `bwrap --die-with-parent` follows the process that launched it, so each
  sandbox is started under a detached holder shell; killing the holder takes the
  sandbox down.
- The private D-Bus runs without AppArmor mediation, which fails inside the
  sandbox and would stop the relaunch path (`ctl`) from working.
