#!/usr/bin/env bash
# Rebuilds this fixture: a data dir written by the last commit before
# Postgres was dropped (ec08176, sqlx::Any), read by tests/it/upgrade.rs.
#   1. git archive ec08176 | tar -x -C OLD && (cd OLD && cargo build -p sun-cli --bin sun -p sun-runtime --bin sun-fake-runtime)
#   2. bash make.sh OLD/target/debug
# Writes sun.db.zst, blobs/ and expected.json (with the test-only master key)
# next to this script.
# Needs curl, jq, sqlite3, zstd and python3.
set -euo pipefail
bin=$(cd "$1" && pwd)
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/sun-pre-t2.XXXXXX")
trap 'kill "$pid" 2>/dev/null || true; rm -rf "$work"' EXIT

port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
base="http://127.0.0.1:$port/api/v1"
data=$work/data
mkdir -p "$data"
cat >"$work/agent.jsonl" <<'EOF'
{"kind":"text","role":"assistant","text":"writing notes"}
{"kind":"file_changed","path":"notes.md","write":"# Notes from before T2\n"}
{"kind":"done_claimed","summary":"wrote notes.md"}
EOF
cat >"$data/sun.yaml" <<EOF
listen: 127.0.0.1:$port
default_runtime: fake
runtimes:
  fake:
    fake: { binary: "$bin/sun-fake-runtime", script: "$work/agent.jsonl" }
EOF
env -u SUN_MASTER_KEY -u SUN_DATABASE_URL "$bin/sun" --data-dir "$data" serve --with-host --no-devices >"$work/sun.log" 2>&1 &
pid=$!
for _ in $(seq 1 300); do curl -fsS "http://127.0.0.1:$port/healthz" >/dev/null 2>&1 && break; sleep 0.1; done
token=$(jq -r .token "$data/kernel.json")
api() { # api METHOD PATH [JSON [TOKEN]]
  local out
  out=$(curl -sS --fail-with-body -X "$1" -H "authorization: Bearer ${4:-$token}" \
    -H 'content-type: application/json' ${3:+--data "$3"} "$base$2") || { echo "$1 $2: $out" >&2 && return 1; }
  printf '%s' "$out"
}
for _ in $(seq 1 100); do api GET /hosts | jq -e 'any(.[]; .status == "online")' >/dev/null && break; sleep 0.2; done

workflow=$(api POST /workflows "$(jq -n '{source_yaml: "name: notes\nsteps:\n  - { id: write, type: agent, prompt: \"Write notes.md.\" }"}')" | jq -r .id)
task=$(api POST /tasks "$(jq -n --arg w "$workflow" '{title: "Written before T2", workflow_id: $w}')" | jq -r .id)
for _ in $(seq 1 300); do api GET "/tasks/$task" | jq -e '.status == "done"' >/dev/null && break; sleep 0.2; done
api GET "/tasks/$task" | jq -e '.status == "done"' >/dev/null || { tail -20 "$work/sun.log"; exit 1; }

group=$(api POST /groups '{"name":"Reviewers","permissions":["run"]}' | jq -r .id)
member=$(api POST /members "$(jq -n --arg g "$group" '{email:"ana@example.com",username:"ana",password:"correct-horse-battery-9",must_change:false,group_ids:[$g]}')" | jq -r .id)
api PUT /secrets/workspace/00000000000000000000000000/API_TOKEN '{"value":"pre-t2-secret-value-42"}' >/dev/null

# Ana adds an authenticator (a TOTP secret sealed with the master key).
curl -sS --fail-with-body -c "$work/jar" -H 'content-type: application/json' --data '{"email":"ana@example.com","password":"correct-horse-battery-9"}' "$base/session"
session=$(awk '$6 == "sun_token" {print $7}' "$work/jar")
started=$(api POST /me/two_step/totp '{"label":"Phone"}' "$session")
totp=$(echo "$started" | jq -r .secret)
totp_hex=$(python3 -c 'import base64, sys; print(base64.b32decode(sys.argv[1] + "=" * (-len(sys.argv[1]) % 8)).hex())' "$totp")
code=$(python3 -c '
import base64, hmac, struct, sys, time
key = base64.b32decode(sys.argv[1] + "=" * (-len(sys.argv[1]) % 8))
mac = hmac.new(key, struct.pack(">Q", int(time.time()) // 30), "sha1").digest()
o = mac[-1] & 15
print("%06d" % ((struct.unpack(">I", mac[o:o + 4])[0] & 0x7FFFFFFF) % 1000000))' "$totp")
factor=$(api POST /me/two_step/totp/confirm "$(echo "$started" | jq --arg c "$code" '{id, code: $c}')" "$session" | jq -r .id)

kill "$pid" && wait "$pid" || true
sqlite3 "$data/sun.db" 'PRAGMA wal_checkpoint(TRUNCATE);' >/dev/null
rm -rf "$here/blobs" && cp -R "$data/blobs" "$here/blobs"
zstd -q -19 -f "$data/sun.db" -o "$here/sun.db.zst"
jq -n --arg task "$task" --arg group "$group" --arg member "$member" --arg factor "$factor" --arg totp "$totp_hex" --arg key "$(cat "$data/master.key")" \
  --argjson blobs "$(find "$data/blobs" -name '*.zst' | wc -l)" \
  '{task: $task, group: $group, member: $member, factor: $factor, totp: $totp, blobs: $blobs, master_key: $key}' >"$here/expected.json"
cat "$here/expected.json"
