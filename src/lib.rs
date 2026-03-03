//! # chirp
//!
//! High-performance UDP-based file transfer protocol.
//!
//! Open-source alternative to Aspera FASP and Byteport DART for reliable,
//! high-throughput file transfer over lossy, high-latency networks
//! (satellite, WAN, drone links, air-gapped environments).
//!
//! ## Feature flags
//!
//! | Feature | Default | Description |
//! |---------|---------|-------------|
//! | `std`   | ✓       | Full async runtime: tokio, file I/O, AES-256-GCM, CLI |
//! | `alloc` | ✓ (via `std`) | Heap allocation; enables `no_std` use with an allocator |
//!
//! ## `no_std` usage
//!
//! Disable default features and enable `alloc`:
//!
//! ```toml
//! [dependencies]
//! chirp = { version = "*", default-features = false, features = ["alloc"] }
//! ```
//!
//! The protocol core (`packet`, `nack`, `fec`, `congestion`) is fully
//! `no_std + alloc`. Integrate with your own transport and clock.
//!
//! ## Quick start (`std`)
//!
//! ```no_run
//! use chirp::transfer::sender::{ChirpSender, SenderConfig};
//! use std::path::Path;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let config = SenderConfig {
//!         remote_addr: "192.168.1.100:9000".parse()?,
//!         ..Default::default()
//!     };
//!     let mut sender = ChirpSender::new(config).await?;
//!     let stats = sender.send_file(Path::new("large_file.bin")).await?;
//!     println!("Transferred {} bytes in {:.1}s ({:.1} Mbps)",
//!         stats.bytes_sent,
//!         stats.duration.as_secs_f64(),
//!         stats.throughput_mbps(),
//!     );
//!     Ok(())
//! }
//! ```

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(any(feature = "alloc", feature = "std"))]
extern crate alloc;

pub mod congestion;
pub mod protocol;

#[cfg(feature = "std")]
pub mod transfer;

pub use protocol::fec::FecMode;

#[cfg(feature = "std")]
pub use transfer::receiver::{ChirpReceiver, ReceiverConfig, RecvStats};
#[cfg(feature = "std")]
pub use transfer::sender::{ChirpSender, SenderConfig, TransferStats};

/// Protocol version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
