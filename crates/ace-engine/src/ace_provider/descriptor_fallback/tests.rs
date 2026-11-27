use super::*;

pub(crate) fn temp_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "outpace-descriptor-fallback-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ))
}

/// A synthetic public live descriptor; `marker` varies the infohash.
pub(crate) fn info(marker: u8) -> StreamInfo {
    StreamInfo {
        protection: Some(Protection::Public),
        infohash: [marker; 20],
        piece_length: 1_048_576,
        chunk_length: 16_384,
        trackers: vec!["udp://tracker.invalid:80".into()],
        metadata: StreamMetadata {
            title: Some("Synthetic Live".into()),
            bitrate: Some(1_000_000),
            categories: vec!["fixture".into()],
        },
        sig_len: 0,
        source_pubkey: Vec::new(),
    }
}

const KEY: [u8; 20] = [0x01; 20];

fn signed_info() -> StreamInfo {
    let (bytes, _) = crate::ace_provider::test_support::synthetic_live_transport(1_048_576);
    ace_swarm::resolve::stream_info_from_transport(&bytes).unwrap()
}

fn row(stored_at: u64) -> String {
    encode_row(
        &KEY,
        &Stored {
            at: stored_at,
            info: info(7),
        },
    )
}

fn file(rows: &[String]) -> Vec<u8> {
    let mut bytes = HEADER.as_bytes().to_vec();
    for row in rows {
        bytes.extend_from_slice(row.as_bytes());
        bytes.push(b'\n');
    }
    bytes
}

#[cfg(unix)]
fn private_file(path: &Path, bytes: &[u8]) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn rows_round_trip_including_a_real_source_key() {
    let time = now();
    for info in [info(7), signed_info()] {
        let stored = Stored { at: time, info };
        let row = encode_row(&KEY, &stored);
        assert!(row.len() < 2048, "typical rows stay small: {}", row.len());
        let (key, restored) = parse_row(&row, time).unwrap().unwrap();
        assert_eq!(key, KEY);
        assert_eq!(restored.info, stored.info);
    }
}

#[test]
fn bounded_parser_rejects_corrupt_hostile_and_oversized_files() {
    let time = now();
    assert_eq!(parse(&file(&[row(time)]), time).unwrap().len(), 1);
    let mut cases: Vec<(&str, Vec<u8>)> = vec![
        ("missing header", row(time).into_bytes()),
        ("wrong header", b"OUTPACE-DESCRIPTORS-2\n".to_vec()),
        ("duplicate id", file(&[row(time), row(time)])),
        (
            "duplicate id behind an expired row",
            file(&[row(time - MAX_AGE_SECS - 1), row(time)]),
        ),
        ("extra field", file(&[format!("{} x", row(time))])),
        (
            "missing field",
            file(&[row(time).rsplit_once(' ').unwrap().0.into()]),
        ),
        ("bad id hex", file(&[format!("zz{}", &row(time)[2..])])),
        (
            "bad geometry",
            file(&[row(time).replace(" 16384 ", " 16000 ")]),
        ),
        (
            "bad pubkey",
            file(&[row(time).replace(" 1000000 - ", " 1000000 abcd ")]),
        ),
        (
            "zero bitrate",
            file(&[row(time).replace(" 1000000 ", " 0 ")]),
        ),
        ("long line", file(&["a".repeat(MAX_ENTRY_BYTES + 1)])),
        (
            "too many trackers",
            file(&[{
                let mut stored = Stored {
                    at: time,
                    info: info(7),
                };
                stored.info.trackers = vec!["u".into(); MAX_LIST_ITEMS + 1];
                encode_row(&KEY, &stored)
            }]),
        ),
    ];
    let mut too_many = Vec::new();
    for index in 0..=MAX_ENTRIES {
        let mut key = [0; 20];
        key[..8].copy_from_slice(&(index as u64).to_be_bytes());
        too_many.push(encode_row(
            &key,
            &Stored {
                at: time,
                info: info(7),
            },
        ));
    }
    cases.push(("too many rows", file(&too_many)));
    let mut oversized = HEADER.as_bytes().to_vec();
    oversized.resize(MAX_FILE_BYTES + 1, b'\n');
    cases.push(("oversized file", oversized));
    let mut non_utf8 = file(&[row(time)]);
    non_utf8.extend_from_slice(b"\xff\n");
    cases.push(("non-utf8 row", non_utf8));
    for (name, bytes) in cases {
        assert!(parse(&bytes, time).is_err(), "{name} must be rejected");
    }
    // An expired row is skipped, not a reason to distrust the file.
    let expired = parse(&file(&[row(time - MAX_AGE_SECS - 1)]), time).unwrap();
    assert!(expired.is_empty());
    // The clock stepped back: a row from the future is kept and counts as stored now.
    let future = parse(&file(&[row(time + 7 * 24 * 60 * 60)]), time).unwrap();
    assert_eq!(future[&KEY].at, time);
    assert_eq!(future[&KEY].info, info(7));
}

#[test]
fn trim_keeps_the_most_recently_stored_entries() {
    let mut entries = HashMap::new();
    for (marker, at) in [(1u8, 30), (2, 10), (3, 20)] {
        entries.insert(
            [marker; 20],
            Stored {
                at,
                info: info(marker),
            },
        );
    }
    trim(&mut entries, 2);
    assert!(entries.contains_key(&[1; 20]) && entries.contains_key(&[3; 20]));
    assert!(!entries.contains_key(&[2; 20]));
}

#[test]
fn disabled_store_is_inert() {
    let dir = temp_dir();
    for store in [
        DescriptorFallback::new(&dir, 0),
        DescriptorFallback::new(Path::new(""), 8),
        DescriptorFallback::default(),
    ] {
        assert!(!store.is_enabled());
        store.record(KEY, &info(7));
        store.note_outage(KEY);
        assert!(!store.recent_outage(&KEY));
        assert_eq!(store.len_for_test(), 0);
    }
    assert!(!dir.exists(), "a disabled store touches no disk");
}

#[cfg(unix)]
#[tokio::test]
async fn record_persists_across_restart_and_fresh_results_replace_stored_ones() {
    let dir = temp_dir();
    let store = DescriptorFallback::new(&dir, 8);
    assert!(store.lookup(KEY).await.is_none());
    store.record(KEY, &info(7));
    store.settled().await;
    drop(store);

    let store = DescriptorFallback::new(&dir, 8);
    let (restored, age) = store.lookup(KEY).await.unwrap();
    assert_eq!(restored, info(7));
    assert!(age < Duration::from_secs(60));
    // The catalog rotated the infohash: the fresh descriptor overwrites the stored one.
    store.record(KEY, &info(8));
    store.settled().await;
    assert_eq!(store.lookup(KEY).await.unwrap().0, info(8));
    drop(store);

    let store = DescriptorFallback::new(&dir, 8);
    assert_eq!(store.lookup(KEY).await.unwrap().0, info(8));
    store.forget(KEY);
    store.settled().await;
    assert!(store.lookup(KEY).await.is_none());
    drop(store);
    let store = DescriptorFallback::new(&dir, 8);
    assert!(
        store.lookup(KEY).await.is_none(),
        "a forgotten entry stays gone"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(FILE)), 0o600);
    }
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[tokio::test]
async fn store_is_bounded_by_entry_count_on_disk_and_in_memory() {
    let dir = temp_dir();
    let store = DescriptorFallback::new(&dir, 2);
    for marker in 1..=3u8 {
        store.record([marker; 20], &info(marker));
    }
    store.settled().await;
    assert_eq!(store.len_for_test(), 2);
    drop(store);
    // A larger file is trimmed to a smaller configured budget on load.
    let store = DescriptorFallback::new(&dir, 1);
    store.lookup(KEY).await;
    assert_eq!(store.len_for_test(), 1);
    drop(store);
    let bytes = std::fs::read(dir.join(FILE)).unwrap();
    assert_eq!(
        parse(&bytes, now()).unwrap().len(),
        2,
        "load alone never rewrites"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn unrestorable_descriptors_are_never_stored() {
    let dir = temp_dir();
    let store = DescriptorFallback::new(&dir, 8);
    let mut unpublished = info(7);
    unpublished.protection = None;
    let mut protected = info(7);
    protected.protection = Some(Protection::Protected);
    let mut geometry = info(7);
    geometry.chunk_length = 2;
    let mut oversized = info(7);
    oversized.trackers = vec!["udp://tracker.invalid:80".repeat(10); MAX_LIST_ITEMS];
    let mut empty_title = info(7);
    empty_title.metadata.title = Some(String::new());
    for info in [unpublished, protected, geometry, oversized, empty_title] {
        store.record(KEY, &info);
    }
    store.settled().await;
    assert_eq!(store.len_for_test(), 0);
    // Each one evicts whatever the disk held for the id, so the file may exist, but empty.
    if let Ok(bytes) = std::fs::read(dir.join(FILE)) {
        assert!(parse(&bytes, now()).unwrap().is_empty());
    }
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[tokio::test]
async fn corrupt_or_unsafe_files_are_ignored_and_corrupt_ones_replaced() {
    let dir = temp_dir();
    let store = DescriptorFallback::new(&dir, 8);
    store.lookup(KEY).await; // creates the private directory
    drop(store);
    private_file(&dir.join(FILE), b"OUTPACE-DESCRIPTORS-1\nnot a row\n");
    let store = DescriptorFallback::new(&dir, 8);
    assert!(store.lookup(KEY).await.is_none());
    store.record(KEY, &info(7));
    store.settled().await;
    drop(store);
    let bytes = std::fs::read(dir.join(FILE)).unwrap();
    assert_eq!(parse(&bytes, now()).unwrap().len(), 1);

    // A world-readable file, or a symlink in its place, is never trusted or replaced.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir.join(FILE), std::fs::Permissions::from_mode(0o644)).unwrap();
    let store = DescriptorFallback::new(&dir, 8);
    assert!(store.lookup(KEY).await.is_none());
    store.record([2; 20], &info(2));
    store.settled().await;
    drop(store);
    assert_eq!(std::fs::read(dir.join(FILE)).unwrap(), bytes);

    let outside = dir.with_extension("outside");
    private_file(&outside, &bytes);
    std::fs::remove_file(dir.join(FILE)).unwrap();
    std::os::unix::fs::symlink(&outside, dir.join(FILE)).unwrap();
    let store = DescriptorFallback::new(&dir, 8);
    assert!(store.lookup(KEY).await.is_none());
    store.record([2; 20], &info(2));
    store.settled().await;
    drop(store);
    assert_eq!(std::fs::read(&outside).unwrap(), bytes);
    let _ = std::fs::remove_file(outside);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn outage_marker_is_per_id_and_cleared_by_a_fresh_result() {
    let dir = temp_dir();
    let store = DescriptorFallback::new(&dir, 8);
    store.note_outage(KEY);
    assert!(store.recent_outage(&KEY));
    assert!(!store.recent_outage(&[2; 20]));
    store.record(KEY, &info(7));
    assert!(!store.recent_outage(&KEY));
    store.settled().await;
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[tokio::test]
async fn a_forget_before_load_never_erases_a_later_record() {
    let dir = temp_dir();
    let store = DescriptorFallback::new(&dir, 8);
    store.record(KEY, &info(7));
    store.settled().await;
    drop(store);
    let store = DescriptorFallback::new(&dir, 8);
    store.forget(KEY);
    store.record(KEY, &info(8));
    store.settled().await;
    assert_eq!(store.lookup(KEY).await.unwrap().0, info(8));
    drop(store);
    let store = DescriptorFallback::new(&dir, 8);
    assert_eq!(store.lookup(KEY).await.unwrap().0, info(8));
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[tokio::test]
async fn a_store_locked_by_another_process_is_reopened_and_merged_later() {
    let dir = temp_dir();
    let holder = DescriptorFallback::new(&dir, 8);
    holder.record([2; 20], &info(2));
    holder.settled().await;
    let blocked = DescriptorFallback::new(&dir, 8);
    blocked.record(KEY, &info(7));
    blocked.settled().await;
    // Kept in memory while the lock is held elsewhere.
    assert_eq!(blocked.lookup(KEY).await.unwrap().0, info(7));
    drop(holder);
    blocked.record([3; 20], &info(3));
    blocked.settled().await;
    drop(blocked);
    let store = DescriptorFallback::new(&dir, 8);
    for key in [KEY, [2; 20], [3; 20]] {
        assert!(store.lookup(key).await.is_some(), "{key:?} persisted");
    }
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[tokio::test]
async fn a_fresh_result_that_cannot_be_stored_evicts_the_superseded_one() {
    let dir = temp_dir();
    let store = DescriptorFallback::new(&dir, 8);
    store.record(KEY, &info(7));
    store.settled().await;
    let mut rotated = info(8);
    rotated.trackers = vec!["udp://tracker.invalid:80".repeat(10); MAX_LIST_ITEMS];
    store.record(KEY, &rotated);
    store.settled().await;
    assert!(store.lookup(KEY).await.is_none());
    drop(store);
    let store = DescriptorFallback::new(&dir, 8);
    assert!(store.lookup(KEY).await.is_none(), "evicted on disk too");
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[tokio::test]
async fn an_unchanged_result_retries_a_write_the_lock_blocked() {
    let dir = temp_dir();
    let holder = DescriptorFallback::new(&dir, 8);
    holder.lookup(KEY).await; // takes the lock
    let blocked = DescriptorFallback::new(&dir, 8);
    blocked.record(KEY, &info(7));
    blocked.settled().await;
    drop(holder);
    // Same descriptor again: nothing changed in memory, but the disk never saw it.
    blocked.record(KEY, &info(7));
    blocked.settled().await;
    drop(blocked);
    let store = DescriptorFallback::new(&dir, 8);
    assert_eq!(store.lookup(KEY).await.unwrap().0, info(7));
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[tokio::test]
async fn a_forget_survives_a_lock_miss_until_the_disk_reflects_it() {
    for looked_up_first in [false, true] {
        let dir = temp_dir();
        let store = DescriptorFallback::new(&dir, 8);
        store.record(KEY, &info(7));
        store.settled().await;
        drop(store);
        let holder = DescriptorFallback::new(&dir, 8);
        holder.lookup([9; 20]).await; // takes the lock
        let blocked = DescriptorFallback::new(&dir, 8);
        if looked_up_first {
            assert!(
                blocked.lookup(KEY).await.is_none(),
                "the file was not readable"
            );
        }
        blocked.forget(KEY);
        blocked.settled().await;
        drop(holder);
        blocked.record([2; 20], &info(2));
        blocked.settled().await;
        assert!(
            blocked.lookup(KEY).await.is_none(),
            "reopen must not resurrect it"
        );
        drop(blocked);
        let store = DescriptorFallback::new(&dir, 8);
        assert!(
            store.lookup(KEY).await.is_none(),
            "{looked_up_first}: evicted on disk"
        );
        assert!(store.lookup([2; 20]).await.is_some());
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_lookup_retries_a_store_the_lock_blocked_on_first_load() {
    let dir = temp_dir();
    let store = DescriptorFallback::new(&dir, 8);
    store.record(KEY, &info(7));
    store.settled().await;
    drop(store);
    let holder = DescriptorFallback::new(&dir, 8);
    holder.lookup([9; 20]).await; // takes the lock
    let blocked = DescriptorFallback::new(&dir, 8);
    assert!(
        blocked.lookup(KEY).await.is_none(),
        "locked out on first load"
    );
    drop(holder);
    // No record or forget in between: the lookup alone reopens the store.
    assert_eq!(blocked.lookup(KEY).await.unwrap().0, info(7));
    drop(blocked);
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[tokio::test]
async fn a_repeated_forget_retries_the_eviction_write_it_still_owes() {
    let dir = temp_dir();
    let store = DescriptorFallback::new(&dir, 8);
    store.record(KEY, &info(7));
    store.settled().await;
    drop(store);
    let holder = DescriptorFallback::new(&dir, 8);
    holder.lookup([9; 20]).await; // takes the lock
    let blocked = DescriptorFallback::new(&dir, 8);
    blocked.forget(KEY);
    blocked.settled().await;
    drop(holder);
    // Already pending, but the disk never saw the eviction: this forget must write it.
    blocked.forget(KEY);
    blocked.settled().await;
    drop(blocked);
    let store = DescriptorFallback::new(&dir, 8);
    assert!(store.lookup(KEY).await.is_none(), "evicted on disk");
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[tokio::test]
async fn an_overflowing_eviction_set_never_restores_a_forgotten_row() {
    let dir = temp_dir();
    let store = DescriptorFallback::new(&dir, 8);
    store.record(KEY, &info(7));
    store.settled().await;
    drop(store);
    let holder = DescriptorFallback::new(&dir, 8);
    holder.lookup([9; 20]).await; // takes the lock
    let blocked = DescriptorFallback::new(&dir, 8);
    for index in 0..MAX_ENTRIES as u64 {
        let mut other = [0xee; 20];
        other[..8].copy_from_slice(&index.to_be_bytes());
        blocked.forget(other);
    }
    blocked.forget(KEY); // past the pending-eviction bound
    blocked.settled().await;
    drop(holder);
    blocked.record([2; 20], &info(2));
    blocked.settled().await;
    assert!(
        blocked.lookup(KEY).await.is_none(),
        "reopen must not resurrect it"
    );
    assert!(blocked.lookup([2; 20]).await.is_some());
    drop(blocked);
    let store = DescriptorFallback::new(&dir, 8);
    assert!(store.lookup(KEY).await.is_none(), "evicted on disk");
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[tokio::test]
async fn a_lookup_that_reopens_the_store_writes_what_it_still_owes() {
    let dir = temp_dir();
    let store = DescriptorFallback::new(&dir, 8);
    store.record(KEY, &info(7));
    store.settled().await;
    drop(store);
    let holder = DescriptorFallback::new(&dir, 8);
    holder.lookup([9; 20]).await; // takes the lock
    let blocked = DescriptorFallback::new(&dir, 8);
    blocked.forget(KEY);
    blocked.record([2; 20], &info(2));
    blocked.settled().await;
    drop(holder);
    // An outage lookup reopens the store: the eviction and the record must reach the disk.
    assert!(blocked.lookup(KEY).await.is_none());
    blocked.settled().await;
    drop(blocked);
    let store = DescriptorFallback::new(&dir, 8);
    assert!(store.lookup(KEY).await.is_none(), "evicted on disk");
    assert!(store.lookup([2; 20]).await.is_some(), "recorded on disk");
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_cancelled_cold_lookup_still_settles() {
    let dir = temp_dir();
    let store = DescriptorFallback::new(&dir, 8);
    // Polled once, which starts the blocking load, then dropped.
    let _ = tokio::time::timeout(Duration::ZERO, store.lookup(KEY)).await;
    store.settled().await;
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
}
