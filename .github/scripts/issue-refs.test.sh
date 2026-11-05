#!/usr/bin/env bash
# Table-driven test of issue-refs.sh.
set -euo pipefail

script="$(dirname "$0")/issue-refs.sh"
status=0

check() {
  local mode=$1 body=$2 want=$3 got
  got=$(printf '%s' "$body" | "$script" "$mode" NotPellew/Thalvi | paste -sd ' ')
  if [[ $got != "$want" ]]; then
    printf 'FAIL %s %q: got %q, want %q\n' "$mode" "$body" "$got" "$want" >&2
    status=1
  fi
}

for keyword in close closes closed fix fixes fixed resolve resolves resolved; do
  check closes "$keyword #7" 7
done
check closes 'CLOSES #1, FiXeD: #2' '1 2'
check closes 'Refs #4, prefixes #5' ''
check closes 'Closes other/repo#5, closes notpellew/thalvi#6' 6
check closes $'## Change\n\nFixes #8 and resolves #9.\n\nCloses #8.\n' '8 9'
check closes 'Reverts NotPellew/Thalvi#12' ''
check reverts 'Reverts NotPellew/Thalvi#12' 12
check reverts 'Reverts #13' 13
check reverts 'Reverts other/repo#14, closes #15' ''

exit "$status"
