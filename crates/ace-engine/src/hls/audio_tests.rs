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
    assert!(!before.is_empty());
    assert_eq!(audio_pids(&before), vec![0x102]);
    generator.set_tracks(&[(0x101, 0x0f, b"eng"), (0x104, 0x0f, b"rus")]);
    feed_frames(&pkg, &mut generator, 120);
    assert_eq!(pkg.status(), HlsStatus::Failed(HlsFailure::Unsupported));
    // Nothing after the change was published: not even a segment with the old PMT.
    let after = all_segments(&pkg);
    assert_eq!(
        after, before,
        "no segment published after the PID left the PMT"
    );
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

#[test]
fn unrelated_scrambling_before_the_first_pmt_does_not_fail_a_variant() {
    let pkg = HlsPackager::new(config(), None);
    pkg.set_audio_track(0x102);
    // A scrambled packet on an unrelated PID, before any PAT or PMT.
    let mut scrambled = [0xffu8; TS_PACKET];
    scrambled[..4].copy_from_slice(&[0x47, 0x01, 0xff, 0x90]);
    pkg.feed(&scrambled);
    assert_ne!(pkg.status(), HlsStatus::Failed(HlsFailure::Unsupported));
    let mut generator = Generator::new(&AUDIO);
    feed_frames(&pkg, &mut generator, 60);
    assert_eq!(
        pkg.status(),
        HlsStatus::Ready,
        "the valid PMT that follows is used"
    );
    assert_eq!(audio_pids(&all_segments(&pkg)), vec![0x102]);
}

#[tokio::test]
async fn unrelated_scrambling_before_the_first_pmt_keeps_language_resolution_pending() {
    let pkg = HlsPackager::new(config(), None);
    pkg.state.lock().unwrap().status = HlsStatus::Ready;
    let language = AudioLanguage::parse("rus").unwrap();
    // A scrambled packet on an unrelated PID marks normalization unsupported, but says
    // nothing about the PMT: the language stays pending instead of falling back.
    let mut scrambled = [0xffu8; TS_PACKET];
    scrambled[..4].copy_from_slice(&[0x47, 0x01, 0xff, 0x90]);
    pkg.feed(&scrambled);
    assert_eq!(pkg.audio_choice(language), AudioChoice::Pending);
    let waiter = pkg.wait_audio_choice(language, Duration::from_secs(30));
    tokio::pin!(waiter);
    assert!(futures::poll!(waiter.as_mut()).is_pending());
    let mut generator = Generator::new(&AUDIO);
    feed_frames(&pkg, &mut generator, 1);
    let choice = tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .expect("resolved by the PMT that follows");
    assert_eq!(choice, AudioChoice::Track(0x102));
}

#[tokio::test]
async fn audio_choice_waiter_wakes_when_unsupported_psi_resolves_tables() {
    let pkg = HlsPackager::new(config(), None);
    // A Ready packager publishes no further readiness notifications, so only the resolution
    // transition can wake this waiter.
    pkg.state.lock().unwrap().status = HlsStatus::Ready;
    let language = AudioLanguage::parse("rus").unwrap();
    // Poll the waiter itself to Pending, so it is registered for notification before the PAT.
    let waiter = pkg.wait_audio_choice(language, Duration::from_secs(30));
    tokio::pin!(waiter);
    assert!(futures::poll!(waiter.as_mut()).is_pending());
    // A two-program PAT: selection is disabled, no PMT is stored, `changed` stays false.
    let packet = crate::test_media::psi(
        0,
        vec![0, 0, 0, 0, 1, 0xc1, 0, 0, 0, 1, 0xe1, 0, 0, 2, 0xe1, 1],
    );
    pkg.feed(&packet);
    let choice = tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .expect("woken by the resolution transition, not the 30 s deadline");
    assert_eq!(choice, AudioChoice::Default);
    // Repeats stay silent: a fresh waiter returns at once from state, not from a notification.
    pkg.feed(&packet);
    assert_eq!(pkg.audio_choice(language), AudioChoice::Default);
}
