# Changelog

## [0.1.0] — 2026-03-03

First public release.

### Protocol
- UDP bulk file transfer with NACK-based selective retransmission
- XOR FEC: parity block every 8 data packets
- Delay-based congestion control (OWD gradient, loss-agnostic)
- AES-256-GCM optional encryption

### Reliability
- Full-scan FIN NACK: post-FIN receiver scans full received map — no NackTracker window blindspot
- `sent_packets` retained until FIN-ACK: late post-FIN NACKs always have retransmit data
- FEC tail padding clamped at receiver: no garbage bytes written past EOF
- Age-only retransmit trimming: no count cap, age reset on each retransmit
- SYN `ConnectionRefused` treated as retryable: receiver not yet ready ≠ fatal error
- `on_loss()` is a no-op: RF loss ≠ congestion; rate stall under high loss eliminated

### `no_std`
- Protocol core (`packet`, `nack`, `fec`, `congestion`) compiles with `no_std + alloc`
- Caller-supplied `fugit::Instant` timing — no `std::time::Instant` in protocol core

### Benchmarks (loopback, `tc netem`, 20 MB)
- Clean: 172.9 Mbps
- Enterprise WAN (0.5%, 160 ms RTT): 30.9 Mbps
- Satellite (2%, 600 ms RTT): 12.9 Mbps
- Drone link (8%, 200 ms RTT): 15.0 Mbps
- Real LAN (200 MB, Tailscale): 14.2 Mbps, MD5 verified ✓
