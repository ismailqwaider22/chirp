#!/usr/bin/env bash
set -euo pipefail

trap 'tc qdisc del dev lo root 2>/dev/null; exit' EXIT INT TERM

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="${SCRIPT_DIR}/../target/release/netem_bench"

if [[ ! -x "$BIN" ]]; then
  echo "netem_bench binary not found — run: cargo build --release" >&2
  exit 1
fi
SIZE_MB="${SIZE_MB:-20}"

# label|loss_pct|oneway_delay_ms|init_rate_mbps|fin_timeout_s|port
SCENARIOS=(
  "clean|0|0|500|60|39901"
  "lan_jitter|0.1|2|200|60|39902"
  "enterprise_wan|0.5|80|50|120|39903"
  "lossy_wan|2|120|20|120|39904"
  "satellite|2|300|10|180|39905"
  "bad_satellite|5|300|5|180|39906"
  "drone_link|8|100|5|180|39907"
)

RESULTS_TSV="$(mktemp /tmp/chirp_netem_results.XXXXXX.tsv)"

printf 'Running %d scenarios (SIZE_MB=%s)\n' "${#SCENARIOS[@]}" "$SIZE_MB"

for row in "${SCENARIOS[@]}"; do
  IFS='|' read -r label loss delay rate fin_timeout port <<<"$row"

  tc qdisc del dev lo root 2>/dev/null || true
  if [[ "$delay" == "0" && "$loss" == "0" ]]; then
    :
  elif [[ "$delay" == "0" ]]; then
    tc qdisc add dev lo root netem loss "${loss}%"
  elif [[ "$loss" == "0" ]]; then
    tc qdisc add dev lo root netem delay "${delay}ms"
  else
    tc qdisc add dev lo root netem delay "${delay}ms" loss "${loss}%"
  fi

  echo "==> ${label}: loss=${loss}% one_way_delay=${delay}ms (RTT~$((delay * 2))ms) init_rate=${rate}Mbps fin_timeout=${fin_timeout}s port=${port}"

  out="$($BIN \
    --label "$label" \
    --size-mb "$SIZE_MB" \
    --port "$port" \
    --initial-rate-mbps "$rate" \
    --fin-timeout "$fin_timeout" 2>&1 || true)"

  line="$(printf '%s\n' "$out" | awk -F'\t' 'NF==6{l=$0} END{print l}')"

  if [[ -n "$line" ]]; then
    printf '%s\n' "$line" | tee -a "$RESULTS_TSV"
  else
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$label" "$SIZE_MB" "0.0" "0" "0.000" "FAIL" | tee -a "$RESULTS_TSV"
    printf '%s\n' "$out"
  fi

  tc qdisc del dev lo root 2>/dev/null || true
  sleep 1

done

echo
echo "Summary"
printf '%-16s %-5s %-8s %-13s %-12s %-5s %-8s %-8s %-6s\n' \
  "label" "loss" "owd_ms" "init_mbps" "fin_timeout" "port" "mbps" "retx" "status"
printf '%-16s %-5s %-8s %-13s %-12s %-5s %-8s %-8s %-6s\n' \
  "-----" "----" "------" "---------" "-----------" "----" "----" "----" "------"

for row in "${SCENARIOS[@]}"; do
  IFS='|' read -r label loss delay rate fin_timeout port <<<"$row"
  result="$(awk -F'\t' -v lbl="$label" '$1==lbl{print $0}' "$RESULTS_TSV" | tail -n1)"

  if [[ -n "$result" ]]; then
    IFS=$'\t' read -r _lbl size mbps retx elapsed status <<<"$result"
    printf '%-16s %-5s %-8s %-13s %-12s %-5s %-8s %-8s %-6s\n' \
      "$label" "$loss" "$delay" "$rate" "$fin_timeout" "$port" "$mbps" "$retx" "$status"
  else
    printf '%-16s %-5s %-8s %-13s %-12s %-5s %-8s %-8s %-6s\n' \
      "$label" "$loss" "$delay" "$rate" "$fin_timeout" "$port" "0.0" "0" "FAIL"
  fi
done

echo
echo "Raw TSV results: $RESULTS_TSV"
