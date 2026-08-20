#!/usr/bin/env bash
# dn-pair.sh — Delta-neutral two-leg launcher for hype-twap.
#
# Launches two independent `hype-twap` processes (one per leg) as detached
# background jobs, each signing with its OWN agent (API) wallet. This is
# required because nonce state and the single-writer advisory lock in
# hype-twap are keyed by (network, agent address) — running both legs
# through the same agent wallet causes the second process to be refused at
# startup (flock contention), leaving the first leg running alone.
#
# bash / GNU Linux only (uses setsid, /proc, GNU date semantics assumed
# elsewhere in the toolchain). Not tested on macOS/BSD.
#
# Usage:
#   HL_AGENT_PK_LEG1=0x... HL_AGENT_PK_LEG2=0x... \
#   scripts/dn-pair.sh \
#     --leg1-symbol ETH --leg1-side long  --leg1-usd 1000 \
#     --leg2-symbol BTC --leg2-side short --leg2-usd 1000 \
#     --duration 30m --slices 10
#
# Stopping: kill -TERM <pid1> <pid2>   (hype-twap performs a graceful
# cancel/settle of resting orders on SIGTERM; do NOT use `pkill -x` — it can
# hit unrelated hype-twap processes on the same host.)

set -euo pipefail

# ---------------------------------------------------------------------------
# Early, unconditional absolute-path resolution. Every path used later is
# computed HERE, before any subshell/background job is spawned, so nothing
# downstream depends on a shell variable that could go out of scope or be
# re-derived incorrectly inside a detached process. (Lesson from a live
# incident: a script that re-resolved paths late left one leg running
# naked for ~25s after the other leg's launch failed.)
# ---------------------------------------------------------------------------
SCRIPT_SOURCE="${BASH_SOURCE[0]}"
SCRIPT_DIR="$(cd -- "$(dirname -- "${SCRIPT_SOURCE}")" >/dev/null 2>&1 && pwd -P)"
readonly SCRIPT_DIR
readonly REPO_ROOT="${SCRIPT_DIR}/.."
readonly DEFAULT_BIN="${SCRIPT_DIR}/../target/release/hype-twap"
readonly WATCHDOG_SCRIPT="${SCRIPT_DIR}/dn-watchdog.sh"

usage() {
  cat <<'EOF'
Usage: dn-pair.sh --leg1-symbol S --leg1-side long|short --leg1-usd N \
                   --leg2-symbol S --leg2-side long|short --leg2-usd N \
                   --duration DUR --slices N \
                   [--child-algo market|passive|follow] \
                   [--max-notional-usd N] \
                   [--log-dir DIR] \
                   [--read-only true|false]

Required env vars (live mode only, i.e. --read-only false):
  HL_AGENT_PK_LEG1   Agent (API wallet) private key for leg 1.
  HL_AGENT_PK_LEG2   Agent (API wallet) private key for leg 2.
                      Must NOT be identical (same agent wallet cannot run
                      two concurrent processes — nonce/flock contention).

Optional env vars (passed through per leg if set):
  HL_AGENT_ADDRESS_LEG1, HL_AGENT_ADDRESS_LEG2

Env var override for the binary path: HYPE_TWAP_BIN
EOF
}

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------
leg1_symbol=""
leg1_side=""
leg1_usd=""
leg2_symbol=""
leg2_side=""
leg2_usd=""
duration=""
slices=""
child_algo="follow"
max_notional_usd=""
log_dir="${HOME}/.local/state/hype-twap/logs"
read_only="false"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --leg1-symbol) leg1_symbol="$2"; shift 2 ;;
    --leg1-side) leg1_side="$2"; shift 2 ;;
    --leg1-usd) leg1_usd="$2"; shift 2 ;;
    --leg2-symbol) leg2_symbol="$2"; shift 2 ;;
    --leg2-side) leg2_side="$2"; shift 2 ;;
    --leg2-usd) leg2_usd="$2"; shift 2 ;;
    --duration) duration="$2"; shift 2 ;;
    --slices) slices="$2"; shift 2 ;;
    --child-algo) child_algo="$2"; shift 2 ;;
    --max-notional-usd) max_notional_usd="$2"; shift 2 ;;
    --log-dir) log_dir="$2"; shift 2 ;;
    --read-only) read_only="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

for required in leg1_symbol leg1_side leg1_usd leg2_symbol leg2_side leg2_usd duration slices; do
  if [[ -z "${!required}" ]]; then
    echo "missing required argument: --${required//_/-}" >&2
    exit 2
  fi
done

if [[ "${read_only}" != "true" && "${read_only}" != "false" ]]; then
  echo "--read-only must be 'true' or 'false' (got: ${read_only})" >&2
  exit 2
fi

for usd_var in leg1_usd leg2_usd; do
  if ! [[ "${!usd_var}" =~ ^[0-9]+(\.[0-9]+)?$ ]]; then
    echo "--${usd_var//_/-} must be a positive number (got: ${!usd_var})" >&2
    exit 2
  fi
done

if [[ "${leg1_side}" == "${leg2_side}" ]]; then
  echo "WARNING: both legs have side '${leg1_side}' — this is NOT delta-neutral." >&2
fi

# ---------------------------------------------------------------------------
# Resolve binary (absolute path resolved up front, see comment above)
# ---------------------------------------------------------------------------
if [[ -n "${HYPE_TWAP_BIN:-}" ]]; then
  bin_path="${HYPE_TWAP_BIN}"
else
  bin_path="${DEFAULT_BIN}"
fi
if [[ ! -x "${bin_path}" ]]; then
  echo "hype-twap binary not found or not executable: ${bin_path}" >&2
  echo "set HYPE_TWAP_BIN or build with: cargo build --release" >&2
  exit 1
fi
bin_path="$(cd -- "$(dirname -- "${bin_path}")" >/dev/null 2>&1 && pwd -P)/$(basename -- "${bin_path}")"
readonly bin_path

if [[ ! -x "${WATCHDOG_SCRIPT}" ]]; then
  echo "watchdog script not found or not executable: ${WATCHDOG_SCRIPT}" >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# Key handling (live mode only). Never echo/log key material.
# ---------------------------------------------------------------------------
if [[ "${read_only}" == "false" ]]; then
  if [[ -z "${HL_AGENT_PK_LEG1:-}" || -z "${HL_AGENT_PK_LEG2:-}" ]]; then
    echo "live mode (--read-only false) requires HL_AGENT_PK_LEG1 and HL_AGENT_PK_LEG2 to be set" >&2
    exit 1
  fi
  # Normalized comparison (strip 0x, lowercase) so the same key stored with
  # differing prefix/case is still caught.
  _norm1="${HL_AGENT_PK_LEG1#0x}"; _norm1="${_norm1,,}"
  _norm2="${HL_AGENT_PK_LEG2#0x}"; _norm2="${_norm2,,}"
  if [[ "${_norm1}" == "${_norm2}" ]]; then
    echo "ABORT: HL_AGENT_PK_LEG1 and HL_AGENT_PK_LEG2 are identical." >&2
    echo "Each leg MUST use a separate agent (API) wallet: nonce state and" >&2
    echo "hype-twap's single-writer advisory lock are keyed by (network, agent" >&2
    echo "address), so the second process using the same agent wallet would be" >&2
    echo "refused at startup (flock contention), leaving one leg running naked." >&2
    exit 1
  fi
fi

# ---------------------------------------------------------------------------
# Notional cap default: --usd is checked as TOTAL notional (not per-slice)
# by hype-twap's --max-notional-usd, so the cap must exceed --usd itself.
# Default to 1.2x each leg's --usd unless explicitly overridden.
# ---------------------------------------------------------------------------
leg1_max_notional="${max_notional_usd}"
leg2_max_notional="${max_notional_usd}"
if [[ -z "${max_notional_usd}" ]]; then
  leg1_max_notional="$(awk -v u="${leg1_usd}" 'BEGIN { printf "%.2f", u * 1.2 }')"
  leg2_max_notional="$(awk -v u="${leg2_usd}" 'BEGIN { printf "%.2f", u * 1.2 }')"
fi

mkdir -p -- "${log_dir}"
log_dir="$(cd -- "${log_dir}" >/dev/null 2>&1 && pwd -P)"
readonly log_dir

leg1_log="${log_dir}/dn-${leg1_symbol}-${leg1_side}.log"
leg1_pidfile="${log_dir}/dn-${leg1_symbol}-${leg1_side}.pid"
leg2_log="${log_dir}/dn-${leg2_symbol}-${leg2_side}.log"
leg2_pidfile="${log_dir}/dn-${leg2_symbol}-${leg2_side}.pid"
readonly leg1_log leg1_pidfile leg2_log leg2_pidfile

launch_leg() {
  # $1=symbol $2=side $3=usd $4=max_notional $5=logfile $6=pidfile
  # $7=agent_pk (may be empty in read-only mode) $8=agent_address (may be empty)
  local symbol="$1" side="$2" usd="$3" max_notional="$4" logfile="$5" pidfile="$6"
  local agent_pk="$7" agent_address="$8"

  (
    if [[ -n "${agent_pk}" ]]; then
      export HL_AGENT_PK="${agent_pk}"
    fi
    if [[ -n "${agent_address}" ]]; then
      export HL_AGENT_ADDRESS="${agent_address}"
    fi
    setsid nohup "${bin_path}" \
      --symbol "${symbol}" \
      --side "${side}" \
      --usd "${usd}" \
      --duration "${duration}" \
      --slices "${slices}" \
      --child-algo "${child_algo}" \
      --max-notional-usd "${max_notional}" \
      --read-only "${read_only}" \
      >>"${logfile}" 2>&1 &
    echo $! >"${pidfile}"
  )
  # The subshell backgrounds the setsid/nohup'd process and writes its PID
  # (`$!`) to the pidfile before the outer `(...)` returns.
}

echo "launching leg1: ${leg1_symbol} ${leg1_side} \$${leg1_usd} (max-notional \$${leg1_max_notional})"
launch_leg "${leg1_symbol}" "${leg1_side}" "${leg1_usd}" "${leg1_max_notional}" \
  "${leg1_log}" "${leg1_pidfile}" \
  "${HL_AGENT_PK_LEG1:-}" "${HL_AGENT_ADDRESS_LEG1:-}"
leg1_pid="$(cat -- "${leg1_pidfile}")"

echo "launching leg2: ${leg2_symbol} ${leg2_side} \$${leg2_usd} (max-notional \$${leg2_max_notional})"
launch_leg "${leg2_symbol}" "${leg2_side}" "${leg2_usd}" "${leg2_max_notional}" \
  "${leg2_log}" "${leg2_pidfile}" \
  "${HL_AGENT_PK_LEG2:-}" "${HL_AGENT_ADDRESS_LEG2:-}"
leg2_pid="$(cat -- "${leg2_pidfile}")"

echo "leg1 pid=${leg1_pid} log=${leg1_log}"
echo "leg2 pid=${leg2_pid} log=${leg2_log}"

sleep 10

leg1_alive=true
leg2_alive=true
kill -0 "${leg1_pid}" 2>/dev/null || leg1_alive=false
kill -0 "${leg2_pid}" 2>/dev/null || leg2_alive=false

if [[ "${leg1_alive}" == "false" || "${leg2_alive}" == "false" ]]; then
  echo "ERROR: one leg died within 10s of launch (leg1_alive=${leg1_alive} leg2_alive=${leg2_alive})" >&2
  echo "leg1 log tail:" >&2
  tail -n 20 -- "${leg1_log}" >&2 2>/dev/null || true
  echo "leg2 log tail:" >&2
  tail -n 20 -- "${leg2_log}" >&2 2>/dev/null || true
  if [[ "${leg1_alive}" == "true" ]]; then
    echo "sending SIGTERM to surviving leg1 pid=${leg1_pid} to avoid a naked position" >&2
    kill -TERM "${leg1_pid}" 2>/dev/null || true
  fi
  if [[ "${leg2_alive}" == "true" ]]; then
    echo "sending SIGTERM to surviving leg2 pid=${leg2_pid} to avoid a naked position" >&2
    kill -TERM "${leg2_pid}" 2>/dev/null || true
  fi
  exit 1
fi

echo "both legs alive after 10s; starting watchdog"

watchdog_log="${log_dir}/dn-watchdog.log"
watchdog_pidfile="${log_dir}/dn-watchdog.pid"

(
  setsid nohup "${WATCHDOG_SCRIPT}" "${leg1_pid}" "${leg2_pid}" \
    --log "${watchdog_log}" \
    --comm "$(basename -- "${bin_path}")" \
    >"${watchdog_log}.launch" 2>&1 &
  echo $! >"${watchdog_pidfile}"
)
watchdog_pid="$(cat -- "${watchdog_pidfile}")"

echo "watchdog pid=${watchdog_pid} log=${watchdog_log}"
echo ""
echo "停止方法: kill -TERM ${leg1_pid} ${leg2_pid} (graceful settle が走ります)"
