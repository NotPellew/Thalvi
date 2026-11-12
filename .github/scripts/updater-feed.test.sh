#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
feed="$root/.github/scripts/updater-feed"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
status=0
export GITHUB_REPOSITORY=owner/repo

key() {
  npm exec --prefix "$root/crates/thalvi-app/ui" -- tauri signer generate --ci -p "pw-$1" -w "$work/$1.key" >/dev/null
  jq -n --arg k "$(<"$work/$1.key.pub")" '{plugins: {updater: {pubkey: $k}}}' >"$work/$1.conf.json"
}
key app
key other

# run NAME PACKAGE_VERSION SIGNING_KEY PASSWORD WANT_ERROR: builds both packages at
# PACKAGE_VERSION (minus any platform named in DROP), runs the feed for release 1.2.3
# with the app's public key, and checks it fails with WANT_ERROR or succeeds when empty.
run() {
  local name=$1 built=$2 signing=$3 password=$4 want=$5 dir="$work/$1" out
  mkdir -p "$dir"
  for suffix in amd64.AppImage x64-setup.exe; do
    [[ ${DROP:-} == "$suffix" ]] || echo "$name $suffix" >"$dir/Thalvi_${built}_$suffix"
  done
  local key=""
  [[ -z $signing ]] || key=$(<"$work/$signing.key")
  if out=$(TAURI_SIGNING_PRIVATE_KEY=$key TAURI_SIGNING_PRIVATE_KEY_PASSWORD=$password \
    "$feed" "$work/app.conf.json" 1.2.3 desktop-v1.2.3 "$dir" 2>&1); then
    [[ -z $want ]] || { printf 'FAIL %s: succeeded, want error %q\n' "$name" "$want" >&2; status=1; }
  elif [[ -z $want || $out != *"$want"* ]]; then
    printf 'FAIL %s: got %q, want %q\n' "$name" "$out" "${want:-success}" >&2
    status=1
  fi
  if [[ -n $want && -e $dir/latest.json ]]; then
    printf 'FAIL %s: wrote latest.json despite failing\n' "$name" >&2
    status=1
  fi
}

run signed 1.2.3 app pw-app ""
run no-key 1.2.3 "" pw-app "TAURI_SIGNING_PRIVATE_KEY secret is missing"
run no-password 1.2.3 app "" "TAURI_SIGNING_PRIVATE_KEY_PASSWORD secret is missing"
DROP=x64-setup.exe run no-windows 1.2.3 app pw-app "windows-x86_64 package Thalvi_1.2.3_x64-setup.exe is missing"
run other-version 1.2.2 app pw-app "package is Thalvi_1.2.2_amd64.AppImage, but the release is version 1.2.3"
run other-key 1.2.3 other pw-other "does not verify against the public key"

dir="$work/signed"
want='{"version":"1.2.3","notes":"https://github.com/owner/repo/releases/tag/desktop-v1.2.3","platforms":["linux-x86_64","windows-x86_64"]}'
got=$(jq -c '{version, notes, platforms: (.platforms | keys)}' "$dir/latest.json")
[[ $got == "$want" ]] || { printf 'FAIL latest.json: got %s, want %s\n' "$got" "$want" >&2; status=1; }
base64 -d <<<"$(jq -r .plugins.updater.pubkey "$work/app.conf.json")" >"$work/key.pub"
for platform in linux-x86_64 windows-x86_64; do
  url=$(jq -r --arg p "$platform" '.platforms[$p].url' "$dir/latest.json")
  file="$dir/${url##*/}"
  [[ $url == https://github.com/owner/repo/releases/download/desktop-v1.2.3/Thalvi_1.2.3_* && -f $file ]] ||
    { printf 'FAIL %s url %s\n' "$platform" "$url" >&2; status=1; continue; }
  jq -r --arg p "$platform" '.platforms[$p].signature' "$dir/latest.json" | base64 -d >"$work/sig"
  minisign -Vq -p "$work/key.pub" -x "$work/sig" -m "$file" || { printf 'FAIL %s signature\n' "$platform" >&2; status=1; }
  echo tampered >>"$file"
  if minisign -Vq -p "$work/key.pub" -x "$work/sig" -m "$file" 2>/dev/null; then
    printf 'FAIL %s: a tampered package verified\n' "$platform" >&2
    status=1
  fi
done

exit "$status"
