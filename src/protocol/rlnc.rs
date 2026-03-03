//! Random Linear Network Coding (RLNC) over GF(2^8).
//!
//! Erasure coding that handles arbitrary loss patterns by generating
//! random linear combinations of source packets. The decoder uses
//! Gaussian elimination over GF(2^8) to recover the original data.
//!
//! Fully `no_std + alloc` compatible.

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

use super::gf::{gf_inv, gf_mul, gf_vec_add_mul};

/// Default generation size (source packets per coding block).
pub const RLNC_GENERATION_SIZE: usize = 16;

/// Default redundancy ratio: r = k * RLNC_REDUNDANCY_NUM / RLNC_REDUNDANCY_DEN.
pub const RLNC_REDUNDANCY_NUM: usize = 1;
pub const RLNC_REDUNDANCY_DEN: usize = 2;

/// RLNC encoder: buffers source packets and generates coded packets
/// with random GF(2^8) coefficients.
pub struct RlncEncoder {
    /// Source packets (each Vec<u8> is one chunk, padded to packet_size).
    source: Vec<Vec<u8>>,
    /// Packet size (all source packets padded to this).
    packet_size: usize,
}

impl Default for RlncEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl RlncEncoder {
    pub fn new() -> Self {
        Self {
            source: Vec::new(),
            packet_size: 0,
        }
    }

    /// Add a source packet. Returns its index.
    pub fn push(&mut self, data: &[u8]) -> usize {
        let idx = self.source.len();
        if data.len() > self.packet_size {
            self.packet_size = data.len();
            // Pad existing packets to new size.
            for pkt in &mut self.source {
                pkt.resize(self.packet_size, 0);
            }
        }
        let mut padded = data.to_vec();
        padded.resize(self.packet_size, 0);
        self.source.push(padded);
        idx
    }

    /// Generate one coded packet with random coefficients over GF(2^8).
    /// Returns (coefficients, coded_data).
    ///
    /// Requires an RNG source. Under `std`, use `rand::thread_rng()`.
    #[cfg(feature = "std")]
    pub fn encode_random(&self) -> (Vec<u8>, Vec<u8>) {
        use rand::RngCore;
        let mut rng = rand::thread_rng();
        let mut coeffs = vec![0u8; self.source.len()];
        rng.fill_bytes(&mut coeffs);
        // Ensure at least one non-zero coefficient.
        if coeffs.iter().all(|&c| c == 0) && !coeffs.is_empty() {
            coeffs[0] = 1;
        }
        let data = self.encode_with_coefficients(&coeffs);
        (coeffs, data)
    }

    /// Generate a coded packet with caller-provided coefficients.
    pub fn encode_with_coefficients(&self, coefficients: &[u8]) -> Vec<u8> {
        let mut coded = vec![0u8; self.packet_size];
        for (i, &coeff) in coefficients.iter().enumerate() {
            if coeff != 0 && i < self.source.len() {
                gf_vec_add_mul(&mut coded, &self.source[i], coeff);
            }
        }
        coded
    }

    /// How many source packets are buffered.
    pub fn len(&self) -> usize {
        self.source.len()
    }

    /// Whether the encoder has no source packets.
    pub fn is_empty(&self) -> bool {
        self.source.is_empty()
    }

    /// Current packet size.
    pub fn packet_size(&self) -> usize {
        self.packet_size
    }

    /// Clear buffer for next generation.
    pub fn clear(&mut self) {
        self.source.clear();
        self.packet_size = 0;
    }
}

/// RLNC decoder: collects coded packets and recovers source data
/// via Gaussian elimination over GF(2^8).
pub struct RlncDecoder {
    /// Number of source packets expected (k).
    k: usize,
    /// Packet size.
    packet_size: usize,
    /// Received coded packets: each is (coefficients, data).
    received: Vec<(Vec<u8>, Vec<u8>)>,
}

impl RlncDecoder {
    pub fn new(k: usize, packet_size: usize) -> Self {
        Self {
            k,
            packet_size,
            received: Vec::new(),
        }
    }

    /// Add a received coded packet (coefficients, data).
    /// Returns true if we now have enough to (potentially) decode.
    pub fn add_packet(&mut self, coefficients: Vec<u8>, data: Vec<u8>) -> bool {
        self.received.push((coefficients, data));
        self.received.len() >= self.k
    }

    /// Attempt Gaussian elimination over GF(2^8) to recover source packets.
    /// Returns Ok(Vec<Vec<u8>>) with k decoded source packets if successful.
    pub fn decode(&self) -> Result<Vec<Vec<u8>>, &'static str> {
        let k = self.k;
        if self.received.len() < k {
            return Err("not enough packets to decode");
        }

        // Build augmented matrix: [A | B]
        // A is n×k coefficient matrix, B is n×packet_size data matrix.
        let n = self.received.len();
        let mut coeff: Vec<Vec<u8>> = Vec::with_capacity(n);
        let mut data: Vec<Vec<u8>> = Vec::with_capacity(n);
        for (c, d) in &self.received {
            let mut c_padded = c.clone();
            c_padded.resize(k, 0);
            let mut d_padded = d.clone();
            d_padded.resize(self.packet_size, 0);
            coeff.push(c_padded);
            data.push(d_padded);
        }

        // Forward elimination with partial pivoting.
        for col in 0..k {
            // Find pivot row (first non-zero in this column, at or below `col`).
            let pivot_row = (col..n).find(|&r| coeff[r][col] != 0);
            let pivot_row = match pivot_row {
                Some(r) => r,
                None => return Err("matrix is singular, cannot decode"),
            };

            // Swap pivot row to position `col`.
            if pivot_row != col {
                coeff.swap(col, pivot_row);
                data.swap(col, pivot_row);
            }

            // Normalize pivot row: divide by pivot element.
            let pivot_val = coeff[col][col];
            let inv = gf_inv(pivot_val);
            for c in &mut coeff[col][col..k] {
                *c = gf_mul(*c, inv);
            }
            for d in &mut data[col][..self.packet_size] {
                *d = gf_mul(*d, inv);
            }

            // Eliminate all other rows.
            for row in 0..n {
                if row == col {
                    continue;
                }
                let factor = coeff[row][col];
                if factor == 0 {
                    continue;
                }
                // coeff[row] -= factor * coeff[col]
                // Must collect pivot row values first to avoid borrow conflict.
                let pivot_coeffs: Vec<u8> = coeff[col][col..k].to_vec();
                for (j, &pc) in pivot_coeffs.iter().enumerate() {
                    coeff[row][col + j] ^= gf_mul(factor, pc);
                }
                // data[row] -= factor * data[col]
                // Use gf_vec_add_mul for the data (large) — this is the hot path.
                let (left, right) = if row < col {
                    let (a, b) = data.split_at_mut(col);
                    (&mut a[row], &b[0])
                } else {
                    let (a, b) = data.split_at_mut(row);
                    (&mut b[0], &a[col])
                };
                gf_vec_add_mul(left, right, factor);
            }
        }

        // Extract the first k rows of the data matrix as decoded source packets.
        Ok(data.into_iter().take(k).collect())
    }

    /// How many packets received so far.
    pub fn received_count(&self) -> usize {
        self.received.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_identity() {
        // Encode 4 source packets, generate 4 identity-coded packets, decode.
        let packets: Vec<&[u8]> = vec![
            &[0x01, 0x02, 0x03, 0x04],
            &[0x10, 0x20, 0x30, 0x40],
            &[0xAA, 0xBB, 0xCC, 0xDD],
            &[0xFF, 0x00, 0x55, 0x77],
        ];
        let mut enc = RlncEncoder::new();
        for p in &packets {
            enc.push(p);
        }

        let mut dec = RlncDecoder::new(4, enc.packet_size());
        // Send identity-encoded packets (coefficient = 1 for one source).
        for i in 0..4 {
            let mut coeffs = vec![0u8; 4];
            coeffs[i] = 1;
            let coded = enc.encode_with_coefficients(&coeffs);
            dec.add_packet(coeffs, coded);
        }

        let decoded = dec.decode().unwrap();
        for (i, p) in packets.iter().enumerate() {
            assert_eq!(&decoded[i][..p.len()], *p);
        }
    }

    #[test]
    fn encode_decode_mixed() {
        // Use non-trivial coefficients.
        let packets: Vec<&[u8]> = vec![
            &[0x01, 0x02, 0x03],
            &[0x10, 0x20, 0x30],
            &[0xAA, 0xBB, 0xCC],
        ];
        let mut enc = RlncEncoder::new();
        for p in &packets {
            enc.push(p);
        }

        let coefficients_list = vec![
            vec![0x01, 0x00, 0x00], // identity
            vec![0x01, 0x01, 0x00], // sum of first two
            vec![0x00, 0x01, 0x01], // sum of last two
        ];

        let mut dec = RlncDecoder::new(3, enc.packet_size());
        for coeffs in &coefficients_list {
            let coded = enc.encode_with_coefficients(coeffs);
            dec.add_packet(coeffs.clone(), coded);
        }

        let decoded = dec.decode().unwrap();
        for (i, p) in packets.iter().enumerate() {
            assert_eq!(&decoded[i][..p.len()], *p);
        }
    }

    #[test]
    fn underdetermined_fails() {
        let mut dec = RlncDecoder::new(4, 8);
        dec.add_packet(vec![1, 0, 0, 0], vec![0; 8]);
        dec.add_packet(vec![0, 1, 0, 0], vec![0; 8]);
        // Only 2 of 4 needed.
        assert!(dec.decode().is_err());
    }

    #[test]
    fn overdetermined_succeeds() {
        let packets: Vec<&[u8]> = vec![&[0x42, 0x43], &[0x99, 0xAA]];
        let mut enc = RlncEncoder::new();
        for p in &packets {
            enc.push(p);
        }

        // Send 3 coded packets for k=2.
        let coefficients_list = vec![
            vec![0x01, 0x00],
            vec![0x00, 0x01],
            vec![0x03, 0x07], // random combo
        ];

        let mut dec = RlncDecoder::new(2, enc.packet_size());
        for coeffs in &coefficients_list {
            let coded = enc.encode_with_coefficients(coeffs);
            dec.add_packet(coeffs.clone(), coded);
        }

        let decoded = dec.decode().unwrap();
        for (i, p) in packets.iter().enumerate() {
            assert_eq!(&decoded[i][..p.len()], *p);
        }
    }

    #[cfg(feature = "std")]
    #[test]
    fn random_encode_decode() {
        let packets: Vec<Vec<u8>> = (0..8).map(|i| vec![(i * 17) as u8; 64]).collect();
        let mut enc = RlncEncoder::new();
        for p in &packets {
            enc.push(p);
        }

        // Generate k + r coded packets (r = k/2 = 4 redundant).
        let k = packets.len();
        let total = k + k / 2;
        let mut dec = RlncDecoder::new(k, enc.packet_size());
        for _ in 0..total {
            let (coeffs, coded) = enc.encode_random();
            dec.add_packet(coeffs, coded);
        }

        let decoded = dec.decode().unwrap();
        for (i, p) in packets.iter().enumerate() {
            assert_eq!(&decoded[i][..p.len()], *p);
        }
    }
}
