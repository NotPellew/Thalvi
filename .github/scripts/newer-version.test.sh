#!/usr/bin/env bash
set -euo pipefail

script="$(dirname "$0")/newer-version"
status=0

check() {
  local a=$1 b=$2 want=$3 got=0
  "$script" "$a" "$b" 2>/dev/null || got=$?
  if [[ $got != "$want" ]]; then
    printf 'FAIL %q %q: exit %s, want %s\n' "$a" "$b" "$got" "$want" >&2
    status=1
  fi
}

check 0.1.3 "" 0
check 0.1.3-dev.1 "" 0
check 0.1.4 0.1.3 0
check 0.1.3 0.1.4 1
check 0.1.10 0.1.9 0
check 1.0.0 0.9.9 0
check 0.1.3 0.1.3 1
check 0.1.3 0.1.3-dev.9 0
check 0.1.3-dev.9 0.1.3 1
check 0.1.3-dev.10 0.1.3-dev.9 0
check 0.1.3-dev.9 0.1.3-dev.10 1
check 0.1.3-dev.9 0.1.3-dev.9 1
check 0.1.4-dev.1 0.1.3 0
check 0.1.3-rc.1 0.1.2 2
check 0.1.3 v0.1.2 2

exit "$status"
