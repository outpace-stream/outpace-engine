//! Mainline DHT (BEP-5) `get_peers` — discover swarm peers for an infohash without a
//! tracker. Acestream populates the swarm on the public DHT, so an iterative `get_peers`
//! lookup toward the infohash returns the same peers the official engine connects to.
//!
//! This is a focused, time-bounded *lookup client* (not a full routing-table node): it
//! bootstraps off the well-known routers, walks closer to the target collecting `nodes`,
//! and harvests any `values` (peers) it's handed along the way.

use crate::resolver::Resolver;
use ace_wire::bencode::Bencode;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

/// Well-known DHT bootstrap routers.
const BOOTSTRAP: &[&str] = &[
    "router.bittorrent.com:6881",
    "router.utorrent.com:6881",
    "dht.transmissionbt.com:6881",
    "dht.libtorrent.org:25401",
];
/// Enough peers to start racing TCP connects. Waiting for a much larger DHT harvest delays
/// first byte even though the engine only needs one good upstream.
const DHT_PEER_TARGET: usize = 8;

/// How long an inflight `(source, txid)` query stays eligible to be answered. Long enough to
/// admit a genuine reply that lands after its slot was handed to another query, short enough that a stale entry can't
/// be matched by an unrelated packet arriving much later in the walk.
const INFLIGHT_TTL: Duration = Duration::from_secs(3);

/// How many `get_peers` queries the walk keeps outstanding at once.
const MAX_ACTIVE: usize = 8;

/// How long an unanswered query holds one of the [`MAX_ACTIVE`] slots before the walk sends
/// a replacement. Shorter than [`INFLIGHT_TTL`]: public DHT nodes are often silent, so a slot
/// waiting out the full TTL would halve the query rate on a sparse frontier. The query stays
/// correlatable until the TTL, so a late reply is still accepted.
const SLOT_TIMEOUT: Duration = Duration::from_millis(1500);

/// How many IPv4 addresses the walk seeds from one bootstrap hostname. Some routers sit
/// behind several addresses (round-robin DNS), and a walk that only ever asks the first
/// one loses that router whenever that single address drops its query.
const MAX_BOOTSTRAP_ADDRS_PER_HOST: usize = 2;

/// How many recently answering nodes the process remembers for the dead-end recovery round.
const NODE_CACHE_CAP: usize = 32;

/// How long a remembered node stays eligible. Public DHT nodes churn within hours; pymdht
/// refreshes its routing table every 15 minutes, so a node older than that is not trusted.
const NODE_CACHE_MAX_AGE: Duration = Duration::from_secs(15 * 60);

/// How many answering nodes one walk adds to the cache, and the most the recovery round
/// takes from it, so a single walk cannot crowd out what earlier walks learned.
const NODE_CACHE_PER_WALK: usize = 8;

/// How many recovery slots stay reserved for remembered nodes when the bootstrap routers
/// alone would fill all [`MAX_ACTIVE`] of them.
const RECOVERY_CACHED_MIN: usize = 4;

/// The client version tag the official engine's DHT (pymdht `NEXTSHARE_VERSION`) puts in
/// every KRPC message's `v` key.
pub const CLIENT_VERSION: &[u8] = b"NS\x01\x18";

/// Our DHT node id: one random id for the whole process, like the engine's, so the
/// tokens a `get_peers` walk collects are valid for the announces that follow it.
pub fn session_node_id() -> [u8; 20] {
    static ID: std::sync::OnceLock<[u8; 20]> = std::sync::OnceLock::new();
    *ID.get_or_init(rand::random)
}

/// Build a `get_peers` KRPC query for `infohash` from our `node_id`.
pub fn build_get_peers(node_id: &[u8; 20], infohash: &[u8; 20], txid: &[u8]) -> Vec<u8> {
    let mut a: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
    a.insert(b"id".to_vec(), Bencode::Bytes(node_id.to_vec()));
    a.insert(b"info_hash".to_vec(), Bencode::Bytes(infohash.to_vec()));
    let mut d: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
    d.insert(b"a".to_vec(), Bencode::Dict(a));
    d.insert(b"q".to_vec(), Bencode::Bytes(b"get_peers".to_vec()));
    d.insert(b"t".to_vec(), Bencode::Bytes(txid.to_vec()));
    d.insert(b"v".to_vec(), Bencode::Bytes(CLIENT_VERSION.to_vec()));
    d.insert(b"y".to_vec(), Bencode::Bytes(b"q".to_vec()));
    Bencode::Dict(d).encode()
}

/// Build a `find_node` KRPC query for `target` from our `node_id`. The walk only sends it
/// to a bootstrap router that already answered, during the dead-end recovery round: a
/// random target makes the router return nodes from another part of its table, which
/// gives a frontier whose first referrals were all silent somewhere new to start from.
pub fn build_find_node(node_id: &[u8; 20], target: &[u8; 20], txid: &[u8]) -> Vec<u8> {
    let mut a: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
    a.insert(b"id".to_vec(), Bencode::Bytes(node_id.to_vec()));
    a.insert(b"target".to_vec(), Bencode::Bytes(target.to_vec()));
    let mut d: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
    d.insert(b"a".to_vec(), Bencode::Dict(a));
    d.insert(b"q".to_vec(), Bencode::Bytes(b"find_node".to_vec()));
    d.insert(b"t".to_vec(), Bencode::Bytes(txid.to_vec()));
    d.insert(b"v".to_vec(), Bencode::Bytes(CLIENT_VERSION.to_vec()));
    d.insert(b"y".to_vec(), Bencode::Bytes(b"q".to_vec()));
    Bencode::Dict(d).encode()
}

/// Build an `announce_peer` KRPC query (BEP-5) — the DHT half of "make ourselves
/// discoverable as a peer for `infohash`". Must carry the opaque `token` a node handed us in
/// its own `get_peers` response (anti-spoofing: a node only accepts an announce echoing a
/// token it issued). An explicit `port` advertises our real listening port; like the engine
/// we send no `implied_port`, which BEP-5 treats as 0.
pub fn build_announce_peer(
    node_id: &[u8; 20],
    infohash: &[u8; 20],
    port: u16,
    token: &[u8],
    txid: &[u8],
) -> Vec<u8> {
    let mut a: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
    a.insert(b"id".to_vec(), Bencode::Bytes(node_id.to_vec()));
    a.insert(b"info_hash".to_vec(), Bencode::Bytes(infohash.to_vec()));
    a.insert(b"port".to_vec(), Bencode::Int(port as i64));
    a.insert(b"token".to_vec(), Bencode::Bytes(token.to_vec()));
    let mut d: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
    d.insert(b"a".to_vec(), Bencode::Dict(a));
    d.insert(b"q".to_vec(), Bencode::Bytes(b"announce_peer".to_vec()));
    d.insert(b"t".to_vec(), Bencode::Bytes(txid.to_vec()));
    d.insert(b"v".to_vec(), Bencode::Bytes(CLIENT_VERSION.to_vec()));
    d.insert(b"y".to_vec(), Bencode::Bytes(b"q".to_vec()));
    Bencode::Dict(d).encode()
}

/// Parsed `get_peers` response: any peers handed to us, closer nodes to continue from, and
/// the opaque `token` (if present) needed to `announce_peer` back to this same node.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GetPeersResponse {
    pub peers: Vec<SocketAddrV4>,
    pub nodes: Vec<([u8; 20], SocketAddrV4)>,
    pub token: Option<Vec<u8>>,
    /// The KRPC transaction id (`t`) this reply echoes, used to correlate it against the
    /// outbound query it answers. Empty if the response omitted `t`.
    pub txid: Vec<u8>,
}

/// Parse a KRPC response, extracting `r.values` (peers), `r.nodes` (compact nodes), and
/// `r.token` (needed to `announce_peer` back to whichever node sent this response).
pub fn parse_response(buf: &[u8]) -> Option<GetPeersResponse> {
    let v = Bencode::parse(buf).ok()?;
    // A KRPC *response* is the message type `y == "r"` (BEP-5). On untrusted public UDP a
    // hostile packet can smuggle an `r` dict under a query (`y == "q"`) or error
    // (`y == "e"`) type, or omit `y` entirely; gate on the type before mining `r` for
    // peers/nodes/tokens so those never come from a non-response.
    if v.get(b"y").and_then(Bencode::as_bytes) != Some(b"r".as_slice()) {
        return None;
    }
    // `r` must be a dict; a response whose `r` is any other shape is malformed, not empty.
    let r = v.get(b"r")?;
    if !matches!(r, Bencode::Dict(_)) {
        return None;
    }
    let mut out = GetPeersResponse::default();
    if let Some(Bencode::List(vals)) = r.get(b"values") {
        for e in vals {
            if let Some(b) = e.as_bytes() {
                if let Some(p) = compact_peer(b) {
                    out.peers.push(p);
                }
            }
        }
    }
    if let Some(nb) = r.get(b"nodes").and_then(Bencode::as_bytes) {
        for c in nb.as_chunks::<26>().0 {
            let mut id = [0u8; 20];
            id.copy_from_slice(&c[0..20]);
            if let Some(addr) = compact_peer(&c[20..26]) {
                out.nodes.push((id, addr));
            }
        }
    }
    out.token = r
        .get(b"token")
        .and_then(Bencode::as_bytes)
        .map(|b| b.to_vec());
    // Transaction id lives at the top level (`t`), alongside `r`/`y` — capture it so the walk
    // can correlate this reply against the query it answers.
    out.txid = v
        .get(b"t")
        .and_then(Bencode::as_bytes)
        .map(|b| b.to_vec())
        .unwrap_or_default();
    Some(out)
}

/// The transaction id of a rejected datagram that is a well-formed KRPC error (`y == "e"`),
/// or `None` if it is unparseable or the wrong shape. Separates a node that answered with an
/// error from genuinely malformed/hostile traffic, and lets a correlated error close its
/// query. An error without a usable `t` yields an empty id, which never matches a query.
/// BEP-5 requires `e` to be `[code, message]`; an error type without that payload is
/// malformed and must not close a query.
fn krpc_error_txid(buf: &[u8]) -> Option<Vec<u8>> {
    let v = Bencode::parse(buf).ok()?;
    if v.get(b"y").and_then(Bencode::as_bytes) != Some(b"e".as_slice()) {
        return None;
    }
    match v.get(b"e") {
        Some(Bencode::List(e)) if matches!(e.as_slice(), [Bencode::Int(_), Bencode::Bytes(_)]) => {}
        _ => return None,
    }
    Some(
        v.get(b"t")
            .and_then(Bencode::as_bytes)
            .map(<[u8]>::to_vec)
            .unwrap_or_default(),
    )
}

/// Per-walk diagnostics for a single iterative lookup. A raw peer count hides the failure
/// modes that matter when comparing the custom DHT against a candidate crate (#43): silence,
/// malformed traffic, KRPC errors, uncorrelated packets, and frontier exhaustion. Logged once
/// at the end of a walk and returned so tests can assert on it; not part of the public API.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct DhtWalkMetrics {
    /// Bootstrap/seed nodes placed in the initial frontier.
    bootstrap_seeded: usize,
    /// `get_peers` queries sent (one per not-yet-queried node).
    nodes_queried: usize,
    /// Datagrams accepted as correlated `get_peers` responses.
    valid_responses: usize,
    /// Peer records harvested from accepted responses (`r.values`).
    peers_discovered: usize,
    /// Compact node records harvested from accepted responses (`r.nodes`).
    nodes_discovered: usize,
    /// Datagrams that failed to parse as a well-formed `get_peers` response and were not
    /// recognizable KRPC errors.
    malformed_responses: usize,
    /// Datagrams that were well-formed KRPC error messages (`y == "e"`).
    krpc_errors: usize,
    /// Datagrams that parsed as a response but matched no inflight `(source, txid)` query —
    /// late, replayed, or spoofed traffic rejected by correlation (#40).
    uncorrelated_responses: usize,
    /// Queries whose slot timeout elapsed without any datagram at all from that node.
    timeouts: usize,
    /// Queries sent by the one dead-end recovery round (#494), which runs only when the
    /// frontier runs dry before any peer was found: a fresh `get_peers` to each bootstrap
    /// router that never answered, and a random-target `find_node` to each that did.
    recovery_queries: usize,
    /// Peer records harvested after the recovery round started.
    recovery_peers: usize,
    /// The walk ran out of new frontier nodes before exhausting its time budget.
    frontier_exhausted: bool,
}

/// 6-byte compact endpoint: 4-byte IPv4 + 2-byte big-endian port.
fn compact_peer(b: &[u8]) -> Option<SocketAddrV4> {
    if b.len() != 6 {
        return None;
    }
    let port = u16::from_be_bytes([b[4], b[5]]);
    if port == 0 {
        return None;
    }
    Some(SocketAddrV4::new(
        Ipv4Addr::new(b[0], b[1], b[2], b[3]),
        port,
    ))
}

/// XOR distance between a node id and the target (BTreeSet-orderable key).
fn distance(id: &[u8; 20], target: &[u8; 20]) -> [u8; 20] {
    let mut d = [0u8; 20];
    for i in 0..20 {
        d[i] = id[i] ^ target[i];
    }
    d
}

/// Iterative `get_peers` walk toward `infohash`, shared by `dht_get_peers` (harvest peers)
/// and `dht_announce_peer` (harvest tokens to announce back with). Seeds the frontier from
/// bootstrap routers, keeps queries outstanding to the closest not-yet-queried nodes, and calls
/// `on_response` for each reply — which returns `true` to stop the walk early (e.g. "enough
/// peers" or "enough tokens"). Bounded by `budget` regardless.
async fn dht_walk(
    infohash: &[u8; 20],
    budget: Duration,
    sock: &UdpSocket,
    on_response: impl FnMut(SocketAddrV4, &GetPeersResponse) -> bool,
) -> DhtWalkMetrics {
    dht_walk_with_bootstraps(
        infohash,
        budget,
        sock,
        BOOTSTRAP,
        Resolver::global(),
        Some(NodeCache::global()),
        on_response,
    )
    .await
}

async fn dht_walk_with_bootstraps(
    infohash: &[u8; 20],
    budget: Duration,
    sock: &UdpSocket,
    bootstraps: &[&str],
    resolver: Resolver,
    cache: Option<&NodeCache>,
    on_response: impl FnMut(SocketAddrV4, &GetPeersResponse) -> bool,
) -> DhtWalkMetrics {
    let deadline = tokio::time::Instant::now() + budget;
    let mut lookups = tokio::task::JoinSet::new();
    for (i, host) in bootstraps.iter().take(BOOTSTRAP.len()).enumerate() {
        let resolver = resolver.clone();
        let host = host.to_string();
        lookups.spawn(async move { (i, resolve_bootstrap(host, resolver, deadline).await) });
    }
    dht_walk_owned(
        infohash,
        deadline.into_std(),
        sock,
        BTreeMap::new(),
        Vec::new(),
        lookups,
        cache,
        on_response,
    )
    .await
}

/// Resolve one bootstrap hostname to at most [`MAX_BOOTSTRAP_ADDRS_PER_HOST`] distinct IPv4
/// addresses, in resolver order. Empty if the lookup fails or misses `deadline`.
pub(crate) async fn resolve_bootstrap(
    host: String,
    resolver: Resolver,
    deadline: tokio::time::Instant,
) -> Vec<SocketAddrV4> {
    let Ok(Ok(addresses)) = tokio::time::timeout_at(deadline, resolver.lookup(&host)).await else {
        return Vec::new();
    };
    let mut out: Vec<SocketAddrV4> = Vec::with_capacity(MAX_BOOTSTRAP_ADDRS_PER_HOST);
    for addr in addresses {
        if let SocketAddr::V4(v4) = addr {
            if !out.contains(&v4) {
                out.push(v4);
                if out.len() == MAX_BOOTSTRAP_ADDRS_PER_HOST {
                    break;
                }
            }
        }
    }
    out
}

#[cfg(test)]
async fn dht_walk_from_seeds(
    infohash: &[u8; 20],
    budget: Duration,
    sock: &UdpSocket,
    seeds: Vec<SocketAddrV4>,
    on_response: impl FnMut(SocketAddrV4, &GetPeersResponse) -> bool,
) -> DhtWalkMetrics {
    let mut frontier: BTreeMap<[u8; 20], SocketAddrV4> = BTreeMap::new();
    for (i, seed) in seeds.into_iter().enumerate() {
        let mut key = [0xffu8; 20];
        key[19] = i as u8;
        frontier.insert(key, seed);
    }
    dht_walk_frontier(infohash, budget, sock, frontier, on_response).await
}

/// Like [`dht_walk_from_seeds`], but the seeds stand in for resolved bootstrap routers, so
/// they are eligible for the dead-end recovery round.
#[cfg(test)]
async fn dht_walk_from_bootstrap_seeds(
    infohash: &[u8; 20],
    budget: Duration,
    sock: &UdpSocket,
    seeds: Vec<SocketAddrV4>,
    cache: Option<&NodeCache>,
    on_response: impl FnMut(SocketAddrV4, &GetPeersResponse) -> bool,
) -> DhtWalkMetrics {
    let mut frontier: BTreeMap<[u8; 20], SocketAddrV4> = BTreeMap::new();
    let mut bootstraps = Vec::new();
    for (i, seed) in seeds.into_iter().enumerate() {
        let mut key = [0xffu8; 20];
        key[19] = i as u8;
        frontier.insert(key, seed);
        bootstraps.push(BootstrapNode {
            addr: seed,
            answered: false,
        });
    }
    dht_walk_owned(
        infohash,
        Instant::now() + budget,
        sock,
        frontier,
        bootstraps,
        tokio::task::JoinSet::new(),
        cache,
        on_response,
    )
    .await
}

#[cfg(test)]
async fn dht_walk_frontier(
    infohash: &[u8; 20],
    budget: Duration,
    sock: &UdpSocket,
    frontier: BTreeMap<[u8; 20], SocketAddrV4>,
    on_response: impl FnMut(SocketAddrV4, &GetPeersResponse) -> bool,
) -> DhtWalkMetrics {
    dht_walk_owned(
        infohash,
        Instant::now() + budget,
        sock,
        frontier,
        Vec::new(),
        tokio::task::JoinSet::new(),
        None,
        on_response,
    )
    .await
}

/// A resolved bootstrap router address. Kept apart from the frontier, whose 64-entry cap
/// evicts the far-away bootstrap keys first, so the recovery round can still reach it.
struct BootstrapNode {
    addr: SocketAddrV4,
    /// Whether a correlated, well-formed response came back from this address.
    answered: bool,
}

type BootstrapLookups = tokio::task::JoinSet<(usize, Vec<SocketAddrV4>)>;

fn add_bootstrap(
    result: Result<(usize, Vec<SocketAddrV4>), tokio::task::JoinError>,
    frontier: &mut BTreeMap<[u8; 20], SocketAddrV4>,
    bootstraps: &mut Vec<BootstrapNode>,
    metrics: &mut DhtWalkMetrics,
) {
    let Ok((i, addrs)) = result else {
        return;
    };
    for (j, addr) in addrs.into_iter().enumerate() {
        if bootstraps.iter().any(|node| node.addr == addr) {
            continue;
        }
        // One key per (host, address), still in the farthest-possible key range.
        let mut key = [0xff; 20];
        key[18] = i as u8;
        key[19] = j as u8;
        frontier.insert(key, addr);
        bootstraps.push(BootstrapNode {
            addr,
            answered: false,
        });
        metrics.bootstrap_seeded += 1;
    }
    while frontier.len() > 64 {
        let key = *frontier.keys().next_back().unwrap();
        frontier.remove(&key);
    }
}
/// One outstanding query. A node has at most one live query at a time, so the destination
/// alone identifies it; the transaction id is checked on every reply. Only the recovery
/// round asks a node twice, and its fresh query replaces the released first one.
struct Inflight {
    txid: [u8; 2],
    /// When the query stops holding one of the [`MAX_ACTIVE`] slots. A reply after this is
    /// still accepted until `expires`; it just no longer blocks a replacement query.
    release: Instant,
    /// When the query is abandoned and its `(source, txid)` can no longer be matched.
    expires: Instant,
    /// Whether the query still holds a slot (no correlated reply, `release` not reached).
    active: bool,
    /// Whether any datagram at all arrived from this node.
    heard: bool,
}

/// Nodes that recently gave a correlated, well-formed reply (#522), newest first. Only the
/// dead-end recovery round reads it, next to the bootstrap routers, so a walk that never
/// dead-ends is not slowed by stale entries (the startup cost that ruled out #42). Bounded
/// at [`NODE_CACHE_CAP`] entries and kept in memory only.
pub(crate) struct NodeCache {
    nodes: std::sync::Mutex<VecDeque<(SocketAddrV4, Instant)>>,
}

impl NodeCache {
    pub(crate) const fn new() -> Self {
        Self {
            nodes: std::sync::Mutex::new(VecDeque::new()),
        }
    }

    /// The process-wide cache the real walks share.
    fn global() -> &'static Self {
        static CACHE: NodeCache = NodeCache::new();
        &CACHE
    }

    /// Remember `addrs` as answering at `now`, the first one newest.
    fn record(&self, addrs: &[SocketAddrV4], now: Instant) {
        let mut nodes = self.nodes.lock().unwrap_or_else(|e| e.into_inner());
        for addr in addrs.iter().rev() {
            nodes.retain(|(known, _)| known != addr);
            nodes.push_front((*addr, now));
        }
        nodes.truncate(NODE_CACHE_CAP);
    }

    /// Up to `limit` remembered nodes still younger than [`NODE_CACHE_MAX_AGE`] at `now`,
    /// newest first, skipping any `skip` rejects. Expired entries are dropped.
    fn fresh(
        &self,
        now: Instant,
        limit: usize,
        skip: impl Fn(&SocketAddrV4) -> bool,
    ) -> Vec<SocketAddrV4> {
        let mut nodes = self.nodes.lock().unwrap_or_else(|e| e.into_inner());
        nodes.retain(|(_, seen)| now.saturating_duration_since(*seen) < NODE_CACHE_MAX_AGE);
        nodes
            .iter()
            .map(|(addr, _)| *addr)
            .filter(|addr| !skip(addr))
            .take(limit)
            .collect()
    }
}

/// The answering nodes one walk hands to the [`NodeCache`]: those that returned peers first,
/// as they sit in a populated part of the DHT, then the rest. Bootstrap routers are left out,
/// since the recovery round asks them anyway.
#[derive(Default)]
struct Answered {
    with_peers: Vec<SocketAddrV4>,
    without_peers: Vec<SocketAddrV4>,
}

impl Answered {
    fn note(&mut self, addr: SocketAddrV4, had_peers: bool) {
        let list = if had_peers {
            &mut self.with_peers
        } else {
            &mut self.without_peers
        };
        if list.len() < NODE_CACHE_PER_WALK && !list.contains(&addr) {
            list.push(addr);
        }
    }

    fn into_newest_first(self) -> Vec<SocketAddrV4> {
        let mut out = self.with_peers;
        for addr in self.without_peers {
            if out.len() == NODE_CACHE_PER_WALK {
                break;
            }
            if !out.contains(&addr) {
                out.push(addr);
            }
        }
        out
    }
}

/// Which addresses the recovery round asks, at most [`MAX_ACTIVE`]: remembered nodes get
/// whatever the routers leave free, and at least [`RECOVERY_CACHED_MIN`] slots when they
/// would not. Routers that answered come before silent ones, since they are known to be up.
fn recovery_targets(
    bootstraps: &[BootstrapNode],
    cached: Vec<SocketAddrV4>,
) -> Vec<(SocketAddrV4, RecoveryQuery)> {
    let cached_take = cached.len().min(
        MAX_ACTIVE
            .saturating_sub(bootstraps.len())
            .max(RECOVERY_CACHED_MIN),
    );
    let router_take = MAX_ACTIVE - cached_take;
    let answered = bootstraps.iter().filter(|node| node.answered);
    let silent = bootstraps.iter().filter(|node| !node.answered);
    let mut out: Vec<(SocketAddrV4, RecoveryQuery)> = answered
        .chain(silent)
        .take(router_take)
        .map(|node| {
            let query = if node.answered {
                RecoveryQuery::FindNode
            } else {
                RecoveryQuery::GetPeers
            };
            (node.addr, query)
        })
        .collect();
    for addr in cached.into_iter().take(cached_take) {
        if !out.iter().any(|(known, _)| *known == addr) {
            out.push((addr, RecoveryQuery::GetPeers));
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryQuery {
    GetPeers,
    FindNode,
}

#[allow(clippy::too_many_arguments)]
async fn dht_walk_owned(
    infohash: &[u8; 20],
    deadline: Instant,
    sock: &UdpSocket,
    mut frontier: BTreeMap<[u8; 20], SocketAddrV4>,
    mut bootstraps: Vec<BootstrapNode>,
    mut lookups: BootstrapLookups,
    cache: Option<&NodeCache>,
    mut on_response: impl FnMut(SocketAddrV4, &GetPeersResponse) -> bool,
) -> DhtWalkMetrics {
    let node_id = session_node_id();
    let mut answered = Answered::default();
    let mut metrics = DhtWalkMetrics {
        bootstrap_seeded: frontier.len(),
        ..Default::default()
    };
    // Peers harvested when the recovery round started; `None` until it runs. One round per
    // walk bounds its extra traffic to one datagram per bootstrap address.
    let mut recovery_started_at: Option<usize> = None;

    let mut queried: HashSet<SocketAddrV4> = HashSet::new();
    // Public DHT UDP is untrusted, so a reply is only trusted if it arrives from the exact node
    // we asked and echoes the exact transaction id we generated for that query.
    let mut inflight: HashMap<SocketAddrV4, Inflight> = HashMap::new();
    // Monotonic per-walk counter → a distinct 2-byte transaction id for every outbound query.
    let mut next_txid: u16 = 0;
    let mut buf = [0u8; 2048];
    let tokio_deadline = tokio::time::Instant::from_std(deadline);
    crate::alog!("[dht] seeded {} bootstrap node(s)", frontier.len());

    // Paced by responses (#384): keep up to MAX_ACTIVE queries outstanding, sending the next
    // closest node as soon as a correlated reply or a slot timeout frees a slot, instead of
    // waiting out a fixed window per batch. A `fallback` proxy switch mid-walk stops it:
    // nothing more goes out directly.
    'walk: while Instant::now() < deadline && crate::proxy::active().is_none() {
        while let Some(result) = lookups.try_join_next() {
            add_bootstrap(result, &mut frontier, &mut bootstraps, &mut metrics);
        }
        let now = Instant::now();
        for query in inflight.values_mut() {
            if query.active && query.release <= now {
                query.active = false;
                if !query.heard {
                    metrics.timeouts += 1;
                }
            }
        }
        // Drop expired entries so a stale (source, txid) can't be matched by a much later
        // packet: an unanswered query is abandoned, not left open for the whole walk.
        inflight.retain(|_, query| query.expires > now);

        let mut active = inflight.values().filter(|query| query.active).count();
        while active < MAX_ACTIVE {
            let Some(addr) = frontier
                .values()
                .find(|addr| !queried.contains(*addr))
                .copied()
            else {
                break;
            };
            queried.insert(addr);
            let txid = next_txid.to_be_bytes();
            next_txid = next_txid.wrapping_add(1);
            let query = build_get_peers(&node_id, infohash, &txid);
            let sent =
                tokio::time::timeout_at(tokio_deadline, sock.send_to(&query, SocketAddr::V4(addr)))
                    .await
                    .is_ok_and(|sent| sent.is_ok());
            if sent {
                metrics.nodes_queried += 1;
                let sent_at = Instant::now();
                inflight.insert(
                    addr,
                    Inflight {
                        txid,
                        release: sent_at + SLOT_TIMEOUT,
                        expires: sent_at + INFLIGHT_TTL,
                        active: true,
                        heard: false,
                    },
                );
                active += 1;
            }
        }

        if active == 0
            && lookups.is_empty()
            && recovery_started_at.is_none()
            && metrics.peers_discovered == 0
        {
            // The frontier ran dry inside the budget without a single peer, usually because
            // the routers' first referrals were all silent (#494). Once per walk, ask every
            // bootstrap address again: a fresh `get_peers` where the first query got no
            // answer, and a random-target `find_node` where it did, for a different set of
            // nodes. A walk that already holds peers ends as before: on a sparse swarm the
            // round found nothing new and only made the walk longer. `peers_discovered` only
            // counts correlated replies, so spoofed traffic can neither start nor suppress
            // the round. `dht_announce_peer` walks follow the same rule; they run in the
            // background, so the extra round costs them nothing visible.
            //
            // Nodes that answered earlier walks (#522) join the round, so a network where
            // the routers are slow or unreachable still has somewhere live to restart from.
            // Nodes this walk already asked are skipped: their answer led here.
            recovery_started_at = Some(metrics.peers_discovered);
            let queried_before = queried.len();
            let cached = cache.map_or_else(Vec::new, |cache| {
                cache.fresh(Instant::now(), NODE_CACHE_PER_WALK, |addr| {
                    queried.contains(addr) || bootstraps.iter().any(|node| node.addr == *addr)
                })
            });
            for (addr, kind) in recovery_targets(&bootstraps, cached) {
                if Instant::now() >= deadline {
                    break;
                }
                let txid = next_txid.to_be_bytes();
                next_txid = next_txid.wrapping_add(1);
                let query = match kind {
                    RecoveryQuery::FindNode => build_find_node(&node_id, &rand::random(), &txid),
                    RecoveryQuery::GetPeers => build_get_peers(&node_id, infohash, &txid),
                };
                let sent = tokio::time::timeout_at(
                    tokio_deadline,
                    sock.send_to(&query, SocketAddr::V4(addr)),
                )
                .await
                .is_ok_and(|sent| sent.is_ok());
                if sent {
                    metrics.nodes_queried += 1;
                    metrics.recovery_queries += 1;
                    queried.insert(addr);
                    let sent_at = Instant::now();
                    // Replaces the first query's entry, whose slot was already released: a
                    // late reply to the old txid is now uncorrelated, and only the fresh txid
                    // from this exact address is accepted.
                    inflight.insert(
                        addr,
                        Inflight {
                            txid,
                            release: sent_at + SLOT_TIMEOUT,
                            expires: sent_at + INFLIGHT_TTL,
                            active: true,
                            heard: false,
                        },
                    );
                    active += 1;
                }
            }
            if active > 0 {
                crate::alog!(
                    "[dht] frontier dry with no peers after queried={}: recovery round sent {}",
                    queried_before,
                    metrics.recovery_queries
                );
            }
        }
        if active == 0 && lookups.is_empty() {
            // Nothing left to ask and nothing outstanding that could add to the frontier.
            metrics.frontier_exhausted = true;
            crate::alog!("[dht] frontier exhausted: queried={}", queried.len());
            break;
        }
        // Wake for the next slot release so a silent node can't hold its slot past it.
        let wake = inflight
            .values()
            .filter(|query| query.active)
            .map(|query| query.release)
            .min()
            .map_or(deadline, |release| release.min(deadline));

        let received = tokio::select! {
            result = lookups.join_next(), if !lookups.is_empty() => {
                if let Some(result) = result {
                    add_bootstrap(result, &mut frontier, &mut bootstraps, &mut metrics);
                }
                continue;
            }
            received = sock.recv_from(&mut buf) => received,
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(wake)) => continue,
        };
        // A receive error (e.g. an ICMP reset some platforms surface on unconnected UDP)
        // consumes one event and the walk carries on.
        let Ok((n, SocketAddr::V4(src))) = received else {
            continue;
        };
        let Some(resp) = parse_response(&buf[..n]) else {
            // Rejected before correlation: either a node's KRPC error or genuinely
            // malformed/hostile traffic. Bucket the two apart for diagnostics.
            let error_txid = krpc_error_txid(&buf[..n]);
            if let Some(query) = inflight.get_mut(&src) {
                query.heard = true;
                // A KRPC error that echoes this query's transaction is the node's final word:
                // close the query so it no longer holds a slot. Anything else from this source
                // (malformed, or an error for another transaction) could be spoofed, so the
                // query stays open for a genuine reply.
                if error_txid.as_deref() == Some(query.txid.as_slice())
                    && query.expires > Instant::now()
                {
                    inflight.remove(&src);
                }
            }
            if error_txid.is_some() {
                metrics.krpc_errors += 1;
            } else {
                metrics.malformed_responses += 1;
            }
            continue;
        };
        // Correlate: only accept a reply whose (source, txid) matches an outstanding query.
        // Removing the entry also means a duplicate/replayed packet for the same query is not
        // processed twice. Unmatched packets never free a slot, so spoofed traffic can't
        // speed the walk up or cut it short.
        // The expiry is re-checked here because `retain` only runs when the loop wakes, and a
        // packet can land after `expires` but before the next wake.
        match inflight.get_mut(&src) {
            Some(query) if resp.txid == query.txid && query.expires > Instant::now() => {
                inflight.remove(&src);
            }
            Some(query) => {
                query.heard = true;
                metrics.uncorrelated_responses += 1;
                continue;
            }
            None => {
                metrics.uncorrelated_responses += 1;
                continue;
            }
        }
        metrics.valid_responses += 1;
        if let Some(node) = bootstraps.iter_mut().find(|node| node.addr == src) {
            node.answered = true;
        } else {
            answered.note(src, !resp.peers.is_empty());
        }
        metrics.peers_discovered += resp.peers.len();
        metrics.nodes_discovered += resp.nodes.len();
        for (id, addr) in &resp.nodes {
            frontier.entry(distance(id, infohash)).or_insert(*addr);
        }
        // Cap frontier growth: keep the 64 closest.
        while frontier.len() > 64 {
            let last = *frontier.keys().next_back().unwrap();
            frontier.remove(&last);
        }
        if on_response(src, &resp) {
            break 'walk;
        }
    }

    if let Some(before) = recovery_started_at {
        metrics.recovery_peers = metrics.peers_discovered - before;
    }
    if let Some(cache) = cache {
        cache.record(&answered.into_newest_first(), Instant::now());
    }
    crate::alog!(
        "[dht] walk done: seeded={} queried={} valid={} peers={} nodes={} malformed={} \
         krpc_err={} uncorrelated={} timeouts={} recovery_queries={} recovery_peers={} \
         frontier_exhausted={}",
        metrics.bootstrap_seeded,
        metrics.nodes_queried,
        metrics.valid_responses,
        metrics.peers_discovered,
        metrics.nodes_discovered,
        metrics.malformed_responses,
        metrics.krpc_errors,
        metrics.uncorrelated_responses,
        metrics.timeouts,
        metrics.recovery_queries,
        metrics.recovery_peers,
        metrics.frontier_exhausted,
    );
    metrics
}

/// Iterative `get_peers` toward `infohash`, bounded by `budget`. Returns discovered peers.
pub async fn dht_get_peers(infohash: &[u8; 20], budget: Duration) -> Vec<SocketAddrV4> {
    dht_get_peers_with_target(infohash, budget, DHT_PEER_TARGET).await
}

pub async fn dht_get_peers_with_target(
    infohash: &[u8; 20],
    budget: Duration,
    peer_target: usize,
) -> Vec<SocketAddrV4> {
    dht_get_peers_incremental(infohash, budget, peer_target, |_| true).await
}

/// Publish each unique peer immediately after its source/transaction correlation succeeds.
/// Returning false from the observer cancels further harvesting. The complete set remains
/// available to aggregate callers; at most 1024 addresses are retained per walk.
pub async fn dht_get_peers_incremental(
    infohash: &[u8; 20],
    budget: Duration,
    peer_target: usize,
    mut on_peer: impl FnMut(SocketAddrV4) -> bool,
) -> Vec<SocketAddrV4> {
    // No proxied DHT transport: skip rather than leak lookups around the proxy.
    if crate::proxy::skip_dht() {
        return Vec::new();
    }
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0").await else {
        return Vec::new();
    };
    let mut peers = BTreeSet::new();
    dht_walk(infohash, budget, &sock, |_src, response| {
        collect_response_peers(response, &mut peers, peer_target, &mut on_peer)
    })
    .await;
    peers.into_iter().collect()
}

fn collect_response_peers(
    response: &GetPeersResponse,
    peers: &mut BTreeSet<SocketAddrV4>,
    target: usize,
    on_peer: &mut impl FnMut(SocketAddrV4) -> bool,
) -> bool {
    for &peer in &response.peers {
        if peers.len() >= 1024 {
            return true;
        }
        if peers.insert(peer) && !on_peer(peer) {
            return true;
        }
    }
    peers.len() >= target.max(1)
}

#[cfg(test)]
async fn dht_get_peers_from_seeds(
    infohash: &[u8; 20],
    budget: Duration,
    target: usize,
    seeds: Vec<SocketAddrV4>,
) -> Vec<SocketAddrV4> {
    dht_peers_from_seeds_with_observer(infohash, budget, target, seeds, |_| true).await
}

#[cfg(test)]
async fn dht_peers_from_seeds_with_observer(
    infohash: &[u8; 20],
    budget: Duration,
    target: usize,
    seeds: Vec<SocketAddrV4>,
    mut observer: impl FnMut(SocketAddrV4) -> bool,
) -> Vec<SocketAddrV4> {
    let Ok(socket) = UdpSocket::bind("0.0.0.0:0").await else {
        return Vec::new();
    };
    let mut peers = BTreeSet::new();
    dht_walk_from_seeds(infohash, budget, &socket, seeds, |_, response| {
        collect_response_peers(response, &mut peers, target, &mut observer)
    })
    .await;
    peers.into_iter().collect()
}

#[cfg(test)]
async fn dht_stream_from_seeds(
    infohash: &[u8; 20],
    budget: Duration,
    target: usize,
    seeds: Vec<SocketAddrV4>,
    peers: tokio::sync::mpsc::Sender<SocketAddrV4>,
) {
    dht_peers_from_seeds_with_observer(infohash, budget, target, seeds, |peer| {
        peers.try_send(peer).is_ok()
    })
    .await;
}

/// The DHT half of self-announcement (BEP-5's `announce_peer`), never previously
/// implemented — `dht_get_peers` only ever read the swarm, never told it we're in it. Walks
/// toward `infohash` exactly like `dht_get_peers`, and for every node that hands us a
/// `get_peers` token (required — a node only accepts an announce echoing a token it itself
/// issued), sends it `announce_peer` for our `peer_port`. Makes outpace organically
/// discoverable via the DHT, not just tracker-discoverable (`announce_seeder`) — real
/// Acestream swarms are largely DHT-populated (see `README.md`).
///
/// Best-effort and fire-and-forget by nature (DHT is UDP): returns how many `announce_peer`
/// queries were sent, not a delivery/propagation guarantee.
pub async fn dht_announce_peer(infohash: &[u8; 20], peer_port: u16, budget: Duration) -> usize {
    if crate::proxy::skip_dht() {
        return 0;
    }
    let sock = match UdpSocket::bind("0.0.0.0:0").await {
        Ok(s) => s,
        Err(_) => return 0,
    };

    // Harvest (node, token) pairs during the walk; the actual announce send happens after,
    // back in async context (the walk's per-response callback is synchronous).
    let mut tokens: Vec<(SocketAddrV4, Vec<u8>)> = Vec::new();
    dht_walk(infohash, budget, &sock, |src, resp| {
        if let Some(token) = &resp.token {
            tokens.push((src, token.clone()));
        }
        tokens.len() >= 8
    })
    .await;
    if crate::proxy::active().is_some() {
        return 0;
    }

    let node_id = session_node_id();
    let mut announced = 0usize;
    for (i, (addr, token)) in tokens.iter().enumerate() {
        let txid = (i as u16).to_be_bytes();
        let q = build_announce_peer(&node_id, infohash, peer_port, token, &txid);
        if sock.send_to(&q, SocketAddr::V4(*addr)).await.is_ok() {
            announced += 1;
        }
    }
    announced
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn bootstrap_resolution_control(slow: bool) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Condvar, Mutex};
        let responder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let seed = v4_addr(&responder);
        let peer: SocketAddrV4 = "127.0.0.1:43210".parse().unwrap();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let slow_started = Arc::new(AtomicUsize::new(0));
        let slow_finished = Arc::new(AtomicUsize::new(0));
        let resolver = Resolver::controlled({
            let gate = gate.clone();
            let started = slow_started.clone();
            let finished = slow_finished.clone();
            move |host| {
                if host.starts_with("slow.") {
                    started.fetch_add(1, Ordering::SeqCst);
                    let mut released = gate.0.lock().unwrap();
                    while !*released {
                        released = gate.1.wait(released).unwrap();
                    }
                    finished.fetch_add(1, Ordering::SeqCst);
                }
                Ok(vec![SocketAddr::V4(seed)])
            }
        });
        let server = tokio::spawn(async move {
            let mut bytes = [0; 2048];
            let (n, caller) = responder.recv_from(&mut bytes).await.unwrap();
            let reply = response_with(&buf_txid(&bytes[..n]), &[peer], &[]);
            responder.send_to(&reply, caller).await.unwrap();
        });
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (publication, mut received) = tokio::sync::mpsc::channel(1);
        let terminal_resolver = resolver.clone();
        let run = tokio::spawn(async move {
            let hosts: &[&str] = if slow {
                &["healthy.invalid:1", "slow.invalid:1"]
            } else {
                &["healthy.invalid:1"]
            };
            dht_walk_with_bootstraps(
                &[0; 20],
                Duration::from_millis(500),
                &socket,
                hosts,
                resolver,
                None,
                |_, response| {
                    if response.peers.contains(&peer) {
                        let _ = publication.try_send(peer);
                        return true;
                    }
                    false
                },
            )
            .await
        });
        let prompt = tokio::time::timeout(Duration::from_millis(200), received.recv()).await;
        let slow_pending = !slow
            || (slow_started.load(Ordering::SeqCst) == 1
                && slow_finished.load(Ordering::SeqCst) == 0);
        let terminal = tokio::time::timeout(Duration::from_millis(800), run)
            .await
            .unwrap()
            .unwrap();
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        tokio::time::timeout(Duration::from_secs(2), async {
            while slow_finished.load(Ordering::SeqCst) != slow_started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        terminal_resolver.finish_controlled().await;
        server.abort();
        let _ = server.await;
        assert!(
            slow_pending,
            "slow native lookup was not outstanding at the measured publication boundary"
        );
        assert!(matches!(prompt, Ok(Some(found)) if found == peer), "healthy resolved bootstrap received no prompt correlated query/publication while another lookup remained pending; queried={}", terminal.nodes_queried);
    }
    #[tokio::test]
    async fn healthy_bootstrap_publishes_before_slow_resolution_finishes() {
        bootstrap_resolution_control(true).await;
    }
    #[tokio::test]
    async fn healthy_bootstrap_resolution_positive_control() {
        bootstrap_resolution_control(false).await;
    }

    #[tokio::test]
    async fn late_bootstrap_joins_the_same_walk_after_first_seed_is_unproductive() {
        use std::sync::{Arc, Condvar, Mutex};
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let healthy = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let silent_addr = v4_addr(&silent);
        let healthy_addr = v4_addr(&healthy);
        let peer: SocketAddrV4 = "127.0.0.1:43211".parse().unwrap();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let resolver = Resolver::controlled({
            let gate = gate.clone();
            move |host| {
                if host.starts_with("late.") {
                    let mut released = gate.0.lock().unwrap();
                    while !*released {
                        released = gate.1.wait(released).unwrap();
                    }
                    Ok(vec![SocketAddr::V4(healthy_addr)])
                } else {
                    Ok(vec![SocketAddr::V4(silent_addr)])
                }
            }
        });
        let release_gate = gate.clone();
        let servers = tokio::spawn(async move {
            let mut bytes = [0; 2048];
            // Resolve the late useful seed only once the first real UDP query was sent.
            silent.recv_from(&mut bytes).await.unwrap();
            *gate.0.lock().unwrap() = true;
            gate.1.notify_all();
            let (n, caller) = healthy.recv_from(&mut bytes).await.unwrap();
            healthy
                .send_to(&response_with(&buf_txid(&bytes[..n]), &[peer], &[]), caller)
                .await
                .unwrap();
        });
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let started = Instant::now();
        let mut published = false;
        let metrics = dht_walk_with_bootstraps(
            &[0; 20],
            Duration::from_millis(500),
            &socket,
            &["first.invalid:1", "late.invalid:1"],
            resolver.clone(),
            None,
            |_, response| {
                published |= response.peers.contains(&peer);
                published
            },
        )
        .await;
        let elapsed = started.elapsed();
        *release_gate.0.lock().unwrap() = true;
        release_gate.1.notify_all();
        servers.abort();
        let _ = servers.await;
        resolver.finish_controlled().await;
        assert!(
            published,
            "later bootstrap was lost after first unproductive seed"
        );
        assert_eq!(metrics.bootstrap_seeded, 2);
        assert_eq!(metrics.nodes_queried, 2);
        assert_eq!(metrics.valid_responses, 1);
        assert!(
            elapsed < Duration::from_millis(300),
            "late seed waited for a renewed/serial response budget"
        );
    }

    #[test]
    fn get_peers_query_roundtrips() {
        let q = build_get_peers(&[1u8; 20], &[2u8; 20], b"aa");
        let d = Bencode::parse(&q).unwrap();
        assert_eq!(
            d.get(b"q").unwrap().as_bytes(),
            Some(b"get_peers".as_slice())
        );
        assert_eq!(d.get(b"y").unwrap().as_bytes(), Some(b"q".as_slice()));
        let a = d.get(b"a").unwrap();
        assert_eq!(
            a.get(b"info_hash").unwrap().as_bytes(),
            Some([2u8; 20].as_slice())
        );
    }

    #[test]
    fn announce_peer_query_roundtrips() {
        let q = build_announce_peer(&[1u8; 20], &[2u8; 20], 8621, b"tok123", b"aa");
        let d = Bencode::parse(&q).unwrap();
        assert_eq!(
            d.get(b"q").unwrap().as_bytes(),
            Some(b"announce_peer".as_slice())
        );
        assert_eq!(d.get(b"y").unwrap().as_bytes(), Some(b"q".as_slice()));
        let a = d.get(b"a").unwrap();
        assert_eq!(
            a.get(b"info_hash").unwrap().as_bytes(),
            Some([2u8; 20].as_slice())
        );
        assert_eq!(a.get(b"id").unwrap().as_bytes(), Some([1u8; 20].as_slice()));
        assert_eq!(a.get(b"port").unwrap().as_int(), Some(8621));
        assert_eq!(
            a.get(b"token").unwrap().as_bytes(),
            Some(b"tok123".as_slice())
        );
        assert!(
            a.get(b"implied_port").is_none(),
            "the engine sends no implied_port"
        );
    }

    /// Byte-exact engine shapes: keys `a q t v y`, the pymdht `v` tag, 2-byte txids.
    #[test]
    fn queries_match_engine_wire_shape() {
        let id = [1u8; 20];
        let ih = [2u8; 20];
        let mut expected = b"d1:ad2:id20:".to_vec();
        expected.extend_from_slice(&id);
        expected.extend_from_slice(b"9:info_hash20:");
        expected.extend_from_slice(&ih);
        expected.extend_from_slice(b"e1:q9:get_peers1:t2:aa1:v4:NS\x01\x181:y1:qe");
        assert_eq!(build_get_peers(&id, &ih, b"aa"), expected);

        let mut expected = b"d1:ad2:id20:".to_vec();
        expected.extend_from_slice(&id);
        expected.extend_from_slice(b"9:info_hash20:");
        expected.extend_from_slice(&ih);
        expected.extend_from_slice(b"4:porti8621e5:token2:tke1:q13:announce_peer1:t2:aa");
        expected.extend_from_slice(b"1:v4:NS\x01\x181:y1:qe");
        assert_eq!(build_announce_peer(&id, &ih, 8621, b"tk", b"aa"), expected);
    }

    #[test]
    fn node_id_is_stable_for_the_process() {
        assert_eq!(session_node_id(), session_node_id());
    }

    #[test]
    fn parses_values_peers() {
        // r = { id:20, token:.., values: [ "<4 ip><2 port>" ] }
        let mut r: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        r.insert(b"id".to_vec(), Bencode::Bytes(vec![0; 20]));
        let peer = vec![87u8, 221, 96, 148, 0x21, 0xAD]; // 87.221.96.148:8621
        r.insert(
            b"values".to_vec(),
            Bencode::List(vec![Bencode::Bytes(peer)]),
        );
        let mut top: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        top.insert(b"r".to_vec(), Bencode::Dict(r));
        top.insert(b"y".to_vec(), Bencode::Bytes(b"r".to_vec()));
        let resp = parse_response(&Bencode::Dict(top).encode()).unwrap();
        assert_eq!(resp.peers, vec!["87.221.96.148:8621".parse().unwrap()]);
    }

    #[test]
    fn parses_token_needed_to_announce_back() {
        let mut r: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        r.insert(b"id".to_vec(), Bencode::Bytes(vec![0; 20]));
        r.insert(b"token".to_vec(), Bencode::Bytes(b"opaque-tok".to_vec()));
        let mut top: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        top.insert(b"r".to_vec(), Bencode::Dict(r));
        top.insert(b"y".to_vec(), Bencode::Bytes(b"r".to_vec()));
        let resp = parse_response(&Bencode::Dict(top).encode()).unwrap();
        assert_eq!(resp.token, Some(b"opaque-tok".to_vec()));
    }

    #[test]
    fn no_token_parses_as_none() {
        let mut r: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        r.insert(b"id".to_vec(), Bencode::Bytes(vec![0; 20]));
        let mut top: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        top.insert(b"r".to_vec(), Bencode::Dict(r));
        top.insert(b"y".to_vec(), Bencode::Bytes(b"r".to_vec()));
        let resp = parse_response(&Bencode::Dict(top).encode()).unwrap();
        assert_eq!(resp.token, None);
    }

    #[test]
    fn parses_compact_nodes() {
        let mut node = vec![9u8; 20]; // node id
        node.extend_from_slice(&[1, 2, 3, 4, 0x1A, 0xE1]); // 1.2.3.4:6881
        let mut r: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        r.insert(b"nodes".to_vec(), Bencode::Bytes(node));
        let mut top: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        top.insert(b"r".to_vec(), Bencode::Dict(r));
        top.insert(b"y".to_vec(), Bencode::Bytes(b"r".to_vec()));
        let resp = parse_response(&Bencode::Dict(top).encode()).unwrap();
        assert_eq!(resp.nodes.len(), 1);
        assert_eq!(resp.nodes[0].1, "1.2.3.4:6881".parse().unwrap());
    }

    // --- Malformed / hostile KRPC response hardening (#41) ---
    //
    // Public DHT UDP is untrusted, so `parse_response` must ignore anything that isn't a
    // well-formed `get_peers` *response*, rather than mining peers/nodes/tokens out of it.

    /// Build a raw top-level KRPC message: an optional `y` (message type), a `t`
    /// transaction id, and the given `r` value — which need not be a dict, so malformed
    /// shapes can be exercised.
    fn krpc_message(y: Option<&[u8]>, r: Bencode) -> Vec<u8> {
        let mut top: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        top.insert(b"t".to_vec(), Bencode::Bytes(b"aa".to_vec()));
        if let Some(y) = y {
            top.insert(b"y".to_vec(), Bencode::Bytes(y.to_vec()));
        }
        top.insert(b"r".to_vec(), r);
        Bencode::Dict(top).encode()
    }

    /// Build an `r` response dict from optional `values`/`nodes`/`token` parts.
    fn r_dict(values: Option<Bencode>, nodes: Option<Bencode>, token: Option<Bencode>) -> Bencode {
        let mut r: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        r.insert(b"id".to_vec(), Bencode::Bytes(vec![7; 20]));
        if let Some(v) = values {
            r.insert(b"values".to_vec(), v);
        }
        if let Some(n) = nodes {
            r.insert(b"nodes".to_vec(), n);
        }
        if let Some(t) = token {
            r.insert(b"token".to_vec(), t);
        }
        Bencode::Dict(r)
    }

    /// A `values` list carrying exactly one valid compact peer (10.0.0.1:1111).
    fn one_valid_peer_values() -> Bencode {
        Bencode::List(vec![Bencode::Bytes(vec![10, 0, 0, 1, 0x04, 0x57])])
    }

    #[test]
    fn ignores_message_whose_type_is_not_a_response() {
        // y = "q" (a query), not "r". A hostile packet can still smuggle an `r` dict with
        // peers and a token; gating on the message type keeps those from being trusted.
        let r = r_dict(
            Some(one_valid_peer_values()),
            None,
            Some(Bencode::Bytes(b"tok".to_vec())),
        );
        assert!(parse_response(&krpc_message(Some(b"q"), r)).is_none());
    }

    #[test]
    fn ignores_response_with_missing_message_type() {
        // BEP-5 requires `y`; a message that omits it is malformed and must be ignored.
        let r = r_dict(Some(one_valid_peer_values()), None, None);
        assert!(parse_response(&krpc_message(None, r)).is_none());
    }

    #[test]
    fn ignores_error_message() {
        // KRPC error: y="e", e=[code, msg], no `r`.
        let mut top: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        top.insert(b"t".to_vec(), Bencode::Bytes(b"aa".to_vec()));
        top.insert(b"y".to_vec(), Bencode::Bytes(b"e".to_vec()));
        top.insert(
            b"e".to_vec(),
            Bencode::List(vec![Bencode::Int(201), Bencode::Bytes(b"err".to_vec())]),
        );
        assert!(parse_response(&Bencode::Dict(top).encode()).is_none());
    }

    #[test]
    fn ignores_response_whose_r_is_not_a_dict() {
        // y="r" but `r` is a byte string, not the expected dict — reject deterministically
        // instead of silently treating it as an empty response.
        let raw = krpc_message(Some(b"r"), Bencode::Bytes(b"not-a-dict".to_vec()));
        assert!(parse_response(&raw).is_none());
    }

    #[test]
    fn ignores_partial_trailing_compact_node_record() {
        // One full 26-byte node followed by a truncated fragment: the fragment is dropped,
        // the valid node survives.
        let mut nodes = vec![9u8; 20];
        nodes.extend_from_slice(&[1, 2, 3, 4, 0x1A, 0xE1]); // 1.2.3.4:6881
        nodes.extend_from_slice(&[0xAB; 10]); // partial record
        let r = r_dict(None, Some(Bencode::Bytes(nodes)), None);
        let resp = parse_response(&krpc_message(Some(b"r"), r)).unwrap();
        assert_eq!(resp.nodes.len(), 1);
        assert_eq!(resp.nodes[0].1, "1.2.3.4:6881".parse().unwrap());
    }

    #[test]
    fn skips_invalid_compact_peer_values() {
        // Wrong-length entries, a port-0 entry, and a non-bytes entry are all skipped;
        // only the one valid compact peer survives.
        let vals = Bencode::List(vec![
            Bencode::Bytes(vec![10, 0, 0, 5]),       // too short (4 bytes)
            Bencode::Bytes(vec![10, 0, 0, 6, 0, 0]), // port 0
            Bencode::Bytes(vec![10, 0, 0, 7, 0x04, 0x57, 0xFF]), // too long (7 bytes)
            Bencode::Int(42),                        // not bytes at all
            Bencode::Bytes(vec![10, 0, 0, 1, 0x04, 0x57]), // valid -> 10.0.0.1:1111
        ]);
        let r = r_dict(Some(vals), None, None);
        let resp = parse_response(&krpc_message(Some(b"r"), r)).unwrap();
        assert_eq!(resp.peers, vec!["10.0.0.1:1111".parse().unwrap()]);
    }

    #[test]
    fn ignores_values_that_are_not_a_list() {
        // `values` as a byte string (not a list) yields no peers.
        let r = r_dict(
            Some(Bencode::Bytes(vec![10, 0, 0, 1, 0x04, 0x57])),
            None,
            None,
        );
        let resp = parse_response(&krpc_message(Some(b"r"), r)).unwrap();
        assert!(resp.peers.is_empty());
    }

    #[test]
    fn ignores_nodes_that_are_not_bytes() {
        // `nodes` as a list (not the compact byte string) yields no nodes.
        let r = r_dict(None, Some(Bencode::List(vec![Bencode::Int(1)])), None);
        let resp = parse_response(&krpc_message(Some(b"r"), r)).unwrap();
        assert!(resp.nodes.is_empty());
    }

    #[test]
    fn ignores_token_that_is_not_bytes() {
        // `token` as an int (not bytes) is treated as absent, not coerced.
        let r = r_dict(None, None, Some(Bencode::Int(42)));
        let resp = parse_response(&krpc_message(Some(b"r"), r)).unwrap();
        assert_eq!(resp.token, None);
    }

    #[test]
    fn drops_payload_truncated_at_the_receive_buffer_boundary() {
        // A datagram larger than the 2048-byte receive buffer arrives truncated; the
        // remaining bencode is incomplete and must be rejected, not partially trusted.
        let big_token = vec![0x5A; 4096];
        let r = r_dict(
            Some(one_valid_peer_values()),
            None,
            Some(Bencode::Bytes(big_token)),
        );
        let raw = krpc_message(Some(b"r"), r);
        assert!(raw.len() > 2048, "fixture must exceed the receive buffer");
        assert!(parse_response(&raw[..2048]).is_none());
    }

    #[test]
    fn distance_is_zero_to_self_and_orders() {
        assert_eq!(distance(&[5; 20], &[5; 20]), [0; 20]);
        assert!(distance(&[0; 20], &[1; 20]) < distance(&[0xff; 20], &[0; 20]));
    }

    /// Extract the KRPC transaction id (`t`) from a raw query/response buffer.
    fn buf_txid(buf: &[u8]) -> Vec<u8> {
        Bencode::parse(buf)
            .unwrap()
            .get(b"t")
            .and_then(Bencode::as_bytes)
            .expect("query must carry a transaction id")
            .to_vec()
    }

    /// Build a `get_peers` `values` response echoing `txid` and carrying `peers`.
    fn values_response(txid: &[u8], peers: &[SocketAddrV4]) -> Vec<u8> {
        let vals: Vec<Bencode> = peers
            .iter()
            .map(|p| {
                let mut b = Vec::new();
                b.extend_from_slice(&p.ip().octets());
                b.extend_from_slice(&p.port().to_be_bytes());
                Bencode::Bytes(b)
            })
            .collect();
        let mut r: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        r.insert(b"id".to_vec(), Bencode::Bytes(vec![7; 20]));
        r.insert(b"values".to_vec(), Bencode::List(vals));
        let mut top: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        top.insert(b"t".to_vec(), Bencode::Bytes(txid.to_vec()));
        top.insert(b"y".to_vec(), Bencode::Bytes(b"r".to_vec()));
        top.insert(b"r".to_vec(), Bencode::Dict(r));
        Bencode::Dict(top).encode()
    }

    #[test]
    fn parse_response_extracts_transaction_id() {
        // The transaction id lives at the top level (`t`), not inside `r`; the walk needs it
        // to correlate a reply against the query it answers.
        let resp = values_response(b"Zx", &["10.0.0.9:9000".parse().unwrap()]);
        let got = parse_response(&resp).unwrap();
        assert_eq!(got.txid, b"Zx".to_vec());
    }

    #[tokio::test]
    async fn dht_stream_publishes_first_correlated_reply_before_later_reply() {
        let mut servers = tokio::task::JoinSet::new();
        let mut seeds = Vec::new();
        for (i, delay) in [Duration::ZERO, Duration::from_millis(600)]
            .into_iter()
            .enumerate()
        {
            let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let SocketAddr::V4(seed) = socket.local_addr().unwrap() else {
                unreachable!()
            };
            seeds.push(seed);
            servers.spawn(async move {
                let mut buf = [0; 2048];
                let (n, client) = socket.recv_from(&mut buf).await.unwrap();
                let txid = buf_txid(&buf[..n]);
                tokio::time::sleep(delay).await;
                let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1000 + i as u16);
                socket
                    .send_to(&values_response(&txid, &[peer]), client)
                    .await
                    .unwrap();
            });
        }
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let walk = tokio::spawn(async move {
            dht_stream_from_seeds(&[9; 20], Duration::from_secs(2), 2, seeds, tx).await;
        });
        let first = tokio::time::timeout(Duration::from_millis(250), rx.recv()).await;
        if first.is_err() {
            walk.abort();
            servers.abort_all();
        }
        if let Ok(first) = &first {
            let second = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert_ne!(first.as_ref().unwrap(), &second);
        }
        let _ = walk.await;
        while servers.join_next().await.is_some() {}
        assert!(
            first.is_ok(),
            "first correlated peer was hidden until the walk completed"
        );
    }

    #[tokio::test]
    async fn incremental_observer_rejects_wrong_txid_source_replay_and_duplicate_values() {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let spoof = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let SocketAddr::V4(seed) = socket.local_addr().unwrap() else {
            unreachable!()
        };
        let expected = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1234);
        let server = tokio::spawn(async move {
            let mut bytes = [0; 2048];
            let (n, client) = socket.recv_from(&mut bytes).await.unwrap();
            let txid = buf_txid(&bytes[..n]);
            let mut wrong = txid.clone();
            wrong.push(0xff);
            let bad = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 4321);
            socket
                .send_to(&values_response(&wrong, &[bad]), client)
                .await
                .unwrap();
            spoof
                .send_to(&values_response(&txid, &[bad]), client)
                .await
                .unwrap();
            socket
                .send_to(&values_response(&txid, &[expected, expected]), client)
                .await
                .unwrap();
            socket
                .send_to(&values_response(&txid, &[bad]), client)
                .await
                .unwrap();
        });
        let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
        dht_stream_from_seeds(&[0; 20], Duration::from_millis(150), 8, vec![seed], sender).await;
        server.await.unwrap();
        assert_eq!(receiver.recv().await, Some(expected));
        assert!(
            receiver.recv().await.is_none(),
            "uncorrelated/replayed/duplicate peer reached production observer"
        );
    }

    #[tokio::test]
    async fn dht_response_window_respects_remaining_walk_budget() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let SocketAddr::V4(seed) = server.local_addr().unwrap() else {
            unreachable!()
        };
        let start = Instant::now();
        let peers =
            dht_get_peers_from_seeds(&[9; 20], Duration::from_millis(40), 8, vec![seed]).await;
        let elapsed = start.elapsed();
        assert!(peers.is_empty());
        let mut query = [0; 2048];
        assert!(
            server.try_recv_from(&mut query).is_ok(),
            "actual UDP query required"
        );
        assert!(
            elapsed < Duration::from_millis(250),
            "40ms walk consumed {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn dht_lookup_stops_once_peer_target_is_met() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let seed = match server.local_addr().unwrap() {
            SocketAddr::V4(v4) => v4,
            _ => panic!("test server must be IPv4"),
        };
        let handle = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, peer) = server.recv_from(&mut buf).await.unwrap();
            // A real node echoes the query's transaction id; the client now requires it.
            let txid = buf_txid(&buf[..n]);
            let resp = values_response(
                &txid,
                &[
                    "10.0.0.1:1111".parse().unwrap(),
                    "10.0.0.2:2222".parse().unwrap(),
                ],
            );
            server.send_to(&resp, peer).await.unwrap();
        });

        let start = Instant::now();
        let peers =
            dht_get_peers_from_seeds(&[9u8; 20], Duration::from_secs(5), 2, vec![seed]).await;
        assert_eq!(
            peers,
            vec![
                "10.0.0.1:1111".parse().unwrap(),
                "10.0.0.2:2222".parse().unwrap()
            ]
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "lookup should return as soon as enough peers are available"
        );
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn dht_ignores_response_with_wrong_transaction_id() {
        // A valid-looking response from the queried node but echoing a transaction id we never
        // sent must not contribute peers — it is unrelated (or forged) traffic.
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let seed = match server.local_addr().unwrap() {
            SocketAddr::V4(v4) => v4,
            _ => panic!("test server must be IPv4"),
        };
        let handle = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, peer) = server.recv_from(&mut buf).await.unwrap();
            let real = buf_txid(&buf[..n]);
            // Deliberately corrupt the echoed txid so it cannot match any inflight query.
            let mut wrong = real.clone();
            wrong.push(0xff);
            let resp = values_response(&wrong, &["10.0.0.1:1111".parse().unwrap()]);
            server.send_to(&resp, peer).await.unwrap();
        });

        let peers =
            dht_get_peers_from_seeds(&[9u8; 20], Duration::from_millis(1200), 2, vec![seed]).await;
        assert!(
            peers.is_empty(),
            "response with a mismatched transaction id must be ignored, got {peers:?}"
        );
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn dht_ignores_response_from_wrong_source() {
        // A response echoing the correct transaction id but arriving from an address we never
        // queried must be ignored: an off-path attacker who guesses/observes the txid still
        // cannot inject peers unless the source also matches the inflight query.
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let spoofer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let seed = match server.local_addr().unwrap() {
            SocketAddr::V4(v4) => v4,
            _ => panic!("test server must be IPv4"),
        };
        let handle = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            // The queried node stays silent; the spoofer (different source) replies with the
            // correct txid it observed from the query.
            let (n, peer) = server.recv_from(&mut buf).await.unwrap();
            let txid = buf_txid(&buf[..n]);
            let resp = values_response(&txid, &["10.0.0.1:1111".parse().unwrap()]);
            spoofer.send_to(&resp, peer).await.unwrap();
        });

        let peers =
            dht_get_peers_from_seeds(&[9u8; 20], Duration::from_millis(1200), 2, vec![seed]).await;
        assert!(
            peers.is_empty(),
            "response from an unqueried source must be ignored, got {peers:?}"
        );
        handle.await.unwrap();
    }

    // --- Walk diagnostics / metrics (#43) ---
    //
    // A raw peer count hides useful failure modes (silence, malformed traffic, KRPC errors,
    // uncorrelated packets, frontier exhaustion). `dht_walk_from_seeds` returns a
    // `DhtWalkMetrics` so those modes are observable and deterministically testable offline.

    /// Build a `get_peers` response echoing `txid` with the given compact peers and nodes.
    fn response_with(
        txid: &[u8],
        peers: &[SocketAddrV4],
        nodes: &[([u8; 20], SocketAddrV4)],
    ) -> Vec<u8> {
        let vals: Vec<Bencode> = peers
            .iter()
            .map(|p| {
                let mut b = Vec::new();
                b.extend_from_slice(&p.ip().octets());
                b.extend_from_slice(&p.port().to_be_bytes());
                Bencode::Bytes(b)
            })
            .collect();
        let mut node_bytes = Vec::new();
        for (id, addr) in nodes {
            node_bytes.extend_from_slice(id);
            node_bytes.extend_from_slice(&addr.ip().octets());
            node_bytes.extend_from_slice(&addr.port().to_be_bytes());
        }
        let mut r: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        r.insert(b"id".to_vec(), Bencode::Bytes(vec![7; 20]));
        if !peers.is_empty() {
            r.insert(b"values".to_vec(), Bencode::List(vals));
        }
        if !nodes.is_empty() {
            r.insert(b"nodes".to_vec(), Bencode::Bytes(node_bytes));
        }
        let mut top: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        top.insert(b"t".to_vec(), Bencode::Bytes(txid.to_vec()));
        top.insert(b"y".to_vec(), Bencode::Bytes(b"r".to_vec()));
        top.insert(b"r".to_vec(), Bencode::Dict(r));
        Bencode::Dict(top).encode()
    }

    /// Build a KRPC error message (`y == "e"`) echoing `txid`.
    fn error_response(txid: &[u8]) -> Vec<u8> {
        let mut top: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        top.insert(b"t".to_vec(), Bencode::Bytes(txid.to_vec()));
        top.insert(b"y".to_vec(), Bencode::Bytes(b"e".to_vec()));
        top.insert(
            b"e".to_vec(),
            Bencode::List(vec![Bencode::Int(201), Bencode::Bytes(b"err".to_vec())]),
        );
        Bencode::Dict(top).encode()
    }

    fn v4_addr(sock: &UdpSocket) -> SocketAddrV4 {
        match sock.local_addr().unwrap() {
            SocketAddr::V4(v4) => v4,
            _ => panic!("test socket must be IPv4"),
        }
    }

    #[tokio::test]
    async fn walk_metrics_count_a_successful_response_path() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let seed = v4_addr(&server);
        let handle = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, peer) = server.recv_from(&mut buf).await.unwrap();
            let txid = buf_txid(&buf[..n]);
            let resp = response_with(
                &txid,
                &[
                    "10.0.0.1:1111".parse().unwrap(),
                    "10.0.0.2:2222".parse().unwrap(),
                ],
                &[([9u8; 20], "1.2.3.4:6881".parse().unwrap())],
            );
            server.send_to(&resp, peer).await.unwrap();
        });

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut seen: BTreeSet<SocketAddrV4> = BTreeSet::new();
        let metrics = dht_walk_from_seeds(
            &[9u8; 20],
            Duration::from_secs(5),
            &client,
            vec![seed],
            |_src, resp| {
                seen.extend(resp.peers.iter().copied());
                seen.len() >= 2
            },
        )
        .await;

        assert_eq!(metrics.bootstrap_seeded, 1);
        assert_eq!(metrics.nodes_queried, 1);
        assert_eq!(metrics.valid_responses, 1);
        assert_eq!(metrics.peers_discovered, 2);
        assert_eq!(metrics.nodes_discovered, 1);
        assert_eq!(metrics.malformed_responses, 0);
        assert_eq!(metrics.krpc_errors, 0);
        assert_eq!(metrics.uncorrelated_responses, 0);
        assert_eq!(metrics.timeouts, 0);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn walk_metrics_count_ignored_malformed_and_error_traffic() {
        // One seed replies with a well-formed KRPC error, another with non-bencode garbage;
        // neither yields peers, and each is bucketed by its own failure mode.
        let err_server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let garbage_server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let err_seed = v4_addr(&err_server);
        let garbage_seed = v4_addr(&garbage_server);

        let h1 = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, peer) = err_server.recv_from(&mut buf).await.unwrap();
            let txid = buf_txid(&buf[..n]);
            err_server
                .send_to(&error_response(&txid), peer)
                .await
                .unwrap();
        });
        let h2 = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (_n, peer) = garbage_server.recv_from(&mut buf).await.unwrap();
            garbage_server
                .send_to(b"not-a-bencode-datagram", peer)
                .await
                .unwrap();
        });

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let metrics = dht_walk_from_seeds(
            &[9u8; 20],
            Duration::from_secs(5),
            &client,
            vec![err_seed, garbage_seed],
            |_src, _resp| false,
        )
        .await;

        assert_eq!(metrics.valid_responses, 0);
        assert_eq!(metrics.peers_discovered, 0);
        assert_eq!(metrics.krpc_errors, 1);
        assert_eq!(metrics.malformed_responses, 1);
        assert!(metrics.frontier_exhausted);
        h1.await.unwrap();
        h2.await.unwrap();
    }

    #[tokio::test]
    async fn walk_metrics_count_a_silent_query_as_a_timeout() {
        // A seed that receives the query but never answers: the response window elapses in
        // total silence, which is a timeout, and the walk then exhausts its frontier.
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let seed = v4_addr(&server);
        // Keep `server` bound (so the port stays reachable, no ICMP unreachable) but silent.

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let metrics = dht_walk_from_seeds(
            &[9u8; 20],
            Duration::from_secs(5),
            &client,
            vec![seed],
            |_src, _resp| false,
        )
        .await;

        assert_eq!(metrics.valid_responses, 0);
        assert_eq!(metrics.timeouts, 1);
        assert!(metrics.frontier_exhausted);
        drop(server);
    }

    // --- Response pacing (#384) ---

    /// Answer one `get_peers` query on `server` with `reply(txid)`, recording when it arrived.
    fn answer_once(
        server: UdpSocket,
        reply: impl FnOnce(&[u8]) -> Vec<u8> + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, caller) = server.recv_from(&mut buf).await.unwrap();
            let resp = reply(&buf_txid(&buf[..n]));
            server.send_to(&resp, caller).await.unwrap();
            // Keep the port bound until the walk is done with it.
            std::future::pending::<()>().await
        })
    }

    #[tokio::test]
    async fn multi_hop_walk_advances_as_replies_arrive() {
        // Four hops, each answering promptly with one node closer to the target and the last
        // handing over a peer. The first peer must arrive at network speed, not after one
        // fixed response window per hop.
        let target = [0u8; 20];
        let hops: Vec<UdpSocket> = bind_loopback(4).await;
        let addrs: Vec<SocketAddrV4> = hops.iter().map(v4_addr).collect();
        let peer: SocketAddrV4 = "10.0.0.1:1111".parse().unwrap();
        let mut servers = Vec::new();
        for (i, hop) in hops.into_iter().enumerate() {
            let next = addrs.get(i + 1).copied();
            servers.push(answer_once(hop, move |txid| match next {
                Some(next) => {
                    let mut id = [0u8; 20];
                    id[0] = 0x80 >> i;
                    response_with(txid, &[], &[(id, next)])
                }
                None => response_with(txid, &[peer], &[]),
            }));
        }
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let start = Instant::now();
        let mut first_peer = None;
        let metrics = dht_walk_from_seeds(
            &target,
            Duration::from_secs(10),
            &client,
            vec![addrs[0]],
            |_src, resp| {
                if resp.peers.contains(&peer) {
                    first_peer = Some(start.elapsed());
                }
                first_peer.is_some()
            },
        )
        .await;
        for server in servers {
            server.abort();
        }
        let first_peer = first_peer.expect("chain end never reached");
        eprintln!("multi-hop first peer after {first_peer:?}");
        assert_eq!(metrics.nodes_queried, 4);
        assert_eq!(metrics.valid_responses, 4);
        assert_eq!(metrics.recovery_queries, 0);
        assert!(
            first_peer < Duration::from_millis(300),
            "4-hop walk took {first_peer:?}; it is waiting out fixed rounds"
        );
    }

    async fn bind_loopback(n: usize) -> Vec<UdpSocket> {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        }
        out
    }

    #[tokio::test]
    async fn reply_after_inflight_ttl_is_rejected_without_an_intervening_wake() {
        // The only query's slot releases at SLOT_TIMEOUT; a still-pending bootstrap lookup then
        // keeps the walk parked with no other wake before the reply lands past INFLIGHT_TTL.
        // That late packet must not be correlated against the expired entry.
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let seed = v4_addr(&server);
        let peer: SocketAddrV4 = "10.0.0.4:4444".parse().unwrap();
        let replier = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, caller) = server.recv_from(&mut buf).await.unwrap();
            tokio::time::sleep(INFLIGHT_TTL + Duration::from_millis(200)).await;
            let resp = response_with(&buf_txid(&buf[..n]), &[peer], &[]);
            server.send_to(&resp, caller).await.unwrap();
            std::future::pending::<()>().await
        });
        let mut lookups = tokio::task::JoinSet::new();
        lookups.spawn(async {
            std::future::pending::<()>().await;
            (0, Vec::new())
        });
        let mut frontier = BTreeMap::new();
        frontier.insert([0xff; 20], seed);
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut accepted = false;
        let metrics = dht_walk_owned(
            &[0u8; 20],
            Instant::now() + INFLIGHT_TTL + Duration::from_millis(600),
            &client,
            frontier,
            Vec::new(),
            lookups,
            None,
            |_src, resp| {
                accepted |= resp.peers.contains(&peer);
                false
            },
        )
        .await;
        replier.abort();
        assert!(!accepted, "reply past INFLIGHT_TTL was correlated");
        assert_eq!(metrics.valid_responses, 0);
        assert_eq!(metrics.uncorrelated_responses, 1);
    }

    #[tokio::test]
    async fn correlated_reply_frees_a_query_slot_immediately() {
        // Nine seeds in frontier order: the first answers at once (with nothing useful), the
        // next seven stay silent, and the ninth is only reachable once a slot frees. The
        // answer must free that slot right away rather than at the end of a round.
        let sockets = bind_loopback(9).await;
        let seeds: Vec<SocketAddrV4> = sockets.iter().map(v4_addr).collect();
        let mut sockets = sockets.into_iter();
        let fast = answer_once(sockets.next().unwrap(), |txid| {
            response_with(txid, &[], &[])
        });
        let silent: Vec<UdpSocket> = sockets.by_ref().take(7).collect();
        let peer: SocketAddrV4 = "10.0.0.2:2222".parse().unwrap();
        let ninth = answer_once(sockets.next().unwrap(), move |txid| {
            response_with(txid, &[peer], &[])
        });
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let start = Instant::now();
        let mut found = None;
        dht_walk_from_seeds(
            &[0u8; 20],
            Duration::from_secs(5),
            &client,
            seeds,
            |_src, resp| {
                if resp.peers.contains(&peer) {
                    found = Some(start.elapsed());
                }
                found.is_some()
            },
        )
        .await;
        fast.abort();
        ninth.abort();
        drop(silent);
        let found = found.expect("ninth seed never queried");
        assert!(
            found < Duration::from_millis(300),
            "freed slot was not refilled until {found:?}"
        );
    }

    #[tokio::test]
    async fn silent_queries_release_their_slots_after_the_soft_timeout() {
        // Eight silent seeds fill every slot. The ninth must not be queried before the soft
        // slot timeout, and must not wait for the longer correlation TTL either.
        let sockets = bind_loopback(9).await;
        let seeds: Vec<SocketAddrV4> = sockets.iter().map(v4_addr).collect();
        let mut sockets = sockets.into_iter();
        let silent: Vec<UdpSocket> = sockets.by_ref().take(8).collect();
        let peer: SocketAddrV4 = "10.0.0.3:3333".parse().unwrap();
        let ninth = answer_once(sockets.next().unwrap(), move |txid| {
            response_with(txid, &[peer], &[])
        });
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let start = Instant::now();
        let metrics = dht_walk_from_seeds(
            &[0u8; 20],
            Duration::from_secs(5),
            &client,
            seeds,
            |_src, resp| resp.peers.contains(&peer),
        )
        .await;
        let mut buf = [0u8; 2048];
        for socket in &silent {
            assert!(
                socket.try_recv_from(&mut buf).is_ok(),
                "silent seed unqueried"
            );
            assert!(
                socket.try_recv_from(&mut buf).is_err(),
                "silent seed queried twice"
            );
        }
        // The walk stops on the ninth seed's reply, so its length is when that seed was asked.
        let elapsed = start.elapsed();
        ninth.abort();
        assert_eq!(metrics.nodes_queried, 9);
        assert_eq!(metrics.valid_responses, 1);
        assert_eq!(metrics.timeouts, 8);
        assert!(
            elapsed >= SLOT_TIMEOUT - Duration::from_millis(100),
            "ninth seed queried before any slot released ({elapsed:?})"
        );
        assert!(
            elapsed < SLOT_TIMEOUT + Duration::from_millis(700),
            "slots were held for {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn correlated_krpc_error_frees_its_slot_immediately() {
        // A node that answers with a KRPC error for our transaction will not send a usable
        // reply, so its slot must free at once rather than at SLOT_TIMEOUT.
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let seed = v4_addr(&server);
        let handle = answer_once(server, error_response);
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let start = Instant::now();
        let metrics = dht_walk_from_seeds(
            &[9u8; 20],
            Duration::from_secs(10),
            &client,
            vec![seed],
            |_src, _resp| false,
        )
        .await;
        let elapsed = start.elapsed();
        handle.abort();
        assert_eq!(metrics.krpc_errors, 1);
        assert_eq!(metrics.timeouts, 0);
        assert!(metrics.frontier_exhausted);
        assert!(
            elapsed < Duration::from_millis(500),
            "waited {elapsed:?} on a node that already refused"
        );
    }

    /// A datagram typed as a KRPC error, with this `t`, carrying `e` as given (or none).
    fn error_with_payload(txid: &[u8], e: Option<Bencode>) -> Vec<u8> {
        let mut top: BTreeMap<Vec<u8>, Bencode> = BTreeMap::new();
        top.insert(b"t".to_vec(), Bencode::Bytes(txid.to_vec()));
        top.insert(b"y".to_vec(), Bencode::Bytes(b"e".to_vec()));
        if let Some(e) = e {
            top.insert(b"e".to_vec(), e);
        }
        Bencode::Dict(top).encode()
    }

    #[test]
    fn krpc_error_txid_requires_a_code_and_message_payload() {
        assert_eq!(
            krpc_error_txid(&error_response(b"ab")),
            Some(b"ab".to_vec())
        );
        let invalid = [
            None,
            Some(Bencode::Bytes(b"err".to_vec())),
            Some(Bencode::List(vec![])),
            Some(Bencode::List(vec![Bencode::Int(201)])),
            Some(Bencode::List(vec![
                Bencode::Bytes(b"201".to_vec()),
                Bencode::Bytes(b"err".to_vec()),
            ])),
            Some(Bencode::List(vec![
                Bencode::Int(201),
                Bencode::Bytes(b"err".to_vec()),
                Bencode::Int(1),
            ])),
        ];
        for e in invalid {
            let label = format!("{e:?}");
            assert_eq!(
                krpc_error_txid(&error_with_payload(b"ab", e)),
                None,
                "{label}"
            );
        }
    }

    #[tokio::test]
    async fn correlated_error_without_a_valid_payload_does_not_close_the_query() {
        // Errors echoing the right transaction but with a missing or invalid `e` are
        // malformed, so they must leave the query open for the genuine reply that follows.
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let seed = v4_addr(&server);
        let peer: SocketAddrV4 = "10.0.0.7:7777".parse().unwrap();
        let handle = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, caller) = server.recv_from(&mut buf).await.unwrap();
            let txid = buf_txid(&buf[..n]);
            for e in [None, Some(Bencode::Bytes(b"err".to_vec()))] {
                server
                    .send_to(&error_with_payload(&txid, e), caller)
                    .await
                    .unwrap();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            server
                .send_to(&response_with(&txid, &[peer], &[]), caller)
                .await
                .unwrap();
            std::future::pending::<()>().await
        });
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut found = false;
        let metrics = dht_walk_from_seeds(
            &[9u8; 20],
            Duration::from_secs(5),
            &client,
            vec![seed],
            |_src, resp| {
                found |= resp.peers.contains(&peer);
                found
            },
        )
        .await;
        handle.abort();
        assert!(found, "malformed error closed the query: {metrics:?}");
        assert_eq!(metrics.malformed_responses, 2);
        assert_eq!(metrics.krpc_errors, 0);
        assert_eq!(metrics.valid_responses, 1);
    }

    #[tokio::test]
    async fn uncorrelated_krpc_error_does_not_close_the_query() {
        // A KRPC error with the wrong transaction id may be spoofed; it must not close the
        // query, so a genuine reply that follows is still accepted.
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let seed = v4_addr(&server);
        let peer: SocketAddrV4 = "10.0.0.6:6666".parse().unwrap();
        let handle = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, caller) = server.recv_from(&mut buf).await.unwrap();
            let txid = buf_txid(&buf[..n]);
            let mut wrong = txid.clone();
            wrong.push(0xff);
            server
                .send_to(&error_response(&wrong), caller)
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            server
                .send_to(&response_with(&txid, &[peer], &[]), caller)
                .await
                .unwrap();
            std::future::pending::<()>().await
        });
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut found = false;
        let metrics = dht_walk_from_seeds(
            &[9u8; 20],
            Duration::from_secs(5),
            &client,
            vec![seed],
            |_src, resp| {
                found |= resp.peers.contains(&peer);
                found
            },
        )
        .await;
        handle.abort();
        assert!(found, "spoofable error closed the query: {metrics:?}");
        assert_eq!(metrics.krpc_errors, 1);
        assert_eq!(metrics.valid_responses, 1);
    }

    #[tokio::test]
    async fn walk_metrics_count_an_uncorrelated_response() {
        // Correct source, wrong transaction id: the datagram arrives (so it is not a timeout)
        // but fails correlation, so it contributes no peers and is bucketed as uncorrelated.
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let seed = v4_addr(&server);
        let handle = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, peer) = server.recv_from(&mut buf).await.unwrap();
            let mut wrong = buf_txid(&buf[..n]);
            wrong.push(0xff);
            let resp = response_with(&wrong, &["10.0.0.1:1111".parse().unwrap()], &[]);
            server.send_to(&resp, peer).await.unwrap();
        });

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let metrics = dht_walk_from_seeds(
            &[9u8; 20],
            Duration::from_secs(5),
            &client,
            vec![seed],
            |_src, _resp| false,
        )
        .await;

        assert_eq!(metrics.valid_responses, 0);
        assert_eq!(metrics.peers_discovered, 0);
        assert_eq!(metrics.uncorrelated_responses, 1);
        assert_eq!(metrics.timeouts, 0);
        handle.await.unwrap();
    }

    // --- Bootstrap dead-end recovery (#494) ---

    /// The KRPC method name (`q`) of a query datagram.
    fn query_method(buf: &[u8]) -> Vec<u8> {
        Bencode::parse(buf)
            .ok()
            .and_then(|v| v.get(b"q").and_then(Bencode::as_bytes).map(<[u8]>::to_vec))
            .unwrap_or_default()
    }

    #[test]
    fn find_node_query_roundtrips() {
        let q = build_find_node(&[1u8; 20], &[2u8; 20], b"ab");
        let v = Bencode::parse(&q).unwrap();
        assert_eq!(
            v.get(b"y").and_then(Bencode::as_bytes),
            Some(b"q".as_slice())
        );
        assert_eq!(query_method(&q), b"find_node");
        assert_eq!(
            v.get(b"t").and_then(Bencode::as_bytes),
            Some(b"ab".as_slice())
        );
        assert_eq!(
            v.get(b"v").and_then(Bencode::as_bytes),
            Some(CLIENT_VERSION)
        );
        let a = v.get(b"a").unwrap();
        assert_eq!(
            a.get(b"id").and_then(Bencode::as_bytes),
            Some([1u8; 20].as_slice())
        );
        assert_eq!(
            a.get(b"target").and_then(Bencode::as_bytes),
            Some([2u8; 20].as_slice())
        );
        assert!(a.get(b"info_hash").is_none());
    }

    #[tokio::test]
    async fn bootstrap_resolution_keeps_distinct_ipv4_addresses_up_to_the_cap() {
        let resolver = Resolver::controlled(|_| {
            Ok(vec![
                "[::1]:6881".parse().unwrap(),
                "10.0.0.1:6881".parse().unwrap(),
                "10.0.0.1:6881".parse().unwrap(),
                "10.0.0.2:6881".parse().unwrap(),
                "10.0.0.3:6881".parse().unwrap(),
            ])
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let addrs =
            resolve_bootstrap("router.invalid:6881".into(), resolver.clone(), deadline).await;
        resolver.finish_controlled().await;
        let expected: Vec<SocketAddrV4> = vec![
            "10.0.0.1:6881".parse().unwrap(),
            "10.0.0.2:6881".parse().unwrap(),
        ];
        assert_eq!(addrs, expected);
    }

    #[tokio::test]
    async fn every_address_behind_a_bootstrap_host_is_seeded() {
        // A router behind two addresses: the first drops every query, the second answers.
        // Asking only the first address would lose the router outright.
        let dead = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let live = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addrs = vec![
            SocketAddr::V4(v4_addr(&dead)),
            SocketAddr::V4(v4_addr(&live)),
        ];
        let peer: SocketAddrV4 = "10.0.0.9:9999".parse().unwrap();
        let server = answer_once(live, move |txid| response_with(txid, &[peer], &[]));
        let resolver = Resolver::controlled(move |_| Ok(addrs.clone()));
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut found = false;
        let metrics = dht_walk_with_bootstraps(
            &[0; 20],
            Duration::from_secs(2),
            &socket,
            &["router.invalid:1"],
            resolver.clone(),
            None,
            |_, response| {
                found |= response.peers.contains(&peer);
                found
            },
        )
        .await;
        server.abort();
        resolver.finish_controlled().await;
        drop(dead);
        assert!(
            found,
            "second address behind the bootstrap host was never asked"
        );
        assert_eq!(metrics.bootstrap_seeded, 2);
        assert_eq!(metrics.recovery_queries, 0);
    }

    /// The KRPC methods a [`scripted_router`] was asked, in arrival order.
    type AskedLog = std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>;

    /// A bootstrap router fixture: answers each query it receives with `reply(n, method, txid)`
    /// (`n` counts queries from zero), or stays silent when that returns `None`. Records the
    /// methods it was asked.
    fn scripted_router(
        server: UdpSocket,
        reply: impl Fn(usize, &[u8], &[u8]) -> Option<Vec<u8>> + Send + 'static,
    ) -> (tokio::task::JoinHandle<()>, AskedLog) {
        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = asked.clone();
        let handle = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            for n in 0.. {
                let Ok((len, caller)) = server.recv_from(&mut buf).await else {
                    return;
                };
                let method = query_method(&buf[..len]);
                log.lock().unwrap().push(method.clone());
                if let Some(resp) = reply(n, &method, &buf_txid(&buf[..len])) {
                    let _ = server.send_to(&resp, caller).await;
                }
            }
        });
        (handle, asked)
    }

    #[tokio::test]
    async fn dead_end_walk_recovers_by_widening_at_an_answering_router() {
        // The failure from #494: the router answers, but its only referral is silent, so the
        // frontier runs dry after one hop. The recovery `find_node` gets a fresh referral
        // that does hold peers.
        let target = [0u8; 20];
        let sockets = bind_loopback(3).await;
        let addrs: Vec<SocketAddrV4> = sockets.iter().map(v4_addr).collect();
        let (router, silent_referral, good_referral) = (addrs[0], addrs[1], addrs[2]);
        let mut sockets = sockets.into_iter();
        let router_sock = sockets.next().unwrap();
        let silent_sock = sockets.next().unwrap();
        let good_sock = sockets.next().unwrap();
        let peer: SocketAddrV4 = "10.0.0.7:7777".parse().unwrap();
        let (router_task, asked) = scripted_router(router_sock, move |_, method, txid| {
            let referral = if method == b"find_node" {
                ([0x60; 20], good_referral)
            } else {
                ([0x40; 20], silent_referral)
            };
            Some(response_with(txid, &[], &[referral]))
        });
        let good_task = answer_once(good_sock, move |txid| response_with(txid, &[peer], &[]));

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let start = Instant::now();
        let mut found = false;
        let metrics = dht_walk_from_bootstrap_seeds(
            &target,
            Duration::from_secs(10),
            &client,
            vec![router],
            None,
            |_, response| {
                found |= response.peers.contains(&peer);
                found
            },
        )
        .await;
        let elapsed = start.elapsed();
        router_task.abort();
        good_task.abort();
        drop(silent_sock);
        assert!(
            found,
            "recovery round did not rescue the dead-end walk: {metrics:?}"
        );
        assert_eq!(
            *asked.lock().unwrap(),
            vec![b"get_peers".to_vec(), b"find_node".to_vec()]
        );
        assert_eq!(metrics.recovery_queries, 1);
        assert_eq!(metrics.recovery_peers, 1);
        assert_eq!(metrics.nodes_queried, 4);
        assert!(!metrics.frontier_exhausted);
        // One slot timeout for the silent referral, then network speed.
        assert!(
            elapsed < SLOT_TIMEOUT + Duration::from_millis(500),
            "took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn silent_router_is_asked_once_more_with_a_fresh_transaction() {
        // A router that dropped the first query (loss, rate limiting) gets one fresh
        // `get_peers` and answers it.
        let router_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let router = v4_addr(&router_sock);
        let peer: SocketAddrV4 = "10.0.0.8:8888".parse().unwrap();
        let txids = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = txids.clone();
        let (router_task, asked) = scripted_router(router_sock, move |n, _, txid| {
            seen.lock().unwrap().push(txid.to_vec());
            (n == 1).then(|| response_with(txid, &[peer], &[]))
        });
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut found = false;
        let metrics = dht_walk_from_bootstrap_seeds(
            &[0; 20],
            Duration::from_secs(10),
            &client,
            vec![router],
            None,
            |_, response| {
                found |= response.peers.contains(&peer);
                found
            },
        )
        .await;
        router_task.abort();
        assert!(
            found,
            "re-asked router's answer was not accepted: {metrics:?}"
        );
        assert_eq!(
            *asked.lock().unwrap(),
            vec![b"get_peers".to_vec(), b"get_peers".to_vec()]
        );
        let txids = txids.lock().unwrap();
        assert_ne!(
            txids[0], txids[1],
            "re-query reused the first transaction id"
        );
        assert_eq!(metrics.recovery_queries, 1);
        assert_eq!(metrics.timeouts, 1);
    }

    #[tokio::test]
    async fn recovery_runs_once_and_a_truly_dead_walk_still_ends() {
        // Every router and referral silent: one recovery query per bootstrap, then the walk
        // ends exhausted well inside its budget instead of retrying forever.
        let sockets = bind_loopback(2).await;
        let seeds: Vec<SocketAddrV4> = sockets.iter().map(v4_addr).collect();
        let mut tasks = Vec::new();
        let mut logs = Vec::new();
        for sock in sockets {
            let (task, asked) = scripted_router(sock, |_, _, _| None);
            tasks.push(task);
            logs.push(asked);
        }
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let start = Instant::now();
        let metrics = dht_walk_from_bootstrap_seeds(
            &[0; 20],
            Duration::from_secs(10),
            &client,
            seeds,
            None,
            |_, _| false,
        )
        .await;
        let elapsed = start.elapsed();
        for task in tasks {
            task.abort();
        }
        for asked in logs {
            assert_eq!(
                asked.lock().unwrap().len(),
                2,
                "each router is asked exactly twice"
            );
        }
        assert_eq!(metrics.recovery_queries, 2);
        assert_eq!(metrics.nodes_queried, 4);
        assert_eq!(metrics.timeouts, 4);
        assert!(metrics.frontier_exhausted);
        assert!(
            elapsed < 2 * SLOT_TIMEOUT + Duration::from_millis(500),
            "took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn spoofed_traffic_cannot_mark_a_router_answered_or_add_queries() {
        // A spoofer floods the client with well-formed responses naming the router's txid
        // space and a referral of its own. None correlates, so the router still counts as
        // unanswered (re-asked with `get_peers`, not widened), and the spoofed referral is
        // never queried.
        let router_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let router = v4_addr(&router_sock);
        let lure = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let lure_addr = v4_addr(&lure);
        let lure_log = scripted_router(lure, |_, _, _| None);
        let (router_task, asked) = scripted_router(router_sock, |_, _, _| None);
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_addr = v4_addr(&client);
        let spoofer = tokio::spawn(async move {
            let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            for i in 0u16..64 {
                let resp = response_with(&i.to_be_bytes(), &[], &[([0x11; 20], lure_addr)]);
                let _ = sock.send_to(&resp, client_addr).await;
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        });
        let metrics = dht_walk_from_bootstrap_seeds(
            &[0; 20],
            Duration::from_secs(10),
            &client,
            vec![router],
            None,
            |_, _| false,
        )
        .await;
        spoofer.abort();
        router_task.abort();
        lure_log.0.abort();
        assert_eq!(
            *asked.lock().unwrap(),
            vec![b"get_peers".to_vec(), b"get_peers".to_vec()]
        );
        assert!(
            lure_log.1.lock().unwrap().is_empty(),
            "spoofed referral was queried"
        );
        assert_eq!(metrics.valid_responses, 0);
        assert_eq!(metrics.nodes_queried, 2);
        assert_eq!(metrics.recovery_queries, 1);
        assert!(metrics.uncorrelated_responses > 0);
    }

    #[tokio::test]
    async fn walk_that_runs_dry_holding_peers_skips_the_recovery_round() {
        // A sparse swarm: the router hands over one peer (below the target) and nothing
        // else. The walk already has a result, so it ends at once instead of asking again.
        let router_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let router = v4_addr(&router_sock);
        let peer: SocketAddrV4 = "10.0.0.6:6666".parse().unwrap();
        let (router_task, asked) = scripted_router(router_sock, move |_, _, txid| {
            Some(response_with(txid, &[peer], &[]))
        });
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let start = Instant::now();
        let metrics = dht_walk_from_bootstrap_seeds(
            &[0; 20],
            Duration::from_secs(10),
            &client,
            vec![router],
            None,
            |_, _| false,
        )
        .await;
        let elapsed = start.elapsed();
        router_task.abort();
        assert_eq!(asked.lock().unwrap().len(), 1);
        assert_eq!(metrics.peers_discovered, 1);
        assert_eq!(metrics.recovery_queries, 0);
        assert!(metrics.frontier_exhausted);
        assert!(elapsed < Duration::from_millis(300), "took {elapsed:?}");
    }

    #[tokio::test]
    async fn walk_that_reaches_its_deadline_sends_no_recovery_round() {
        // The silent router's slot outlives the budget, so the walk ends on its deadline and
        // never runs dry inside it: nothing more may be sent.
        let router_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let router = v4_addr(&router_sock);
        let (router_task, asked) = scripted_router(router_sock, |_, _, _| None);
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let metrics = dht_walk_from_bootstrap_seeds(
            &[0; 20],
            SLOT_TIMEOUT - Duration::from_millis(500),
            &client,
            vec![router],
            None,
            |_, _| false,
        )
        .await;
        router_task.abort();
        assert_eq!(asked.lock().unwrap().len(), 1);
        assert_eq!(metrics.recovery_queries, 0);
        assert!(!metrics.frontier_exhausted);
    }

    // --- Recovery seeds from recently answering nodes (#522) ---

    fn addr(last: u8) -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::new(10, 0, 1, last), 6881)
    }

    #[test]
    fn node_cache_keeps_newest_first_without_duplicates_up_to_the_cap() {
        let cache = NodeCache::new();
        let now = Instant::now();
        cache.record(&[addr(1), addr(2)], now);
        cache.record(&[addr(3), addr(1)], now);
        assert_eq!(
            cache.fresh(now, usize::MAX, |_| false),
            vec![addr(3), addr(1), addr(2)]
        );
        let many: Vec<SocketAddrV4> = (0..=NODE_CACHE_CAP as u8 + 4).map(addr).collect();
        cache.record(&many, now);
        let kept = cache.fresh(now, usize::MAX, |_| false);
        assert_eq!(kept.len(), NODE_CACHE_CAP);
        assert_eq!(kept[0], addr(0));
        assert_eq!(
            cache.fresh(now, 2, |a| *a == addr(0)),
            vec![addr(1), addr(2)]
        );
    }

    #[test]
    fn node_cache_drops_entries_older_than_the_max_age() {
        let cache = NodeCache::new();
        let then = Instant::now();
        cache.record(&[addr(1)], then);
        cache.record(&[addr(2)], then + Duration::from_secs(60));
        let later = then + NODE_CACHE_MAX_AGE + Duration::from_secs(1);
        assert_eq!(cache.fresh(later, usize::MAX, |_| false), vec![addr(2)]);
        assert_eq!(cache.nodes.lock().unwrap().len(), 1, "expired entry kept");
    }

    #[test]
    fn answered_prefers_nodes_that_returned_peers_and_is_bounded() {
        let mut answered = Answered::default();
        for i in 0..20 {
            answered.note(addr(i), i % 2 == 1);
        }
        answered.note(addr(1), true);
        let out = answered.into_newest_first();
        assert_eq!(out.len(), NODE_CACHE_PER_WALK);
        assert!(out.iter().all(|a| a.ip().octets()[3] % 2 == 1), "{out:?}");
    }

    #[test]
    fn recovery_targets_stay_within_max_active_and_reserve_cached_slots() {
        let routers: Vec<BootstrapNode> = (0..8)
            .map(|i| BootstrapNode {
                addr: addr(100 + i),
                answered: i == 7,
            })
            .collect();
        let cached: Vec<SocketAddrV4> = (0..8).map(addr).collect();
        let targets = recovery_targets(&routers, cached.clone());
        assert_eq!(targets.len(), MAX_ACTIVE);
        // The router that answered comes first and is widened with find_node.
        assert_eq!(targets[0], (addr(107), RecoveryQuery::FindNode));
        let from_cache = targets.iter().filter(|(a, _)| cached.contains(a)).count();
        assert_eq!(from_cache, RECOVERY_CACHED_MIN);
        // Few routers: the cache fills what they leave free.
        let targets = recovery_targets(&routers[..2], cached.clone());
        assert_eq!(targets.len(), MAX_ACTIVE);
        assert!(targets[2..]
            .iter()
            .all(|(_, q)| *q == RecoveryQuery::GetPeers));
        // No cache: identical to the #494 round.
        let targets = recovery_targets(&routers[..3], Vec::new());
        assert_eq!(targets.len(), 3);
    }

    #[tokio::test]
    async fn silent_routers_recover_through_a_remembered_node() {
        // Routers that stay silent even when asked again, but a node that answered an
        // earlier walk is still up and points at the swarm.
        let router_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let router = v4_addr(&router_sock);
        let (router_task, router_asked) = scripted_router(router_sock, |_, _, _| None);
        let remembered_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let remembered = v4_addr(&remembered_sock);
        let peer: SocketAddrV4 = "10.0.0.5:5555".parse().unwrap();
        let (remembered_task, remembered_asked) =
            scripted_router(remembered_sock, move |_, _, txid| {
                Some(response_with(txid, &[peer], &[]))
            });
        let cache = NodeCache::new();
        cache.record(&[remembered], Instant::now());
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let start = Instant::now();
        let mut found = false;
        let metrics = dht_walk_from_bootstrap_seeds(
            &[0; 20],
            Duration::from_secs(10),
            &client,
            vec![router],
            Some(&cache),
            |_, response| {
                found |= response.peers.contains(&peer);
                found
            },
        )
        .await;
        let elapsed = start.elapsed();
        router_task.abort();
        remembered_task.abort();
        assert!(
            found,
            "remembered node did not rescue the walk: {metrics:?}"
        );
        assert_eq!(router_asked.lock().unwrap().len(), 2);
        assert_eq!(
            *remembered_asked.lock().unwrap(),
            vec![b"get_peers".to_vec()]
        );
        assert_eq!(metrics.recovery_queries, 2);
        assert_eq!(metrics.recovery_peers, 1);
        assert!(
            elapsed < SLOT_TIMEOUT + Duration::from_millis(500),
            "took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn unresolvable_routers_recover_through_a_remembered_node() {
        // Every bootstrap hostname fails to resolve, so the walk starts with nothing to ask.
        // Before #522 it ended at once with no peers.
        let remembered_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let remembered = v4_addr(&remembered_sock);
        let peer: SocketAddrV4 = "10.0.0.4:4444".parse().unwrap();
        let server = answer_once(remembered_sock, move |txid| {
            response_with(txid, &[peer], &[])
        });
        let cache = NodeCache::new();
        cache.record(&[remembered], Instant::now());
        let resolver = Resolver::controlled(|_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no such host",
            ))
        });
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut found = false;
        let metrics = dht_walk_with_bootstraps(
            &[0; 20],
            Duration::from_secs(2),
            &client,
            &["router.invalid:1"],
            resolver.clone(),
            Some(&cache),
            |_, response| {
                found |= response.peers.contains(&peer);
                found
            },
        )
        .await;
        server.abort();
        resolver.finish_controlled().await;
        assert!(found, "walk with no routers ignored the cache: {metrics:?}");
        assert_eq!(metrics.bootstrap_seeded, 0);
        assert_eq!(metrics.recovery_queries, 1);
    }

    #[tokio::test]
    async fn healthy_walk_never_asks_the_cache_but_teaches_it() {
        // The router hands over a referral that returns peers. The remembered node must not
        // be asked, and the referral that answered is remembered for later walks.
        let sockets = bind_loopback(3).await;
        let addrs: Vec<SocketAddrV4> = sockets.iter().map(v4_addr).collect();
        let (router, referral, remembered) = (addrs[0], addrs[1], addrs[2]);
        let mut sockets = sockets.into_iter();
        let peer: SocketAddrV4 = "10.0.0.3:3333".parse().unwrap();
        let (router_task, _) = scripted_router(sockets.next().unwrap(), move |_, _, txid| {
            Some(response_with(txid, &[], &[([0x20; 20], referral)]))
        });
        let (referral_task, _) = scripted_router(sockets.next().unwrap(), move |_, _, txid| {
            Some(response_with(txid, &[peer], &[]))
        });
        let (remembered_task, remembered_asked) =
            scripted_router(sockets.next().unwrap(), |_, _, _| None);
        let cache = NodeCache::new();
        cache.record(&[remembered], Instant::now());
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let metrics = dht_walk_from_bootstrap_seeds(
            &[0; 20],
            Duration::from_secs(5),
            &client,
            vec![router],
            Some(&cache),
            |_, response| response.peers.contains(&peer),
        )
        .await;
        router_task.abort();
        referral_task.abort();
        remembered_task.abort();
        assert_eq!(metrics.peers_discovered, 1);
        assert_eq!(metrics.recovery_queries, 0);
        assert!(remembered_asked.lock().unwrap().is_empty());
        assert_eq!(
            cache.fresh(Instant::now(), usize::MAX, |_| false),
            vec![referral, remembered],
            "the router is not cached, the answering referral is, newest first"
        );
    }

    #[tokio::test]
    async fn spoofed_replies_never_enter_the_cache() {
        // A flood of well-formed but uncorrelated responses during a dead walk leaves the
        // cache empty: only a reply to a query we sent can add a node.
        let router_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let router = v4_addr(&router_sock);
        let (router_task, _) = scripted_router(router_sock, |_, _, _| None);
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_addr = v4_addr(&client);
        let spoofer = tokio::spawn(async move {
            let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            for i in 0u16..64 {
                let resp = response_with(&i.to_be_bytes(), &[addr(9)], &[]);
                let _ = sock.send_to(&resp, client_addr).await;
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        });
        let cache = NodeCache::new();
        let metrics = dht_walk_from_bootstrap_seeds(
            &[0; 20],
            Duration::from_secs(10),
            &client,
            vec![router],
            Some(&cache),
            |_, _| false,
        )
        .await;
        spoofer.abort();
        router_task.abort();
        assert!(metrics.uncorrelated_responses > 0);
        assert_eq!(metrics.valid_responses, 0);
        assert!(cache
            .fresh(Instant::now(), usize::MAX, |_| false)
            .is_empty());
    }

    #[tokio::test]
    async fn remembered_node_already_asked_this_walk_is_not_asked_again() {
        // The router's referral is also in the cache. It answered without peers, so asking it
        // the same question in the recovery round would only repeat that answer.
        let sockets = bind_loopback(2).await;
        let addrs: Vec<SocketAddrV4> = sockets.iter().map(v4_addr).collect();
        let (router, referral) = (addrs[0], addrs[1]);
        let mut sockets = sockets.into_iter();
        let (router_task, _) = scripted_router(sockets.next().unwrap(), move |n, _, txid| {
            (n == 0).then(|| response_with(txid, &[], &[([0x20; 20], referral)]))
        });
        let (referral_task, referral_asked) =
            scripted_router(sockets.next().unwrap(), |_, _, txid| {
                Some(response_with(txid, &[], &[]))
            });
        let cache = NodeCache::new();
        cache.record(&[referral], Instant::now());
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let metrics = dht_walk_from_bootstrap_seeds(
            &[0; 20],
            Duration::from_secs(10),
            &client,
            vec![router],
            Some(&cache),
            |_, _| false,
        )
        .await;
        router_task.abort();
        referral_task.abort();
        assert_eq!(referral_asked.lock().unwrap().len(), 1);
        assert_eq!(
            metrics.recovery_queries, 1,
            "only the router is asked again"
        );
        assert!(metrics.frontier_exhausted);
    }

    // Live DHT lookup against a real infohash:
    //   ACE_INFOHASH=50e935...2d6e47 cargo test -p ace-swarm dht_live -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn dht_live_finds_peers() {
        let hex = std::env::var("ACE_INFOHASH").expect("set ACE_INFOHASH=40hex");
        let mut ih = [0u8; 20];
        for i in 0..20 {
            ih[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap();
        }
        let peers = dht_get_peers(&ih, std::time::Duration::from_secs(20)).await;
        println!("DHT found {} peer(s):", peers.len());
        for p in &peers {
            println!("  {p}");
        }
        assert!(!peers.is_empty(), "DHT returned no peers");
    }

    // Live check of the recovery seeds (#522): a warm-up walk with the real routers fills a
    // cache, then a second walk toward another infohash runs with the routers replaced by a
    // silent documentation address (TEST-NET-1), as on a network where none answers. Runs
    // alternate between using the cache and not. Prints one `measured` line per walk.
    //   ACE_INFOHASHES=<40hex>,<40hex>[,...] ACE_RUNS=20 \
    //     cargo test --release -p ace-swarm dht_live_recovery_cache -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn dht_live_recovery_cache_rescues_walks_without_routers() {
        let parse = |hex: &str| {
            let mut ih = [0u8; 20];
            for (i, byte) in ih.iter_mut().enumerate() {
                *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap();
            }
            ih
        };
        let hashes: Vec<[u8; 20]> = std::env::var("ACE_INFOHASHES")
            .expect("set ACE_INFOHASHES=40hex,40hex,...")
            .split(',')
            .map(|hex| parse(hex.trim()))
            .collect();
        assert!(hashes.len() >= 2, "need at least two infohashes");
        let runs: usize = std::env::var("ACE_RUNS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10);
        let mut rescued = [0usize; 2];
        for run in 0..runs {
            let warm = &hashes[run % hashes.len()];
            let measured = &hashes[(run + 1) % hashes.len()];
            let use_cache = run % 2 == 0;
            let cache = NodeCache::new();
            let socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
            let mut warm_peers = 0;
            dht_walk_with_bootstraps(
                warm,
                Duration::from_secs(20),
                &socket,
                BOOTSTRAP,
                Resolver::global(),
                Some(&cache),
                |_, response| {
                    warm_peers += response.peers.len();
                    warm_peers >= DHT_PEER_TARGET
                },
            )
            .await;
            let cached = cache.fresh(Instant::now(), usize::MAX, |_| false).len();
            let socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
            let start = Instant::now();
            let mut first_peer = None;
            let mut peers = 0;
            let metrics = dht_walk_with_bootstraps(
                measured,
                Duration::from_secs(20),
                &socket,
                &["192.0.2.1:6881"],
                Resolver::global(),
                use_cache.then_some(&cache),
                |_, response| {
                    if !response.peers.is_empty() && first_peer.is_none() {
                        first_peer = Some(start.elapsed());
                    }
                    peers += response.peers.len();
                    peers >= DHT_PEER_TARGET
                },
            )
            .await;
            if metrics.peers_discovered > 0 {
                rescued[usize::from(use_cache)] += 1;
            }
            println!(
                "measured run={run} cache={use_cache} cached={cached} warm_peers={warm_peers} \
                 peers={} queried={} valid={} recovery_queries={} first_peer_ms={} \
                 walk_ms={}",
                metrics.peers_discovered,
                metrics.nodes_queried,
                metrics.valid_responses,
                metrics.recovery_queries,
                first_peer.map_or(-1, |d| d.as_millis() as i64),
                start.elapsed().as_millis(),
            );
        }
        println!(
            "summary runs={runs} with_peers_cache={} with_peers_no_cache={}",
            rescued[1], rescued[0]
        );
    }

    // Live DHT self-announce against a real infohash — confirms we can get tokens from real
    // nodes and send announce_peer without erroring. Doesn't (can't, from one host) prove a
    // third party subsequently finds us; that's what Task 7's reverse-direction proof needs.
    //   ACE_INFOHASH=50e935...2d6e47 cargo test -p ace-swarm dht_live_announce -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn dht_live_announce_sends_without_erroring() {
        let hex = std::env::var("ACE_INFOHASH").expect("set ACE_INFOHASH=40hex");
        let mut ih = [0u8; 20];
        for i in 0..20 {
            ih[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap();
        }
        let announced = dht_announce_peer(&ih, 8621, std::time::Duration::from_secs(20)).await;
        println!("DHT announce_peer sent to {announced} node(s)");
        assert!(
            announced > 0,
            "no node handed us a token to announce back with"
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod comparison;
