#!/usr/bin/env bash
# Footprint gate. Builds the release `sun` with the web UI embedded,
# then measures and checks:
#   - binary size                               <= 40 MB
#   - idle RSS of `sun serve --with-host`        <= 40 MB  (no sessions, no mods)
#   - cold start to API ready (fresh data dir)    <= 300 ms
# Exits non-zero over budget. FOOTPRINT_JSON=<file> also writes the numbers.
# Env: FOOTPRINT_PORT (17788), FOOTPRINT_IDLE_SECS (5), SUN_BIN (skip the build).
# Portable to macOS /bin/bash 3.2 and Linux bash.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/sun-footprint.XXXXXX")
pid=""
cleanup() {
  [ -z "$pid" ] || kill "$pid" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

die() { echo "footprint: $*" >&2; exit 1; }
now_ms() { perl -MTime::HiRes=time -e 'printf "%d\n", time * 1000'; }
mb() { awk -v b="$1" 'BEGIN { printf "%.1f", b / 1048576 }'; }
budget=$((40 * 1024 * 1024))

if [ -z "${SUN_BIN:-}" ]; then
  echo "== build (web UI, then release binary)"
  (
    cd "$root/web"
    [ -d node_modules ] || npm ci --no-audit --no-fund
    npm run build >/dev/null
  )
  cargo build --release --quiet --manifest-path "$root/Cargo.toml" -p sun-cli --bin sun
  SUN_BIN="$root/target/release/sun"
fi
[ -x "$SUN_BIN" ] || die "no binary at $SUN_BIN"

size=$(wc -c <"$SUN_BIN" | tr -d ' ')

echo "== serve --with-host (idle)"
# The first exec of a freshly built binary includes the OS's one-time scan of
# it (about 0.6-0.9 s on macOS); that is not kernel boot, so pay it here.
"$SUN_BIN" --help >/dev/null
port=${FOOTPRINT_PORT:-17788}
url="http://127.0.0.1:$port"
start=$(now_ms)
"$SUN_BIN" serve --with-host --data-dir "$work/data" --listen "127.0.0.1:$port" \
  >"$work/serve.log" 2>&1 &
pid=$!
disown "$pid" 2>/dev/null || true
n=0
until curl -s -o /dev/null "$url/api/v1/tasks"; do
  kill -0 "$pid" 2>/dev/null || { cat "$work/serve.log" >&2; die "sun serve exited"; }
  n=$((n + 1))
  [ "$n" -le 3000 ] || die "sun serve not ready after 30 s"
  sleep 0.01
done
cold_ms=$(($(now_ms) - start))
ui=$(curl -s -o /dev/null -w '%{http_code}' "$url/")
[ "$ui" = 200 ] || die "GET / answered $ui: the web UI is not embedded"

sleep "${FOOTPRINT_IDLE_SECS:-5}"
rss=$(($(ps -o rss= -p "$pid" | tr -d ' ') * 1024))

printf '%-22s %s MB (budget 40)\n' "binary size" "$(mb "$size")"
printf '%-22s %s MB (budget 40)\n' "idle RSS" "$(mb "$rss")"
printf '%-22s %s ms (budget 300)\n' "cold start to API" "$cold_ms"
if [ -n "${FOOTPRINT_JSON:-}" ]; then
  printf '{"target":"%s","binary_bytes":%s,"idle_rss_bytes":%s,"cold_start_ms":%s}\n' \
    "$(uname -sm)" "$size" "$rss" "$cold_ms" >"$FOOTPRINT_JSON"
fi

fail=0
[ "$size" -le "$budget" ] || { echo "footprint: binary over 40 MB" >&2; fail=1; }
[ "$rss" -le "$budget" ] || { echo "footprint: idle RSS over 40 MB" >&2; fail=1; }
[ "$cold_ms" -le 300 ] || { echo "footprint: cold start over 300 ms" >&2; fail=1; }
exit "$fail"
