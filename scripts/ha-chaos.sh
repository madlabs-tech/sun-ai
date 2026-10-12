#!/usr/bin/env bash
# Adversarial checks for scripts/ha.sh, beyond the happy path in
# scripts/ha-failover.sh. Each scenario runs in its own temp dir
# with its own replicas and key:
#   heavy        many large blobs, kill -9 while runs write: the restored DB is
#                whole, cites no missing or torn blob, has nothing newer than
#                the restore point, and every run finishes on the standby
#   chain        A -> B -> C: a second failover after B restored to an older
#                point than A's last upload keeps B's writes, not A's
#   return       a killed primary that starts again waits and never writes
#   freeze       a primary frozen (SIGSTOP) past the lease, then resumed: it
#                stops (exit 3) and its writes never reach the replica
#   race         two standbys start at the same moment: exactly one primary
#   race_fresh   the same on an empty replica (no primary ever ran)
#   key          missing or wrong SUN_MASTER_KEY on the standby: no takeover
#   key_unsealed the wrong key when nothing is sealed yet
#   litestream_crash  only Litestream dies: Sun must not outlive it
#   retention    file sync down past snapshot retention: warns, restores the
#                latest, a missing blob is a clean 404, Sun keeps serving
#   missing_blob blobs the database cites are gone: 404, the run ends, no panic
#   switchover   clean stop of ha.sh (as documented): nothing lost
#   switchover_systemd  the same with SIGTERM to every process (systemd's
#                default KillMode=control-group)
#   key_file     a standby whose data dir holds another install's master.key:
#                SUN_MASTER_KEY wins, so it takes over; the reverse is refused
#   mid_restore  a second standby starts while the first has claimed and is
#                still restoring: it waits, and never takes over
#   mid_restore_slow  the same with a restore slower than lease - fence_wait:
#                the first node's heartbeat must stay fresh meanwhile
#   doctor      `sun doctor`'s backup line: fresh, 10 of 10 ok under writes,
#                lagging replica, stale file sync, standby, and without
#                SUN_DATA_DIR in the environment
# Usage: bash scripts/ha-chaos.sh [scenario...]   (none = all; HA_S3=1 and
# HA_KEEP=1 as in ha-failover.sh). Exits 1 on a failure. A check that hits a
# known bug prints "KNOWN BUG:" and does not fail the run.
if [ $# -ne 1 ]; then # run each scenario in its own process
  all="heavy chain return freeze race race_fresh key key_unsealed key_file mid_restore mid_restore_slow retention missing_blob switchover switchover_systemd litestream_crash doctor"
  failed="" summary=""
  for s in ${*:-$all}; do
    start=$SECONDS
    bash "$0" "$s" 2>&1 | tee "${TMPDIR:-/tmp}/ha-chaos-$s.log"
    status=${PIPESTATUS[0]}
    [ "$status" = 0 ] || failed="$failed $s"
    bugs=$(grep -c "KNOWN BUG:" "${TMPDIR:-/tmp}/ha-chaos-$s.log" || true)
    summary="$summary\n  $s: $([ "$status" = 0 ] && echo pass || echo FAIL), $bugs known bug(s), $((SECONDS - start))s"
    rm -f "${TMPDIR:-/tmp}/ha-chaos-$s.log"
  done
  printf "ha-chaos summary:$summary\n"
  [ -z "$failed" ] || { echo "ha-chaos: FAILED:$failed"; exit 1; }
  exit 0
fi
scenario=$1
source "$(dirname "$0")/ha-test-lib.sh"
me="ha-chaos $scenario"

known_bug() { echo "$me: KNOWN BUG: $*" >&2; }
ok() { echo "$me: ok: $*"; }
lease=$((3 * SUN_FILES_SYNC_INTERVAL + 15))
fence_wait=$((2 * SUN_FILES_SYNC_INTERVAL + 5))

cat >"$work/agent.jsonl" <<'EOF'
{"kind":"text","role":"assistant","text":"writing notes"}
{"delay_ms":500,"kind":"file_changed","path":"notes.md","write":"# Notes\n"}
{"kind":"done_claimed","summary":"wrote notes.md"}
EOF
approval_workflow='name: ha
steps:
  - { id: approve, type: approval, who: [role:admin] }
  - { id: write, type: agent, prompt: "Write notes.md." }'
waiting='any(.run.steps[]?; .step_key == "approve" and .status == "waiting_approval")'

# login NODE: the kernel token of the node now serving.
login() { token=$(jq -r .token "$work/$1/kernel.json"); }
# primary NODE [args]: start NODE as the first primary with a workflow.
primary() {
  node_config "$1"
  start_node "$@"
  wait_up 60
  login "$1"
  wait_for "the local host" 30 'any(.[]; .status == "online")' /hosts
  workflow_id=$(api POST /workflows "$(jq -n --arg y "$approval_workflow" '{source_yaml: $y}')" | jq -r .id)
  covered # a standby restores with the workflow
}
new_task() { # new_task TITLE [WORKFLOW_ID]
  api POST /tasks "$(jq -n --arg t "$1" --arg w "${2:-$workflow_id}" '{title: $t, workflow_id: $w}')" | jq -r .id
}
has_task() { api GET "/tasks/$1" >/dev/null 2>&1; }
kill_node() { kill -9 -- "-$(eval echo "\$pid_$1")" 2>/dev/null || true; }
standby() { # standby NODE [args]: start NODE, check it waits
  mkdir -p "$work/$1"
  start_node "$@"
  sleep 3
  grep -q "standby: the primary" "$work/$1.log" || fail "$1 did not wait as a standby"
}
# A fresh restore of the replica into FILE (what the next standby would get).
restore_replica() { # restore_replica FILE
  mkdir -p "$work/probe"
  SUN_DATA_DIR="$work/probe" litestream restore -config "$LITESTREAM_CONFIG" -o "$1" "$work/probe/sun.db" >>"$work/probe.log" 2>&1
}
titles() { sqlite3 "$1" "SELECT title FROM task ORDER BY title"; }
integrity() { # integrity DB: "ok" and no foreign key violation
  [ "$(sqlite3 "$1" 'PRAGMA integrity_check')" = ok ] || fail "$1 fails integrity_check"
  [ -z "$(sqlite3 "$1" 'PRAGMA foreign_key_check')" ] || fail "$1 has foreign key violations"
}
wait_exit() { # wait_exit NODE [SECONDS]: wait for its ha.sh; its exit code in $status
  local pid
  pid=$(eval echo "\$pid_$1")
  for _ in $(seq 1 $((${2:-120} * 5))); do kill -0 "$pid" 2>/dev/null || break; sleep 0.2; done
  ! kill -0 "$pid" 2>/dev/null || fail "$1 still runs after ${2:-120}s$(up && echo ' and serves')"
  status=0
  wait "$pid" || status=$?
}
# check_missing_blob NODE TASK: the task cites at least one blob missing on
# NODE; reading it is a clean 404, the task loads, approving it runs on, and
# the node neither stops nor panics.
check_missing_blob() {
  local missing=0 hash artifact code
  for hash in $(sqlite3 "$work/$1/sun.db" "SELECT blob FROM artifact"); do
    [ -e "$work/$1/blobs/${hash:0:2}/$hash.zst" ] || missing=$((missing + 1))
  done
  [ "$missing" -gt 0 ] || fail "no blob is missing on $1; the scenario did not bite"
  artifact=$(api GET "/tasks/$2/artifacts" | jq -r '.[0].id')
  code=$(curl -s -o "$work/body" -w '%{http_code}' -H "authorization: Bearer $token" "$base/api/v1/artifacts/$artifact")
  [ "$code" = 404 ] || fail "reading a missing blob answered $code: $(cat "$work/body")"
  ok "a missing blob reads as 404: $(cat "$work/body")"
  api GET "/tasks/$2" >/dev/null || fail "the task with a missing blob does not load"
  api POST "/tasks/$2/approve" '{}' >/dev/null || fail "approving the task with a missing blob failed"
  wait_for "the run to end" 60 '.run.status == "done" or .run.status == "failed"' "/tasks/$2"
  up || fail "$1 stopped serving after reading a missing blob"
  ! grep -q "panicked" "$work/$1.log" || fail "$1 panicked: $(grep -m1 panicked "$work/$1.log")"
  ok "$missing missing blob(s): 404, the run ended $(api GET "/tasks/$2" | jq -r .run.status), no panic"
}
covered() { # wait until a file sync pass covers everything written so far
  # +2 s: Litestream stamps an upload up to its 1 s sync interval after the
  # commit, and a restore to the marker leaves out later stamps.
  local now s=$(($(date +%s) + 2))
  now=$(date -u -r "$s" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d "@$s" +%Y-%m-%dT%H:%M:%SZ)
  for _ in $(seq 1 150); do [[ $(ha_get synced-at) > $now ]] && return 0; sleep 0.2; done
  fail "no file sync covered $now"
}

case $scenario in
heavy)
  # Four runtimes, each writing three distinct ~200 KB files (random, so zstd
  # can't shrink them away) over a few seconds: many large blobs.
  for n in 1 2 3 4; do
    python3 - "$work/agent$n.jsonl" "$n" <<'EOF'
import base64, json, os, sys
with open(sys.argv[1], "w") as f:
    for i in range(3):
        text = base64.b64encode(os.urandom(150_000)).decode()
        f.write(json.dumps({"delay_ms": 400, "kind": "file_changed", "path": f"part{sys.argv[2]}-{i}.txt", "write": text}) + "\n")
    f.write(json.dumps({"kind": "done_claimed", "summary": "wrote parts"}) + "\n")
EOF
  done
  node_config a
  for n in 1 2 3 4; do
    printf '    fake%s: { binary: "%s", script: "%s" }\n' "$n" "$target/sun-fake-runtime" "$work/agent$n.jsonl" >>"$work/a/sun.yaml"
  done
  start_node a
  wait_up 60
  login a
  wait_for "the local host" 30 'any(.[]; .status == "online")' /hosts
  heavy_ids=()
  for n in 1 2 3 4; do
    heavy="name: heavy$n
runtime: { chain: [fake$n] }
steps:
  - { id: one, type: agent, prompt: \"Part one.\" }
  - { id: two, type: agent, prompt: \"Part two.\" }"
    heavy_ids+=("$(api POST /workflows "$(jq -n --arg y "$heavy" '{source_yaml: $y}')" | jq -r .id)")
  done
  workflow_id=$(api POST /workflows "$(jq -n --arg y "$approval_workflow" '{source_yaml: $y}')" | jq -r .id)
  covered
  mkdir -p "$work/b"
  standby b
  tasks=()
  for i in 0 1 2 3 4 5 6 7; do tasks+=("$(new_task "heavy $i" "${heavy_ids[$((i % 4))]}")"); done
  # Keep writing rows (tasks that wait for approval) through the kill.
  (while true; do new_task "filler $RANDOM" >/dev/null 2>&1 || true; sleep 0.3; done) &
  writer=$!
  for _ in $(seq 1 300); do
    [ "$(find "$work/a/blobs" -name '*.zst' 2>/dev/null | wc -l)" -ge 20 ] && break
    sleep 0.2
  done
  [ "$(find "$work/a/blobs" -name '*.zst' | wc -l)" -ge 20 ] || fail "A wrote too few blobs"
  covered # the restore point has heavy runs mid-flight...
  for i in 8 9 10 11; do tasks+=("$(new_task "heavy $i" "${heavy_ids[$((i % 4))]}")"); done
  sleep 1.5 # ...and A is writing more blobs and rows when it dies
  blobs_a=$(find "$work/a/blobs" -name '*.zst' | wc -l | tr -d ' ')
  kill_node a
  kill "$writer" 2>/dev/null || true
  marker=$(ha_get synced-at)
  ok "killed A mid-write with $blobs_a blobs, $(du -sh "$work/a/blobs" | cut -f1); files synced at $marker"
  wait_up 180
  login b
  integrity "$work/b/sun.db"
  checked=$(check_cited_blobs b)
  [ "$checked" -ge 10 ] || fail "B's database cites only $checked blobs; the kill came too early to test much"
  ok "B's database is whole and all $checked blobs it cites exist"
  for f in $(find "$work/b/blobs" -type f); do
    name=$(basename "$f")
    [[ $name =~ ^[0-9a-f]{64}\.zst$ ]] || fail "a stray file in B's blobs: $f"
    [ "$(zstd -dc "$f" | shasum -a 256 | cut -d' ' -f1)" = "${name%.zst}" ] || fail "torn blob on B: $f"
  done
  ok "every blob file on B is whole and matches its hash"
  newest=$(sqlite3 "$work/b/sun.db" 'SELECT max(at) FROM event')
  if grep -q "restoring the latest database" "$work/b.log"; then
    ok "B restored the latest backup (nothing was written after the marker)"
  else
    [[ ! $newest > $marker ]] || fail "B's newest event ($newest) is after the restore point ($marker)"
    ok "nothing on B is newer than the restore point ($newest <= $marker)"
  fi
  resumed=0
  for t in "${tasks[@]}"; do
    has_task "$t" || continue # created after the restore point
    wait_for "heavy task $t done on B" 240 '.status == "done"' "/tasks/$t"
    resumed=$((resumed + 1))
  done
  [ "$resumed" -gt 0 ] || fail "B has none of the heavy tasks"
  ok "all $resumed heavy runs B has resumed to done"
  ;;

chain)
  primary a
  standby b
  first=$(new_task "before the first failover")
  covered
  # Many transactions after the last file sync, all uploaded by Litestream:
  # B restores without them, then writes fewer transactions than A did, so
  # B's TXIDs end inside the range A already uploaded.
  late=$(new_task "A after its last file sync")
  for i in $(seq 1 10); do new_task "A late $i" >/dev/null; done
  sleep 2
  kill_node a
  wait_up 120
  login b
  has_task "$first" || fail "B lost the task from before the first failover"
  ! has_task "$late" || fail "B has A's task from after the last file sync"
  on_b=$(new_task "written on B")
  standby c
  covered
  kill_node b
  wait_up 120
  login c
  integrity "$work/c/sun.db"
  has_task "$first" || fail "C lost the task from before the first failover"
  has_task "$on_b" || fail "C lost the task written on B"
  if has_task "$late" || titles "$work/c/sun.db" | grep -q "^A late"; then
    fail "C resurrected A's tasks that B never had"
  fi
  ok "a second failover keeps B's writes and none of A's abandoned ones"
  ;;

return)
  primary a
  standby b
  kill_node a
  wait_up 120
  login b
  epoch_b=$(ha_get epoch)
  before=$(shasum -a 256 "$work/a/sun.db" | cut -d' ' -f1)
  starts=$(grep -c "replicating to" "$work/a.log" || true)
  start_node a
  sleep $((lease + 10)) # longer than the lease: A must keep waiting while B beats
  grep -q "standby: the primary" "$work/a.log" || fail "A did not wait as a standby"
  [ "$(ha_get epoch)" = "$epoch_b" ] || fail "A took the epoch from B"
  [ "$(shasum -a 256 "$work/a/sun.db" | cut -d' ' -f1)" = "$before" ] || fail "A changed its old sun.db"
  [ "$(grep -c "replicating to" "$work/a.log" || true)" = "$starts" ] || fail "A started Litestream"
  up || fail "B stopped"
  task=$(new_task "after A came back")
  has_task "$task" || fail "B does not serve writes"
  ok "the returning A waits, never writes, and B keeps serving"
  ;;

freeze)
  port_b=$(free_port)
  primary a
  standby b --with-host --listen "127.0.0.1:$port_b"
  kill -STOP -- "-$pid_a"
  ok "froze A; B takes over after the lease"
  base_a=$base base="http://127.0.0.1:$port_b"
  wait_up 180
  login b
  on_b=$(new_task "written on B")
  sleep 3
  # A resumes and gets a write before it notices B's claim.
  kill -CONT -- "-$pid_a"
  base=$base_a token=$(jq -r .token "$work/a/kernel.json")
  zombie=$(new_task "written on the resumed A" 2>/dev/null || true)
  wait_exit a
  [ "$status" = 3 ] || fail "the resumed A exited $status, not 3"
  grep -q "another node claimed the primary epoch" "$work/a.log" || fail "A did not say why it stopped"
  ok "the resumed A stopped with exit 3${zombie:+ (it accepted one write first)}"
  base="http://127.0.0.1:$port_b"
  login b
  sleep 3
  restore_replica "$work/after.db" || fail "the replica no longer restores after A resumed"
  integrity "$work/after.db"
  restored=$(titles "$work/after.db")
  grep -qx "written on B" <<<"$restored" || fail "the replica lost B's write after A resumed"
  if grep -qx "written on the resumed A" <<<"$restored"; then
    known_bug "the resumed old primary's write reached the replica B owns"
  else
    ok "nothing the resumed A wrote reached the replica"
  fi
  ;;

race | race_fresh)
  port_c=$(free_port)
  if [ "$scenario" = race ]; then
    primary a
    task=$(new_task "before the race")
    covered
    kill_node a
    # The template's sun.yaml says $port; C overrides it.
  fi
  mkdir -p "$work/b" "$work/c"
  [ "$scenario" = race ] || node_config b
  [ "$scenario" = race ] || node_config c
  start_node b
  start_node c --with-host --listen "127.0.0.1:$port_c"
  base_c="http://127.0.0.1:$port_c" both=0
  deadline=$((SECONDS + lease + 90))
  while [ $SECONDS -lt $deadline ]; do
    if up && up "$base_c"; then both=$((both + 1)); fi
    sleep 0.2
  done
  serving=0
  up && serving=$((serving + 1))
  up "$base_c" && serving=$((serving + 1))
  [ "$serving" = 1 ] || fail "$serving nodes serve after the race"
  for n in b c; do
    if ! kill -0 "$(eval echo "\$pid_$n")" 2>/dev/null; then
      loser=$n && wait_exit "$n"
    fi
  done
  if [ -z "${loser:-}" ]; then
    # One claimed first and the other saw its fresh heartbeat: still a standby.
    loser=$(up && echo c || echo b)
    ! grep -q "claimed the primary epoch" "$work/$loser.log" || fail "neither node exited, and both claimed"
    status=3
    ok "$loser saw the other's claim in time and stays a standby"
  fi
  # rclone serve s3 refuses one of two PUTs of the same key at once ("corrupted
  # on transfer"): that ha.sh exits 1 in its claim. Real S3 takes both.
  [ "$status" = 3 ] || { [ "$status" = 1 ] && grep -q "Failed to rcat" "$work/$loser.log"; } ||
    fail "the losing node $loser exited $status, not 3"
  [ "$both" = 0 ] || fail "both nodes served at once for ~$((both / 5))s before $loser stopped"
  ok "never two nodes serving at once"
  winner=$([ "$loser" = b ] && echo c || echo b)
  [ "$winner" = c ] && base=$base_c
  login "$winner"
  if [ "$scenario" = race ]; then
    has_task "$task" || fail "the winner lost the task from before the race"
  else
    workflow_id=$(api POST /workflows "$(jq -n --arg y "$approval_workflow" '{source_yaml: $y}')" | jq -r .id)
  fi
  mine=$(new_task "written by the winner")
  sleep 4
  restore_replica "$work/after.db" || fail "the replica does not restore after the race"
  integrity "$work/after.db"
  grep -qx "written by the winner" <<<"$(titles "$work/after.db")" || fail "a restore of the replica lacks the winner's write"
  ok "exactly one primary: $winner (the other exited 3)"
  ;;

key)
  primary a
  api PUT /secrets/ecosystem/00000000000000000000000000/API_TOKEN '{"value":"ha-key-check-value-42"}' >/dev/null
  covered
  kill_node a
  # Missing key: refuses before Litestream or Sun start.
  mkdir -p "$work/b"
  SUN_MASTER_KEY='' start_node b
  wait_exit b
  grep -q "set SUN_MASTER_KEY" "$work/b.log" || fail "B with no key did not say why: $(tail -3 "$work/b.log")"
  [ "$status" != 0 ] || fail "B with no key exited 0"
  ! up || fail "B with no key serves"
  ! grep -q "replicating to" "$work/b.log" || fail "B with no key started Litestream"
  ! grep -q "claimed the primary epoch" "$work/b.log" || fail "the standby with no key claimed the primary epoch (and fenced the old primary) before refusing"
  ok "a standby with no SUN_MASTER_KEY refuses (exit $status) without claiming"
  # Wrong key: Sun refuses to open the secrets; nothing serves.
  mkdir -p "$work/c"
  SUN_MASTER_KEY=$(openssl rand -hex 32) start_node c
  wait_exit c $((lease + 60))
  ! up || fail "C with the wrong key serves"
  grep -qi "master key\|crypto\|decrypt" "$work/c.log" || fail "C with the wrong key did not say why: $(tail -5 "$work/c.log")"
  ! grep -q "ha-key-check-value-42" "$work/c.log" || fail "C printed the secret"
  ok "a standby with the wrong key refuses to start Sun (exit $status)"
  # The right key still takes over, with the secret.
  mkdir -p "$work/d"
  start_node d
  wait_up 180
  login d
  api GET /secrets | jq -e 'any(.[]; .name == "API_TOKEN")' >/dev/null || fail "D lost the secret"
  ok "a standby with the right key takes over afterwards"
  ;;

key_unsealed)
  # The primary holds no sealed value yet (no secret, no authenticator).
  primary a
  new_task "a task, no secret" >/dev/null
  covered
  sqlite3 "$work/a/sun.db" "SELECT count(*) FROM secret" | grep -qx 0 || fail "A already has secrets"
  kill_node a
  mkdir -p "$work/b"
  SUN_MASTER_KEY=$(openssl rand -hex 32) start_node b
  # wait_up fails (exits) when B never serves: that is the pass, so keep it in a subshell.
  ! (wait_up 120 2>/dev/null) || fail "a standby with the wrong SUN_MASTER_KEY took over: nothing told it apart"
  grep -q "not the key this data dir was first opened with" "$work/b.log" ||
    fail "B with the wrong key did not say why: $(tail -3 "$work/b.log")"
  ok "a standby with the wrong key refused: $(grep -iE 'key|crypto' "$work/b.log" | tail -1)"
  ;;

key_file)
  primary a
  api PUT /secrets/ecosystem/00000000000000000000000000/API_TOKEN '{"value":"ha-key-file-value-42"}' >/dev/null
  covered
  kill_node a
  # Another install's master.key in the standby's data dir: the environment wins.
  mkdir -p "$work/b"
  openssl rand -hex 32 >"$work/b/master.key"
  chmod 600 "$work/b/master.key"
  start_node b
  wait_up 180
  login b
  api GET /secrets | jq -e 'any(.[]; .name == "API_TOKEN")' >/dev/null || fail "B lost the secret"
  ok "a stale master.key beside the right SUN_MASTER_KEY: B took over with the secret"
  kill_node b
  # The right key only in the file, a wrong one in the environment: refused.
  mkdir -p "$work/c"
  printf '%s\n' "$SUN_MASTER_KEY" >"$work/c/master.key"
  chmod 600 "$work/c/master.key"
  SUN_MASTER_KEY=$(openssl rand -hex 32) start_node c
  wait_exit c $((lease + 60))
  ! up || fail "C with the wrong SUN_MASTER_KEY (right master.key) serves"
  grep -qi "master key\|crypto\|decrypt" "$work/c.log" || fail "C did not say why: $(tail -3 "$work/c.log")"
  ! grep -q "ha-key-file-value-42" "$work/c.log" || fail "C printed the secret"
  ok "the right master.key under a wrong SUN_MASTER_KEY is refused (exit $status)"
  ;;

mid_restore | mid_restore_slow)
  slow=""
  if [ "$scenario" = mid_restore_slow ]; then
    # The first `litestream restore` (B's) sleeps past lease - fence_wait: a
    # claim stamped once would be stale before B's sync loop starts.
    slow=$((lease - fence_wait + 5))
    real=${LITESTREAM_BIN:-$(command -v litestream)}
    rm -f "$work/bin/litestream"
    printf '#!/bin/sh\n[ "$1" = restore ] && [ ! -e "%s/slow-used" ] && touch "%s/slow-used" && sleep %s\nexec "%s" "$@"\n' \
      "$work" "$work" "$slow" "$real" >"$work/bin/litestream"
    chmod +x "$work/bin/litestream"
  fi
  primary a
  task=$(new_task "before the failover")
  covered
  kill_node a
  mkdir -p "$work/b"
  start_node b
  for _ in $(seq 1 300); do grep -q "claimed the primary epoch" "$work/b.log" && break; sleep 0.2; done
  grep -q "claimed the primary epoch" "$work/b.log" || fail "B never claimed"
  claimed=$SECONDS
  ! up || fail "B serves before its fence wait ended"
  # C starts while B is fenced/restoring: B's claim is its first heartbeat.
  port_c=$(free_port)
  standby c --with-host --listen "127.0.0.1:$port_c"
  wait_up $((${slow:-0} + 120))
  [ -z "$slow" ] || [ $((SECONDS - claimed)) -gt $((lease - fence_wait)) ] ||
    fail "B served $((SECONDS - claimed))s after its claim: the restore was not slow enough to bite"
  login b
  has_task "$task" || fail "B lost the task"
  sleep $((lease + 5))
  ! up "http://127.0.0.1:$port_c" || fail "C serves beside B"
  ! grep -q "claimed the primary epoch" "$work/c.log" || fail "C claimed while B was restoring"
  up || fail "B stopped"
  [ "$(ha_get epoch)" = "$(cat "$work/b/ha-epoch")" ] || fail "the replica's epoch is not B's: $(ha_get epoch)"
  ok "a standby that starts mid-restore keeps waiting; B serves alone${slow:+ after a ${slow}s restore}"
  ;;

retention)
  [ "${HA_S3:-}" != 1 ] || fail "retention runs on the dir replica only"
  # An rsync that fails while $work/sync-down exists: the file sync outage.
  printf '#!/bin/sh\n[ -e "%s/sync-down" ] && echo "rsync: simulated outage" >&2 && exit 23\nexec "%s" "$@"\n' \
    "$work" "$(command -v rsync)" >"$work/bin/rsync"
  chmod +x "$work/bin/rsync"
  primary a
  write_workflow=$(api POST /workflows "$(jq -n '{source_yaml: "name: w\nsteps:\n  - { id: write, type: agent, prompt: \"Write notes.md.\" }\n  - { id: approve, type: approval, who: [role:admin] }"}')" | jq -r .id)
  task1=$(new_task "covered by the last file sync" "$write_workflow")
  wait_for "task 1 waiting" 60 "$waiting" "/tasks/$task1"
  covered
  # The file sync breaks...
  touch "$work/sync-down"
  sleep 3
  # ...for longer than snapshot retention: the marker predates every backup.
  ha_put synced-at "2000-01-01T00:00:00Z"
  # A new blob (another file content) that the broken sync can't copy.
  sed 's/# Notes/# Notes while the sync is down/' "$work/agent.jsonl" >"$work/agent2.jsonl"
  cp "$work/agent2.jsonl" "$work/agent.jsonl"
  task2=$(new_task "written while the file sync was down" "$write_workflow")
  wait_for "task 2 waiting" 60 "$waiting" "/tasks/$task2"
  sleep 3
  grep -q "file sync failed" "$work/a.log" || fail "A did not log the failed file sync"
  standby b
  kill_node a
  rm "$work/sync-down"
  wait_up 180
  login b
  grep -q "WARNING: no backup at" "$work/b.log" || fail "B did not warn about the fallback: $(grep '^ha: restoring' "$work/b.log")"
  has_task "$task1" || fail "B lost task 1: $(grep '^ha: ' "$work/b.log" | tail -3)"
  has_task "$task2" || fail "B did not restore the latest backup"
  check_missing_blob b "$task2"
  ok "B warned and restored the latest backup"
  ;;

missing_blob)
  # A blob the database cites is gone (what the retention fallback leaves).
  primary a
  write_workflow=$(api POST /workflows "$(jq -n '{source_yaml: "name: w\nsteps:\n  - { id: write, type: agent, prompt: \"Write notes.md.\" }\n  - { id: approve, type: approval, who: [role:admin] }\n  - { id: again, type: agent, prompt: \"Again.\" }"}')" | jq -r .id)
  task=$(new_task "its blobs go missing" "$write_workflow")
  wait_for "the task waiting" 60 "$waiting" "/tasks/$task"
  for hash in $(sqlite3 "$work/a/sun.db" .dump | grep -oE '[0-9a-f]{64}' | sort -u); do
    rm -f "$work/a/blobs/${hash:0:2}/$hash.zst"
  done
  check_missing_blob a "$task"
  ;;

switchover | switchover_systemd)
  primary a
  standby b
  sleep 6 # idle, so only the last write is recent
  last=$(new_task "written right before the stop")
  sleep 1.5 # Litestream uploads it (1 s sync interval)
  if [ "$scenario" = switchover ]; then kill -TERM "$pid_a"; else kill -TERM -- "-$pid_a"; fi
  wait_exit a
  [ "$status" = 0 ] || fail "A exited $status on a clean stop, not 0"
  ok "A stopped (exit $status)"
  [ -z "$(ha_get alive)" ] || fail "A left its heartbeat on a clean stop"
  wait_up 60
  login b
  grep '^ha: restoring' "$work/b.log" || true
  has_task "$last" || fail "planned switchover lost the write made 1.5 s before the stop ($(grep '^ha: restoring' "$work/b.log"))"
  ok "planned switchover lost nothing"
  ;;

litestream_crash)
  # Only Litestream dies (a crash, the OOM killer): Sun, its child, must not
  # keep serving unreplicated writes beside the standby that takes over.
  port_b=$(free_port)
  primary a
  standby b --with-host --listen "127.0.0.1:$port_b"
  kill -9 "$(pgrep -g "$pid_a" -x litestream)"
  wait_exit a 30
  [ "$status" != 0 ] || fail "A's ha.sh exited 0 after Litestream died: a crash is not a clean stop"
  ! up || fail "A's sun serve outlived Litestream and ha.sh (orphaned) while a standby waits"
  ok "A's ha.sh exited $status after Litestream died, and stopped Sun with it"
  [ -n "$(ha_get alive)" ] || fail "A cleared its heartbeat after a crash (a restart should keep its database)"
  wait_up 120 "http://127.0.0.1:$port_b"
  ok "B took over after A's heartbeat went stale"
  ;;

doctor)
  primary a
  sleep 3
  line() { SUN_DATA_DIR="$work/$1" sun doctor 2>&1 | grep -E '^(ok|FAIL) +backup' || true; }
  fresh=$(line a)
  grep -qE '^ok +backup +replica at txid ([0-9a-f]+) \(local \1\), last upload .*; files synced [0-9]+s ago' <<<"$fresh" ||
    fail "fresh primary: $fresh"
  ok "fresh primary: $fresh"
  # Healthy, under steady writes: a write Litestream has not shipped yet is
  # not lag (REPLICA_GRACE), so every call says ok.
  (while true; do new_task "under writes $RANDOM" >/dev/null 2>&1 || true; sleep 0.3; done) &
  writer=$!
  oks=0
  for _ in $(seq 1 10); do grep -qE '^ok +backup' <<<"$(line a)" && oks=$((oks + 1)); done
  kill "$writer" 2>/dev/null || true
  [ "$oks" = 10 ] || fail "sun doctor on a healthy primary under writes: $oks/10 ok (last: $(line a))"
  ok "10 of 10 ok under writes"
  # Litestream stalls: the replica lags the local TXID.
  litestream_pid=$(pgrep -g "$pid_a" -x litestream || pgrep -P "$pid_a" litestream)
  kill -STOP "$litestream_pid"
  new_task "while Litestream is stalled" >/dev/null
  sleep 6 # past doctor's grace for a write still on its way to the replica
  lag=$(line a)
  kill -CONT "$litestream_pid"
  grep -qE '^FAIL +backup +replica at txid' <<<"$lag" || fail "sun doctor says ok while Litestream is stalled and a write is unreplicated: $lag"
  ok "lagging replica: $lag"
  # The file sync stalls: its marker ages past 5 minutes.
  loop_pid=$(pgrep -P "$pid_a" -x bash | head -n 1)
  kill -STOP "$loop_pid"
  touch -t 202001010000 "$work/a/ha-files-synced"
  stale=$(line a)
  kill -CONT "$loop_pid"
  grep -qE '^FAIL +backup +.*files synced [0-9]{6,}s ago' <<<"$stale" || fail "stale file sync: $stale"
  ok "stale file sync: $stale"
  # A standby with no database yet.
  standby b
  waiting_line=$(line b)
  grep -qE '^ok +backup +standby: no local database yet' <<<"$waiting_line" || fail "standby: $waiting_line"
  ok "standby: $waiting_line"
  # The admin's shell often lacks the ha.env variables; --data-dir alone
  # should still check the right database.
  sleep 2
  plain=$(env -u SUN_DATA_DIR sun --data-dir "$work/a" doctor 2>&1 | grep -E '^(ok|FAIL) +backup' || true)
  grep -qE '^ok +backup +replica' <<<"$plain" || fail "sun doctor --data-dir on a healthy primary, without SUN_DATA_DIR set: $plain"
  ok "--data-dir without SUN_DATA_DIR: $plain"
  ;;

*) fail "unknown scenario $scenario" ;;
esac
echo "$me: PASS"
