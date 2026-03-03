#!/usr/bin/env bash
# Run chirp vs TCP benchmarks under real tc netem impairment.
# Requires Docker (uses --privileged for tc netem inside container).
# Usage: bash benchmarks/netem/run_netem.sh
set -euo pipefail
REPO="$(cd "$(dirname "$0")/../.." && pwd)"

echo "=== chirp tc netem benchmark ==="
echo "Repo: $REPO"
echo "This will take ~5 minutes."
echo ""

docker run --rm --privileged \
  --volume "$REPO":/chirp \
  --volume "$HOME/.cargo/registry":/root/.cargo/registry \
  --workdir /chirp \
  rust:1.93 \
  bash /chirp/benchmarks/netem/bench_inside.sh
