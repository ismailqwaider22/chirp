use std::net::SocketAddr;
use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::net::TcpStream;

/// Configuration for the parallel TCP sender.
#[derive(Clone, Debug)]
pub struct TcpSenderConfig {
    pub remote_addr: SocketAddr,
    /// Number of parallel TCP streams (default 8).
    pub streams: usize,
}

impl Default for TcpSenderConfig {
    fn default() -> Self {
        Self {
            remote_addr: "127.0.0.1:9000".parse().unwrap(),
            streams: 8,
        }
    }
}

/// Statistics returned after a TCP transfer completes.
#[derive(Clone, Debug)]
pub struct TcpTransferStats {
    pub bytes_sent: u64,
    pub duration: std::time::Duration,
    pub streams_used: usize,
}

impl TcpTransferStats {
    pub fn throughput_mbps(&self) -> f64 {
        let secs = self.duration.as_secs_f64();
        if secs > 0.0 {
            (self.bytes_sent as f64 * 8.0) / (secs * 1_000_000.0)
        } else {
            0.0
        }
    }
}

/// Parallel TCP file sender.
///
/// Splits a file into N chunks and streams each over a dedicated TCP connection.
pub struct TcpSender {
    config: TcpSenderConfig,
}

impl TcpSender {
    pub fn new(config: TcpSenderConfig) -> Self {
        Self { config }
    }

    /// Send a file over N parallel TCP streams.
    pub async fn send_file(&self, path: &Path) -> Result<TcpTransferStats> {
        let metadata = tokio::fs::metadata(path).await?;
        let total_size = metadata.len();
        let n = self.config.streams;
        let start = Instant::now();

        let chunk_size = total_size / n as u64;
        let mut handles = Vec::with_capacity(n);

        for i in 0..n {
            let offset = i as u64 * chunk_size;
            let length = if i == n - 1 {
                total_size - offset
            } else {
                chunk_size
            };

            let remote_addr = self.config.remote_addr;
            let path = path.to_path_buf();

            handles.push(tokio::spawn(async move {
                send_chunk(remote_addr, i as u32, offset, length, &path).await
            }));
        }

        let mut bytes_sent = 0u64;
        for handle in handles {
            bytes_sent += handle.await??;
        }

        Ok(TcpTransferStats {
            bytes_sent,
            duration: start.elapsed(),
            streams_used: n,
        })
    }
}

/// Send a single chunk: connect, write header, stream file data.
async fn send_chunk(
    addr: SocketAddr,
    stream_id: u32,
    offset: u64,
    length: u64,
    path: &Path,
) -> Result<u64> {
    use tokio::io::AsyncWriteExt;

    let mut stream = TcpStream::connect(addr).await?;

    // Write 20-byte header: [stream_id(4) | offset(8) | length(8)] big-endian
    let mut header = [0u8; 20];
    header[0..4].copy_from_slice(&stream_id.to_be_bytes());
    header[4..12].copy_from_slice(&offset.to_be_bytes());
    header[12..20].copy_from_slice(&length.to_be_bytes());
    stream.write_all(&header).await?;

    // Stream file data from the correct offset
    let mut file = File::open(path).await?;
    file.seek(std::io::SeekFrom::Start(offset)).await?;

    let mut remaining = length;
    let mut buf = vec![0u8; 64 * 1024]; // 64 KB buffer
    while remaining > 0 {
        let to_read = remaining.min(buf.len() as u64) as usize;
        let n = file.read(&mut buf[..to_read]).await?;
        if n == 0 {
            break;
        }
        stream.write_all(&buf[..n]).await?;
        remaining -= n as u64;
    }

    stream.shutdown().await?;
    Ok(length)
}
