//! A TS `format=json` lease warms its source on the first stat poll (issue #557): clients such
//! as Horus wait for `status: "dl"` before they open `playback_url`.
use super::*;
use std::sync::atomic::AtomicUsize;

const ID: &str = "0123456789abcdef0123456789abcdef01234567";

/// Every test is bounded, so a regression fails instead of hanging the suite.
async fn bounded(test: impl std::future::Future<Output = ()>) {
    tokio::time::timeout(Duration::from_secs(30), test)
        .await
        .expect("test timed out");
}

/// What every open of the test provider does.
#[derive(Clone, Copy)]
enum Outcome {
    Play,
    Fail,
    /// Resolves late to an HLS origin, as a TS lease can.
    Origin,
}

/// Counts opens. With `release`, the first open waits for it; then it does `outcome`.
struct WarmProvider {
    opens: Arc<AtomicUsize>,
    release: Option<Arc<tokio::sync::Notify>>,
    outcome: Outcome,
}

#[async_trait]
impl StreamProvider for WarmProvider {
    fn network(&self) -> &'static str {
        "fix"
    }

    async fn open(&self, _id: &str) -> Result<Box<dyn TsSource>, ProviderError> {
        let first = self.opens.fetch_add(1, Ordering::SeqCst) == 0;
        if let (true, Some(release)) = (first, &self.release) {
            release.notified().await;
        }
        match self.outcome {
            Outcome::Play => {}
            Outcome::Fail => return Err(ProviderError::NoPeers("synthetic".into())),
            Outcome::Origin => {
                return Err(ProviderError::DirectHls(Box::new(
                    ace_swarm::resolve::HlsOrigin {
                        manifest_url: "https://cdn.example.test/master.m3u8".into(),
                        metadata: StreamMetadata::default(),
                    },
                )))
            }
        }
        // Loops, so a viewer that joins a warmed source late still receives media.
        Ok(Box::new(PacedFixtureSource { pos: 0 }))
    }
}

struct Warm {
    state: AppState,
    app: Router,
    opens: Arc<AtomicUsize>,
    release: Arc<tokio::sync::Notify>,
}

fn warm_state(pending: bool, outcome: Outcome, sessions: AceSessionStore) -> Warm {
    let opens = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(WarmProvider {
        opens: opens.clone(),
        release: pending.then(|| release.clone()),
        outcome,
    }));
    let mut state = ace_compat_state(0);
    state.manager = StreamManager::new(registry);
    state.ace_sessions = Arc::new(sessions);
    Warm {
        app: router(state.clone()),
        state,
        opens,
        release,
    }
}

async fn get(app: &Router, path: &str) -> Response {
    app.clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn get_json(app: &Router, path: &str) -> serde_json::Value {
    let response = get(app, path).await;
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&response_bytes(response).await).unwrap()
}

/// The returned URLs, as paths. Clients must treat them as opaque, as this does.
struct Lease {
    playback: String,
    stat: String,
    command: String,
    token: String,
}

async fn mint(app: &Router) -> Lease {
    let minted = get_json(app, &format!("/ace/getstream?format=json&id={ID}")).await;
    assert_eq!(minted["error"], serde_json::Value::Null);
    let path = |key: &str| {
        let url = minted["response"][key].as_str().unwrap();
        url[url.find("/ace/").unwrap()..].to_owned()
    };
    Lease {
        playback: path("playback_url"),
        stat: path("stat_url"),
        command: path("command_url"),
        token: minted["response"]["playback_session_id"]
            .as_str()
            .unwrap()
            .to_owned(),
    }
}

/// Poll like Horus does, collecting each reported status, until `done` holds.
async fn poll_until(
    app: &Router,
    path: &str,
    done: impl Fn(&serde_json::Value) -> bool,
) -> (serde_json::Value, Vec<String>) {
    let mut statuses = Vec::new();
    for _ in 0..1000 {
        let json = get_json(app, path).await;
        statuses.push(json["response"]["status"].as_str().unwrap_or("").to_owned());
        if done(&json) {
            return (json, statuses);
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("stat never reached the expected state: {statuses:?}");
}

fn is_dl(json: &serde_json::Value) -> bool {
    json["response"]["status"] == "dl"
}

async fn session(state: &AppState) -> Option<Arc<StreamSession>> {
    state.manager.get("fix", &format!("cid:{ID}")).await
}

#[tokio::test]
async fn horus_style_client_starts_a_cold_stream_with_one_open() {
    bounded(async {
        use futures::StreamExt;
        let warm = warm_state(false, Outcome::Play, AceSessionStore::default());
        let lease = mint(&warm.app).await;
        assert_eq!(
            warm.opens.load(Ordering::SeqCst),
            0,
            "minting opens nothing"
        );

        let (_, statuses) = poll_until(&warm.app, &lease.stat, is_dl).await;
        assert_eq!(statuses[0], "prebuf", "the first poll starts the source");
        assert!(statuses
            .iter()
            .all(|status| status == "prebuf" || status == "dl"));
        for _ in 0..5 {
            assert!(is_dl(&get_json(&warm.app, &lease.stat).await));
        }
        let held = session(&warm.state).await.unwrap();
        assert_eq!(held.subscriber_count(), 0, "the warm hold is not a viewer");
        assert_eq!(held.support_pin_count(), 1);

        let media = get(&warm.app, &lease.playback).await;
        assert_eq!(media.status(), StatusCode::OK);
        let mut body = media.into_body().into_data_stream();
        assert_eq!(body.next().await.unwrap().unwrap()[0], 0x47);
        assert_eq!(held.subscriber_count(), 1);
        assert_eq!(held.support_pin_count(), 0, "playback takes over the hold");
        assert!(is_dl(&get_json(&warm.app, &lease.stat).await));
        assert_eq!(warm.opens.load(Ordering::SeqCst), 1);
        drop(body);
        warm.state.manager.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn a_json_mint_that_is_never_polled_opens_nothing() {
    bounded(async {
        let warm = warm_state(false, Outcome::Play, AceSessionStore::default());
        let _lease = mint(&warm.app).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(warm.opens.load(Ordering::SeqCst), 0);
        assert!(warm.state.manager.list().await.is_empty());
    })
    .await;
}

#[tokio::test]
async fn concurrent_first_polls_start_the_source_once() {
    bounded(async {
        let warm = warm_state(true, Outcome::Play, AceSessionStore::default());
        let lease = mint(&warm.app).await;
        let polls = (0..8).map(|_| get_json(&warm.app, &lease.stat));
        for json in futures::future::join_all(polls).await {
            assert_eq!(json["response"]["status"], "prebuf");
        }
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        assert_eq!(warm.opens.load(Ordering::SeqCst), 1);
        warm.release.notify_one();
        poll_until(&warm.app, &lease.stat, is_dl).await;
        assert_eq!(warm.opens.load(Ordering::SeqCst), 1);
        warm.state.manager.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn stopping_a_polled_lease_releases_its_hold_to_idle_cleanup() {
    bounded(async {
        let warm = warm_state(false, Outcome::Play, AceSessionStore::default());
        let lease = mint(&warm.app).await;
        poll_until(&warm.app, &lease.stat, is_dl).await;
        assert!(warm.state.ace_sessions.warm_held(&lease.token));
        let held = session(&warm.state).await.unwrap();

        let stopped = get_json(&warm.app, &format!("{}?method=stop", lease.command)).await;
        assert_eq!(stopped["response"], "ok");
        assert_eq!(held.support_pin_count(), 0);
        warm.state
            .manager
            .reap_idle_at(Instant::now() + Duration::from_secs(60))
            .await;
        assert!(session(&warm.state).await.is_none());
        warm.state.manager.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn stopping_a_lease_cancels_its_pending_warm_start() {
    bounded(async {
        let warm = warm_state(true, Outcome::Play, AceSessionStore::default());
        let lease = mint(&warm.app).await;
        assert_eq!(
            get_json(&warm.app, &lease.stat).await["response"]["status"],
            "prebuf"
        );
        for _ in 0..1000 {
            if warm.opens.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(warm.opens.load(Ordering::SeqCst), 1);
        let stopped = get_json(&warm.app, &format!("{}?method=stop", lease.command)).await;
        assert_eq!(stopped["response"], "ok");
        // The abandoned open is disposed when it completes, never published.
        warm.release.notify_one();
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        assert!(warm.state.manager.list().await.is_empty());
        warm.state.manager.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn an_expired_polled_lease_releases_its_hold_without_further_requests() {
    bounded(async {
        // Real clock: lease deadlines use the standard clock.
        let warm = warm_state(
            false,
            Outcome::Play,
            AceSessionStore::new(Duration::from_millis(300), 8),
        );
        let lease = mint(&warm.app).await;
        poll_until(&warm.app, &lease.stat, is_dl).await;
        let held = session(&warm.state).await.unwrap();
        assert_eq!(held.support_pin_count(), 1);
        // Nothing touches the lease store: the lease reaper alone must end the hold.
        for _ in 0..200 {
            if held.support_pin_count() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(held.support_pin_count(), 0);
        assert_eq!(warm.state.ace_sessions.active_count(), 0);
        warm.state
            .manager
            .reap_idle_at(Instant::now() + Duration::from_secs(60))
            .await;
        assert!(session(&warm.state).await.is_none());
        warm.state.manager.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn a_failed_warm_start_reports_an_in_band_error_once() {
    bounded(async {
        let warm = warm_state(false, Outcome::Fail, AceSessionStore::default());
        let lease = mint(&warm.app).await;
        let (failed, statuses) =
            poll_until(&warm.app, &lease.stat, |json| json["error"].is_string()).await;
        assert_eq!(statuses[0], "prebuf");
        assert_eq!(failed["response"], serde_json::Value::Null);
        assert_eq!(failed["error"], "no peer is sharing this stream");
        for _ in 0..5 {
            assert_eq!(
                get_json(&warm.app, &lease.stat).await["error"],
                failed["error"]
            );
        }
        assert_eq!(
            warm.opens.load(Ordering::SeqCst),
            1,
            "a failed lease is not retried by polls"
        );
        assert!(warm.state.manager.list().await.is_empty());
        warm.state.manager.shutdown().await;
    })
    .await;
}

/// A short poll idle bound for the idle-release tests; lease deadlines use the real clock.
fn idle_bounded(pending: bool) -> Warm {
    warm_state(
        pending,
        Outcome::Play,
        AceSessionStore::default().with_warm_poll_idle(Duration::from_millis(200)),
    )
}

async fn wait_for_pins(held: &StreamSession, pins: u64) {
    for _ in 0..300 {
        if held.support_pin_count() == pins {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("support pins never reached {pins}");
}

#[tokio::test]
async fn an_unpolled_warm_hold_is_released_after_the_poll_idle_bound() {
    bounded(async {
        let warm = idle_bounded(false);
        let lease = mint(&warm.app).await;
        poll_until(&warm.app, &lease.stat, is_dl).await;
        let held = session(&warm.state).await.unwrap();
        assert_eq!(held.support_pin_count(), 1);
        // No further requests: the lease reaper alone ends the hold.
        wait_for_pins(&held, 0).await;
        assert!(!warm.state.ace_sessions.warm_held(&lease.token));
        assert_eq!(
            warm.state.ace_sessions.active_count(),
            1,
            "the lease itself stays valid"
        );
        warm.state
            .manager
            .reap_idle_at(Instant::now() + Duration::from_secs(60))
            .await;
        assert!(session(&warm.state).await.is_none());
        warm.state.manager.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn regular_polls_keep_a_warm_hold_past_the_poll_idle_bound() {
    bounded(async {
        let warm = idle_bounded(false);
        let lease = mint(&warm.app).await;
        poll_until(&warm.app, &lease.stat, is_dl).await;
        let held = session(&warm.state).await.unwrap();
        let until = Instant::now() + Duration::from_millis(700);
        while Instant::now() < until {
            assert!(is_dl(&get_json(&warm.app, &lease.stat).await));
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        assert!(warm.state.ace_sessions.warm_held(&lease.token));
        assert_eq!(held.support_pin_count(), 1);
        assert_eq!(warm.opens.load(Ordering::SeqCst), 1);
        warm.state.manager.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn a_poll_after_idle_release_warms_the_lease_again() {
    bounded(async {
        let warm = idle_bounded(false);
        let lease = mint(&warm.app).await;
        poll_until(&warm.app, &lease.stat, is_dl).await;
        let held = session(&warm.state).await.unwrap();
        wait_for_pins(&held, 0).await;
        warm.state
            .manager
            .reap_idle_at(Instant::now() + Duration::from_secs(60))
            .await;
        assert!(session(&warm.state).await.is_none());

        let (_, statuses) = poll_until(&warm.app, &lease.stat, is_dl).await;
        assert_eq!(
            statuses[0], "prebuf",
            "the resumed poll starts the source again"
        );
        assert!(warm.state.ace_sessions.warm_held(&lease.token));
        assert_eq!(
            warm.opens.load(Ordering::SeqCst),
            2,
            "exactly one more open"
        );
        warm.state.manager.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn the_poll_idle_bound_cancels_a_pending_warm_start() {
    bounded(async {
        let warm = idle_bounded(true);
        let lease = mint(&warm.app).await;
        assert_eq!(
            get_json(&warm.app, &lease.stat).await["response"]["status"],
            "prebuf"
        );
        // Past the bound with no polls, the reaper cancels the pending open.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(warm.opens.load(Ordering::SeqCst), 1);
        warm.release.notify_one();
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        assert!(
            warm.state.manager.list().await.is_empty(),
            "nothing published"
        );
        assert_eq!(warm.state.ace_sessions.active_count(), 1);
        warm.state.manager.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn a_warm_start_that_resolves_to_an_hls_origin_reports_dl_and_redirects() {
    bounded(async {
        let warm = warm_state(false, Outcome::Origin, AceSessionStore::default());
        let lease = mint(&warm.app).await;
        let (_, statuses) = poll_until(&warm.app, &lease.stat, is_dl).await;
        assert_eq!(statuses[0], "prebuf");
        // Nothing local is held, but playback_url works, so a client waiting for dl proceeds.
        assert!(!warm.state.ace_sessions.warm_held(&lease.token));
        assert!(warm.state.manager.list().await.is_empty());
        let playback = get(&warm.app, &lease.playback).await;
        assert!(playback.status().is_redirection(), "{}", playback.status());
        warm.state.manager.shutdown().await;
    })
    .await;
}
