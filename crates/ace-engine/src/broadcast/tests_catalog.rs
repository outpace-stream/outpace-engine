//! Opt-in catalog registration of announced broadcasts. The registrar is stubbed: these tests
//! never contact a catalog.

use super::{BroadcastRegistry, RegistrarFuture};
use crate::config::CacheType;
use ace_swarm::listen::SeedRegistry;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

const CATALOG_ID: [u8; 20] = [0x5a; 20];

fn temp_dir() -> PathBuf {
    let path = std::env::temp_dir().join(format!("outpace-catalog-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// Transports the stub registrar was handed, in call order.
type SeenTransports = Arc<Mutex<Vec<Vec<u8>>>>;

/// A registry with a quiet announcer and a registrar answering from `results` in order
/// (the last answer repeats). Returns the call counter and the transports it was given.
fn registry_with(
    dir: &std::path::Path,
    results: Vec<Result<[u8; 20], String>>,
) -> (Arc<BroadcastRegistry>, Arc<AtomicUsize>, SeenTransports) {
    let registry = BroadcastRegistry::with_persist(dir, CacheType::Memory, PathBuf::new());
    registry.set_test_announcer(Arc::new(|_, _| Box::pin(std::future::pending::<()>())));
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (counter, log) = (calls.clone(), seen.clone());
    registry.set_registrar(Arc::new(move |transport| -> RegistrarFuture {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        log.lock().unwrap().push(transport.as_ref().clone());
        let result = results[n.min(results.len() - 1)].clone();
        Box::pin(async move { result })
    }));
    (registry, calls, seen)
}

async fn wait_for_id(registry: &BroadcastRegistry, key: &[u8; 20]) -> [u8; 20] {
    tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            if let Some(id) = registry.catalog_content_id(key) {
                return id;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("catalog id recorded")
}

#[tokio::test]
async fn registers_the_broadcast_transport_once() {
    let dir = temp_dir();
    let (registry, calls, seen) = registry_with(&dir, vec![Ok(CATALOG_ID)]);
    registry.set_catalog_registration(true);
    let seed = SeedRegistry::new();
    let bc = registry
        .mint("synthetic-catalog", &[], &seed, 8 << 20)
        .await
        .unwrap();
    let (_port_tx, port) = watch::channel(Some(4661));
    registry
        .ensure_announcing("synthetic-catalog", port.clone())
        .await
        .unwrap();

    assert_eq!(wait_for_id(&registry, &bc.content_id).await, CATALOG_ID);
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [bc.transport_bytes.to_vec()]
    );
    // Announcing is idempotent, and so is registration.
    registry
        .ensure_announcing("synthetic-catalog", port)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    registry.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(start_paused = true)]
async fn retries_with_backoff_until_registered() {
    let dir = temp_dir();
    let (registry, calls, _) = registry_with(
        &dir,
        vec![
            Err("catalog down".into()),
            Err("catalog down".into()),
            Ok(CATALOG_ID),
        ],
    );
    registry.set_catalog_registration(true);
    let seed = SeedRegistry::new();
    let bc = registry
        .mint("synthetic-retry", &[], &seed, 8 << 20)
        .await
        .unwrap();
    let (_port_tx, port) = watch::channel(Some(4661));
    registry
        .ensure_announcing("synthetic-retry", port)
        .await
        .unwrap();

    let started = tokio::time::Instant::now();
    assert_eq!(wait_for_id(&registry, &bc.content_id).await, CATALOG_ID);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    // 30 s, then 60 s of backoff before the third attempt.
    assert!(started.elapsed() >= Duration::from_secs(90));

    registry.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn off_unless_enabled() {
    let dir = temp_dir();
    let (registry, calls, _) = registry_with(&dir, vec![Ok(CATALOG_ID)]);
    let seed = SeedRegistry::new();
    let bc = registry
        .mint("synthetic-off", &[], &seed, 8 << 20)
        .await
        .unwrap();
    let (_port_tx, port) = watch::channel(Some(4661));
    registry
        .ensure_announcing("synthetic-off", port)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(registry.catalog_content_id(&bc.content_id), None);

    registry.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(start_paused = true)]
async fn shutdown_cancels_a_pending_retry() {
    let dir = temp_dir();
    let (registry, calls, _) = registry_with(&dir, vec![Err("catalog down".into())]);
    registry.set_catalog_registration(true);
    let seed = SeedRegistry::new();
    registry
        .mint("synthetic-stop", &[], &seed, 8 << 20)
        .await
        .unwrap();
    let (_port_tx, port) = watch::channel(Some(4661));
    registry
        .ensure_announcing("synthetic-stop", port)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    registry.shutdown().await.unwrap();
    tokio::time::sleep(Duration::from_secs(3600)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "no retries after shutdown");
    let _ = std::fs::remove_dir_all(dir);
}
