#!/usr/bin/env bash
# Enforces the crate dependency table from `cargo metadata`.
# Checks normal and build dependencies; dev-dependencies (test fakes) are free.
# Needs: cargo, jq. Bash 3 compatible (macOS).
set -euo pipefail

# Allowed workspace (sun-*) dependencies per crate. "*" = anything.
allowed_internal() {
  case "$1" in
    sun-core) echo "" ;;
    sun-store | sun-workflow | sun-runtime) echo "sun-core" ;;
    # Apache-2.0 SDK (sdk/rust): standalone, nothing from crates/.
    sun-mod-sdk) echo "" ;;
    sun-mod) echo "sun-core sun-mod-sdk" ;;
    sun-context) echo "sun-core sun-store" ;;
    sun-engine) echo "sun-core sun-store sun-workflow sun-context" ;;
    sun-host) echo "sun-core sun-runtime" ;;
    # Catalog: reuses the MCP registration from sun-mod.
    sun-catalog) echo "sun-core sun-mod" ;;
    sun-api) echo "sun-core sun-engine sun-context sun-catalog" ;;
    sun-cli) echo "*" ;;
    *) return 1 ;;
  esac
}

# sun-core has no I/O: only these external crates.
SUN_CORE_EXTERNAL="serde serde_json ulid chrono thiserror async-trait futures-core ed25519-dalek"

contains() { # word, list
  case " $2 " in *" $1 "*) return 0 ;; *) return 1 ;; esac
}

metadata=$(cargo metadata --format-version 1 --no-deps)
crates=$(jq -r '.packages[].name' <<<"$metadata")
pairs=$(jq -r '.packages[] | .name as $crate | .dependencies[]
  | select(.kind != "dev") | "\($crate) \(.name)"' <<<"$metadata")

fail=0
for crate in $crates; do
  if ! allowed_internal "$crate" >/dev/null; then
    echo "check-deps: $crate is not in the dependency table (add it to scripts/check-deps.sh)" >&2
    fail=1
  fi
done

while read -r crate dep; do
  [ -z "$crate" ] && continue
  allowed=$(allowed_internal "$crate") || continue
  case "$dep" in
    sun-*)
      if [ "$allowed" != "*" ] && ! contains "$dep" "$allowed"; then
        echo "check-deps: $crate must not depend on $dep (allowed: ${allowed:-none})" >&2
        fail=1
      fi
      ;;
    *)
      if [ "$crate" = sun-core ] && ! contains "$dep" "$SUN_CORE_EXTERNAL"; then
        echo "check-deps: sun-core must not depend on $dep (allowed: $SUN_CORE_EXTERNAL)" >&2
        fail=1
      fi
      ;;
  esac
done <<<"$pairs"

if [ "$fail" -ne 0 ]; then
  exit 1
fi
echo "check-deps: ok ($(echo "$crates" | wc -l | tr -d ' ') crates)"
