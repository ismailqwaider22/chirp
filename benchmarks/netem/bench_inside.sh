#!/usr/bin/env bash
# Runs inside a Docker --privileged container.
# Applies tc netem scenarios, runs chirp + iperf3 TCP, records real results.
set -euo pipefail

apt-get update -qq >/dev/null 2>&1
apt-get install -qq -y iproute2 iperf3 python3 >/dev/null 2>&1

cd /chirp
echo "[build] cargo build --release..."
cargo build --release -q --bin netem_bench 2>/dev/null

RESULTS="benchmarks/netem/results.txt"
{
echo "chirp vs TCP — tc netem real impairment benchmarks"
echo "Generated : $(date -u '+%Y-%m-%d %H:%M UTC')"
echo "Kernel    : $(uname -r)"
echo "CPU       : $(grep 'model name' /proc/cpuinfo | head -1 | cut -d: -f2 | xargs)"
echo ""
printf "%-22s  %6s  %8s  %12s  %12s  %6s  %12s\n" \
    "Scenario" "Loss%" "Delay" "chirp" "TCP" "Ratio" "Retransmits"
printf '%.0s─' $(seq 1 85); echo ""
} | tee "$RESULTS"

run_scenario() {
    # args: name delay_ms loss_pct bw_mbit port size_mb initial_rate_mbps
    local name="$1" delay_ms="$2" loss_pct="$3" bw_mbit="${4:-0}"
    local port="${5:-39901}" size_mb="${6:-20}" init_mbps="${7:-100}"

    ip link set lo up 2>/dev/null || true
    tc qdisc del dev lo root 2>/dev/null || true
    if [[ "$delay_ms" -gt 0 ]] || [[ "$loss_pct" != "0" ]]; then
        if [[ "$bw_mbit" -gt 0 ]]; then
            tc qdisc add dev lo root netem \
                delay "${delay_ms}ms" loss "${loss_pct}%" rate "${bw_mbit}mbit"
        else
            tc qdisc add dev lo root netem \
                delay "${delay_ms}ms" loss "${loss_pct}%"
        fi
    fi

    # chirp transfer — initial rate tuned per scenario so CC can establish base RTT
    set +e
    DART_LINE=$(./target/release/netem_bench \
        --size-mb "$size_mb" --port "$port" \
        --initial-rate-mbps "$init_mbps" \
        --label "$name" 2>/dev/null)
    DART_EXIT=$?
    set -e
    if [[ $DART_EXIT -ne 0 ]] || [[ -z "$DART_LINE" ]]; then
        DART_MBPS="FAIL"; DART_RTX="—"
    else
        DART_MBPS="$(echo "$DART_LINE" | cut -f3) Mbps"
        DART_RTX=$(echo "$DART_LINE" | cut -f4)
        DART_INTEG=$(echo "$DART_LINE" | cut -f6)
        [[ "$DART_INTEG" != "OK" ]] && DART_MBPS="CORRUPT"
    fi

    # TCP via iperf3 (10 s, same impairment active)
    iperf3 -s -1 -p 15201 >/dev/null 2>&1 &
    IPERF_PID=$!
    sleep 0.3
    set +e
    TCP_MBPS=$(iperf3 -c 127.0.0.1 -p 15201 -t 10 -J 2>/dev/null \
        | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    bps = d['end']['sum_received']['bits_per_second']
    print(f'{bps/1e6:.1f}')
except:
    print('N/A')
" 2>/dev/null)
    set -e
    wait $IPERF_PID 2>/dev/null || true
    [[ -z "$TCP_MBPS" ]] && TCP_MBPS="N/A"
    TCP_MBPS="${TCP_MBPS} Mbps"

    tc qdisc del dev lo root 2>/dev/null || true

    RATIO=$(python3 -c "
a='${DART_MBPS% Mbps}'; b='${TCP_MBPS% Mbps}'
try:
    r = float(a)/float(b)
    print(f'{r:.1f}×')
except:
    print('N/A')
" 2>/dev/null)

    printf "%-22s  %6s  %8s  %12s  %12s  %6s  %12s\n" \
        "$name" "${loss_pct}%" "${delay_ms}ms" \
        "$DART_MBPS" "$TCP_MBPS" "$RATIO" "$DART_RTX" \
        | tee -a "$RESULTS"

    sleep 1
}

#                          name               delay  loss  bw   port   MB  init_Mbps
run_scenario "Clean loopback"              0    0     0    39901  20  500
run_scenario "LAN + jitter"                2    0.1   0    39902  20  200
run_scenario "Enterprise WAN"             80    0.5   0    39903  50   10
run_scenario "Lossy WAN"                 120    2     0    39904  30    5
run_scenario "Satellite"                 600    2    20    39905  20    3
run_scenario "Bad satellite"             600    5    20    39906  10    2
run_scenario "Drone link"                200    8    10    39907  10    2

{
echo ""
echo "chirp: FEC N=8, AES disabled, byte-exact integrity verified"
echo "TCP: iperf3 10-second stream, same netem impairment"
echo "Initial CC rate tuned per scenario (delay-based CC needs base RTT)"
echo "Note: loopback benchmarks — real WAN results will differ"
} | tee -a "$RESULTS"

echo "=== Complete. Results: $RESULTS ==="
