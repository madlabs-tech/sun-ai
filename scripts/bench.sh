#!/usr/bin/env bash
# Footprint bench.
#
#   1. hello-kernel (tokio + axum + sqlx/SQLite): binary size, cold start to
#      first HTTP 200 (median of 5), idle RSS. Fails if the product ceilings are broken.
#   2. 1..N concurrent idle `claude -p` sessions: RSS per session (process tree).
#   3. Token use of one small real task via `claude -p --output-format json`.
#
# Steps 2-3 spend real quota (N tiny prompts + one small task); BENCH_CLI=0 skips them.
# Env: BENCH_CLI (1), BENCH_SESSIONS (5), BENCH_PORT (18787),
#      BENCH_IDLE_MODEL (haiku), BENCH_TASK_MODEL (sonnet).
# Portable to macOS /bin/bash 3.2 and Linux bash.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/sun-bench.XXXXXX")
pids=""
cleanup() {
  for p in $pids; do kill "$p" 2>/dev/null || true; done
  rm -rf "$work"
}
trap cleanup EXIT

die() { echo "bench: $*" >&2; exit 1; }
now_ms() { perl -MTime::HiRes=time -e 'printf "%d\n", time * 1000'; }
rss_kb() { ps -o rss= -p "$1" | tr -d ' '; }
# RSS (KB) of a process plus all its descendants.
tree_rss_kb() {
  ps -A -o pid=,ppid=,rss= | awk -v root="$1" '
    { parent[$1] = $2; rss[$1] = $3 }
    END {
      for (p in rss) {
        q = p
        while (q != "" && q != root && q > 1) q = parent[q]
        if (q == root) total += rss[p]
      }
      print total + 0
    }'
}
mb() { awk -v kb="$1" 'BEGIN { printf "%.1f", kb / 1024 }'; }
# wait_for <seconds> <command...>: poll every 0.2 s until the command succeeds.
wait_for() {
  local limit=$(($1 * 5)) n=0
  shift
  until "$@"; do
    n=$((n + 1))
    [ "$n" -le "$limit" ] || return 1
    sleep 0.2
  done
}

echo "== machine"
echo "$(uname -sm), $(sysctl -n machdep.cpu.brand_string 2>/dev/null || grep -m1 'model name' /proc/cpuinfo | cut -d: -f2)"

# ---------------------------------------------------------------- 1. kernel
echo "== hello-kernel"
cargo build --release --quiet --manifest-path "$root/bench/hello-kernel/Cargo.toml"
bin="$root/bench/hello-kernel/target/release/hello-kernel"
port=${BENCH_PORT:-18787}
url="http://127.0.0.1:$port/health"
curl -sf -o /dev/null "$url" && die "port $port already in use"

size_bytes=$(wc -c <"$bin" | tr -d ' ')
starts=""
idle_kb=0
for run in 1 2 3 4 5; do
  rm -f "$work"/hello.db*
  t0=$(now_ms)
  HELLO_DB="$work/hello.db" HELLO_ADDR="127.0.0.1:$port" "$bin" &
  kpid=$!
  pids="$pids $kpid"
  # Tight poll: granularity is one curl round (a few ms).
  deadline=$((t0 + 10000))
  until curl -sf -o /dev/null "$url"; do
    kill -0 "$kpid" 2>/dev/null || die "hello-kernel exited during start"
    [ "$(now_ms)" -lt "$deadline" ] || die "hello-kernel not ready after 10 s"
  done
  t1=$(now_ms)
  starts="$starts $((t1 - t0))"
  if [ "$run" = 5 ]; then
    sleep 5
    idle_kb=$(rss_kb "$kpid")
  fi
  kill "$kpid"
  wait "$kpid" 2>/dev/null || true
done
cold_ms=$(echo $starts | tr ' ' '\n' | sort -n | sed -n 3p)

verdict() { if [ "$1" -le "$2" ]; then echo ok; else echo OVER; fi; }
echo "binary size      : $(mb $((size_bytes / 1024))) MB (budget 40 MB with UI: $(verdict "$size_bytes" $((40 * 1024 * 1024))))"
echo "cold start runs  :$starts ms"
echo "cold start median: $cold_ms ms (target 300 ms: $(verdict "$cold_ms" 300); ceiling 2000 ms: $(verdict "$cold_ms" 2000))"
echo "idle RSS         : $(mb "$idle_kb") MB (target 40 MB: $(verdict "$idle_kb" $((40 * 1024))); ceiling 100 MB: $(verdict "$idle_kb" $((100 * 1024))))"
[ "$cold_ms" -le 2000 ] || die "cold start above the ceiling (2 s)"
[ "$idle_kb" -le $((100 * 1024)) ] || die "idle RSS above the ceiling (100 MB)"

# ---------------------------------------------------------------- 2-3. agent CLI
if [ "${BENCH_CLI:-1}" = 0 ]; then
  echo "== claude: skipped (BENCH_CLI=0)"
  exit 0
fi
command -v claude >/dev/null || die "claude not on PATH (BENCH_CLI=0 to skip)"
echo "== claude $(claude --version)"

# Bare CLI: no user plugins, hooks, MCP servers or CLAUDE.md; nothing saved.
cli_flags="-p --safe-mode --strict-mcp-config --no-session-persistence"
sessions=${BENCH_SESSIONS:-5}
[ "$sessions" -ge 1 ] && [ "$sessions" -le 6 ] || die "BENCH_SESSIONS must be 1..6"
cli_dir="$work/cli"
mkdir -p "$cli_dir"
spids=""
echo "session  boot RSS (MB)  idle RSS after 1 turn (MB)  all sessions (MB)"
for i in $(seq 1 "$sessions"); do
  fifo="$cli_dir/in$i"
  out="$cli_dir/out$i.jsonl"
  mkfifo "$fifo"
  # Close the earlier sessions' stdin fds so each fifo has exactly one writer.
  # shellcheck disable=SC2086 # cli_flags is a word list
  (exec 3>&- 4>&- 5>&- 6>&- 7>&- 8>&- && cd "$cli_dir" && exec claude $cli_flags --model "${BENCH_IDLE_MODEL:-haiku}" \
    --input-format stream-json --output-format stream-json --verbose \
    <"$fifo" >"$out" 2>"$cli_dir/err$i") &
  spid=$!
  pids="$pids $spid"
  spids="$spids $spid"
  fd=$((2 + i)) # fds 3..8 keep each session's stdin open
  eval "exec $fd>\"$fifo\""
  sleep 5
  boot_kb=$(tree_rss_kb "$spid")
  eval "printf '%s\n' '{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"Reply with the single word: ok\"}}' >&$fd"
  wait_for 120 grep -q '"type":"result"' "$out" || die "session $i: no result (see $cli_dir/err$i)"
  sleep 3
  idle_kb=$(tree_rss_kb "$spid")
  total_kb=0
  for p in $spids; do total_kb=$((total_kb + $(tree_rss_kb "$p"))); done
  printf '%-8s %-14s %-27s %s\n' "$i" "$(mb "$boot_kb")" "$(mb "$idle_kb")" "$(mb "$total_kb")"
done
for i in $(seq 1 "$sessions"); do eval "exec $((2 + i))>&-"; done
# shellcheck disable=SC2086
kill $spids 2>/dev/null || true
for p in $spids; do wait "$p" 2>/dev/null || true; done

echo "== Token use of one small real task (model ${BENCH_TASK_MODEL:-sonnet})"
task_dir="$work/task"
mkdir -p "$task_dir"
cat >"$task_dir/calc.py" <<'EOF'
def average(numbers):
    """Return the mean of a non-empty list; raise ValueError on an empty one."""
    return sum(numbers) / len(numbers) + 1
EOF
cat >"$task_dir/test_calc.py" <<'EOF'
import unittest
from calc import average


class AverageTest(unittest.TestCase):
    def test_mean(self):
        self.assertEqual(average([2, 4, 6]), 4)

    def test_empty(self):
        with self.assertRaises(ValueError):
            average([])


if __name__ == "__main__":
    unittest.main()
EOF
# --permission-prompts none: anything outside the allow list is denied, never waits on a host.
# shellcheck disable=SC2086
(cd "$task_dir" && exec claude $cli_flags --model "${BENCH_TASK_MODEL:-sonnet}" --output-format json \
  --permission-mode acceptEdits --permission-prompts none --allowedTools "Bash(python3 *)" \
  --max-budget-usd 1 \
  "The tests in test_calc.py fail. Fix calc.py so that 'python3 -m unittest' passes. Do not edit the tests.") \
  >"$work/t8.json" &
tpid=$!
pids="$pids $tpid"
wait "$tpid" || die "token-use task failed: $(head -c 500 "$work/t8.json")"
python3 - "$work/t8.json" <<'EOF'
import json, sys
r = json.load(open(sys.argv[1]))
u = r.get("usage", {})
print(f"turns {r.get('num_turns')}, duration {r.get('duration_ms')} ms, cost_usd {r.get('total_cost_usd')}")
for k in ("input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens", "output_tokens"):
    print(f"{k:28}: {u.get(k)}")
EOF
if (cd "$task_dir" && python3 -m unittest -q 2>/dev/null); then
  echo "task result      : tests pass"
else
  echo "task result      : tests still FAIL (model outcome, not a bench failure)"
fi
