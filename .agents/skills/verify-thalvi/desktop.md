# Verify the desktop app in a sandbox

Runs the real `thalvi-desktop` (a chosen commit's debug build) on a real
Hyprland, nested in a headless compositor, inside `bwrap`. The developer's
home, desktop, microphone, audio, config, model cache and running Thalvi are out
of reach (the rest of `/` is read-only, and the network is shared unless the run
is `--offline`; see Isolation), so an agent can use it with no developer involved. The web client has
its own recipes (`SKILL.md`); this file is the desktop counterpart.

It covers what shows on screen and in stdout: the overlay, the Settings window at
any size and look, model downloads, pause and resume. It has no real tray (use
the CLI below) and no Windows, and no microphone unless it is a GNOME run or `up --mic`
(audio is a null device otherwise). Report what a run covered with those limits.

`H` is the helper, run from the repository root:

```sh
H=.agents/skills/verify-thalvi/scripts/thalvi-desktop-rig
```

Needs `bwrap`, `kwin_wayland`, `Hyprland`, `hyprctl`, `wayland-info`, `grim`, `wtype`, python3
with Pillow (for image checks), and a non-NVIDIA render node (`/dev/dri`). Run
`$H help` for the commands. A GNOME run (below) instead needs `gnome-shell`,
`pipewire`, `wireplumber`, `ffmpeg`, `xdpyinfo`, python3 with `gi` and GTK 3, and
`wl-clipboard` for tests.

## Build the commit

Build in the change's own worktree, never in a checkout someone else uses, and
keep that worktree's `target/` between runs: the second build of the same commit
compiles nothing. Follow the machine's own build rules (a build runner, lean
build settings) when the run's brief carries them.

```sh
cd <worktree>   # at the commit to verify
ui=crates/thalvi-app/ui
[ -f $ui/dist/settings.html ] && [ -z "$(find $ui/src $ui/settings.html $ui/vite.config.ts $ui/package-lock.json -newer $ui/dist/settings.html)" ] ||
  (cd $ui && { [ -d node_modules ] || npm ci; } && npm run build)
cargo build -p thalvi-app --locked
T=${CARGO_TARGET_DIR:-target}
mkdir -p /var/tmp/thalvi-rig/bin/<name> &&
cp $T/debug/thalvi-desktop /var/tmp/thalvi-rig/bin/<name>/ &&
cp -L $T/debug/libwebgpu_dawn.so /var/tmp/thalvi-rig/bin/<name>/
```

The UI is rebuilt only when a source is newer than the last build: Tauri embeds
`ui/dist`, so rebuilding unchanged files would recompile the app.

The binary loads `libwebgpu_dawn.so` from its own folder (rpath `$ORIGIN`);
`app-start` and `ctl` bind every `.so` beside it into the sandbox, and without it
the app exits 127 at once. A warm target builds in about a minute. Evidence and binaries live under
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
- Without `--config`, speech uses On this computer. To verify migration of an
  old saved config, pass an active legacy Thalvi server entry with an unreachable
  address so it cannot connect to the developer's server:
  `{"stt":{"entries":[{"kind":"thalvi_server","id":"rig","name":"Rig","active":true,"address":"ws://127.0.0.1:1"}]}}`.
  An on-device entry (`{"kind":"on_device","id":"dev","name":"On this computer","active":true,"model":"nemotron_160ms"}`)
  downloads about 650 MB at start; add `--env HTTPS_PROXY=http://127.0.0.1:1`
  unless the download is the point. Add `"recordings":{"metrics":true,"text":true,"audio":false}`
  for the evidence below.
- Copy model files read-only from a cache into `$R/home/.local/share/thalvi/models/`
  to skip downloads (the app checks their hashes). Every start needs
  `melspectrogram.onnx`, `embedding_model.onnx`, `hey_thalvi_v0.0.14.onnx`,
  `hey_mycroft_v0.1.onnx`, `openwakeword/` and the ending spotter's
  `kws_gigaspeech_3m.*` and `sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01/`
  (about 20 MB together); on-device Nemotron adds `nemotron_160ms.*`. A missing
  ending spotter downloads from modelscope.cn, which can be slow enough to fail;
  the app then says so and keeps working.
- App output is `$R/out/app-<tag>.log`, every line stamped with its arrival time.
  The app's own state: `$H ctl $R status`.

## Drive

| Need | Command |
| --- | --- |
| Tray actions | `$H ctl $R pause`, `resume`, `toggle`, `cancel`, `status` (the relaunch path the tray and key binds use) |
| Open Settings | `$H ctl $R` with no argument, or `$H ctl $R show capture` for one page |
| What Thalvi and its overlay show | `$H ctl $R state` (state, `overlay`, `recognizer`; never what was said) |
| Set up from a fresh config | `$H ctl $R models`, `$H ctl $R model-download speech nemotron_160ms` (replies when ready), `$H ctl $R speech-check '<entry json>'`, `$H ctl $R settings set '<json>'` |
| Quit the app like the tray | `$H ctl $R quit`; the log ends with `Stopped.` |
| Keys into the focused window | `$H sh $R wtype -k Tab` (also `-M shift -k Tab -m shift`, `-k Return`, `-k Down`, `-k Next` for Page Down) |
| Window size | `$H hypr $R dispatch setfloating "title:^Thalvi settings\$"`, then `$H hypr $R dispatch resizewindowpixel "exact 400 680,title:^Thalvi settings\$"`; repeat until `$H hypr $R clients` shows `size: 400,680` |
| Screenshot | `$H shot $R name` (whole screen, `$R/out/name.png`); crop with Pillow, view with the image reader |
| Frame burst | `$H burst $R name 100 0.15` writes `name-001.png...` and `name-times.txt` (time of each frame, same clock as the log) |
| Download proxy | `.agents/skills/verify-thalvi/scripts/throttle-proxy.py PORT 250000` slows a real download so the notice stays on screen; with rate `0` it answers 502 after 2 s |
| Anything else inside | `$H sh $R <command>` |

- `app-start` returns before the app is ready. `$H ctl $R status` answers at once
  with class `unavailable` (not running yet), then `loading`; repeat it under a
  bounded loop until the class is neither, open Settings, then wait for its window in
  `$H hypr $R clients` before sending keys.
- The overlay is a layer surface (`$H hypr $R layers` lists `Thalvi overlay`), not
  a window, so `$H hypr $R clients` shows only Settings.
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
- The overlay hangs at the top centre of the screen, above windows. Move the
  Settings window away from it (`movewindowpixel`) or float it small, and look at
  frames, not the log: a log line and a visible notice differ by a frame or two.
  A download that fails at once (a refused connection) never shows its notice
  long enough to catch; use `throttle-proxy.py PORT 0` for a late failure.
- Change the proxy a running app uses by moving what listens on its port, since
  an app's environment is fixed when it starts.
- `Hyprland` only renders a window that is mapped; open Settings before taking
  screenshots of it.

## Feed a file instead of speaking

`$H ctl $R feed /var/tmp/thalvi-rig/wake.wav` plays a WAV file into the app in place of its
microphone, at the pace of speech and through the same wake word, recognition, endings and
delivery, so a dictation needs no `--mic`, no `say` and no GNOME run. The reply comes when the last
frame has been fed (`state` shows what the capture is doing then); `ctl cancel` stops a feed. The
file is read by the app inside its sandbox, so keep it under `/var/tmp`, not in a home folder, and
make it a 16-bit PCM WAV no longer than the capture limit. The engine drops audio while its
wake cue plays, whether it comes from the microphone or a file, so leave about half a second of
silence after the wake phrase. Captures fed from a file carry `"source": "file"` in
`metrics.jsonl`. Pair it with `victim` and `--hosted` to see the text land.

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
  speech server first as a control that the trace sees what it should.
- Copy the model files (Launch lists them) in first. With
  `recordings` on in the config (`metrics`, `text`), `$R/home/.local/share/thalvi/recordings/` holds
  what the recognizer heard and how each capture ended.
- Say the wake phrase (or `ctl hold-start`), then wait until the capture is ready before
  saying the dictation. `ctl status` reads `capturing` from the moment the wake cue starts,
  and the app records nothing until the cue (250 to 370 ms) has ended and the recognizer is
  connected; a clip started on that word lost its first 250 ms in 1 of 16 trials. With
  `metrics` on, a new `desktop_transcription_ready` line in
  `$R/home/.local/share/thalvi/recordings/metrics.jsonl` marks the moment (0.25 to 0.55 s after
  `hold-start`); `say` right after it kept the whole clip in 16 of 16 trials. Without
  metrics, wait about two seconds after `capturing`. Sound played into the microphone before
  that is lost.
- Synthesize speech with Piper's `en_US-ryan-high` voice; espeak-ng and flite voices
  score near 0 on `hey_thalvi` and don't wake it. Voice:
  `https://huggingface.co/rhasspy/piper-voices/resolve/v1.0.0/en/en_US/ryan/high/en_US-ryan-high.onnx`
  plus its `.json`. Then `printf 'Hey Thalvi.' | uvx --from piper-tts==1.8.0 piper -m
  en_US-ryan-high.onnx -f raw.wav` and `ffmpeg -i raw.wav -ar 16000 -ac 1 -sample_fmt s16 wake.wav`.
- `say` takes a host path. It prints `QUEUED=NAME`, and `$R/out/mic.log` gets `played NAME`
  once the last sample is in the microphone. Wait for that line before `ctl hold-end`:
  `timeout 20 sh -c 'until grep -q "played $1" "$0"; do sleep 0.05; done' $R/out/mic.log NAME`.
  The queue empties when a file starts, not when it ends.
- Pad each dictation clip with trailing silence (`ffmpeg ... -af apad=pad_dur=1s`). A hold
  released at the very last sample lost the last word (`on Tuesday` became `onto`); with
  300 ms or 1 s of silence the whole sentence arrived in 4 of 4 holds.
- For a spoken ending use `Finish dictation.`: from the Ryan voice it ended and pasted 3 of 3
  wake captures, while `Paste that.` was heard as `Taste that` and the capture stayed open.
  `ctl cancel` closes a capture that never ends.
- `say` holds a file while the app isn't reading the microphone, so a file can play late,
  into the next case. Wait until `$R/home/mic-queue` is empty before starting a case.
- `pw-link -l` and `pw-cli ls Node` in `$H sh $R` with `PIPEWIRE_REMOTE=rig-pipewire` show
  that the app reads only `rig-mic`.

## The NVIDIA GPU and the AppImage

- The sandbox binds only the non-NVIDIA render node, so an app sees no NVIDIA GPU. `app-start --nvidia` (not with
  `--hosted`) binds the NVIDIA device nodes into the app's sandbox and drops the Mesa-only EGL vendor list, which
  makes NVIDIA's Vulkan driver fail to load.
- A compositor that renders on the Radeon makes Mesa's device-select layer list the Radeon first, and that is the
  adapter ONNX Runtime's WebGPU picks. `--env NODEVICE_SELECT=1` restores the RTX as the first device, as on a
  desktop whose compositor renders on it.
- An AppImage cannot mount in the sandbox (no FUSE). Extract it with `--appimage-extract` and give `app-start` a
  wrapper script that `exec`s the extracted `AppRun`; `ctl` runs the same wrapper.

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
| Say something | `$H say $R file.wav` plays into the default microphone, in real time, once the app captures (16 kHz mono; later files queue; `played NAME` in `$R/out/mic.log` when done) |
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
$H ctl $R quit     # the app quits as the tray's Quit does; the next two make sure
$H app-stop $R     # the app only
$H down $R         # app and compositors; $R/out stays
```

Both stop by recorded PID (the holder shell of each sandbox). The one name
match is a `--hosted` app, which `app-stop` ends with `pkill -x app-bin` inside
the run's own compositor PID namespace. Check with `ps -eo pid,etime,cmd | grep -E 'bwrap|/tmp/thalvi'`
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
