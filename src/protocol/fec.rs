//! XOR-based Forward Error Correction — `no_std + alloc` compatible.
//!
//! For every N data packets (a "block"), emit 1 parity packet whose payload
//! is the XOR of all N payloads. If exactly one packet in the block is lost,
//! the receiver reconstructs it by XOR-ing the parity with the N-1 survivors.
//!
//! Limitation: single-loss recovery per block. For multi-loss recovery,
//! Reed-Solomon or fountain codes (e.g. RaptorQ) are required.

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

/// Default: 1 parity packet per 8 data packets.
pub const FEC_BLOCK_SIZE: usize = 8;

/// FEC operating mode.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum FecMode {
    /// XOR-based single-loss recovery (original).
    #[default]
    Xor,
    /// Random Linear Network Coding over GF(2^8) — handles arbitrary loss patterns.
    Rlnc,
}

/// Encodes data packets into XOR parity blocks.
pub struct FecEncoder {
    block_size: usize,
    parity: Vec<u8>,
    max_len: usize,
    count: usize,
}

impl FecEncoder {
    pub fn new(block_size: usize) -> Self {
        Self {
            block_size,
            parity: Vec::new(),
            max_len: 0,
            count: 0,
        }
    }

    /// Feed a data payload. Returns `Some(parity)` when a full block completes.
    pub fn add_payload(&mut self, payload: &[u8]) -> Option<Vec<u8>> {
        if payload.len() > self.max_len {
            self.max_len = payload.len();
            self.parity.resize(self.max_len, 0);
        }
        for (i, &b) in payload.iter().enumerate() {
            self.parity[i] ^= b;
        }
        self.count += 1;
        if self.count >= self.block_size {
            let p = self.parity.clone();
            self.reset();
            Some(p)
        } else {
            None
        }
    }

    /// Flush a partial block (end of file).
    pub fn flush(&mut self) -> Option<Vec<u8>> {
        if self.count > 0 {
            let p = self.parity.clone();
            self.reset();
            Some(p)
        } else {
            None
        }
    }

    fn reset(&mut self) {
        self.parity.clear();
        self.max_len = 0;
        self.count = 0;
    }
}

/// Recovers a single missing packet from XOR parity.
pub struct FecDecoder {
    block_size: usize,
}

impl FecDecoder {
    pub fn new(block_size: usize) -> Self {
        Self { block_size }
    }

    /// Recover the missing packet given (block_size - 1) received payloads + parity.
    /// Returns `None` if the wrong number of payloads is provided.
    pub fn recover(&self, received: &[&[u8]], parity: &[u8]) -> Option<Vec<u8>> {
        if received.len() != self.block_size - 1 {
            return None;
        }
        let mut out = parity.to_vec();
        for payload in received {
            for (i, &b) in payload.iter().enumerate() {
                if i < out.len() {
                    out[i] ^= b;
                }
            }
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xor_roundtrip() {
        let mut enc = FecEncoder::new(4);
        let pkts: &[&[u8]] = &[
            &[0x01, 0x02, 0x03],
            &[0x10, 0x20, 0x30],
            &[0xAA, 0xBB, 0xCC],
            &[0xFF, 0x00, 0x55],
        ];
        let mut parity = None;
        for p in pkts {
            parity = enc.add_payload(p);
        }
        let parity = parity.unwrap();
        let dec = FecDecoder::new(4);
        let recv: Vec<&[u8]> = alloc::vec![pkts[0], pkts[1], pkts[3]];
        let recovered = dec.recover(&recv, &parity).unwrap();
        assert_eq!(recovered, pkts[2]);
    }

    #[test]
    fn wrong_receiver_count_returns_none() {
        let dec = FecDecoder::new(4);
        let parity = alloc::vec![0u8; 4];
        assert!(dec.recover(&[], &parity).is_none()); // needs 3, got 0
    }
}
