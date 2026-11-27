//! Audio-language behavior of one packager: fixed-track variants and resolution waiters.
use super::*;
use crate::test_media::{audio_pids, Generator, AUDIO, PMT_PID};

fn config() -> HlsConfig {
    HlsConfig {
        segment_duration_ms: 1000,
        startup_segments: 1,
        window_segments: 64,
        ..HlsConfig::default()
    }
}

fn feed_frames(pkg: &HlsPackager, generator: &mut Generator, frames: usize) {
    let mut out = Vec::new();
    for _ in 0..frames {
        generator.frame(&mut out);
    }
    pkg.feed(&out);
}

fn all_segments(pkg: &HlsPackager) -> Vec<u8> {
    let st = pkg.state.lock().unwrap();
    st.segments
        .iter()
        .flat_map(|s| s.bytes.iter().copied())
        .collect()
}

#[test]
fn variant_keeps_its_keyed_pid_when_the_language_moves() {
    let pkg = HlsPackager::new(config(), None);
    pkg.set_audio_track(0x102);
    let mut generator = Generator::new(&AUDIO);
    feed_frames(&pkg, &mut generator, 60);
    // `rus` moves to 0x104 while 0x102 stays in the PMT (now tagged `spa`).
    generator.set_tracks(&[
        (0x101, 0x0f, b"eng"),
        (0x102, 0x0f, b"spa"),
        (0x104, 0x0f, b"rus"),
    ]);
    feed_frames(&pkg, &mut generator, 120);
    assert_eq!(pkg.status(), HlsStatus::Ready);
    assert_eq!(audio_pids(&all_segments(&pkg)), vec![0x102]);
}

#[test]
fn variant_fails_instead_of_switching_when_its_pid_leaves_the_pmt() {
    let pkg = HlsPackager::new(config(), None);
    pkg.set_audio_track(0x102);
    let mut generator = Generator::new(&AUDIO);
    feed_frames(&pkg, &mut generator, 60);
    let before = all_segments(&pkg);
    assert_eq!(audio_pids(&before), vec![0x102]);
    generator.set_tracks(&[(0x101, 0x0f, b"eng"), (0x104, 0x0f, b"rus")]);
    feed_frames(&pkg, &mut generator, 120);
    assert_eq!(pkg.status(), HlsStatus::Failed(HlsFailure::Unsupported));
    // Nothing after the change was published: no segment carries another track.
    let after = all_segments(&pkg);
    assert_eq!(audio_pids(&after), vec![0x102]);
    for packet in after.as_chunks::<TS_PACKET>().0 {
        if ts_pid(packet) == PMT_PID {
            let order = crate::test_media::first_pmt_order(packet).unwrap();
            assert_eq!(
                order,
                vec![0x100, 0x102],
                "only the keyed track is ever listed"
            );
        }
    }
}

#[tokio::test]
async fn audio_choice_waiter_wakes_when_unsupported_psi_resolves_tables() {
    let pkg = HlsPackager::new(config(), None);
    // A Ready packager publishes no further readiness notifications, so only the resolution
    // transition can wake this waiter.
    pkg.state.lock().unwrap().status = HlsStatus::Ready;
    let language = AudioLanguage::parse("rus").unwrap();
    let waiter = {
        let pkg = pkg.clone();
        tokio::spawn(async move {
            pkg.wait_audio_choice(language, Duration::from_secs(30))
                .await
        })
    };
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished());
    // A two-program PAT: selection is disabled, no PMT is stored, `changed` stays false.
    let mut section = vec![0, 0, 0, 0, 1, 0xc1, 0, 0, 0, 1, 0xe1, 0, 0, 2, 0xe1, 1];
    let packet = crate::test_media::psi(0, std::mem::take(&mut section));
    pkg.feed(&packet);
    let choice = tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .expect("woken by the resolution transition, not the 30 s deadline")
        .unwrap();
    assert_eq!(choice, AudioChoice::Default);
    // Repeats stay silent: a fresh waiter returns at once from state, not from a notification.
    pkg.feed(&packet);
    assert_eq!(pkg.audio_choice(language), AudioChoice::Default);
}
