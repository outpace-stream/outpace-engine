//! Bounded, private hints from authenticated contiguous producers. Never descriptor authority.
use super::CandidateKind;
use crate::private_dir::{Failure, FailureKey};
use std::collections::{BTreeMap, HashMap};
use std::io::{self, Write};
use std::net::SocketAddrV4;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TTL: u64 = 300;
const MAX_BYTES: usize = 256 * 1024;
const MAX_IDENTITIES: usize = 16;
const HEADER: &str = "OUTPACE-WARM-1\n";
// Only the unix store persists hints; other platforms keep them in memory.
#[cfg_attr(not(unix), allow(dead_code))]
const FILE: &str = "recent-peers-v1";
#[cfg_attr(not(unix), allow(dead_code))]
const LOCK_FILE: &str = "recent-peers-v1.lock";
/// Hang guard for tests that wait on the writer thread's real disk I/O (load, or a durable
/// write with two fsyncs). It bounds a stuck writer only: a slow disk is not a failure, and
/// every wait it bounds is woken by the writer itself, so success never costs this long.
#[cfg(test)]
const TEST_IO_DEADLINE: Duration = Duration::from_secs(30);
#[derive(Clone, Copy)]
struct Entry {
    addr: SocketAddrV4,
    source: bool,
    at: u64,
}
#[derive(Default)]
struct State {
    entries: BTreeMap<[u8; 20], Vec<Entry>>,
    revision: u64,
    persisted: u64,
    loaded: bool,
    flush: bool,
    stop: bool,
}
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
enum WriterObservation {
    Wait {
        timed: bool,
        dirty: bool,
        delay: Duration,
    },
    Write {
        success: bool,
    },
}
struct Shared {
    #[cfg(test)]
    observations: Mutex<Option<std::sync::mpsc::SyncSender<WriterObservation>>>,
    state: Mutex<State>,
    wake: Condvar,
    changed: tokio::sync::Notify,
    allow_loopback: bool,
}
struct Owner {
    shared: Arc<Shared>,
    identity: Option<PathBuf>,
    worker: Mutex<Option<JoinHandle<()>>>,
}
struct Slot {
    owner: Weak<Owner>,
    retired: Option<JoinHandle<()>>,
}
fn registry() -> &'static Mutex<HashMap<PathBuf, Slot>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Slot>>> = OnceLock::new();
    REGISTRY.get_or_init(Mutex::default)
}
impl Drop for Owner {
    fn drop(&mut self) {
        {
            let mut state = self.shared.state.lock().unwrap();
            state.flush = true;
            state.stop = true;
        }
        self.shared.wake.notify_one();
        if let Some(identity) = &self.identity {
            let handle = self.worker.lock().unwrap().take();
            if let Some(slot) = registry().lock().unwrap().get_mut(identity) {
                slot.retired = handle;
            }
        }
    }
}
#[derive(Clone)]
pub struct WarmPeerCache(Arc<Owner>);
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn normalize(path: &Path) -> Option<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir => result.push("/"),
            Component::Normal(p) => result.push(p),
            Component::CurDir => {}
            Component::ParentDir => {
                if !result.pop() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    Some(result)
}
fn endpoint_allowed(addr: SocketAddrV4, allow_loopback: bool) -> bool {
    let ip = addr.ip();
    let octets = ip.octets();
    addr.port() != 0
        && ((allow_loopback && ip.is_loopback())
            || !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_unspecified()
                || ip.is_multicast()
                || octets[0] == 0
                || octets[0] >= 240
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                || (octets[0] == 198 && (18..=19).contains(&octets[1]))))
}
fn sweep(slots: &mut HashMap<PathBuf, Slot>) {
    // Join only known-finished handles: a filesystem stall never blocks async teardown.
    slots.retain(|_, slot| {
        if slot.owner.strong_count() > 0 {
            return true;
        }
        match slot.retired.as_ref() {
            None => true,
            Some(handle) if !handle.is_finished() => true,
            _ => {
                if let Some(handle) = slot.retired.take() {
                    let _ = handle.join();
                }
                false
            }
        }
    });
}
impl WarmPeerCache {
    pub fn memory() -> Self {
        Self::new(Path::new(""), false)
    }
    pub fn new(path: &Path, allow_loopback: bool) -> Self {
        Self::new_inner(
            path,
            allow_loopback,
            #[cfg(test)]
            None,
        )
    }
    fn new_inner(
        path: &Path,
        allow_loopback: bool,
        #[cfg(test)] pause: Option<Arc<(Mutex<bool>, Condvar)>>,
    ) -> Self {
        let shared = Arc::new(Shared {
            #[cfg(test)]
            observations: Mutex::new(None),
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            changed: tokio::sync::Notify::new(),
            allow_loopback,
        });
        let memory = || {
            Self(Arc::new(Owner {
                shared: shared.clone(),
                identity: None,
                worker: Mutex::new(None),
            }))
        };
        if path.as_os_str().is_empty() {
            shared.state.lock().unwrap().loaded = true;
            return memory();
        }
        let Some(identity) = normalize(path) else {
            shared.state.lock().unwrap().loaded = true;
            return memory();
        };
        let mut slots = registry().lock().unwrap();
        sweep(&mut slots);
        if let Some(slot) = slots.get(&identity) {
            if let Some(owner) = slot.owner.upgrade() {
                return Self(owner);
            }
            // An unfinished retired writer owns this path until its actual terminal.
            shared.state.lock().unwrap().loaded = true;
            return memory();
        }
        if slots.len() >= MAX_IDENTITIES {
            shared.state.lock().unwrap().loaded = true;
            return memory();
        }
        let owner = Arc::new(Owner {
            shared: shared.clone(),
            identity: Some(identity.clone()),
            worker: Mutex::new(None),
        });
        let worker_path = identity.clone();
        match std::thread::Builder::new()
            .name("warm-peer-cache".into())
            .spawn(move || {
                #[cfg(test)]
                if let Some(pause) = pause {
                    let mut released = pause.0.lock().unwrap();
                    while !*released {
                        released = pause.1.wait(released).unwrap();
                    }
                }
                worker(shared, worker_path)
            }) {
            Ok(handle) => {
                *owner.worker.lock().unwrap() = Some(handle);
                slots.insert(
                    identity,
                    Slot {
                        owner: Arc::downgrade(&owner),
                        retired: None,
                    },
                );
                Self(owner)
            }
            Err(_) => {
                owner.shared.state.lock().unwrap().loaded = true;
                Self(owner)
            }
        }
    }
    /// Best-effort startup wait: returns once the initial disk load finishes or after 100 ms,
    /// whichever comes first. It does not mean the load completed; see [`Self::wait_loaded`].
    pub async fn initialized(&self) {
        self.wait_loaded(Duration::from_millis(100)).await;
    }
    /// Whether the initial disk load has finished, or this cache uses the in-memory fallback.
    /// Completion only: a failed or corrupt load also counts, and hints may still be empty.
    pub fn is_initialized(&self) -> bool {
        self.0.shared.state.lock().unwrap().loaded
    }
    /// Wait up to `deadline` for [`Self::is_initialized`]. Returns whether loading finished.
    pub async fn wait_loaded(&self, deadline: Duration) -> bool {
        tokio::time::timeout(deadline, async {
            loop {
                let notified = self.0.shared.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.is_initialized() {
                    return;
                }
                notified.await;
            }
        })
        .await
        .is_ok()
    }
    pub fn candidate_addresses(&self, hash: &[u8; 20]) -> Vec<SocketAddrV4> {
        self.hints(hash).into_iter().map(|(addr, _)| addr).collect()
    }

    pub(super) fn hints(&self, hash: &[u8; 20]) -> Vec<(SocketAddrV4, CandidateKind)> {
        let mut state = self.0.shared.state.lock().unwrap();
        prune(&mut state.entries, now());
        let mut entries = state.entries.get(hash).cloned().unwrap_or_default();
        entries.sort_by_key(|entry| (!entry.source, std::cmp::Reverse(entry.at)));
        entries
            .into_iter()
            .map(|entry| {
                (
                    entry.addr,
                    if entry.source {
                        CandidateKind::Source
                    } else {
                        CandidateKind::Discovered
                    },
                )
            })
            .collect()
    }
    pub(super) fn record_productive(
        &self,
        hash: [u8; 20],
        addr: SocketAddrV4,
        kind: CandidateKind,
    ) {
        if !endpoint_allowed(addr, self.0.shared.allow_loopback) {
            return;
        }
        let mut state = self.0.shared.state.lock().unwrap();
        insert(
            &mut state.entries,
            hash,
            Entry {
                addr,
                source: kind == CandidateKind::Source,
                at: now(),
            },
        );
        state.revision = state.revision.wrapping_add(1);
        drop(state);
        self.0.shared.wake.notify_one();
    }
    pub(super) fn request_flush(&self) {
        self.0.shared.state.lock().unwrap().flush = true;
        self.0.shared.wake.notify_one();
    }
    #[cfg(test)]
    pub(super) async fn flush(&self) -> bool {
        let revision = {
            let mut state = self.0.shared.state.lock().unwrap();
            state.flush = true;
            state.revision
        };
        self.0.shared.wake.notify_one();
        tokio::time::timeout(TEST_IO_DEADLINE, async {
            loop {
                let notified = self.0.shared.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.0.shared.state.lock().unwrap().persisted >= revision {
                    break;
                }
                notified.await;
            }
        })
        .await
        .is_ok()
    }
}
fn prune(entries: &mut BTreeMap<[u8; 20], Vec<Entry>>, time: u64) {
    entries.retain(|_, peers| {
        peers.retain(|peer| peer.at <= time && time - peer.at <= TTL);
        !peers.is_empty()
    });
}
fn insert(entries: &mut BTreeMap<[u8; 20], Vec<Entry>>, hash: [u8; 20], mut entry: Entry) {
    prune(entries, now());
    let previous = entries
        .get(&hash)
        .and_then(|peers| peers.iter().find(|old| old.addr == entry.addr))
        .copied();
    if let Some(old) = previous {
        if old.at > entry.at {
            return;
        }
        entry.source |= old.source;
    }
    if !entries.contains_key(&hash) && entries.len() >= 256 {
        if let Some(old) = entries
            .iter()
            .min_by_key(|(_, peers)| peers.iter().map(|p| p.at).max().unwrap_or(0))
            .map(|(hash, _)| *hash)
        {
            entries.remove(&old);
        }
    }
    if previous.is_none() && entries.values().map(Vec::len).sum::<usize>() >= 3000 {
        if let Some((old_hash, index)) = entries
            .iter()
            .flat_map(|(hash, peers)| {
                peers
                    .iter()
                    .enumerate()
                    .map(move |(index, entry)| (*hash, index, entry.at))
            })
            .min_by_key(|(_, _, at)| *at)
            .map(|(hash, index, _)| (hash, index))
        {
            entries.get_mut(&old_hash).unwrap().remove(index);
            if entries[&old_hash].is_empty() {
                entries.remove(&old_hash);
            }
        }
    }
    let peers = entries.entry(hash).or_default();
    if let Some(index) = peers.iter().position(|old| old.addr == entry.addr) {
        // Refreshes do not consume another row. Apply source upgrades through the same cap.
        peers.remove(index);
    }
    if entry.source && peers.iter().filter(|peer| peer.source).count() >= 8 {
        if let Some(index) = peers
            .iter()
            .enumerate()
            .filter(|(_, peer)| peer.source)
            .min_by_key(|(_, peer)| peer.at)
            .map(|(i, _)| i)
        {
            peers.remove(index);
        }
    }
    if peers.len() >= 16 {
        let index = peers
            .iter()
            .enumerate()
            .min_by_key(|(_, peer)| peer.at)
            .unwrap()
            .0;
        peers.remove(index);
    }
    peers.push(entry);
}
fn encode(entries: &BTreeMap<[u8; 20], Vec<Entry>>) -> Vec<u8> {
    let mut bytes = HEADER.as_bytes().to_vec();
    for (hash, peers) in entries {
        for peer in peers {
            writeln!(
                &mut bytes,
                "{} {} {} {} {}",
                hex::encode(hash),
                peer.at,
                if peer.source { "S" } else { "D" },
                peer.addr.ip(),
                peer.addr.port()
            )
            .unwrap();
        }
    }
    bytes
}
fn parse(bytes: &[u8], allow_loopback: bool) -> io::Result<BTreeMap<[u8; 20], Vec<Entry>>> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid warm cache");
    if bytes.len() > MAX_BYTES || !bytes.starts_with(HEADER.as_bytes()) {
        return Err(invalid());
    }
    let mut entries = BTreeMap::<[u8; 20], Vec<Entry>>::new();
    let mut rows = 0;
    let time = now();
    for line in bytes[HEADER.len()..].split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        if line.len() > 128 || rows >= 3000 {
            return Err(invalid());
        }
        rows += 1;
        let row = std::str::from_utf8(line).map_err(|_| invalid())?;
        let mut fields = row.split(' ');
        let mut hash = [0; 20];
        let encoded = fields.next().ok_or_else(invalid)?;
        if encoded.len() != 40 || hex::decode_to_slice(encoded, &mut hash).is_err() {
            return Err(invalid());
        }
        let at = fields
            .next()
            .ok_or_else(invalid)?
            .parse::<u64>()
            .map_err(|_| invalid())?;
        let source = match fields.next() {
            Some("S") => true,
            Some("D") => false,
            _ => return Err(invalid()),
        };
        let ip = fields
            .next()
            .ok_or_else(invalid)?
            .parse()
            .map_err(|_| invalid())?;
        let port = fields
            .next()
            .ok_or_else(invalid)?
            .parse()
            .map_err(|_| invalid())?;
        let addr = SocketAddrV4::new(ip, port);
        if fields.next().is_some()
            || at > time
            || time - at > TTL
            || !endpoint_allowed(addr, allow_loopback)
        {
            return Err(invalid());
        }
        if !entries.contains_key(&hash) && entries.len() >= 256 {
            return Err(invalid());
        }
        let peers = entries.entry(hash).or_default();
        if peers.len() >= 16
            || peers.iter().any(|entry| entry.addr == addr)
            || (source && peers.iter().filter(|entry| entry.source).count() >= 8)
        {
            return Err(invalid());
        }
        peers.push(Entry { addr, source, at });
    }
    Ok(entries)
}
/// Reports each distinct failure once: a retry that fails the same way stays silent, and a
/// successful write re-arms reporting.
#[derive(Default)]
struct FailureLog {
    last: Option<FailureKey>,
}
impl FailureLog {
    /// Whether `failure` differs from the last reported one; records it if so.
    fn first(&mut self, failure: &Failure) -> bool {
        let key = failure.key();
        if self.last == Some(key) {
            return false;
        }
        self.last = Some(key);
        true
    }
    fn succeeded(&mut self) {
        self.last = None;
    }
}
fn report(failure: &Failure) {
    let errno = failure
        .error
        .raw_os_error()
        .map_or_else(|| "none".to_owned(), |errno| errno.to_string());
    let path = failure
        .path
        .as_ref()
        .map(|path| format!(" ({})", path.display()))
        .unwrap_or_default();
    crate::alog!(
        "[ace] warm peer cache I/O failed at {}{path}: {} (kind {:?}, errno {errno}); streaming continues without durable hints, and repeats of this error are not logged",
        failure.stage,
        failure.error,
        failure.error.kind(),
    );
}
#[cfg(test)]
fn observe_writer(shared: &Shared, event: WriterObservation) {
    if let Some(observer) = &*shared.observations.lock().unwrap() {
        let _ = observer.try_send(event);
    }
}
fn worker(shared: Arc<Shared>, path: PathBuf) {
    // Resolve operator-given ancestor symlinks once, here, before the strict no-follow walk.
    let directory = secure::Directory::open(&secure::resolve_ancestors(&path));
    let mut failures = FailureLog::default();
    let loaded = directory
        .as_ref()
        .ok()
        .and_then(|directory| directory.read().ok())
        .and_then(|bytes| parse(&bytes, shared.allow_loopback).ok());
    {
        let mut state = shared.state.lock().unwrap();
        if let Some(entries) = loaded {
            for (hash, peers) in entries {
                for entry in peers {
                    insert(&mut state.entries, hash, entry);
                }
            }
        }
        state.loaded = true;
    }
    shared.changed.notify_waiters();
    let mut last_write = Instant::now()
        .checked_sub(Duration::from_secs(5))
        .unwrap_or_else(Instant::now);
    loop {
        let (snapshot, revision, stop) = {
            let mut state = shared.state.lock().unwrap();
            while !state.stop {
                if state.revision == state.persisted {
                    #[cfg(test)]
                    observe_writer(
                        &shared,
                        WriterObservation::Wait {
                            timed: false,
                            dirty: false,
                            delay: Duration::ZERO,
                        },
                    );
                    state = shared.wake.wait(state).unwrap();
                } else if !state.flush && last_write.elapsed() < Duration::from_secs(5) {
                    let delay = Duration::from_secs(5).saturating_sub(last_write.elapsed());
                    #[cfg(test)]
                    observe_writer(
                        &shared,
                        WriterObservation::Wait {
                            timed: true,
                            dirty: true,
                            delay,
                        },
                    );
                    state = shared.wake.wait_timeout(state, delay).unwrap().0;
                } else {
                    break;
                }
            }
            prune(&mut state.entries, now());
            state.flush = false;
            (encode(&state.entries), state.revision, state.stop)
        };
        let written;
        let failure = match &directory {
            Ok(directory) => {
                written = directory.write(&snapshot);
                written.as_ref().err()
            }
            // A failed open is reported on the first write, then deduplicated like any other.
            Err(failure) => Some(failure),
        };
        let success = failure.is_none();
        last_write = Instant::now();
        #[cfg(test)]
        observe_writer(&shared, WriterObservation::Write { success });
        match failure {
            None => {
                failures.succeeded();
                shared.state.lock().unwrap().persisted = revision;
                shared.changed.notify_waiters();
            }
            Some(failure) => {
                if failures.first(failure) {
                    report(failure);
                }
            }
        }
        if stop {
            break;
        }
        if !success {
            // Failed writes retry only after coalescing interval, never a hot spin.
            let state = shared.state.lock().unwrap();
            if !state.stop {
                #[cfg(test)]
                observe_writer(
                    &shared,
                    WriterObservation::Wait {
                        timed: true,
                        dirty: true,
                        delay: Duration::from_secs(5),
                    },
                );
                let _ = shared.wake.wait_timeout(state, Duration::from_secs(5));
            }
        }
    }
}

/// The warm-peer file inside its private directory; I/O hardening lives in
/// [`crate::private_dir`].
mod secure {
    use super::*;
    pub(super) use crate::private_dir::resolve_ancestors;
    pub(super) struct Directory(crate::private_dir::PrivateDir);
    impl Directory {
        pub(super) fn open(path: &Path) -> Result<Self, Failure> {
            crate::private_dir::PrivateDir::open(path, LOCK_FILE).map(Self)
        }
        pub(super) fn read(&self) -> io::Result<Vec<u8>> {
            self.0.read(FILE, MAX_BYTES)
        }
        pub(super) fn write(&self, bytes: &[u8]) -> Result<(), Failure> {
            self.0.write(FILE, bytes, MAX_BYTES)
        }
    }
}

/// Fixtures that assert on persisted contents need the load to have actually finished, not the
/// best-effort 100 ms of [`WarmPeerCache::initialized`].
#[cfg(test)]
pub(super) async fn fixture_loaded(cache: &WarmPeerCache) {
    assert!(
        cache.wait_loaded(TEST_IO_DEADLINE).await,
        "cache fixture initial load must finish"
    );
}

#[cfg(test)]
pub(super) async fn retired_terminal(path: &Path) -> bool {
    let identity = normalize(path).unwrap();
    tokio::time::timeout(TEST_IO_DEADLINE, async {
        loop {
            {
                let mut slots = registry().lock().unwrap();
                sweep(&mut slots);
                if !slots.contains_key(&identity) {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

#[cfg(test)]
pub(super) async fn test_guard() -> tokio::sync::MutexGuard<'static, ()> {
    static GUARD: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    GUARD
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    async fn writer_observation(
        receiver: &std::sync::mpsc::Receiver<WriterObservation>,
    ) -> WriterObservation {
        tokio::time::timeout(TEST_IO_DEADLINE, async {
            loop {
                if let Ok(event) = receiver.try_recv() {
                    return event;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned writer must reach the observed state")
    }
    fn observed_cache(
        path: &Path,
    ) -> (WarmPeerCache, std::sync::mpsc::Receiver<WriterObservation>) {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let cache = WarmPeerCache::new_inner(path, true, Some(gate.clone()));
        let (sender, receiver) = std::sync::mpsc::sync_channel(16);
        *cache.0.shared.observations.lock().unwrap() = Some(sender);
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        (cache, receiver)
    }
    #[tokio::test]
    async fn idle_writer_uses_untimed_wait_and_dirty_flush_stop_wake_it() {
        let _guard = test_guard().await;
        let path = temp();
        let (cache, events) = observed_cache(&path);
        fixture_loaded(&cache).await;
        let idle = writer_observation(&events).await;
        cache.record_productive([0; 20], peer(34561), CandidateKind::Source);
        assert!(
            cache.flush().await,
            "dirty notification/flush must persist actual revision"
        );
        cache.record_productive([0; 20], peer(34562), CandidateKind::Discovered);
        assert!(
            cache.flush().await,
            "a new revision must wake the idle owner again"
        );
        drop(cache);
        assert!(
            retired_terminal(&path).await,
            "stop must wake and join the owned writer"
        );
        let entries = parse(&std::fs::read(path.join(FILE)).unwrap(), true).unwrap();
        std::fs::remove_dir_all(path).unwrap();
        assert_eq!(entries[&[0; 20]].len(), 2);
        assert!(
            matches!(
                idle,
                WriterObservation::Wait {
                    timed: false,
                    dirty: false,
                    ..
                }
            ),
            "clean writer entered timed polling instead of notification wait: {idle:?}"
        );
    }
    #[tokio::test]
    async fn dirty_writer_retry_delay_survives_notification_and_flush_stop() {
        let _guard = test_guard().await;
        let path = temp();
        std::fs::create_dir_all(path.join(FILE)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (cache, events) = observed_cache(&path);
        fixture_loaded(&cache).await;
        let _ = writer_observation(&events).await;
        cache.record_productive([0; 20], peer(34563), CandidateKind::Source);
        loop {
            if matches!(
                writer_observation(&events).await,
                WriterObservation::Write { success: false }
            ) {
                break;
            }
        }
        let retry = writer_observation(&events).await;
        cache.record_productive([0; 20], peer(34564), CandidateKind::Discovered);
        let dirty_wait = writer_observation(&events).await;
        std::fs::remove_dir(path.join(FILE)).unwrap();
        assert!(
            cache.flush().await,
            "explicit flush must wake retry and persist both revisions"
        );
        drop(cache);
        assert!(retired_terminal(&path).await);
        let entries = parse(&std::fs::read(path.join(FILE)).unwrap(), true).unwrap();
        std::fs::remove_dir_all(path).unwrap();
        assert_eq!(entries[&[0; 20]].len(), 2);
        for wait in [retry, dirty_wait] {
            assert!(
                matches!(wait,WriterObservation::Wait{timed:true,dirty:true,delay} if delay>Duration::ZERO && delay<=Duration::from_secs(5)),
                "dirty retry lost its coalescing interval: {wait:?}"
            );
        }
    }
    fn temp() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "outpace-warm-file-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }
    fn peer(port: u16) -> SocketAddrV4 {
        SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, port)
    }
    fn valid() -> Vec<u8> {
        encode(&BTreeMap::from([(
            [0; 20],
            vec![Entry {
                addr: peer(23456),
                source: true,
                at: now(),
            }],
        )]))
    }
    fn private_file(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    #[test]
    fn bounded_parser_rejects_corrupt_expired_future_poison_and_excess_rows() {
        assert_eq!(parse(&valid(), true).unwrap()[&[0; 20]].len(), 1);
        assert!(
            parse(&valid(), false).is_err(),
            "default policy must reject loopback hints"
        );
        let mut malformed = vec![b'x'; MAX_BYTES + 1];
        assert!(parse(&malformed, true).is_err());
        malformed = valid();
        malformed[0] = b'x';
        assert!(parse(&malformed, true).is_err());
        let row =
            |at: u64, port: u16| format!("{} {at} S 127.0.0.1 {port}\n", hex::encode([0; 20]));
        for suffix in [
            row(now() + 1, 23456),
            row(now() - TTL - 1, 23456),
            row(now(), 0),
            "x".repeat(129),
        ] {
            assert!(parse(format!("{HEADER}{suffix}").as_bytes(), true).is_err());
        }
        let duplicate = format!("{HEADER}{}{}", row(now(), 23456), row(now(), 23456));
        assert!(parse(duplicate.as_bytes(), true).is_err());
        let sources = format!(
            "{HEADER}{}",
            (1..=9).map(|port| row(now(), port)).collect::<String>()
        );
        assert!(parse(sources.as_bytes(), true).is_err());
        let peers = format!(
            "{HEADER}{}",
            (1..=17)
                .map(|port| row(now(), port).replace(" S ", " D "))
                .collect::<String>()
        );
        assert!(parse(peers.as_bytes(), true).is_err());
        let mut swarms = BTreeMap::new();
        for hash in 0..257u16 {
            let mut key = [0; 20];
            key[..2].copy_from_slice(&hash.to_be_bytes());
            swarms.insert(
                key,
                vec![Entry {
                    addr: peer(23456),
                    source: false,
                    at: now(),
                }],
            );
        }
        assert!(
            parse(&encode(&swarms), true).is_err(),
            "257 swarms must fail before allocating another row"
        );
        assert!(
            parse(
                valid().as_slice().strip_prefix(HEADER.as_bytes()).unwrap(),
                true
            )
            .is_err(),
            "header/version is required"
        );
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn cache_persists_through_execute_only_parent() {
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipping execute-only parent test: root bypasses directory permissions");
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("search-only");
        let existing = parent.join("existing");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o111)).unwrap();
        let result = (|| -> io::Result<()> {
            assert_eq!(
                std::fs::read_dir(&parent).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied,
                "fixture must deny directory reads"
            );
            // Also exercise mkdirat through a search-only ancestor.
            for path in [existing.clone(), existing.join("new").join("cache")] {
                // One snapshot: `valid()` stamps the current second, which a slow fsync can cross.
                let snapshot = valid();
                let directory = secure::Directory::open(&path)?;
                directory.write(&snapshot)?;
                assert_eq!(directory.read()?, snapshot);
                drop(directory);
                let reopened = secure::Directory::open(&path)?;
                assert_eq!(reopened.read()?, snapshot);
            }
            Ok(())
        })();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
        result.unwrap();
    }
    #[test]
    fn cache_inode_and_directory_rejections_are_nonblocking() {
        let root = temp();
        let directory = secure::Directory::open(&root).unwrap();
        let target = root.join(FILE);
        let outside = root.join("outside");
        private_file(&outside, b"private original");
        symlink(&outside, &target).unwrap();
        assert!(directory.read().is_err());
        assert!(directory.write(&valid()).is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"private original");
        std::fs::remove_file(&target).unwrap();
        std::fs::hard_link(&outside, &target).unwrap();
        assert!(directory.read().is_err());
        assert!(directory.write(&valid()).is_err());
        std::fs::remove_file(&target).unwrap();
        private_file(&target, &valid());
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(directory.read().is_err());
        assert!(directory.write(&valid()).is_err());
        std::fs::remove_file(&target).unwrap();
        let name = std::ffi::CString::new(target.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let start = Instant::now();
        assert!(directory.read().is_err());
        assert!(directory.write(&valid()).is_err());
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "FIFO must never block cache access"
        );
        std::fs::remove_file(&target).unwrap();
        private_file(&target, &vec![0; MAX_BYTES + 1]);
        assert!(directory.read().is_err());
        drop(directory);
        let alias = root.with_extension("alias");
        symlink(&root, &alias).unwrap();
        assert!(secure::Directory::open(&alias).is_err());
        assert!(secure::Directory::open(&alias.join("child")).is_err());
        std::fs::remove_file(alias).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(secure::Directory::open(&root).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    /// Android app storage: in an app's mount namespace `/data/user/0` is a symlink to
    /// `/data/data`, so `Context.getCacheDir()` has a symlinked ancestor (issue #525).
    #[tokio::test]
    async fn cache_persists_across_restart_under_symlinked_ancestor() {
        let _guard = test_guard().await;
        let root = tempfile::tempdir().unwrap();
        // The platform temp dir may itself sit under a link (macOS `/var`); keep `user-0` the
        // only link in the path.
        let base = std::fs::canonicalize(root.path()).unwrap();
        let real = base.join("data");
        std::fs::create_dir_all(real.join("app")).unwrap();
        let link = base.join("user-0");
        symlink(&real, &link).unwrap();
        // The app creates `cache`; the engine creates `engine` itself here.
        let path = link.join("app").join("cache").join("engine");

        // The strict walk alone reports the link as a non-directory ancestor: this is the
        // stage and errno an affected build logs.
        let failure = secure::Directory::open(&path).err().unwrap();
        assert_eq!(failure.stage, "open cache path");
        assert_eq!(failure.path.as_deref(), Some(link.as_path()));
        // Other kernels reject the link with a different errno (FreeBSD: EMLINK).
        #[cfg(any(target_os = "linux", target_os = "android"))]
        assert_eq!(failure.error.raw_os_error(), Some(libc::ENOTDIR));

        let cache = WarmPeerCache::new(&path, true);
        fixture_loaded(&cache).await;
        cache.record_productive([0; 20], peer(34571), CandidateKind::Source);
        assert!(
            cache.flush().await,
            "hints must persist under a linked ancestor"
        );
        drop(cache);
        assert!(retired_terminal(&path).await);
        let persisted = real.join("app").join("cache").join("engine").join(FILE);
        assert!(std::fs::symlink_metadata(&persisted).unwrap().is_file());

        let restarted = WarmPeerCache::new(&path, true);
        fixture_loaded(&restarted).await;
        assert_eq!(
            restarted.candidate_addresses(&[0; 20]),
            vec![peer(34571)],
            "a restart must reload hints written through the linked ancestor"
        );
        drop(restarted);
        assert!(retired_terminal(&path).await);
    }
    #[test]
    fn ancestor_resolution_keeps_final_and_dangling_links_strict() {
        let root = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(root.path()).unwrap();
        let real = base.join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = base.join("alias");
        symlink(&real, &alias).unwrap();
        assert_eq!(
            secure::resolve_ancestors(&alias),
            alias,
            "the cache directory itself is never resolved"
        );
        assert!(secure::Directory::open(&secure::resolve_ancestors(&alias)).is_err());
        assert_eq!(
            secure::resolve_ancestors(&alias.join("child").join("cache")),
            real.join("child").join("cache"),
            "linked ancestors resolve; the missing suffix is kept for the walk to create"
        );
        let dangling = base.join("dangling");
        symlink(base.join("missing"), &dangling).unwrap();
        let path = dangling.join("cache");
        assert_eq!(secure::resolve_ancestors(&path), path);
        let failure = secure::Directory::open(&path).err().unwrap();
        assert_eq!(failure.path.as_deref(), Some(dangling.as_path()));
        assert!(
            std::fs::symlink_metadata(base.join("missing")).is_err(),
            "a dangling ancestor must not be followed into creating its target"
        );
    }
    #[test]
    fn failure_log_reports_each_distinct_error_once_until_success() {
        let failure = |stage, errno| Failure {
            stage,
            path: None,
            error: io::Error::from_raw_os_error(errno),
        };
        let mut log = FailureLog::default();
        assert!(log.first(&failure("open cache path", libc::ENOTDIR)));
        assert!(!log.first(&failure("open cache path", libc::ENOTDIR)));
        assert!(log.first(&failure("open cache path", libc::EACCES)));
        assert!(log.first(&failure("lock cache", libc::EACCES)));
        assert!(!log.first(&failure("lock cache", libc::EACCES)));
        log.succeeded();
        assert!(
            log.first(&failure("lock cache", libc::EACCES)),
            "a success re-arms reporting"
        );
    }
    #[test]
    fn atomic_private_replacement_and_alias_writer_lock() {
        let root = temp();
        let directory = secure::Directory::open(&root).unwrap();
        assert!(
            secure::Directory::open(&root).is_err(),
            "directory inode lock must reject alias writers"
        );
        // One snapshot: `valid()` stamps the current second, which a slow fsync can cross.
        let snapshot = valid();
        directory.write(&snapshot).unwrap();
        assert_eq!(directory.read().unwrap(), snapshot);
        assert_eq!(
            std::fs::metadata(root.join(FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            2,
            "only completed cache and owned lock remain"
        );
        drop(directory);
        assert!(secure::Directory::open(&root).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn clone_updates_merge_with_initial_load_and_persist_actual_revision() {
        let _guard = test_guard().await;
        let root = temp();
        let directory = secure::Directory::open(&root).unwrap();
        directory.write(&valid()).unwrap();
        drop(directory);
        let cache = WarmPeerCache::new(&root, true);
        let clone = WarmPeerCache::new(&root.join("."), true);
        assert!(Arc::ptr_eq(&cache.0, &clone.0));
        // New progress can arrive before the old file finishes loading.
        clone.record_productive([0; 20], peer(23457), CandidateKind::Source);
        fixture_loaded(&cache).await;
        assert_eq!(cache.hints(&[0; 20]).len(), 2);
        assert!(cache.flush().await);
        drop(clone);
        drop(cache);
        assert!(retired_terminal(&root).await);
        let fresh = WarmPeerCache::new(&root, true);
        fixture_loaded(&fresh).await;
        assert_eq!(fresh.hints(&[0; 20]).len(), 2);
        assert!(fresh
            .hints(&[0; 20])
            .iter()
            .all(|(_, kind)| *kind == CandidateKind::Source));
        drop(fresh);
        assert!(retired_terminal(&root).await);
        std::fs::remove_dir_all(root).unwrap();
    }
    /// Releases paused synthetic workers on drop, including during a test panic.
    struct ReleasePausedWorkers(Arc<(Mutex<bool>, Condvar)>);
    impl Drop for ReleasePausedWorkers {
        fn drop(&mut self) {
            *self.0 .0.lock().unwrap_or_else(|e| e.into_inner()) = true;
            self.0 .1.notify_all();
        }
    }
    #[tokio::test]
    async fn paused_io_keeps_load_and_drop_bounded_and_retired_workers_owned() {
        let _guard = test_guard().await;
        let pause = Arc::new((Mutex::new(false), Condvar::new()));
        // An early assertion failure must still release the paused synthetic workers.
        let release = ReleasePausedWorkers(pause.clone());
        let mut owners = Vec::new();
        let mut roots = Vec::new();
        for _ in 0..MAX_IDENTITIES {
            let path = temp();
            owners.push(WarmPeerCache::new_inner(&path, true, Some(pause.clone())));
            roots.push(path);
        }
        assert!(owners
            .iter()
            .all(|cache| cache.0.worker.lock().unwrap().is_some()));
        let start = Instant::now();
        owners[0].initialized().await;
        assert!(start.elapsed() < Duration::from_millis(250));
        let extra = WarmPeerCache::new(&temp(), true);
        assert!(extra.0.worker.lock().unwrap().is_none());
        let start = Instant::now();
        drop(owners);
        assert!(start.elapsed() < Duration::from_millis(100));
        let same = WarmPeerCache::new(&roots[0], true);
        assert!(
            same.0.worker.lock().unwrap().is_none(),
            "retired stalled writer still owns path"
        );
        assert_eq!(
            registry().lock().unwrap().len(),
            MAX_IDENTITIES,
            "retired handles stay bounded and observable"
        );
        drop(release);
        for root in roots {
            assert!(retired_terminal(&root).await);
            std::fs::remove_dir_all(root).unwrap();
        }
    }
    #[tokio::test]
    async fn load_acknowledgement_waits_for_completion_not_success() {
        let _guard = test_guard().await;
        let memory = WarmPeerCache::memory();
        assert!(
            memory.is_initialized(),
            "memory fallback needs no disk load"
        );
        assert!(memory.wait_loaded(Duration::ZERO).await);

        let pause = Arc::new((Mutex::new(false), Condvar::new()));
        let _release = ReleasePausedWorkers(pause.clone());
        let stalled_root = temp();
        let stalled = WarmPeerCache::new_inner(&stalled_root, true, Some(pause.clone()));
        assert!(
            !stalled.is_initialized(),
            "paused load must not be acknowledged"
        );
        assert!(
            !stalled.wait_loaded(Duration::from_millis(50)).await,
            "a bounded wait must report that loading has not finished"
        );

        // A corrupt file still completes loading; acknowledgement is not success.
        let corrupt_root = temp();
        let directory = secure::Directory::open(&corrupt_root).unwrap();
        directory.write(b"invalid cache payload").unwrap();
        drop(directory);
        let corrupt = WarmPeerCache::new(&corrupt_root, true);
        fixture_loaded(&corrupt).await;
        assert!(corrupt.candidate_addresses(&[0; 20]).is_empty());

        drop((stalled, corrupt));
        drop(_release);
        for root in [stalled_root, corrupt_root] {
            assert!(retired_terminal(&root).await);
            std::fs::remove_dir_all(root).unwrap();
        }
    }
    #[test]
    fn productive_provenance_upgrades_preserve_eight_source_cache_cap() {
        let cache = WarmPeerCache::new(Path::new(""), true);
        for port in 1..=16 {
            cache.record_productive([0; 20], peer(port), CandidateKind::Discovered);
        }
        for port in 1..=16 {
            cache.record_productive([0; 20], peer(port), CandidateKind::Source);
            let hints = cache.hints(&[0; 20]);
            assert!(
                hints
                    .iter()
                    .filter(|(_, kind)| *kind == CandidateKind::Source)
                    .count()
                    <= 8,
                "productive source upgrade bypassed source cap"
            );
            assert!(hints.len() <= 16);
        }
    }

    #[test]
    fn existing_productive_update_at_total_row_cap_does_not_evict_another_record() {
        let mut entries = BTreeMap::new();
        for hash in 0..250u16 {
            let mut key = [0; 20];
            key[..2].copy_from_slice(&hash.to_be_bytes());
            for port in 1..=12 {
                insert(
                    &mut entries,
                    key,
                    Entry {
                        addr: peer(port),
                        source: false,
                        at: now(),
                    },
                );
            }
        }
        assert_eq!(entries.values().map(Vec::len).sum::<usize>(), 3000);
        let before = entries
            .iter()
            .flat_map(|(key, peers)| peers.iter().map(move |entry| (*key, entry.addr)))
            .collect::<std::collections::BTreeSet<_>>();
        insert(
            &mut entries,
            [0; 20],
            Entry {
                addr: peer(12),
                source: true,
                at: now(),
            },
        );
        let after = entries
            .iter()
            .flat_map(|(key, peers)| peers.iter().map(move |entry| (*key, entry.addr)))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            before, after,
            "refreshing an existing row evicted unrelated productive evidence"
        );
    }

    #[test]
    fn cache_loader_rejects_total_row_budget_before_another_entry_allocation() {
        let mut entries = BTreeMap::new();
        for hash in 0..251u16 {
            let mut key = [0; 20];
            key[..2].copy_from_slice(&hash.to_be_bytes());
            entries.insert(
                key,
                (1..=12)
                    .map(|port| Entry {
                        addr: peer(port),
                        source: false,
                        at: now(),
                    })
                    .collect(),
            );
        }
        let bytes = encode(&entries);
        assert!(bytes.len() < MAX_BYTES);
        assert!(
            parse(&bytes, true).is_err(),
            "loader accepted more than 3000 rows within byte/per-swarm limits"
        );
    }

    #[test]
    fn productive_memory_bounds_limit_sources_swarms_and_snapshot_size() {
        let cache = WarmPeerCache::memory();
        for hash in 0..300u16 {
            let mut key = [0; 20];
            key[..2].copy_from_slice(&hash.to_be_bytes());
            for port in 1..=30 {
                cache.record_productive(key, peer(port), CandidateKind::Source);
            }
        }
        // Default endpoint policy rejected loopback, including process-local hints.
        assert!(cache.hints(&[0; 20]).is_empty());
        let cache = WarmPeerCache::new(Path::new(""), true);
        for hash in 0..300u16 {
            let mut key = [0; 20];
            key[..2].copy_from_slice(&hash.to_be_bytes());
            for port in 1..=30 {
                cache.record_productive(
                    key,
                    peer(port),
                    if port <= 10 {
                        CandidateKind::Source
                    } else {
                        CandidateKind::Discovered
                    },
                );
            }
        }
        let state = cache.0.shared.state.lock().unwrap();
        assert!(state.entries.len() <= 256);
        assert!(
            state
                .entries
                .values()
                .all(|peers| peers.len() <= 16
                    && peers.iter().filter(|peer| peer.source).count() <= 8)
        );
        assert!(state.entries.values().map(Vec::len).sum::<usize>() <= 3000);
        assert!(encode(&state.entries).len() <= MAX_BYTES);
        let mut expired = state.entries.clone();
        prune(&mut expired, now() + TTL + 1);
        assert!(
            expired.is_empty(),
            "expired productive hints must be evicted"
        );
    }
}
