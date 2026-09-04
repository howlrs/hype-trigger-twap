#!/usr/bin/env bash
# shellcheck disable=SC2016,SC2251,SC2086,SC2015
set -euo pipefail
repo="$(cd -- "$(dirname -- "$0")/.." && pwd -P)"
tmp="$(mktemp -d)"
fixture_process_matches() {
  local pid="$1" expected_start="$2" expected_exe="$3" stat rest actual_start actual_exe
  [[ "$pid" =~ ^[1-9][0-9]*$ && "$expected_start" =~ ^[1-9][0-9]*$ && "$expected_exe" = /* ]] || return 1
  kill -0 "$pid" 2>/dev/null || return 1
  stat="$(<"/proc/$pid/stat")" || return 1
  rest="${stat##*) }"
  # shellcheck disable=SC2086
  set -- $rest
  actual_start="${20-}"
  actual_exe="$(readlink -f -- "/proc/$pid/exe" 2>/dev/null || true)"
  [[ "$actual_start" == "$expected_start" && "$actual_exe" == "$expected_exe" ]]
}
signal_fixture_processes() {
  local signal="$1" manifest name pid start executable
  [[ -d "$tmp" ]] || return 0
  while IFS= read -r -d '' manifest; do
    for name in leg1 leg2 watchdog; do
      pid="$(jq -r ".$name.pid // empty" "$manifest" 2>/dev/null || true)"
      start="$(jq -r ".$name.starttime // empty" "$manifest" 2>/dev/null || true)"
      executable="$(jq -r ".$name.executable // empty" "$manifest" 2>/dev/null || true)"
      if fixture_process_matches "$pid" "$start" "$executable"; then
        kill "-$signal" "$pid" 2>/dev/null || true
      fi
    done
  done < <(find "$tmp" -type f -name manifest.json -print0 2>/dev/null)
}
cleanup_fixtures() {
  trap - EXIT
  set +e
  signal_fixture_processes TERM
  sleep 0.2
  signal_fixture_processes KILL
  pkill -P $$ 2>/dev/null || true
  rm -rf "$tmp"
}
trap cleanup_fixtures EXIT
# Keep fake live-journal defaults inside the fixture.  The launcher and real
# binary both default state under HOME when --state-dir is absent.
export HOME="$tmp/home"
mkdir "$HOME"
fake="$tmp/hype-twap"
log="$tmp/calls"
fake_runs="$tmp/hype-twap-runs"
printf '%s\n' '#!/usr/bin/env bash' 'env | sort >>"$TMPDIR/calls"' 'tr "\0" "\n" <"/proc/$$/cmdline" | sed "s/^/CMDLINE:/" >>"$TMPDIR/calls"' "printf '%s\\n' -- >>\"\$TMPDIR/calls\"" 'printf "%s\n" "$@" >>"$TMPDIR/calls"' 'ready=' 'rid=' 'sd=' 'symbol=' 'side=' 'ro=true' 'for arg in "$@"; do [ "$arg" = FAIL ] && exit 1; done' 'while (($#)); do case "$1" in --pair-ready-file) ready="$2"; shift 2 ;; --pair-run-id) rid="$2"; shift 2 ;; --state-dir) sd="$2"; shift 2 ;; --symbol) symbol="$2"; shift 2 ;; --side) side="$2"; shift 2 ;; --read-only) ro="$2"; shift 2 ;; *) shift ;; esac; done' 'journal_id=' 'if [[ "$ro" == false ]]; then [[ -n "$sd" ]] || sd="${XDG_STATE_HOME:-$HOME/.local/state/hype-twap}"; journal_id="journal-$rid-$symbol-$side"; mkdir -p "$sd/runs/$journal_id"; printf "{\"kind\":\"Header\",\"run_id\":\"%s\",\"symbol\":\"%s\",\"side\":\"%s\",\"started_at_unix_ms\":%s}\n" "$journal_id" "$symbol" "$side" "$(date +%s%3N)" >"$sd/runs/$journal_id/journal.jsonl"; fi' 'if [[ -n "$journal_id" ]]; then journal_json="\"$journal_id\""; else journal_json=null; fi' 'printf "{\"run_id\":\"%s\",\"ready_at_unix_ms\":1,\"pid\":%s,\"journal_run_id\":%s}\n" "$rid" "$$" "$journal_json" >"$ready"' "trap 'printf \"%s\\n\" TERM >>\"\$TMPDIR/calls\"; exit 0' TERM" 'while :; do sleep 0.05; done' >"$fake"
chmod +x "$fake"
# Deterministic stand-in for the read-only replay CLI.  It lets this shell
# contract test exercise `dn-pair recover`'s interpretation of completed,
# incomplete, and corrupt journals without coupling to a prebuilt release
# binary.  The Rust CLI itself has separate black-box coverage.
printf '%s\n' '#!/usr/bin/env bash' 'set -euo pipefail' 'state=' 'command=' 'run_id=' 'while (($#)); do case "$1" in --state-dir) state="$2"; shift 2 ;; inspect|verify) command="$1"; run_id="$2"; shift 2 ;; *) shift ;; esac; done' 'journal="$state/runs/$run_id/journal.jsonl"' '[[ -f "$journal" ]] || exit 1' 'if [[ "$command" == verify ]]; then grep -q CORRUPT "$journal" && exit 1 || exit 0; fi' 'identity="$(jq -r "select(.kind == \"Header\") | [.symbol, .side] | @tsv" "$journal" 2>/dev/null | head -n1 || true)"' 'symbol="${identity%%$'"'\t'"'*}"' 'side="${identity#*$'"'\t'"'}"' 'if grep -q COMPLETED "$journal"; then status=success; elif grep -q ABANDONED "$journal"; then status=abandoned; else status=incomplete; fi' 'printf "{\"schema_version\":1,\"identity\":{\"symbol\":\"%s\",\"side\":\"%s\"},\"status\":\"%s\",\"validation\":{\"valid\":true}}\n" "$symbol" "$side" "$status"' >"$fake_runs"
chmod +x "$fake_runs"
mkdir "$tmp/bin"
printf '%s\n' '#!/bin/bash' 'if [[ "${PAIR_ENV_STAGE:-}" == sanitize ]]; then tr "\0" "\n" </proc/$$/environ >>"$TMPDIR/intermediate-env"; fi' 'exec /usr/bin/env "$@"' >"$tmp/bin/env"
chmod +x "$tmp/bin/env"
TMPDIR="$tmp" HYPE_TWAP_BIN="$fake" HL_AGENT_PK=generic-key-must-not-leak HL_AGENT_ADDRESS=generic-address-must-not-leak "$repo/scripts/dn-pair.sh" --leg1-symbol ETH --leg1-side long --leg1-usd 100 --leg2-symbol BTC --leg2-side short --leg2-usd 100 --duration 1m --slices 1 --log-dir "$tmp/runs" >/dev/null
grep -q '^HL_AGENT_PK=' "$log" && exit 1 || true
grep -q '^HL_AGENT_ADDRESS=' "$log" && exit 1 || true
TMPDIR="$tmp" PATH="$tmp/bin:$PATH" HYPE_TWAP_BIN="$fake" HL_AGENT_PK=generic-live-key-must-not-leak HL_AGENT_ADDRESS=generic-live-address-must-not-leak HL_AGENT_PK_LEG1=one HL_AGENT_PK_LEG2=two "$repo/scripts/dn-pair.sh" --live --leg1-symbol ETH --leg1-side long --leg1-usd 100 --leg1-max-notional-usd 120 --leg2-symbol BTC --leg2-side short --leg2-usd 100 --leg2-max-notional-usd 120 --duration 1m --slices 1 --log-dir "$tmp/runs" >/dev/null
grep -q '^HL_AGENT_PK=one$' "$log"
grep -q '^HL_AGENT_PK=two$' "$log"
! grep -q 'HL_AGENT_PK_LEG' "$log"
! grep -q 'generic-live-.*-must-not-leak' "$log"
! grep -q '^CMDLINE:.*\(one\|two\)' "$log"
[[ -s "$tmp/intermediate-env" ]]
! grep -Eq '^(HL_AGENT_PK|HL_AGENT_ADDRESS|HL_AGENT_PK_LEG1|HL_AGENT_PK_LEG2)=' "$tmp/intermediate-env"
# Regression guard: a secret assignment must never be an argv element of env.
! grep -Fq 'env -i "HL_AGENT_PK=' "$repo/scripts/dn-pair.sh"
# The pair launcher restores only its fixed TLS/proxy/logging allowlist for
# each leg, after clearing the inherited environment.  This must apply to
# both read-only and live launches without restoring generic credentials or
# arbitrary caller variables.
allow_tmp="$tmp/allowlist-tmp"
mkdir "$allow_tmp"
mkdir "$allow_tmp/certs"
touch "$allow_tmp/ca.pem"
pair_hook='https://hooks.example.invalid/pair-private-token'
allow_env=(SSL_CERT_FILE="$allow_tmp/ca.pem" SSL_CERT_DIR="$allow_tmp/certs" HTTPS_PROXY='https://proxy.example.invalid:8443' HTTP_PROXY='http://proxy.example.invalid:8080' ALL_PROXY='socks5://proxy.example.invalid:1080' NO_PROXY='localhost,127.0.0.1' https_proxy='https://proxy-lower.example.invalid:8443' http_proxy='http://proxy-lower.example.invalid:8080' all_proxy='socks5://proxy-lower.example.invalid:1080' no_proxy='localhost' RUST_LOG='hype_twap=debug' RUST_BACKTRACE=1 HL_ALERT_HOOK_URL="$pair_hook" PAIR_FORBIDDEN_SETTING=must-not-leak HL_AGENT_PK=generic-allowlist-key-must-not-leak HL_AGENT_ADDRESS=generic-allowlist-address-must-not-leak)
env TMPDIR="$allow_tmp" PATH="$tmp/bin:$PATH" HYPE_TWAP_BIN="$fake" "${allow_env[@]}" "$repo/scripts/dn-pair.sh" --leg1-symbol ETH --leg1-side long --leg1-usd 100 --leg2-symbol BTC --leg2-side short --leg2-usd 100 --duration 1m --slices 1 --log-dir "$tmp/allow-read-only-runs" >"$allow_tmp/read-only.stdout" 2>"$allow_tmp/read-only.stderr"
env TMPDIR="$allow_tmp" PATH="$tmp/bin:$PATH" HYPE_TWAP_BIN="$fake" HL_AGENT_PK_LEG1=allow-one HL_AGENT_PK_LEG2=allow-two "${allow_env[@]}" "$repo/scripts/dn-pair.sh" --live --leg1-symbol ETH --leg1-side long --leg1-usd 100 --leg1-max-notional-usd 120 --leg2-symbol BTC --leg2-side short --leg2-usd 100 --leg2-max-notional-usd 120 --duration 1m --slices 1 --log-dir "$tmp/allow-live-runs" >"$allow_tmp/live.stdout" 2>"$allow_tmp/live.stderr"
for allowed in "SSL_CERT_FILE=$allow_tmp/ca.pem" "SSL_CERT_DIR=$allow_tmp/certs" 'HTTPS_PROXY=https://proxy.example.invalid:8443' 'HTTP_PROXY=http://proxy.example.invalid:8080' 'ALL_PROXY=socks5://proxy.example.invalid:1080' 'NO_PROXY=localhost,127.0.0.1' 'https_proxy=https://proxy-lower.example.invalid:8443' 'http_proxy=http://proxy-lower.example.invalid:8080' 'all_proxy=socks5://proxy-lower.example.invalid:1080' 'no_proxy=localhost' 'RUST_LOG=hype_twap=debug' 'RUST_BACKTRACE=1'; do
  [[ "$(grep -Fxc "$allowed" "$allow_tmp/calls")" -eq 4 ]]
done
[[ "$(grep -Fxc "HL_ALERT_HOOK_URL=$pair_hook" "$allow_tmp/calls")" -eq 4 ]]
! grep -Eq '^(PAIR_FORBIDDEN_SETTING|HL_AGENT_PK_LEG1|HL_AGENT_PK_LEG2|HL_AGENT_ADDRESS_LEG1|HL_AGENT_ADDRESS_LEG2)=' "$allow_tmp/calls"
! grep -Eq 'generic-allowlist-.*-must-not-leak' "$allow_tmp/calls"
grep -q '^HL_AGENT_PK=allow-one$' "$allow_tmp/calls"
grep -q '^HL_AGENT_PK=allow-two$' "$allow_tmp/calls"
! grep -Fq "$pair_hook" "$allow_tmp/intermediate-env"
! grep -Fq "$pair_hook" "$allow_tmp/read-only.stdout"
! grep -Fq "$pair_hook" "$allow_tmp/read-only.stderr"
! grep -Fq "$pair_hook" "$allow_tmp/live.stdout"
! grep -Fq "$pair_hook" "$allow_tmp/live.stderr"
! grep '^CMDLINE:' "$allow_tmp/calls" | grep -Fq "$pair_hook"
! grep -rFq "$pair_hook" "$tmp/allow-read-only-runs" "$tmp/allow-live-runs"

# The watchdog is launched through its own clean child environment.  Inspect
# that exact child rather than inferring it from the legs' captured envs.
watchdog_capture="$tmp/watchdog-capture"
printf '%s\n' '#!/usr/bin/env bash' 'env | sort >"$TMPDIR/watchdog-child-env"' "exec \"$repo/scripts/dn-watchdog.sh\" \"\$@\"" >"$watchdog_capture"
chmod +x "$watchdog_capture"
env TMPDIR="$tmp" HYPE_TWAP_BIN="$fake" DN_WATCHDOG_BIN="$watchdog_capture" WATCHDOG_FORBIDDEN=must-not-leak HL_AGENT_PK=generic-watchdog-key-must-not-leak HL_AGENT_ADDRESS=generic-watchdog-address-must-not-leak "$repo/scripts/dn-pair.sh" --leg1-symbol ETH --leg1-side long --leg1-usd 100 --leg2-symbol BTC --leg2-side short --leg2-usd 100 --duration 1m --slices 1 --log-dir "$tmp/watchdog-env-runs" >/dev/null
[[ -s "$tmp/watchdog-child-env" ]]
! grep -Eq '^(WATCHDOG_FORBIDDEN|HL_AGENT_PK|HL_AGENT_ADDRESS|HL_AGENT_PK_LEG1|HL_AGENT_PK_LEG2|PAIR_FORBIDDEN_SETTING)=' "$tmp/watchdog-child-env"

# Per-leg controls must retain their leg prefix through parsing, validation,
# forwarding, and the durable manifest; they must not be shared implicitly.
option_runs="$tmp/pair-option-runs"
TMPDIR="$tmp" HYPE_TWAP_BIN="$fake" "$repo/scripts/dn-pair.sh" --leg1-symbol ETH --leg1-side long --leg1-usd 100 --leg1-network testnet --leg1-trigger-price 2000 --leg1-trigger-when above --leg1-slippage-bps 0 --leg1-follow-poll-secs 3 --leg1-follow-repost-secs 11 --leg1-follow-threshold-bps 0.5 --leg1-wait-network-grace 5m --leg1-child-algo follow --leg2-symbol BTC --leg2-side short --leg2-usd 100 --leg2-network mainnet --leg2-trigger-price 60000 --leg2-trigger-when below --leg2-slippage-bps 1000 --leg2-follow-poll-secs 4 --leg2-follow-repost-secs 12 --leg2-follow-threshold-bps 1.5 --leg2-wait-network-grace 6m --leg2-child-algo follow --duration 1m --slices 1 --log-dir "$option_runs" >/dev/null
option_manifest="$(find "$option_runs" -type f -name manifest.json)"
jq -e '
  .effective.leg1_args[0:14] == ["--symbol","ETH","--side","long","--usd","100","--duration","1m","--slices","1","--child-algo","follow","--read-only","true"] and
  .effective.leg2_args[0:14] == ["--symbol","BTC","--side","short","--usd","100","--duration","1m","--slices","1","--child-algo","follow","--read-only","true"] and
  .effective.leg1_args[-16:] == ["--network","testnet","--trigger-price","2000","--trigger-when","above","--slippage-bps","0","--follow-poll-secs","3","--follow-repost-secs","11","--follow-threshold-bps","0.5","--wait-network-grace","5m"] and
  .effective.leg2_args[-16:] == ["--network","mainnet","--trigger-price","60000","--trigger-when","below","--slippage-bps","1000","--follow-poll-secs","4","--follow-repost-secs","12","--follow-threshold-bps","1.5","--wait-network-grace","6m"]
' "$option_manifest" >/dev/null
for invalid in '--leg1-follow-poll-secs 0' '--leg2-follow-repost-secs 0' '--leg1-follow-threshold-bps -0.1' '--leg2-wait-network-grace 0s' '--leg1-wait-network-grace' '--leg1-trigger-price 0' '--leg2-slippage-bps 1000.1'; do
  # Deliberately expand these compact flag/value fixtures into argv.
  read -r -a invalid_args <<<"$invalid"
  if TMPDIR="$tmp" HYPE_TWAP_BIN="$fake" "$repo/scripts/dn-pair.sh" --leg1-symbol ETH --leg1-side long --leg1-usd 100 --leg2-symbol BTC --leg2-side short --leg2-usd 100 --duration 1m --slices 1 --log-dir "$tmp/invalid-pair-options" "${invalid_args[@]}" >/dev/null 2>&1; then exit 1; else [[ $? == 2 ]]; fi
done

# The exact tolerance boundary succeeds; a beyond-boundary imbalance needs
# the explicit unsafe override, which is retained in the successful log.
TMPDIR="$tmp" HYPE_TWAP_BIN="$fake" "$repo/scripts/dn-pair.sh" --leg1-symbol ETH --leg1-side long --leg1-usd 100 --leg2-symbol BTC --leg2-side short --leg2-usd 101 --hedge-ratio 1 --hedge-tolerance-bps 100 --duration 1m --slices 1 --log-dir "$tmp/hedge-boundary" >/dev/null
if TMPDIR="$tmp" HYPE_TWAP_BIN="$fake" "$repo/scripts/dn-pair.sh" --leg1-symbol ETH --leg1-side long --leg1-usd 100 --leg2-symbol BTC --leg2-side short --leg2-usd 102 --hedge-ratio 1 --hedge-tolerance-bps 100 --duration 1m --slices 1 --log-dir "$tmp/hedge-outside" >/dev/null 2>&1; then exit 1; fi
TMPDIR="$tmp" HYPE_TWAP_BIN="$fake" "$repo/scripts/dn-pair.sh" --leg1-symbol ETH --leg1-side long --leg1-usd 100 --leg2-symbol BTC --leg2-side long --leg2-usd 102 --hedge-ratio 1 --hedge-tolerance-bps 100 --allow-same-side --allow-hedge-imbalance --watchdog-grace 7 --duration 1m --slices 1 --log-dir "$tmp/hedge-override" >/dev/null
override_log="$(find "$tmp/hedge-override" -type f -name launcher.log)"
grep -q 'launcher unsafe override: allow_same_side=true' "$override_log"
grep -q 'launcher unsafe override: allow_hedge_imbalance=true' "$override_log"
grep -q 'launcher watchdog grace override: 7s' "$override_log"
! HYPE_TWAP_BIN="$fake" "$repo/scripts/dn-pair.sh" --leg1-symbol ETH --leg1-side long --leg1-usd 100 --leg2-symbol BTC --leg2-side long --leg2-usd 100 --duration 1m --slices 1 --log-dir "$tmp/runs" 2>/dev/null
if "$repo/scripts/dn-watchdog.sh" --pid1 0 --pid1-starttime 1 --pid1-exe /x --pid2 2 --pid2-starttime 1 --pid2-exe /x >/dev/null 2>&1; then exit 1; else [[ $? == 2 ]]; fi
if "$repo/scripts/dn-watchdog.sh" --pid1 -1 --pid1-starttime 1 --pid1-exe /x --pid2 2 --pid2-starttime 1 --pid2-exe /x >/dev/null 2>&1; then exit 1; else [[ $? == 2 ]]; fi
if "$repo/scripts/dn-watchdog.sh" --pid1 7 --pid1-starttime 1 --pid1-exe /x --pid2 7 --pid2-starttime 1 --pid2-exe /x >"$tmp/duplicate-watchdog.out" 2>&1; then exit 1; else [[ $? == 2 ]]; fi
grep -q 'pid1 and pid2 must differ' "$tmp/duplicate-watchdog.out"
for bad_grace in -1 nope 3601; do
  if "$repo/scripts/dn-watchdog.sh" --pid1 1 --pid1-starttime 1 --pid1-exe /x --pid2 2 --pid2-starttime 1 --pid2-exe /x --grace "$bad_grace" >/dev/null 2>&1; then exit 1; else [[ $? == 2 ]]; fi
done
if "$repo/scripts/dn-watchdog.sh" --pid1 >/dev/null 2>&1; then exit 1; else [[ $? == 2 ]]; fi

# The legacy alias is live-equivalent but loud, and cannot override an
# explicit dry-run request. Keep this compatibility contract covered.
alias_tmp="$tmp/alias-tmp"
mkdir "$alias_tmp"
TMPDIR="$alias_tmp" HYPE_TWAP_BIN="$fake" HL_AGENT_PK_LEG1=alias-one HL_AGENT_PK_LEG2=alias-two "$repo/scripts/dn-pair.sh" --read-only false --leg1-symbol ETH --leg1-side long --leg1-usd 100 --leg1-max-notional-usd 120 --leg2-symbol BTC --leg2-side short --leg2-usd 100 --leg2-max-notional-usd 120 --duration 1m --slices 1 --log-dir "$tmp/alias-runs" >"$tmp/alias.out" 2>"$tmp/alias.err"
grep -q -- '--read-only false is deprecated; use --live' "$tmp/alias.err"
grep -qx -- '--read-only' "$alias_tmp/calls"
grep -qx -- 'false' "$alias_tmp/calls"
! HYPE_TWAP_BIN="$fake" "$repo/scripts/dn-pair.sh" --live --read-only true --leg1-symbol ETH --leg1-side long --leg1-usd 100 --leg2-symbol BTC --leg2-side short --leg2-usd 100 --duration 1m --slices 1 --log-dir "$tmp/runs" >"$tmp/live-readonly-conflict.out" 2>&1
grep -q -- '--live conflicts with --read-only true' "$tmp/live-readonly-conflict.out"
! HYPE_TWAP_BIN="$fake" "$repo/scripts/dn-pair.sh" --read-only false --read-only true --leg1-symbol ETH --leg1-side long --leg1-usd 100 --leg2-symbol BTC --leg2-side short --leg2-usd 100 --duration 1m --slices 1 --log-dir "$tmp/runs" >"$tmp/alias-readonly-conflict.out" 2>&1
grep -q -- '--live conflicts with --read-only true' "$tmp/alias-readonly-conflict.out"

TMPDIR="$tmp" HYPE_TWAP_BIN="$fake" "$repo/scripts/dn-pair.sh" --leg1-symbol ETH --leg1-side long --leg1-usd 100 --leg1-state-dir "$tmp/state-one" --leg2-symbol BTC --leg2-side short --leg2-usd 100 --duration 1m --slices 1 --log-dir "$tmp/runs" >/dev/null
[[ "$(find "$tmp/runs" -type f -name manifest.json | wc -l)" -ge 3 ]]
run_dir="$(
  while IFS= read -r manifest; do
    if jq -e --arg state "$tmp/state-one" '.leg1.state_dir == $state' "$manifest" >/dev/null 2>&1; then
      dirname "$manifest"
    fi
  done < <(find "$tmp/runs" -type f -name manifest.json)
  true
)"
[[ -n "$run_dir" && "$(printf '%s\n' "$run_dir" | wc -l)" -eq 1 ]]
run_id="$(jq -r .run_id "$run_dir/manifest.json")"
[[ -f "$run_dir/leg1.ready.json" && -f "$run_dir/leg2.ready.json" && -f "$run_dir/start.json" ]]
jq -e --arg rid "$(jq -r .run_id "$run_dir/manifest.json")" '.run_id == $rid and (.start_at_unix_ms|type == "number")' "$run_dir/start.json" >/dev/null
jq -e '.effective.leg1_args | type == "array" and index("--state-dir") != null' "$run_dir/manifest.json" >/dev/null
jq -e '.effective.leg2_args | type == "array" and index("--symbol") != null' "$run_dir/manifest.json" >/dev/null
jq -e '.leg1.journal_run_id == "" and .leg2.journal_run_id == ""' "$run_dir/manifest.json" >/dev/null
# Concurrent live pairs may share the same state roots and symbol/side. Each
# must retain the exact journal id published by its own ready handshake.
shared_pairs="$tmp/shared-pairs"
shared_leg1="$tmp/shared-state-leg1"
shared_leg2="$tmp/shared-state-leg2"
for n in 1 2; do
  TMPDIR="$tmp" HYPE_TWAP_BIN="$fake" HL_AGENT_PK_LEG1="shared-one-$n" HL_AGENT_PK_LEG2="shared-two-$n" "$repo/scripts/dn-pair.sh" --live --leg1-symbol ETH --leg1-side long --leg1-usd 100 --leg1-max-notional-usd 120 --leg1-state-dir "$shared_leg1" --leg2-symbol BTC --leg2-side short --leg2-usd 100 --leg2-max-notional-usd 120 --leg2-state-dir "$shared_leg2" --duration 1m --slices 1 --log-dir "$shared_pairs" >"$tmp/shared-pair-$n.out" 2>&1 &
done
wait
mapfile -t shared_manifests < <(find "$shared_pairs" -type f -name manifest.json | sort)
[[ "${#shared_manifests[@]}" == 2 ]]
for shared_manifest in "${shared_manifests[@]}"; do
  shared_id="$(jq -r .run_id "$shared_manifest")"
  jq -e --arg journal1 "journal-$shared_id-ETH-long" --arg journal2 "journal-$shared_id-BTC-short" '.leg1.journal_run_id == $journal1 and .leg2.journal_run_id == $journal2 and .leg1.journal_run_id != .leg2.journal_run_id' "$shared_manifest" >/dev/null
done
"$repo/scripts/dn-pair.sh" status --run-id "$run_id" --log-dir "$tmp/runs" --json | jq -e '.manifest_version == 1 and (.identity.leg1|type == "string")' >/dev/null
# The start command releases its lock at exit, while a concurrent lifecycle
# command fails fast rather than racing a manifest read/write.
flock "$run_dir/lifecycle.lock" sleep 2 & lifecycle_locker=$!
sleep 0.1
! "$repo/scripts/dn-pair.sh" status --run-id "$run_id" --log-dir "$tmp/runs" >"$tmp/lock-contention.out" 2>&1
grep -q 'lifecycle operation already in progress' "$tmp/lock-contention.out"
wait "$lifecycle_locker"
live_before="$(sha256sum "$run_dir/manifest.json")"
"$repo/scripts/dn-pair.sh" recover --run-id "$run_id" --log-dir "$tmp/runs" | grep -q '^leg1:'
[[ "$live_before" == "$(sha256sum "$run_dir/manifest.json")" ]]

# #41: recovery classifications come only from validated replay output.  A
# completed journal is terminal; incomplete/unresolved and corrupt journals
# must remain visibly non-terminal even if their raw text contains a terminal
# marker-like word.
fixture_base="$tmp/recovery-fixtures"
mkdir "$fixture_base"
chmod 700 "$fixture_base"
for fixture in completed incomplete corrupt; do
  fixture_id="$run_id-$fixture"
  fixture_state="$tmp/recovery-state-$fixture"
  mkdir -p "$fixture_state/runs/journal-$fixture_id" "$fixture_base/$fixture_id"
  case "$fixture" in
    completed) marker=COMPLETED; expected='VALIDATED-SUCCESS' ;;
    incomplete) marker='FinalReport completed:false unresolved'; expected='VALIDATED-INCOMPLETE' ;;
    corrupt) marker=CORRUPT; expected='INVALID-OR-CORRUPT' ;;
  esac
  printf '{"kind":"Header","symbol":"ETH","side":"long"}\n%s\n' "$marker" >"$fixture_state/runs/journal-$fixture_id/journal.jsonl"
  jq --arg rid "$fixture_id" --arg state "$fixture_state" --arg journal "journal-$fixture_id" '.run_id=$rid | .leg1.state_dir=$state | .leg1.journal_run_id=$journal | .leg2.state_dir=$state | .leg2.journal_run_id=""' "$run_dir/manifest.json" >"$fixture_base/$fixture_id/manifest.json"
  jq -n '{}' >"$fixture_base/$fixture_id/lifecycle.lock"
  HYPE_TWAP_RUNS_BIN="$fake_runs" "$repo/scripts/dn-pair.sh" recover --run-id "$fixture_id" --log-dir "$fixture_base" | grep -q "journals: leg1=present:journal-$fixture_id+$expected"
done

# recover is diagnostic only: a stale manifest is neither rewritten nor used
# to signal a process, even when it contains an unrelated live PID.
stale_id="$run_id-stale"
mkdir "$tmp/$stale_id"
jq -n '{}' >"$tmp/$stale_id/lifecycle.lock"
jq --arg rid "$stale_id" '.run_id=$rid | .leg1.starttime="1" | .leg2.starttime="1" | .watchdog.starttime="1"' "$run_dir/manifest.json" >"$tmp/$stale_id/manifest.json"
before="$(sha256sum "$tmp/$stale_id/manifest.json")"
"$repo/scripts/dn-pair.sh" recover --run-id "$stale_id" --log-dir "$tmp" | grep -q 'no process, signal, order, or file was changed'
[[ "$before" == "$(sha256sum "$tmp/$stale_id/manifest.json")" ]]

# Path traversal and a symlink log root are refused before the manifest is read.
! "$repo/scripts/dn-pair.sh" status --run-id ../"$run_id" --log-dir "$tmp/runs" >/dev/null 2>&1
! "$repo/scripts/dn-pair.sh" status --run-id .bad --log-dir "$tmp/runs" >"$tmp/bad-run-id.out" 2>&1
grep -q 'requires a safe --run-id' "$tmp/bad-run-id.out"
run_id_128="a$(head -c 127 /dev/zero | tr '\0' a)"
run_id_129="${run_id_128}a"
! "$repo/scripts/dn-pair.sh" status --run-id "$run_id_128" --log-dir "$tmp/runs" >"$tmp/run-id-128.out" 2>&1
! grep -q 'requires a safe --run-id' "$tmp/run-id-128.out"
! "$repo/scripts/dn-pair.sh" status --run-id "$run_id_129" --log-dir "$tmp/runs" >"$tmp/run-id-129.out" 2>&1
grep -q 'requires a safe --run-id' "$tmp/run-id-129.out"
for bad_timeout in -1 3601 1.5; do
  ! "$repo/scripts/dn-pair.sh" stop --run-id "$run_id" --log-dir "$tmp/runs" --timeout "$bad_timeout" >"$tmp/bad-timeout-$bad_timeout.out" 2>&1
  grep -q 'invalid --timeout' "$tmp/bad-timeout-$bad_timeout.out"
done
! "$repo/scripts/dn-pair.sh" recover --run-id "$run_id" --log-dir "$tmp/runs" --unexpected >"$tmp/unknown-operation-flag.out" 2>&1
grep -q 'unknown recover argument' "$tmp/unknown-operation-flag.out"
! HYPE_TWAP_BIN="$fake" "$repo/scripts/dn-pair.sh" --leg1-child-algo not-an-algo --leg1-symbol ETH --leg1-side long --leg1-usd 100 --leg2-symbol BTC --leg2-side short --leg2-usd 100 --duration 1m --slices 1 --log-dir "$tmp/runs" >"$tmp/bad-start-flag.out" 2>&1
grep -q 'invalid execution setting' "$tmp/bad-start-flag.out"
ln -s "$tmp/runs" "$tmp/runs-link"
! TMPDIR="$tmp" HYPE_TWAP_BIN="$fake" "$repo/scripts/dn-pair.sh" --leg1-symbol ETH --leg1-side long --leg1-usd 100 --leg2-symbol BTC --leg2-side short --leg2-usd 100 --duration 1m --slices 1 --log-dir "$tmp/runs-link" >/dev/null 2>&1

# stop must not signal a stale/PID-reused identity.
sleep 20 & stale_survivor=$!
stale_stat="$(<"/proc/$stale_survivor/stat")"; stale_rest="${stale_stat##*) }"; set -- $stale_rest; stale_start="${20}"
tmp_stale_manifest="$tmp/$stale_id/manifest.json"
jq --arg rid "$stale_id" --arg pid "$stale_survivor" --arg start "$((stale_start + 1))" --arg exe "$(readlink -f /proc/$stale_survivor/exe)" '.run_id=$rid | .leg1.pid=$pid | .leg1.starttime=$start | .leg1.executable=$exe | .leg2.pid="" | .leg2.starttime="" | .leg2.executable="" | .watchdog.pid="" | .watchdog.starttime="" | .watchdog.executable=""' "$run_dir/manifest.json" >"$tmp_stale_manifest"
! "$repo/scripts/dn-pair.sh" stop --run-id "$stale_id" --log-dir "$tmp" --timeout 0 >"$tmp/stop-partial.out" 2>&1
grep -q 'stop_partial.*residual risk' "$tmp/stop-partial.out"
jq -e '.state == "stop_partial"' "$tmp_stale_manifest" >/dev/null
kill -0 "$stale_survivor"
kill -TERM "$stale_survivor"
wait "$stale_survivor" 2>/dev/null || true

# A live PID with the wrong executable is PID reuse/identity mismatch: every
# lifecycle view must refuse to signal it and leave it running.
sleep 20 & exe_mismatch_survivor=$!
exe_stat="$(<"/proc/$exe_mismatch_survivor/stat")"; exe_rest="${exe_stat##*) }"; set -- $exe_rest; exe_start="${20}"
exe_mismatch_id="$run_id-exe-mismatch"
mkdir "$tmp/$exe_mismatch_id"
jq -n '{}' >"$tmp/$exe_mismatch_id/lifecycle.lock"
jq --arg rid "$exe_mismatch_id" --arg pid "$exe_mismatch_survivor" --arg start "$exe_start" '.run_id=$rid | .leg1.pid=$pid | .leg1.starttime=$start | .leg1.executable="/bin/false" | .leg2.pid="" | .leg2.starttime="" | .leg2.executable="" | .watchdog.pid="" | .watchdog.starttime="" | .watchdog.executable=""' "$run_dir/manifest.json" >"$tmp/$exe_mismatch_id/manifest.json"
"$repo/scripts/dn-pair.sh" status --run-id "$exe_mismatch_id" --log-dir "$tmp" | grep -q "^leg1:mismatch:$exe_mismatch_survivor$"
"$repo/scripts/dn-pair.sh" recover --run-id "$exe_mismatch_id" --log-dir "$tmp" | grep -q "^leg1:mismatch:$exe_mismatch_survivor$"
! "$repo/scripts/dn-pair.sh" stop --run-id "$exe_mismatch_id" --log-dir "$tmp" --timeout 0 >"$tmp/exe-mismatch-stop.out" 2>&1
grep -q 'stop_partial.*residual risk' "$tmp/exe-mismatch-stop.out"
kill -0 "$exe_mismatch_survivor"
kill -TERM "$exe_mismatch_survivor"
wait "$exe_mismatch_survivor" 2>/dev/null || true

# Malformed identity records fail closed for every lifecycle operation.
for malformed in 'pid=not-a-pid' 'starttime=0' 'executable=relative'; do
  malformed_id="$run_id-malformed-${malformed%%=*}"
  mkdir "$tmp/$malformed_id"
  jq -n '{}' >"$tmp/$malformed_id/lifecycle.lock"
  field="${malformed%%=*}"; value="${malformed#*=}"
  jq --arg rid "$malformed_id" --arg field "$field" --arg value "$value" '.run_id=$rid | .leg1.pid="123" | .leg1.starttime="1" | .leg1.executable="/bin/sleep" | .leg1[$field]=$value | .leg2.pid="" | .leg2.starttime="" | .leg2.executable="" | .watchdog.pid="" | .watchdog.starttime="" | .watchdog.executable=""' "$run_dir/manifest.json" >"$tmp/$malformed_id/manifest.json"
  "$repo/scripts/dn-pair.sh" status --run-id "$malformed_id" --log-dir "$tmp" | grep -q '^leg1:mismatch:'
  "$repo/scripts/dn-pair.sh" recover --run-id "$malformed_id" --log-dir "$tmp" | grep -q '^leg1:mismatch:'
  ! "$repo/scripts/dn-pair.sh" stop --run-id "$malformed_id" --log-dir "$tmp" --timeout 0 >"$tmp/malformed-stop-${field}.out" 2>&1
  grep -q 'stop_partial.*residual risk' "$tmp/malformed-stop-${field}.out"
done

# A verified process is signalled, then the manifest becomes stopped only
# after all recorded identities are absent.
sleep 20 & verified_survivor=$!
verified_stat="$(<"/proc/$verified_survivor/stat")"; verified_rest="${verified_stat##*) }"; set -- $verified_rest; verified_start="${20}"
verified_id="$run_id-verified"
mkdir "$tmp/$verified_id"
jq -n '{}' >"$tmp/$verified_id/lifecycle.lock"
jq --arg rid "$verified_id" --arg pid "$verified_survivor" --arg start "$verified_start" --arg exe "$(readlink -f /proc/$verified_survivor/exe)" '.run_id=$rid | .leg1.pid=$pid | .leg1.starttime=$start | .leg1.executable=$exe | .leg2.pid="" | .leg2.starttime="" | .leg2.executable="" | .watchdog.pid="" | .watchdog.starttime="" | .watchdog.executable=""' "$run_dir/manifest.json" >"$tmp/$verified_id/manifest.json"
"$repo/scripts/dn-pair.sh" stop --run-id "$verified_id" --log-dir "$tmp" --timeout 1 >/dev/null
! kill -0 "$verified_survivor" 2>/dev/null || { sleep 0.2; ! kill -0 "$verified_survivor" 2>/dev/null; }
wait "$verified_survivor" 2>/dev/null || true
jq -e '.state == "stopped"' "$tmp/$verified_id/manifest.json" >/dev/null

# stop is deliberately TERM-only.  A process that needs time to settle after
# TERM remains alive and leaves a non-clean manifest; lifecycle tooling must
# never silently SIGKILL it.
bash -c 'trap "sleep 20; exit 0" TERM; sleep 30' & stubborn_survivor=$!
stubborn_stat="$(<"/proc/$stubborn_survivor/stat")"; stubborn_rest="${stubborn_stat##*) }"; set -- $stubborn_rest; stubborn_start="${20}"
sleep 20 & stubborn_watchdog=$!
stubborn_wstat="$(<"/proc/$stubborn_watchdog/stat")"; stubborn_wrest="${stubborn_wstat##*) }"; set -- $stubborn_wrest; stubborn_wstart="${20}"
stubborn_id="$run_id-stubborn"
mkdir "$tmp/$stubborn_id"
jq -n '{}' >"$tmp/$stubborn_id/lifecycle.lock"
jq --arg rid "$stubborn_id" --arg pid "$stubborn_survivor" --arg start "$stubborn_start" --arg exe "$(readlink -f /proc/$stubborn_survivor/exe)" --arg wpid "$stubborn_watchdog" --arg wstart "$stubborn_wstart" --arg wexe "$(readlink -f /proc/$stubborn_watchdog/exe)" '.run_id=$rid | .leg1.pid=$pid | .leg1.starttime=$start | .leg1.executable=$exe | .leg2.pid="" | .leg2.starttime="" | .leg2.executable="" | .watchdog.pid=$wpid | .watchdog.starttime=$wstart | .watchdog.executable=$wexe' "$run_dir/manifest.json" >"$tmp/$stubborn_id/manifest.json"
! "$repo/scripts/dn-pair.sh" stop --run-id "$stubborn_id" --log-dir "$tmp" --timeout 0 >"$tmp/stubborn-stop.out" 2>&1
grep -q 'stop_partial.*residual risk' "$tmp/stubborn-stop.out"
kill -0 "$stubborn_survivor"
kill -0 "$stubborn_watchdog"
jq -e '.state == "stop_partial"' "$tmp/$stubborn_id/manifest.json" >/dev/null
kill -KILL "$stubborn_survivor" "$stubborn_watchdog"
wait "$stubborn_survivor" "$stubborn_watchdog" 2>/dev/null || true

# A live one-leg state is reported by recover without any mutation or signal.
sleep 20 & partial_survivor=$!
partial_stat="$(<"/proc/$partial_survivor/stat")"; partial_rest="${partial_stat##*) }"; set -- $partial_rest; partial_start="${20}"
partial_id="$run_id-partial"
mkdir "$tmp/$partial_id"
jq -n '{}' >"$tmp/$partial_id/lifecycle.lock"
jq --arg rid "$partial_id" --arg pid "$partial_survivor" --arg start "$partial_start" --arg exe "$(readlink -f /proc/$partial_survivor/exe)" '.run_id=$rid | .leg1.pid="" | .leg1.starttime="" | .leg1.executable="" | .leg2.pid=$pid | .leg2.starttime=$start | .leg2.executable=$exe | .watchdog.pid="" | .watchdog.starttime="" | .watchdog.executable=""' "$run_dir/manifest.json" >"$tmp/$partial_id/manifest.json"
partial_before="$(sha256sum "$tmp/$partial_id/manifest.json")"
"$repo/scripts/dn-pair.sh" recover --run-id "$partial_id" --log-dir "$tmp" | grep -q 'leg2:verified:'
[[ "$partial_before" == "$(sha256sum "$tmp/$partial_id/manifest.json")" ]]
kill -0 "$partial_survivor"
kill -TERM "$partial_survivor"
wait "$partial_survivor" 2>/dev/null || true

# A dead watchdog is visible to both status and read-only recover.
watchdog_pid="$(jq -r .watchdog.pid "$run_dir/manifest.json")"
kill -TERM "$watchdog_pid"
sleep 0.2
"$repo/scripts/dn-pair.sh" status --run-id "$run_id" --log-dir "$tmp/runs" | grep -q '^watchdog:absent:'
"$repo/scripts/dn-pair.sh" recover --run-id "$run_id" --log-dir "$tmp/runs" | grep -q '^watchdog:absent:'

# A watchdog that cannot become healthy prevents the common start release and
# rolls both preflight-ready legs back before any execution can begin.
bad_watchdog="$tmp/bad-watchdog"
printf '%s\n' '#!/bin/bash' 'exit 1' >"$bad_watchdog"
chmod +x "$bad_watchdog"
terms_before="$(grep -c '^TERM$' "$log" || true)"
! TMPDIR="$tmp" HYPE_TWAP_BIN="$fake" DN_WATCHDOG_BIN="$bad_watchdog" "$repo/scripts/dn-pair.sh" --leg1-symbol ETH --leg1-side long --leg1-usd 100 --leg2-symbol BTC --leg2-side short --leg2-usd 100 --duration 1m --slices 1 --log-dir "$tmp/watchdog-fail" >/dev/null 2>&1
[[ -z "$(find "$tmp/watchdog-fail" -type f -name start.json -print -quit)" ]]
sleep 0.2
(( $(grep -c '^TERM$' "$log" || true) >= terms_before + 2 ))

TMPDIR="$tmp" HYPE_TWAP_BIN="$fake" "$repo/scripts/dn-pair.sh" --leg1-symbol ETH --leg1-side long --leg1-usd 100 --leg2-symbol FAIL --leg2-side short --leg2-usd 100 --duration 1m --slices 1 --log-dir "$tmp/runs" >/dev/null 2>&1 && exit 1
sleep 1
grep -q '^TERM$' "$log"

# Once start.json is public, a one-leg failure must not kill the watchdog
# first.  The survivor deliberately takes longer than the launcher's bounded
# graceful-cleanup wait after TERM; it must stay alive, leave a non-clean
# state, and remain under watchdog supervision rather than being SIGKILLed.
slow_cleanup_fake="$tmp/slow-cleanup-hype-twap"
printf '%s\n' '#!/usr/bin/env bash' 'set -euo pipefail' 'ready= start= rid= symbol=' 'while (($#)); do case "$1" in --pair-ready-file) ready="$2"; shift 2 ;; --pair-start-file) start="$2"; shift 2 ;; --pair-run-id) rid="$2"; shift 2 ;; --symbol) symbol="$2"; shift 2 ;; *) shift ;; esac; done' 'printf "{\"run_id\":\"%s\",\"ready_at_unix_ms\":1,\"pid\":%s}\n" "$rid" "$$" >"$ready"' 'while [[ ! -f "$start" ]]; do sleep 0.02; done' '[[ "$symbol" == BTC ]] && exit 1' "trap 'printf \"%s\\n\" LONG_TERM >>\"\$TMPDIR/slow-cleanup.calls\"; sleep 20; exit 0' TERM" 'while :; do sleep 0.05; done' >"$slow_cleanup_fake"
chmod +x "$slow_cleanup_fake"
post_release_root="$tmp/post-release-fail"
! TMPDIR="$tmp" HYPE_TWAP_BIN="$slow_cleanup_fake" "$repo/scripts/dn-pair.sh" --leg1-symbol ETH --leg1-side long --leg1-usd 100 --leg2-symbol BTC --leg2-side short --leg2-usd 100 --duration 1m --slices 1 --log-dir "$post_release_root" >"$tmp/post-release.out" 2>&1
post_manifest="$(find "$post_release_root" -type f -name manifest.json)"
[[ -n "$post_manifest" ]]
jq -e '.state == "start_failed_partial"' "$post_manifest" >/dev/null
post_leg_pid="$(jq -r .leg1.pid "$post_manifest")"
post_watchdog_pid="$(jq -r .watchdog.pid "$post_manifest")"
kill -0 "$post_leg_pid"
kill -0 "$post_watchdog_pid"
grep -q '^LONG_TERM$' "$tmp/slow-cleanup.calls"
grep -q 'start_failed_partial.*watchdog retained' "$tmp/post-release.out"
kill -KILL "$post_leg_pid" "$post_watchdog_pid" 2>/dev/null || true
wait "$post_leg_pid" "$post_watchdog_pid" 2>/dev/null || true

# TERM before the barrier release is a rollback, not an execution shutdown:
# both preflight children and their watchdog are stopped and start.json never
# becomes visible.
pre_release_fake="$tmp/pre-release-hype-twap"
printf '%s\n' '#!/usr/bin/env bash' 'set -euo pipefail' "trap 'printf \"%s\\n\" PRE_RELEASE_TERM >>\"\$TMPDIR/pre-release.calls\"; exit 0' TERM" 'while :; do sleep 0.05; done' >"$pre_release_fake"
chmod +x "$pre_release_fake"
pre_release_root="$tmp/pre-release-term"
if TMPDIR="$tmp" HYPE_TWAP_BIN="$pre_release_fake" timeout --preserve-status --signal=TERM --kill-after=5s 1s "$repo/scripts/dn-pair.sh" --leg1-symbol ETH --leg1-side long --leg1-usd 100 --leg2-symbol BTC --leg2-side short --leg2-usd 100 --duration 1m --slices 1 --log-dir "$pre_release_root" >"$tmp/pre-release.out" 2>&1; then
  exit 1
else
  pre_release_status=$?
fi
[[ "$pre_release_status" == 143 ]]
pre_release_manifest="$(find "$pre_release_root" -type f -name manifest.json -print -quit)"
[[ -n "$pre_release_manifest" ]]
jq -e '
  (.leg1.pid | test("^[1-9][0-9]*$")) and
  (.leg2.pid | test("^[1-9][0-9]*$")) and
  (.watchdog.pid | test("^[1-9][0-9]*$"))
' "$pre_release_manifest" >/dev/null
pre_release_dir="$(dirname "$pre_release_manifest")"
pre_p1="$(jq -r .leg1.pid "$pre_release_manifest")"
pre_p2="$(jq -r .leg2.pid "$pre_release_manifest")"
pre_wp="$(jq -r .watchdog.pid "$pre_release_manifest")"
sleep 0.2
[[ ! -f "$pre_release_dir/start.json" ]]
jq -e '.state == "failed"' "$pre_release_manifest" >/dev/null
[[ "$(grep -c '^PRE_RELEASE_TERM$' "$tmp/pre-release.calls")" -eq 2 ]]
for pre_pid in "$pre_p1" "$pre_p2" "$pre_wp"; do ! kill -0 "$pre_pid" 2>/dev/null; done

# INT follows the same pre-release rollback path and retains its conventional
# exit status.  Run it in the foreground under timeout so SIGINT is not
# inherited as ignored from an asynchronous shell job.
pre_int_root="$tmp/pre-release-int"
if TMPDIR="$tmp" HYPE_TWAP_BIN="$pre_release_fake" timeout --preserve-status --signal=INT --kill-after=5s 1s "$repo/scripts/dn-pair.sh" --leg1-symbol ETH --leg1-side long --leg1-usd 100 --leg2-symbol BTC --leg2-side short --leg2-usd 100 --duration 1m --slices 1 --log-dir "$pre_int_root" >"$tmp/pre-release-int.out" 2>&1; then
  exit 1
else
  pre_int_status=$?
fi
[[ "$pre_int_status" == 130 ]]
pre_int_manifest="$(find "$pre_int_root" -type f -name manifest.json -print -quit)"
[[ -n "$pre_int_manifest" ]]
pre_int_dir="$(dirname "$pre_int_manifest")"
pre_int_p1="$(jq -r .leg1.pid "$pre_int_manifest")"
pre_int_p2="$(jq -r .leg2.pid "$pre_int_manifest")"
pre_int_wp="$(jq -r .watchdog.pid "$pre_int_manifest")"
[[ ! -f "$pre_int_dir/start.json" ]]
jq -e '.state == "failed"' "$pre_int_manifest" >/dev/null
[[ "$(grep -c '^PRE_RELEASE_TERM$' "$tmp/pre-release.calls")" -eq 4 ]]
for pre_int_pid in "$pre_int_p1" "$pre_int_p2" "$pre_int_wp"; do ! kill -0 "$pre_int_pid" 2>/dev/null; done

# A signal delivered by the exact `mv ... start.json` publication command is
# post-release: the conservative latch is set before that command begins.
# Keep leg1 in its TERM cleanup longer than the launcher grace so the test can
# prove the watchdog is retained instead of being killed by the pre-release
# rollback path.
publish_boundary_fake="$tmp/publish-boundary-hype-twap"
printf '%s\n' '#!/usr/bin/env bash' 'set -euo pipefail' 'ready= start= rid= symbol=' 'while (($#)); do case "$1" in --pair-ready-file) ready="$2"; shift 2 ;; --pair-start-file) start="$2"; shift 2 ;; --pair-run-id) rid="$2"; shift 2 ;; --symbol) symbol="$2"; shift 2 ;; *) shift ;; esac; done' "trap 'if [[ \"\$symbol\" == ETH ]]; then printf \"%s\\n\" BOUNDARY_LONG_TERM >>\"\$TMPDIR/publish-boundary.calls\"; sleep 20; else printf \"%s\\n\" BOUNDARY_SHORT_TERM >>\"\$TMPDIR/publish-boundary.calls\"; exit 0; fi' TERM" 'printf "{\"run_id\":\"%s\",\"ready_at_unix_ms\":1,\"pid\":%s}\n" "$rid" "$$" >"$ready"' 'while [[ ! -f "$start" ]]; do sleep 0.02; done' 'while :; do sleep 0.05; done' >"$publish_boundary_fake"
chmod +x "$publish_boundary_fake"
publish_boundary_bin="$tmp/publish-boundary-bin"
mkdir "$publish_boundary_bin"
publish_boundary_mv="$publish_boundary_bin/mv"
printf '%s\n' '#!/usr/bin/env bash' 'set -euo pipefail' 'destination="${!#}"' 'if [[ "$destination" == */start.json ]]; then' '  printf "%s\\n" START_MV_BOUNDARY >>"$TMPDIR/publish-boundary.calls"' '  kill -TERM "$PPID"' '  sleep 0.05' 'fi' 'exec /bin/mv "$@"' >"$publish_boundary_mv"
chmod +x "$publish_boundary_mv"
publish_boundary_root="$tmp/publish-boundary-term"
if PATH="$publish_boundary_bin:$PATH" TMPDIR="$tmp" HYPE_TWAP_BIN="$publish_boundary_fake" "$repo/scripts/dn-pair.sh" --leg1-symbol ETH --leg1-side long --leg1-usd 100 --leg2-symbol BTC --leg2-side short --leg2-usd 100 --duration 1m --slices 1 --log-dir "$publish_boundary_root" >"$tmp/publish-boundary.out" 2>&1; then
  exit 1
else
  publish_boundary_status=$?
fi
if [[ "$publish_boundary_status" != 143 ]]; then
  cat "$tmp/publish-boundary.out" >&2
  [[ ! -f "$tmp/publish-boundary.calls" ]] || cat "$tmp/publish-boundary.calls" >&2
  exit 1
fi
publish_boundary_manifest="$(find "$publish_boundary_root" -type f -name manifest.json -print -quit)"
[[ -n "$publish_boundary_manifest" ]]
publish_boundary_dir="$(dirname "$publish_boundary_manifest")"
publish_boundary_p1="$(jq -r .leg1.pid "$publish_boundary_manifest")"
publish_boundary_wp="$(jq -r .watchdog.pid "$publish_boundary_manifest")"
[[ -f "$publish_boundary_dir/start.json" ]]
jq -e '.state == "start_failed_partial"' "$publish_boundary_manifest" >/dev/null
kill -0 "$publish_boundary_p1"
kill -0 "$publish_boundary_wp"
grep -qx 'START_MV_BOUNDARY' "$tmp/publish-boundary.calls"
grep -qx 'BOUNDARY_LONG_TERM' "$tmp/publish-boundary.calls"
grep -qx 'BOUNDARY_SHORT_TERM' "$tmp/publish-boundary.calls"
grep -q 'start_failed_partial.*watchdog retained' "$tmp/publish-boundary.out"
kill -KILL "$publish_boundary_p1" "$publish_boundary_wp" 2>/dev/null || true
wait "$publish_boundary_p1" "$publish_boundary_wp" 2>/dev/null || true

sleep 20 & survivor=$!
stat="$(<"/proc/$survivor/stat")"; rest="${stat##*) }"; set -- $rest; start="${20}"
sleep 0.1 & dead=$!; wait "$dead" || true
! "$repo/scripts/dn-watchdog.sh" --pid1 "$survivor" --pid1-starttime "$((start + 1))" --pid1-exe "$(readlink -f /proc/$survivor/exe)" --pid2 "$dead" --pid2-starttime 1 --pid2-exe /bin/sleep --grace 0 >/dev/null 2>&1
kill -0 "$survivor"
kill -TERM "$survivor"
wait "$survivor" 2>/dev/null || true

# A PID whose starttime/exe no longer matches is a reused PID, never a signal
# target.  The watchdog treats its original leg as absent and independently
# contains the other verified original leg.
sleep 30 & verified_watch_survivor=$!
verified_watch_stat="$(<"/proc/$verified_watch_survivor/stat")"; verified_watch_rest="${verified_watch_stat##*) }"; set -- $verified_watch_rest; verified_watch_start="${20}"
sleep 30 & reused_pid=$!
reused_stat="$(<"/proc/$reused_pid/stat")"; reused_rest="${reused_stat##*) }"; set -- $reused_rest; reused_start="${20}"
fake_proc="$tmp/fake-proc"
mkdir -p "$fake_proc/$verified_watch_survivor" "$fake_proc/$reused_pid"
fake_stat() {
  local index
  {
    printf '1 (fake) S'
    for ((index = 0; index < 18; index++)); do printf ' 0'; done
    printf ' %s\n' "$2"
  } >"$fake_proc/$1/stat"
}
fake_stat "$verified_watch_survivor" "$verified_watch_start"
fake_stat "$reused_pid" "$((reused_start + 1))"
ln -s "/proc/$verified_watch_survivor/exe" "$fake_proc/$verified_watch_survivor/exe"
ln -s "/proc/$reused_pid/exe" "$fake_proc/$reused_pid/exe"
! DN_PROC_ROOT="$fake_proc" timeout 5 "$repo/scripts/dn-watchdog.sh" --pid1 "$verified_watch_survivor" --pid1-starttime "$verified_watch_start" --pid1-exe "$(readlink -f /proc/$verified_watch_survivor/exe)" --pid2 "$reused_pid" --pid2-starttime "$reused_start" --pid2-exe "$(readlink -f /proc/$reused_pid/exe)" --grace 0 >"$tmp/reused-pid-watchdog.out" 2>&1
! kill -0 "$verified_watch_survivor" 2>/dev/null || { sleep 0.2; ! kill -0 "$verified_watch_survivor" 2>/dev/null; }
kill -0 "$reused_pid"
grep -q 'reused or unknown PID' "$tmp/reused-pid-watchdog.out"
kill -KILL "$reused_pid"
wait "$reused_pid" 2>/dev/null || true
wait "$verified_watch_survivor" 2>/dev/null || true

# A verified one-leg failure writes only a closed, secret-free sidecar event.
sleep 20 & watched=$!
watched_stat="$(<"/proc/$watched/stat")"; watched_rest="${watched_stat##*) }"; set -- $watched_rest; watched_start="${20}"
sleep 0.1 & gone=$!; gone_stat="$(<"/proc/$gone/stat")"; gone_rest="${gone_stat##*) }"; set -- $gone_rest; gone_start="${20}"; wait "$gone" || true
event_file="$tmp/watchdog-events.jsonl"
if timeout 5 "$repo/scripts/dn-watchdog.sh" --pid1 "$watched" --pid1-starttime "$watched_start" --pid1-exe "$(readlink -f /proc/$watched/exe)" --pid2 "$gone" --pid2-starttime "$gone_start" --pid2-exe /bin/sleep --grace 0 --event-log "$event_file" >/dev/null 2>&1; then
  exit 1
else
  [[ $? == 1 ]]
fi
grep -q '"schema_version":1' "$event_file"
grep -q '"kind":"pair_leg_abnormal"' "$event_file"
! grep -Eqi 'private|secret|token|signature' "$event_file"
wait "$watched" 2>/dev/null || true

# With no containment needed, two already-exited original legs remain a
# normal completion and the watchdog reports success.
sleep 0.1 & normal_one=$!
normal_one_stat="$(<"/proc/$normal_one/stat")"; normal_one_rest="${normal_one_stat##*) }"; set -- $normal_one_rest; normal_one_start="${20}"
sleep 0.1 & normal_two=$!
normal_two_stat="$(<"/proc/$normal_two/stat")"; normal_two_rest="${normal_two_stat##*) }"; set -- $normal_two_rest; normal_two_start="${20}"
wait "$normal_one" "$normal_two" || true
timeout 5 "$repo/scripts/dn-watchdog.sh" --pid1 "$normal_one" --pid1-starttime "$normal_one_start" --pid1-exe /bin/sleep --pid2 "$normal_two" --pid2-starttime "$normal_two_start" --pid2-exe /bin/sleep --grace 0 >/dev/null 2>&1

# Pair watchdog alerts are a single bounded, asynchronous curl delivery. A
# curl failure cannot prevent the verified survivor from receiving SIGTERM.
mkdir "$tmp/fakebin"
fake_curl="$tmp/fakebin/curl"
printf '%s\n' '#!/usr/bin/env bash' 'printf "%s\n" "$@" >"$TMPDIR/curl-call"' 'exit 1' >"$fake_curl"
chmod +x "$fake_curl"
sleep 20 & hook_survivor=$!
hook_stat="$(<"/proc/$hook_survivor/stat")"; hook_rest="${hook_stat##*) }"; set -- $hook_rest; hook_start="${20}"
sleep 0.1 & hook_gone=$!; hook_gone_stat="$(<"/proc/$hook_gone/stat")"; hook_gone_rest="${hook_gone_stat##*) }"; set -- $hook_gone_rest; hook_gone_start="${20}"; wait "$hook_gone" || true
if TMPDIR="$tmp" PATH="$tmp/fakebin:$PATH" HL_ALERT_HOOK_URL='https://hooks.example.invalid/pair' timeout 5 "$repo/scripts/dn-watchdog.sh" --pid1 "$hook_survivor" --pid1-starttime "$hook_start" --pid1-exe "$(readlink -f /proc/$hook_survivor/exe)" --pid2 "$hook_gone" --pid2-starttime "$hook_gone_start" --pid2-exe /bin/sleep --grace 0 >/dev/null 2>&1; then
  exit 1
else
  [[ $? == 1 ]]
fi
grep -q -- '--max-time' "$tmp/curl-call"
grep -q -- '"kind":"pair_leg_abnormal"' "$tmp/curl-call"
! kill -0 "$hook_survivor" 2>/dev/null || { sleep 0.2; ! kill -0 "$hook_survivor" 2>/dev/null; }
wait "$hook_survivor" 2>/dev/null || true

# Unsafe URL forms, including remote cleartext HTTP, disable delivery but
# retain watchdog safety behavior.
rm -f "$tmp/curl-call"
sleep 20 & invalid_survivor=$!
invalid_stat="$(<"/proc/$invalid_survivor/stat")"; invalid_rest="${invalid_stat##*) }"; set -- $invalid_rest; invalid_start="${20}"
sleep 0.1 & invalid_gone=$!; invalid_gone_stat="$(<"/proc/$invalid_gone/stat")"; invalid_gone_rest="${invalid_gone_stat##*) }"; set -- $invalid_gone_rest; invalid_gone_start="${20}"; wait "$invalid_gone" || true
if TMPDIR="$tmp" PATH="$tmp/fakebin:$PATH" HL_ALERT_HOOK_URL='https://token@hooks.example.invalid/pair' timeout 5 "$repo/scripts/dn-watchdog.sh" --pid1 "$invalid_survivor" --pid1-starttime "$invalid_start" --pid1-exe "$(readlink -f /proc/$invalid_survivor/exe)" --pid2 "$invalid_gone" --pid2-starttime "$invalid_gone_start" --pid2-exe /bin/sleep --grace 0 >/dev/null 2>&1; then
  exit 1
else
  [[ $? == 1 ]]
fi
[[ ! -e "$tmp/curl-call" ]]
! kill -0 "$invalid_survivor" 2>/dev/null || { sleep 0.2; ! kill -0 "$invalid_survivor" 2>/dev/null; }
wait "$invalid_survivor" 2>/dev/null || true

sleep 20 & cleartext_survivor=$!
cleartext_stat="$(<"/proc/$cleartext_survivor/stat")"; cleartext_rest="${cleartext_stat##*) }"; set -- $cleartext_rest; cleartext_start="${20}"
sleep 0.1 & cleartext_gone=$!; cleartext_gone_stat="$(<"/proc/$cleartext_gone/stat")"; cleartext_gone_rest="${cleartext_gone_stat##*) }"; set -- $cleartext_gone_rest; cleartext_gone_start="${20}"; wait "$cleartext_gone" || true
if TMPDIR="$tmp" PATH="$tmp/fakebin:$PATH" HL_ALERT_HOOK_URL='http://hooks.example.invalid/pair' timeout 5 "$repo/scripts/dn-watchdog.sh" --pid1 "$cleartext_survivor" --pid1-starttime "$cleartext_start" --pid1-exe "$(readlink -f /proc/$cleartext_survivor/exe)" --pid2 "$cleartext_gone" --pid2-starttime "$cleartext_gone_start" --pid2-exe /bin/sleep --grace 0 >/dev/null 2>&1; then
  exit 1
else
  [[ $? == 1 ]]
fi
[[ ! -e "$tmp/curl-call" ]]
! kill -0 "$cleartext_survivor" 2>/dev/null || { sleep 0.2; ! kill -0 "$cleartext_survivor" 2>/dev/null; }
wait "$cleartext_survivor" 2>/dev/null || true

# Literal loopback HTTP is intentionally retained for local test receivers.
rm -f "$tmp/curl-call"
sleep 20 & loopback_survivor=$!
loopback_stat="$(<"/proc/$loopback_survivor/stat")"; loopback_rest="${loopback_stat##*) }"; set -- $loopback_rest; loopback_start="${20}"
sleep 0.1 & loopback_gone=$!; loopback_gone_stat="$(<"/proc/$loopback_gone/stat")"; loopback_gone_rest="${loopback_gone_stat##*) }"; set -- $loopback_gone_rest; loopback_gone_start="${20}"; wait "$loopback_gone" || true
if TMPDIR="$tmp" PATH="$tmp/fakebin:$PATH" HL_ALERT_HOOK_URL='http://127.0.0.1/pair' timeout 5 "$repo/scripts/dn-watchdog.sh" --pid1 "$loopback_survivor" --pid1-starttime "$loopback_start" --pid1-exe "$(readlink -f /proc/$loopback_survivor/exe)" --pid2 "$loopback_gone" --pid2-starttime "$loopback_gone_start" --pid2-exe /bin/sleep --grace 0 >/dev/null 2>&1; then
  exit 1
else
  [[ $? == 1 ]]
fi
grep -q -- '"kind":"pair_leg_abnormal"' "$tmp/curl-call"
! kill -0 "$loopback_survivor" 2>/dev/null || { sleep 0.2; ! kill -0 "$loopback_survivor" 2>/dev/null; }
wait "$loopback_survivor" 2>/dev/null || true
