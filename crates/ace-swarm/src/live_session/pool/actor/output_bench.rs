//! Bounded synthetic benchmark of the live output path: piece reassembly, the shared
//! output step that sends to the source channel, and the per-turn position publication.
use super::*;
use std::hint::black_box;

const PACKETS: usize = 349;
const PIECE: usize = 188 * PACKETS;
const PIECES: u64 = 50_000;

/// Mirrors the engine's observer cost: one mutex-guarded position per publication.
#[derive(Default)]
struct Sink(std::sync::Mutex<(Option<LivePosition>, u64)>);
impl LiveObserver for Sink {
    fn received_bytes(&self, count: u64) {
        black_box(count);
    }
    fn publish_position(&self, position: Option<LivePosition>, buffer_ms: u64) {
        *self.0.lock().unwrap() = (position, buffer_ms);
    }
    fn clear_active_coverage(&self) {}
}

#[tokio::test]
#[ignore = "bounded synthetic live output benchmark; run explicitly in release mode"]
async fn live_output_measurement() {
    let info = StreamInfo {
        protection: None,
        infohash: [0; 20],
        piece_length: PIECE as u64,
        chunk_length: PIECE as u64,
        trackers: vec![],
        metadata: Default::default(),
        sig_len: 0,
        source_pubkey: vec![],
    };
    let mut packet = vec![0xff; PIECE];
    for p in packet.as_chunks_mut::<188>().0 {
        p[0..4].copy_from_slice(&[0x47, 0x1f, 0xff, 0x10]);
    }
    for trial in 0..=5 {
        let sink = Arc::new(Sink::default());
        let seed = SeedConfig {
            peer_policy: Default::default(),
            metrics: Some(sink.clone()),
            registry: SeedRegistry::new(),
            store_factory: test_store_factory(4096, None),
            enabled: true,
            prefetch_pieces: 0,
            live_recovery: LiveRecoveryConfig::default(),
            warm_peers: WarmPeerCache::memory(),
        };
        let (mut c, _) = Continuity::fresh(&info, 0, 0, 0, seed.live_recovery);
        c.startup_deadline = None;
        c.observer = Some(sink.clone());
        let mut candidates = SessionCandidates::default();
        let mut peers = BTreeMap::new();
        let downloaded = Arc::new(AtomicU64::new(0));
        let (tx, mut rx) = mpsc::channel(256);
        let mut bytes = 0usize;
        let start = Instant::now();
        for index in 0..PIECES {
            c.head = index;
            c.reasm
                .add_live_chunk(index, 0, PIECE as u64, [0; 8], black_box(&packet))
                .unwrap();
            while c.output_demand() {
                let permit = tx.clone().reserve_owned().await;
                let end = finish_output_step(
                    OutputContext {
                        continuity: &mut c,
                        candidates: &mut candidates,
                        peers: &mut peers,
                        info: &info,
                        seed: &seed,
                        downloaded: &downloaded,
                        tx: &tx,
                    },
                    permit,
                );
                assert!(end.is_none());
                publish_live_stats(&c, seed.metrics.as_ref(), &info);
                while let Ok(output) = rx.try_recv() {
                    bytes += output.bytes.len();
                    black_box(output);
                }
            }
        }
        let seconds = start.elapsed().as_secs_f64();
        assert_eq!(c.reasm.next_needed(), PIECES);
        assert!(bytes >= PIECE * (PIECES as usize - 1));
        if trial != 0 {
            println!(
                "{{\"trial\":{trial},\"pieces\":{PIECES},\"piece_bytes\":{PIECE},\"seconds\":{seconds:.9},\"ns_per_piece\":{:.1},\"mib_per_s\":{:.1}}}",
                seconds * 1e9 / PIECES as f64,
                bytes as f64 / seconds / 1_048_576.0
            );
        }
    }
}
