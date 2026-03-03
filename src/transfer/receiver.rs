use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use serde::{Deserialize, Serialize};
use tokio::fs::File;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

use crate::protocol::fec::{FecDecoder, FecMode, FEC_BLOCK_SIZE};
use crate::protocol::nack::{InstantMs, NackTracker};
use crate::protocol::packet::{Packet, PacketFlags, MAX_PACKET_SIZE, MAX_PAYLOAD};
use crate::protocol::rlnc::{RlncDecoder, RLNC_GENERATION_SIZE};

/// Configuration for a chirp receiver.
#[derive(Clone, Debug)]
pub struct ReceiverConfig {
    /// Local address to bind to.
    pub bind_addr: SocketAddr,
    /// NACK interval in milliseconds (rate-limit NACK emissions).
    pub nack_interval_ms: u64,
    /// Enable FEC recovery.
    pub fec_enabled: bool,
    /// FEC block size (XOR mode).
    pub fec_block_size: usize,
    /// FEC mode: Xor (default) or Rlnc.
    pub fec_mode: FecMode,
    /// RLNC generation size (must match sender).
    pub rlnc_generation_size: usize,
    /// Enable AES-256-GCM decryption.
    pub encryption_enabled: bool,
    /// Decryption key (must match sender's key).
    pub encryption_key: Option<[u8; 32]>,
    /// Idle timeout: give up if no packets for this long.
    pub idle_timeout_secs: u64,
}

impl Default for ReceiverConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0:9000".parse().unwrap(),
            nack_interval_ms: 50,
            fec_enabled: true,
            fec_block_size: FEC_BLOCK_SIZE,
            fec_mode: FecMode::Xor,
            rlnc_generation_size: RLNC_GENERATION_SIZE,
            encryption_enabled: false,
            encryption_key: None,
            idle_timeout_secs: 5,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RecvProgress {
    pub filename: String,
    pub bytes_received: u64,
    pub total_size: Option<u64>,
    pub elapsed: Duration,
    pub nack_count: u64,
    pub fec_recoveries: u64,
}

/// chirp file receiver.
pub struct ChirpReceiver {
    config: ReceiverConfig,
    socket: UdpSocket,
    cipher: Option<Aes256Gcm>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ByteRange {
    start: u64,
    end: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PartialSidecar {
    version: u32,
    ranges: Vec<ByteRange>,
}

impl ChirpReceiver {
    /// Create a new receiver bound to the configured address.
    pub async fn new(config: ReceiverConfig) -> anyhow::Result<Self> {
        let socket = UdpSocket::bind(config.bind_addr).await?;
        info!(addr = %config.bind_addr, "receiver listening");

        let cipher = if config.encryption_enabled {
            let key = config
                .encryption_key
                .ok_or_else(|| anyhow::anyhow!("encryption enabled but no key provided"))?;
            Some(
                Aes256Gcm::new_from_slice(&key)
                    .map_err(|e| anyhow::anyhow!("invalid key: {}", e))?,
            )
        } else {
            None
        };

        Ok(Self {
            config,
            socket,
            cipher,
        })
    }

    /// Receive a file and write it to the given output path.
    pub async fn receive_file(&self, output_path: &Path) -> anyhow::Result<RecvStats> {
        let mut file = File::create(output_path).await?;
        self.receive_writer_internal(
            &mut file,
            Some(output_path),
            None,
            output_path.to_string_lossy().to_string(),
        )
        .await
    }

    /// Receive a file with periodic progress snapshots.
    pub async fn receive_file_with_progress<F>(
        &self,
        output_path: &Path,
        mut progress: F,
    ) -> anyhow::Result<RecvStats>
    where
        F: FnMut(RecvProgress) + Send,
    {
        let mut file = File::create(output_path).await?;
        self.receive_writer_internal(
            &mut file,
            Some(output_path),
            Some(&mut progress),
            output_path.to_string_lossy().to_string(),
        )
        .await
    }

    /// Receive a file and write to an arbitrary async writer.
    pub async fn receive_writer<W>(
        &self,
        writer: &mut W,
        output_name: String,
    ) -> anyhow::Result<RecvStats>
    where
        W: AsyncWrite + Unpin,
    {
        self.receive_writer_internal(writer, None, None, output_name)
            .await
    }

    /// Receive a file and write to an arbitrary async writer with progress snapshots.
    pub async fn receive_writer_with_progress<W, F>(
        &self,
        writer: &mut W,
        output_name: String,
        mut progress: F,
    ) -> anyhow::Result<RecvStats>
    where
        W: AsyncWrite + Unpin,
        F: FnMut(RecvProgress) + Send,
    {
        self.receive_writer_internal(writer, None, Some(&mut progress), output_name)
            .await
    }

    async fn receive_writer_internal<W>(
        &self,
        writer: &mut W,
        output_path: Option<&Path>,
        mut progress: Option<&mut (dyn FnMut(RecvProgress) + Send)>,
        output_name: String,
    ) -> anyhow::Result<RecvStats>
    where
        W: AsyncWrite + Unpin,
    {
        let mut buf = vec![0u8; MAX_PACKET_SIZE];
        let mut nack_tracker = NackTracker::new(self.config.nack_interval_ms);
        let mut received_data: HashMap<u32, Vec<u8>> = HashMap::new();
        let mut fec_blocks: HashMap<u32, Vec<u8>> = HashMap::new(); // block_start_seq -> parity
        let fec_decoder = FecDecoder::new(self.config.fec_block_size);
        // RLNC: map from generation_id (first source seq) to (decoder, source_seqs).
        let mut rlnc_gens: HashMap<u16, (RlncDecoder, Vec<u32>)> = HashMap::new();
        // Track FEC seq numbers in RLNC mode so we can exclude them from missing-data checks.
        let mut rlnc_fec_seqs: hashbrown::HashSet<u32> = hashbrown::HashSet::new();
        let mut total_packets: u64 = 0;
        let mut file_size: u64 = 0;
        let mut received_bytes: usize = 0;
        let mut filename = String::new();
        let mut fin_received = false;
        let mut fin_seq: u32 = 0;
        let mut nack_count: u64 = 0;
        let mut fec_recoveries: u64 = 0;
        let start = Instant::now();
        let mut last_progress_emit = Instant::now();
        let mut last_sidecar_write = Instant::now();
        let sidecar_path = output_path.map(sidecar_path);

        if let Some(sidecar) = &sidecar_path {
            if sidecar.exists() {
                match tokio::fs::read(sidecar).await {
                    Ok(raw) => {
                        if let Ok(existing) = serde_json::from_slice::<PartialSidecar>(&raw) {
                            info!(
                                path = %sidecar.display(),
                                ranges = existing.ranges.len(),
                                "loaded existing .chirp-partial sidecar"
                            );
                            // TODO(resume): send these ranges during handshake and skip on sender.
                        }
                    }
                    Err(e) => warn!(?e, "failed to read existing sidecar"),
                }
            }
        }

        info!("waiting for connection...");
        let sender: SocketAddr = loop {
            let (n, addr) = tokio::time::timeout(
                Duration::from_secs(self.config.idle_timeout_secs),
                self.socket.recv_from(&mut buf),
            )
            .await
            .map_err(|_| anyhow::anyhow!("timeout waiting for connection"))??;

            if let Ok(pkt) = Packet::decode(&buf[..n]) {
                if pkt.header.flags.contains(PacketFlags::SYN) {
                    // Parse SYN payload: [8B file_size][filename...]
                    if pkt.payload.len() >= 8 {
                        file_size = u64::from_be_bytes(pkt.payload[..8].try_into().unwrap());
                        filename = String::from_utf8_lossy(&pkt.payload[8..]).to_string();
                    }
                    info!(
                        from = %addr,
                        file = %filename,
                        size = file_size,
                        "connection established"
                    );
                    let ack = Packet::ack(0);
                    self.socket.send_to(&ack.encode(), addr).await?;
                    break addr;
                }
            }
        };

        let mut last_activity = Instant::now();
        let unknown_size = file_size == u64::MAX;
        let total_size_opt = if unknown_size { None } else { Some(file_size) };

        loop {
            if fin_received {
                let full_missing =
                    self.full_missing_data_seqs(fin_seq, &received_data, &rlnc_fec_seqs);
                let complete = if unknown_size {
                    full_missing.is_empty()
                } else {
                    received_bytes as u64 >= file_size
                };
                if complete {
                    let ack = Packet::ack(fin_seq).encode();
                    for _ in 0..3 {
                        self.socket.send_to(&ack, sender).await?;
                    }
                    info!("all data received, sent FIN-ACK");
                    break;
                }
            }

            if last_activity.elapsed() > Duration::from_secs(self.config.idle_timeout_secs) {
                anyhow::bail!(
                    "idle timeout - no packets received for {}s",
                    self.config.idle_timeout_secs
                );
            }

            match tokio::time::timeout(Duration::from_millis(100), self.socket.recv_from(&mut buf))
                .await
            {
                Ok(Ok((n, addr))) => {
                    if addr != sender {
                        continue;
                    }
                    last_activity = Instant::now();

                    if let Ok(pkt) = Packet::decode(&buf[..n]) {
                        self.process_packet(
                            &pkt,
                            &mut nack_tracker,
                            &mut received_data,
                            &mut fec_blocks,
                            &fec_decoder,
                            &mut rlnc_gens,
                            &mut rlnc_fec_seqs,
                            &mut total_packets,
                            &mut fin_received,
                            &mut fin_seq,
                            &mut received_bytes,
                            &mut fec_recoveries,
                        )?;
                    }
                }
                Ok(Err(e)) => {
                    warn!(?e, "recv error");
                }
                Err(_) => {}
            }

            const MAX_NACK_SEQS: usize = 300;
            if fin_received && fin_seq > 0 {
                let full_missing =
                    self.full_missing_data_seqs(fin_seq, &received_data, &rlnc_fec_seqs);
                if !full_missing.is_empty() {
                    for chunk in full_missing.chunks(MAX_NACK_SEQS) {
                        let nack = Packet::nack(chunk);
                        if let Err(e) = self.socket.send_to(&nack.encode(), sender).await {
                            if e.kind() != std::io::ErrorKind::ConnectionRefused {
                                return Err(e.into());
                            }
                        }
                        nack_count += 1;
                    }
                    debug!(
                        missing_count = full_missing.len(),
                        "sent full-scan NACK (post-FIN)"
                    );
                }
            } else if let Some(missing) = nack_tracker
                .get_nack_list(1, InstantMs::from_ticks(start.elapsed().as_millis() as u64))
            {
                for chunk in missing.chunks(MAX_NACK_SEQS) {
                    let nack = Packet::nack(chunk);
                    if let Err(e) = self.socket.send_to(&nack.encode(), sender).await {
                        if e.kind() != std::io::ErrorKind::ConnectionRefused {
                            return Err(e.into());
                        }
                    }
                    nack_count += 1;
                }
                debug!(missing_count = missing.len() as u32, "sent NACK");
            }

            if let Some(sidecar) = &sidecar_path {
                if last_sidecar_write.elapsed() >= Duration::from_secs(1) {
                    write_partial_sidecar(sidecar, &received_data).await?;
                    last_sidecar_write = Instant::now();
                }
            }

            if last_progress_emit.elapsed() >= Duration::from_millis(200) {
                self.emit_progress(
                    &mut progress,
                    &filename,
                    received_bytes as u64,
                    total_size_opt,
                    start,
                    nack_count,
                    fec_recoveries,
                    &output_name,
                );
                last_progress_emit = Instant::now();
            }
        }

        let max_seq = received_data.keys().copied().max().unwrap_or(0);
        let mut bytes_written: u64 = 0;

        for seq in 1..=max_seq {
            if let Some(data) = received_data.get(&seq) {
                if unknown_size {
                    writer.write_all(data).await?;
                    bytes_written += data.len() as u64;
                } else {
                    if bytes_written >= file_size {
                        break;
                    }
                    let remaining = (file_size - bytes_written) as usize;
                    let to_write = core::cmp::min(data.len(), remaining);
                    writer.write_all(&data[..to_write]).await?;
                    bytes_written += to_write as u64;
                }
            }
        }
        writer.flush().await?;

        if let Some(sidecar) = &sidecar_path {
            if let Err(e) = tokio::fs::remove_file(sidecar).await {
                if e.kind() != std::io::ErrorKind::NotFound {
                    warn!(?e, "failed to remove sidecar after successful transfer");
                }
            }
        }

        let elapsed = start.elapsed();
        let display_name = if filename.is_empty() {
            output_name
        } else {
            filename
        };
        let stats = RecvStats {
            bytes_received: bytes_written,
            duration: elapsed,
            packets_received: total_packets,
            filename: display_name,
            nack_count,
            fec_recoveries,
        };

        self.emit_progress(
            &mut progress,
            &stats.filename,
            stats.bytes_received,
            total_size_opt,
            start,
            nack_count,
            fec_recoveries,
            &stats.filename,
        );

        info!(
            bytes = stats.bytes_received,
            duration_ms = stats.duration.as_millis(),
            throughput_mbps =
                (stats.bytes_received as f64 * 8.0) / (stats.duration.as_secs_f64() * 1_000_000.0),
            nacks = stats.nack_count,
            fec_recoveries = stats.fec_recoveries,
            "receive complete"
        );

        Ok(stats)
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_progress(
        &self,
        progress: &mut Option<&mut (dyn FnMut(RecvProgress) + Send)>,
        filename: &str,
        bytes_received: u64,
        total_size: Option<u64>,
        start: Instant,
        nack_count: u64,
        fec_recoveries: u64,
        output_name: &str,
    ) {
        if let Some(cb) = progress.as_deref_mut() {
            let display_name = if filename.is_empty() {
                output_name
            } else {
                filename
            };
            cb(RecvProgress {
                filename: display_name.to_string(),
                bytes_received,
                total_size,
                elapsed: start.elapsed(),
                nack_count,
                fec_recoveries,
            });
        }
    }

    fn full_missing_data_seqs(
        &self,
        fin_seq: u32,
        received_data: &HashMap<u32, Vec<u8>>,
        rlnc_fec_seqs: &hashbrown::HashSet<u32>,
    ) -> Vec<u32> {
        let bs = self.config.fec_block_size as u32;
        let xor_super_block =
            if self.config.fec_enabled && self.config.fec_mode == FecMode::Xor && bs > 0 {
                bs + 1
            } else {
                0
            };

        (1..fin_seq)
            .filter(|&s| {
                // In XOR mode, FEC seqs are at periodic positions.
                let is_xor_fec = xor_super_block > 0 && s.saturating_sub(1) % xor_super_block == bs;
                // In RLNC mode, FEC seqs are tracked explicitly.
                let is_rlnc_fec = rlnc_fec_seqs.contains(&s);
                !is_xor_fec && !is_rlnc_fec && !received_data.contains_key(&s)
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn process_packet(
        &self,
        pkt: &Packet,
        nack_tracker: &mut NackTracker,
        received_data: &mut HashMap<u32, Vec<u8>>,
        fec_blocks: &mut HashMap<u32, Vec<u8>>,
        fec_decoder: &FecDecoder,
        rlnc_gens: &mut HashMap<u16, (RlncDecoder, Vec<u32>)>,
        rlnc_fec_seqs: &mut hashbrown::HashSet<u32>,
        total_packets: &mut u64,
        fin_received: &mut bool,
        fin_seq: &mut u32,
        received_bytes: &mut usize,
        fec_recoveries: &mut u64,
    ) -> anyhow::Result<()> {
        let flags = pkt.header.flags;

        if flags.contains(PacketFlags::DATA) {
            let seq = pkt.header.seq;
            let payload = self.maybe_decrypt(&pkt.payload)?;
            nack_tracker.record(seq);
            if !received_data.contains_key(&seq) {
                *received_bytes += payload.len();
            }
            received_data.insert(seq, payload);
            *total_packets += 1;

            if self.config.fec_enabled && self.config.fec_mode == FecMode::Xor {
                self.try_fec_recovery(
                    seq,
                    nack_tracker,
                    received_data,
                    fec_blocks,
                    fec_decoder,
                    received_bytes,
                    fec_recoveries,
                );
            }
        } else if flags.contains(PacketFlags::FEC) {
            nack_tracker.record(pkt.header.seq);

            if self.config.fec_enabled {
                match self.config.fec_mode {
                    FecMode::Xor => {
                        let block_start = self.fec_block_start(pkt.header.seq);
                        fec_blocks.insert(block_start, pkt.payload.clone());
                        self.try_fec_recovery(
                            block_start,
                            nack_tracker,
                            received_data,
                            fec_blocks,
                            fec_decoder,
                            received_bytes,
                            fec_recoveries,
                        );
                    }
                    FecMode::Rlnc => {
                        rlnc_fec_seqs.insert(pkt.header.seq);
                        self.process_rlnc_fec_packet(
                            &pkt.payload,
                            nack_tracker,
                            received_data,
                            rlnc_gens,
                            received_bytes,
                            fec_recoveries,
                        );
                    }
                }
            }
        } else if flags.contains(PacketFlags::FIN) {
            info!(seq = pkt.header.seq, "received FIN");
            *fin_received = true;
            *fin_seq = pkt.header.seq;
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn try_fec_recovery(
        &self,
        trigger_seq: u32,
        nack_tracker: &mut NackTracker,
        received_data: &mut HashMap<u32, Vec<u8>>,
        fec_blocks: &HashMap<u32, Vec<u8>>,
        fec_decoder: &FecDecoder,
        received_bytes: &mut usize,
        fec_recoveries: &mut u64,
    ) {
        let block_start = self.fec_block_start(trigger_seq);
        let block_end = block_start + self.config.fec_block_size as u32 - 1;

        let parity = match fec_blocks.get(&block_start) {
            Some(p) => p,
            None => return,
        };

        let mut missing_seq = None;
        let mut missing_count = 0;
        for seq in block_start..=block_end {
            if !received_data.contains_key(&seq) {
                missing_seq = Some(seq);
                missing_count += 1;
            }
        }

        if missing_count != 1 {
            return;
        }

        let missing = match missing_seq {
            Some(s) => s,
            None => return,
        };
        let mut present: Vec<&[u8]> = Vec::new();
        for seq in block_start..=block_end {
            if seq != missing {
                if let Some(data) = received_data.get(&seq) {
                    present.push(data.as_slice());
                }
            }
        }

        if let Some(recovered) = fec_decoder.recover(&present, parity) {
            debug!(seq = missing, "FEC recovered packet");
            nack_tracker.record(missing);
            if !received_data.contains_key(&missing) {
                *received_bytes += recovered.len();
                *fec_recoveries += 1;
            }
            received_data.insert(missing, recovered);
        }
    }

    /// Determine the starting sequence number of the FEC block containing `seq`.
    fn fec_block_start(&self, seq: u32) -> u32 {
        let bs = self.config.fec_block_size as u32;
        if bs == 0 {
            return seq;
        }
        let super_block_size = bs + 1;
        let k = (seq - 1) / super_block_size;
        k * super_block_size + 1
    }

    /// Process an RLNC FEC packet: parse header, feed to decoder, attempt recovery.
    /// FEC payload layout: [2B k][2B gen_id][k bytes coefficients][coded_data].
    #[allow(clippy::too_many_arguments)]
    fn process_rlnc_fec_packet(
        &self,
        payload: &[u8],
        nack_tracker: &mut NackTracker,
        received_data: &mut HashMap<u32, Vec<u8>>,
        rlnc_gens: &mut HashMap<u16, (RlncDecoder, Vec<u32>)>,
        received_bytes: &mut usize,
        fec_recoveries: &mut u64,
    ) {
        if payload.len() < 4 {
            return;
        }
        let k = u16::from_be_bytes([payload[0], payload[1]]) as usize;
        let gen_id = u16::from_be_bytes([payload[2], payload[3]]);
        if payload.len() < 4 + k {
            return;
        }
        let coefficients = payload[4..4 + k].to_vec();
        let coded_data = payload[4 + k..].to_vec();
        let packet_size = coded_data.len();

        let entry = rlnc_gens
            .entry(gen_id)
            .or_insert_with(|| (RlncDecoder::new(k, packet_size), Vec::new()));

        // Track which source seqs belong to this generation.
        // The source seqs are gen_id..(gen_id + k) in sequence space, but in RLNC mode
        // we don't interleave FEC seqs into the data seq space like XOR does.
        // Source seqs for this generation: contiguous block starting at gen_id.
        if entry.1.is_empty() {
            for i in 0..k {
                entry.1.push(gen_id as u32 + i as u32);
            }
        }

        let ready = entry.0.add_packet(coefficients, coded_data);

        if ready {
            // Check which source packets in this generation are missing.
            let source_seqs = entry.1.clone();
            let missing: Vec<usize> = source_seqs
                .iter()
                .enumerate()
                .filter(|(_, &seq)| !received_data.contains_key(&seq))
                .map(|(idx, _)| idx)
                .collect();

            if missing.is_empty() {
                return; // All source packets already received, nothing to recover.
            }

            if let Ok(decoded) = entry.0.decode() {
                for &idx in &missing {
                    if idx < decoded.len() && idx < source_seqs.len() {
                        let seq = source_seqs[idx];
                        let recovered = decoded[idx].clone();
                        debug!(seq, "RLNC recovered packet");
                        nack_tracker.record(seq);
                        if !received_data.contains_key(&seq) {
                            *received_bytes += recovered.len();
                            *fec_recoveries += 1;
                        }
                        received_data.insert(seq, recovered);
                    }
                }
            }
        }
    }

    fn maybe_decrypt(&self, data: &[u8]) -> anyhow::Result<Vec<u8>> {
        match &self.cipher {
            Some(cipher) => {
                if data.len() < 12 {
                    anyhow::bail!("encrypted payload too short for nonce");
                }
                let nonce = Nonce::from_slice(&data[..12]);
                let plaintext = cipher
                    .decrypt(nonce, &data[12..])
                    .map_err(|e| anyhow::anyhow!("decryption failed: {}", e))?;
                Ok(plaintext)
            }
            None => Ok(data.to_vec()),
        }
    }
}

fn sidecar_path(output_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.chirp-partial", output_path.display()))
}

async fn write_partial_sidecar(
    sidecar_path: &Path,
    received_data: &HashMap<u32, Vec<u8>>,
) -> anyhow::Result<()> {
    let mut ranges: Vec<ByteRange> = Vec::new();

    let mut seqs: Vec<u32> = received_data.keys().copied().collect();
    seqs.sort_unstable();

    let mut current: Option<ByteRange> = None;
    for seq in seqs {
        let payload = match received_data.get(&seq) {
            Some(p) => p,
            None => continue,
        };
        let start = (seq as u64).saturating_sub(1) * MAX_PAYLOAD as u64;
        let end = start + payload.len() as u64;

        match current.as_mut() {
            Some(r) if r.end == start => r.end = end,
            Some(_) => {
                if let Some(done) = current.take() {
                    ranges.push(done);
                }
                current = Some(ByteRange { start, end });
            }
            None => {
                current = Some(ByteRange { start, end });
            }
        }
    }

    if let Some(done) = current {
        ranges.push(done);
    }

    let sidecar = PartialSidecar { version: 1, ranges };
    let data = serde_json::to_vec_pretty(&sidecar)?;
    tokio::fs::write(sidecar_path, data).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    struct Helper {
        fec_enabled: bool,
        fec_block_size: usize,
    }
    impl Helper {
        fn is_fec_seq(&self, seq: u32) -> bool {
            if !self.fec_enabled || self.fec_block_size == 0 {
                return false;
            }
            let bs = self.fec_block_size as u32;
            seq.saturating_sub(1) % (bs + 1) == bs
        }
        fn fec_block_start(&self, seq: u32) -> u32 {
            let bs = self.fec_block_size as u32;
            if bs == 0 {
                return seq;
            }
            let k = (seq - 1) / (bs + 1);
            k * (bs + 1) + 1
        }
    }

    #[test]
    fn fec_seq_identification() {
        let h = Helper {
            fec_enabled: true,
            fec_block_size: 8,
        };
        assert!(h.is_fec_seq(9), "seq 9 must be FEC");
        assert!(h.is_fec_seq(18), "seq 18 must be FEC");
        assert!(h.is_fec_seq(27), "seq 27 must be FEC");
        assert!(!h.is_fec_seq(1), "seq 1 is data");
        assert!(!h.is_fec_seq(8), "seq 8 is data");
        assert!(!h.is_fec_seq(10), "seq 10 is data");
    }

    #[test]
    fn fec_seq_false_when_fec_disabled() {
        let h = Helper {
            fec_enabled: false,
            fec_block_size: 8,
        };
        assert!(!h.is_fec_seq(9), "FEC disabled -> never a FEC seq");
    }

    #[test]
    fn fec_block_start_proof() {
        let h = Helper {
            fec_enabled: true,
            fec_block_size: 8,
        };
        for seq in 1u32..=9 {
            assert_eq!(h.fec_block_start(seq), 1, "seq={seq}");
        }
        for seq in 10u32..=18 {
            assert_eq!(h.fec_block_start(seq), 10, "seq={seq}");
        }
        assert_eq!(h.fec_block_start(19), 19);
        assert_eq!(h.fec_block_start(27), 19);
    }
}

/// Statistics from a completed receive.
#[derive(Debug, Clone)]
pub struct RecvStats {
    pub bytes_received: u64,
    pub duration: Duration,
    pub packets_received: u64,
    pub filename: String,
    pub nack_count: u64,
    pub fec_recoveries: u64,
}

impl RecvStats {
    pub fn throughput_mbps(&self) -> f64 {
        if self.duration.as_secs_f64() == 0.0 {
            return 0.0;
        }
        (self.bytes_received as f64 * 8.0) / (self.duration.as_secs_f64() * 1_000_000.0)
    }
}
