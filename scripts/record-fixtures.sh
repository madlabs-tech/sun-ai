#!/usr/bin/env bash
# Records runtime event fixtures for sun-runtime or checks them.
#
#   scripts/record-fixtures.sh --check            validate committed fixtures (no CLI, no quota)
#   scripts/record-fixtures.sh --version <cli>    print the installed CLI's version (no quota)
#   scripts/record-fixtures.sh [cli...]           re-record normal/tool-error/crash for
#                                                 claude-code, codex, opencode (default: all)
#   RECORD_CASES=crash scripts/record-fixtures.sh codex   re-record only some cases
#
# Recordings land in fixtures/<cli>/<version>/, one dir per CLI version;
# fixtures/versions.txt lists the tested ones, pinned first. scripts/cli-matrix.sh
# records a new version, runs the contract tests and pins it.
#
# Recording spends real quota: prompts are tiny and every run happens in a throwaway temp dir.
# limit-hit cases can't be forced; they are hand-written ("synthetic": true) and never overwritten.
# Transports and flags per CLI are in crates/sun-runtime/src/{claude,codex,opencode}.rs.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
FIX=$ROOT/crates/sun-runtime/fixtures
CLIS="claude-code codex opencode"
CASES="normal tool-error limit-hit crash"
OPENCODE_VERSION=${OPENCODE_VERSION:-1.18.34} # `latest` in the weekly CI matrix
TIMEOUT_S=240

# Tool-error asks for a failing tool call: a file-read tool where the CLI has one, a shell command for codex.
prompt_for() {
  case $2 in
    tool-error) [ "$1" = codex ] && echo 'Run `cat missing.txt` (do not create it), then report the error in one line.' ||
      echo 'Read missing.txt with your file-read tool, not the shell (do not create it), then report the error in one line.' ;;
    *) echo 'Create file hello.txt containing hi, then reply done.' ;;
  esac
}

die() { echo "record-fixtures: $*" >&2; exit 1; }

# ---------- check ----------

check() {
  local fail=0 cli case f m
  for dir in "$FIX"/*/*/; do
    cli=$(basename "$(dirname "$dir")")
    case " $CLIS " in *" $cli "*) ;; *) echo "unknown cli dir $dir"; fail=1; continue ;; esac
    for case in $CASES; do
      f=$dir$case.jsonl m=$dir$case.meta.json
      [ -s "$f" ] || { echo "missing or empty $f"; fail=1; continue; }
      [ -f "$m" ] || { echo "missing $m"; fail=1; continue; }
      jq -e '(.synthetic | type) == "boolean" and .cli == "'"$cli"'" and .case == "'"$case"'" and (.transport | type) == "string"' \
        "$m" >/dev/null 2>&1 || { echo "bad meta $m (needs cli, case, transport, synthetic)"; fail=1; }
      # Every line is one JSON value. A crash may cut the final line mid-write: allowed there only.
      if [ "$case" = crash ]; then sed '$d' "$f"; else cat "$f"; fi |
        while IFS= read -r line; do printf '%s\n' "$line" | jq -e . >/dev/null 2>&1 || { echo "bad json line in $f: ${line:0:80}"; exit 1; }; done ||
        fail=1
    done
  done
  # No home paths, user names or credentials may leak into committed fixtures.
  if grep -rnoE '/Users/[^/"]*|-Users-[^-"]*|/home/[A-Za-z0-9_.-]+' "$FIX" | grep -vE ':/home/user$'; then
    echo "home path leaked into fixtures"; fail=1
  fi
  if grep -rnE 'sk-[A-Za-z0-9_-]{20,}|Bearer [A-Za-z0-9._-]{10,}|eyJ[A-Za-z0-9_-]{10,}\.|gh[pousr]_[A-Za-z0-9]{20,}|xox[abprs]-' "$FIX"; then
    echo "credential-like string in fixtures"; fail=1
  fi
  [ "$fail" = 0 ] || die "check failed"
  echo "record-fixtures: ok ($(find "$FIX" -name '*.jsonl' | wc -l | tr -d ' ') fixtures)"
}

# ---------- recording ----------

# Rewrites the temp dir, home dir and user name, and drops opaque thinking signatures.
scrub() {
  SCRUB_REAL=$WORK_REAL SCRUB_LINK=$WORK SCRUB_ENC=$WORK_ENC REAL_HOME=$REAL_HOME REAL_USER=$REAL_USER perl -pe '
    s/\Q$ENV{SCRUB_REAL}\E/\/work/g; s/\Q$ENV{SCRUB_LINK}\E/\/work/g; s/\Q$ENV{SCRUB_ENC}\E/-work/g;
    s/\Q$ENV{REAL_HOME}\E/\/home\/user/g; s/\b\Q$ENV{REAL_USER}\E\b/user/g;
    s/"signature":"[^"]*"/"signature":"<redacted>"/g;'
}

cli_argv() { # prints one arg per line for the CLI's headless/ACP entry point
  case $1 in
    claude-code) printf '%s\n' claude -p "$2" --output-format stream-json --verbose --model haiku \
      --permission-mode acceptEdits --setting-sources project --strict-mcp-config \
      --no-session-persistence --max-turns 4 ;;
    codex) printf '%s\n' codex exec --json --skip-git-repo-check --ephemeral --ignore-user-config \
      --ignore-rules -s workspace-write -c 'model_reasoning_effort="low"' "$2" ;;
    opencode) printf '%s\n' npx -y "opencode-ai@$OPENCODE_VERSION" acp ;;
  esac
}
transport() { case $1 in opencode) echo acp ;; *) echo headless-json ;; esac; }
done_pattern() { case $1 in claude-code) echo '"type":"result"' ;; codex) echo '"type":"turn\.(completed|failed)"' ;; opencode) echo '"id":3,' ;; esac; }
mid_pattern() { case $1 in claude-code) echo '"type":"tool_use"' ;; codex) echo '"type":"item\.started"' ;; opencode) echo '"sessionUpdate":"tool_call"' ;; esac; }
# The help text the adapter's permission-mode probe reads (none for opencode: ACP, no flag).
cli_help() { case $1 in claude-code) claude --help ;; codex) codex exec --help ;; esac; }
cli_version() { case $1 in claude-code) claude --version ;; codex) codex --version ;; opencode) npx -y "opencode-ai@$OPENCODE_VERSION" --version ;; esac | head -1; }
# `2.1.291 (Claude Code)`, `codex-cli 0.160.0` → the version (process::parse_version).
version_of() { cli_version "$1" | awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^[0-9]/) { print $i; exit } }'; }
pinned_of() { awk -v c="$1" '$1 == c { print $2 }' "$FIX/versions.txt"; }

# poll <file> <ere> : wait until the pattern appears in the file or the run dies / times out.
poll() {
  local i=0
  while [ $i -lt $((TIMEOUT_S * 5)) ]; do
    grep -qE "$2" "$1" 2>/dev/null && return 0
    kill -0 "$PID" 2>/dev/null || return 1
    sleep 0.2; i=$((i + 1))
  done
  return 1
}

# ACP handshake for opencode: initialize, session/new, session/prompt. Client messages go to $CLIENT.
acp_send() { printf '%s\n' "$1" >&3; printf '%s\n' "$1" >>"$CLIENT"; }
acp_drive() {
  local out=$1 prompt=$2 sid
  acp_send '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false}}}'
  poll "$out" '"id":1,' || return 1
  acp_send "$(jq -cn --arg cwd "$WORK_REAL" '{jsonrpc:"2.0",id:2,method:"session/new",params:{cwd:$cwd,mcpServers:[]}}')"
  poll "$out" '"id":2,' || return 1
  sid=$(grep -E '"id":2,' "$out" | jq -r '.result.sessionId')
  acp_send "$(jq -cn --arg s "$sid" --arg p "$prompt" '{jsonrpc:"2.0",id:3,method:"session/prompt",params:{sessionId:$s,prompt:[{type:"text",text:$p}]}}')"
}

record_case() {
  local cli=$1 case=$2 prompt=$3 run out err code argv=()
  # Fresh agent cwd per case; run artifacts live outside it so the agent can't see them.
  WORK=$(mktemp -d) run=$(mktemp -d)
  WORK_REAL=$(cd "$WORK" && pwd -P)
  WORK_ENC=$(printf %s "$WORK_REAL" | sed 's/[^A-Za-z0-9]/-/g')
  git -C "$WORK" init -q
  out=$run/out err=$run/err CLIENT=$run/client
  : >"$out"; : >"$CLIENT"
  while IFS= read -r a; do argv+=("$a"); done < <(cli_argv "$cli" "$prompt")

  set -m # own process group per run, so kill reaches npx/node children too
  if [ "$cli" = opencode ]; then
    mkfifo "$run/in.fifo"; mkdir "$run/home"
    # Isolated HOME: opencode free models need no login, and it must not pick up the user's skills.
    (cd "$WORK" && HOME=$run/home npm_config_cache=$REAL_HOME/.npm OPENCODE_CONFIG_CONTENT='{"model":"opencode/big-pickle","permission":{"edit":"allow","bash":"allow"}}' \
      exec "${argv[@]}" <"$run/in.fifo" >"$out" 2>"$err") &
    PID=$!
    exec 3>"$run/in.fifo"
    acp_drive "$out" "$prompt" || true
  else
    (cd "$WORK" && exec "${argv[@]}" </dev/null >"$out" 2>"$err") &
    PID=$!
  fi
  set +m

  if [ "$case" = crash ]; then
    poll "$out" "$(mid_pattern "$cli")" || true
    kill -KILL -- -"$PID" 2>/dev/null || true
  else
    poll "$out" "$(done_pattern "$cli")" || true
  fi
  [ "$cli" = opencode ] && exec 3>&-
  # Give the CLI a moment to exit on its own (stdin EOF for ACP), then stop it.
  for _ in 1 2 3 4 5 6 7 8 9 10; do kill -0 "$PID" 2>/dev/null || break; sleep 0.5; done
  kill -TERM -- -"$PID" 2>/dev/null || true
  code=0; wait "$PID" || code=$?

  scrub <"$out" >"$DIR/$case.jsonl"
  jq -n --arg cli "$cli" --arg case "$case" --arg transport "$(transport "$cli")" --arg version "$VERSION" \
    --arg recorded_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --argjson exit_code "$code" \
    --arg stderr "$(tail -n 20 "$err" | scrub)" \
    --argjson argv "$(printf '%s\n' "${argv[@]}" | scrub | jq -R . | jq -s .)" \
    --argjson client_messages "$(scrub <"$CLIENT" | jq -s .)" \
    '{cli: $cli, case: $case, transport: $transport, synthetic: false, cli_version: $version,
      recorded_at: $recorded_at, argv: $argv, client_messages: $client_messages,
      exit_code: $exit_code, stderr_tail: $stderr,
      note: (if $case == "crash" then "process group killed with SIGKILL right after the first tool call started" else null end)}' \
    >"$DIR/$case.meta.json"
  echo "recorded $cli/$VERSION_DIR/$case (exit $code, $(wc -l <"$DIR/$case.jsonl" | tr -d ' ') lines)"
  rm -rf "$WORK" "$run"
}

record() {
  local cli=$1 case
  VERSION=$(cli_version "$cli") VERSION_DIR=$(version_of "$cli")
  [[ $VERSION_DIR =~ ^[0-9][0-9A-Za-z.+-]*$ ]] || die "$cli reported no usable version: $VERSION"
  DIR=$FIX/$cli/$VERSION_DIR
  mkdir -p "$DIR"
  # limit-hit can't be forced: a new version starts from the pinned version's synthetic case.
  local pinned; pinned=$(pinned_of "$cli")
  for f in limit-hit.jsonl limit-hit.meta.json; do
    [ -f "$DIR/$f" ] || cp "$FIX/$cli/$pinned/$f" "$DIR/$f"
  done
  for case in ${RECORD_CASES:-normal tool-error crash}; do
    record_case "$cli" "$case" "$(prompt_for "$cli" "$case")"
  done
  # Replayed by the contract tests: the version must still offer every permission mode.
  [ "$cli" = opencode ] || cli_help "$cli" >"$DIR/help.txt" # --check greps it for home paths
}

command -v jq >/dev/null || die "jq is required"
if [ "${1:-}" = --check ]; then check; exit 0; fi
if [ "${1:-}" = --version ]; then version_of "${2:?usage: --version <cli>}"; exit 0; fi
REAL_HOME=$HOME REAL_USER=${USER:-$(id -un)}
for cli in ${@:-$CLIS}; do
  case " $CLIS " in *" $cli "*) record "$cli" ;; *) die "unknown cli: $cli" ;; esac
done
check
