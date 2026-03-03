use std::net::SocketAddr;
use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use indicatif::{ProgressBar, ProgressStyle};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Configuration for the parallel TCP receiver.
#[derive(Clone, Debug)]
pub struct TcpReceiverConfig {
    pub bind_addr: SocketAddr,
    /// Number of parallel TCP streams to accept (default 8).
    pub streams: usize,
}

impl Default for TcpReceiverConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0:9000".parse().unwrap(),
            streams: 8,
        }
    }
}

/// Statistics returned after a TCP receive completes.
#[derive(Clone, Debug)]
pub struct TcpRecvStats {
    pub bytes_received: u64,
    pub duration: std::time::Duration,
    pub streams_used: usize,
}

impl TcpRecvStats {
    pub fn throughput_mbps(&self) -> f64 {
        let secs = self.duration.as_secs_f64();
        if secs > 0.0 {
            (self.bytes_received as f64 * 8.0) / (secs * 1_000_000.0)
        } else {
            0.0
        }
    }
}

/// Parallel TCP file receiver.
///
/// Listens on a single TCP port, accepts N connections, and writes each
/// chunk to the correct offset in the output file.
pub struct TcpReceiver {
    config: TcpReceiverConfig,
}

impl TcpReceiver {
    pub fn new(config: TcpReceiverConfig) -> Self {
        Self { config }
    }

    /// Receive a file over N parallel TCP streams.
    pub async fn receive_file(&self, output_path: &Path) -> Result<TcpRecvStats> {
        let listener = TcpListener::bind(self.config.bind_addr).await?;
        let n = self.config.streams;

        // We need to figure out total file size from the incoming headers.
        // Accept all N connections and read headers first, then stream data.
        let start = Instant::now();

        // Pre-allocate by accepting all streams and reading headers
        let mut connections = Vec::with_capacity(n);
        for _ in 0..n {
            let (stream, _peer) = listener.accept().await?;
            connections.push(stream);
        }

        // Read headers from all connections
        let mut stream_info: Vec<(u32, u64, u64, tokio::net::TcpStream)> = Vec::with_capacity(n);
        for mut conn in connections {
            let mut header = [0u8; 20];
            conn.read_exact(&mut header).await?;

            let stream_id = u32::from_be_bytes(header[0..4].try_into().unwrap());
            let offset = u64::from_be_bytes(header[4..12].try_into().unwrap());
            let length = u64::from_be_bytes(header[12..20].try_into().unwrap());

            stream_info.push((stream_id, offset, length, conn));
        }

        // Compute total file size from max(offset + length)
        let total_size = stream_info
            .iter()
            .map(|(_, offset, length, _)| offset + length)
            .max()
            .unwrap_or(0);

        // Pre-allocate the output file
        let file = tokio::fs::File::create(output_path).await?;
        file.set_len(total_size).await?;
        drop(file);

        // Progress bar
        let pb = ProgressBar::new(total_size);
        let style = ProgressStyle::with_template(
            "{spinner:.green} TCP recv {bar:40.cyan/blue} {percent:>3}% {bytes}/{total_bytes} {binary_bytes_per_sec:>12} ETA {eta:>6}",
        )
        .unwrap()
        .progress_chars("#>-");
        pb.set_style(style);
        pb.enable_steady_tick(std::time::Duration::from_millis(120));

        // Spawn a task per stream to write its chunk
        let output_path = output_path.to_path_buf();
        let mut handles = Vec::with_capacity(n);

        for (stream_id, offset, length, tcp_stream) in stream_info {
            let path = output_path.clone();
            let pb = pb.clone();
            handles.push(tokio::spawn(async move {
                recv_chunk(stream_id, offset, length, tcp_stream, &path, &pb).await
            }));
        }

        let mut bytes_received = 0u64;
        for handle in handles {
            bytes_received += handle.await??;
        }

        pb.finish_and_clear();

        Ok(TcpRecvStats {
            bytes_received,
            duration: start.elapsed(),
            streams_used: n,
        })
    }
}

/// Receive a single chunk: read data from TCP stream, write to file at offset.
async fn recv_chunk(
    _stream_id: u32,
    offset: u64,
    length: u64,
    mut tcp_stream: tokio::net::TcpStream,
    path: &Path,
    pb: &ProgressBar,
) -> Result<u64> {
    let mut file = tokio::fs::OpenOptions::new().write(true).open(path).await?;
    file.seek(std::io::SeekFrom::Start(offset)).await?;

    let mut remaining = length;
    let mut buf = vec![0u8; 64 * 1024];
    while remaining > 0 {
        let to_read = remaining.min(buf.len() as u64) as usize;
        let n = tcp_stream.read(&mut buf[..to_read]).await?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).await?;
        remaining -= n as u64;
        pb.inc(n as u64);
    }

    file.flush().await?;
    Ok(length - remaining)
}
