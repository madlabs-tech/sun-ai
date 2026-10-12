# Shared setup for the HA checks (scripts/ha-failover.sh, scripts/ha-chaos.sh).
# Sourced, not run. Builds sun and sun-fake-runtime, makes a temp dir ($work)
# that is removed on exit (HA_KEEP=1 keeps it), and kills every node started
# with start_node.
#
# Replicas: Litestream's file replica and a plain dir by default. HA_S3=1 puts
# both in a local S3 server instead (`rclone serve s3`): Litestream's s3://
# replica and an rclone remote, as on a real bucket.
# Needs: cargo, litestream v0.5 (PATH or $LITESTREAM_BIN), curl, jq, sqlite3,
# rsync, python3, openssl; rclone (PATH or $RCLONE_BIN) for HA_S3=1.
set -euo pipefail
set -m # each background job gets its own process group, killed as one

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/sun-ha.XXXXXX")
pids=()
me=$(basename "$0" .sh)

fail() {
  echo "$me: FAIL: $*" >&2
  for f in "$work"/*.log; do [ -e "$f" ] && echo "--- $f" >&2 && tail -n 25 "$f" >&2; done
  exit 1
}
cleanup() {
  for p in "${pids[@]:-}"; do [ -n "$p" ] && kill -9 -- "-$p" 2>/dev/null || true; done
  if [ "${HA_KEEP:-}" = 1 ]; then echo "$me: kept $work"; else rm -rf "$work"; fi
}
trap cleanup EXIT

echo "$me: building sun, sun-fake-runtime"
cargo build -q --manifest-path "$root/Cargo.toml" -p sun-cli --bin sun -p sun-runtime --bin sun-fake-runtime
target=$(cargo metadata -q --format-version 1 --no-deps --manifest-path "$root/Cargo.toml" | jq -r .target_directory)/debug
mkdir -p "$work/bin"
if [ -n "${LITESTREAM_BIN:-}" ]; then ln -s "$LITESTREAM_BIN" "$work/bin/litestream"; fi
if [ -n "${RCLONE_BIN:-}" ]; then ln -s "$RCLONE_BIN" "$work/bin/rclone"; fi
export PATH="$work/bin:$target:$PATH"
litestream version >/dev/null || fail "litestream not found"

free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'; }
port=$(free_port)
base="http://127.0.0.1:$port"
export LITESTREAM_CONFIG="$root/packaging/litestream.yml"
export SUN_FILES_SYNC_INTERVAL=1
SUN_MASTER_KEY=$(openssl rand -hex 32)
export SUN_MASTER_KEY

if [ "${HA_S3:-}" = 1 ]; then
  rclone version >/dev/null || fail "rclone not found"
  s3_port=$(free_port)
  export AWS_ACCESS_KEY_ID=sun-ha-test AWS_SECRET_ACCESS_KEY=$(openssl rand -hex 16) AWS_REGION=us-east-1
  mkdir -p "$work/s3/sun"
  rclone serve s3 "$work/s3" --addr "127.0.0.1:$s3_port" --auth-key "$AWS_ACCESS_KEY_ID,$AWS_SECRET_ACCESS_KEY" \
    >>"$work/s3.log" 2>&1 &
  pids+=($!)
  for _ in $(seq 1 50); do curl -s "http://127.0.0.1:$s3_port/" >/dev/null && break; sleep 0.1; done
  export SUN_DB_REPLICA="s3://sun/db?endpoint=http://127.0.0.1:$s3_port&region=us-east-1&forcePathStyle=true"
  export RCLONE_CONFIG_S3REMOTE_TYPE=s3 RCLONE_CONFIG_S3REMOTE_PROVIDER=Rclone \
    RCLONE_CONFIG_S3REMOTE_ENDPOINT="http://127.0.0.1:$s3_port" \
    RCLONE_CONFIG_S3REMOTE_ACCESS_KEY_ID=$AWS_ACCESS_KEY_ID \
    RCLONE_CONFIG_S3REMOTE_SECRET_ACCESS_KEY=$AWS_SECRET_ACCESS_KEY
  export SUN_FILES_REPLICA=s3remote:sun/files
  ha_get() { rclone cat "$SUN_FILES_REPLICA/ha/$1" 2>/dev/null || true; }
  ha_put() { printf '%s\n' "$2" | rclone rcat "$SUN_FILES_REPLICA/ha/$1"; }
  replica_has_blob() { rclone lsf "$SUN_FILES_REPLICA/blobs/$1" >/dev/null 2>&1; }
else
  export SUN_DB_REPLICA="file://$work/replica/db"
  export SUN_FILES_REPLICA="$work/replica/files"
  ha_get() { cat "$SUN_FILES_REPLICA/ha/$1" 2>/dev/null || true; }
  ha_put() { mkdir -p "$SUN_FILES_REPLICA/ha" && printf '%s\n' "$2" >"$SUN_FILES_REPLICA/ha/$1"; }
  replica_has_blob() { [ -e "$SUN_FILES_REPLICA/blobs/$1" ]; }
fi

# node_config NAME [AGENT_SCRIPT]: a data dir whose sun.yaml listens on $port
# and runs the fake runtime with that script (default $work/agent.jsonl).
node_config() {
  mkdir -p "$work/$1"
  cat >"$work/$1/sun.yaml" <<EOF
listen: 127.0.0.1:$port
default_runtime: fake
runtimes:
  fake:
    fake: { binary: "$target/sun-fake-runtime", script: "${2:-$work/agent.jsonl}" }
EOF
}

start_node() { # start_node NAME [sun serve args]: ha.sh start on data dir $work/NAME
  local name=$1
  shift
  [ $# -gt 0 ] || set -- --with-host
  SUN_DATA_DIR="$work/$name" "$root/scripts/ha.sh" start "$@" >>"$work/$name.log" 2>&1 &
  pids+=($!)
  eval "pid_$name=$!"
}
up() { curl -fsS "${1:-$base}/healthz" >/dev/null 2>&1; }
wait_up() { # wait_up SECONDS [BASE]
  for _ in $(seq 1 $(($1 * 10))); do up "${2:-}" && return 0; sleep 0.1; done
  fail "sun did not answer on ${2:-$base}"
}
api() { # api METHOD PATH [JSON]
  curl -fsS -X "$1" -H "authorization: Bearer $token" -H 'content-type: application/json' \
    ${3:+--data "$3"} "$base/api/v1$2"
}
wait_for() { # wait_for WHAT SECONDS JQ_FILTER PATH
  local deadline=$((SECONDS + $2)) body=""
  while [ $SECONDS -lt $deadline ]; do
    body=$(api GET "$4" 2>/dev/null || true)
    if [ -n "$body" ] && echo "$body" | jq -e "$3" >/dev/null 2>&1; then return 0; fi
    sleep 0.2
  done
  echo "$body" | jq . >&2 || true
  fail "timed out waiting for $1"
}
# Every 64-hex string in a node's sun.db that names a blob somewhere in $work
# must exist in that node's blobs/. Prints how many it checked.
check_cited_blobs() { # check_cited_blobs NODE
  local hash blob checked=0
  for hash in $(sqlite3 "$work/$1/sun.db" .dump | grep -oE '[0-9a-f]{64}' | sort -u); do
    blob="${hash:0:2}/$hash.zst"
    compgen -G "$work/*/blobs/$blob" >/dev/null || continue # a sha256 that is not a blob
    [ -e "$work/$1/blobs/$blob" ] || fail "$1's database cites blob $hash, missing on $1"
    checked=$((checked + 1))
  done
  echo "$checked"
}
