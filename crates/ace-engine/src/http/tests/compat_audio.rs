//! `preferred_audio_language` on `/ace/getstream` and `/ace/manifest.m3u8`, over synthetic
//! multi-audio TS (see `crate::test_media`).
use super::*;
use crate::test_media::{audio_pids, first_pmt_order, Generator, AUDIO, VIDEO_PID};
use futures::StreamExt;
use std::sync::atomic::AtomicUsize;

const ID: &str = "0123456789abcdef0123456789abcdef01234567";
const SIX: [(u16, u8, &[u8; 3]); 6] = [
    (0x101, 0x0f, b"eng"),
    (0x102, 0x0f, b"rus"),
    (0x103, 0x0f, b"spa"),
    (0x104, 0x0f, b"fra"),
    (0x105, 0x0f, b"deu"),
    (0x106, 0x0f, b"ita"),
];

struct MultiAudioProvider {
    opens: Arc<AtomicUsize>,
    tracks: &'static [(u16, u8, &'static [u8; 3])],
}

struct MultiAudioSource(Generator);

#[async_trait]
impl TsSource for MultiAudioSource {
    async fn next(&mut self) -> Option<Bytes> {
        // One 40 ms frame per millisecond: segments complete quickly without spinning.
        tokio::time::sleep(Duration::from_millis(1)).await;
        let mut out = Vec::with_capacity(150 * 188);
        self.0.frame(&mut out);
        Some(out.into())
    }
    fn stats(&self) -> SourceStats {
        SourceStats::default()
    }
}

#[async_trait]
impl StreamProvider for MultiAudioProvider {
    fn network(&self) -> &'static str {
        "multi"
    }
    async fn open(&self, _id: &str) -> Result<Box<dyn TsSource>, ProviderError> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(MultiAudioSource(Generator::new(self.tracks))))
    }
}

fn audio_state(
    tracks: &'static [(u16, u8, &'static [u8; 3])],
    normalizer: Option<crate::hls_audio::PreparedNormalizer>,
) -> (AppState, Arc<AtomicUsize>) {
    let opens = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(MultiAudioProvider {
        opens: opens.clone(),
        tracks,
    }));
    let state = AppState {
        peer_policy: Default::default(),
        manager: StreamManager::with_normalizer(
            registry,
            1024,
            crate::config::HlsConfig {
                window_segments: 4,
                segment_duration_ms: 1000,
                startup_segments: 1,
                startup_timeout_ms: 5_000,
                ..crate::config::HlsConfig::default()
            },
            normalizer,
        ),
        networks: vec!["multi".into()],
        resolve_content_ids_in_getstream: false,
        ace_sessions: Arc::new(AceSessionStore::default()),
        experimental_ace_compat: true,
        search: None,
        origin_hls: None,
        broadcasts: None,
    };
    (state, opens)
}

/// Mint one compatibility HLS lease and return `(playlist path, token)`.
async fn manifest(app: &Router, language: Option<&str>) -> (String, String) {
    let query = language.map_or(String::new(), |l| format!("&preferred_audio_language={l}"));
    let response = app
        .clone()
        .oneshot(
            Request::get(format!("/ace/manifest.m3u8?infohash={ID}{query}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FOUND);
    let path = response.headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .strip_prefix("http://127.0.0.1:6878")
        .unwrap()
        .to_owned();
    let token = path
        .rsplit('/')
        .next()
        .unwrap()
        .trim_end_matches(".m3u8")
        .to_owned();
    (path, token)
}

/// The audio PIDs carried by the first retained segment of a lease's playlist.
async fn segment_audio(app: &Router, playlist: &str) -> Vec<u16> {
    let playlist = wait_for_hls_playlist(app, playlist).await;
    let response = app
        .clone()
        .oneshot(
            Request::get(playlist_segment_path(&playlist))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 64 << 20)
        .await
        .unwrap();
    audio_pids(&bytes)
}

#[tokio::test]
async fn hls_viewers_with_different_languages_share_one_upstream_session() {
    let (state, opens) = audio_state(&AUDIO, None);
    let manager = state.manager.clone();
    let app = router(state);
    let (default, _) = manifest(&app, None).await;
    let (rus, _) = manifest(&app, Some("rus")).await;
    let (rus_upper, _) = manifest(&app, Some("RUS")).await;
    // Matches only an MPEG audio track while ADTS AAC exists: native HLS keeps the default.
    let (spa, _) = manifest(&app, Some("spa")).await;
    let (unknown, _) = manifest(&app, Some("fra")).await;
    let (invalid, _) = manifest(&app, Some("ru1")).await;

    assert_eq!(segment_audio(&app, &default).await, vec![0x101]);
    assert_eq!(segment_audio(&app, &rus).await, vec![0x102]);
    assert_eq!(segment_audio(&app, &rus_upper).await, vec![0x102]);
    for fallback in [&spa, &unknown, &invalid] {
        assert_eq!(segment_audio(&app, fallback).await, vec![0x101]);
    }
    assert_eq!(opens.load(Ordering::SeqCst), 1, "one upstream download");
    let session = manager.get("multi", ID).await.unwrap();
    assert_eq!(session.subscriber_count(), 6, "one pin per lease");
    assert_eq!(
        manager.hls_variant_count("multi", ID),
        1,
        "`rus` and `RUS` share a variant; fallbacks share the default packager"
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn hls_variants_are_capped_per_stream_and_extra_languages_get_the_default() {
    let (state, opens) = audio_state(&SIX, None);
    let manager = state.manager.clone();
    let app = router(state);
    let mut leases = Vec::new();
    for language in ["rus", "spa", "fra", "ger", "ita"] {
        leases.push(manifest(&app, Some(language)).await.0);
    }
    assert_eq!(
        manager.hls_variant_count("multi", ID),
        crate::manager::MAX_AUDIO_VARIANTS
    );
    // `ger` folds to the track tagged `deu`.
    let expected = [0x102, 0x103, 0x104, 0x105, 0x101];
    for (playlist, pid) in leases.iter().zip(expected) {
        assert_eq!(segment_audio(&app, playlist).await, vec![pid]);
    }
    // An existing variant is still shared at the cap.
    let (again, _) = manifest(&app, Some("rus")).await;
    assert_eq!(segment_audio(&app, &again).await, vec![0x102]);
    assert_eq!(opens.load(Ordering::SeqCst), 1);
    manager.shutdown().await;
}

#[tokio::test]
async fn hls_variant_is_retired_after_its_last_lease_stops() {
    let (state, _) = audio_state(&AUDIO, None);
    let manager = state.manager.clone();
    let app = router(state);
    let (default, _) = manifest(&app, None).await;
    let (rus, token) = manifest(&app, Some("rus")).await;
    assert_eq!(segment_audio(&app, &rus).await, vec![0x102]);
    let variant = manager.get_hls_variant("multi", ID, 0x102).unwrap();

    // Another lease on the variant keeps it through a sweep.
    let (second, second_token) = manifest(&app, Some("rus")).await;
    let stop = |token: String| {
        let app = app.clone();
        async move {
            let response = app
                .oneshot(
                    Request::get(format!("/ace/cmd/{ID}/{token}?method=stop"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(response_text(response).await.contains("\"ok\""));
        }
    };
    stop(token).await;
    manager.sweep_idle_for_test().await;
    assert_eq!(manager.hls_variant_count("multi", ID), 1);
    assert_eq!(segment_audio(&app, &second).await, vec![0x102]);

    stop(second_token).await;
    manager.sweep_idle_for_test().await;
    assert_eq!(manager.hls_variant_count("multi", ID), 0);
    assert_eq!(variant.status(), crate::hls::HlsStatus::Stopped);
    // The shared session and the default viewer are untouched.
    assert_eq!(segment_audio(&app, &default).await, vec![0x101]);
    // A later request starts a fresh variant on the same session.
    let (fresh, _) = manifest(&app, Some("rus")).await;
    assert_eq!(segment_audio(&app, &fresh).await, vec![0x102]);
    manager.shutdown().await;
}

#[tokio::test]
async fn normalized_hls_ignores_the_language_preference() {
    let (state, _) = audio_state(
        &AUDIO,
        Some(crate::hls_audio::PreparedNormalizer {
            executable: "/not/an/executable".into(),
        }),
    );
    let manager = state.manager.clone();
    let app = router(state);
    // Normalization owns the audio selection: no wait for tables, no extra packager or child.
    let started = std::time::Instant::now();
    manifest(&app, Some("rus")).await;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(manager.hls_variant_count("multi", ID), 0);
    manager.shutdown().await;
}

/// Read a direct TS body until it carries a PMT and return that PMT's elementary order.
async fn ts_pmt_order(response: Response) -> Vec<u16> {
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    let mut bytes = Vec::new();
    loop {
        bytes.extend_from_slice(&body.next().await.unwrap().unwrap());
        if let Some(order) = first_pmt_order(&bytes[..bytes.len() / 188 * 188]) {
            return order;
        }
    }
}

#[tokio::test]
async fn direct_and_json_ts_reorder_only_the_requesting_viewers_pmt() {
    let (state, opens) = audio_state(&AUDIO, None);
    let manager = state.manager.clone();
    let app = router(state);
    let get = |query: String| {
        let app = app.clone();
        async move {
            app.oneshot(
                Request::get(format!("/ace/getstream?infohash={ID}{query}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    };
    let source = vec![VIDEO_PID, 0x101, 0x102, 0x103];
    assert_eq!(ts_pmt_order(get(String::new()).await).await, source);
    let preferred = vec![VIDEO_PID, 0x102, 0x101, 0x103];
    assert_eq!(
        ts_pmt_order(get("&preferred_audio_language=Rus".into()).await).await,
        preferred
    );
    // Any recognized codec may lead in TS: every track is still carried.
    assert_eq!(
        ts_pmt_order(get("&preferred_audio_language=spa".into()).await).await,
        vec![VIDEO_PID, 0x103, 0x101, 0x102]
    );
    for ignored in ["fra", "russian", ""] {
        let query = format!("&preferred_audio_language={ignored}");
        assert_eq!(ts_pmt_order(get(query).await).await, source, "{ignored}");
    }
    // JSON mode carries the preference on its lease to the returned playback URL.
    let json = get("&format=json&preferred_audio_language=rus".into()).await;
    let value: serde_json::Value = serde_json::from_str(&response_text(json).await).unwrap();
    let playback = value["response"]["playback_url"]
        .as_str()
        .unwrap()
        .strip_prefix("http://127.0.0.1:6878")
        .unwrap()
        .to_owned();
    let response = app
        .clone()
        .oneshot(Request::get(playback).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(ts_pmt_order(response).await, preferred);
    assert_eq!(
        opens.load(Ordering::SeqCst),
        1,
        "all viewers share one source"
    );
    manager.shutdown().await;
}
