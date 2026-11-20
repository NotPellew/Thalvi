#!/usr/bin/env bash
# Observable checks for the GNOME AppImage smoke: a delivery that leaves the clipboard
# untouched must fail with that clipboard text, and a blank Recognition page must fail.
set -euo pipefail

root=$(cd "$(dirname "$0")/../../../.." && pwd)
check=$root/.github/scripts/e2e/gnome/check
rig=$root/.agents/skills/verify-thalvi/scripts/thalvi-desktop-rig
expected=$(tr -d '\r' < "$root/.github/scripts/e2e/fixtures/dictation.txt")
expected=${expected%$'\n'}
status=0

pass() { printf 'PASS %s\n' "$1"; }
fail() { printf 'FAIL %s\n' "$1" >&2; status=1; }

if [[ $expected == 'The blue umbrella.' ]]; then
  pass 'committed transcript stays The blue umbrella.'
else
  fail "committed transcript stays The blue umbrella. (got $(printf %q "$expected"))"
fi

help=$("$rig" help)
if [[ $help == *Hyprland* && $help == *'--gnome'* && $help != *'--software is required'* ]]; then
  pass 'rig help still describes Hyprland and optional GNOME'
else
  fail 'rig help still describes Hyprland and optional GNOME'
fi

unknown=$(mktemp)
if "$rig" up --no-such-flag >/dev/null 2>"$unknown"; then
  fail 'an unknown up flag is rejected'
elif grep -q 'unknown argument' "$unknown"; then
  pass 'an unknown up flag is rejected'
else
  fail "an unknown up flag is rejected ($(tr '\n' ' ' < "$unknown"))"
fi
rm -f "$unknown"

have_check() {
  if [[ -f $check ]]; then return 0; fi
  fail "$1: check is absent ($check)"
  return 1
}

transcript() {
  local name=$1 want_ok=$2 actual=$3
  if ! have_check "$name"; then return; fi
  local err
  err=$(mktemp)
  if "$check" transcript "$expected" "$actual" > /dev/null 2>"$err"; then
    if [[ $want_ok == yes ]]; then pass "$name"; else fail "$name: accepted $(printf %q "$actual")"; fi
  else
    if [[ $want_ok == yes ]]; then
      fail "$name: $(tr '\n' ' ' < "$err")"
    elif grep -qF "$actual" "$err"; then
      pass "$name"
    else
      fail "$name hid the clipboard text: $(tr '\n' ' ' < "$err")"
    fi
  fi
  rm -f "$err"
}

page() {
  local name=$1 want_ok=$2 text=$3
  if ! have_check "$name"; then return; fi
  local err
  err=$(mktemp)
  if printf '%s' "$text" | "$check" page > /dev/null 2>"$err"; then
    if [[ $want_ok == yes ]]; then pass "$name"; else fail "$name: accepted a page without the Recognition labels"; fi
  else
    if [[ $want_ok == yes ]]; then
      fail "$name: $(tr '\n' ' ' < "$err")"
    elif grep -qi 'blank' "$err"; then
      pass "$name"
    else
      fail "$name did not say the page was blank: $(tr '\n' ' ' < "$err")"
    fi
  fi
  rm -f "$err"
}

transcript 'the normalized fixture matches itself' yes "$expected"
transcript 'case and a missing stop match the fixture' yes 'the blue umbrella'
transcript 'a different phrase fails with the actual clipboard' no 'The red umbrella.'
transcript 'a delivery no-op keeps the sentinel clipboard' no 'THALVI-GNOME-SENTINEL-603'

page 'Recognition labels pass' yes $'Recognition\nLive recognizer\nOn this computer'
page 'a blank Recognition page fails' no ''
page 'whitespace is a blank Recognition page' no $' \n\t'

if have_check 'a blank screenshot fails the render check'; then
  blank=$(mktemp --suffix .png)
  err=$(mktemp)
  python3 - "$blank" <<'PY'
import struct
import sys
import zlib

path, width, height = sys.argv[1], 64, 64
raw = b"".join(b"\x00" + bytes([20, 24, 32]) * width for _ in range(height))

def chunk(tag, data):
    return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
png += chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b"")
open(path, "wb").write(png)
PY
  if "$check" page-image "$blank" > /dev/null 2>"$err"; then
    fail 'a blank screenshot fails the render check: the solid image was accepted'
  elif grep -qi 'blank' "$err"; then
    pass 'a blank screenshot fails the render check'
  else
    fail "a blank screenshot fails the render check: $(tr '\n' ' ' < "$err")"
  fi
  rm -f "$blank" "$err"
fi

exit "$status"
