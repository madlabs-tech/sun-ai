#!/usr/bin/env bash
# Run Sun locally with one command: builds the UI (web/dist) and the kernel,
# starts `sun serve --with-host` and the vite dev server (hot reload), and
# stops everything on Ctrl-C.
#
# Usage: bash scripts/dev.sh [--no-web] [--reset] [-h|--help]
#   --no-web  kernel only; the UI comes from web/dist at the kernel's URL
#   --reset   wipe the kernel dev data (.dev/sun) first
#
# Env:
#   SUN_DATA_DIR       kernel data dir, default <repo>/.dev/sun
#   SUN_PORT           kernel port, default 7700. The vite dev server proxies
#                      /api to 7700 only, so another port works with --no-web.
#   DEV_BUILD_GB       RAM (GB) the mem-check gate asks for before a kernel
#                      rebuild, default 2 (measured peak 1.6 GB). No gate when the
#                      kernel binary is up to date.
# A ./.env with the git app config (chmod 600) is loaded into the kernel's
# secret store once it runs (scripts/load-git-apps.sh).
# Logs: .dev/logs/. Needs cargo, npm, curl (sccache is used when installed).
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
dev=$root/.dev
data=${SUN_DATA_DIR:-$dev/sun}
port=${SUN_PORT:-7700}
web_port=5173
logs=$dev/logs
web=1 reset=0
children="" # name:pid of every process started here

for arg in "$@"; do
  case $arg in
    --no-web) web=0 ;;
    --reset) reset=1 ;;
    -h | --help) sed -n '2,19p' "$0" && exit 0 ;;
    *) echo "dev: unknown flag $arg (see --help)" >&2 && exit 2 ;;
  esac
done

log() { echo "dev: $*" >&2; }
fail() { log "FAIL: $*" && exit 1; }

# Every pid in the tree under $1 (children first are collected before anyone
# dies, so nothing is reparented out of reach).
descendants() {
  local child
  for child in $(pgrep -P "$1" 2>/dev/null); do echo "$child" && descendants "$child"; done
}
stop_all() {
  local pids="" entry pid alive
  for entry in $children; do pids="$pids ${entry#*:} $(descendants "${entry#*:}")"; done
  [ -n "${pids// /}" ] || return 0
  log "stopping"
  # shellcheck disable=SC2086 # one pid per word
  kill -TERM $pids 2>/dev/null || true
  for _ in $(seq 1 100); do
    alive=""
    for pid in $pids; do kill -0 "$pid" 2>/dev/null && alive=1; done
    [ -z "$alive" ] && return 0
    sleep 0.1
  done
  log "still running after 10 s: kill -9"
  # shellcheck disable=SC2086
  kill -KILL $pids 2>/dev/null || true
}
trap stop_all EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Prefixes a process's output (`[sun] ...`) and keeps a copy in .dev/logs/NAME.log.
prefix() { tee "$logs/$1.log" | awk -v p="[$1] " '{ print p $0; fflush() }'; }

# wait_http NAME URL SECONDS: polls URL until it answers, else fails with the log tail.
wait_http() {
  local deadline=$((SECONDS + $3)) pid
  pid=$(echo "$children" | tr ' ' '\n' | sed -n "s/^$1://p")
  until curl -fsS -o /dev/null "$2" 2>/dev/null; do
    if ! kill -0 "$pid" 2>/dev/null || [ $SECONDS -ge $deadline ]; then
      log "--- last $1 log lines ($logs/$1.log) ---"
      tail -n 20 "$logs/$1.log" >&2 || true
      fail "$1 did not answer on $2"
    fi
    sleep 0.2
  done
}

for tool in cargo npm curl; do
  command -v "$tool" >/dev/null || fail "$tool is not installed (needs cargo, npm, curl)"
done
if [ "$web" = 1 ] && [ "$port" != 7700 ]; then
  log "WARNING: SUN_PORT=$port, but the vite dev server proxies /api to 7700 only; use the kernel's URL or --no-web"
fi

if [ "$reset" = 1 ]; then
  mkdir -p "$dev"
  dev_real=$(cd "$dev" && pwd -P)
  if [ -e "$data" ]; then
    real=$(cd "$data" && pwd -P)
    case $real in
      "$dev_real"/?*) log "reset: removing $real" && rm -rf "$real" ;;
      *) fail "--reset only wipes dirs under $dev_real; $real is not (unset SUN_DATA_DIR)" ;;
    esac
  fi
fi
mkdir -p "$data" "$logs"

# --- build ---
# stale TARGET PATHS...: true when TARGET is missing or any file under PATHS is
# newer (the mtime rule cargo uses).
stale() {
  local target=$1 && shift
  [ ! -e "$target" ] || [ -n "$(cd "$root" && find "$@" -newer "$target" -print -quit)" ]
}
export CARGO_BUILD_JOBS=4 CARGO_INCREMENTAL=0
command -v sccache >/dev/null && export RUSTC_WRAPPER=sccache
sun=${CARGO_TARGET_DIR:-$root/target}/debug/sun

# npm ci when node_modules is missing or older than the lockfile (npm writes
# node_modules/.package-lock.json on every install; -nt is true when it is missing).
if [ "$root/web/package-lock.json" -nt "$root/web/node_modules/.package-lock.json" ]; then
  log "npm ci"
  (cd "$root/web" && npm ci)
fi
# Debug builds read web/dist from disk (crates/sun-api/src/ui.rs), so a UI
# change never forces a cargo rebuild.
if stale "$root/web/dist/index.html" web/src web/public web/index.html web/package-lock.json web/vite.config.ts; then
  log "building the UI (web/dist)"
  (cd "$root/web" && npm run build)
fi

# The gate only when cargo will compile; a fresh `cargo build` is a quick no-op.
if stale "$sun" crates sdk catalog schema templates Cargo.toml Cargo.lock rust-toolchain.toml; then
  "$root/scripts/mem-check.sh" "${DEV_BUILD_GB:-2}" --wait
fi
log "building sun"
cargo build --manifest-path "$root/Cargo.toml" -p sun-cli --bin sun
touch "$sun" # cargo skips relinking for e.g. tests-only edits; mark sun fresh so the gate doesn't fire every start

# --- start ---
curl -s -o /dev/null "http://127.0.0.1:$port/" && fail "port $port is in use (another kernel?)"

SUN_DATA_DIR=$data "$sun" serve --with-host --port "$port" > >(prefix sun) 2>&1 &
children="$children sun:$!"
wait_http sun "http://127.0.0.1:$port/api/v1/auth" 60

# The git apps from ./.env into the sealed secret store (values never printed).
if [ -f "$root/.env" ]; then
  SUN="$sun" SUN_DATA_DIR=$data "$root/scripts/load-git-apps.sh" "$root/.env" ||
    fail "loading the git apps from .env (scripts/load-git-apps.sh)"
fi

if [ "$web" = 1 ]; then
  (cd "$root/web" && exec npm run dev -- --port "$web_port" --strictPort) > >(prefix web) 2>&1 &
  children="$children web:$!"
  wait_http web "http://localhost:$web_port/" 60
fi

token=$(sed -n 's/.*"token": *"\([^"]*\)".*/\1/p' "$data/kernel.json" 2>/dev/null || true)
echo
echo "  kernel + UI (web/dist)  http://127.0.0.1:$port"
[ "$web" = 1 ] && echo "  vite dev (hot reload)   http://localhost:$web_port"
echo "  sign in                 admin@sun.local, token ${token:-<see [sun] output>}"
echo "                          (from $data/kernel.json; changes after you set a password)"
echo "  data $data, logs $logs. Ctrl-C stops everything."
echo

# Exit (and stop the rest) as soon as one process dies.
while :; do
  for entry in $children; do
    kill -0 "${entry#*:}" 2>/dev/null || fail "${entry%%:*} exited (see $logs/${entry%%:*}.log)"
  done
  sleep 1
done
