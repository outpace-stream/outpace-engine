//! Bounded native HLS audio-language variants for compatibility `preferred_audio_language`.
//!
//! A variant is an extra [`HlsPackager`] on the stream's one shared [`StreamSession`]: it reads
//! the same broadcast (no second upstream download) and differs only in which audio track it
//! keeps. A manifest request resolves its language to an elementary PID once, against the
//! default packager's current PMT; the variant is keyed by that PID and always carries exactly
//! that PID ([`HlsPackager::set_audio_track`]). Languages that resolve to the default track
//! share the default packager, and synonyms share one variant.
//!
//! Rules:
//! - If the keyed PID leaves the PMT, the variant fails. A failed variant is treated as gone:
//!   its leases get the unavailable response and the next request for that PID creates a fresh
//!   variant. It never switches to another track.
//! - A lease resolves only to the exact entry it pinned (by entry id), never to a later variant
//!   with the same PID. Stopping the stream removes every variant, so their leases get the
//!   unavailable response until the client requests a new manifest.
//! - A variant belongs to the session it was created on. A replacement session for the same
//!   stream never receives it; pinned variants of a finished session keep serving their leases
//!   until those end and do not count toward the replacement's cap.
//! - At most [`MAX_AUDIO_VARIANTS`] run per session, and at most
//!   [`MAX_STREAM_AUDIO_VARIANTS`] per stream counting pinned variants of replaced sessions. Unpinned variants are retired by the idle
//!   sweep, on stop and on shutdown.

use super::*;
use ace_media::audio::AudioLanguage;

/// Distinct non-default audio variants per session. Each costs one packager: its retained
/// segment window (bounded like the default packager's) plus one broadcast receiver.
pub(crate) const MAX_AUDIO_VARIANTS: usize = 4;

/// Hard bound on variants a stream holds across sessions. Pinned variants of replaced sessions
/// do not count toward the current session's [`MAX_AUDIO_VARIANTS`], so repeated replacements
/// within one lease lifetime could otherwise stack them; past this total, new languages get
/// the default packager until old leases end.
pub(crate) const MAX_STREAM_AUDIO_VARIANTS: usize = 2 * MAX_AUDIO_VARIANTS;

/// Whether a new variant fits: under the per-session cap and the per-stream total.
fn admits(on_session: usize, total: usize) -> bool {
    on_session < MAX_AUDIO_VARIANTS && total < MAX_STREAM_AUDIO_VARIANTS
}

#[derive(Default)]
pub(super) struct HlsVariants {
    sets: StdMutex<HashMap<StreamKey, VariantSet>>,
    next_id: std::sync::atomic::AtomicU64,
}

#[derive(Default)]
struct VariantSet {
    entries: Vec<VariantEntry>,
    cap_logged: bool,
}

struct VariantEntry {
    id: u64,
    pid: u16,
    session: Weak<StreamSession>,
    entry: HlsEntry,
    pins: Weak<()>,
}

impl VariantEntry {
    fn on(&self, session: &Arc<StreamSession>) -> bool {
        std::ptr::eq(self.session.as_ptr(), Arc::as_ptr(session))
    }

    fn failed(&self) -> bool {
        matches!(self.entry.media.status(), crate::hls::HlsStatus::Failed(_))
    }

    fn pinned(&self) -> bool {
        self.pins.strong_count() > 0
    }
}

/// Held by a compatibility HLS lease: keeps its exact variant entry from being retired.
pub struct HlsVariantPin {
    id: u64,
    _pin: Arc<()>,
}

impl HlsVariantPin {
    pub(crate) fn id(&self) -> u64 {
        self.id
    }
}

impl HlsVariants {
    /// The packager of entry `id`, unless it was removed or has failed.
    fn get(&self, key: &StreamKey, id: u64) -> Option<Arc<HlsPackager>> {
        let sets = self.sets.lock().unwrap();
        sets.get(key)?
            .entries
            .iter()
            .find(|variant| variant.id == id && !variant.failed())
            .map(|variant| variant.entry.media.clone())
    }

    /// Pin the `pid` variant of `session`, creating it under the cap. Failed variants, and
    /// unpinned variants of any session, are retired first, so the cap is a hard bound; they
    /// are returned for the caller to fence and cancel.
    fn acquire(
        &self,
        key: &StreamKey,
        pid: u16,
        session: &Arc<StreamSession>,
        create: impl FnOnce() -> HlsEntry,
    ) -> (Option<HlsVariantPin>, Vec<HlsEntry>) {
        let mut sets = self.sets.lock().unwrap();
        let set = sets.entry(key.clone()).or_default();
        let (keep, gone): (Vec<_>, Vec<_>) = std::mem::take(&mut set.entries)
            .into_iter()
            .partition(|v| !v.failed() && (v.pinned() || (v.on(session) && v.pid == pid)));
        set.entries = keep;
        let retired = gone.into_iter().map(|variant| variant.entry).collect();
        if let Some(variant) = set
            .entries
            .iter_mut()
            .find(|v| v.pid == pid && v.on(session))
        {
            let pin = variant.pins.upgrade().unwrap_or_else(|| {
                let pin = Arc::new(());
                variant.pins = Arc::downgrade(&pin);
                pin
            });
            let id = variant.id;
            return (Some(HlsVariantPin { id, _pin: pin }), retired);
        }
        let on_session = set.entries.iter().filter(|v| v.on(session)).count();
        if !admits(on_session, set.entries.len()) {
            if !std::mem::replace(&mut set.cap_logged, true) {
                crate::alog!(
                    "[hls] audio variant limit reached ({MAX_AUDIO_VARIANTS} per session, \
                     {MAX_STREAM_AUDIO_VARIANTS} per stream); further languages get the \
                     default audio track"
                );
            }
            return (None, retired);
        }
        let pin = Arc::new(());
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        set.entries.push(VariantEntry {
            id,
            pid,
            session: Arc::downgrade(session),
            entry: create(),
            pins: Arc::downgrade(&pin),
        });
        (Some(HlsVariantPin { id, _pin: pin }), retired)
    }

    pub(super) fn take_stream(&self, key: &StreamKey) -> Vec<HlsEntry> {
        self.sets
            .lock()
            .unwrap()
            .remove(key)
            .into_iter()
            .flat_map(|set| set.entries)
            .map(|variant| variant.entry)
            .collect()
    }

    pub(super) fn take_all(&self) -> Vec<(StreamKey, HlsEntry)> {
        let mut sets = self.sets.lock().unwrap();
        sets.drain()
            .flat_map(|(key, set)| {
                set.entries
                    .into_iter()
                    .map(move |variant| (key.clone(), variant.entry))
            })
            .collect()
    }

    /// Remove every variant of a stream with no live session, and every variant that has failed
    /// or that no lease pins any more.
    pub(super) fn take_idle(&self, retained: &HashSet<StreamKey>) -> Vec<(StreamKey, HlsEntry)> {
        let mut sets = self.sets.lock().unwrap();
        let mut idle = Vec::new();
        sets.retain(|key, set| {
            let keep_stream = retained.contains(key);
            let (live, gone): (Vec<_>, Vec<_>) = std::mem::take(&mut set.entries)
                .into_iter()
                .partition(|variant| keep_stream && variant.pinned() && !variant.failed());
            set.entries = live;
            idle.extend(gone.into_iter().map(|variant| (key.clone(), variant.entry)));
            !set.entries.is_empty()
        });
        idle
    }

    #[cfg(test)]
    pub(super) fn count(&self, key: &StreamKey) -> usize {
        self.sets
            .lock()
            .unwrap()
            .get(key)
            .map_or(0, |set| set.entries.len())
    }
}

impl StreamManager {
    /// Fence, stop and cancel retired variant packagers; the caller joins the controls.
    pub(super) fn retire_hls_variants(
        &self,
        retired: impl IntoIterator<Item = (StreamKey, HlsEntry)>,
    ) -> Vec<WorkerControl> {
        retired
            .into_iter()
            .map(|(key, entry)| {
                self.record_stop(&key, entry.control.completion_receiver());
                entry.media.stop();
                entry.control.cancel();
                entry.control
            })
            .collect()
    }

    /// Resolve a compatibility viewer's preferred audio language for native HLS. `None` serves
    /// the default packager: the preference is omitted from the decision when normalization owns
    /// the audio selection, when no track matches, when the match is the default track, when
    /// tables do not arrive within the HLS startup timeout, or when the variant limit is reached.
    pub(crate) async fn compatibility_hls_audio(
        self: &Arc<Self>,
        network: &str,
        id: &str,
        default: &Arc<HlsPackager>,
        session: &Arc<StreamSession>,
        language: AudioLanguage,
    ) -> Option<HlsVariantPin> {
        // Normalization selected one shared track (and would need one FFmpeg child per variant).
        if self.normalizer.is_some()
            || self.live_normalizer.is_some()
            || session.playback_state().is_some()
        {
            return None;
        }
        let crate::hls::AudioChoice::Track(pid) = default
            .wait_audio_choice(language, self.hls_startup_timeout())
            .await
        else {
            return None;
        };
        let id = &*self.canonical_id(network, id);
        let key = (network.to_string(), id.to_string());
        let _finalizing = self.hls_finalization.lock().await;
        if self
            .shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
            || !self
                .sessions
                .lock()
                .await
                .get(&key)
                .is_some_and(|current| Arc::ptr_eq(current, session))
        {
            return None;
        }
        let (pin, retired) = self.hls_variants.acquire(&key, pid, session, || {
            let media = HlsPackager::create_with_history(
                session,
                self.hls,
                false,
                self.hls_history,
                self.hls_history_budget.clone(),
            );
            media.set_audio_track(pid);
            let control = crate::hls_audio::spawn_worker(
                session,
                media.clone(),
                self.hls,
                None,
                &self.cleanup,
            );
            crate::alog!(
                "[hls] audio variant started: language={} pid={pid:#x}",
                language.as_str()
            );
            HlsEntry { media, control }
        });
        // Dropping the retired controls transfers their joins to the cleanup coordinator.
        drop(self.retire_hls_variants(retired.into_iter().map(|entry| (key.clone(), entry))));
        pin
    }

    /// The exact variant packager a lease pinned (`variant` is [`HlsVariantPin::id`]), unless
    /// it was removed (stop, idle retirement) or has failed.
    pub(crate) fn get_hls_variant(
        &self,
        network: &str,
        id: &str,
        variant: u64,
    ) -> Option<Arc<HlsPackager>> {
        let id = &*self.canonical_id(network, id);
        self.hls_variants
            .get(&(network.to_string(), id.to_string()), variant)
    }

    #[cfg(test)]
    pub(crate) fn hls_variant_count(&self, network: &str, id: &str) -> usize {
        self.hls_variants
            .count(&(network.to_string(), id.to_string()))
    }

    /// The current, usable variant packager for `pid` on the stream, for tests.
    #[cfg(test)]
    pub(crate) fn hls_variant_for_pid(
        &self,
        network: &str,
        id: &str,
        pid: u16,
    ) -> Option<Arc<HlsPackager>> {
        let sets = self.hls_variants.sets.lock().unwrap();
        sets.get(&(network.to_string(), id.to_string()))?
            .entries
            .iter()
            .find(|v| v.pid == pid && !v.failed())
            .map(|v| v.entry.media.clone())
    }

    /// Run one idle sweep now, as the reaper would after its grace period.
    #[cfg(test)]
    pub(crate) async fn sweep_idle_for_test(&self) {
        self.reap_idle_at(Instant::now()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{SourceStats, StreamProvider};
    use crate::test_media::{Generator, AUDIO};
    use async_trait::async_trait;
    use bytes::Bytes;
    use std::sync::atomic::{AtomicUsize, Ordering};

    type Tracks = &'static [(u16, u8, &'static [u8; 3])];

    /// Frames from a fresh generator per open; optionally a new layout after `switch_after`
    /// frames, and EOF after `end_after` frames.
    struct Provider {
        opens: Arc<AtomicUsize>,
        end_after: Option<u64>,
        switch_after: Option<(u64, Tracks)>,
    }

    struct Source {
        generator: Generator,
        frames: u64,
        end_after: Option<u64>,
        switch_after: Option<(u64, Tracks)>,
    }

    #[async_trait]
    impl TsSource for Source {
        async fn next(&mut self) -> Option<Bytes> {
            tokio::time::sleep(Duration::from_millis(1)).await;
            if self.end_after.is_some_and(|end| self.frames >= end) {
                return None;
            }
            if let Some((at, tracks)) = self.switch_after {
                if self.frames == at {
                    self.generator.set_tracks(tracks);
                }
            }
            self.frames += 1;
            let mut out = Vec::new();
            self.generator.frame(&mut out);
            Some(out.into())
        }
        fn stats(&self) -> SourceStats {
            SourceStats::default()
        }
    }

    #[async_trait]
    impl StreamProvider for Provider {
        fn network(&self) -> &'static str {
            "multi"
        }
        async fn open(&self, _id: &str) -> Result<Box<dyn TsSource>, ProviderError> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Source {
                generator: Generator::new(&AUDIO),
                frames: 0,
                end_after: self.end_after,
                switch_after: self.switch_after,
            }))
        }
    }

    fn manager(provider: Provider) -> Arc<StreamManager> {
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(provider));
        StreamManager::with_config(
            registry,
            1024,
            HlsConfig {
                window_segments: 4,
                segment_duration_ms: 1000,
                startup_segments: 1,
                startup_timeout_ms: 5_000,
                ..HlsConfig::default()
            },
        )
    }

    #[test]
    fn admission_bounds_both_the_session_and_the_stream() {
        assert!(admits(0, 0));
        assert!(admits(MAX_AUDIO_VARIANTS - 1, MAX_AUDIO_VARIANTS - 1));
        assert!(!admits(MAX_AUDIO_VARIANTS, MAX_AUDIO_VARIANTS));
        // A fresh session may add variants beside pinned ones from a replaced session ...
        assert!(admits(0, MAX_AUDIO_VARIANTS));
        assert!(admits(3, MAX_STREAM_AUDIO_VARIANTS - 1));
        // ... but never past the stream total, however many sessions were replaced.
        assert!(!admits(0, MAX_STREAM_AUDIO_VARIANTS));
        assert!(!admits(2, MAX_STREAM_AUDIO_VARIANTS + 3));
    }

    fn rus() -> AudioLanguage {
        AudioLanguage::parse("rus").unwrap()
    }

    #[tokio::test]
    async fn replacement_session_after_completed_eof_never_receives_the_old_variant() {
        let opens = Arc::new(AtomicUsize::new(0));
        let manager = manager(Provider {
            opens: opens.clone(),
            end_after: Some(100),
            switch_after: None,
        });
        let support = manager.acquire_support("multi", "a").await.unwrap();
        let (pkg, first, _lease) = manager
            .get_or_start_compatibility_hls("multi", "a")
            .await
            .unwrap();
        let old = manager
            .compatibility_hls_audio("multi", "a", &pkg, &first, rus())
            .await
            .expect("a variant on the first session");
        let old_media = manager.get_hls_variant("multi", "a", old.id()).unwrap();
        // Natural EOF completes the session; releasing support then retires it and its
        // default packager while a lease still pins the variant.
        let mut completed = first.completion_receiver();
        assert!(wait_cleanup(&mut completed).await);
        assert!(manager
            .release_support("multi", "a", support)
            .await
            .unwrap());
        assert!(manager.get("multi", "a").await.is_none());

        let (pkg, second, _lease) = manager
            .get_or_start_compatibility_hls("multi", "a")
            .await
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &second));
        assert_eq!(opens.load(Ordering::SeqCst), 2);
        let fresh = manager
            .compatibility_hls_audio("multi", "a", &pkg, &second, rus())
            .await
            .expect("a variant on the replacement session");
        assert_ne!(fresh.id(), old.id());
        let fresh_media = manager.get_hls_variant("multi", "a", fresh.id()).unwrap();
        assert!(!Arc::ptr_eq(&fresh_media, &old_media));
        // The old lease still reads exactly its own (finished) variant.
        let still = manager.get_hls_variant("multi", "a", old.id()).unwrap();
        assert!(Arc::ptr_eq(&still, &old_media));
        // Once its lease ends, the old variant is retired; the new one stays.
        drop(old);
        manager.sweep_idle_for_test().await;
        assert!(manager.get_hls_variant("multi", "a", fresh.id()).is_some());
        assert_eq!(manager.hls_variant_count("multi", "a"), 1);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn failed_variant_is_never_served_and_new_requests_re_resolve() {
        // After 60 frames `rus` moves from 0x102 to 0x104 and 0x102 disappears.
        const MOVED: [(u16, u8, &[u8; 3]); 3] = [
            (0x101, 0x0f, b"eng"),
            (0x103, 0x03, b"spa"),
            (0x104, 0x0f, b"rus"),
        ];
        let manager = manager(Provider {
            opens: Arc::default(),
            end_after: None,
            switch_after: Some((60, &MOVED)),
        });
        let (pkg, session, _lease) = manager
            .get_or_start_compatibility_hls("multi", "a")
            .await
            .unwrap();
        let first = manager
            .compatibility_hls_audio("multi", "a", &pkg, &session, rus())
            .await
            .unwrap();
        let media = manager.get_hls_variant("multi", "a", first.id()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !matches!(media.status(), crate::hls::HlsStatus::Failed(_)) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the variant fails once its PID leaves the PMT");
        assert!(
            manager.get_hls_variant("multi", "a", first.id()).is_none(),
            "a failed variant is unavailable, even to its own lease"
        );
        let again = manager
            .compatibility_hls_audio("multi", "a", &pkg, &session, rus())
            .await
            .unwrap();
        assert_ne!(again.id(), first.id());
        assert!(manager.hls_variant_for_pid("multi", "a", 0x104).is_some());
        assert!(manager.hls_variant_for_pid("multi", "a", 0x102).is_none());
        assert_eq!(
            manager.hls_variant_count("multi", "a"),
            1,
            "the failed entry was retired"
        );
        manager.shutdown().await;
    }
}
