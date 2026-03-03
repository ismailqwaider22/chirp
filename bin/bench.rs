use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use tokio::io::{duplex, sink, AsyncWriteExt};
use tokio::task::LocalSet;

use chirp::transfer::{
    receiver::{ChirpReceiver, ReceiverConfig},
    sender::{ChirpSender, SenderConfig},
};

#[derive(Parser, Debug)]
#[command(name = "chirp-bench", about = "In-process chirp loopback benchmark")]
struct Args {
    /// Payload size in MB.
    #[arg(long, default_value = "64")]
    size: usize,
}

fn pick_loopback_port() -> Result<u16> {
    let sock = std::net::UdpSocket::bind("127.0.0.1:0")?;
    Ok(sock.local_addr()?.port())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let local = LocalSet::new();
    local.run_until(run()).await
}

async fn run() -> Result<()> {
    let args = Args::parse();
    let size_bytes = args.size * 1024 * 1024;

    let payload: Vec<u8> = (0..size_bytes)
        .map(|i| (i.wrapping_mul(6364136223846793005_usize).wrapping_add(1) >> 33) as u8)
        .collect();

    let port = pick_loopback_port()?;
    let recv_addr: SocketAddr = format!("127.0.0.1:{port}").parse()?;

    let receiver_cfg = ReceiverConfig {
        bind_addr: recv_addr,
        idle_timeout_secs: 30,
        ..Default::default()
    };
    let sender_cfg = SenderConfig {
        remote_addr: recv_addr,
        initial_rate_bps: 1_000_000_000,
        max_rate_bps: 10_000_000_000,
        fin_timeout_secs: 30,
        ..Default::default()
    };

    // spawn_local does not require Send — works with non-Send async fns.
    let receiver_task = tokio::task::spawn_local(async move {
        let receiver = ChirpReceiver::new(receiver_cfg).await?;
        let mut out = sink();
        receiver.receive_writer(&mut out, "bench".to_string()).await
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut rd, mut wr) = duplex(1024 * 1024);
    let producer_payload = payload;
    let producer = tokio::task::spawn_local(async move {
        wr.write_all(&producer_payload).await?;
        wr.shutdown().await?;
        Ok::<(), anyhow::Error>(())
    });

    let sender_task = tokio::task::spawn_local(async move {
        let mut sender = ChirpSender::new(sender_cfg).await?;
        sender
            .send_reader(&mut rd, Some(size_bytes as u64), "bench.bin".to_string())
            .await
    });

    let t0 = Instant::now();
    let send_stats = sender_task.await??;
    producer.await??;
    let recv_stats = receiver_task.await??;
    let elapsed = t0.elapsed();

    let throughput_mbps =
        (send_stats.bytes_sent as f64 * 8.0) / (elapsed.as_secs_f64() * 1_000_000.0);

    println!("chirp-bench");
    println!("  size_mb:         {}", args.size);
    println!("  time_s:          {:.3}", elapsed.as_secs_f64());
    println!("  throughput_mbps: {:.2}", throughput_mbps);
    println!("  send_nacks:      {}", send_stats.nack_count);
    println!("  send_fec_recoveries: {}", send_stats.fec_recoveries);
    println!("  recv_nacks:      {}", recv_stats.nack_count);
    println!("  recv_fec_recoveries: {}", recv_stats.fec_recoveries);

    Ok(())
}
