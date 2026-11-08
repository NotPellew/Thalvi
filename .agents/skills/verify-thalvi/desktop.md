# Verify the desktop app in a sandbox

Runs the real `thalvi-desktop` (a chosen commit's debug build) on a real
Hyprland, nested in a headless compositor, inside `bwrap`. Nothing of the
developer's desktop, microphone, audio, config, model cache or running Thalvi is
reachable, so an agent can use it with no developer involved. The web client has
its own recipes (`SKILL.md`); this file is the desktop counterpart.

It covers what shows on screen and in stdout: the overlay, the Settings window at
any size and look, model downloads, pause and resume. It has no real tray (use
the CLI below) and no Windows, and no microphone unless it is a GNOME run or `up --mic`
(audio is a null device otherwise). Report what a run covered with those limits.

`H` is the helper, run from the repository root:

```sh
H=.agents/skills/verify-thalvi/scripts/thalvi-desktop-rig
```

Needs `bwrap`, `kwin_wayland`, `Hyprland`, `hyprctl`, `grim`, `wtype`, python3
with Pillow (for image checks), and a non-NVIDIA render node (`/dev/dri`). Run
`$H help` for the commands. A GNOME run (below) instead needs `gnome-shell`,
`pipewire`, `wireplumber`, `ffmpeg`, `xdpyinfo`, python3 with `gi` and GTK 3, and
`wl-clipboard` for tests.

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
| Window size | `$H hypr $R dispatch setfloating "title:^Thalvi settings\$"`, then `$H hypr $R dispatch resizewindowpixel "exact 400 680,title:^Thalvi settings\$"`; repeat until `$H hypr $R clients` shows `size: 400,680` |
| Screenshot | `$H shot $R name` (whole screen, `$R/out/name.png`); crop with Pillow, view with the image reader |
| Frame burst | `$H burst $R name 100 0.15` writes `name-001.png...` and `name-times.txt` (time of each frame, same clock as the log) |
| Download proxy | `scripts/throttle-proxy.py PORT 250000` slows a real download so the notice stays on screen; with rate `0` it answers 502 after 2 s |
| Anything else inside | `$H sh $R <command>` |

- `app-start` returns before the app is ready. Repeat `$H ctl $R status` under a
  bounded loop until it answers, open Settings, then wait for its window in
  `$H hypr $R clients` before sending keys.
- The overlay and Settings share the window class `thalvi-desktop`, so a `class:`
  selector can move the overlay instead. Select Settings by its title.
- A screenshot right after a key can show the previous frame. Wait for the change
  to appear before keeping it as evidence.
- There is no pointer, so Settings is keyboard-driven. The first `Tab` focuses the
  search box, then the sidebar pages in order (Speech, Microphone, Capture,
  Feedback and appearance, Apps & tasks, History, App); `Return` opens the page.
  The page's fields follow, and `Shift+Tab` from the search box wraps to the
  footer. Take a screenshot after each step to see where the focus ring is
  rather than counting: the footer's order is Copy, ×, Revert, Save, and a long
  page needs about ten `Shift+Tab` presses from the sidebar to reach Save.
- A long page scrolls inside its own pane: focus a field in the page and press
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

## Speak, type and go offline (Hyprland)

`$H up --mic --offline` adds what a dictation check needs. `--mic` gives the run the
private PipeWire microphone a GNOME run has (`$H say $R file.wav`, real time, once the app
captures). `--offline` puts every sandbox of the run in its own network namespace with
only loopback, so nothing leaves the machine and abstract unix sockets are the run's own.

| Need | Command |
| --- | --- |
| Where typed text goes | `$H victim $R` opens a terminal that writes every byte typed into it to `$R/out/typed.txt` |
| App that can type | `$H app-start $R BIN --hosted ...` |
| Every socket the app opens | `$H app-start ... --trace` writes `$R/out/trace-TAG.txt` (strace of socket, connect, bind and datagram sends) |

- Use `--hosted` for anything that types or pastes. Thalvi finds the window to type into
  by its PID, and a window from another sandbox has PID 0 in the compositor, so the app
  and `victim` are started by Hyprland itself (`app-stop` stops the app the same way).
- A trace shows no AF_INET `connect` when nothing tried to leave the machine; a refused
  attempt still appears there (`ENETUNREACH`). Run an app with an unreachable
  `server` first as a control that the trace sees what it should.
- Copy model files into `$R/home/.local/share/thalvi/models/` first. With
  `recordings` on in the config (`metrics`, `text`), `$R/home/.local/share/thalvi/recordings/` holds
  what the recognizer heard and how each capture ended.
- Say the wake phrase, wait until `$H ctl $R status` shows `capturing`, wait about two
  seconds more (the app does not listen during its wake cue), then say the dictation. Sound
  played into the microphone before that is lost.
- Synthesize speech with Piper's `en_US-ryan-high` voice; espeak-ng and flite voices
  score near 0 on `hey_thalvi` and don't wake it. Voice:
  `https://huggingface.co/rhasspy/piper-voices/resolve/v1.0.0/en/en_US/ryan/high/en_US-ryan-high.onnx`
  plus its `.json`. Then `printf 'Hey Thalvi.' | uvx --from piper-tts==1.8.0 piper -m
  en_US-ryan-high.onnx -f raw.wav` and `ffmpeg -i raw.wav -ar 16000 -ac 1 -sample_fmt s16 wake.wav`.
- `say` holds a file while the app isn't reading the microphone, so a file can play late,
  into the next case. Wait until `$R/home/mic-queue` is empty before starting a case.
- `pw-link -l` and `pw-cli ls Node` in `$H sh $R` with `PIPEWIRE_REMOTE=rig-pipewire` show
  that the app reads only `rig-mic`.

## GNOME

`$H up --gnome` starts a headless `gnome-shell` instead of kwin and Hyprland, in the
same sandbox. Apps see `XDG_CURRENT_DESKTOP=GNOME`, `XDG_SESSION_TYPE=wayland`,
Wayland `rig-gnome` and Xwayland (`DISPLAY` names the socket file, with the run's
`XAUTHORITY`). `proof`, `app-start`, `app-stop`, `ctl`, `sh`, `shot` and `down`
work as above; `hypr` and `burst` do not exist.

| Need | Command |
| --- | --- |
| Which window has focus | `$H gnome $R focus` (titles), `$H gnome $R windows` (focus, class, size, title) |
| Keys | `$H gnome $R key Return`, `$H gnome $R type 'text'` (to the focused window) |
| Clipboard text | `$H gnome $R clip` reads through X11 without taking focus |
| Say something | `$H say $R file.wav` plays into the default microphone, in real time, once the app captures (16 kHz mono; later files queue) |
| A window that logs focus | `/tmp/thalvi-gnome-rig.py victim LOG &` inside `$H sh $R bash -c '...'`; the sandbox ends with its command |

- `wl-copy` and `wl-paste` each take keyboard focus for about a millisecond, every
  time (10 of 10 in a victim window). An X11 selection does not, and mutter
  bridges it both ways: judge whether the app steals focus from what it does, not
  from reading back with `wl-paste`.
- A Wayland window cannot refuse focus or place itself (GTK 3 `accept_focus`,
  keep-above and type hints did not help). An X11 window with the same hints takes
  no focus, and `move()` puts it at the top centre (below the 32 px panel).
- `shot` and `windows` ask the shell over D-Bus, which answers only the owner of
  `org.freedesktop.impl.portal.desktop.gnome`; the helper takes that name on the
  run's private bus. The orange dot in the panel comes with the virtual keyboard
  session.
- Playback goes to a speaker that discards, in real time.

## Isolation, and its limits

`$H proof $R` records what the sandbox cannot reach. A passing run shows: a PID
namespace that differs from the host's, no `/dev/snd`, only one render node, a
private `/run/user/<uid>` holding only the run's sockets (no PipeWire, Pulse or
real Wayland or Hyprland socket), `/home/<you>` absent with a private `HOME`,
no `DISPLAY`, and an ALSA null device. A GNOME run has its own system bus,
Xwayland and a private PipeWire instead (`rig-pipewire`, also in `up --mic` runs). The network
namespace is shared on purpose unless the run is `--offline`, so downloads are real and local
proxies work; it also means abstract unix sockets are shared with the host, which is why X11
is off and the app is forced onto Wayland.

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
- `gnome-shell` cannot start without logind, and the host's is the developer's
  session, so the host's system bus is hidden and `thalvi-gnome-rig.py logind`
  serves one session on a private one. Without a keyboard in the seat clients never
  get focus, so a held RemoteDesktop session makes one.
- ALSA's `file` and `null` plugins deliver audio at CPU speed and zero-fill a short
  read, so a FIFO does not pace them. The microphone is a PipeWire source on a pipe
  that a feeder writes in real time, and the ALSA default device is PipeWire's.
- `DISPLAY=:0` tries the host's abstract X socket first; the run's cookie makes the
  host refuse it (`proof` shows it), and the socket-file form avoids the attempt.
- The private D-Bus runs without AppArmor mediation, which fails inside the
  sandbox and would stop the relaunch path (`ctl`) from working.
