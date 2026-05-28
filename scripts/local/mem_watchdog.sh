#!/bin/bash
# Hard memory kill-switch for local Hera profiling runs.
# Sums RSS of all node-hera processes every 0.3s; if total exceeds the ceiling,
# SIGKILLs them all immediately so the machine cannot OOM/restart again.
# Ceiling default 18 GiB (override: CEIL_GIB=NN). Logs trips to stderr + LOG.
set -u
CEIL_GIB="${CEIL_GIB:-18}"
POLL="${POLL:-0.3}"
CEIL_KB=$(( CEIL_GIB * 1024 * 1024 ))
LOG="${LOG:-/tmp/hera_memwatch.log}"
echo "$(date +%T) watchdog up: ceiling=${CEIL_GIB}GiB poll=${POLL}s log=${LOG}" | tee -a "$LOG"
while true; do
  total=$(ps -axo rss=,command= | grep -E 'node-hera' | grep -v grep | awk '{s+=$1} END{print s+0}')
  if [ "${total:-0}" -gt "$CEIL_KB" ]; then
    echo "$(date +%T) TRIP: node-hera RSS=$((total/1024))MiB > ${CEIL_GIB}GiB -> SIGKILL all" | tee -a "$LOG"
    killall -9 node-hera 2>/dev/null
  fi
  sleep "$POLL"
done
