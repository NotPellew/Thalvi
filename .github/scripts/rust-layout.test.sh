#!/usr/bin/env bash
set -euo pipefail

script="$(cd "$(dirname "$0")" && pwd)/rust-layout"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
status=0
fixture=0

# check WANT FILE...: lays out the files as crates/c/src/FILE, each given as `path=content`
# (\n expands), and runs the script on them.
# It must succeed when WANT is empty, otherwise fail with WANT in its output.
check() {
  local want=$1 dir="$work/$((++fixture))" out spec
  shift
  mkdir -p "$dir/crates/c/src"
  for spec in "$@"; do
    mkdir -p "$dir/crates/c/src/$(dirname "${spec%%=*}")"
    printf '%b' "${spec#*=}" >"$dir/crates/c/src/${spec%%=*}"
  done
  if out=$("$script" "$dir" 2>&1); then
    [[ -z $want ]] || { printf 'FAIL %s: succeeded, want %q\n' "$want" "$want" >&2; status=1; }
  elif [[ -z $want || $out != *"$want"* ]]; then
    printf 'FAIL want %q, got %q\n' "${want:-success}" "$out" >&2
    status=1
  fi
}

doc='//! A module.\n'
os_fn='#[cfg(target_os = "linux")]\nfn a() {}\n'

check 'crates/c/src/shared.rs:2: OS cfg in shared code: move the OS-specific part' "shared.rs=$doc$os_fn"
check '' "shared.rs=$doc"'#[cfg(windows)]\nmod w;\n#[cfg(target_os = "linux")]\npub(crate) use self::l as native;\n'
check '' "linux.rs=$doc$os_fn" "host/windows/place.rs=$doc$os_fn" "gnome/x.rs=$doc$os_fn"
check '' "shared.rs=$doc"'fn a() {}\n#[cfg(test)]\nmod tests {\n    #[cfg(unix)]\n    fn t() {}\n}\n#[cfg(test)]\nmod more;\n'
check 'inline test module is not the end of the file' "shared.rs=$doc"'#[cfg(test)]\nmod tests {\n}\nfn after() {}\n'
check 'shared.rs:2: OS cfg in shared code' "shared.rs=$doc"'fn a() -> bool { cfg!(unix) }\n'
check '' "main.rs=$doc"'#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]\n'
check 'shared.rs:1: the file does not start with a' 'shared.rs=fn a() {}\n'
check 'lib.rs:3: lib.rs holds more than module declarations' "lib.rs=${doc}pub mod a;\nfn b() {}\n"

[[ $status == 0 ]] || exit 1
