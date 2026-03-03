# Loopback Transfer Methodology

## Test environment

- Transport: UDP loopback, 127.0.0.1 (no network impairment)
- Execution: in-process tokio tasks within a single process (no subprocess)
- Binding: sender → ephemeral port, receiver → fixed port (9000+)
- Rate cap: 1 Gbps (SenderConfig default)

## Test cases

| Test | Size | FEC |
|---|---|---|
| transfer_1mb_fec_on | 1 MB | enabled (N=8) |
| transfer_10mb_fec_on | 10 MB | enabled (N=8) |
| transfer_10mb_fec_off | 10 MB | disabled |
| transfer_100mb_fec_on | 100 MB | enabled (N=8) |

## Timing

Wall clock measured with `std::time::Instant` from first `send_file()` call
to last byte written to output file (inclusive of FIN handshake).

## Integrity verification

Received file is compared byte-for-byte against the original in-memory buffer.
Any mismatch fails the test immediately.

## Interpretation

Loopback throughput (~55 Mbps) does NOT reflect real-world performance:
- No network delay → CC rate ramps from 10 Mbps but never fully converges
- No packet loss → FEC and NACK paths are not exercised
- tokio async scheduling overhead dominates at high packet rates

Loopback tests verify correctness (integrity), not performance.
Real performance is measured analytically in benchmarks/simulation/.
