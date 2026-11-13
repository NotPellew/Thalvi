#!/usr/bin/env bash
set -euo pipefail

script="$(cd "$(dirname "$0")" && pwd)/changed"
repo=$(mktemp -d)
trap 'rm -rf "$repo"' EXIT
cd "$repo"
git init -q
git config user.email test@example.com
git config user.name test
git commit -q --allow-empty -m base
status=0

check() {
  local want=$1 got
  shift
  git switch -q --detach main
  for file in "$@"; do
    mkdir -p "$(dirname "$file")"
    echo "$file" > "$file"
  done
  git add -A
  git commit -q --allow-empty -m change
  got=$("$script" HEAD~1 HEAD)
  if [[ $got != "$want" ]]; then
    printf 'FAIL %s: got %q, want %q\n' "$*" "$got" "$want" >&2
    status=1
  fi
}

git branch -q main
check server crates/thalvi-server/src/lib.rs
check 'desktop app' crates/thalvi-desktop/src/config.rs
check app crates/thalvi-app/src/update.rs
check desktop-ui crates/thalvi-app/ui/src/main.ts
check 'web speech' web/src/view.ts speech/asr/app.py compose.yaml
check issues .github/scripts/issue-refs.test.sh
check 'server desktop app' crates/thalvi-server/Cargo.toml crates/thalvi-desktop/src/a.rs crates/thalvi-app/src/b.rs
for file in Cargo.toml Cargo.lock rust-toolchain.toml .cargo/mutants.toml .node-version deny.toml \
  .github/workflows/ci.yml .github/scripts/changed crates/thalvi-app/tauri.conf.json crates/thalvi-asr/src/lib.rs README.md; do
  check all web/src/view.ts "$file"
done
check all

git switch -q --detach main
if [[ $("$script" HEAD missing 2>/dev/null) != all ]]; then
  echo "FAIL an unreadable diff: want all" >&2
  status=1
fi

git clone -q --depth 1 "file://$repo" shallow
if [[ $(cd shallow && "$script" HEAD HEAD) != all ]]; then
  echo "FAIL a shallow clone: want all" >&2
  status=1
fi

exit "$status"
