#!/usr/bin/env bash
# Opt-in check of the real Claude Code adapter (uses your quota: one tiny
# prompt): fresh data dir → `sun serve --with-host` → a one-step workflow
# (agent + diff_not_empty gate) on a temp git repo → done. Needs a logged-in
# `claude` (`claude auth status`), cargo, git, curl, jq.
# Usage: bash tests/e2e/claude.sh   (SUN_E2E_KEEP=1 keeps the temp dir)
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/sun-claude.XXXXXX")
pid=""
cleanup() {
  [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  if [ "${SUN_E2E_KEEP:-}" = 1 ]; then echo "claude: kept $work"; else rm -rf "$work"; fi
}
trap cleanup EXIT
fail() { echo "claude: FAIL: $*" >&2; tail -n 30 "$work/sun.log" >&2 || true; exit 1; }

claude auth status --json | jq -e .loggedIn >/dev/null || fail "claude is not logged in"
cargo build -q --manifest-path "$root/Cargo.toml" -p sun-cli --bin sun
sun=$(cargo metadata -q --format-version 1 --no-deps --manifest-path "$root/Cargo.toml" | jq -r .target_directory)/debug/sun

mkdir -p "$work/seed"
echo "# tiny" >"$work/seed/README.md"
git -C "$work/seed" init -q -b main
git -C "$work/seed" add .
git -C "$work/seed" -c user.name=e2e -c user.email=e2e@sun.dev commit -q -m base
git clone -q --bare "$work/seed" "$work/origin.git"

data="$work/data"
mkdir -p "$data"
port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
base="http://127.0.0.1:$port"
printf 'listen: 127.0.0.1:%s\ndefault_runtime: claude-code\n' "$port" >"$data/sun.yaml"
SUN_DATA_DIR="$data" "$sun" serve --with-host >"$work/sun.log" 2>&1 &
pid=$!
for _ in $(seq 1 100); do curl -fsS "$base/api/v1/auth" >/dev/null 2>&1 && break; sleep 0.1; done
token=$(jq -r .token "$data/kernel.json")
api() {
  curl -fsS -X "$1" -H "authorization: Bearer $token" -H 'content-type: application/json' \
    ${3:+--data "$3"} "$base/api/v1$2"
}

workflow='name: tiny
steps:
  - id: implement
    type: agent
    prompt: |
      Create a file named hello.txt whose only content is the line "hello from sun".
      Change nothing else and run no commands. Then call the sun done tool.
  - id: verify
    type: gate
    gates: [diff_not_empty]
'
workflow_id=$(api POST /workflows "$(jq -n --arg y "$workflow" '{source_yaml: $y}')" | jq -r .id)
repo_id=$(api POST /repos "$(jq -n --arg u "$work/origin.git" '{url: $u, default_branch: "main"}')" | jq -r .id)
task=$(api POST /tasks "$(jq -n --arg w "$workflow_id" --arg r "$repo_id" \
  '{title: "Add hello.txt", workflow_id: $w, repo_ids: [$r]}')" | jq -r .id)
echo "claude: task $task (up to 5 min)"
deadline=$((SECONDS + 300))
while [ $SECONDS -lt $deadline ]; do
  status=$(api GET "/tasks/$task" | jq -r .status)
  case "$status" in
    done) break ;;
    failed | cancelled) api GET "/tasks/$task" | jq . >&2; fail "task $status" ;;
  esac
  sleep 2
done
[ "$status" = done ] || fail "timed out (status $status)"
run=$(api GET "/tasks/$task" | jq -r .run.id)
api GET "/runs/$run/events" | jq -r '[.[].kind] | group_by(.) | map("\(.[0])×\(length)") | join(" ")'
api GET "/usage?run_id=$run" | jq -c .
echo "claude: PASS"
