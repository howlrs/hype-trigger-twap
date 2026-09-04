#!/usr/bin/env bash
# shellcheck disable=SC1007,SC2155
# Linux-only, fail-closed pair watchdog.
set -euo pipefail
usage() { echo "usage: dn-watchdog.sh --pid1 PID --pid1-starttime T --pid1-exe PATH --pid2 PID --pid2-starttime T --pid2-exe PATH [--grace SECS] [--log FILE] [--event-log FILE]" >&2; }
pid1= pid2= start1= start2= exe1= exe2= log_file="" event_log="" grace=0 event_seq=0
# Optional only: dn-pair passes this exact environment value to the
# watchdog's otherwise-clean env. It is never placed in argv, a manifest, an
# event, or a log line.
alert_hook_url="${HL_ALERT_HOOK_URL:-}"
unset HL_ALERT_HOOK_URL
alert_pid=""
need() {
  [[ $# -ge 2 && -n "${2-}" && "${2-}" != --* ]] || {
    echo "missing value for $1" >&2
    exit 2
  }
}
while (($#)); do
  case "$1" in
  --pid1)
    need "$@"
    pid1="${2-}"
    shift 2
    ;;
  --pid2)
    need "$@"
    pid2="${2-}"
    shift 2
    ;;
  --pid1-starttime)
    need "$@"
    start1="${2-}"
    shift 2
    ;;
  --pid2-starttime)
    need "$@"
    start2="${2-}"
    shift 2
    ;;
  --pid1-exe)
    need "$@"
    exe1="${2-}"
    shift 2
    ;;
  --pid2-exe)
    need "$@"
    exe2="${2-}"
    shift 2
    ;;
  --grace)
    need "$@"
    grace="${2-}"
    shift 2
    ;;
  --log)
    need "$@"
    log_file="${2-}"
    shift 2
    ;;
  --event-log)
    need "$@"
    event_log="${2-}"
    shift 2
    ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    echo "unknown argument: $1" >&2
    usage
    exit 2
    ;;
  esac
done
for item in pid1 pid2 start1 start2; do [[ "${!item}" =~ ^[1-9][0-9]*$ ]] || {
  echo "invalid $item" >&2
  exit 2
}; done
[[ "$pid1" != "$pid2" ]] || {
  echo "pid1 and pid2 must differ" >&2
  exit 2
}
[[ "$grace" =~ ^[0-9]+$ && "$grace" -le 3600 ]] || {
  echo "--grace must be 0..3600" >&2
  exit 2
}
for item in exe1 exe2; do [[ "${!item}" = /* ]] || {
  echo "invalid $item" >&2
  exit 2
}; done
proc_root="${DN_PROC_ROOT:-/proc}"
log() {
  local m="[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] $*"
  [[ -n "$log_file" ]] && printf '%s\n' "$m" >>"$log_file" || printf '%s\n' "$m"
}
valid_alert_hook_url() {
  # Remote hooks require HTTPS. HTTP is only for literal loopback test
  # receivers, never a hostname whose resolution could be changed. The
  # deliberately narrow path alphabet also makes curl stdin config safe.
  [[ "$1" =~ ^https://([A-Za-z0-9.-]+|\[[0-9A-Fa-f:.]+\])(:[0-9]{1,5})?(/[A-Za-z0-9._~:/%+=,-]*)?$ || "$1" =~ ^http://(127\.0\.0\.1|\[::1\])(:[0-9]{1,5})?(/[A-Za-z0-9._~:/%+=,-]*)?$ ]]
}
reap_alert() {
  [[ -n "$alert_pid" ]] || return 0
  kill -0 "$alert_pid" 2>/dev/null && return 0
  if ! wait "$alert_pid" 2>/dev/null; then
    log 'alert hook delivery failed; watchdog continues'
  fi
  alert_pid=""
}
send_alert() {
  local leg="$1"
  [[ -n "$alert_hook_url" ]] || return 0
  reap_alert
  if [[ -n "$alert_pid" ]]; then
    log 'alert hook busy; dropping pair alert; watchdog continues'
    return 0
  fi
  # One background worker at a time. curl's own connect/overall deadlines
  # mean it cannot become a permanent child; its status is reaped next loop.
  (
    printf 'url = "%s"\n' "$alert_hook_url" | \
      curl --config - --silent --show-error --fail --max-time 2 --connect-timeout 1 \
        --max-redirs 0 --request POST --header 'Content-Type: application/json' \
        --data "{\"schema_version\":1,\"kind\":\"pair_leg_abnormal\",\"leg\":\"$leg\",\"reason\":\"interrupted\"}" \
        >/dev/null 2>&1
  ) &
  alert_pid=$!
}
if [[ -n "$alert_hook_url" ]]; then
  if ! valid_alert_hook_url "$alert_hook_url"; then
    log 'invalid alert hook URL; external alerting disabled'
    alert_hook_url=""
  elif ! command -v curl >/dev/null 2>&1; then
    log 'curl unavailable; external alerting disabled'
    alert_hook_url=""
  fi
fi
event() {
  # Sidecar telemetry only. A failure here must not change watchdog safety
  # behavior (including its signal/exit path), so it is explicitly ignored.
  if [[ -n "$event_log" ]]; then
    event_seq=$((event_seq + 1))
    if ! printf '{"schema_version":1,"sequence":%s,"emitted_at":"%s","payload":{"kind":"pair_leg_abnormal","leg":"%s","reason":"interrupted"}}\n' "$event_seq" "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$1" >>"$event_log"; then
      log 'observability event write failed; watchdog continues'
    fi
  fi
  send_alert "$1"
}
starttime() {
  local s rest
  s="$(<"$proc_root/$1/stat")" || return 1
  rest="${s##*) }"
  # The state after ')' is field 3, so starttime (field 22) is word 20 here.
  # shellcheck disable=SC2086
  set -- $rest
  [[ "${20-}" =~ ^[1-9][0-9]*$ ]] && printf '%s\n' "${20}"
}
same_identity() {
  local actual_start actual_exe
  kill -0 "$1" 2>/dev/null || return 1
  actual_start="$(starttime "$1")" || return 2
  actual_exe="$(readlink -f -- "$proc_root/$1/exe")" || return 2
  [[ "$actual_start" == "$2" && "$actual_exe" == "$3" ]] || return 2
}
identity_state() {
  # A changed starttime/exe means this PID is no longer the original leg.
  # Treat that original leg as absent, never as a candidate to signal.  The
  # other original leg can still be independently verified and contained.
  local rc
  if same_identity "$@"; then
    printf 'verified'
    return
  else
    rc=$?
  fi
  case "$rc" in
    1) printf 'absent' ;;
    *) printf 'mismatch' ;;
  esac
}
log "watchdog started pid1=$pid1 pid2=$pid2 grace=${grace}s"
one_dead_since=""
term_sent=false
last_alert=0
while true; do
  reap_alert
  s1="$(identity_state "$pid1" "$start1" "$exe1")"
  s2="$(identity_state "$pid2" "$start2" "$exe2")"
  a1=false
  a2=false
  [[ "$s1" == verified ]] && a1=true
  [[ "$s2" == verified ]] && a2=true
  if [[ "$a1" == false && "$a2" == false ]]; then
    if [[ "$s1" == mismatch || "$s2" == mismatch ]]; then
      log "ALERT: no original leg remains verified (leg1=$s1 leg2=$s2); refusing to signal a reused or unknown PID"
      exit 1
    fi
    if [[ "$term_sent" == true ]]; then
      log 'ALERT: both original legs exited after asymmetric containment'
      exit 1
    fi
    log "both original legs exited"
    exit 0
  fi
  if [[ "$a1" == true && "$a2" == true ]]; then
    one_dead_since=""
    term_sent=false
  else
    now="$(date +%s)"
    [[ -n "$one_dead_since" ]] || {
      one_dead_since="$now"
      log "one leg absent; grace timer started"
      if [[ "$a1" == false ]]; then event one; else event two; fi
    }
    if ((now - one_dead_since >= grace)); then
      survivor_pid="$pid1"
      survivor_start="$start1"
      survivor_exe="$exe1"
      [[ "$a2" == true ]] && {
        survivor_pid="$pid2"
        survivor_start="$start2"
        survivor_exe="$exe2"
      }
      if [[ "$term_sent" == false ]]; then
        if same_identity "$survivor_pid" "$survivor_start" "$survivor_exe"; then
          log "grace elapsed; sending SIGTERM to verified survivor pid=$survivor_pid"
          if [[ "$survivor_pid" == "$pid1" ]]; then event two; else event one; fi
          kill -TERM "$survivor_pid"
          term_sent=true
        else
          log "ALERT: verified survivor identity changed; no signal sent"
          exit 1
        fi
      elif ((now - last_alert >= 30)); then
        log "ALERT: verified survivor pid=$survivor_pid remains after SIGTERM; operator action required"
        last_alert="$now"
      fi
    fi
  fi
  sleep 1
done
