#!/usr/bin/env bash
# Observable checks for rig startup, model caching, stale-run pruning and Whisper helpers.
set -euo pipefail

root=$(cd "$(dirname "$0")/../../../.." && pwd)
rig=$root/.agents/skills/verify-thalvi/scripts/thalvi-desktop-rig
status=0
bases=()
stubs=()

pass() { printf 'PASS %s\n' "$1"; }
fail() { printf 'FAIL %s\n' "$1" >&2; status=1; }

# shellcheck disable=SC2329  # invoked by the EXIT trap
cleanup() {
  local d pidfile
  for d in "${stubs[@]}"; do
    if [ -s "$d/compositor.pid" ]; then
      IFS= read -r pidfile < "$d/compositor.pid"
      kill -- "-$pidfile" 2>/dev/null || true
    fi
  done
  for d in "${stubs[@]}"; do rm -rf -- "$d"; done
  for d in "${bases[@]}"; do rm -rf -- "$d"; done
}
trap cleanup EXIT

fresh_base() {
  local d
  d=$(mktemp -d)
  bases+=("$d")
  THALVI_RIG_BASE=$d
  export THALVI_RIG_BASE
}

# A PID that is already gone, without leaving a sleeper behind.
dead_pid() {
  local pid
  sleep 0.01 & pid=$!
  wait "$pid"
  printf '%s\n' "$pid"
}

test_missing_model() {
  fresh_base
  local base=$THALVI_RIG_BASE err=$THALVI_RIG_BASE/stderr
  if "$rig" up --models nosuch >"$base/stdout" 2>"$err"; then
    fail 'a missing model stops up'
    return
  fi
  if ! grep -q 'nosuch' "$err" || ! grep -Fq "$base/models/nosuch" "$err"; then
    fail "a missing model names the file and the cache path ($(tr '\n' ' ' < "$err"))"
    return
  fi
  if compgen -G "$base/run-*" > /dev/null; then
    fail 'a missing model leaves no run folder'
    return
  fi
  pass 'a missing model stops up, names the file and cache path, and leaves no run folder'
}

test_prune() {
  fresh_base
  local base=$THALVI_RIG_BASE
  local gone
  gone=$(dead_pid)
  mkdir -p "$base/models" "$base/bin/old" "$base/bin/new" "$base/bin/held" "$base/run-old" "$base/run-live" "$base/run-app"
  printf x > "$base/models/x.onnx"
  printf x > "$base/vt-dictation.wav"
  printf '%s\n' "$gone" > "$base/run-old/compositor.pid"
  printf '%s\n' "$$" > "$base/run-live/compositor.pid"
  printf '%s\n' "$gone" > "$base/run-app/compositor.pid"
  printf '%s\n' "$$" > "$base/run-app/app.pid"
  printf '%s\n' "$base/bin/held/thalvi-desktop" > "$base/run-live/app.bin"
  printf bin > "$base/bin/old/build"
  printf bin > "$base/bin/new/build"
  printf bin > "$base/bin/held/build"
  touch -d '2 days ago' "$base/bin/old/build" "$base/bin/held/build"
  touch -d '2 days ago' "$base/bin/old" "$base/bin/held" "$base/run-old" "$base/run-live" "$base/run-app"

  if "$rig" up --models nosuch >"$base/stdout" 2>"$base/stderr"; then
    fail 'prune: up with a missing model is expected to stop after pruning'
    return
  fi
  if [ -d "$base/run-old" ]; then
    fail 'prune: a stopped run older than 24 h stays'
    return
  fi
  if [ ! -d "$base/run-live" ]; then
    fail 'prune: a backdated run with a live compositor is removed'
    return
  fi
  if [ ! -d "$base/run-app" ]; then
    fail 'prune: a backdated run with a live app is removed'
    return
  fi
  if [ ! -f "$base/models/x.onnx" ] || [ ! -f "$base/vt-dictation.wav" ]; then
    fail 'prune: the model cache or a fixture WAV was touched'
    return
  fi
  if [ -d "$base/bin/old" ]; then
    fail 'prune: an unused bin folder older than 24 h stays'
    return
  fi
  if [ ! -d "$base/bin/new" ]; then
    fail 'prune: a fresh bin folder is removed'
    return
  fi
  if [ ! -d "$base/bin/held" ]; then
    fail 'prune: a bin folder a kept run uses is removed'
    return
  fi
  local expected actual
  expected=$(printf '%s\n' "$base/run-old" "$base/bin/old" | sort)
  actual=$(grep 'up: pruned ' "$base/stderr" | sed 's/^up: pruned //' | sort) || actual=
  if [ "$actual" != "$expected" ]; then
    fail "prune: stderr lists the wrong removals (got $(printf %q "$actual"), want $(printf %q "$expected"))"
    return
  fi
  pass 'prune: removes a stopped old run and an unused old bin, keeps a run with a live compositor or app, a used bin, the cache and fixtures, and reports exactly those removals'
}

test_models_are_hardlinked() {
  fresh_base
  local base=$THALVI_RIG_BASE
  mkdir -p "$base/models"
  printf a > "$base/models/a.onnx"
  printf x > "$base/models/stem.x"
  printf y > "$base/models/stem.y"
  chmod 444 "$base/models/a.onnx" "$base/models/stem.x" "$base/models/stem.y"

  local stub
  stub=$(mktemp -d)
  stubs+=("$stub")
  cat > "$stub/bwrap" <<'STUB'
#!/usr/bin/env bash
# up cats $out/*.log when the compositor dies; give it one so it reaches its own message.
[ -n "${LOG:-}" ] && : > "$(dirname "$LOG")/out/bwrap.log"
exit 1
STUB
  chmod +x "$stub/bwrap"

  if PATH="$stub:$PATH" "$rig" up --gnome --software --models a.onnx,stem >"$base/stdout" 2>"$base/stderr"; then
    fail 'models: a bwrap stub that exits 1 must stop up'
    return
  fi
  if ! grep -q 'GNOME Shell exited early' "$base/stderr"; then
    fail "models: expected the compositor to be reported as exited ($(tr '\n' ' ' < "$base/stderr"))"
    return
  fi
  local run
  run=$(find "$base" -mindepth 1 -maxdepth 1 -type d -name 'run-*' -print -quit)
  if [ -z "$run" ]; then
    fail 'models: no run folder survived the compositor failure'
    return
  fi
  local dest=$run/home/.local/share/thalvi/models f
  for f in a.onnx stem.x stem.y; do
    if [ ! -f "$dest/$f" ]; then
      fail "models: $f was not linked into the run"
      return
    fi
    if [ "$(stat -c %h "$dest/$f")" != 2 ]; then
      fail "models: $f is not a hardlink (link count $(stat -c %h "$dest/$f"))"
      return
    fi
    if [ "$(stat -c %i "$dest/$f")" != "$(stat -c %i "$base/models/$f")" ]; then
      fail "models: $f does not share the cache file's inode"
      return
    fi
  done
  pass 'models: --models a.onnx,stem hardlinks the named file and its NAME.* files into the run'
}

test_prune_keeps_build_named_by_relative_path() {
  fresh_base
  local base=$THALVI_RIG_BASE stub
  stub=$(mktemp -d)
  stubs+=("$stub")
  printf '#!/bin/sh\nexit 0\n' > "$stub/bwrap"
  chmod +x "$stub/bwrap"
  mkdir -p "$base/bin/rel" "$base/run-rel"
  printf '#!/bin/sh\n' > "$base/bin/rel/thalvi-desktop"
  chmod +x "$base/bin/rel/thalvi-desktop"
  touch "$base/run-rel/software"
  printf '%s\n' "$$" > "$base/run-rel/compositor.pid"
  (cd "$base" && PATH="$stub:$PATH" "$rig" app-start "$base/run-rel" bin/./rel/thalvi-desktop > /dev/null 2>&1)
  touch -d '2 days ago' "$base/bin/rel/thalvi-desktop" "$base/bin/rel"
  THALVI_RIG_BASE=$base/ "$rig" up --models nosuch > /dev/null 2>&1 || true
  if [ -d "$base/bin/rel" ]; then
    pass 'prune: a build a live run started by a relative path stays'
  else
    fail 'prune: a build a live run started by a relative path stays'
  fi
}

test_whisper_helpers() {
  fresh_base
  local base=$THALVI_RIG_BASE stub r
  stub=$(mktemp -d)
  stubs+=("$stub")
  cat > "$stub/bwrap" <<'STUB'
#!/bin/sh
printf '%s\n' "$@" > "$0.args"
STUB
  chmod +x "$stub/bwrap"
  r=$base/run-w
  mkdir -p "$base/bin/w" "$base/helpers/cpu" "$r/home" "$r/out"
  printf '#!/bin/sh\n' | tee "$base/bin/w/thalvi-desktop" > "$base/helpers/cpu/whisper-server"
  chmod +x "$base/bin/w/thalvi-desktop" "$base/helpers/cpu/whisper-server"
  touch "$r/software"
  PATH="$stub:$PATH" "$rig" app-start "$r" "$base/bin/w/thalvi-desktop" --whisper "$base/helpers" > /dev/null 2>&1 || true
  for _ in $(seq 50); do [ -s "$stub/bwrap.args" ] && break; sleep 0.1; done
  if [ ! -x "$r/home/appdir/usr/lib/Thalvi/whisper/cpu/whisper-server" ]; then
    fail 'whisper: --whisper DIR puts DIR/cpu/whisper-server under the run'"'"'s APPDIR'
    return
  fi
  if ! grep -qx APPDIR "$stub/bwrap.args" || ! grep -qx /home/rig/appdir "$stub/bwrap.args"; then
    fail "whisper: the app is started with APPDIR=/home/rig/appdir ($(tr '\n' ' ' < "$stub/bwrap.args"))"
    return
  fi
  if PATH="$stub:$PATH" "$rig" app-start "$r" "$base/bin/w/thalvi-desktop" --whisper "$base/bin" > /dev/null 2>&1; then
    fail 'whisper: a DIR without helpers stops app-start'
    return
  fi
  pass 'whisper: --whisper DIR copies the helpers to APPDIR/usr/lib/Thalvi/whisper and sets APPDIR; a DIR without them stops'
}

test_hyprland_startup() {
  local monitor_delay=$1 deadline=$2 pause=${3:-0} stub start elapsed rc=0
  fresh_base
  stub=$(mktemp -d)
  stubs+=("$stub")
  cat > "$stub/bwrap" <<'STUB'
#!/usr/bin/env bash
set -eu
if [ "${!#}" != /tmp/supervisor.sh ]; then
  printf 'Monitor fixture\n'
  exit
fi
run=${LOG%/*}
printf '%s\n' "$PPID" > "${0%/*}/compositor.pid"
if [ "$RIG_TEST_PAUSE" = 1 ]; then
  up_pid=$(ps -o ppid= -p "$PPID" | tr -d ' ')
  kill -STOP "$up_pid"
  (
    timeout 10s bash -c 'until [ -s "$1" ]; do sleep 0.1; done' _ "$run/out/inner.display" || true
    kill -CONT "$up_pid" 2>/dev/null || true
  ) </dev/null >/dev/null 2>&1 &
fi
export RIG_W=1920 RIG_H=1080
while [ "$1" != /tmp/supervisor.sh ]; do
  if [ "$1" = --setenv ] && [ "$2" = DBUS_SESSION_BUS_ADDRESS ]; then
    export DBUS_SESSION_BUS_ADDRESS="$3"
  fi
  shift
done
sed -e "s|/home/rig|$run/home|g" -e "s|/tmp/out|$run/out|g" \
  -e "s|^RUN=.*|export RUN=$run/rt|" "$run/supervisor.sh" > "$run/fixture-supervisor.sh"
exec bash "$run/fixture-supervisor.sh"
STUB
  cat > "$stub/fixture" <<'STUB'
#!/usr/bin/env bash
set -eu
case ${0##*/} in
  cat)
    case ${1:-} in /sys/class/drm/renderD*/device/vendor) echo 0x1002 ;; *) exec /bin/cat "$@" ;; esac ;;
  dbus-daemon) ;;
  wayland-info) echo "interface: 'wl_output'" ;;
  kwin_wayland) cd "$RUN"; exec python3 -c 'import socket; socket.socket(socket.AF_UNIX).bind("rig-outer")' ;;
  Hyprland)
    [ -n "$DBUS_SESSION_BUS_ADDRESS" ]
    for i in $(seq 25); do echo "hyprland log line $i"; done
    cd "$RUN"
    mkdir -p hypr/fixture
    exec python3 -c 'import signal,socket
for path in ["wayland-1", "hypr/fixture/.socket.sock"]:
    socket.socket(socket.AF_UNIX).bind(path)
signal.pause()' ;;
  hyprctl)
    now=$(date +%s%3N)
    [ -e "$RUN/monitor-probe" ] || printf '%s 0\n' "$now" > "$RUN/monitor-probe"
    read -r first probes < "$RUN/monitor-probe"
    (( probes += 1 ))
    printf '%s %s\n' "$first" "$probes" > "$RUN/monitor-probe"
    # Under load the old 100 probes may take longer than 10 s, so require another probe in that case.
    if [ "$RIG_TEST_MONITOR_DELAY" != never ] &&
        (( now - first >= RIG_TEST_MONITOR_DELAY && (RIG_TEST_PAUSE || probes > 100) )); then
      echo '[{"name":"fixture"}]'
    else
      echo '[]'
    fi ;;
esac
STUB
  chmod +x "$stub/bwrap" "$stub/fixture"
  local command
  for command in cat dbus-daemon kwin_wayland wayland-info Hyprland hyprctl; do
    ln -s fixture "$stub/$command"
  done

  start=$(date +%s%3N)
  PATH="$stub:$PATH" THALVI_RIG_RENDER=hardware THALVI_RIG_STARTUP_TIMEOUT="$deadline" \
    RIG_TEST_MONITOR_DELAY="$monitor_delay" RIG_TEST_PAUSE="$pause" timeout --kill-after=1s 30s "$rig" up \
    > "$THALVI_RIG_BASE/stdout" 2> "$THALVI_RIG_BASE/stderr" || rc=$?
  elapsed=$(( $(date +%s%3N) - start ))
  local description='Hyprland up times out at the monitor step and prints the last 20 Hyprland log lines'
  [ "$pause" = 0 ] || description='Hyprland up rejects a monitor that appeared after the deadline while up was paused'
  if [ "$monitor_delay" != never ] && [ "$pause" = 0 ]; then
    if [ "$rc" = 0 ] && [ "$elapsed" -ge 11000 ] && grep -q '^RUN_DIR=' "$THALVI_RIG_BASE/stdout"; then
      pass 'Hyprland up waits for a monitor that appears after the old 10 s limit'
    else
      cat "$THALVI_RIG_BASE/stderr" >&2
      fail "Hyprland up did not wait for the late monitor (exit $rc in ${elapsed}ms)"
    fi
  elif [ "$rc" = 1 ] && [ "$elapsed" -lt 8000 ] &&
      grep -Fq 'timed out waiting for Hyprland monitors' "$THALVI_RIG_BASE/stderr" &&
      diff -u <(seq -f 'hyprland log line %g' 6 25) <(grep '^hyprland log line ' "$THALVI_RIG_BASE/stderr"); then
    pass "$description"
  else
    cat "$THALVI_RIG_BASE/stderr" >&2
    fail "$description (exit $rc in ${elapsed}ms)"
  fi
}

test_missing_model
test_prune
test_prune_keeps_build_named_by_relative_path
test_models_are_hardlinked
test_whisper_helpers
test_hyprland_startup 11000 60
test_hyprland_startup never 3
test_hyprland_startup 4000 3 1
exit "$status"
