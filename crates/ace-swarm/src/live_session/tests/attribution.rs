//! Attribution of pieces failing their source signature to the peer that sent them (#561).
use super::*;

const PIECE: u32 = 7;
const CHUNKS: u16 = 2;

enum Seen {
    /// The peer was asked for every chunk of a piece.
    Request(usize, u32),
    /// The pool closed this peer's connection.
    Closed(usize),
}

struct SignedPiece {
    info: StreamInfo,
    payload: Vec<u8>,
    authentic: Vec<u8>,
    corrupt: Vec<u8>,
}

impl SignedPiece {
    fn new() -> Self {
        let auth = ace_wire::live_auth::LiveSourceAuth::generate();
        let payload: Vec<u8> = (0..4)
            .flat_map(|cc| {
                let mut packet = vec![0x55; 188];
                packet[..4].copy_from_slice(&[0x47, 0x01, 0x00, 0x10 | cc]);
                packet
            })
            .collect();
        let mut authentic = payload.clone();
        authentic.extend(auth.sign(&payload));
        let mut corrupt = authentic.clone();
        corrupt[0] ^= 1;
        let info = StreamInfo {
            protection: None,
            infohash: [0; 20],
            piece_length: authentic.len() as u64,
            chunk_length: authentic.len() as u64 / CHUNKS as u64,
            trackers: vec![],
            metadata: StreamMetadata::default(),
            sig_len: auth.signature_len(),
            source_pubkey: auth.pubkey_der(),
        };
        Self {
            info,
            payload,
            authentic,
            corrupt,
        }
    }

    fn chunk(&self, bytes: &[u8], chunk: u16) -> Vec<u8> {
        let len = self.info.chunk_length as usize;
        let begin = chunk as usize * len;
        bytes[begin..begin + len].to_vec()
    }

    fn authentic_chunk(&self, chunk: u16) -> Vec<u8> {
        self.chunk(&self.authentic, chunk)
    }

    fn corrupt_chunk(&self, chunk: u16) -> Vec<u8> {
        self.chunk(&self.corrupt, chunk)
    }
}

/// Loopback upstreams scripted by the test: each reports full-piece requests and its own
/// disconnect, and sends exactly the chunks the test hands it.
struct Swarm {
    addrs: Vec<SocketAddrV4>,
    send: Vec<mpsc::UnboundedSender<(u16, Vec<u8>)>>,
    seen: mpsc::UnboundedReceiver<Seen>,
    servers: Vec<tokio::task::JoinHandle<()>>,
    rx: mpsc::Receiver<LiveOutput>,
    observer: Arc<TestObserver>,
    pool: tokio::task::JoinHandle<SessionCandidates>,
}

impl Swarm {
    async fn start(piece: &SignedPiece, peers: usize, policy: LiveRecoveryConfig) -> Self {
        let (seen_tx, seen) = mpsc::unbounded_channel();
        let mut addrs = Vec::new();
        let mut send = Vec::new();
        let mut servers = Vec::new();
        let mut upstreams = Vec::new();
        for index in 0..peers {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = match listener.local_addr().unwrap() {
                std::net::SocketAddr::V4(addr) => addr,
                _ => unreachable!(),
            };
            let client = TcpStream::connect(addr).await.unwrap();
            let (server, _) = listener.accept().await.unwrap();
            let (chunk_tx, mut chunk_rx) = mpsc::unbounded_channel::<(u16, Vec<u8>)>();
            let seen_tx = seen_tx.clone();
            servers.push(tokio::spawn(async move {
                let mut session = PeerSession::new(server);
                loop {
                    tokio::select! {
                        msg = session.read_message() => match msg {
                            Ok(PeerMessage::Unknown { id: 6, payload }) => {
                                let piece = u32::from_be_bytes(payload[4..8].try_into().unwrap());
                                let chunk = u16::from_be_bytes(payload[8..10].try_into().unwrap());
                                if chunk == CHUNKS - 1 {
                                    let _ = seen_tx.send(Seen::Request(index, piece));
                                }
                            }
                            Ok(_) => {}
                            Err(_) => {
                                let _ = seen_tx.send(Seen::Closed(index));
                                return;
                            }
                        },
                        Some((chunk, bytes)) = chunk_rx.recv() => {
                            let _ = session
                                .send(&build_piece(0, PIECE, chunk, [0; 8], &bytes))
                                .await;
                        }
                    }
                }
            }));
            addrs.push(addr);
            send.push(chunk_tx);
            upstreams.push(ConnectedUpstream {
                session: PeerSession::new(client),
                addr,
                window: live_pos(PIECE as i64, PIECE as i64),
                unchoked: true,
                yourip: None,
            });
        }
        let observer = TestObserver::new(
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicU32::new(0)),
        );
        let seed = SeedConfig {
            peer_policy: Default::default(),
            metrics: Some(observer.clone()),
            registry: SeedRegistry::new(),
            store_factory: test_store_factory(4096, None),
            enabled: false,
            prefetch_pieces: 0,
            live_recovery: policy,
            warm_peers: WarmPeerCache::memory(),
        };
        let mut candidates = SessionCandidates::default();
        for &addr in &addrs {
            candidates.learn(addr, CandidateKind::Discovered);
        }
        let info = piece.info.clone();
        let (tx, rx) = mpsc::channel(4);
        let pool = tokio::spawn(async move {
            let store = Arc::new(tokio::sync::Mutex::new(PieceStore::new(
                info.piece_length,
                info.chunk_length,
                4096,
            )));
            follow_peer_pool(
                upstreams,
                &info,
                &Identity::generate(),
                CHUNKS,
                &tx,
                &Arc::new(AtomicU64::new(0)),
                &Arc::new(AtomicU64::new(0)),
                &Arc::new(AtomicU32::new(0)),
                &seed,
                &store,
                &mut None,
                vec![],
                vec![],
                completed_discovery(|_| Box::pin(async { vec![] })),
                &mut candidates,
                &Arc::new(AtomicU32::new(0)),
                None,
            )
            .await;
            candidates
        });
        Self {
            addrs,
            send,
            seen,
            servers,
            rx,
            observer,
            pool,
        }
    }

    async fn next(&mut self) -> Seen {
        tokio::time::timeout(Duration::from_secs(5), self.seen.recv())
            .await
            .expect("pool must act on the scripted peers")
            .unwrap()
    }

    async fn next_request(&mut self) -> usize {
        match self.next().await {
            Seen::Request(peer, piece) => {
                assert_eq!(piece, PIECE);
                peer
            }
            Seen::Closed(peer) => panic!("peer {peer} was disconnected unexpectedly"),
        }
    }

    fn send(&self, peer: usize, chunk: u16, bytes: Vec<u8>) {
        self.send[peer].send((chunk, bytes)).unwrap();
    }

    async fn output(&mut self) -> Vec<u8> {
        tokio::time::timeout(Duration::from_secs(5), self.rx.recv())
            .await
            .expect("verified piece must be emitted")
            .unwrap()
            .bytes
            .to_vec()
    }

    fn signature_rejects(&self) -> usize {
        self.observer
            .snapshot()
            .recovery_events
            .iter()
            .filter(|event| **event == RecoveryEvent::SignatureReject)
            .count()
    }

    /// Stop the pool and confirm which peers it disconnected before shutdown.
    async fn finish(mut self) -> (SessionCandidates, Vec<usize>) {
        let mut closed = Vec::new();
        while let Ok(seen) = self.seen.try_recv() {
            match seen {
                Seen::Closed(peer) => closed.push(peer),
                Seen::Request(..) => panic!("unexpected request after the piece completed"),
            }
        }
        drop(self.rx);
        let candidates = self.pool.await.unwrap();
        for server in self.servers {
            server.await.unwrap();
        }
        (candidates, closed)
    }

    /// Assemble piece 7 from two peers: the first assigned peer sends a corrupt chunk 0
    /// and goes silent, then the request-timeout retry on the other peer completes the
    /// piece with chunk 1. Returns the peer the solo refetch was assigned to.
    async fn mixed_source_reject(&mut self, piece: &SignedPiece) -> usize {
        let first = self.next_request().await;
        self.send(first, 0, piece.corrupt_chunk(0));
        let retry = self.next_request().await;
        assert_ne!(retry, first, "the silent peer still holds its request slot");
        self.send(retry, 1, piece.authentic_chunk(1));
        // The refetch request is only issued once the mixed piece was rejected.
        let owner = self.next_request().await;
        assert_eq!(self.signature_rejects(), 1);
        owner
    }
}

fn policy() -> LiveRecoveryConfig {
    LiveRecoveryConfig {
        max_active_upstreams: 2,
        max_piece_advance: 1,
        request_timeout_ms: 200,
        request_check_interval_ms: 10,
        ..default_live_recovery()
    }
}

fn rejected(candidates: &SessionCandidates, addr: SocketAddrV4) -> bool {
    candidates
        .failure_snapshot(addr)
        .is_some_and(|(_, retry_at)| {
            retry_at >= Instant::now() + reconnect::REJECTED_PEER_COOLDOWN / 2
        })
}

#[tokio::test]
async fn single_source_bad_piece_disconnects_and_avoids_that_peer() {
    let piece = SignedPiece::new();
    let mut swarm = Swarm::start(&piece, 2, policy()).await;
    // A first failure, even with every block from one peer, only makes the piece solo.
    let first = swarm.next_request().await;
    swarm.send(first, 0, piece.corrupt_chunk(0));
    swarm.send(first, 1, piece.corrupt_chunk(1));
    let bad = swarm.next_request().await;
    assert_eq!(swarm.signature_rejects(), 1);
    // The solo owner sends every block of the piece and it fails again: proven bad.
    swarm.send(bad, 0, piece.corrupt_chunk(0));
    swarm.send(bad, 1, piece.corrupt_chunk(1));
    let (closed, retry) = (swarm.next().await, swarm.next().await);
    assert!(matches!(closed, Seen::Closed(peer) if peer == bad));
    let good = match retry {
        Seen::Request(peer, PIECE) => peer,
        _ => panic!("the piece must be refetched from the remaining peer"),
    };
    assert_ne!(good, bad);
    swarm.send(good, 0, piece.authentic_chunk(0));
    swarm.send(good, 1, piece.authentic_chunk(1));
    // The TS resynchronizer retains one packet for its next-sync check.
    assert_eq!(swarm.output().await, piece.payload[..564]);
    assert_eq!(swarm.signature_rejects(), 2);
    let addrs = swarm.addrs.clone();
    let (candidates, _) = swarm.finish().await;
    assert!(
        rejected(&candidates, addrs[bad]),
        "bad peer must not be redialled soon"
    );
    assert!(!rejected(&candidates, addrs[good]));
}

#[tokio::test]
async fn mixed_source_bad_piece_penalises_nobody() {
    let piece = SignedPiece::new();
    let mut swarm = Swarm::start(&piece, 2, policy()).await;
    let owner = swarm.mixed_source_reject(&piece).await;
    swarm.send(owner, 0, piece.authentic_chunk(0));
    swarm.send(owner, 1, piece.authentic_chunk(1));
    assert_eq!(swarm.output().await, piece.payload[..564]);
    assert_eq!(swarm.signature_rejects(), 1);
    let addrs = swarm.addrs.clone();
    let (candidates, closed) = swarm.finish().await;
    assert!(closed.is_empty(), "no peer is disconnected: {closed:?}");
    for addr in addrs {
        assert!(!rejected(&candidates, addr));
    }
}

#[tokio::test]
async fn solo_piece_ignores_chunks_from_other_peers() {
    let piece = SignedPiece::new();
    let mut swarm = Swarm::start(&piece, 2, policy()).await;
    let owner = swarm.mixed_source_reject(&piece).await;
    let other = 1 - owner;
    // Unsolicited corrupt bytes for the whole piece from the non-owner. Accepted, they
    // would complete and fail the piece a second time.
    swarm.send(other, 0, piece.corrupt_chunk(0));
    swarm.send(other, 1, piece.corrupt_chunk(1));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(swarm.signature_rejects(), 1);
    swarm.send(owner, 0, piece.authentic_chunk(0));
    swarm.send(owner, 1, piece.authentic_chunk(1));
    assert_eq!(swarm.output().await, piece.payload[..564]);
    assert_eq!(swarm.signature_rejects(), 1);
    let (_, closed) = swarm.finish().await;
    assert!(closed.is_empty(), "no peer is disconnected: {closed:?}");
}

#[tokio::test]
async fn silent_solo_peer_hands_the_piece_over_after_the_timeout() {
    let piece = SignedPiece::new();
    let policy = policy();
    let mut swarm = Swarm::start(&piece, 2, policy).await;
    let silent = swarm.mixed_source_reject(&piece).await;
    let assigned = Instant::now();
    // A partial from the silent owner must not be mixed into the next peer's fetch.
    swarm.send(silent, 0, piece.corrupt_chunk(0));
    let next = swarm.next_request().await;
    let waited = assigned.elapsed();
    assert_ne!(
        next, silent,
        "the held slot steers the handover to another peer"
    );
    assert!(
        waited >= policy.request_timeout() * 3 / 2,
        "handover follows the solo silence timeout, not the request timeout: {waited:?}"
    );
    swarm.send(next, 1, piece.authentic_chunk(1));
    swarm.send(next, 0, piece.authentic_chunk(0));
    assert_eq!(swarm.output().await, piece.payload[..564]);
    assert_eq!(swarm.signature_rejects(), 1);
    let (_, closed) = swarm.finish().await;
    assert!(
        closed.is_empty(),
        "silence alone is not punished: {closed:?}"
    );
}
