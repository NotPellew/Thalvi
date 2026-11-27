#!/usr/bin/env bash
set -euo pipefail

scripts=$(cd "$(dirname "$0")" && pwd)
workflow=$scripts/../workflows/release.yml
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
check=$scripts/release-no-update-fixture

sed 's/tauri build --bundles/tauri build --features update-fixture --bundles/' \
  "$workflow" > "$scratch/release.yml"
if "$check" "$scratch/release.yml"; then
  echo 'The guard accepted a release that enables update-fixture.' >&2
  exit 1
fi
sed '/THALVI_RELEASE_BUILD:/d' "$workflow" > "$scratch/release.yml"
if "$check" "$scratch/release.yml"; then
  echo 'The guard accepted a release without THALVI_RELEASE_BUILD.' >&2
  exit 1
fi
