//! Bounded synthetic benchmark of native live HLS packaging (one packager's feed path).
//! Run: `cargo test --release --locked -p ace-engine --lib hls_packaging_measurement -- --ignored --nocapture`
use super::*;
use crate::test_media::stream;
use std::hint::black_box;

type Setup = fn() -> Arc<HlsPackager>;

fn config() -> HlsConfig {
    HlsConfig {
        segment_duration_ms: 2000,
        ..HlsConfig::default()
    }
}

fn cases() -> Vec<(&'static str, Setup)> {
    vec![
        ("default", || {
            HlsPackager::new(config(), Some("Synthetic".into()))
        }),
        ("variant_rus", || {
            let pkg = HlsPackager::new(config(), Some("Synthetic".into()));
            pkg.set_audio_language(ace_media::audio::AudioLanguage::parse("rus").unwrap());
            pkg
        }),
    ]
}

#[test]
#[ignore = "bounded synthetic HLS packaging benchmark; run explicitly in release mode"]
fn hls_packaging_measurement() {
    // 60 s of synthetic media (~40 MB) in 64-packet chunks, like the packager's receiver.
    let bytes = stream(1500);
    let chunk = 64 * 188;
    for (name, setup) in cases() {
        let mut best = f64::MAX;
        let mut segments = 0;
        for _ in 0..15 {
            let pkg = setup();
            let start = Instant::now();
            for piece in bytes.chunks(chunk) {
                pkg.feed(black_box(piece));
            }
            best = best.min(start.elapsed().as_secs_f64());
            let st = pkg.state.lock().unwrap();
            segments = st.media_seq as usize + st.segments.len();
        }
        assert!(segments > 20, "the synthetic stream must produce segments");
        println!(
            "{{\"case\":\"{name}\",\"bytes\":{},\"segments\":{segments},\"best_s\":{best:.6},\"mib_per_s\":{:.0},\"ns_per_packet\":{:.2}}}",
            bytes.len(),
            bytes.len() as f64 / best / (1024.0 * 1024.0),
            best * 1e9 / (bytes.len() / 188) as f64
        );
    }
}
