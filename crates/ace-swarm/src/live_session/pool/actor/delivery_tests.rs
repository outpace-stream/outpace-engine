//! The delivery cursor advances only once a piece's output is in the source channel (#477).
use super::*;

const PIECE: usize = 188 * 4;

#[derive(Default)]
struct Positions(std::sync::Mutex<Option<LivePosition>>);
impl LiveObserver for Positions {
    fn received_bytes(&self, _: u64) {}
    fn publish_position(&self, position: Option<LivePosition>, _: u64) {
        *self.0.lock().unwrap() = position;
    }
    fn clear_active_coverage(&self) {}
}
impl Positions {
    fn delivered(&self) -> Option<u64> {
        self.0.lock().unwrap().as_ref().and_then(|p| p.delivered)
    }
}

struct Fixture {
    info: StreamInfo,
    seed: SeedConfig,
    store: Arc<tokio::sync::Mutex<PieceStore>>,
    candidates: SessionCandidates,
    downloaded: Arc<AtomicU64>,
    positions: Arc<Positions>,
    c: Continuity,
}

fn fixture(pieces: u64) -> Fixture {
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
    let positions = Arc::new(Positions::default());
    let seed = SeedConfig {
        peer_policy: Default::default(),
        metrics: Some(positions.clone()),
        registry: SeedRegistry::new(),
        store_factory: test_store_factory(4096, None),
        enabled: true,
        prefetch_pieces: 0,
        live_recovery: LiveRecoveryConfig::default(),
        warm_peers: WarmPeerCache::memory(),
    };
    let store = Arc::new(tokio::sync::Mutex::new(PieceStore::new(
        PIECE as u64,
        PIECE as u64,
        4096,
    )));
    let (mut c, _) = Continuity::fresh(&info, 0, 0, 0, seed.live_recovery);
    c.head = pieces;
    c.startup_deadline = None;
    let mut packet = vec![0xff; PIECE];
    for p in packet.as_chunks_mut::<188>().0 {
        p[0..4].copy_from_slice(&[0x47, 0x1f, 0xff, 0x10]);
    }
    for i in 0..pieces {
        c.reasm.add_live_chunk(i, 0, 1, [0; 8], &packet).unwrap();
    }
    Fixture {
        info,
        seed,
        store,
        candidates: SessionCandidates::default(),
        downloaded: Arc::new(AtomicU64::new(0)),
        positions,
        c,
    }
}

/// One real output step: reserve channel capacity, then run the shared send path.
async fn step(f: &mut Fixture, tx: &mpsc::Sender<LiveOutput>) {
    let permit = tx.clone().reserve_owned().await;
    let mut peers = BTreeMap::new();
    let end = finish_output_step(
        OutputContext {
            continuity: &mut f.c,
            candidates: &mut f.candidates,
            peers: &mut peers,
            info: &f.info,
            seed: &f.seed,
            downloaded: &f.downloaded,
            tx,
        },
        permit,
    );
    assert!(end.is_none());
    publish_live_stats(&f.c, f.seed.metrics.as_ref(), &f.info);
}

#[tokio::test]
async fn delivered_cursor_advances_only_after_a_piece_is_sent() {
    let mut f = fixture(2);
    let (tx, mut rx) = mpsc::channel(4);
    publish_live_stats(&f.c, f.seed.metrics.as_ref(), &f.info);
    assert_eq!(f.positions.delivered(), None);
    step(&mut f, &tx).await;
    assert!(f.c.pending_output.is_empty());
    // The resync keeps at most one trailing packet to confirm the next sync byte.
    assert!(rx.try_recv().unwrap().bytes.len() >= PIECE - 188);
    assert_eq!(f.c.delivered_cursor(), Some(1));
    assert_eq!(f.positions.delivered(), Some(1));
    let published = f.positions.0.lock().unwrap().clone().unwrap();
    assert_eq!(published.pos, Some(1));
    step(&mut f, &tx).await;
    assert_eq!(f.positions.delivered(), Some(2));
}

#[tokio::test]
async fn signature_publication_hold_keeps_the_delivered_cursor() {
    let mut f = fixture(2);
    let (tx, mut rx) = mpsc::channel(4);
    step(&mut f, &tx).await;
    assert!(rx.try_recv().is_ok());
    assert_eq!(f.positions.delivered(), Some(1));
    // Piece 1 is reassembled, but output waits for its cache publication.
    f.c.publication.insert(1, PublicationStatus::Needed);
    step(&mut f, &tx).await;
    assert!(rx.try_recv().is_err(), "held output must not be sent");
    assert_eq!(f.positions.delivered(), Some(1));
    f.c.publication.clear();
    step(&mut f, &tx).await;
    assert!(rx.try_recv().is_ok());
    assert_eq!(f.positions.delivered(), Some(2));
}

#[tokio::test]
async fn pending_segments_keep_the_delivered_cursor() {
    let mut f = fixture(1);
    // A taken piece prepared as two segments; only one fits the consumer.
    assert_eq!(f.c.reasm.take_ready_piece().unwrap().index, 0);
    f.c.output_piece = Some(0);
    for bytes in [b"one".as_slice(), b"two"] {
        f.c.pending_output.push_back(LiveOutput {
            bytes: Bytes::copy_from_slice(bytes),
            discontinuity: false,
        });
        f.c.pending_credit.push_back(vec![]);
    }
    let (tx, mut rx) = mpsc::channel(1);
    {
        let drain = drain_retained(
            &mut f.c,
            &mut f.candidates,
            &f.info,
            &f.seed,
            &f.store,
            &f.downloaded,
            &tx,
        );
        tokio::pin!(drain);
        std::future::poll_fn(|cx| {
            assert!(drain.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
    }
    assert_eq!(f.c.pending_output.len(), 1);
    // Reassembly is past the piece, delivery is not.
    let published = f.positions.0.lock().unwrap().clone().unwrap();
    assert_eq!((published.pos, published.delivered), (Some(1), None));
    assert_eq!(f.c.delivered_cursor(), None);
    assert_eq!(rx.recv().await.unwrap().bytes.as_ref(), b"one");
    assert!(
        drain_retained(
            &mut f.c,
            &mut f.candidates,
            &f.info,
            &f.seed,
            &f.store,
            &f.downloaded,
            &tx,
        )
        .await
    );
    assert_eq!(rx.recv().await.unwrap().bytes.as_ref(), b"two");
    assert_eq!(f.c.delivered_cursor(), Some(1));
}

#[tokio::test]
async fn full_consumer_keeps_the_delivered_cursor() {
    let mut f = fixture(1);
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(LiveOutput {
        bytes: Bytes::from_static(b"barrier"),
        discontinuity: false,
    })
    .unwrap();
    let mut pending = Box::pin(tx.clone().reserve_owned());
    std::future::poll_fn(|cx| {
        assert!(pending.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    publish_live_stats(&f.c, f.seed.metrics.as_ref(), &f.info);
    assert_eq!(f.positions.delivered(), None);
    rx.recv().await.unwrap();
    drop(pending);
    step(&mut f, &tx).await;
    assert_eq!(f.positions.delivered(), Some(1));
}
