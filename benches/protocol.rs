//! Criterion micro-benchmarks for chirp protocol components.
//!
//! Run: cargo bench
//! HTML report: target/criterion/report/index.html

use chirp::congestion::delay_based::DelayBasedController;
use chirp::protocol::{
    fec::{FecDecoder, FecEncoder, FEC_BLOCK_SIZE},
    nack::{InstantMs, NackTracker},
    packet::{Packet, MAX_PAYLOAD},
};
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

fn ms(t: u64) -> InstantMs {
    InstantMs::from_ticks(t)
}

// ─── Packet encode/decode ────────────────────────────────────────────────────

fn bench_packet_encode(c: &mut Criterion) {
    let payload = vec![0xABu8; MAX_PAYLOAD];
    c.bench_function("packet/encode_1200B", |b| {
        b.iter(|| {
            let pkt = Packet::data(black_box(42), black_box(payload.clone()));
            black_box(pkt.encode())
        })
    });
}

fn bench_packet_decode(c: &mut Criterion) {
    let wire = Packet::data(42, vec![0xABu8; MAX_PAYLOAD]).encode();
    c.bench_function("packet/decode_1200B", |b| {
        b.iter(|| black_box(Packet::decode(black_box(&wire)).unwrap()))
    });
}

// ─── NackTracker (hashbrown HashSet) ─────────────────────────────────────────

fn bench_nack_record(c: &mut Criterion) {
    let mut group = c.benchmark_group("nack");
    for &n in &[100u32, 1_000, 10_000, 100_000] {
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::new("record", n), &n, |b, &n| {
            b.iter(|| {
                let mut t = NackTracker::new(0);
                for s in 1..=n {
                    t.record(black_box(s));
                }
                t
            })
        });
    }
    group.finish();
}

fn bench_nack_missing(c: &mut Criterion) {
    // Pre-fill tracker with every other seq missing (worst-case scan)
    let mut group = c.benchmark_group("nack");
    for &n in &[1_000u32, 10_000, 100_000] {
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::new("missing_50pct_gaps", n), &n, |b, &n| {
            let mut t = NackTracker::new(0);
            for s in (1..=n).step_by(2) {
                t.record(s);
            } // every other seq received
            b.iter(|| black_box(t.missing(1)))
        });
    }
    group.finish();
}

fn bench_nack_contains(c: &mut Criterion) {
    let mut t = NackTracker::new(0);
    for s in 1u32..=100_000 {
        t.record(s);
    }
    c.bench_function("nack/contains_O1", |b| {
        b.iter(|| black_box(t.has(black_box(99_999))))
    });
}

// ─── FEC encode/decode ───────────────────────────────────────────────────────

fn bench_fec_encode(c: &mut Criterion) {
    let block: Vec<Vec<u8>> = (0..FEC_BLOCK_SIZE)
        .map(|i| vec![i as u8; MAX_PAYLOAD])
        .collect();
    c.bench_function("fec/encode_block8x1200B", |b| {
        b.iter(|| {
            let mut enc = FecEncoder::new(FEC_BLOCK_SIZE);
            let mut parity = None;
            for p in &block {
                parity = enc.add_payload(black_box(p));
            }
            black_box(parity)
        })
    });
}

fn bench_fec_recover(c: &mut Criterion) {
    let payloads: Vec<Vec<u8>> = (0..FEC_BLOCK_SIZE)
        .map(|i| vec![i as u8; MAX_PAYLOAD])
        .collect();
    let mut enc = FecEncoder::new(FEC_BLOCK_SIZE);
    let mut parity = None;
    for p in &payloads {
        parity = enc.add_payload(p);
    }
    let parity = parity.unwrap();
    let dec = FecDecoder::new(FEC_BLOCK_SIZE);

    c.bench_function("fec/recover_1_loss_8x1200B", |b| {
        b.iter(|| {
            let recv: Vec<&[u8]> = payloads[..FEC_BLOCK_SIZE - 1]
                .iter()
                .map(|v| v.as_slice())
                .collect();
            black_box(dec.recover(black_box(&recv), black_box(&parity)))
        })
    });
}

// ─── Congestion controller ───────────────────────────────────────────────────

fn bench_cc_on_delay(c: &mut Criterion) {
    let mut cc = DelayBasedController::new(1_250_000.0, 125_000_000.0);
    c.bench_function("cc/on_delay_sample", |b| {
        let mut tick = 0u64;
        b.iter(|| {
            cc.on_delay_sample(black_box(1000 + tick % 200), ms(tick));
            tick += 10;
        })
    });
}

fn bench_cc_inter_packet_delay(c: &mut Criterion) {
    let cc = DelayBasedController::new(1_250_000.0, 125_000_000.0);
    c.bench_function("cc/inter_packet_delay", |b| {
        b.iter(|| black_box(cc.inter_packet_delay_us(black_box(1212))))
    });
}

// ─── Throughput: encode + decode pipeline ────────────────────────────────────

fn bench_pipeline(c: &mut Criterion) {
    let mut group = c.benchmark_group("pipeline");
    for &payload_kb in &[1u64, 10, 100, 1024] {
        let data = vec![0xAAu8; payload_kb as usize * 1024];
        group.throughput(Throughput::Bytes(payload_kb * 1024));
        group.bench_with_input(
            BenchmarkId::new("encode_decode_MB_s", payload_kb),
            &data,
            |b, data| {
                b.iter(|| {
                    let mut decoded_bytes = 0usize;
                    for chunk in data.chunks(MAX_PAYLOAD) {
                        let wire = Packet::data(1, chunk.to_vec()).encode();
                        let pkt = Packet::decode(black_box(&wire)).unwrap();
                        decoded_bytes += pkt.payload.len();
                    }
                    black_box(decoded_bytes)
                })
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_packet_encode,
    bench_packet_decode,
    bench_nack_record,
    bench_nack_missing,
    bench_nack_contains,
    bench_fec_encode,
    bench_fec_recover,
    bench_cc_on_delay,
    bench_cc_inter_packet_delay,
    bench_pipeline,
);
criterion_main!(benches);
