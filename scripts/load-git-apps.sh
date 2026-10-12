#!/usr/bin/env bash
# Loads the GitHub App and GitLab OAuth app config into Sun's sealed secret
# store (ecosystem secrets), where the kernel reads it. Sun never reads it
# from the environment or a file itself.
#
# Usage: scripts/load-git-apps.sh [ENV_FILE]
#   ENV_FILE  KEY=VALUE lines, default ./.env (skipped when the default is
#             missing). Must not be group- or world-readable (chmod 600).
#             Variables already in the environment win over the file.
#
# Keys: GITHUB_APP_ID, GITHUB_APP_SLUG, GITHUB_APP_PRIVATE_KEY_PATH (the PEM
# is read into GITHUB_APP_PRIVATE_KEY), GITHUB_APP_WEBHOOK_SECRET,
# GITLAB_OAUTH_CLIENT_ID, GITLAB_OAUTH_CLIENT_SECRET, GITLAB_OAUTH_URL.
# Empty or unset keys are skipped; running it again replaces the values.
#
# Env: SUN (the sun binary, default `sun` on PATH), SUN_DATA_DIR (passed on).
# Values go to `sun secret set NAME` on stdin only: never in argv or output.
# Prints one `set NAME` line per secret.
set -euo pipefail

sun=${SUN:-sun}
keys="GITHUB_APP_ID GITHUB_APP_SLUG GITHUB_APP_PRIVATE_KEY_PATH GITHUB_APP_WEBHOOK_SECRET
GITLAB_OAUTH_CLIENT_ID GITLAB_OAUTH_CLIENT_SECRET GITLAB_OAUTH_URL"

fail() { echo "load-git-apps: $*" >&2 && exit 1; }

file=${1:-.env}
if [ $# -ge 1 ] && [ ! -f "$file" ]; then
  fail "$file does not exist"
fi

# file_value KEY: KEY's value in $file (last one wins), quotes stripped.
# The file is parsed, never sourced: nothing in it runs.
file_value() {
  [ -f "$file" ] || return 0
  local line value=""
  while IFS= read -r line || [ -n "$line" ]; do
    line=${line#export }
    case $line in
      "$1="*)
        value=${line#*=}
        value=${value%$'\r'}
        case $value in
          \"*\") value=${value#\"} value=${value%\"} ;;
          \'*\') value=${value#\'} value=${value%\'} ;;
        esac
        ;;
    esac
  done <"$file"
  printf '%s' "$value"
}

if [ -f "$file" ]; then
  mode=$(stat -c %a "$file" 2>/dev/null || stat -f %Lp "$file")
  if (((8#$mode & 8#044) != 0)); then
    fail "$file is readable by group or others (mode $mode): run chmod 600 $file"
  fi
fi

# set_secret NAME: the value on stdin.
set_secret() {
  "$sun" secret set "$1" >/dev/null || fail "sun secret set $1 failed"
  echo "set $1"
}

for key in $keys; do
  value=${!key:-}
  [ -n "$value" ] || value=$(file_value "$key")
  [ -n "$value" ] || continue
  if [ "$key" = GITHUB_APP_PRIVATE_KEY_PATH ]; then
    [ -f "$value" ] && [ -r "$value" ] || fail "GITHUB_APP_PRIVATE_KEY_PATH: no readable file there"
    set_secret GITHUB_APP_PRIVATE_KEY <"$value"
  else
    # `printf` is a shell builtin: the value is in no process's argv.
    printf '%s' "$value" | set_secret "$key"
  fi
done
