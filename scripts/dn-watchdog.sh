#!/usr/bin/env bash
# dn-watchdog.sh — PID-based liveness watchdog for a delta-neutral leg pair.
#
# Watches two hype-twap PIDs directly (kill -0), NOT process-name counting
# (`pgrep -cx` etc.) — a name-based count would collide with unrelated
# hype-twap processes running on the same host. If exactly one of the two
# PIDs is alive for more than --grace seconds, the survivor is sent SIGTERM
# so it performs its graceful cancel/settle instead of running the leg
# naked indefinitely.
#
# bash / GNU Linux only.
#
# Usage: dn-watchdog.sh <pid1> <pid2> [--grace SECS] [--log FILE] [--comm NAME]
#
# Exit codes:
#   0  both PIDs exited on their own (or after SIGTERM cleanup below)
#   1  had to SIGTERM the survivor and it did not exit within 180s

set -euo pipefail

if [[ $# -lt 2 ]]; then
  echo "usage: dn-watchdog.sh <pid1> <pid2> [--grace SECS] [--log FILE]" >&2
  exit 2
fi

pid1="$1"
pid2="$2"
shift 2

grace=90
log_file=""
expected_comm="hype-twap"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --grace) grace="$2"; shift 2 ;;
    --log) log_file="$2"; shift 2 ;;
    --comm) expected_comm="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

# /proc/<pid>/comm is truncated to 15 characters by the kernel; truncate the
# expected name the same way so a long binary name still matches.
expected_comm="${expected_comm:0:15}"

log() {
  local msg="[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] $*"
  if [[ -n "${log_file}" ]]; then
    echo "${msg}" >>"${log_file}"
  else
    echo "${msg}"
  fi
}

# Returns 0 (alive) if kill -0 succeeds AND, when /proc/<pid>/comm is
# readable, it names the expected binary (guards against PID reuse; the
# expected name comes from --comm, default hype-twap). If /proc/<pid>/comm
# cannot be read but kill -0 succeeds, the process is still treated as alive
# (best-effort check only, not required to be conclusive).
is_alive() {
  local pid="$1"
  if ! kill -0 "${pid}" 2>/dev/null; then
    return 1
  fi
  local comm_file="/proc/${pid}/comm"
  if [[ -r "${comm_file}" ]]; then
    local comm
    comm="$(cat -- "${comm_file}" 2>/dev/null || true)"
    if [[ -n "${comm}" && "${comm}" != "${expected_comm}" ]]; then
      return 1
    fi
  fi
  return 0
}

log "watchdog started for pid1=${pid1} pid2=${pid2} grace=${grace}s"

one_dead_since=""

while true; do
  alive1=false
  alive2=false
  is_alive "${pid1}" && alive1=true
  is_alive "${pid2}" && alive2=true

  if [[ "${alive1}" == "false" && "${alive2}" == "false" ]]; then
    # Pidfile cleanup is intentionally skipped here: this script only knows
    # the two PIDs passed on argv, not the pidfile paths dn-pair.sh wrote
    # them to (<log-dir>/dn-<symbol>-<side>.pid). Callers that need cleanup
    # should do it themselves after this process exits 0.
    log "both pids exited; exiting 0"
    exit 0
  fi

  if [[ "${alive1}" == "true" && "${alive2}" == "true" ]]; then
    one_dead_since=""
  else
    now="$(date +%s)"
    if [[ -z "${one_dead_since}" ]]; then
      one_dead_since="${now}"
      log "one leg died (alive1=${alive1} alive2=${alive2}); starting ${grace}s grace timer"
    fi
    elapsed=$(( now - one_dead_since ))
    if (( elapsed >= grace )); then
      survivor=""
      if [[ "${alive1}" == "true" ]]; then survivor="${pid1}"; fi
      if [[ "${alive2}" == "true" ]]; then survivor="${pid2}"; fi
      if [[ -n "${survivor}" ]]; then
        log "grace period (${grace}s) elapsed with only one leg alive; sending SIGTERM to pid=${survivor}"
        kill -TERM "${survivor}" 2>/dev/null || true
        deadline=$(( $(date +%s) + 180 ))
        while (( $(date +%s) < deadline )); do
          if ! is_alive "${survivor}"; then
            log "survivor pid=${survivor} exited after SIGTERM; exiting 0"
            exit 0
          fi
          sleep 5
        done
        log "survivor pid=${survivor} did not exit within 180s of SIGTERM; exiting 1"
        exit 1
      fi
    fi
  fi

  sleep 5
done
