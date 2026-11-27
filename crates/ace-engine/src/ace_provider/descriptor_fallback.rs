//! Last-known-good live descriptors on disk, served only when the catalog cannot be reached
//! (#562).
//!
//! Content-id resolution normally needs the signed catalog. Its hosts can be unreachable while
//! a channel's swarm is still reachable, for example under ISP blocks or after a restart with the
//! catalog down. This store keeps the [`StreamInfo`] of each recently resolved live content id.
//! Resolution uses it only after a network failure ("stale-if-error"). A fresh catalog result
//! always wins and replaces the stored entry, a rejected or not-found reply never falls back, and
//! a definitive authenticated answer (an HLS origin, an unusable or protected descriptor) evicts
//! the entry. HLS origins are never stored: their signed URLs are per playback.
//!
//! The file is owner-private and written through [`ace_swarm::private_dir`] (no-follow walk,
//! inode checks, writer lock, atomic replacement). The parser is bounded and rejects the whole
//! file on any malformed row, and restored rows are validated again exactly as on record. File
//! names are constants: no path is derived from request input. Loading is lazy, so startup does
//! no extra I/O, and every disk step runs on a blocking thread.
use ace_swarm::private_dir::{resolve_ancestors, FailureKey, PrivateDir};
use ace_swarm::types::{StreamInfo, StreamMetadata};
use ace_wire::protection::Protection;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Default entry budget: a household's channel list, about 64 KiB on disk at typical sizes.
pub(crate) const DEFAULT_ENTRIES: usize = 64;
/// Largest accepted `OUTPACE_DESCRIPTOR_FALLBACK_ENTRIES`.
pub(crate) const MAX_ENTRIES: usize = 512;
/// Encoded row ceiling. A typical row (RSA-2048 key, a few trackers, a title) is about 1 KiB.
const MAX_ENTRY_BYTES: usize = 8 * 1024;
const HEADER: &str = "OUTPACE-DESCRIPTORS-1\n";
const MAX_FILE_BYTES: usize = HEADER.len() + MAX_ENTRIES * (MAX_ENTRY_BYTES + 1);
const FILE: &str = "descriptors-v1";
const LOCK_FILE: &str = "descriptors-v1.lock";
/// Older rows are dropped: live swarms behind a content id do not outlive this in practice.
const MAX_AGE_SECS: u64 = 30 * 24 * 60 * 60;
/// An unchanged descriptor is rewritten at most this often, only to refresh its age.
const REFRESH_SECS: u64 = 24 * 60 * 60;
/// A store that could not be opened (another process holds its lock, a permission problem) is
/// retried on a later write, at most this often.
const REOPEN_BACKOFF: Duration = if cfg!(test) {
    Duration::ZERO
} else {
    Duration::from_secs(60)
};
/// After a fallback, the same id skips the catalog for this long. Without it, a compat start
/// (prepare, then open) would wait through every catalog host's timeout twice.
const OUTAGE_TTL: Duration = Duration::from_secs(30);
const MAX_OUTAGES: usize = 128;
const MAX_LIST_ITEMS: usize = 32;
const MAX_ITEM_BYTES: usize = 512;
const MAX_TITLE_BYTES: usize = 256;
const MAX_PUBKEY_BYTES: usize = 2048;

#[derive(Clone)]
struct Stored {
    at: u64,
    info: StreamInfo,
}

#[derive(Default)]
struct State {
    entries: HashMap<[u8; 20], Stored>,
    loaded: bool,
    revision: u64,
    writing: bool,
    last_failure: Option<FailureKey>,
    /// Ids forgotten before the file was loaded: skipped when it loads, so a forget can never
    /// erase a descriptor recorded after it.
    forgotten: HashSet<[u8; 20]>,
}

enum Io {
    Unopened,
    Open(PrivateDir),
    /// Opening failed at this instant; descriptors stay in memory until a retry succeeds.
    Unavailable(Instant),
}

/// Outstanding blocking tasks. Lives outside [`Inner`] so a finished task can report after it
/// dropped its reference, which releases the directory lock once the store itself is dropped.
#[derive(Default)]
struct Tasks {
    active: AtomicUsize,
    #[cfg(test)]
    idle: tokio::sync::Notify,
}

struct Inner {
    path: PathBuf,
    capacity: usize,
    state: Mutex<State>,
    io: Mutex<Io>,
    outages: Mutex<VecDeque<([u8; 20], Instant)>>,
    tasks: Arc<Tasks>,
}

/// Bounded on-disk store of last-known-good live descriptors. `Default` is disabled.
#[derive(Clone, Default)]
pub(crate) struct DescriptorFallback(Option<Arc<Inner>>);

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl DescriptorFallback {
    /// A store under `dir` holding at most `capacity` (clamped to [`MAX_ENTRIES`]) descriptors.
    /// An empty `dir` or a zero `capacity` disables it.
    pub(crate) fn new(dir: &Path, capacity: usize) -> Self {
        if dir.as_os_str().is_empty() || capacity == 0 {
            return Self(None);
        }
        Self(Some(Arc::new(Inner {
            path: dir.to_owned(),
            capacity: capacity.min(MAX_ENTRIES),
            state: Mutex::default(),
            io: Mutex::new(Io::Unopened),
            outages: Mutex::default(),
            tasks: Arc::default(),
        })))
    }

    #[cfg(test)]
    pub(crate) fn is_enabled(&self) -> bool {
        self.0.is_some()
    }

    /// Remember a fresh catalog descriptor, replacing any stored one. Descriptors that could not
    /// be restored exactly (not public, invalid geometry, oversized) are not stored.
    pub(crate) fn record(&self, key: [u8; 20], info: &StreamInfo) {
        let Some(inner) = &self.0 else { return };
        inner.outages.lock().unwrap().retain(|(id, _)| *id != key);
        let time = now();
        let stored = Stored {
            at: time,
            info: info.clone(),
        };
        let row = encode_row(&key, &stored);
        if row.len() > MAX_ENTRY_BYTES
            || parse_row(&row, time)
                .ok()
                .flatten()
                .map(|(_, restored)| restored.info)
                != Some(info.clone())
        {
            return;
        }
        {
            let mut state = inner.state.lock().unwrap();
            state.forgotten.remove(&key);
            if let Some(existing) = state.entries.get(&key) {
                if existing.info == *info && time.saturating_sub(existing.at) < REFRESH_SECS {
                    return;
                }
            }
            state.entries.insert(key, stored);
            trim(&mut state.entries, inner.capacity);
        }
        inner.schedule_write();
    }

    /// Drop a stored descriptor after a definitive catalog answer. Runs in the background.
    pub(crate) fn forget(&self, key: [u8; 20]) {
        let Some(inner) = &self.0 else { return };
        inner.outages.lock().unwrap().retain(|(id, _)| *id != key);
        let write = {
            let mut state = inner.state.lock().unwrap();
            let removed = state.entries.remove(&key).is_some();
            // Not loaded yet: the entry may exist only on disk, so the load skips it. Bounded:
            // the first forget's write loads the file and clears the set.
            if !state.loaded && state.forgotten.len() < MAX_ENTRIES {
                state.forgotten.insert(key);
                true
            } else {
                removed
            }
        };
        if write {
            inner.schedule_write();
        }
    }

    /// The stored descriptor for `key` and its age, loading the file on first use.
    pub(crate) async fn lookup(&self, key: [u8; 20]) -> Option<(StreamInfo, Duration)> {
        let inner = self.0.as_ref()?;
        if !inner.state.lock().unwrap().loaded {
            let loader = Arc::clone(inner);
            let tasks = Arc::clone(&inner.tasks);
            tasks.active.fetch_add(1, Ordering::SeqCst);
            let loaded = tokio::task::spawn_blocking(move || {
                loader.ensure_loaded();
                drop(loader);
            })
            .await;
            tasks.finish();
            loaded.ok()?;
        }
        let state = inner.state.lock().unwrap();
        let stored = state.entries.get(&key)?;
        let age = now().saturating_sub(stored.at);
        (age <= MAX_AGE_SECS).then(|| (stored.info.clone(), Duration::from_secs(age)))
    }

    /// Mark `key` as just served from the store, after a catalog network failure.
    pub(crate) fn note_outage(&self, key: [u8; 20]) {
        let Some(inner) = &self.0 else { return };
        let mut outages = inner.outages.lock().unwrap();
        outages.retain(|(id, at)| *id != key && at.elapsed() < OUTAGE_TTL);
        if outages.len() >= MAX_OUTAGES {
            outages.pop_front();
        }
        outages.push_back((key, Instant::now()));
    }

    /// Whether `key` fell back within the last [`OUTAGE_TTL`].
    pub(crate) fn recent_outage(&self, key: &[u8; 20]) -> bool {
        let Some(inner) = &self.0 else { return false };
        inner
            .outages
            .lock()
            .unwrap()
            .iter()
            .any(|(id, at)| id == key && at.elapsed() < OUTAGE_TTL)
    }

    /// Wait until no load, write or eviction task is running.
    #[cfg(test)]
    pub(crate) async fn settled(&self) {
        let Some(inner) = &self.0 else { return };
        let tasks = Arc::clone(&inner.tasks);
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let notified = tasks.idle.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if tasks.active.load(Ordering::SeqCst) == 0 {
                    return;
                }
                notified.await;
            }
        })
        .await
        .expect("descriptor store tasks settle");
    }

    #[cfg(test)]
    pub(crate) fn len_for_test(&self) -> usize {
        self.0
            .as_ref()
            .map_or(0, |inner| inner.state.lock().unwrap().entries.len())
    }
}

impl Tasks {
    fn finish(&self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
        #[cfg(test)]
        self.idle.notify_waiters();
    }
}

impl Inner {
    /// Run `task` on a blocking thread, or inline without a runtime.
    fn spawn(self: &Arc<Self>, task: impl FnOnce(&Arc<Self>) + Send + 'static) {
        let tasks = Arc::clone(&self.tasks);
        tasks.active.fetch_add(1, Ordering::SeqCst);
        let inner = Arc::clone(self);
        let run = move || {
            task(&inner);
            drop(inner);
            tasks.finish();
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => drop(handle.spawn_blocking(run)),
            Err(_) => run(),
        }
    }

    fn schedule_write(self: &Arc<Self>) {
        {
            let mut state = self.state.lock().unwrap();
            state.revision = state.revision.wrapping_add(1);
            if state.writing {
                return;
            }
            state.writing = true;
        }
        self.spawn(|inner| inner.write_until_current());
    }

    /// One writer at a time; it keeps writing until the snapshot it wrote is current.
    fn write_until_current(&self) {
        let mut io = self.io.lock().unwrap();
        self.ensure_loaded_locked(&mut io);
        loop {
            let (snapshot, revision) = {
                let state = self.state.lock().unwrap();
                (encode(&state.entries), state.revision)
            };
            if let Io::Open(directory) = &*io {
                match directory.write(FILE, &snapshot, MAX_FILE_BYTES) {
                    Ok(()) => self.state.lock().unwrap().last_failure = None,
                    Err(failure) => self.report(
                        failure.key(),
                        &format!(
                            "write failed at {}: {}; the saved copy may be out of date",
                            failure.stage, failure.error
                        ),
                    ),
                }
            }
            let mut state = self.state.lock().unwrap();
            if state.revision == revision {
                state.writing = false;
                return;
            }
        }
    }

    fn ensure_loaded(&self) {
        let mut io = self.io.lock().unwrap();
        self.ensure_loaded_locked(&mut io);
    }

    /// Load the file on first use. A store that could not be opened is retried here, from the
    /// writer, after [`REOPEN_BACKOFF`]; its entries then merge under the in-memory ones.
    fn ensure_loaded_locked(&self, io: &mut Io) {
        let retry = matches!(io, Io::Unavailable(since) if since.elapsed() >= REOPEN_BACKOFF);
        if self.state.lock().unwrap().loaded && !retry {
            return;
        }
        if matches!(io, Io::Unopened) || retry {
            *io = match PrivateDir::open(&resolve_ancestors(&self.path), LOCK_FILE) {
                Ok(directory) => Io::Open(directory),
                Err(failure) => {
                    self.report(
                        failure.key(),
                        &format!(
                            "unavailable at {}{}: {}; descriptors are kept in memory only until it opens",
                            failure.stage,
                            failure
                                .path
                                .as_ref()
                                .map(|path| format!(" ({})", path.display()))
                                .unwrap_or_default(),
                            failure.error
                        ),
                    );
                    Io::Unavailable(Instant::now())
                }
            };
        }
        let restored = match &*io {
            Io::Open(directory) => match directory.read(FILE, MAX_FILE_BYTES) {
                Ok(bytes) => match parse(&bytes, now()) {
                    Ok(entries) => entries,
                    Err(_) => {
                        crate::alog!(
                            "[resolve] ignoring malformed descriptor fallback file; it is replaced on the next resolution"
                        );
                        HashMap::new()
                    }
                },
                Err(error) if error.kind() == io::ErrorKind::NotFound => HashMap::new(),
                Err(error) => {
                    crate::alog!("[resolve] ignoring unreadable descriptor fallback file: {error}");
                    HashMap::new()
                }
            },
            _ => HashMap::new(),
        };
        let mut state = self.state.lock().unwrap();
        let forgotten = std::mem::take(&mut state.forgotten);
        // Entries recorded before the load are fresher than anything on disk.
        for (key, stored) in restored {
            if !forgotten.contains(&key) {
                state.entries.entry(key).or_insert(stored);
            }
        }
        trim(&mut state.entries, self.capacity);
        state.loaded = true;
    }

    /// Log each distinct failure once until a write succeeds.
    fn report(&self, key: FailureKey, message: &str) {
        let mut state = self.state.lock().unwrap();
        if state.last_failure == Some(key) {
            return;
        }
        state.last_failure = Some(key);
        drop(state);
        crate::alog!("[resolve] descriptor fallback store {message} (repeats are not logged)");
    }
}

/// Keep the `capacity` most recently stored entries.
fn trim(entries: &mut HashMap<[u8; 20], Stored>, capacity: usize) {
    while entries.len() > capacity {
        let Some(oldest) = entries
            .iter()
            .min_by_key(|(key, stored)| (stored.at, **key))
            .map(|(key, _)| *key)
        else {
            return;
        };
        entries.remove(&oldest);
    }
}

fn encode(entries: &HashMap<[u8; 20], Stored>) -> Vec<u8> {
    let mut rows: Vec<_> = entries.iter().collect();
    rows.sort_by_key(|(key, _)| **key);
    let mut bytes = HEADER.as_bytes().to_vec();
    for (key, stored) in rows {
        bytes.extend_from_slice(encode_row(key, stored).as_bytes());
        bytes.push(b'\n');
    }
    bytes
}

fn encode_list(items: &[String]) -> String {
    if items.is_empty() {
        "-".into()
    } else {
        items.iter().map(hex::encode).collect::<Vec<_>>().join(",")
    }
}

/// `cid at infohash piece_length chunk_length bitrate pubkey title categories trackers`, with
/// `-` for an absent value and every byte string hex-encoded, so no field can break the row.
fn encode_row(key: &[u8; 20], stored: &Stored) -> String {
    let info = &stored.info;
    let optional = |bytes: &[u8]| {
        if bytes.is_empty() {
            "-".to_owned()
        } else {
            hex::encode(bytes)
        }
    };
    format!(
        "{} {} {} {} {} {} {} {} {} {}",
        hex::encode(key),
        stored.at,
        hex::encode(info.infohash),
        info.piece_length,
        info.chunk_length,
        info.metadata
            .bitrate
            .map_or_else(|| "-".to_owned(), |bitrate| bitrate.to_string()),
        optional(&info.source_pubkey),
        optional(
            info.metadata
                .title
                .as_deref()
                .unwrap_or_default()
                .as_bytes()
        ),
        encode_list(&info.metadata.categories),
        encode_list(&info.trackers),
    )
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid descriptor fallback file",
    )
}

fn parse(bytes: &[u8], time: u64) -> io::Result<HashMap<[u8; 20], Stored>> {
    if bytes.len() > MAX_FILE_BYTES || !bytes.starts_with(HEADER.as_bytes()) {
        return Err(invalid());
    }
    let mut entries = HashMap::new();
    let mut rows = 0;
    for line in bytes[HEADER.len()..].split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        rows += 1;
        if line.len() > MAX_ENTRY_BYTES || rows > MAX_ENTRIES {
            return Err(invalid());
        }
        let row = std::str::from_utf8(line).map_err(|_| invalid())?;
        if let Some((key, stored)) = parse_row(row, time)? {
            if entries.insert(key, stored).is_some() {
                return Err(invalid());
            }
        }
    }
    Ok(entries)
}

fn hex_field(field: &str, max: usize) -> io::Result<Vec<u8>> {
    if field.len() > max * 2 {
        return Err(invalid());
    }
    hex::decode(field).map_err(|_| invalid())
}

fn hex20_field(field: &str) -> io::Result<[u8; 20]> {
    let mut bytes = [0; 20];
    if field.len() != 40 || hex::decode_to_slice(field, &mut bytes).is_err() {
        return Err(invalid());
    }
    Ok(bytes)
}

fn text_field(field: &str, max: usize) -> io::Result<String> {
    String::from_utf8(hex_field(field, max)?).map_err(|_| invalid())
}

fn list_field(field: &str) -> io::Result<Vec<String>> {
    if field == "-" {
        return Ok(Vec::new());
    }
    let items = field.split(',');
    if items.clone().count() > MAX_LIST_ITEMS {
        return Err(invalid());
    }
    items.map(|item| text_field(item, MAX_ITEM_BYTES)).collect()
}

/// One row, or `None` for a valid row past [`MAX_AGE_SECS`].
fn parse_row(row: &str, time: u64) -> io::Result<Option<([u8; 20], Stored)>> {
    let mut fields = row.split(' ');
    let mut next = || fields.next().ok_or_else(invalid);
    let key = hex20_field(next()?)?;
    let at: u64 = next()?.parse().map_err(|_| invalid())?;
    let infohash = hex20_field(next()?)?;
    let piece_length: u64 = next()?.parse().map_err(|_| invalid())?;
    let chunk_length: u64 = next()?.parse().map_err(|_| invalid())?;
    let bitrate = match next()? {
        "-" => None,
        value => Some(
            value
                .parse::<u64>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(invalid)?,
        ),
    };
    let (source_pubkey, sig_len) = match next()? {
        "-" => (Vec::new(), 0),
        value => {
            let pubkey = hex_field(value, MAX_PUBKEY_BYTES)?;
            let sig_len =
                ace_wire::live_auth::signature_len_from_pubkey_der(&pubkey).ok_or_else(invalid)?;
            (pubkey, sig_len)
        }
    };
    let title = match next()? {
        "-" => None,
        value => Some(text_field(value, MAX_TITLE_BYTES)?),
    };
    let categories = list_field(next()?)?;
    let trackers = list_field(next()?)?;
    if fields.next().is_some() {
        return Err(invalid());
    }
    // A clock that stepped back (a board without an RTC, booted offline) must not cost every
    // saved descriptor: a row from the future counts as stored now.
    let at = at.min(time);
    ace_swarm::resolve::validate_geometry(piece_length, chunk_length).map_err(|_| invalid())?;
    if time.saturating_sub(at) > MAX_AGE_SECS {
        return Ok(None);
    }
    Ok(Some((
        key,
        Stored {
            at,
            info: StreamInfo {
                protection: Some(Protection::Public),
                infohash,
                piece_length,
                chunk_length,
                trackers,
                metadata: StreamMetadata {
                    title,
                    bitrate,
                    categories,
                },
                sig_len,
                source_pubkey,
            },
        },
    )))
}

#[cfg(test)]
pub(crate) mod tests;
