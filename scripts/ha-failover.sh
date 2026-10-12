#!/usr/bin/env bash
# Failover check for scripts/ha.sh, all local:
# Litestream's file replica and a files dir stand in for object storage.
#   1. primary A (ha.sh start) runs a task to its approval step;
#   2. a standby B with an empty data dir waits while A's heartbeat is fresh;
#   3. kill -9 of all of A; B restores sun.db and the files, starts Sun;
#   4. the task is there, every blob the restored DB cites exists, and the run
#      resumes on B to done;
#   5. A starts again: it keeps waiting as a standby while B is alive.
# HA_S3=1 runs the same over a local S3 server (rclone serve s3): Litestream's
# s3:// replica and an rclone remote for the files.
# Needs: see scripts/ha-test-lib.sh. Usage: bash scripts/ha-failover.sh
#   (HA_KEEP=1 keeps the temp dir; HA_S3=1 RCLONE_BIN=/path/to/rclone for S3)
source "$(dirname "$0")/ha-test-lib.sh"

# The agent writes a file (a task-folder artifact, so a blob) and takes 2 s.
cat >"$work/agent.jsonl" <<'EOF'
{"kind":"text","role":"assistant","text":"writing notes"}
{"delay_ms":2000,"kind":"file_changed","path":"notes.md","write":"# Notes\n"}
{"kind":"done_claimed","summary":"wrote notes.md"}
EOF
node_config a
mkdir -p "$work/b"

# --- 1. primary A runs a task to its approval step ---
start_node a
wait_up 30
token=$(jq -r .token "$work/a/kernel.json")
wait_for "the local host" 30 'any(.[]; .status == "online")' /hosts
workflow='name: ha
steps:
  - { id: write, type: agent, prompt: "Write notes.md." }
  - { id: approve, type: approval, who: [role:admin] }
  - { id: finish, type: agent, prompt: "Finish." }'
workflow_id=$(api POST /workflows "$(jq -n --arg y "$workflow" '{source_yaml: $y}')" | jq -r .id)
task=$(api POST /tasks "$(jq -n --arg w "$workflow_id" '{title: "HA failover", workflow_id: $w}')" | jq -r .id)
echo "$me: task $task on A"
waiting='any(.run.steps[]?; .step_key == "approve" and .status == "waiting_approval")'
wait_for "the approval step on A" 120 "$waiting" "/tasks/$task"
blobs_a=$(find "$work/a/blobs" -name '*.zst' | wc -l | tr -d ' ')
[ "$blobs_a" -gt 0 ] || fail "A wrote no blobs"

# --- 2. standby B waits while A is alive ---
start_node b
sleep 4
grep -q "standby: the primary" "$work/b.log" || fail "B did not wait as a standby"
[ ! -e "$work/b/sun.db" ] || fail "B restored while A was alive"
doctor=$(SUN_DATA_DIR="$work/b" sun doctor 2>&1 || true)
echo "$doctor" | grep -E '^ok +backup +standby' || fail "sun doctor on standby B: $doctor"

# A file sync that started after the approval state, so the restore point has it.
reached=$(date -u +%Y-%m-%dT%H:%M:%SZ)
for _ in $(seq 1 100); do
  [[ $(ha_get synced-at) > $reached ]] && break
  sleep 0.2
done
[[ $(ha_get synced-at) > $reached ]] || fail "no file sync after $reached"
# A write newer than any file-sync marker (they trail by 5 s), once Litestream
# has uploaded it: B must restore to the marker, without it.
late=$(api POST /tasks "$(jq -n --arg w "$workflow_id" '{title: "after the last sync", workflow_id: $w}')" | jq -r .id)
sleep 2

# --- 3. kill -9 all of A (ha.sh, litestream, sun, the host) ---
kill -9 -- "-$pid_a"
for _ in $(seq 1 50); do up || break; sleep 0.1; done
! up || fail "A still answers after kill -9"
echo "$me: killed A ($blobs_a blobs); B takes over"
wait_up 120

# --- 4. B has the data and every blob, and the run resumes ---
grep -q "restoring the database as of the last file sync" "$work/b.log" || fail "B did not restore to the file-sync marker"
token=$(jq -r .token "$work/b/kernel.json")
api GET "/tasks/$task" | jq -e "$waiting" >/dev/null || fail "the task is not waiting for approval on B"
! api GET "/tasks/$late" >/dev/null 2>&1 || fail "B has the task created after the last file sync"
checked=$(check_cited_blobs b)
[ "$checked" -gt 0 ] || fail "no blob cited by B's database"
echo "$me: ok: B restored the task and all $checked blobs its database cites"
sleep 3 # B's first file sync and Litestream upload
doctor=$(SUN_DATA_DIR="$work/b" sun doctor 2>&1 || true)
echo "$doctor" | grep -E '^ok +backup' || fail "sun doctor on B: $doctor"
api POST "/tasks/$task/approve" '{}' >/dev/null
wait_for "done on B" 120 '.status == "done" and .run.status == "done"' "/tasks/$task"
echo "$me: ok: the run resumed on B to done"

# --- 5. A comes back as a standby ---
start_node a
sleep 4
grep -q "standby: the primary" "$work/a.log" || fail "A did not wait as a standby"
up || fail "B stopped when A came back"
echo "$me: PASS"
