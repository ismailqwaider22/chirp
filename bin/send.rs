use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use tokio::fs::File;
use tokio::io;
use tracing_subscriber::EnvFilter;

use chirp::transfer::sender::{ChirpSender, SendProgress, SenderConfig};
use chirp::transfer::tcp_sender::{TcpSender, TcpSenderConfig};
use chirp::FecMode;

/// chirp-send: High-performance UDP file sender.
#[derive(Parser, Debug)]
#[command(name = "chirp-send", about = "Send a file using the chirp protocol")]
struct Args {
    /// Path to the file to send. Use '-' for stdin streaming mode.
    file: String,

    /// Remote address (host:port) of the receiver.
    #[arg(default_value = "127.0.0.1:9000")]
    remote: String,

    /// Initial sending rate in Mbps.
    #[arg(long, default_value = "10")]
    rate: u64,

    /// Maximum sending rate in Mbps.
    #[arg(long, default_value = "1000")]
    max_rate: u64,

    /// Disable forward error correction.
    #[arg(long)]
    no_fec: bool,

    /// Use RLNC (random linear network coding) instead of XOR FEC.
    #[arg(long)]
    fec_rlnc: bool,

    /// Enable AES-256-GCM encryption (uses random key - for testing only).
    #[arg(long)]
    encrypt: bool,

    /// Use parallel TCP streams instead of UDP.
    #[arg(long)]
    tcp: bool,

    /// Number of parallel TCP streams (default 8, requires --tcp).
    #[arg(long, default_value = "8")]
    streams: usize,
}

fn progress_bar(total: Option<u64>, filename: &str) -> ProgressBar {
    let pb = match total {
        Some(size) => ProgressBar::new(size),
        None => ProgressBar::new_spinner(),
    };

    let style = ProgressStyle::with_template(
        "{spinner:.green} {msg} {bar:40.cyan/blue} {percent:>3}% {bytes}/{total_bytes} {binary_bytes_per_sec:>12} ETA {eta:>6}",
    )
    .unwrap()
    .progress_chars("#>-");

    pb.set_style(style);
    pb.set_message(filename.to_string());
    pb.enable_steady_tick(Duration::from_millis(120));
    pb
}

fn apply_progress(pb: &ProgressBar, snapshot: &SendProgress) {
    pb.set_message(snapshot.filename.clone());
    pb.set_position(snapshot.bytes_sent);

    let pct = snapshot
        .total_size
        .map(|total| {
            if total == 0 {
                0.0
            } else {
                (snapshot.bytes_sent as f64 / total as f64) * 100.0
            }
        })
        .unwrap_or(0.0);
    let mbps = if snapshot.elapsed.as_secs_f64() > 0.0 {
        (snapshot.bytes_sent as f64 / 1_000_000.0) / snapshot.elapsed.as_secs_f64()
    } else {
        0.0
    };

    pb.set_prefix(format!(
        "{pct:>5.1}% {mbps:>8.2} MB/s FEC {} NACK {}",
        snapshot.fec_recoveries, snapshot.nack_count
    ));

    if snapshot.total_size.is_none() {
        pb.set_message(format!(
            "{}  {:.2} MB sent  {mbps:.2} MB/s  FEC {}  NACK {}",
            snapshot.filename,
            snapshot.bytes_sent as f64 / 1_000_000.0,
            snapshot.fec_recoveries,
            snapshot.nack_count
        ));
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    let remote_addr = args.remote.parse().map_err(|e| {
        anyhow::anyhow!(
            "invalid remote address '{}': {}. Expected format: host:port",
            args.remote,
            e
        )
    })?;

    if args.tcp {
        // ── Parallel TCP mode ──────────────────────────────────────────
        let file_path = PathBuf::from(&args.file);
        if args.file == "-" {
            anyhow::bail!("TCP mode does not support stdin streaming");
        }
        if !file_path.exists() {
            anyhow::bail!("file not found: {}", file_path.display());
        }

        let config = TcpSenderConfig {
            remote_addr,
            streams: args.streams,
        };
        let sender = TcpSender::new(config);
        let stats = sender.send_file(&file_path).await?;

        println!();
        println!("TCP transfer complete:");
        println!("  File:       {}", args.file);
        println!("  Bytes:      {}", stats.bytes_sent);
        println!("  Duration:   {:.2}s", stats.duration.as_secs_f64());
        println!("  Throughput: {:.2} Mbps", stats.throughput_mbps());
        println!("  Streams:    {}", stats.streams_used);
    } else {
        // ── UDP (chirp) mode ───────────────────────────────────────────
        let fec_mode = if args.fec_rlnc {
            FecMode::Rlnc
        } else {
            FecMode::Xor
        };
        let config = SenderConfig {
            remote_addr,
            initial_rate_bps: args.rate * 1_000_000,
            max_rate_bps: args.max_rate * 1_000_000,
            fec_enabled: !args.no_fec,
            fec_mode,
            encryption_enabled: args.encrypt,
            ..Default::default()
        };

        let mut sender = ChirpSender::new(config).await?;

        let stats = if args.file == "-" {
            let filename = "stdin".to_string();
            let pb = progress_bar(None, &filename);
            let mut stdin = io::stdin();
            let stats = sender
                .send_reader_with_progress(&mut stdin, None, filename, |snapshot| {
                    apply_progress(&pb, &snapshot);
                })
                .await?;
            pb.finish_and_clear();
            stats
        } else {
            let file_path = PathBuf::from(&args.file);
            if !file_path.exists() {
                anyhow::bail!("file not found: {}", file_path.display());
            }

            let mut file = File::open(&file_path).await?;
            let metadata = file.metadata().await?;
            let total = metadata.len();
            let filename = file_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let pb = progress_bar(Some(total), &filename);

            let stats = sender
                .send_reader_with_progress(&mut file, Some(total), filename, |snapshot| {
                    apply_progress(&pb, &snapshot);
                })
                .await?;
            pb.finish_and_clear();
            stats
        };

        println!();
        println!("Transfer complete:");
        println!("  File:       {}", args.file);
        println!("  Bytes:      {}", stats.bytes_sent);
        println!("  Duration:   {:.2}s", stats.duration.as_secs_f64());
        println!("  Throughput: {:.2} Mbps", stats.throughput_mbps());
        println!("  Packets:    {}", stats.packets_sent);
        println!("  Retransmit: {}", stats.retransmissions);
        println!("  NACKs:      {}", stats.nack_count);
        println!("  FEC recov:  {}", stats.fec_recoveries);
    }

    Ok(())
}
