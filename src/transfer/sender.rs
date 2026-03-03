use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::congestion::delay_based::DelayBasedController;
use crate::protocol::fec::{FecEncoder, FecMode, FEC_BLOCK_SIZE};
use crate::protocol::packet::{Packet, PacketFlags, MAX_PACKET_SIZE, MAX_PAYLOAD};
use crate::protocol::rlnc::{
    RlncEncoder, RLNC_GENERATION_SIZE, RLNC_REDUNDANCY_DEN, RLNC_REDUNDANCY_NUM,
};

/// Configuration for a chirp sender.
#[derive(Clone, Debug)]
pub struct SenderConfig {
    /// Target address (receiver).
    pub remote_addr: SocketAddr,
    /// Initial sending rate in bits per second (default: 10 Mbps).
    pub initial_rate_bps: u64,
    /// Maximum sending rate in bits per second (default: 1 Gbps).
    pub max_rate_bps: u64,
    /// Enable FEC (forward error correction).
    pub fec_enabled: bool,
    /// FEC block size (data packets per parity packet, XOR mode).
    pub fec_block_size: usize,
    /// FEC mode: Xor (default, backward compat) or Rlnc.
    pub fec_mode: FecMode,
    /// RLNC generation size (source packets per coding block).
    pub rlnc_generation_size: usize,
    /// Enable AES-256-GCM encryption.
    pub encryption_enabled: bool,
    /// Encryption key (32 bytes for AES-256). If None, a random key is generated.
    pub encryption_key: Option<[u8; 32]>,
    /// Timeout waiting for final ACK (seconds).
    pub fin_timeout_secs: u64,
}

impl Default for SenderConfig {
    fn default() -> Self {
        Self {
            remote_addr: "127.0.0.1:9000".parse().unwrap(),
            initial_rate_bps: 10_000_000, // 10 Mbps
            max_rate_bps: 1_000_000_000,  // 1 Gbps
            fec_enabled: true,
            fec_block_size: FEC_BLOCK_SIZE,
            fec_mode: FecMode::Xor,
            rlnc_generation_size: RLNC_GENERATION_SIZE,
            encryption_enabled: false,
            encryption_key: None,
            fin_timeout_secs: 120,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SendProgress {
    pub filename: String,
    pub bytes_sent: u64,
    pub total_size: Option<u64>,
    pub elapsed: Duration,
    pub nack_count: u64,
    pub fec_recoveries: u64,
}

/// chirp file sender.
pub struct ChirpSender {
    config: SenderConfig,
    socket: Arc<UdpSocket>,
    cc: Arc<Mutex<DelayBasedController>>,
    /// Sent packets keyed by sequence number (for retransmission).
    sent_packets: HashMap<u32, SentPacket>,
    /// AES-GCM cipher (if encryption enabled).
    cipher: Option<Aes256Gcm>,
    /// XOR FEC encoder.
    fec_encoder: FecEncoder,
    /// RLNC encoder (used when fec_mode == Rlnc).
    rlnc_encoder: RlncEncoder,
    /// Sequence numbers of source packets in the current RLNC generation.
    rlnc_source_seqs: Vec<u32>,
    /// Next sequence number to assign.
    next_seq: u32,
    /// Sequence number of the last data packet.
    last_data_seq: u32,
    /// Last observed round-trip time (us) - proxy from NACK timestamps.
    last_rtt_us: u64,
    /// NACK packets received from receiver.
    nack_count: u64,
}

struct SentPacket {
    data: Vec<u8>, // wire-encoded packet
    sent_at: Instant,
    retransmit_count: u32,
}

impl ChirpSender {
    /// Create a new sender bound to an ephemeral local port.
    pub async fn new(config: SenderConfig) -> anyhow::Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        socket.connect(config.remote_addr).await?;

        let initial_rate_bytes = config.initial_rate_bps as f64 / 8.0;
        let max_rate_bytes = config.max_rate_bps as f64 / 8.0;
        let cc = DelayBasedController::new(initial_rate_bytes, max_rate_bytes);

        let cipher = if config.encryption_enabled {
            let key = config.encryption_key.unwrap_or_else(|| {
                use rand::RngCore;
                let mut key = [0u8; 32];
                OsRng.fill_bytes(&mut key);
                key
            });
            Some(
                Aes256Gcm::new_from_slice(&key)
                    .map_err(|e| anyhow::anyhow!("invalid key: {}", e))?,
            )
        } else {
            None
        };

        let fec_block_size = if config.fec_enabled {
            config.fec_block_size
        } else {
            usize::MAX // effectively disabled
        };

        Ok(Self {
            config,
            socket: Arc::new(socket),
            cc: Arc::new(Mutex::new(cc)),
            sent_packets: HashMap::new(),
            cipher,
            fec_encoder: FecEncoder::new(fec_block_size),
            rlnc_encoder: RlncEncoder::new(),
            rlnc_source_seqs: Vec::new(),
            next_seq: 1,
            last_data_seq: 0,
            last_rtt_us: 5_000, // 5 ms default until first NACK (typical LAN)
            nack_count: 0,
        })
    }

    /// Send a file to the remote receiver.
    pub async fn send_file(&mut self, path: &Path) -> anyhow::Result<TransferStats> {
        let mut file = File::open(path).await?;
        let metadata = file.metadata().await?;
        let file_size = metadata.len();

        let filename = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".into());

        self.send_reader_internal(&mut file, Some(file_size), filename, None)
            .await
    }

    /// Send bytes from any async reader.
    pub async fn send_reader<R>(
        &mut self,
        reader: &mut R,
        total_size: Option<u64>,
        filename: String,
    ) -> anyhow::Result<TransferStats>
    where
        R: AsyncRead + Unpin,
    {
        self.send_reader_internal(reader, total_size, filename, None)
            .await
    }

    /// Send bytes from any async reader with periodic progress snapshots.
    pub async fn send_reader_with_progress<R, F>(
        &mut self,
        reader: &mut R,
        total_size: Option<u64>,
        filename: String,
        mut progress: F,
    ) -> anyhow::Result<TransferStats>
    where
        R: AsyncRead + Unpin,
        F: FnMut(SendProgress) + Send,
    {
        self.send_reader_internal(reader, total_size, filename, Some(&mut progress))
            .await
    }

    async fn send_reader_internal<R>(
        &mut self,
        reader: &mut R,
        total_size: Option<u64>,
        filename: String,
        mut progress: Option<&mut (dyn FnMut(SendProgress) + Send)>,
    ) -> anyhow::Result<TransferStats>
    where
        R: AsyncRead + Unpin,
    {
        self.sent_packets.clear();
        self.next_seq = 1;
        self.last_data_seq = 0;
        self.nack_count = 0;

        info!(file = %filename, size = ?total_size, "starting transfer");

        // Phase 1: Send SYN and wait for ACK.
        self.send_syn(total_size.unwrap_or(u64::MAX), &filename)
            .await?;

        // TODO(resume): parse receiver-provided resume ranges from SYN-ACK and skip already-received ranges.

        // Phase 2: Stream data packets with rate control.
        let start = Instant::now();
        let mut last_tick = Instant::now(); // for time-based CC additive-increase
        let mut total_sent: u64 = 0;
        let mut sleep_debt_us: u64 = 0; // accumulated rate-control debt
        let mut buf = vec![0u8; MAX_PAYLOAD];
        let mut last_progress_emit = Instant::now();

        loop {
            let n = reader.read(&mut buf).await?;
            if n == 0 {
                break;
            }

            let payload = self.maybe_encrypt(&buf[..n])?;
            let seq = self.next_seq;
            self.next_seq += 1;
            self.last_data_seq = seq;

            let pkt = Packet::data(seq, payload.clone());
            let wire = pkt.encode();

            // Rate control: token-bucket accumulator.
            {
                let delay_us = {
                    let cc = self.cc.lock().await;
                    cc.inter_packet_delay_us(wire.len())
                };
                sleep_debt_us += delay_us;
                if sleep_debt_us >= 1_000 {
                    tokio::time::sleep(Duration::from_micros(sleep_debt_us)).await;
                    sleep_debt_us = 0;
                }
            }

            // Ignore spurious ICMP "port unreachable" that macOS receivers
            // can emit mid-transfer on connected UDP sockets.
            if let Err(e) = self.socket.send(&wire).await {
                let e: std::io::Error = e;
                if e.kind() != std::io::ErrorKind::ConnectionRefused {
                    return Err(e.into());
                }
            }
            self.sent_packets.insert(
                seq,
                SentPacket {
                    data: wire,
                    sent_at: Instant::now(),
                    retransmit_count: 0,
                },
            );
            total_sent += n as u64;

            // FEC: feed into encoder, send parity/coded packets as needed.
            if self.config.fec_enabled {
                match self.config.fec_mode {
                    FecMode::Xor => {
                        if let Some(parity) = self.fec_encoder.add_payload(&payload) {
                            let fec_seq = self.next_seq;
                            self.next_seq += 1;
                            let fec_pkt = Packet::fec(fec_seq, parity);
                            let fec_wire = fec_pkt.encode();
                            if let Err(e) = self.socket.send(&fec_wire).await {
                                let e: std::io::Error = e;
                                if e.kind() != std::io::ErrorKind::ConnectionRefused {
                                    return Err(e.into());
                                }
                            }
                        }
                    }
                    FecMode::Rlnc => {
                        self.rlnc_encoder.push(&payload);
                        self.rlnc_source_seqs.push(seq);
                        if self.rlnc_encoder.len() >= self.config.rlnc_generation_size {
                            self.flush_rlnc_generation().await?;
                        }
                    }
                }
            }

            // Time-based additive-increase tick every 100 ms.
            if last_tick.elapsed() >= Duration::from_millis(100) {
                last_tick = Instant::now();
                let mut cc = self.cc.lock().await;
                cc.tick_increase();
            }

            // Check for incoming NACKs (non-blocking).
            self.handle_feedback().await?;

            if last_progress_emit.elapsed() >= Duration::from_millis(200) {
                self.emit_progress(&mut progress, &filename, total_sent, total_size, start);
                last_progress_emit = Instant::now();
            }

            if total_sent % (1024 * 1024) < MAX_PAYLOAD as u64 {
                let rate_mbps = {
                    let cc = self.cc.lock().await;
                    cc.rate_bps() * 8.0 / 1_000_000.0
                };
                debug!(sent_mb = total_sent / (1024 * 1024), rate_mbps, "progress");
            }
        }

        // Flush partial FEC block.
        if self.config.fec_enabled {
            match self.config.fec_mode {
                FecMode::Xor => {
                    if let Some(parity) = self.fec_encoder.flush() {
                        let fec_seq = self.next_seq;
                        self.next_seq += 1;
                        let fec_pkt = Packet::fec(fec_seq, parity);
                        self.socket.send(&fec_pkt.encode()).await?;
                    }
                }
                FecMode::Rlnc => {
                    if !self.rlnc_encoder.is_empty() {
                        self.flush_rlnc_generation().await?;
                    }
                }
            }
        }

        // Phase 3: Send FIN and handle remaining NACKs until ACK.
        self.send_fin_and_drain().await?;

        let elapsed = start.elapsed();
        let retransmissions: u32 = self.sent_packets.values().map(|p| p.retransmit_count).sum();
        let stats = TransferStats {
            bytes_sent: total_sent,
            duration: elapsed,
            packets_sent: self.last_data_seq,
            retransmissions,
            nack_count: self.nack_count,
            fec_recoveries: 0,
        };

        self.emit_progress(&mut progress, &filename, total_sent, total_size, start);

        info!(
            bytes = stats.bytes_sent,
            duration_ms = stats.duration.as_millis(),
            throughput_mbps =
                (stats.bytes_sent as f64 * 8.0) / (stats.duration.as_secs_f64() * 1_000_000.0),
            retransmissions = stats.retransmissions,
            nacks = stats.nack_count,
            "transfer complete"
        );

        Ok(stats)
    }

    fn emit_progress(
        &self,
        progress: &mut Option<&mut (dyn FnMut(SendProgress) + Send)>,
        filename: &str,
        bytes_sent: u64,
        total_size: Option<u64>,
        start: Instant,
    ) {
        if let Some(cb) = progress.as_deref_mut() {
            cb(SendProgress {
                filename: filename.to_string(),
                bytes_sent,
                total_size,
                elapsed: start.elapsed(),
                nack_count: self.nack_count,
                fec_recoveries: 0,
            });
        }
    }

    async fn send_syn(&self, file_size: u64, filename: &str) -> anyhow::Result<()> {
        let syn = Packet::syn(file_size, filename);
        let wire = syn.encode();

        for attempt in 0..10 {
            match self.socket.send(&wire).await {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                    debug!(
                        attempt,
                        "SYN send: receiver not ready (ConnectionRefused), retrying"
                    );
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
            debug!(attempt, "sent SYN");

            let mut resp_buf = vec![0u8; MAX_PACKET_SIZE];
            match tokio::time::timeout(Duration::from_secs(2), self.socket.recv(&mut resp_buf))
                .await
            {
                Ok(Ok(n)) => {
                    if let Ok(pkt) = Packet::decode(&resp_buf[..n]) {
                        if pkt.header.flags.contains(PacketFlags::ACK) {
                            info!("SYN-ACK received, starting data transfer");
                            return Ok(());
                        }
                    }
                }
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                    debug!(
                        attempt,
                        "SYN recv: ConnectionRefused, receiver not ready, retrying"
                    );
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                Ok(Err(e)) => warn!(?e, "recv error during SYN"),
                Err(_) => debug!("SYN timeout, retrying"),
            }
        }

        anyhow::bail!("failed to establish connection after 10 SYN attempts")
    }

    /// Flush the current RLNC generation: emit r redundant coded packets.
    /// Each coded packet has the FEC flag and carries [2B k][2B gen_id][k coeff bytes][coded data].
    async fn flush_rlnc_generation(&mut self) -> anyhow::Result<()> {
        let k = self.rlnc_encoder.len();
        if k == 0 {
            return Ok(());
        }
        let r = (k * RLNC_REDUNDANCY_NUM / RLNC_REDUNDANCY_DEN).max(1);
        // Generation ID: first source seq of this generation.
        let gen_id = self.rlnc_source_seqs.first().copied().unwrap_or(0);
        let k16 = k as u16;
        let gen_id16 = gen_id as u16;

        for _ in 0..r {
            let (coeffs, coded_data) = self.rlnc_encoder.encode_random();
            // Build FEC payload: [2B k][2B gen_id][k bytes coefficients][coded_data]
            let mut fec_payload = Vec::with_capacity(4 + coeffs.len() + coded_data.len());
            fec_payload.extend_from_slice(&k16.to_be_bytes());
            fec_payload.extend_from_slice(&gen_id16.to_be_bytes());
            fec_payload.extend_from_slice(&coeffs);
            fec_payload.extend_from_slice(&coded_data);

            let fec_seq = self.next_seq;
            self.next_seq += 1;
            let fec_pkt = Packet::fec(fec_seq, fec_payload);
            let fec_wire = fec_pkt.encode();
            if let Err(e) = self.socket.send(&fec_wire).await {
                let e: std::io::Error = e;
                if e.kind() != std::io::ErrorKind::ConnectionRefused {
                    return Err(e.into());
                }
            }
        }

        self.rlnc_encoder.clear();
        self.rlnc_source_seqs.clear();
        Ok(())
    }

    /// Non-blocking check for NACK/ACK feedback from receiver.
    async fn handle_feedback(&mut self) -> anyhow::Result<()> {
        let mut buf = vec![0u8; MAX_PACKET_SIZE];
        loop {
            match self.socket.try_recv(&mut buf) {
                Ok(n) => {
                    if let Ok(pkt) = Packet::decode(&buf[..n]) {
                        if pkt.header.flags.contains(PacketFlags::NACK) {
                            self.handle_nack(&pkt).await?;
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(ref e) if e.kind() == std::io::ErrorKind::ConnectionRefused => break,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    async fn handle_nack(&mut self, nack_pkt: &Packet) -> anyhow::Result<()> {
        let payload = &nack_pkt.payload;
        let missing_count = payload.len() / 4;
        self.nack_count += 1;
        debug!(missing_count, "received NACK");

        let rtt_us: u64 = payload
            .chunks_exact(4)
            .next()
            .and_then(|c| {
                let seq = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
                self.sent_packets
                    .get(&seq)
                    .map(|sp| sp.sent_at.elapsed().as_micros() as u64)
            })
            .unwrap_or(self.last_rtt_us);

        if rtt_us > 0 {
            self.last_rtt_us = rtt_us;
        }

        {
            let mut cc = self.cc.lock().await;
            cc.on_loss(missing_count as u32);
        }

        for chunk in payload.chunks_exact(4) {
            let seq = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            if let Some(sent) = self.sent_packets.get_mut(&seq) {
                self.socket.send(&sent.data).await?;
                sent.retransmit_count += 1;
                sent.sent_at = Instant::now();
                debug!(seq, retransmit = sent.retransmit_count, "retransmitted");
            }
        }
        Ok(())
    }

    async fn send_fin_and_drain(&mut self) -> anyhow::Result<()> {
        let fin_seq = self.next_seq;
        let fin = Packet::fin(fin_seq);
        let fin_wire = fin.encode();

        let deadline = Instant::now() + Duration::from_secs(self.config.fin_timeout_secs);
        let mut buf = vec![0u8; MAX_PACKET_SIZE];
        let mut last_fin_sent = Instant::now() - Duration::from_secs(1);

        loop {
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "FIN timeout after {}s - receiver did not acknowledge completion; file data was transmitted but receipt is unconfirmed",
                    self.config.fin_timeout_secs
                );
            }

            if last_fin_sent.elapsed() >= Duration::from_millis(500) {
                self.socket.send(&fin_wire).await?;
                last_fin_sent = Instant::now();
                debug!("sent FIN");
            }

            loop {
                match self.socket.try_recv(&mut buf) {
                    Ok(n) => {
                        if let Ok(pkt) = Packet::decode(&buf[..n]) {
                            if pkt.header.flags.contains(PacketFlags::ACK)
                                && pkt.header.seq == fin_seq
                            {
                                info!("FIN-ACK received, transfer complete");
                                return Ok(());
                            }
                            if pkt.header.flags.contains(PacketFlags::NACK) {
                                self.handle_nack(&pkt).await?;
                            }
                        }
                    }
                    Err(ref e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::ConnectionRefused =>
                    {
                        break;
                    }
                    Err(e) => return Err(e.into()),
                }
            }

            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    fn maybe_encrypt(&self, data: &[u8]) -> anyhow::Result<Vec<u8>> {
        match &self.cipher {
            Some(cipher) => {
                use rand::RngCore;
                let mut nonce_bytes = [0u8; 12];
                OsRng.fill_bytes(&mut nonce_bytes);
                let nonce = Nonce::from_slice(&nonce_bytes);
                let ciphertext = cipher
                    .encrypt(nonce, data)
                    .map_err(|e| anyhow::anyhow!("encryption failed: {}", e))?;
                let mut out = Vec::with_capacity(12 + ciphertext.len());
                out.extend_from_slice(&nonce_bytes);
                out.extend_from_slice(&ciphertext);
                Ok(out)
            }
            None => Ok(data.to_vec()),
        }
    }
}

/// Statistics from a completed transfer.
#[derive(Debug, Clone)]
pub struct TransferStats {
    pub bytes_sent: u64,
    pub duration: Duration,
    pub packets_sent: u32,
    pub retransmissions: u32,
    pub nack_count: u64,
    pub fec_recoveries: u64,
}

impl TransferStats {
    /// Throughput in megabits per second.
    pub fn throughput_mbps(&self) -> f64 {
        if self.duration.as_secs_f64() == 0.0 {
            return 0.0;
        }
        (self.bytes_sent as f64 * 8.0) / (self.duration.as_secs_f64() * 1_000_000.0)
    }
}
