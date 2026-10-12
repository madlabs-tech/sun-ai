#!/usr/bin/env bash
# Weekly agent-CLI matrix, run by .github/workflows/cli-matrix.yml.
# For one CLI: record its installed (latest) version into fixtures/<cli>/<version>/,
# replay every version's recordings through the adapter contract tests, and pin the
# new version in fixtures/versions.txt when they pass. When they fail, the recording
# moves to $MATRIX_OUT (the job's artifact) and the script fails: the version stays
# unpinned, so readiness warns users on it and names the pinned known-good fallback.
#
#   scripts/cli-matrix.sh <cli>      record with the real, logged-in CLI (spends quota)
#   MATRIX_FROM=<dir> MATRIX_VERSION=<v> scripts/cli-matrix.sh <cli>
#                                    dry run: a copy of <dir> stands in for the recording (no CLI)
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
FIX=$ROOT/crates/sun-runtime/fixtures
VERSIONS=$FIX/versions.txt
OUT=${MATRIX_OUT:-$ROOT/target/cli-matrix}

die() { echo "cli-matrix: $*" >&2; exit 1; }

cli=${1:?usage: cli-matrix.sh <cli>}
pinned=$(awk -v c="$cli" '$1 == c { print $2 }' "$VERSIONS")
[ -n "$pinned" ] || die "unknown cli: $cli (not in versions.txt)"

if [ -n "${MATRIX_FROM:-}" ]; then
  version=${MATRIX_VERSION:?MATRIX_VERSION is required with MATRIX_FROM}
else
  version=$(bash "$ROOT/scripts/record-fixtures.sh" --version "$cli")
fi
[[ $version =~ ^[0-9][0-9A-Za-z.+-]*$ ]] || die "bad version: $version"
if awk -v c="$cli" -v v="$version" '$1 == c { for (i = 2; i <= NF; i++) if ($i == v) f = 1 } END { exit !f }' "$VERSIONS"; then
  echo "cli-matrix: $cli $version is already tested (pinned: $pinned)"
  exit 0
fi

if [ -n "${MATRIX_FROM:-}" ]; then
  mkdir -p "$FIX/$cli/$version"
  cp "$MATRIX_FROM"/*.json* "$FIX/$cli/$version/"
else
  bash "$ROOT/scripts/record-fixtures.sh" "$cli"
fi

if bash "$ROOT/scripts/record-fixtures.sh" --check &&
  (cd "$ROOT" && cargo test --locked -q -p sun-runtime --test it contract::); then
  # The new version goes first: it becomes the pinned fallback.
  awk -v c="$cli" -v v="$version" '$1 == c { $0 = c " " v substr($0, length(c) + 1) } { print }' \
    "$VERSIONS" >"$VERSIONS.tmp"
  mv "$VERSIONS.tmp" "$VERSIONS"
  echo "cli-matrix: $cli $version passes the contract tests; pinned (was $pinned)"
else
  mkdir -p "$OUT"
  rm -rf "${OUT:?}/$cli-$version"
  mv "$FIX/$cli/$version" "$OUT/$cli-$version"
  echo "::warning title=Agent CLI contract broken::$cli $version breaks the adapter contract; Sun keeps $pinned pinned. Recording: $OUT/$cli-$version"
  exit 1
fi
