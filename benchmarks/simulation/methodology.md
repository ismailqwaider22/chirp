# Network Simulation Methodology

## Overview

`bin/simulate.rs` computes analytical throughput estimates for chirp and TCP
across seven representative network conditions. No real packet transmission
occurs; the model is closed-form.

---

## TCP reference model — Mathis formula

```
B_TCP = MSS · √(3/2) / (RTT · √p)
```

**Source:** Mathis M., Semke J., Mahdavi J., Ott T. (1997).
*The Macroscopic Behavior of the TCP Congestion Avoidance Algorithm.*
ACM SIGCOMM Computer Communication Review, 27(3), 67–82.

Models TCP Reno/CUBIC at steady state under random i.i.d. loss probability `p`.
AIMD: +1 MSS/RTT additive, ×0.5 multiplicative decrease on loss.

---

## chirp equilibrium throughput derivation

`DelayBasedController` (src/congestion/delay_based.rs):
- Additive increase: `rate += initial_rate × 0.05` per RTT (FIXED step, not proportional)
- Loss decrease (NACK): `rate *= 0.95`
- Delay decrease (rising OWD): `rate *= 0.70`
- Default initial_rate: 10 Mbps

**Equilibrium:** per-RTT increase = per-RTT loss decrement

Let R = rate, A = initial_rate × 0.05, m = 0.95, λ = (R·RTT/DART_PKT)·p_eff

```
A = R · λ · (1−m)            [first-order expansion for small λ]
initial_rate·0.05 = R · (R·RTT/DART_PKT·p_eff) · 0.05
R_eq = √(initial_rate · DART_PKT / (RTT · p_eff))
     = √(1.515×10⁹ / (RTT_s · p_eff))   [bytes/sec]
```

chirp AI step ≈ 350× larger than TCP → massive equilibrium advantage on lossy links.

---

## FEC effective loss derivation

```
p_eff = p · (1 − (1−p)^(N−1))
```

Numerical (p=2%, N=8):
- 0.98^7 = 0.868126
- p_eff = 0.02 × (1 − 0.868126) = 0.002637  (87% reduction)

At 5%: p_eff = 1.509% (70% reduction). At 8%: p_eff = 3.386% (58% reduction).

---

## Limitations

- Analytical only — no real packet impairment (no tc netem)
- Assumes i.i.d. Bernoulli loss (burst loss not modelled)
- Delay-based ×0.7 backoff not included in formula (approximated by 90% cap)
- Equilibrium assumes long steady-state transfer
