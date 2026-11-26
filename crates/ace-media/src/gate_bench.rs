//! Bounded synthetic benchmark of the per-viewer live TS gate (`/ace/getstream` fanout).
//! Run: `cargo test --release --locked -p ace-media ts_gate_measurement -- --ignored --nocapture`
use crate::mpegts::KeyframeGate;
use crate::test_stream::stream;
use std::hint::black_box;
use std::time::Instant;

type Setup = fn() -> KeyframeGate;

fn cases() -> Vec<(&'static str, Setup)> {
    vec![("default", || {
        KeyframeGate::with_service_name(Some("Synthetic".into()))
    })]
}

#[test]
#[ignore = "bounded synthetic TS gate benchmark; run explicitly in release mode"]
fn ts_gate_measurement() {
    // 60 s of synthetic media (~40 MB) in 64-packet chunks, like a viewer's fanout.
    let bytes = stream(1500);
    let chunk = 64 * 188;
    for (name, setup) in cases() {
        let mut best = f64::MAX;
        let mut out_bytes = 0;
        for _ in 0..15 {
            let mut gate = setup();
            let start = Instant::now();
            let mut emitted = 0;
            for piece in bytes.chunks(chunk) {
                emitted += black_box(gate.push(black_box(piece))).len();
            }
            best = best.min(start.elapsed().as_secs_f64());
            out_bytes = emitted;
        }
        println!(
            "{{\"case\":\"{name}\",\"bytes\":{},\"out\":{out_bytes},\"best_s\":{best:.6},\"mib_per_s\":{:.0},\"ns_per_packet\":{:.2}}}",
            bytes.len(),
            bytes.len() as f64 / best / (1024.0 * 1024.0),
            best * 1e9 / (bytes.len() / 188) as f64
        );
    }
}
