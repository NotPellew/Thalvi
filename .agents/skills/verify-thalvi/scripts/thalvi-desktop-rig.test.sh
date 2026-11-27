#!/usr/bin/env bash
# Exercises Hyprland startup through the rig's public up command with compositor stubs.
set -euo pipefail

rig=$(cd "$(dirname "$0")" && pwd)/thalvi-desktop-rig
scratch=$(mktemp -d)
status=0
stubs=()

pass() { printf 'PASS %s\n' "$1"; }
fail() { printf 'FAIL %s\n' "$1" >&2; status=1; }

# shellcheck disable=SC2329  # invoked by the EXIT trap
cleanup() {
  local stub pid
  for stub in "${stubs[@]}"; do
    if [ -s "$stub/compositor.pid" ]; then
      IFS= read -r pid < "$stub/compositor.pid"
      kill -- "-$pid" 2>/dev/null || true
    fi
  done
  rm -rf -- "$scratch"
}
trap cleanup EXIT

monotonic_ms() {
  local uptime
  read -r uptime _ < /proc/uptime
  printf '%s\n' "$(( 10#${uptime/./} * 10 ))"
}

test_hyprland_startup() {
  local monitor_delay=$1 deadline=$2 pause=${3:-0} fixture stub start elapsed rc=0
  fixture=$(mktemp -d "$scratch/case.XXXXXX")
  stub=$fixture/stubs
  mkdir "$stub"
  export THALVI_RIG_BASE=$fixture/base
  mkdir "$THALVI_RIG_BASE"
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
    read -r uptime _ < /proc/uptime
    now=$(( 10#${uptime/./} * 10 ))
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

  start=$(monotonic_ms)
  PATH="$stub:$PATH" THALVI_RIG_RENDER=hardware THALVI_RIG_STARTUP_TIMEOUT="$deadline" \
    RIG_TEST_MONITOR_DELAY="$monitor_delay" RIG_TEST_PAUSE="$pause" timeout --kill-after=1s 30s "$rig" up \
    > "$THALVI_RIG_BASE/stdout" 2> "$THALVI_RIG_BASE/stderr" || rc=$?
  elapsed=$(( $(monotonic_ms) - start ))
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

test_hyprland_startup 11000 60
test_hyprland_startup never 3
test_hyprland_startup 4000 3 1
exit "$status"
