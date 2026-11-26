#!/usr/bin/env bash
# Observable checks for the rig's read-only model cache, hardlinks, stale-run pruning and Whisper helpers.
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
  local d
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

test_missing_model
test_prune
test_prune_keeps_build_named_by_relative_path
test_models_are_hardlinked
test_whisper_helpers
exit "$status"
