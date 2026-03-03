//! Selective NACK tracker — `no_std + alloc` compatible.
//!
//! Uses `hashbrown::HashSet` (SwissTable, O(1) amortized) instead of `BTreeSet`
//! for O(log n) membership. For high packet rates the difference is measurable.
//!
//! Timing is driven by a caller-supplied [`InstantMs`] (fugit millisecond
//! instant), removing any dependency on `std::time::Instant`.

use fugit::Instant;
use hashbrown::HashSet;

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

/// Millisecond-resolution monotonic instant (1 000 ticks per second).
///
/// On `std` targets: `InstantMs::from_ticks(start.elapsed().as_millis() as u64)`.
/// On embedded: `InstantMs::from_ticks(rtos_tick_ms())`.
pub type InstantMs = Instant<u64, 1, 1_000>;

/// Millisecond-resolution duration.
pub type DurationMs = fugit::Duration<u64, 1, 1_000>;

/// Maximum gap window scanned by [`NackTracker::missing`].
///
/// Bounds two failure modes:
/// 1. **Memory explosion**: `missing(1)` with `highest_seen = u32::MAX` would
///    allocate a ~16 GB Vec. The cap keeps allocation O(1) regardless of seq range.
/// 2. **NACK packet overflow**: a NACK payload encodes seqs as 4-byte big-endian
///    integers. `MAX_PACKET_SIZE = 1212 B` → max 303 seqs per NACK. We report up
///    to 16 384 at a time (multiple NACKs will drain the full gap iteratively).
///
/// A retransmit window of 16 384 × 1 200 B ≈ 19 MB covers any practical
/// in-flight window at ≤1 Gbps with RTT ≤150 ms.
pub const MAX_GAP_WINDOW: u32 = 16_384;

/// Tracks received sequence numbers and identifies gaps for selective NACK.
pub struct NackTracker {
    received: HashSet<u32>,
    highest_seen: u32,
    /// Instant of the last NACK sent (`None` = never sent).
    last_nack: Option<InstantMs>,
    /// Minimum gap between consecutive NACKs.
    nack_interval: DurationMs,
}

impl NackTracker {
    /// Create a new tracker.
    ///
    /// `nack_interval_ms`: minimum milliseconds between NACKs (pass 0 for no rate-limiting).
    pub fn new(nack_interval_ms: u64) -> Self {
        Self {
            received: HashSet::new(),
            highest_seen: 0,
            last_nack: None,
            nack_interval: DurationMs::from_ticks(nack_interval_ms),
        }
    }

    /// Record a received sequence number.
    ///
    /// Uses wrapping comparison so `highest_seen` advances correctly across the
    /// `u32::MAX → 0` boundary: `seq` is considered "ahead" if its unsigned
    /// distance from `highest_seen` is less than 2³¹ (half the seq space).
    #[inline]
    pub fn record(&mut self, seq: u32) {
        self.received.insert(seq);
        if seq.wrapping_sub(self.highest_seen) < 0x8000_0000 {
            self.highest_seen = seq;
        }
    }

    /// Check if a sequence number has been received — O(1).
    #[inline]
    pub fn has(&self, seq: u32) -> bool {
        self.received.contains(&seq)
    }

    /// Highest sequence number seen so far.
    #[inline]
    pub fn highest(&self) -> u32 {
        self.highest_seen
    }

    /// All missing sequence numbers in `[start_seq, highest_seen]`, wrapping-safe.
    ///
    /// The scan is capped at [`MAX_GAP_WINDOW`] entries to prevent memory
    /// explosion near `u32::MAX` and to bound NACK packet size. If more gaps
    /// exist beyond the window, subsequent calls will drain them iteratively
    /// (the receiver sends periodic NACKs until all gaps are filled).
    pub fn missing(&self, start_seq: u32) -> Vec<u32> {
        // Forward distance from start_seq to highest_seen (wrapping).
        let dist = self.highest_seen.wrapping_sub(start_seq);
        let window = dist.min(MAX_GAP_WINDOW.saturating_sub(1));
        (0..=window)
            .map(|i| start_seq.wrapping_add(i))
            .filter(|s| !self.received.contains(s))
            .collect()
    }

    /// Returns the NACK list if the interval has elapsed since the last NACK.
    ///
    /// `now`: current monotonic instant — see [`InstantMs`] for construction.
    pub fn get_nack_list(&mut self, start_seq: u32, now: InstantMs) -> Option<Vec<u32>> {
        if let Some(last) = self.last_nack {
            // fugit Duration subtraction: panics if now < last (clock regression).
            // On well-behaved monotonic clocks this never happens.
            if now - last < self.nack_interval {
                return None;
            }
        }
        let gaps = self.missing(start_seq);
        if gaps.is_empty() {
            return None;
        }
        self.last_nack = Some(now);
        Some(gaps)
    }

    /// Number of unique packets received.
    #[inline]
    pub fn received_count(&self) -> usize {
        self.received.len()
    }

    /// Drop all entries below `below_seq` to bound memory growth.
    pub fn trim_below(&mut self, below_seq: u32) {
        self.received.retain(|&seq| seq >= below_seq);
    }

    /// Override `highest_seen` — test use only.
    /// Allows tests to simulate "we've already seen N packets" without
    /// recording N packets. Real code must use `record()`.
    #[cfg(test)]
    pub fn force_highest_seen(&mut self, seq: u32) {
        self.highest_seen = seq;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(t: u64) -> InstantMs {
        InstantMs::from_ticks(t)
    }

    #[test]
    fn detect_gaps() {
        let mut t = NackTracker::new(0);
        for s in [1u32, 2, 4, 6] {
            t.record(s);
        }
        assert_eq!(t.missing(1), alloc::vec![3, 5]);
    }

    #[test]
    fn no_gaps_when_complete() {
        let mut t = NackTracker::new(0);
        for s in 1u32..=10 {
            t.record(s);
        }
        assert!(t.missing(1).is_empty());
    }

    #[test]
    fn hashset_o1_contains() {
        let mut t = NackTracker::new(0);
        for s in 0u32..10_000 {
            t.record(s);
        }
        // All received — no gaps
        assert!(t.missing(0).is_empty());
        assert!(t.has(9_999));
        assert!(!t.has(10_000));
    }

    #[test]
    fn rate_limiting() {
        let mut t = NackTracker::new(200);
        t.record(1);
        t.record(3); // gap at 2
        assert!(t.get_nack_list(1, ms(0)).is_some()); // first — always fires
        assert!(t.get_nack_list(1, ms(100)).is_none()); // too soon
        assert!(t.get_nack_list(1, ms(201)).is_some()); // interval elapsed
    }

    #[test]
    fn trim_below_frees_memory() {
        let mut t = NackTracker::new(0);
        for s in 1u32..=100 {
            t.record(s);
        }
        t.trim_below(51);
        assert!(!t.has(50));
        assert!(t.has(51));
        assert_eq!(t.received_count(), 50);
    }

    // ── Wraparound & gap-window safety ──────────────────────────────────────

    #[test]
    fn wrapping_record_advances_highest_seen() {
        let mut t = NackTracker::new(0);
        // In practice, seq starts at 1 and climbs to u32::MAX after ~4 billion
        // packets (~5 TB at 1 200 B/packet). We cannot record 4 billion entries
        // in a test, so we use force_highest_seen() to set up the pre-wrap state.
        t.force_highest_seen(u32::MAX - 2);

        // Normal advance near the boundary.
        t.record(u32::MAX - 1);
        assert_eq!(
            t.highest(),
            u32::MAX - 1,
            "highest should advance normally near u32::MAX"
        );

        t.record(u32::MAX);
        assert_eq!(t.highest(), u32::MAX);

        // Wrap: 0.wrapping_sub(u32::MAX) = 1 < 0x8000_0000 → 0 IS ahead.
        t.record(0);
        assert_eq!(
            t.highest(),
            0,
            "highest should advance across the u32::MAX → 0 boundary"
        );

        t.record(1);
        assert_eq!(t.highest(), 1);
    }

    #[test]
    fn missing_bounded_by_max_gap_window() {
        let mut t = NackTracker::new(0);
        // highest_seen is far ahead of start_seq — would be billions without the cap.
        t.record(u32::MAX);
        // missing(1) would iterate [1..u32::MAX] without the cap — ~4 billion entries.
        let gaps = t.missing(1);
        assert!(
            gaps.len() <= MAX_GAP_WINDOW as usize,
            "missing() returned {} entries, expected ≤ {}",
            gaps.len(),
            MAX_GAP_WINDOW
        );
    }

    #[test]
    fn missing_wraps_across_u32_max() {
        let mut t = NackTracker::new(0);
        // Record packets on both sides of the wrap boundary.
        // Gaps: u32::MAX-1 and 1 are missing.
        t.record(u32::MAX - 2);
        t.record(u32::MAX);
        t.record(0);
        t.record(2);
        // highest_seen = 2 (wrapping advance: 2 is "ahead" of u32::MAX-2)
        // missing from u32::MAX-2: should include u32::MAX-1 and 1
        let gaps = t.missing(u32::MAX - 2);
        assert!(
            gaps.contains(&(u32::MAX - 1)),
            "expected u32::MAX-1 in gaps"
        );
        assert!(gaps.contains(&1), "expected 1 in gaps (post-wrap)");
    }
}
