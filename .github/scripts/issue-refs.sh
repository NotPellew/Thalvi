#!/usr/bin/env bash
# Prints the numbers that a PR body on stdin closes with a closing keyword, or
# reverts, in the repo OWNER/REPO, one per line in first-seen order.
set -euo pipefail

case "${1-}" in
  closes) keyword='close[sd]?|fix(e[sd])?|resolve[sd]?' ;;
  reverts) keyword='reverts' ;;
  *)
    echo "usage: $0 closes|reverts OWNER/REPO < body" >&2
    exit 2
    ;;
esac
repo=${2,,}

{ grep -oiE "(^|[^[:alnum:]_])($keyword):?[[:space:]]+([[:alnum:]_.-]+/[[:alnum:]_.-]+)?#[0-9]+" || true; } |
  while IFS= read -r ref; do
    ref=${ref##*[[:space:]]}
    owner_repo=${ref%#*}
    if [[ -z $owner_repo || ${owner_repo,,} == "$repo" ]]; then
      echo "${ref##*#}"
    fi
  done |
  awk '!seen[$0]++'
