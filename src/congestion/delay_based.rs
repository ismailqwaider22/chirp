//! Delay-based congestion controller — `no_std + alloc` compatible.
//!
//! Core insight: on lossy links (satellite, WAN, drone), packet loss ≠ congestion.
//! We monitor one-way delay (OWD) *trends* instead:
//!
//! - Rising OWD   → queues filling  → multiplicative decrease (×0.7)
//! - Falling OWD  → spare capacity  → fast additive increase  (+10%)
//! - Stable OWD   → gentle increase (+5%)
//!
//! This decouples rate control from loss events — exactly what makes UDP-based
//! protocols (Aspera FASP, Byteport DART) outperform TCP on lossy links.
//!
//! Timing uses [`fugit::Instant`] driven by a caller-supplied monotonic tick,
//! removing any dependency on `std::time::Instant`.

use fugit::Instant;

#[cfg(not(feature = "std"))]
use alloc::collections::VecDeque;
#[cfg(feature = "std")]
use std::collections::VecDeque;

/// Millisecond-resolution monotonic instant (1 000 ticks/second).
pub type InstantMs = Instant<u64, 1, 1_000>;

/// One-way delay sample with a caller-provided fugit instant.
#[derive(Clone, Debug)]
struct DelaySample {
    owd_us: u64,
    tick: InstantMs,
}

/// Delay-based AIMD congestion controller.
pub struct DelayBasedController {
    /// Current send rate (bytes/sec).
    rate_bps: f64,
    min_rate_bps: f64,
    max_rate_bps: f64,
    delay_samples: VecDeque<DelaySample>,
    max_samples: usize,
    /// Additive-increase step (bytes/sec per cycle).
    ai_factor: f64,
    /// Multiplicative-decrease factor on congestion (0..1).
    md_factor: f64,
    cwnd: u32,
    /// Minimum OWD observed — proxy for propagation delay.
    min_owd_us: u64,
    /// Normalised gradient above/below this triggers rate change.
    gradient_threshold: f64,
}

impl DelayBasedController {
    /// Create a new controller.
    ///
    /// - `initial_rate_bps`: starting rate in **bytes/sec** (10 Mbps → 1_250_000)
    /// - `max_rate_bps`: ceiling in bytes/sec
    pub fn new(initial_rate_bps: f64, max_rate_bps: f64) -> Self {
        Self {
            rate_bps: initial_rate_bps,
            min_rate_bps: 100_000.0, // 100 KB/s floor
            max_rate_bps,
            delay_samples: VecDeque::with_capacity(64),
            max_samples: 64,
            // Fixed 1 Mbps step — decoupled from initial_rate to prevent runaway
            // increases when starting at high rates (500 Mbps * 5% = 25 MB/s/tick).
            ai_factor: 125_000.0, // 1 Mbps additive increase per tick
            md_factor: 0.7,       // 30% back-off on congestion
            cwnd: 64,
            min_owd_us: u64::MAX,
            gradient_threshold: 0.05, // 5% relative delay rise = congestion
        }
    }

    /// Feed a one-way delay measurement.
    ///
    /// - `owd_us`: measured OWD in microseconds
    /// - `now`: current monotonic instant — `InstantMs::from_ticks(elapsed_ms)`
    ///
    /// Since end-to-end clocks are rarely synchronised, only the *trend*
    /// (gradient) matters, not the absolute OWD value.
    pub fn on_delay_sample(&mut self, owd_us: u64, now: InstantMs) {
        if owd_us < self.min_owd_us {
            self.min_owd_us = owd_us;
        }
        self.delay_samples
            .push_back(DelaySample { owd_us, tick: now });
        if self.delay_samples.len() > self.max_samples {
            self.delay_samples.pop_front();
        }
        self.adjust_rate();
    }

    /// Report packet loss (from a received NACK).
    ///
    /// On lossy links, isolated loss ≠ congestion — and on LAN, many NACKs
    /// are packet reordering, not real congestion.
    /// Delay gradient is the congestion signal, so loss events are ignored.
    pub fn on_loss(&mut self, _lost_count: u32) {
        // Intentionally no-op.
    }

    /// Additive-increase tick — call every ~100 ms when no congestion.
    /// Completely decoupled from OWD measurement and NACK events.
    pub fn tick_increase(&mut self) {
        self.rate_bps = (self.rate_bps + self.ai_factor).min(self.max_rate_bps);
        self.cwnd = (self.cwnd + 1).min(4096);
    }

    /// Current send rate in bytes/sec.
    #[inline]
    pub fn rate_bps(&self) -> f64 {
        self.rate_bps
    }

    /// Current congestion window (max in-flight packets).
    #[inline]
    pub fn cwnd(&self) -> u32 {
        self.cwnd
    }

    /// Inter-packet delay in µs to maintain the current rate for `packet_size` bytes.
    #[inline]
    pub fn inter_packet_delay_us(&self, packet_size: usize) -> u64 {
        if self.rate_bps <= 0.0 {
            return 1_000;
        }
        (packet_size as f64 / self.rate_bps * 1_000_000.0) as u64
    }

    fn adjust_rate(&mut self) {
        if self.delay_samples.len() < 4 {
            return;
        }
        let g = self.compute_gradient();
        if g > self.gradient_threshold {
            // Congestion — multiplicative decrease
            self.rate_bps = (self.rate_bps * self.md_factor).max(self.min_rate_bps);
            self.cwnd = (self.cwnd as f64 * self.md_factor).max(8.0) as u32;
            tracing::debug!(
                gradient = g,
                rate_mbps = self.rate_bps / 1e6,
                "CC: congestion, back off"
            );
        } else if g < -self.gradient_threshold {
            // Delay falling — ramp up aggressively
            self.rate_bps = (self.rate_bps + self.ai_factor * 2.0).min(self.max_rate_bps);
            self.cwnd = (self.cwnd + 2).min(4096);
        } else {
            // Stable — gentle additive increase
            self.rate_bps = (self.rate_bps + self.ai_factor).min(self.max_rate_bps);
            self.cwnd = (self.cwnd + 1).min(4096);
        }
    }

    /// Normalised delay gradient with exponential time-weighting.
    ///
    /// Weights each sample by recency: w = exp(-age_ms / 200).
    /// This uses the `tick` timestamp stored in each `DelaySample` to suppress
    /// stale samples when OWD changes rapidly.
    /// Gradient = (weighted_recent_half − weighted_older_half) / min_owd.
    fn compute_gradient(&self) -> f64 {
        let n = self.delay_samples.len();
        if n < 2 || self.min_owd_us == 0 || self.min_owd_us == u64::MAX {
            return 0.0;
        }
        const HALF_LIFE_MS: f64 = 200.0;
        let half = n / 2;

        let newest_tick = self.delay_samples.back().unwrap().tick;

        let weighted = |samples: &[DelaySample]| -> f64 {
            let (mut wsum, mut dsum) = (0.0_f64, 0.0_f64);
            for s in samples {
                let age_ms = (newest_tick - s.tick).to_millis() as f64;
                let w = libm::exp(-age_ms / HALF_LIFE_MS);
                dsum += s.owd_us as f64 * w;
                wsum += w;
            }
            if wsum == 0.0 {
                0.0
            } else {
                dsum / wsum
            }
        };

        let samples: alloc::vec::Vec<_> = self.delay_samples.iter().cloned().collect();
        let recent = weighted(&samples[half..]);
        let older = weighted(&samples[..half]);
        (recent - older) / self.min_owd_us as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(t: u64) -> InstantMs {
        InstantMs::from_ticks(t)
    }

    #[test]
    fn initial_state() {
        let cc = DelayBasedController::new(1_250_000.0, 125_000_000.0);
        assert_eq!(cc.rate_bps(), 1_250_000.0);
        assert_eq!(cc.cwnd(), 64);
    }

    #[test]
    fn loss_is_noop() {
        let mut cc = DelayBasedController::new(1_250_000.0, 125_000_000.0);
        let before_rate = cc.rate_bps();
        let before_cwnd = cc.cwnd();
        cc.on_loss(1);
        assert_eq!(cc.rate_bps(), before_rate);
        assert_eq!(cc.cwnd(), before_cwnd);
    }

    #[test]
    fn inter_packet_delay_10mbps() {
        // 10 Mbps = 1.25 MB/s → 1 200 B packet → ~960 µs
        let cc = DelayBasedController::new(1_250_000.0, 125_000_000.0);
        let d = cc.inter_packet_delay_us(1200);
        assert!(d > 900 && d < 1100, "expected ~960µs, got {d}µs");
    }

    #[test]
    fn fugit_clock_rising_delay_backs_off() {
        let mut cc = DelayBasedController::new(1_250_000.0, 125_000_000.0);
        let owds = [1000u64, 1100, 1200, 1400, 1600, 1800, 2000];
        for (i, owd) in owds.iter().enumerate() {
            cc.on_delay_sample(*owd, ms(i as u64 * 10));
        }
        assert!(
            cc.rate_bps() < 1_250_000.0,
            "rate should have decreased on rising OWD"
        );
    }

    #[test]
    fn fugit_clock_stable_delay_ramps_up() {
        let mut cc = DelayBasedController::new(500_000.0, 125_000_000.0);
        // Flat delay → additive increase
        for i in 0u64..20 {
            cc.on_delay_sample(1000, ms(i * 10));
        }
        assert!(
            cc.rate_bps() > 500_000.0,
            "rate should have increased on stable OWD"
        );
    }
}
