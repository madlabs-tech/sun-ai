#!/usr/bin/env bash
# Resource gate: run before any heavy step (cargo build/test, release build,
# Playwright, VMs). Passes only if RAM available >= need + 1 GB
# buffer and the 1-min load average is below 1.5x the CPU count.
# usage: scripts/mem-check.sh [need_gb=4] [--wait [max_minutes=30]]
set -euo pipefail
need_gb=${1:-3}
buffer_gb=${MEM_BUFFER_GB:-1}
wait=0 max_min=30
[ "${2:-}" = --wait ] && { wait=1; max_min=${3:-30}; }

available_gb() {
  if [ -r /proc/meminfo ]; then
    awk '/MemAvailable/ {printf "%.1f", $2/1048576}' /proc/meminfo
  else
    # macOS: free + inactive + speculative + purgeable pages are reclaimable.
    vm_stat | awk -v ps="$(pagesize)" '
      /Pages free/ || /Pages inactive/ || /Pages speculative/ || /Pages purgeable/ {gsub("\\.","",$NF); n+=$NF}
      END {printf "%.1f", n*ps/1073741824}'
  fi
}
cpus() { getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu; }
load1() { uptime | awk -F'load averages?: ' '{print $2}' | tr -d ',' | awk '{print $1}'; }

check() {
  avail=$(available_gb); load=$(load1); ncpu=$(cpus)
  ok_mem=$(awk -v a="$avail" -v n="$need_gb" -v b="$buffer_gb" 'BEGIN {print (a >= n + b) ? 1 : 0}')
  ok_cpu=$(awk -v l="$load" -v c="$ncpu" 'BEGIN {print (l < c * 1.5) ? 1 : 0}')
  echo "mem-check: available ${avail} GB (need ${need_gb} + ${buffer_gb} buffer), load ${load} on ${ncpu} CPUs"
  [ "$ok_mem" = 1 ] && [ "$ok_cpu" = 1 ]
}

if check; then exit 0; fi
if [ "$wait" = 1 ]; then
  deadline=$(( $(date +%s) + max_min * 60 ))
  while [ "$(date +%s)" -lt "$deadline" ]; do
    sleep 30
    check && exit 0
  done
fi
echo "mem-check: not enough headroom — wait, free memory (stop servers/builds you started), or lower parallelism" >&2
exit 1
