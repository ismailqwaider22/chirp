//! Standalone loopback benchmark for tc netem impairment testing.
//!
//! Spawns chirp sender + receiver on 127.0.0.1, transfers a fixed payload,
//! and prints one TSV line: label  size_mb  mbps  retransmits  elapsed_s  integrity
//!
//! Usage: netem_bench [--size-mb N] [--port P] [--no-fec] [--label NAME]
//!                    [--initial-rate-mbps N] [--fin-timeout N]

use anyhow::Result;
use chirp::transfer::{
    receiver::{ChirpReceiver, ReceiverConfig},
    sender::{ChirpSender, SenderConfig},
};
use std::{
    net::SocketAddr,
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::{fs, task::LocalSet, time::sleep};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let local = LocalSet::new();
    local.run_until(run()).await
}

async fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mut size_mb: usize = 20;
    let mut port: u16 = 39901;
    let mut fec_enabled = true;
    let mut label = String::from("loopback");
    let mut initial_rate_mbps: u64 = 500; // start fast; netem/CC will constrain
    let mut fin_timeout_secs: u64 = 120;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--size-mb" => {
                i += 1;
                size_mb = args[i].parse().unwrap_or(20);
            }
            "--port" => {
                i += 1;
                port = args[i].parse().unwrap_or(39901);
            }
            "--no-fec" => {
                fec_enabled = false;
            }
            "--label" => {
                i += 1;
                label = args[i].clone();
            }
            "--initial-rate-mbps" => {
                i += 1;
                initial_rate_mbps = args[i].parse().unwrap_or(500);
            }
            "--fin-timeout" => {
                i += 1;
                fin_timeout_secs = args[i].parse().unwrap_or(120);
            }
            _ => {}
        }
        i += 1;
    }

    let data: Vec<u8> = (0..size_mb * 1024 * 1024)
        .map(|i| (i.wrapping_mul(6364136223846793005_usize).wrapping_add(1) >> 33) as u8)
        .collect();
    let src = PathBuf::from(format!("/tmp/dart_nb_src_{port}.bin"));
    let dst = PathBuf::from(format!("/tmp/dart_nb_dst_{port}.bin"));
    fs::write(&src, &data).await?;
    let _ = fs::remove_file(&dst).await;

    let recv_addr: SocketAddr = format!("127.0.0.1:{port}").parse()?;

    let rc = ReceiverConfig {
        bind_addr: recv_addr,
        fec_enabled,
        idle_timeout_secs: 60,
        ..Default::default()
    };
    let sc = SenderConfig {
        remote_addr: recv_addr,
        fec_enabled,
        initial_rate_bps: initial_rate_mbps * 1_000_000,
        max_rate_bps: 10_000_000_000,
        fin_timeout_secs,
        ..Default::default()
    };

    sleep(Duration::from_millis(200)).await;

    let dst2 = dst.clone();
    // spawn_local does not require Send — works with non-Send async fns.
    let rh =
        tokio::task::spawn_local(
            async move { ChirpReceiver::new(rc).await?.receive_file(&dst2).await },
        );
    sleep(Duration::from_millis(500)).await; // wait for receiver to bind (> max one-way delay)

    let t0 = Instant::now();
    let mut sender = ChirpSender::new(sc).await?;
    let stats = sender.send_file(&src).await?;
    let elapsed = t0.elapsed();
    let _recv_stats = rh.await??;

    let received = fs::read(&dst).await.unwrap_or_default();
    let integrity = if received == data { "OK" } else { "FAIL" };
    let mbps = (size_mb as f64 * 8.0) / elapsed.as_secs_f64();

    println!(
        "{}\t{}\t{:.1}\t{}\t{:.3}\t{}",
        label,
        size_mb,
        mbps,
        stats.retransmissions,
        elapsed.as_secs_f64(),
        integrity
    );

    let _ = fs::remove_file(&src).await;
    let _ = fs::remove_file(&dst).await;
    Ok(())
}
