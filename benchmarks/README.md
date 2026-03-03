# chirp Benchmark Suite

## Hardware & Environment

```
CPU:     Intel Core i7-10700KF @ 3.80GHz (8 cores / 16 threads)
RAM:     128 GB
OS:      Linux 6.8.0-101-generic (x86-64)
Rust:    rustc 1.93.1 (01f6ddf75 2026-02-11)
Build:   cargo build --release (opt-level=3, LTO=thin)
```

---

## Benchmark types

### 1. Network simulation (`simulation/`)

Analytical model — no real packet transmission. Computes steady-state throughput
for chirp and TCP under seven network conditions.

- TCP: Mathis formula `B = MSS·√(3/2) / (RTT·√p)` — see `simulation/methodology.md`
- chirp: equilibrium derivation `R_eq = √(initial_rate·DART_PKT / (RTT·p_eff))` with FEC loss reduction
- **Not** a live benchmark — use `tc netem` for real impairment validation

→ Full methodology: [simulation/methodology.md](simulation/methodology.md)  
→ Results: [simulation/results.txt](simulation/results.txt)

### 2. Loopback transfer (`loopback/`)

Real UDP transfers over 127.0.0.1 with no impairment. Verifies correctness
(byte-exact integrity check). Throughput (~55 Mbps) reflects tokio scheduling
overhead, not network performance.

→ Full methodology: [loopback/methodology.md](loopback/methodology.md)  
→ Results: [loopback/results.txt](loopback/results.txt)

### 3. Protocol micro-benchmarks (`microbench/`)

CPU-bound component timing: packet encode/decode, FEC XOR operations, NACK
HashSet lookups. 1M iterations per benchmark, `std::time::Instant`.

→ Full methodology: [microbench/methodology.md](microbench/methodology.md)

---

## Results summary

### Simulation: chirp vs TCP (100 MB transfer)

| Scenario | chirp | TCP | Ratio |
|---|---:|---:|---:|
| Clean LAN (0% loss) | 836 Mbps | 940 Mbps | 0.89× |
| LAN + jitter (0.1%, 2ms) | 752 Mbps | 226 Mbps | **3.3×** |
| Enterprise WAN (0.5%, 80ms) | 75 Mbps | 2.5 Mbps | **29.5×** |
| Lossy WAN (2%, 120ms) | 16 Mbps | 0.8 Mbps | **18.5×** |
| Satellite (2%, 600ms RTT) | 7 Mbps | 0.2 Mbps | **41.3×** |
| Bad satellite (5%, 600ms) | 2.9 Mbps | 0.1 Mbps | **27.3×** |
| Drone link (8%, 200ms) | 3.3 Mbps | 0.3 Mbps | **13.0×** |

### Loopback transfers (correctness)

| Test | Size | FEC | Throughput | Integrity |
|---|---|---|---:|---|
| transfer_1mb_fec_on | 1 MB | ✓ | 55 Mbps | ✓ |
| transfer_10mb_fec_on | 10 MB | ✓ | 56 Mbps | ✓ |
| transfer_10mb_fec_off | 10 MB | ✗ | 55 Mbps | ✓ |

### Protocol micro-benchmarks

| Component | ns/op | Throughput |
|---|---:|---:|
| packet/encode 1200B | 108 ns | 88.9 Gbps |
| packet/decode 1200B | 50 ns | 192 Gbps |
| NACK lookup O(1) | 6 ns | — |
| FEC/encode 8×1200B | 340 ns | 226 Gbps |
| FEC/recover 1 loss | 3 662 ns | 18.4 Gbps |

---

## Reproducing all results

```bash
cd /home/theta-gamma/chirp
bash benchmarks/run_all.sh
```

Or individually:

```bash
# Simulation
source ~/.cargo/env && cargo build --release --bin simulate -q
./target/release/simulate --size-mb 100

# Loopback transfers
source ~/.cargo/env
RUST_LOG=warn cargo test --test loopback --release -- \
  --nocapture --test-threads=1 \
  transfer_1mb_fec_on transfer_10mb_fec_on transfer_10mb_fec_off

# Protocol micro-benchmarks (Criterion)
cargo bench --bench protocol
```

---

## Honest limitations

1. **No real network impairment** — simulation is analytical. Use `tc netem`
   for real validation (requires `CAP_NET_ADMIN`).
2. **Loopback ≠ WAN** — 55 Mbps loopback is tokio-limited, not network-limited.
3. **i.i.d. loss assumed** — burst loss (Gilbert-Elliott model) would reduce the
   FEC advantage.
4. **Single-run microbenchmarks** — use `cargo bench` for confidence intervals.
