#!/usr/bin/env bash
# break-it cloud refuses bad requests before creating anything, and deletes the sandbox it created when a later step fails.
set -euo pipefail

script="$(cd "$(dirname "$0")" && pwd)/break-it"
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
git init -q --bare "$dir/origin.git"
git clone -q "$dir/origin.git" "$dir/work" 2> /dev/null
cd "$dir/work"
git config user.email test@example.com
git config user.name test
git commit -q --allow-empty -m base
git branch -q -M main
git tag -a v1 -m v1
git branch -q behind
git branch -q ahead "$(git commit-tree 'HEAD^{tree}' -p HEAD -m unpushed)"
git push -q origin main v1 HEAD:refs/heads/ahead "$(git commit-tree 'HEAD^{tree}' -p HEAD -m later):refs/heads/behind"

mkdir "$dir/bin"
cat > "$dir/bin/sbx" << 'EOF'
#!/usr/bin/env bash
echo "$*" >> "$SBX_LOG"
[[ $2 != exec ]]
EOF
chmod +x "$dir/bin/sbx"
export PATH="$dir/bin:$PATH" SBX_LOG=$dir/sbx.log BREAK_IT_REPO=$dir/origin.git HOME=$dir/home
status=0

# Expects exit 2, `want` in the message, and the sbx calls `calls` matches.
check() {
  local want=$1 calls=$2 got
  shift 2
  : > "$SBX_LOG"
  if got=$("$script" "$@" 2>&1 < /dev/null); then
    printf 'FAIL %s: exited 0\n' "$*" >&2
    status=1
  elif [[ $got != *"$want"* ]]; then
    printf 'FAIL %s: got %q, want %q\n' "$*" "$got" "$want" >&2
    status=1
  elif ! [[ $(paste -sd ';' "$SBX_LOG") =~ ^$calls$ ]]; then
    printf 'FAIL %s: sbx calls %q, want %q\n' "$*" "$(paste -sd ';' "$SBX_LOG")" "$calls" >&2
    status=1
  fi
}

check 'at most 8' '' cloud --ref main --hours 9
check 'at most 8' '' cloud --ref main --hours 0
check 'at most 8' '' inside --hours 9
check 'xl, large or medium' '' cloud --ref main --size huge
check '--ref is required' '' cloud
check 'not a branch or tag on GitHub' '' cloud --ref missing
check 'has commits' '' cloud --ref ahead
check 'pass --yes' '' cloud --ref behind
check 'pass --yes' '' cloud --ref v1
check 'could not clone' '--cloud create --name (break-it-[0-9-]+) .*;--cloud exec \1 git clone .*;--cloud rm --force \1' \
  cloud --ref main --hours 1 --yes --out "$dir/report"

exit "$status"
