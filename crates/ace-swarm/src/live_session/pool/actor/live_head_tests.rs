//! Issue #560: one peer's live-head claim must not move playback by itself.
use super::*;

fn runtime(id: u16, max_piece: u64) -> (PeerRuntime, mpsc::Receiver<PeerCommand>) {
    let (commands, rx) = mpsc::channel(64);
    let worker = tokio::spawn(std::future::pending::<()>());
    (
        PeerRuntime {
            addr: SocketAddrV4::new([127, 0, 0, 1].into(), id),
            min_piece: 0,
            max_piece,
            unchoked_peer: false,
            produced_output: false,
            seen_ids: HashSet::new(),
            commands,
            worker,
        },
        rx,
    )
}

struct Outcome {
    cursor: u64,
    trusted_head: u64,
    request_head: u64,
    honest_window_resumes: bool,
    honest_requests: Vec<u64>,
}

/// Every peer is honest at `honest_max` except the last one, which then claims `claim`
/// in a single HAVE. Returns where playback and the honest peer's requests end up.
async fn bogus_have(peer_count: u16, honest_max: u64, claim: u32) -> Outcome {
    let info = StreamInfo {
        protection: None,
        infohash: [0; 20],
        piece_length: 4,
        chunk_length: 2,
        trackers: vec![],
        metadata: Default::default(),
        sig_len: 0,
        source_pubkey: vec![],
    };
    let identity = Identity::generate();
    let policy = LiveRecoveryConfig::default();
    let prefetch = 4;
    let seed = SeedConfig {
        peer_policy: Default::default(),
        metrics: None,
        registry: SeedRegistry::new(),
        store_factory: test_store_factory(4096, None),
        enabled: false,
        prefetch_pieces: prefetch,
        live_recovery: policy,
        warm_peers: WarmPeerCache::memory(),
    };
    let store = Arc::new(tokio::sync::Mutex::new(PieceStore::new(4, 2, 4096)));
    let (tx, _output) = mpsc::channel(1);
    let downloaded = Arc::new(AtomicU64::new(0));
    let uploaded = Arc::new(AtomicU64::new(0));
    let served = Arc::new(AtomicU32::new(0));
    let count = Arc::new(AtomicU32::new(0));
    let (mut continuity, start) = Continuity::fresh(&info, 0, honest_max, prefetch, policy);
    assert_eq!(start, honest_max - prefetch);
    // Output is already flowing; startup corroboration (#170, #521) is over.
    continuity.emitted = 1;
    let mut candidates = SessionCandidates::default();
    let mut discovery = None;
    let (event_tx, event_rx) = mpsc::channel(128);
    let (pex_tx, refill_rx) = mpsc::channel(2);
    let window = WireLivePosition {
        min_piece: 0,
        max_piece: honest_max as i64,
        position: 0,
        distance_from_source: 1,
    };
    let mut peers = BTreeMap::new();
    let mut receivers = BTreeMap::new();
    for id in 1..=peer_count {
        let (peer, commands) = runtime(id, honest_max);
        continuity.register_active_peer(id as u64, peer.addr, window);
        continuity.set_peer_unchoked(id as u64, true);
        peers.insert(id as u64, peer);
        receivers.insert(id as u64, commands);
    }
    let liar = peer_count as u64;
    let liar_addr = peers[&liar].addr;
    let mut actor = PoolActor {
        output_reservation: None,
        activation: None,
        reciprocal: Default::default(),
        publication: None,
        gossip: None,
        closed_peer_events: BTreeMap::new(),
        observed_retry: Instant::now(),
        observed_capacity: (2, 0),
        refill_closed: true,
        next_peer_id: peer_count as u64 + 1,
        loss_count: 0,
        peers,
        learned_pending: HashSet::new(),
        learned_connects: tokio::task::JoinSet::new(),
        pex_tx,
        refill_rx,
        event_rx,
        event_tx,
        _stats_guard: LiveStatsGuard(None),
        live_recovery: policy,
        info: &info,
        identity: &identity,
        chunks_per_piece: 2,
        tx: &tx,
        downloaded: &downloaded,
        uploaded: &uploaded,
        peers_served: &served,
        seed: &seed,
        store: &store,
        candidates: &mut candidates,
        peer_count: &count,
        reachability: None,
        discovery_run: &mut discovery,
        continuity: &mut continuity,
    };

    actor
        .on_message(liar, liar_addr, PeerMessage::Have(claim))
        .await;

    let mut honest_requests = Vec::new();
    while let Ok(command) = receivers.get_mut(&1).unwrap().try_recv() {
        if let PeerCommand::RequestPiece { piece, .. } = command {
            honest_requests.push(piece);
        }
    }
    let outcome = Outcome {
        cursor: actor.continuity.reasm.next_needed(),
        trusted_head: actor.continuity.head,
        request_head: actor.continuity.request_head(),
        honest_window_resumes: actor.continuity.window_can_resume(&window),
        honest_requests,
    };
    shutdown_peer_runtimes(&mut actor.peers).await;
    outcome
}

#[tokio::test]
async fn one_peer_claiming_a_far_head_does_not_move_playback_off_the_honest_peers() {
    let outcome = bogus_have(2, 100, 1_000_000).await;
    assert_eq!(
        outcome.cursor, 96,
        "an uncorroborated head must not skip playback past pieces the honest peer holds"
    );
    assert_eq!(outcome.trusted_head, 100);
    assert!(
        outcome.honest_window_resumes,
        "the honest peer's window must still be able to serve the cursor"
    );
    assert!(
        !outcome.honest_requests.is_empty()
            && outcome
                .honest_requests
                .iter()
                .all(|piece| (96..=100).contains(piece)),
        "the honest peer keeps serving the cursor: {:?}",
        outcome.honest_requests
    );
    // The liar may still be asked for pieces inside its own claimed window.
    assert_eq!(outcome.request_head, 1_000_000);
}

#[tokio::test]
async fn a_lone_peer_moves_the_trusted_head_by_at_most_one_request_horizon() {
    let outcome = bogus_have(1, 100, 1_000_000).await;
    let step = LiveRecoveryConfig::default().max_piece_advance;
    assert_eq!(outcome.trusted_head, 100 + step);
    assert_eq!(outcome.cursor, 100 + step - 4);
    assert_eq!(outcome.request_head, 1_000_000);
}

#[tokio::test]
async fn a_lone_peer_advancing_normally_is_trusted_in_full() {
    let outcome = bogus_have(1, 100, 101).await;
    assert_eq!(outcome.trusted_head, 101);
    assert_eq!(outcome.cursor, 97);
}
