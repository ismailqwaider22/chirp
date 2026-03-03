//! In-process loopback transfer tests — sender and receiver run as tokio tasks
//! in the same process, communicating over UDP 127.0.0.1. No subprocesses.

use std::path::Path;
use std::time::Instant;
use tokio::fs;

use chirp::transfer::{
    receiver::{ChirpReceiver, ReceiverConfig},
    sender::{ChirpSender, SenderConfig},
};
use chirp::FecMode;

/// Generate deterministic pseudo-random bytes (no rand dependency in tests).
fn gen_data(size: usize) -> Vec<u8> {
    (0..size)
        .map(|i| (i.wrapping_mul(6364136223846793005_usize).wrapping_add(1) >> 33) as u8)
        .collect()
}

async fn loopback(size_mb: usize, port: u16, fec: bool, rate_gbps: f64, fin_timeout_secs: u64) {
    let src = format!("/tmp/dart-loopback-src-{port}.bin");
    let dst = format!("/tmp/dart-loopback-dst-{port}.bin");
    let _ = fs::remove_file(&dst).await;

    let data = gen_data(size_mb * 1024 * 1024);
    fs::write(&src, &data).await.unwrap();

    let recv_cfg = ReceiverConfig {
        bind_addr: format!("0.0.0.0:{port}").parse().unwrap(),
        fec_enabled: fec,
        idle_timeout_secs: fin_timeout_secs + 10,
        ..Default::default()
    };
    let send_cfg = SenderConfig {
        remote_addr: format!("127.0.0.1:{port}").parse().unwrap(),
        fec_enabled: fec,
        initial_rate_bps: (rate_gbps * 1e9 / 8.0) as u64,
        max_rate_bps: 10_000_000_000,
        fin_timeout_secs,
        ..Default::default()
    };

    // Receiver task
    let dst_path = dst.clone();
    let recv_handle = tokio::spawn(async move {
        let recv = ChirpReceiver::new(recv_cfg).await.unwrap();
        recv.receive_file(Path::new(&dst_path)).await.unwrap()
    });

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Sender
    let t0 = Instant::now();
    let mut sender = ChirpSender::new(send_cfg).await.unwrap();
    let stats = sender.send_file(Path::new(&src)).await.unwrap();
    let elapsed = t0.elapsed();

    let recv_timeout = fin_timeout_secs + 15;
    let recv_stats =
        tokio::time::timeout(std::time::Duration::from_secs(recv_timeout), recv_handle)
            .await
            .expect("receiver timed out")
            .unwrap();

    // Integrity check
    let received = fs::read(&dst).await.unwrap_or_default();
    assert_eq!(received.len(), data.len(), "size mismatch");
    assert_eq!(received, data, "data integrity check FAILED");

    eprintln!(
        "\n  [{size_mb} MB | fec={fec} | ~{:.1}Gbps cap]\n  \
         elapsed:        {:.3}s\n  \
         throughput:     {:.0} Mbps\n  \
         retransmits:    {}\n  \
         recv bytes:     {} MB\n  \
         integrity:      OK ✓",
        rate_gbps,
        elapsed.as_secs_f64(),
        stats.throughput_mbps(),
        stats.retransmissions,
        recv_stats.bytes_received / (1024 * 1024),
    );

    // Cleanup
    let _ = fs::remove_file(&src).await;
    let _ = fs::remove_file(&dst).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_1mb_fec_on() {
    // 1 Gbps cap, 5s FIN timeout — small transfer, generous window
    loopback(1, 29901, true, 1.0, 5).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_10mb_fec_on() {
    loopback(10, 29902, true, 1.0, 10).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_100mb_fec_on() {
    // Cap at 1 Gbps for debug builds — avoids kernel UDP buffer saturation
    // and the resulting NACK storm that blows the FIN timeout.
    loopback(100, 29903, true, 1.0, 30).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_10mb_fec_off() {
    loopback(10, 29904, false, 1.0, 10).await;
}

async fn loopback_rlnc(size_mb: usize, port: u16, rate_gbps: f64, fin_timeout_secs: u64) {
    let src = format!("/tmp/dart-loopback-src-{port}.bin");
    let dst = format!("/tmp/dart-loopback-dst-{port}.bin");
    let _ = fs::remove_file(&dst).await;

    let data = gen_data(size_mb * 1024 * 1024);
    fs::write(&src, &data).await.unwrap();

    let recv_cfg = ReceiverConfig {
        bind_addr: format!("0.0.0.0:{port}").parse().unwrap(),
        fec_enabled: true,
        fec_mode: FecMode::Rlnc,
        idle_timeout_secs: fin_timeout_secs + 10,
        ..Default::default()
    };
    let send_cfg = SenderConfig {
        remote_addr: format!("127.0.0.1:{port}").parse().unwrap(),
        fec_enabled: true,
        fec_mode: FecMode::Rlnc,
        initial_rate_bps: (rate_gbps * 1e9 / 8.0) as u64,
        max_rate_bps: 10_000_000_000,
        fin_timeout_secs,
        ..Default::default()
    };

    let dst_path = dst.clone();
    let recv_handle = tokio::spawn(async move {
        let recv = ChirpReceiver::new(recv_cfg).await.unwrap();
        recv.receive_file(Path::new(&dst_path)).await.unwrap()
    });

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let t0 = Instant::now();
    let mut sender = ChirpSender::new(send_cfg).await.unwrap();
    let stats = sender.send_file(Path::new(&src)).await.unwrap();
    let elapsed = t0.elapsed();

    let recv_timeout = fin_timeout_secs + 15;
    let recv_stats =
        tokio::time::timeout(std::time::Duration::from_secs(recv_timeout), recv_handle)
            .await
            .expect("receiver timed out")
            .unwrap();

    let received = fs::read(&dst).await.unwrap_or_default();
    assert_eq!(received.len(), data.len(), "size mismatch");
    assert_eq!(received, data, "data integrity check FAILED");

    eprintln!(
        "\n  [{size_mb} MB | fec=RLNC | ~{:.1}Gbps cap]\n  \
         elapsed:        {:.3}s\n  \
         throughput:     {:.0} Mbps\n  \
         retransmits:    {}\n  \
         recv bytes:     {} MB\n  \
         integrity:      OK",
        rate_gbps,
        elapsed.as_secs_f64(),
        stats.throughput_mbps(),
        stats.retransmissions,
        recv_stats.bytes_received / (1024 * 1024),
    );

    let _ = fs::remove_file(&src).await;
    let _ = fs::remove_file(&dst).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_10mb_rlnc_fec() {
    loopback_rlnc(10, 29905, 1.0, 10).await;
}
