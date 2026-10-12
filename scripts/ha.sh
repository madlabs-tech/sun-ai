#!/usr/bin/env bash
# Active-passive high availability for a SQLite Sun.
# Litestream replicates sun.db; this script copies the files beside it,
# restores a standby and keeps one primary at a time.
#
#   ha.sh start [sun serve args]  restore if needed, then run `sun serve` (default
#                                 --with-host) under `litestream replicate`, plus
#                                 the file sync loop. On a standby it first waits
#                                 until the primary's heartbeat stops.
#   ha.sh sync-files              one file sync pass
#
# Env:
#   SUN_DATA_DIR             the data dir (required)
#   SUN_FILES_REPLICA        an absolute dir, or an rclone remote such as
#                            s3:my-bucket/sun/files (required)
#   LITESTREAM_CONFIG        default /etc/litestream.yml (packaging/litestream.yml)
#   SUN_FILES_SYNC_INTERVAL  seconds between file sync passes, default 10
#   SUN_MASTER_KEY           the same key on every node (master.key is not copied)
# Needs litestream (and rclone for a remote) on PATH; sun too, for start.
# Exit 3: another node is primary now; this one stopped (start it again to
# make it a standby). Exit 1: refused to start (no key, or no backup where
# one must exist), or Litestream died under a running Sun (Sun is stopped;
# a restart keeps this node's database). A stop by SIGTERM exits 0.
set -euo pipefail

data=${SUN_DATA_DIR:?set SUN_DATA_DIR}
files=${SUN_FILES_REPLICA:?set SUN_FILES_REPLICA}
config=${LITESTREAM_CONFIG:-/etc/litestream.yml}
interval=${SUN_FILES_SYNC_INTERVAL:-10}
# A primary's heartbeat older than this means it is gone.
lease=$((3 * interval + 15))
# How long a new primary waits after its claim, so the old one sees it and stops.
fence_wait=$((2 * interval + 5))
db=$data/sun.db
# The claim's heartbeat helper (claim), until the sync loop takes over (start).
heartbeat=""
trap 'kill "$heartbeat" 2>/dev/null || true' EXIT # || true: set -e must not turn the exit code into 1

# What a standby needs beside sun.db: the blobs, what an ecosystem export
# carries (ARCHIVED_PATHS in crates/sun-store/src/export.rs) and the ecosystem's
# identity. Not master.key: give every node SUN_MASTER_KEY, so the key never
# sits in the bucket next to the secrets it opens.
synced="blobs sun.yaml mods mod-data skills mcp_servers mcp catalog kernel.json license publisher.key host/host.json"
private="kernel.json license publisher.key host/host.json"

log() { echo "ha: $*" >&2; }

# The files replica is a local (or mounted) dir, else an rclone remote. Its
# ha/ folder holds epoch (who is primary), alive (their heartbeat) and
# synced-at (the restore point the copied files cover).
is_dir() { [[ $files == /* ]]; }
reachable() { if is_dir; then mkdir -p "$files/ha"; else rclone mkdir "$files"; fi; }
get() { if is_dir; then cat "$files/ha/$1" 2>/dev/null; else rclone cat "$files/ha/$1" 2>/dev/null; fi; }
put() { # put NAME VALUE (the temp name is per process: two nodes may claim at once)
  if is_dir; then
    printf '%s\n' "$2" >"$files/ha/$1.tmp.$$" && mv "$files/ha/$1.tmp.$$" "$files/ha/$1"
  else
    printf '%s\n' "$2" | rclone rcat "$files/ha/$1"
  fi
}

# UTC RFC 3339, OFFSET seconds from now (GNU date, then BSD date).
stamp() {
  local s=$(($(date +%s) + $1))
  date -u -d "@$s" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -r "$s" +%Y-%m-%dT%H:%M:%SZ
}

# sync_files [OFFSET]: one pass. Blobs are put before the row that cites them
# commits, so every row committed before this pass started cites a blob this
# pass copies: the start (minus 5 s of clock slew) is a restore point with no
# missing blob. The final pass of a clean stop passes OFFSET 0: Litestream has
# exited, so nothing newer than now can follow. Each step must succeed before
# synced-at moves.
# ponytail: copies never delete and rclone lists the remote each pass; add
# --max-age/--no-traverse when the blob count makes a pass slow.
sync_files() {
  local started paths="" p
  started=$(stamp "${1:--5}") || return 1
  for p in $synced; do [[ -e $data/$p ]] && paths="$paths $p"; done
  if [[ -n $paths ]]; then
    if is_dir; then
      # shellcheck disable=SC2086 # word splitting is the point
      (cd "$data" && rsync -aR --exclude '*.tmp*' $paths "$files/") || return 1
    else
      local filters=(--filter '- *.tmp*')
      for p in $synced; do filters+=(--filter "+ /$p" --filter "+ /$p/**"); done
      rclone copy "$data" "$files" "${filters[@]}" --filter '- *' || return 1
    fi
  fi
  put synced-at "$started" || return 1
  touch "$data/ha-files-synced" # its age is what `sun doctor` reports
}

pull_files() {
  if is_dir; then
    rsync -a --exclude '/ha/' "$files/" "$data/"
  else
    rclone copy "$files" "$data" --filter '- /ha/**'
  fi
  local p
  for p in $private; do [[ -e $data/$p ]] && chmod 600 "$data/$p"; done
  return 0
}

# Claims the primary epoch, then waits so every other node sees it: an old
# primary stops, and a node that claimed at the same moment loses to whichever
# claim landed last (exit 3). A claim is only final after that check.
claim() {
  local epoch
  epoch="$(hostname)-$(date -u +%Y%m%dT%H%M%SZ)-$$"
  mkdir -p "$data"
  put epoch "$epoch"
  put alive "$(stamp 0)"
  # Keep it fresh until the sync loop starts: a restore slower than
  # lease - fence_wait must not let a second standby claim mid-restore.
  while sleep "$interval"; do put alive "$(stamp 0)" || log "heartbeat failed"; done &
  heartbeat=$!
  printf '%s\n' "$epoch" >"$data/ha-epoch"
  log "claimed the primary epoch $epoch; waiting ${fence_wait}s for the other nodes to see it"
  sleep "$fence_wait"
  if [[ $(get epoch || true) != "$epoch" ]]; then
    log "another node claimed the epoch after this one: it is the primary"
    exit 3
  fi
}

# Without -if-replica-exists: "no backup at that time" is an error, so the
# caller falls back to the latest instead of starting on an empty database.
restore_db() { litestream restore -config "$config" -integrity-check quick "$@" "$db"; }

# The newest upload on the replica (empty: no backup at all).
newest_backup() {
  litestream ltx -config "$config" -level all -json "$db" 2>/dev/null |
    grep -o '"timestamp": *"[^"]*"' | cut -d'"' -f4 | sort | tail -n 1 || true
}

# Keeps the local sun.db when this node was the last primary (the replica's
# epoch is this node's, or no node has claimed one yet). Otherwise waits while
# another primary's heartbeat is fresh, claims the epoch, gives the old primary
# time to stop, and restores sun.db and its files.
restore() {
  reachable
  local remote mine alive
  remote=$(get epoch || true)
  mine=$(cat "$data/ha-epoch" 2>/dev/null || true)
  if [[ -e $db && (-z $remote || $remote == "$mine") ]]; then
    log "keeping the local database: this node was the last primary"
    [[ -n $remote ]] || claim
    return 0
  fi
  # Before claiming anything: a primary's data is sealed with its key.
  if [[ -n $remote && -z ${SUN_MASTER_KEY:-} && ! -e $data/master.key ]]; then
    log "set SUN_MASTER_KEY to the primary's key: master.key is not copied"
    exit 1
  fi

  alive=$(get alive || true)
  if [[ -n $alive && ! $alive < $(stamp -$lease) ]]; then
    log "standby: the primary ($remote) is alive; taking over when its heartbeat stops"
    while alive=$(get alive || true) && [[ -n $alive && ! $alive < $(stamp -$lease) ]]; do
      sleep "$interval"
    done
  fi
  claim

  local synced newest
  newest=$(newest_backup)
  if [[ -z $newest && -n $remote ]]; then
    log "the replica has no database backup, yet $remote was primary: refusing to start on an empty database" \
      "(to start fresh on purpose, remove ha/epoch on the files replica)"
    exit 1
  fi
  if [[ -e $db ]]; then
    local aside
    aside=$data/backups/superseded-$(date -u +%Y%m%dT%H%M%SZ)
    mkdir -p "$aside"
    mv "$db" "$db"-wal "$db"-shm "$aside"/ 2>/dev/null || true
    log "another node was primary since: moved the old database to $aside"
  fi
  # Litestream's local state belongs to the database it watched.
  rm -rf "$data/.sun.db-litestream"

  # Read the marker before pulling, so the pulled files cover it.
  synced=$(get synced-at || true)
  pull_files
  if [[ -z $newest ]]; then
    log "no database replica yet: Sun starts with a new database"
    return 0
  fi
  if [[ -z $synced ]]; then
    log "WARNING: the files were never synced; restoring the latest database, blobs may be missing"
    restore_db
  elif [[ ! $synced < $newest ]]; then
    log "restoring the latest database ($newest; files synced at $synced)"
    restore_db
  else
    log "restoring the database as of the last file sync ($synced; newest $newest)"
    restore_db -timestamp "$synced" || {
      log "WARNING: no backup at $synced (retention?); restoring the latest, blobs may be missing"
      restore_db
    }
  fi
}

# Every interval: the heartbeat, then a file sync. When another node has
# claimed the epoch, stops Litestream (and so Sun) instead.
sync_loop() {
  local remote mine
  mine=$(cat "$data/ha-epoch")
  while sleep "$interval"; do
    remote=$(get epoch) || continue # unreachable: can't tell, keep going
    if [[ -n $remote && $remote != "$mine" ]]; then
      log "another node claimed the primary epoch ($remote): stopping this one"
      touch "$data/ha-superseded"
      kill -TERM "$1" 2>/dev/null
      return
    fi
    put alive "$(stamp 0)" || log "heartbeat failed"
    sync_files || log "file sync failed; retrying in ${interval}s"
  done
}

start() {
  restore
  # The sync loop beats from here on (wait: no "Terminated" notice in the log).
  { kill "$heartbeat" && wait "$heartbeat"; } 2>/dev/null || true
  rm -f "$data/ha-superseded"
  [[ $# -gt 0 ]] || set -- --with-host
  litestream replicate -config "$config" -exec "sun serve $*" &
  local replicate=$! loop status=0 stopping="" sun=""
  sync_loop "$replicate" &
  loop=$!
  trap 'stopping=1; kill -TERM "$replicate" 2>/dev/null' TERM INT
  # Sun is Litestream's child; remember it, so a Litestream crash can't leave
  # it serving (unreplicated) beside the standby that takes over.
  for _ in $(seq 1 50); do
    sun=$(pgrep -P "$replicate" -x sun) && break
    kill -0 "$replicate" 2>/dev/null || break
    sleep 0.2
  done
  # `wait` returns early when a trapped signal arrives; wait until it exits.
  until wait "$replicate"; do
    status=$?
    kill -0 "$replicate" 2>/dev/null || break
  done
  kill "$loop" 2>/dev/null || true
  if [[ -e $data/ha-superseded ]]; then exit 3; fi
  if [[ -n $sun ]] && kill -0 "$sun" 2>/dev/null; then
    log "litestream exited ($status) while Sun still runs: stopping Sun; the heartbeat stays so a restart keeps this database"
    kill -TERM "$sun" 2>/dev/null || true
    for _ in $(seq 1 100); do kill -0 "$sun" 2>/dev/null || break; sleep 0.2; done
    kill -KILL "$sun" 2>/dev/null || true
    exit 1
  fi
  # A clean stop: one last pass stamped now (nothing newer can follow), so a
  # standby restores the latest database, and no heartbeat, so it takes over
  # at once. Litestream exits 143 after a SIGTERM; a stop we asked for is 0.
  sync_files 0 || log "final file sync failed"
  put alive "" || true
  [[ -n $stopping ]] && status=0
  exit "$status"
}

case "${1:-}" in
  start) shift && start "$@" ;;
  sync-files) reachable && sync_files ;;
  *) sed -n '2,21p' "$0" >&2 && exit 2 ;;
esac
