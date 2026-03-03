use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use tokio::io;
use tracing_subscriber::EnvFilter;

use chirp::transfer::receiver::{ChirpReceiver, ReceiverConfig, RecvProgress};
use chirp::transfer::tcp_receiver::{TcpReceiver, TcpReceiverConfig};
use chirp::FecMode;

/// chirp-recv: High-performance UDP file receiver.
#[derive(Parser, Debug)]
#[command(name = "chirp-recv", about = "Receive a file using the chirp protocol")]
struct Args {
    /// Local port to listen on.
    #[arg(default_value = "9000")]
    port: u16,

    /// Output file path. Use '-' to write to stdout.
    #[arg(short, long)]
    output: Option<String>,

    /// Disable forward error correction.
    #[arg(long)]
    no_fec: bool,

    /// Use RLNC (random linear network coding) instead of XOR FEC.
    #[arg(long)]
    fec_rlnc: bool,

    /// Enable AES-256-GCM decryption (key must be provided via --key).
    #[arg(long)]
    encrypt: bool,

    /// Hex-encoded 32-byte AES-256 key for decryption.
    #[arg(long)]
    key: Option<String>,

    /// Idle timeout in seconds.
    #[arg(long, default_value = "30")]
    timeout: u64,

    /// Use parallel TCP streams instead of UDP.
    #[arg(long)]
    tcp: bool,

    /// Number of parallel TCP streams (default 8, requires --tcp).
    #[arg(long, default_value = "8")]
    streams: usize,
}

fn progress_bar() -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    let style = ProgressStyle::with_template(
        "{spinner:.green} {msg} {bar:40.cyan/blue} {percent:>3}% {bytes}/{total_bytes} {binary_bytes_per_sec:>12} ETA {eta:>6}",
    )
    .unwrap()
    .progress_chars("#>-");
    pb.set_style(style);
    pb.enable_steady_tick(Duration::from_millis(120));
    pb
}

fn apply_progress(pb: &ProgressBar, snapshot: &RecvProgress) {
    pb.set_message(format!(
        "{}  FEC {}  NACK {}",
        snapshot.filename, snapshot.fec_recoveries, snapshot.nack_count
    ));
    pb.set_position(snapshot.bytes_received);
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    let encryption_key = if args.encrypt {
        let hex = args
            .key
            .ok_or_else(|| anyhow::anyhow!("--encrypt requires --key <hex-key>"))?;
        let bytes = hex::decode(&hex).map_err(|e| anyhow::anyhow!("invalid hex key: {}", e))?;
        if bytes.len() != 32 {
            anyhow::bail!(
                "key must be exactly 32 bytes (64 hex chars), got {}",
                bytes.len()
            );
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes);
        Some(key)
    } else {
        None
    };

    let bind_addr = format!("0.0.0.0:{}", args.port).parse()?;

    if args.tcp {
        // ── Parallel TCP mode ──────────────────────────────────────────
        let output = args
            .output
            .unwrap_or_else(|| "chirp_received_file".to_string());
        if output == "-" {
            anyhow::bail!("TCP mode does not support stdout output");
        }

        let config = TcpReceiverConfig {
            bind_addr,
            streams: args.streams,
        };
        let receiver = TcpReceiver::new(config);
        let output_path = PathBuf::from(&output);
        let stats = receiver.receive_file(&output_path).await?;

        println!();
        println!("TCP receive complete:");
        println!("  File:       {}", output);
        println!("  Bytes:      {}", stats.bytes_received);
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
        let config = ReceiverConfig {
            bind_addr,
            fec_enabled: !args.no_fec,
            fec_mode,
            encryption_enabled: args.encrypt,
            encryption_key,
            idle_timeout_secs: args.timeout,
            ..Default::default()
        };

        let receiver = ChirpReceiver::new(config).await?;
        let output = args
            .output
            .unwrap_or_else(|| "chirp_received_file".to_string());

        let stats = if output == "-" {
            let mut stdout = io::stdout();
            receiver
                .receive_writer(&mut stdout, "stdout".to_string())
                .await?
        } else {
            let output_path = PathBuf::from(&output);
            let pb = progress_bar();
            let stats = receiver
                .receive_file_with_progress(&output_path, |snapshot| {
                    apply_progress(&pb, &snapshot);
                })
                .await?;
            pb.finish_and_clear();
            stats
        };

        println!();
        println!("Receive complete:");
        println!("  File:       {}", stats.filename);
        println!("  Bytes:      {}", stats.bytes_received);
        println!("  Duration:   {:.2}s", stats.duration.as_secs_f64());
        println!("  Throughput: {:.2} Mbps", stats.throughput_mbps());
        println!("  Packets:    {}", stats.packets_received);
        println!("  NACKs:      {}", stats.nack_count);
        println!("  FEC recov:  {}", stats.fec_recoveries);
    }

    Ok(())
}
