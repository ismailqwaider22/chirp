//! On-wire packet format for the DART protocol.
//!
//! Wire layout (big-endian):
//! ```text
//! [4B magic][4B seq][2B flags][2B payload_len][payload...]
//! ```

// no_std support
#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

use bytes::{Buf, BufMut, BytesMut};
use core::fmt;

/// Magic bytes: ASCII "DART".
pub const MAGIC: [u8; 4] = [0x44, 0x41, 0x52, 0x54];

/// Maximum payload per packet (MTU-safe; leaves room for UDP/IP headers).
pub const MAX_PAYLOAD: usize = 1200;

/// Header size: 4B magic + 4B seq + 2B flags + 2B payload_len.
pub const HEADER_SIZE: usize = 12;

/// Maximum packet size on the wire.
pub const MAX_PACKET_SIZE: usize = HEADER_SIZE + MAX_PAYLOAD;

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct PacketFlags: u16 {
        const DATA = 0x01;
        const NACK = 0x02;
        const FIN  = 0x04;
        const ACK  = 0x08;
        const FEC  = 0x10;
        const SYN  = 0x20;
    }
}

/// On-wire packet header.
#[derive(Clone, PartialEq, Eq)]
pub struct Header {
    pub seq: u32,
    pub flags: PacketFlags,
    pub payload_len: u16,
}

impl fmt::Debug for Header {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Header")
            .field("seq", &self.seq)
            .field("flags", &self.flags)
            .field("payload_len", &self.payload_len)
            .finish()
    }
}

/// A complete DART packet (header + payload).
#[derive(Clone, Debug)]
pub struct Packet {
    pub header: Header,
    pub payload: Vec<u8>,
}

impl Packet {
    pub fn data(seq: u32, payload: Vec<u8>) -> Self {
        let payload_len = payload.len() as u16;
        Self {
            header: Header {
                seq,
                flags: PacketFlags::DATA,
                payload_len,
            },
            payload,
        }
    }

    pub fn fin(seq: u32) -> Self {
        Self {
            header: Header {
                seq,
                flags: PacketFlags::FIN,
                payload_len: 0,
            },
            payload: Vec::new(),
        }
    }

    pub fn ack(seq: u32) -> Self {
        Self {
            header: Header {
                seq,
                flags: PacketFlags::ACK,
                payload_len: 0,
            },
            payload: Vec::new(),
        }
    }

    pub fn syn(total_size: u64, filename: &str) -> Self {
        let mut payload = Vec::with_capacity(8 + filename.len());
        payload.extend_from_slice(&total_size.to_be_bytes());
        payload.extend_from_slice(filename.as_bytes());
        let payload_len = payload.len() as u16;
        Self {
            header: Header {
                seq: 0,
                flags: PacketFlags::SYN,
                payload_len,
            },
            payload,
        }
    }

    pub fn nack(missing: &[u32]) -> Self {
        let mut payload = Vec::with_capacity(missing.len() * 4);
        for &seq in missing {
            payload.extend_from_slice(&seq.to_be_bytes());
        }
        let payload_len = payload.len() as u16;
        Self {
            header: Header {
                seq: 0,
                flags: PacketFlags::NACK,
                payload_len,
            },
            payload,
        }
    }

    pub fn fec(seq: u32, parity: Vec<u8>) -> Self {
        let payload_len = parity.len() as u16;
        Self {
            header: Header {
                seq,
                flags: PacketFlags::FEC,
                payload_len,
            },
            payload: parity,
        }
    }

    /// Encode to wire bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = BytesMut::with_capacity(HEADER_SIZE + self.payload.len());
        buf.put_slice(&MAGIC);
        buf.put_u32(self.header.seq);
        buf.put_u16(self.header.flags.bits());
        buf.put_u16(self.header.payload_len);
        buf.put_slice(&self.payload);
        buf.to_vec()
    }

    /// Decode from wire bytes.
    pub fn decode(data: &[u8]) -> Result<Self, PacketError> {
        if data.len() < HEADER_SIZE {
            return Err(PacketError::TooShort(data.len()));
        }
        let mut buf = data;
        let mut magic = [0u8; 4];
        magic.copy_from_slice(&buf[..4]);
        buf.advance(4);
        if magic != MAGIC {
            return Err(PacketError::BadMagic(magic));
        }
        let seq = buf.get_u32();
        let flags_raw = buf.get_u16();
        let flags = PacketFlags::from_bits(flags_raw).ok_or(PacketError::BadFlags(flags_raw))?;
        let payload_len = buf.get_u16() as usize;
        if buf.remaining() < payload_len {
            return Err(PacketError::PayloadTruncated {
                expected: payload_len,
                got: buf.remaining(),
            });
        }
        let payload = buf[..payload_len].to_vec();
        Ok(Self {
            header: Header {
                seq,
                flags,
                payload_len: payload_len as u16,
            },
            payload,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PacketError {
    #[error("packet too short: {0} bytes")]
    TooShort(usize),
    #[error("bad magic bytes")]
    BadMagic([u8; 4]),
    #[error("unknown flags: 0x{0:04x}")]
    BadFlags(u16),
    #[error("payload truncated: expected {expected}, got {got}")]
    PayloadTruncated { expected: usize, got: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_data() {
        let pkt = Packet::data(42, alloc::vec![1, 2, 3, 4, 5]);
        let decoded = Packet::decode(&pkt.encode()).unwrap();
        assert_eq!(decoded.header.seq, 42);
        assert!(decoded.header.flags.contains(PacketFlags::DATA));
        assert_eq!(decoded.payload, [1, 2, 3, 4, 5]);
    }

    #[test]
    fn roundtrip_nack() {
        let pkt = Packet::nack(&[10, 20, 30]);
        let decoded = Packet::decode(&pkt.encode()).unwrap();
        assert!(decoded.header.flags.contains(PacketFlags::NACK));
        assert_eq!(decoded.header.payload_len, 12);
    }

    #[test]
    fn bad_magic_rejected() {
        let mut wire = Packet::data(1, alloc::vec![0u8; 4]).encode();
        wire[0] = 0xFF;
        assert!(matches!(
            Packet::decode(&wire),
            Err(PacketError::BadMagic(_))
        ));
    }
}
