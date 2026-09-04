#!/usr/bin/env bash
# shellcheck disable=SC1007,SC2015,SC2174,SC2086,SC1132
# Safe two-leg launcher: dry-run unless --live is explicitly supplied.
set -euo pipefail
# Remove source-leg credentials from the inherited environment before *any*
# helper can be run. These shell-local values are deliberately not exported.
# The one intended key is exported only after launch() has sanitized the child
# environment, immediately before that shell execs the leg process.
pair_key1="${HL_AGENT_PK_LEG1:-}"
pair_key2="${HL_AGENT_PK_LEG2:-}"
pair_address1="${HL_AGENT_ADDRESS_LEG1:-}"
pair_address2="${HL_AGENT_ADDRESS_LEG2:-}"
pair_alert_hook_url="${HL_ALERT_HOOK_URL:-}"
# These are the only operator-owned runtime settings allowed through to a leg.
# Keep this a fixed list: accepting a caller-provided variable name would turn
# the pair launcher into a general environment-injection mechanism.
pair_ssl_cert_file="${SSL_CERT_FILE:-}"
pair_ssl_cert_dir="${SSL_CERT_DIR:-}"
pair_https_proxy="${HTTPS_PROXY:-}"
pair_http_proxy="${HTTP_PROXY:-}"
pair_all_proxy="${ALL_PROXY:-}"
pair_no_proxy="${NO_PROXY:-}"
pair_https_proxy_lower="${https_proxy:-}"
pair_http_proxy_lower="${http_proxy:-}"
pair_all_proxy_lower="${all_proxy:-}"
pair_no_proxy_lower="${no_proxy:-}"
pair_rust_log="${RUST_LOG:-}"
pair_rust_backtrace="${RUST_BACKTRACE:-}"
# Generic single-leg credentials must never bleed into either managed leg.
# Live mode below installs only the corresponding LEG1/LEG2 value after the
# child environment has been reduced to its public runtime minimum.
unset HL_AGENT_PK HL_AGENT_ADDRESS HL_AGENT_PK_LEG1 HL_AGENT_PK_LEG2 HL_AGENT_ADDRESS_LEG1 HL_AGENT_ADDRESS_LEG2 HL_ALERT_HOOK_URL
unset SSL_CERT_FILE SSL_CERT_DIR HTTPS_PROXY HTTP_PROXY ALL_PROXY NO_PROXY https_proxy http_proxy all_proxy no_proxy RUST_LOG RUST_BACKTRACE
D="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
BIN="${HYPE_TWAP_BIN:-$D/../target/release/hype-twap}"
RUNS_BIN="${HYPE_TWAP_RUNS_BIN:-$D/../target/release/hype-twap-runs}"
WD="${DN_WATCHDOG_BIN:-$D/dn-watchdog.sh}"
usage() { echo 'usage: dn-pair.sh {start options}|status --run-id ID [--log-dir DIR] [--json]|stop --run-id ID [--log-dir DIR] [--timeout SECS]|recover --run-id ID [--log-dir DIR]' >&2; }

export_leg_runtime_allowlist() {
  [[ -z "$pair_ssl_cert_file" ]] || export SSL_CERT_FILE="$pair_ssl_cert_file"
  [[ -z "$pair_ssl_cert_dir" ]] || export SSL_CERT_DIR="$pair_ssl_cert_dir"
  [[ -z "$pair_https_proxy" ]] || export HTTPS_PROXY="$pair_https_proxy"
  [[ -z "$pair_http_proxy" ]] || export HTTP_PROXY="$pair_http_proxy"
  [[ -z "$pair_all_proxy" ]] || export ALL_PROXY="$pair_all_proxy"
  [[ -z "$pair_no_proxy" ]] || export NO_PROXY="$pair_no_proxy"
  [[ -z "$pair_https_proxy_lower" ]] || export https_proxy="$pair_https_proxy_lower"
  [[ -z "$pair_http_proxy_lower" ]] || export http_proxy="$pair_http_proxy_lower"
  [[ -z "$pair_all_proxy_lower" ]] || export all_proxy="$pair_all_proxy_lower"
  [[ -z "$pair_no_proxy_lower" ]] || export no_proxy="$pair_no_proxy_lower"
  [[ -z "$pair_rust_log" ]] || export RUST_LOG="$pair_rust_log"
  [[ -z "$pair_rust_backtrace" ]] || export RUST_BACKTRACE="$pair_rust_backtrace"
}

# Manifest operational commands deliberately trust neither a PID alone nor a
# process name.  A signal is possible only after all three recorded identity
# components match the live /proc entry.
proc_root="${DN_PROC_ROOT:-/proc}"
proc_starttime() {
  local s rest
  s="$(<"$proc_root/$1/stat")" || return 1
  rest="${s##*) }"
  # shellcheck disable=SC2086
  set -- $rest
  [[ "${20-}" =~ ^[1-9][0-9]*$ ]] && printf '%s' "${20}"
}
proc_exe() { readlink -f -- "$proc_root/$1/exe"; }
same_pid_identity() {
  [[ "$1" =~ ^[1-9][0-9]*$ && -n "$2" && "$3" = /* ]] || return 1
  kill -0 "$1" 2>/dev/null || return 1
  [[ "$(proc_starttime "$1" 2>/dev/null || true)" == "$2" && "$(proc_exe "$1" 2>/dev/null || true)" == "$3" ]]
}
pid_identity_state() {
  local pid="$1" start="$2" executable="$3"
  [[ -n "$pid$start$executable" ]] || { printf 'absent'; return; }
  [[ "$pid" =~ ^[1-9][0-9]*$ && "$start" =~ ^[1-9][0-9]*$ && "$executable" = /* ]] || {
    printf 'mismatch'; return;
  }
  kill -0 "$pid" 2>/dev/null || { printf 'absent'; return; }
  if same_pid_identity "$pid" "$start" "$executable"; then
    printf 'verified'
  else
    printf 'mismatch'
  fi
}
read_manifest() {
  local run_dir="$1"
  MANIFEST="$run_dir/manifest.json"
  [[ "$run_dir" = /* && -d "$run_dir" && ! -L "$run_dir" && -f "$MANIFEST" && ! -L "$MANIFEST" ]] || {
    echo 'invalid run directory or manifest' >&2; return 2;
  }
  command -v jq >/dev/null || { echo 'jq is required for pair operations' >&2; return 2; }
  jq -e --arg rid "$RUN_ID" '.manifest_version == 1 and .run_id == $rid' "$MANIFEST" >/dev/null || {
    echo 'unsupported or invalid manifest; refusing operation' >&2; return 2;
  }
}
create_lifecycle_lock() {
  local run_dir="$1" lock="$1/lifecycle.lock"
  (umask 077; : >"$lock") || return 1
  sync -f "$lock" 2>/dev/null || true
}
acquire_lifecycle_lock() {
  local run_dir="$1" lock="$1/lifecycle.lock"
  [[ -f "$lock" && ! -L "$lock" ]] || {
    echo 'missing or unsafe lifecycle lock; refusing concurrent pair operation' >&2
    return 1
  }
  exec {LIFECYCLE_LOCK_FD}>"$lock"
  flock -n "$LIFECYCLE_LOCK_FD" || {
    echo 'pair lifecycle operation already in progress; retry after it completes' >&2
    exec {LIFECYCLE_LOCK_FD}>&-
    return 1
  }
}
manifest_state() {
  local state="$1" temp
  temp="$(mktemp "$run/.manifest.XXXXXX")"
  jq --arg state "$state" --arg at "$(date -u +%FT%TZ)" '.state=$state | .updated_at=$at | .lifecycle.state=$state | .lifecycle.updated_at=$at' "$run/manifest.json" >"$temp"
  sync -f "$temp" 2>/dev/null || true
  mv -f -- "$temp" "$run/manifest.json"
  sync -f "$run" 2>/dev/null || true
}
# Classify an explicitly mapped child journal through the same validated
# replay used by `hype-twap-runs`.  `recover` is diagnostic-only, but its
# diagnosis must not call an incomplete/corrupt journal terminal merely
# because arbitrary text happens to contain "FinalReport" or "Abandoned".
#
# The state root and run id are already manifest-scoped; the identity check
# below also makes a stale mapping fail closed instead of borrowing a journal
# for another symbol/side.  This helper never writes state.
journal_recovery_state() {
  local sd="$1" journal_id="$2" symbol="$3" side="$4" inspect status
  [[ "$sd" = /* && -d "$sd" && ! -L "$sd" && -d "$sd/runs" && ! -L "$sd/runs" ]] || {
    printf 'declared:%s+invalid-or-state-dir-missing' "$journal_id"; return
  }
  [[ -x "$RUNS_BIN" ]] || {
    printf 'present:%s+UNVERIFIED-runs-cli-unavailable' "$journal_id"; return
  }
  # `verify` rejects malformed/corrupt journals.  Do not fall back to a text
  # search if the shared replay is unavailable or rejects the journal.
  if ! "$RUNS_BIN" --state-dir "$sd" verify "$journal_id" >/dev/null 2>&1; then
    printf 'present:%s+INVALID-OR-CORRUPT' "$journal_id"; return
  fi
  inspect="$("$RUNS_BIN" --state-dir "$sd" inspect "$journal_id" 2>/dev/null)" || {
    printf 'present:%s+INVALID-OR-CORRUPT' "$journal_id"; return
  }
  if ! jq -e --arg symbol "$symbol" --arg side "$side" '
      .schema_version == 1 and .validation.valid == true and
      .identity.symbol == $symbol and .identity.side == $side and
      (.status == "success" or .status == "incomplete" or .status == "abandoned")
    ' <<<"$inspect" >/dev/null 2>&1; then
    printf 'present:%s+INVALID-OR-CORRUPT' "$journal_id"; return
  fi
  status="$(jq -r .status <<<"$inspect")"
  case "$status" in
    success) printf 'present:%s+VALIDATED-SUCCESS' "$journal_id" ;;
    incomplete) printf 'present:%s+VALIDATED-INCOMPLETE' "$journal_id" ;;
    abandoned) printf 'present:%s+VALIDATED-ABANDONED' "$journal_id" ;;
    *) printf 'present:%s+INVALID-OR-CORRUPT' "$journal_id" ;;
  esac
}
pair_operation() {
  local op="$1"; shift
  local base="${XDG_STATE_HOME:-$HOME/.local/state}/hype-twap/pairs" run_dir="" json=false timeout=10
  RUN_ID=""
  while (($#)); do case "$1" in
    --run-id) [[ $# -ge 2 ]] || { echo 'missing --run-id value' >&2; return 2; }; RUN_ID="$2"; shift 2 ;;
    --log-dir) [[ $# -ge 2 ]] || { echo 'missing --log-dir value' >&2; return 2; }; base="$2"; shift 2 ;;
    --json) json=true; shift ;;
    --timeout) [[ $# -ge 2 && "$2" =~ ^[0-9]+$ && "$2" -le 3600 ]] || { echo 'invalid --timeout' >&2; return 2; }; timeout="$2"; shift 2 ;;
    *) echo "unknown $op argument: $1" >&2; return 2 ;;
  esac; done
  [[ "$RUN_ID" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$ ]] || { echo "$op requires a safe --run-id" >&2; return 2; }
  [[ "$base" = /* && -d "$base" && ! -L "$base" ]] || { echo 'invalid --log-dir root' >&2; return 2; }
  [[ "$(stat -c %u -- "$base")" == "$UID" && "$(stat -c %a -- "$base")" == 700 ]] || {
    echo '--log-dir root must be owned by the invoking user and mode 0700' >&2
    return 2
  }
  run_dir="$base/$RUN_ID"
  acquire_lifecycle_lock "$run_dir" || return 1
  read_manifest "$run_dir" || return $?
  local p st ex name identity
  local -a rows=()
  for name in leg1 leg2 watchdog; do
    p="$(jq -r ".$name.pid // empty" "$MANIFEST")"; st="$(jq -r ".$name.starttime // empty" "$MANIFEST")"; ex="$(jq -r ".$name.executable // empty" "$MANIFEST")"
    identity="$(pid_identity_state "$p" "$st" "$ex")"
    rows+=("$name:$identity:${p:-none}")
  done
  case "$op" in
    status)
      if $json; then
        jq --arg leg1 "${rows[0]#leg1:}" --arg leg2 "${rows[1]#leg2:}" --arg watchdog "${rows[2]#watchdog:}" '. + {identity:{leg1:$leg1,leg2:$leg2,watchdog:$watchdog}}' "$MANIFEST"
      else printf 'run_id=%s state=%s\n%s\n%s\n%s\n' "$(jq -r .run_id "$MANIFEST")" "$(jq -r .state "$MANIFEST")" "${rows[0]}" "${rows[1]}" "${rows[2]}"; fi
      ;;
    stop)
      # Contain execution legs first.  Keeping the watchdog alive while a
      # leg still needs cancel/settle cleanup avoids creating an unmonitored
      # one-leg state just because an operator requested stop.
      for name in leg1 leg2; do
        p="$(jq -r ".$name.pid // empty" "$MANIFEST")"; st="$(jq -r ".$name.starttime // empty" "$MANIFEST")"; ex="$(jq -r ".$name.executable // empty" "$MANIFEST")"
        if [[ "$(pid_identity_state "$p" "$st" "$ex")" == verified ]]; then
          kill -TERM "$p" || true
        else
          printf 'refusing signal: %s is absent or has an identity mismatch\n' "$name" >&2
        fi
      done
      local deadline=$((SECONDS + timeout))
      while ((SECONDS < deadline)); do
        local any_leg_verified=false
        for name in leg1 leg2; do p="$(jq -r ".$name.pid // empty" "$MANIFEST")"; st="$(jq -r ".$name.starttime // empty" "$MANIFEST")"; ex="$(jq -r ".$name.executable // empty" "$MANIFEST")"; [[ "$(pid_identity_state "$p" "$st" "$ex")" == verified ]] && any_leg_verified=true; done
        $any_leg_verified || break; sleep 1
      done
      # Do not escalate an operational stop to SIGKILL.  A TWAP child can be
      # settling a cancel/late-fill race after TERM; killing it would erase
      # its in-process cleanup.  The durable manifest stays non-clean until
      # every recorded identity is observed absent.
      local legs_absent=true
      rows=()
      for name in leg1 leg2; do
        p="$(jq -r ".$name.pid // empty" "$MANIFEST")"; st="$(jq -r ".$name.starttime // empty" "$MANIFEST")"; ex="$(jq -r ".$name.executable // empty" "$MANIFEST")"
        identity="$(pid_identity_state "$p" "$st" "$ex")"
        rows+=("$name:$identity:${p:-none}")
        [[ "$identity" == absent ]] || legs_absent=false
      done
      if $legs_absent; then
        p="$(jq -r '.watchdog.pid // empty' "$MANIFEST")"; st="$(jq -r '.watchdog.starttime // empty' "$MANIFEST")"; ex="$(jq -r '.watchdog.executable // empty' "$MANIFEST")"
        if [[ "$(pid_identity_state "$p" "$st" "$ex")" == verified ]]; then
          kill -TERM "$p" || true
        fi
        while ((SECONDS < deadline)); do
          [[ "$(pid_identity_state "$p" "$st" "$ex")" == verified ]] || break
          sleep 1
        done
      fi
      # Re-check all identities only after the ordered stop sequence.
      rows=()
      local all_absent=true
      for name in leg1 leg2 watchdog; do
        p="$(jq -r ".$name.pid // empty" "$MANIFEST")"; st="$(jq -r ".$name.starttime // empty" "$MANIFEST")"; ex="$(jq -r ".$name.executable // empty" "$MANIFEST")"
        identity="$(pid_identity_state "$p" "$st" "$ex")"
        rows+=("$name:$identity:${p:-none}")
        [[ "$identity" == absent ]] || all_absent=false
      done
      run="$run_dir"
      if $all_absent; then
        manifest_state stopped
        printf 'stopped %s\n' "$(jq -r .run_id "$MANIFEST")"
      else
        manifest_state stop_partial
        printf 'stop_partial %s: residual risk; verify exchange positions/orders and investigate process identity before manual hedge\n%s\n%s\n%s\n' "$(jq -r .run_id "$MANIFEST")" "${rows[0]}" "${rows[1]}" "${rows[2]}" >&2
        return 1
      fi
      ;;
    recover)
      # Diagnostic-only: it must not spawn, signal, rewrite, or remove files.
      local log1 log2 journal_note state state1 state2 journal1 journal2
      log1="$(jq -r '.leg1.log // empty' "$MANIFEST")"; log2="$(jq -r '.leg2.log // empty' "$MANIFEST")"; state="$(jq -r .state "$MANIFEST")"
      state1="$(jq -r '.leg1.state_dir // empty' "$MANIFEST")"; state2="$(jq -r '.leg2.state_dir // empty' "$MANIFEST")"
      journal1='not declared'; journal2='not declared'
      for name in 1 2; do
        local sd result journal_id symbol side
        [[ "$name" == 1 ]] && sd="$state1" || sd="$state2"
        [[ "$name" == 1 ]] && symbol="$(jq -r '.leg1.symbol // empty' "$MANIFEST")" || symbol="$(jq -r '.leg2.symbol // empty' "$MANIFEST")"
        [[ "$name" == 1 ]] && side="$(jq -r '.leg1.side // empty' "$MANIFEST")" || side="$(jq -r '.leg2.side // empty' "$MANIFEST")"
        journal_id="$(jq -r ".leg$name.journal_run_id // empty" "$MANIFEST")"
        if [[ -n "$journal_id" && "$journal_id" =~ ^[A-Za-z0-9_-]+$ && -d "$sd/runs" ]]; then
          if [[ -d "$sd/runs/$journal_id" && ! -L "$sd/runs/$journal_id" && -f "$sd/runs/$journal_id/journal.jsonl" && ! -L "$sd/runs/$journal_id/journal.jsonl" ]]; then
            result="$(journal_recovery_state "$sd" "$journal_id" "$symbol" "$side")"
          else result="declared:$journal_id+missing-or-unsafe"; fi
        elif [[ -n "$journal_id" ]]; then
          result="declared:$journal_id+invalid-or-state-dir-missing"
        else
          # Never attach an arbitrary first journal from a shared state root.
          result='unmapped (no deterministic journal association)'
        fi
        [[ "$name" == 1 ]] && journal1="$result" || journal2="$result"
      done
      journal_note="journals: leg1=$journal1 leg2=$journal2"
      [[ -n "$log1" && -f "$log1" ]] || log1='missing'
      [[ -n "$log2" && -f "$log2" ]] || log2='missing'
      printf 'recover diagnostic (read-only): run_id=%s state=%s\n%s\n%s\n%s\nleg logs: leg1=%s leg2=%s\n%s\nresidual risk: if one leg/watchdog is absent while another is alive, use status then an explicit stop or verify exchange positions/orders and per-leg journals before any manual hedge; no process, signal, order, or file was changed\n' "$(jq -r .run_id "$MANIFEST")" "$state" "${rows[0]}" "${rows[1]}" "${rows[2]}" "$log1" "$log2" "$journal_note"
      ;;
  esac
}
case "${1-}" in
  status|stop|recover) pair_operation "$@"; exit $? ;;
esac
l1s= l1d= l1u= l2s= l2d= l2u= dur= slices= cap1= cap2=
live=false
ro=true
ro_true_seen=false
a1=follow
a2=follow
grace=0
barrier_timeout=30
root="${XDG_STATE_HOME:-$HOME/.local/state}/hype-twap/pairs"
ratio=1
tolerance=100
same=false
imbalance=false
declare -a x1=() x2=()
need() { [[ $# -ge 2 && -n "${2-}" && "${2-}" != --* ]] || {
  echo "missing value for $1" >&2
  exit 2
}; }
while (($#)); do case "$1" in
  --live)
    live=true
    ro=false
    shift
    ;;
  --read-only)
    need "$@"
    [[ "$2" == true || "$2" == false ]] || {
      echo 'invalid --read-only' >&2
      exit 2
    }
    [[ "$2" == true ]] && ro_true_seen=true
    [[ "$2" == false ]] && {
      echo '--read-only false is deprecated; use --live' >&2
      live=true
      ro=false
    }
    shift 2
    ;;
  --leg1-symbol | --leg1-side | --leg1-usd | --leg2-symbol | --leg2-side | --leg2-usd | --duration | --slices | --leg1-max-notional-usd | --leg2-max-notional-usd | --leg1-child-algo | --leg2-child-algo | --log-dir | --watchdog-grace | --pair-barrier-timeout | --hedge-ratio | --hedge-tolerance-bps)
    need "$@"
    case "$1" in --leg1-symbol) l1s="$2" ;; --leg1-side) l1d="$2" ;; --leg1-usd) l1u="$2" ;; --leg2-symbol) l2s="$2" ;; --leg2-side) l2d="$2" ;; --leg2-usd) l2u="$2" ;; --duration) dur="$2" ;; --slices) slices="$2" ;; --leg1-max-notional-usd) cap1="$2" ;; --leg2-max-notional-usd) cap2="$2" ;; --leg1-child-algo) a1="$2" ;; --leg2-child-algo) a2="$2" ;; --log-dir) root="$2" ;; --watchdog-grace) grace="$2" ;; --pair-barrier-timeout) barrier_timeout="$2" ;; --hedge-ratio) ratio="$2" ;; --hedge-tolerance-bps) tolerance="$2" ;; esac
    shift 2
    ;;
  --allow-same-side)
    same=true
    shift
    ;;
  --allow-hedge-imbalance)
    imbalance=true
    shift
    ;;
  --leg1-network | --leg1-state-dir | --leg1-trigger-price | --leg1-trigger-when | --leg1-start-after | --leg1-expire-after | --leg1-slippage-bps | --leg1-max-book-age-ms | --leg1-follow-poll-secs | --leg1-follow-repost-secs | --leg1-follow-threshold-bps | --leg1-wait-network-grace)
    need "$@"
    x1+=("--${1#--leg1-}" "$2")
    shift 2
    ;;
  --leg2-network | --leg2-state-dir | --leg2-trigger-price | --leg2-trigger-when | --leg2-start-after | --leg2-expire-after | --leg2-slippage-bps | --leg2-max-book-age-ms | --leg2-follow-poll-secs | --leg2-follow-repost-secs | --leg2-follow-threshold-bps | --leg2-wait-network-grace)
    need "$@"
    x2+=("--${1#--leg2-}" "$2")
    shift 2
    ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    echo "unknown argument: $1" >&2
    exit 2
    ;;
  esac done
[[ "$live" == false || "$ro_true_seen" == false ]] || {
  echo '--live conflicts with --read-only true' >&2
  exit 2
}
for v in l1s l1d l1u l2s l2d l2u dur slices; do [[ -n "${!v}" ]] || {
  echo "missing $v" >&2
  exit 2
}; done
for v in l1s l2s; do [[ "${!v}" =~ ^[A-Za-z0-9][A-Za-z0-9._:-]{0,63}$ ]] || {
  echo 'invalid symbol' >&2
  exit 2
}; done
for v in l1u l2u ratio; do [[ "${!v}" =~ ^[0-9]+(\.[0-9]+)?$ ]] && awk "BEGIN{exit !(${!v}>0)}" || {
  echo 'invalid USD/ratio' >&2
  exit 2
}; done
[[ "$l1d" =~ ^(long|short)$ && "$l2d" =~ ^(long|short)$ && "$a1" =~ ^(market|passive|follow)$ && "$a2" =~ ^(market|passive|follow)$ && "$slices" =~ ^[1-9][0-9]*$ && "$dur" =~ ^[1-9][0-9]*[smhd]$ && "$grace" =~ ^[0-9]+$ && "$grace" -le 3600 && "$barrier_timeout" =~ ^[1-9][0-9]*$ && "$barrier_timeout" -le 3600 && "$tolerance" =~ ^[0-9]+$ && "$tolerance" -le 10000 ]] || {
  echo 'invalid execution setting' >&2
  exit 2
}
validate_leg_options() {
  local -n opts="$1"
  local i=0 flag value
  while ((i < ${#opts[@]})); do
    flag="${opts[i]}"
    value="${opts[i + 1]}"
    case "$flag" in
      --network) [[ "$value" =~ ^(mainnet|testnet)$ ]]
        ;;
      --trigger-when) [[ "$value" =~ ^(above|below)$ ]]
        ;;
      --state-dir) [[ "$value" = /* && "$value" != *$'\n'* ]]
        ;;
      --trigger-price) [[ "$value" =~ ^[0-9]+(\.[0-9]+)?$ ]] && awk "BEGIN { exit !($value > 0) }"
        ;;
      # Zero slippage is valid in the Rust CLI.  The pair launcher deliberately
      # stops at the normal 1000 bps safety threshold because it does not expose
      # the single-leg --allow-high-slippage unsafe override.
      --slippage-bps) [[ "$value" =~ ^[0-9]+(\.[0-9]+)?$ ]] && awk "BEGIN { exit !($value >= 0 && $value <= 1000) }"
        ;;
      --follow-threshold-bps) [[ "$value" =~ ^[0-9]+(\.[0-9]+)?$ ]]
        ;;
      --max-book-age-ms) [[ "$value" =~ ^[0-9]+$ ]]
        ;;
      --follow-poll-secs | --follow-repost-secs) [[ "$value" =~ ^[1-9][0-9]*$ ]]
        ;;
      --start-after | --expire-after | --wait-network-grace) [[ "$value" =~ ^[1-9][0-9]*[smhd]$ ]]
        ;;
      *) false
        ;;
    esac || {
      echo "invalid $flag value" >&2
      exit 2
    }
    ((i += 2))
  done
}
validate_leg_options x1
validate_leg_options x2
state_dir_from_options() {
  local -n opts="$1"
  local i
  for ((i = 0; i < ${#opts[@]}; i += 2)); do
    [[ "${opts[i]}" == --state-dir ]] && { printf '%s' "${opts[i + 1]}"; return; }
  done
  return 0
}
leg1_state_dir="$(state_dir_from_options x1)"
leg2_state_dir="$(state_dir_from_options x2)"
default_state_dir="${HOME:-/nonexistent}/.local/state/hype-twap"
[[ -n "$leg1_state_dir" ]] || leg1_state_dir="$default_state_dir"
[[ -n "$leg2_state_dir" ]] || leg2_state_dir="$default_state_dir"
[[ "$l1d" != "$l2d" || "$same" == true ]] || {
  echo 'same side requires --allow-same-side' >&2
  exit 2
}
# Compare |b - a*r| * 10000 <= a*r*t directly.  Avoiding b/a prevents an
# exact tolerance boundary (for example 100 vs 101 at 100 bps) from flipping
# to rejection because of floating-point cancellation in awk.
awk -v a="$l1u" -v b="$l2u" -v r="$ratio" -v t="$tolerance" 'BEGIN{x=b-a*r;if(x<0)x=-x;exit !(x*10000<=a*r*t)}' || [[ "$imbalance" == true ]] || {
  echo 'hedge ratio outside tolerance' >&2
  exit 2
}
if $live; then
  for v in cap1 cap2; do [[ "${!v}" =~ ^[0-9]+(\.[0-9]+)?$ ]] && awk "BEGIN{exit !(${!v}>0)}" || {
    echo 'live requires per-leg cap' >&2
    exit 2
  }; done
  [[ -n "$pair_key1" && -n "$pair_key2" ]] || {
    echo 'live requires leg keys' >&2
    exit 1
  }
  k1="${pair_key1#0x}"
  k1="${k1,,}"
  k2="${pair_key2#0x}"
  k2="${k2,,}"
  [[ "$k1" != "$k2" ]] || {
    echo 'leg keys must differ' >&2
    exit 1
  }
fi
[[ -x "$BIN" && -x "$WD" ]] || {
  echo 'binary/watchdog unavailable' >&2
  exit 1
}
command -v jq >/dev/null || { echo 'jq is required for dn-pair manifest/barrier safety' >&2; exit 1; }
BIN="$(readlink -f -- "$BIN")"
WD="$(readlink -f -- "$WD")"
umask 077
[[ "$root" = /* ]] || { echo '--log-dir must be an absolute path' >&2; exit 2; }
[[ ! -L "$root" ]] || { echo 'refusing symlink --log-dir root' >&2; exit 2; }
mkdir -p -m700 -- "$root"
[[ -d "$root" && ! -L "$root" ]] || { echo 'invalid --log-dir root' >&2; exit 2; }
root_owner="$(stat -c %u -- "$root")"
root_mode="$(stat -c %a -- "$root")"
[[ "$root_owner" == "$UID" && "$root_mode" == 700 ]] || {
  echo '--log-dir root must be owned by the invoking user and mode 0700' >&2
  exit 2
}
root="$(cd -- "$root" && pwd -P)"
[[ "$root" != *$'\n'* && "$root" != *$'\r'* ]] || { echo 'invalid --log-dir' >&2; exit 2; }
rid="pair-$(date -u +%Y%m%dT%H%M%S%N)-$$-$RANDOM"
run="$root/$rid"
mkdir -m700 -- "$run"
create_lifecycle_lock "$run" || { echo 'failed to create pair lifecycle lock' >&2; exit 1; }
acquire_lifecycle_lock "$run" || exit 1
id() {
  local s rest
  s="$(<"/proc/$1/stat")" || return 1
  rest="${s##*) }"
  set -- $rest
  printf '%s' "${20-}"
}
exe() { readlink -f -- "/proc/$1/exe"; }
esc() {
  local q="$1"
  q="${q//\\/\\\\}"
  q="${q//\"/\\\"}"
  printf '%s' "$q"
}
p1= p2= st1= st2= exe1= exe2= wp= wst= wexe=
journal1= journal2=
# This is deliberately a conservative release latch, not a reflection of
# start.json's current directory entry.  It becomes true *before* the atomic
# rename so a signal in the publication boundary takes the post-release
# cleanup path: a child must never be left executing after its watchdog has
# been stopped just because the shell has not run a subsequent assignment.
start_release_may_be_visible=false
startup_cleanup_timeout=10
ready1="$run/leg1.ready.json"
ready2="$run/leg2.ready.json"
start_file="$run/start.json"
log1="$run/leg1.log"
log2="$run/leg2.log"
created_at="$(date -u +%FT%TZ)"
same() { [[ -n "${1:-}" && -n "${2:-}" && -n "${3:-}" ]] && [[ "$(id "$1" 2>/dev/null || true)" == "$2" && "$(exe "$1" 2>/dev/null || true)" == "$3" ]]; }
cleanup() {
  local rc=$? deadline legs_absent=true
  trap - EXIT INT TERM HUP
  if ! $start_release_may_be_visible; then
    # Before the common release neither child may trade, so the original
    # rollback is safe: stop the watchdog and both preflight children.
    if same "$wp" "$wst" "$wexe"; then kill -TERM "$wp" 2>/dev/null || true; fi
    if same "$p1" "$st1" "$exe1"; then kill -TERM "$p1" 2>/dev/null || true; fi
    if same "$p2" "$st2" "$exe2"; then kill -TERM "$p2" 2>/dev/null || true; fi
    manifest failed 2>/dev/null || true
    exit "$rc"
  fi

  # Once start.json is public a leg may already be in execution.  Never kill
  # its watchdog first: request graceful TERM for verified legs, wait a
  # bounded interval for cancel/settle cleanup, then decide from fresh
  # identities.  If either leg remains, retain the watchdog so it continues
  # to contain and alert on the asymmetric state.
  if same "$p1" "$st1" "$exe1"; then kill -TERM "$p1" 2>/dev/null || true; fi
  if same "$p2" "$st2" "$exe2"; then kill -TERM "$p2" 2>/dev/null || true; fi
  deadline=$((SECONDS + startup_cleanup_timeout))
  while ((SECONDS < deadline)); do
    same "$p1" "$st1" "$exe1" || same "$p2" "$st2" "$exe2" || break
    sleep 1
  done
  same "$p1" "$st1" "$exe1" && legs_absent=false
  same "$p2" "$st2" "$exe2" && legs_absent=false
  if $legs_absent; then
    # Both execution legs are gone; the watchdog has no residual to protect.
    if same "$wp" "$wst" "$wexe"; then kill -TERM "$wp" 2>/dev/null || true; fi
    manifest start_failed 2>/dev/null || true
  else
    manifest start_failed_partial 2>/dev/null || true
    echo 'start_failed_partial: a released leg remains after bounded graceful cleanup; watchdog retained for containment; verify exchange positions/orders before manual hedge' >&2
  fi
  exit "$rc"
}
manifest() {
  local state="$1" t x1_json x2_json
  t="$(mktemp "$run/.manifest.XXXXXX")"
  x1_json="$(jq -n '$ARGS.positional' --args -- "${c1[@]}" "${x1[@]}")"
  x2_json="$(jq -n '$ARGS.positional' --args -- "${c2[@]}" "${x2[@]}")"
  jq -n --arg rid "$rid" --arg state "$state" --arg created "$created_at" --arg updated "$(date -u +%FT%TZ)" --argjson live "$live" --argjson grace "$grace" --argjson barrier_timeout "$barrier_timeout" --arg ready1 "$ready1" --arg ready2 "$ready2" --arg start "$start_file" --argjson same "$same" --argjson imbalance "$imbalance" --arg duration "$dur" --argjson slices "$slices" --arg ratio "$ratio" --argjson tolerance "$tolerance" --arg a1 "$a1" --arg a2 "$a2" --argjson x1 "$x1_json" --argjson x2 "$x2_json" --arg wp "$wp" --arg wst "$wst" --arg wexe "$wexe" --arg l1s "$l1s" --arg l1d "$l1d" --arg l1u "$l1u" --arg cap1 "$cap1" --arg p1 "$p1" --arg st1 "$st1" --arg exe1 "$exe1" --arg log1 "$log1" --arg state1 "$leg1_state_dir" --arg journal1 "$journal1" --arg l2s "$l2s" --arg l2d "$l2d" --arg l2u "$l2u" --arg cap2 "$cap2" --arg p2 "$p2" --arg st2 "$st2" --arg exe2 "$exe2" --arg log2 "$log2" --arg state2 "$leg2_state_dir" --arg journal2 "$journal2" '{manifest_version:1,run_id:$rid,state:$state,created_at:$created,updated_at:$updated,lifecycle:{state:$state,updated_at:$updated},live:$live,watchdog_grace:$grace,barrier:{timeout_seconds:$barrier_timeout,ready1:$ready1,ready2:$ready2,start_file:$start},unsafe:{allow_same_side:$same,allow_hedge_imbalance:$imbalance},effective:{duration:$duration,slices:$slices,hedge_ratio:$ratio,hedge_tolerance_bps:$tolerance,leg1_child_algo:$a1,leg2_child_algo:$a2,leg1_args:$x1,leg2_args:$x2},watchdog:{pid:$wp,starttime:$wst,executable:$wexe},leg1:{symbol:$l1s,side:$l1d,usd:$l1u,cap:$cap1,pid:$p1,starttime:$st1,executable:$exe1,log:$log1,state_dir:$state1,journal_run_id:$journal1},leg2:{symbol:$l2s,side:$l2d,usd:$l2u,cap:$cap2,pid:$p2,starttime:$st2,executable:$exe2,log:$log2,state_dir:$state2,journal_run_id:$journal2}}' >"$t"
  sync -f "$t" 2>/dev/null || true
  mv -f -- "$t" "$run/manifest.json"
  sync -f "$run" 2>/dev/null || true
}
launch() {
  local key="$1" addr="$2" out="$3"
  shift 3
  # Do not use `env -i HL_AGENT_PK=...`: environment assignments supplied to
  # an external command are command-line arguments and can be observed via
  # /proc/<pid>/cmdline while that helper is executing.
  (
    # shellcheck disable=SC2030 # Deliberately scoped to the child only.
    export PATH="${PATH:-/usr/bin:/bin}" HOME="${HOME:-/nonexistent}" TMPDIR="${TMPDIR:-/tmp}"
    # This helper runs before the intended key is exported. Source-leg keys
    # were removed at process entry, so it cannot inherit either of them.
    # Do not let the launcher's lifecycle lock escape into a long-lived leg.
    [[ -z "${LIFECYCLE_LOCK_FD:-}" ]] || exec {LIFECYCLE_LOCK_FD}>&-
    while IFS='=' read -r variable _; do
      case "$variable" in PATH|HOME|TMPDIR) ;; *) unset "$variable" ;; esac
    done < <(PAIR_ENV_STAGE=sanitize env)
    export_leg_runtime_allowlist
    # The hook stays shell-local until the environment is reduced.  Export it
    # directly here, never as an `env` argv assignment, so it reaches the leg
    # without appearing in the sanitizer, command line, manifest, or logs.
    # shellcheck disable=SC2030 # This assignment is intentionally child-local.
    [[ -z "$pair_alert_hook_url" ]] || export HL_ALERT_HOOK_URL="$pair_alert_hook_url"
    if $live; then
      export HL_AGENT_PK="$key"
      [[ -n "$addr" ]] && export HL_AGENT_ADDRESS="$addr"
    fi
    exec setsid "$BIN" "$@"
  ) >>"$out" 2>&1 &
  echo $!
}
ready_is_valid() {
  local file="$1" expected_pid="$2"
  [[ -f "$file" && ! -L "$file" ]] || return 1
  jq -e --arg rid "$rid" --argjson pid "$expected_pid" '
    .run_id == $rid and (.ready_at_unix_ms|type == "number") and .pid == $pid and
    ((.journal_run_id // null) == null or (.journal_run_id | type == "string" and test("^[A-Za-z0-9_-]+$")))
  ' "$file" >/dev/null 2>&1
}
publish_start() {
  local future tmp
  future="$(( $(date +%s%3N) + 500 ))"
  tmp="$(mktemp "$run/.start.XXXXXX")"
  printf '{"run_id":"%s","start_at_unix_ms":%s}\n' "$rid" "$future" >"$tmp"
  sync -f "$tmp" 2>/dev/null || true
  # Latch before the rename.  Bash can run a pending signal trap after any
  # command, including `mv`; treating this small pre-rename window as
  # released is conservative and preserves watchdog containment.
  start_release_may_be_visible=true
  mv -f -- "$tmp" "$start_file"
  sync -f "$run" 2>/dev/null || true
}
wait_for_both_ready() {
  local deadline=$((SECONDS + barrier_timeout))
  while ((SECONDS < deadline)); do
    if ready_is_valid "$ready1" "$p1" && ready_is_valid "$ready2" "$p2"; then return 0; fi
    # A leg that dies before ready is an immediate rollback, not a timeout.
    same "$p1" "$st1" "$exe1" && same "$p2" "$st2" "$exe2" || return 1
    sleep 0.05
  done
  return 1
}
adopt_ready_journal_mappings() {
  local file state_dir symbol side journal_id journal_file
  local -a files=("$ready1" "$ready2") states=("$leg1_state_dir" "$leg2_state_dir") symbols=("$l1s" "$l2s") sides=("$l1d" "$l2d")
  journal1=
  journal2=
  for index in 0 1; do
    file="${files[$index]}"; state_dir="${states[$index]}"; symbol="${symbols[$index]}"; side="${sides[$index]}"
    journal_id="$(jq -r '.journal_run_id // empty' "$file")"
    if $ro; then
      [[ -z "$journal_id" ]] || return 1
      continue
    fi
    [[ "$journal_id" =~ ^[A-Za-z0-9_-]+$ && "$state_dir" = /* && -d "$state_dir/runs/$journal_id" && ! -L "$state_dir/runs/$journal_id" ]] || return 1
    journal_file="$state_dir/runs/$journal_id/journal.jsonl"
    [[ -f "$journal_file" && ! -L "$journal_file" ]] || return 1
    jq -e --arg journal_id "$journal_id" --arg symbol "$symbol" --arg side "$side" '
      select(.kind == "Header") | .run_id == $journal_id and .symbol == $symbol and .side == $side
    ' "$journal_file" >/dev/null 2>&1 || return 1
    [[ "$index" == 0 ]] && journal1="$journal_id" || journal2="$journal_id"
  done
}
wait_for_watchdog_healthy() {
  local deadline=$((SECONDS + 2))
  while ((SECONDS < deadline)); do
    same "$wp" "$wst" "$wexe" || return 1
    grep -q 'watchdog started ' "$run/watchdog.log" 2>/dev/null && return 0
    sleep 0.05
  done
  return 1
}
c1=(--symbol "$l1s" --side "$l1d" --usd "$l1u" --duration "$dur" --slices "$slices" --child-algo "$a1" --read-only "$ro" --pair-ready-file "$ready1" --pair-start-file "$start_file" --pair-run-id "$rid" --pair-barrier-timeout "${barrier_timeout}s")
c2=(--symbol "$l2s" --side "$l2d" --usd "$l2u" --duration "$dur" --slices "$slices" --child-algo "$a2" --read-only "$ro" --pair-ready-file "$ready2" --pair-start-file "$start_file" --pair-run-id "$rid" --pair-barrier-timeout "${barrier_timeout}s")
$live && {
  c1+=(--max-notional-usd "$cap1")
  c2+=(--max-notional-usd "$cap2")
}
manifest starting
trap cleanup EXIT
# Preserve a conventional non-zero signal status.  `trap cleanup TERM` enters
# cleanup with a successful `$?` on bash and could otherwise report a
# pre-release rollback as success.
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP
if ! $live; then
  printf 'READ-ONLY / DRY-RUN: no orders will be placed. Use --live for execution.\n'
fi
p1="$(launch "$pair_key1" "$pair_address1" "$log1" "${c1[@]}" "${x1[@]}")"
st1="$(id "$p1")"
exe1="$(exe "$p1")" || exit 1
p2="$(launch "$pair_key2" "$pair_address2" "$log2" "${c2[@]}" "${x2[@]}")"
st2="$(id "$p2")"
exe2="$(exe "$p2")" || exit 1
# Start and prove the watchdog before releasing the pair barrier. The
# lifecycle FD is closed in its child so this run's lock is not held forever.
(
  exec {LIFECYCLE_LOCK_FD}>&-
  # shellcheck disable=SC2031 # Child-only environment normalization is intentional.
  export PATH="${PATH:-/usr/bin:/bin}" HOME="${HOME:-/nonexistent}" TMPDIR="${TMPDIR:-/tmp}"
  # Normalize before exporting the private hook URL. In particular, never
  # pass an `HL_ALERT_HOOK_URL=value` assignment as an `env` argv element:
  # even a query-free URL may carry a private path and must stay out of
  # /proc/<pid>/cmdline.
  while IFS='=' read -r variable _; do
    case "$variable" in PATH|HOME|TMPDIR) ;; *) unset "$variable" ;; esac
  done < <(PAIR_ENV_STAGE=watchdog-sanitize env)
  # shellcheck disable=SC2031 # This assignment is intentionally child-local.
  [[ -z "$pair_alert_hook_url" ]] || export HL_ALERT_HOOK_URL="$pair_alert_hook_url"
  exec setsid "$WD" --pid1 "$p1" --pid1-starttime "$st1" --pid1-exe "$exe1" --pid2 "$p2" --pid2-starttime "$st2" --pid2-exe "$exe2" --grace "$grace" --log "$run/watchdog.log" --event-log "$run/events.jsonl"
) >>"$run/watchdog.launch.log" 2>&1 &
wp=$!
sleep 0.1
wst="$(id "$wp")"
wexe="$(exe "$wp")" || exit 1
wait_for_watchdog_healthy || { echo 'watchdog failed health check before common release; rolling back both legs' >&2; exit 1; }
manifest waiting_ready
wait_for_both_ready || { echo 'pair barrier failed before common release; rolling back both legs' >&2; exit 1; }
adopt_ready_journal_mappings || { echo 'pair ready journal mapping is missing, unsafe, or does not match its immutable leg identity; rolling back before common release' >&2; exit 1; }
manifest both_ready
publish_start
manifest start_released
sleep 1
same "$p1" "$st1" "$exe1" && same "$p2" "$st2" "$exe2" || exit 1
manifest watching
trap - EXIT INT TERM HUP
{
  printf 'launcher success: watchdog_grace=%ss\n' "$grace"
  $same && printf 'launcher unsafe override: allow_same_side=true\n'
  $imbalance && printf 'launcher unsafe override: allow_hedge_imbalance=true\n'
  ((grace > 0)) && printf 'launcher watchdog grace override: %ss\n' "$grace"
} >>"$run/launcher.log"
printf 'RUN_ID=%s\nRUN_DIR=%s\nleg1=%s leg2=%s watchdog=%s\n' "$rid" "$run" "$p1" "$p2" "$wp"
