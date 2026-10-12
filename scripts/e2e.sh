#!/usr/bin/env bash
# End-to-end check on the real `sun` binary:
#   fresh data dir → `sun serve --with-host` → the first-run owner signs in
#   and names their email → Main gets a repo (a temp bare origin behind an
#   https URL) → a task on the default workflow's steps with a failover chain
#   → the first fake runtime hits its limit → failover to the second → gates
#   → PR step → the owner approves → done, every step succeeded, the task's
#   branch pushed.
# Then the whole-binary crash test: a second task, `kill -9` of the kernel
# while the agent step runs and again while it waits for approval; each
# restart resumes the run from the journal to done.
#
# Needs: cargo, git, curl, jq. Usage: bash scripts/e2e.sh   (SUN_E2E_KEEP=1 keeps the temp dir)
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/sun-e2e.XXXXXX")
log="$work/sun.log"
pid=""

fail() {
  echo "e2e: FAIL: $*" >&2
  echo "--- last kernel log lines ($log) ---" >&2
  tail -n 40 "$log" >&2 || true
  exit 1
}
cleanup() {
  [ -n "$pid" ] && kill -9 "$pid" 2>/dev/null || true
  # Fake agent sessions of a killed kernel.
  pkill -9 -f "$work/" 2>/dev/null || true
  if [ "${SUN_E2E_KEEP:-}" = 1 ]; then echo "e2e: kept $work"; else rm -rf "$work"; fi
}
trap cleanup EXIT

echo "e2e: building sun, sun-fake-runtime, sun-e2e-mod"
cargo build -q --manifest-path "$root/Cargo.toml" -p sun-cli --bin sun --bin sun-e2e-mod \
  -p sun-runtime --bin sun-fake-runtime
target=$(cargo metadata -q --format-version 1 --no-deps --manifest-path "$root/Cargo.toml" | jq -r .target_directory)/debug
sun="$target/sun"

# --- a tiny Rust crate behind a bare origin (the default test/lint gates run cargo) ---
mkdir -p "$work/seed/src"
printf '[package]\nname = "e2e"\nversion = "0.1.0"\nedition = "2021"\n\n[workspace]\n' >"$work/seed/Cargo.toml"
printf '//! E2E.\n' >"$work/seed/src/lib.rs"
printf 'target/\nCargo.lock\n' >"$work/seed/.gitignore"
git -C "$work/seed" init -q -b main
git -C "$work/seed" add .
git -C "$work/seed" -c user.name=e2e -c user.email=e2e@sun.dev commit -q -m base
git clone -q --bare "$work/seed" "$work/origin.git"
origin="$work/origin.git"
# Repos are added by URL (https, ssh or git): git on the host maps this one
# to the bare origin, so nothing listens on the network.
origin_url="https://git.e2e.invalid/sun/e2e.git"
cat >"$work/gitconfig" <<EOF
[user]
  name = e2e
  email = e2e@sun.dev
[url "$origin"]
  insteadOf = $origin_url
EOF
export GIT_CONFIG_GLOBAL="$work/gitconfig"

# --- fake runtimes: `fake-limit` hits its quota, `fake-ok` does the work ---
reset_at=$(date -u -v+1H +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d '+1 hour' +%Y-%m-%dT%H:%M:%SZ)
cat >"$work/limit.jsonl" <<EOF
{"kind":"text","role":"assistant","text":"starting"}
{"delay_ms":300,"kind":"limit_hit","limit":"quota","reset_at":"$reset_at","raw":"usage limit reached"}
EOF
# The delay is the crash test's window to kill the kernel mid-step.
cat >"$work/ok.jsonl" <<'EOF'
{"kind":"text","role":"assistant","text":"picking up the hand-off"}
{"delay_ms":3000,"kind":"file_changed","path":"src/lib.rs","write":"//! Changed by the agent.\n"}
{"kind":"done_claimed","summary":"changed src/lib.rs"}
EOF

data="$work/data"
mkdir -p "$data"
port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
base="http://127.0.0.1:$port"
cat >"$data/sun.yaml" <<EOF
listen: 127.0.0.1:$port
default_runtime: fake-limit
runtimes:
  fake:
    fake-limit: { binary: "$target/sun-fake-runtime", script: "$work/limit.jsonl" }
    fake-ok: { binary: "$target/sun-fake-runtime", script: "$work/ok.jsonl" }
EOF
export SUN_DATA_DIR="$data"

# The PR step's `github` mod: the network-free fixture, installed like a real one.
mkdir -p "$work/github-mod"
cat >"$work/github-mod/mod.yaml" <<EOF
name: github
kind: plugin
version: 0.1.0
api: ">=1.0 <2.0"
entry: { command: "$target/sun-e2e-mod" }
provides: { actions: [open_pr, reconcile, config] }
EOF
"$sun" plugin add "$work/github-mod" --yes >/dev/null

start_kernel() {
  "$sun" serve --with-host >>"$log" 2>&1 &
  pid=$!
  disown "$pid"
  for _ in $(seq 1 100); do
    curl -fsS "$base/api/v1/auth" >/dev/null 2>&1 && return 0
    kill -0 "$pid" 2>/dev/null || fail "sun serve exited"
    sleep 0.1
  done
  fail "sun serve did not answer on $base"
}
crash_kernel() {
  kill -9 "$pid"
  while kill -0 "$pid" 2>/dev/null; do sleep 0.05; done
  pid=""
  echo "e2e: kill -9 the kernel ($1)"
}

started=$(python3 -c 'import time; print(time.time())')
start_kernel
python3 -c "import time; print('e2e: kernel up in %d ms' % ((time.time()-$started)*1000))"
token=$(jq -r .token "$data/kernel.json")
main_ws=00000000000000000000000001

# Everything below runs as the owner's browser session (a cookie), not the
# kernel token: the first-run owner signs in with it once, chooses a password
# (which retires the printed token) and names a real email, as onboarding
# does. Sessions are stored, so they outlive the kernel restarts.
jar="$work/cookies"
api() { # api METHOD PATH [JSON]
  curl -fsS -b "$jar" -c "$jar" -X "$1" -H 'content-type: application/json' \
    ${3:+--data "$3"} "$base/api/v1$2"
}
api POST /session "$(jq -n --arg p "$token" '{email: "admin@sun.local", password: $p}')" >/dev/null ||
  fail "the first-run owner could not sign in"
api PUT /me/password "$(jq -n --arg p "$token" '{password: "the e2e owner’s password", current_password: $p}')" \
  >/dev/null || fail "the owner could not choose a password"
api PUT /me/email '{"email": "owner@e2e.dev"}' >/dev/null || fail "the owner could not name their email"
api GET /me | jq -e '.is_owner and (.needs_email | not)' >/dev/null || fail "not signed in as the owner: $(api GET /me)"
# wait_for WHAT SECONDS JQ_FILTER PATH: polls GET PATH until the filter is true.
wait_for() {
  local deadline=$((SECONDS + $2)) body=""
  while [ $SECONDS -lt $deadline ]; do
    body=$(api GET "$4" 2>/dev/null || true)
    if [ -n "$body" ] && echo "$body" | jq -e "$3" >/dev/null 2>&1; then return 0; fi
    if [ -n "$body" ] && echo "$body" | jq -e '.status? == "failed" or .status? == "cancelled"' >/dev/null 2>&1; then
      echo "$body" | jq . >&2
      fail "the task ended before $1"
    fi
    sleep 0.2
  done
  echo "$body" | jq . >&2 || true
  fail "timed out waiting for $1"
}

wait_for "the local host" 30 'any(.[]; .status == "online")' /hosts

# The default workflow's steps, with the failover chain in front.
default_yaml=$(api GET /workflows | jq -r '.[] | select(.name == "default") | .id' |
  { read -r id; api GET "/workflows/$id"; } | jq -r .source_yaml)
workflow=$(printf 'runtime:\n  chain: [fake-limit, fake-ok]\n  on_limit: switch\n%s\n' \
  "$(echo "$default_yaml" | sed 's/^name: default$/name: e2e/')")
workflow_id=$(api POST /workflows "$(jq -n --arg y "$workflow" '{source_yaml: $y}')" | jq -r .id)
api POST "/workspaces/$main_ws/repos" "$(jq -n --arg u "$origin_url" '{url: $u, default_branch: "main"}')" |
  jq -e '.repo.id' >/dev/null || fail "could not add the repo"

# A run works in the workspace repos its task names: "e2e" in the title.
new_task() {
  api POST /tasks "$(jq -n --arg t "$1" --arg w "$workflow_id" '{title: $t, workflow_id: $w}')" | jq -r .id
}
# branch_of TASK: the task's branch on the origin (`sun/<KEY>-<slug>`).
branch_of() {
  local key
  key=$(api GET "/tasks/$1" | jq -r .key)
  git -C "$origin" for-each-ref --format='%(refname:short)' "refs/heads/sun/$key-*" "refs/heads/sun/$key" | head -n 1
}
waiting_review='any(.run.steps[]?; .step_key == "review" and .status == "waiting_approval")'
all_done='.status == "done" and .run.status == "done" and all(.run.steps[]; .status == "succeeded")'

# --- 1. failover → gates → PR → approval → done ---
task=$(new_task "E2E failover")
echo "e2e: task $task"
wait_for "the review approval" 300 "$waiting_review" "/tasks/$task"
run=$(api GET "/tasks/$task" | jq -r .run.id)
events=$(api GET "/runs/$run/events")
echo "$events" | jq -e 'any(.[]; .kind == "handoff")' >/dev/null ||
  fail "no handoff event: $(echo "$events" | jq -c '[.[].kind]')"
echo "$events" | jq -e 'any(.[]; .kind == "limit_hit" or (.kind == "agent_event" and .payload.kind? == "limit_hit"))' >/dev/null ||
  echo "e2e: note: no limit_hit event kind found (kinds: $(echo "$events" | jq -c '[.[].kind] | unique'))"
api POST "/tasks/$task/approve" '{}' >/dev/null
wait_for "done" 60 "$all_done" "/tasks/$task"
branch=$(branch_of "$task")
[ -n "$branch" ] || fail "the task's branch was not pushed: $(git -C "$origin" branch --list)"
pushed=$(git -C "$origin" show "$branch:src/lib.rs")
[ "$pushed" = "//! Changed by the agent." ] || fail "the task branch holds: $pushed"
echo "e2e: ok: limit hit → failover → gates → PR → approval → done"

# --- 2. whole-binary crash test ---
task=$(new_task "E2E crash")
echo "e2e: task $task"
wait_for "the agent step on fake-ok" 120 \
  'any(.run.steps[]?; .step_key == "implement" and .status == "running")' "/tasks/$task"
crash_kernel "agent step running"
start_kernel
wait_for "the review approval after the restart" 300 "$waiting_review" "/tasks/$task"
crash_kernel "waiting for approval"
start_kernel
api POST "/tasks/$task/approve" '{}' >/dev/null
wait_for "done after the restarts" 60 "$all_done" "/tasks/$task"
[ -n "$(branch_of "$task")" ] || fail "the crash task's branch was not pushed"
echo "e2e: ok: kill -9 twice, resumed to done"
echo "e2e: PASS"
