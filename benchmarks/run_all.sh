#!/usr/bin/env bash
# Run all chirp benchmarks and save results.
set -euo pipefail
cd "$(dirname "$0")/.."
source ~/.cargo/env

echo "=== Building release ==="
cargo build --release -q

echo ""
echo "=== Network simulation ==="
./target/release/simulate --size-mb 100 | tee benchmarks/simulation/results.txt

echo ""
echo "=== Loopback transfers ==="
RUST_LOG=warn cargo test --test loopback --release -- \
  --nocapture --test-threads=1 \
  transfer_1mb_fec_on transfer_10mb_fec_on transfer_10mb_fec_off 2>&1 \
  | tee benchmarks/loopback/results.txt

echo ""
echo "=== Protocol micro-benchmarks (Criterion) ==="
echo "Run: cargo bench --bench protocol"
echo "     (skipped here — takes several minutes)"

echo ""
echo "All benchmarks complete."
