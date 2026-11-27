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
//! - A request resolves its language, then revalidates that choice under the admission lock:
//!   if the PMT moved the language while it waited, it resolves again. Existing leases stay on
//!   the PID they pinned.
//! - At most [`MAX_AUDIO_VARIANTS`] run per session, and at most
//!   [`MAX_STREAM_AUDIO_VARIANTS`] per stream counting pinned variants of replaced or removed
//!   sessions. A retired variant's worker keeps counting against both limits until it has
//!   exited; a request that needs that slot waits for the exit (within the HLS startup
//!   timeout) instead of running one worker over the limit. Unpinned variants are retired by
//!   the idle sweep, on stop and on shutdown.

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
    /// Signalled when a request has resolved its PID and is about to take the admission lock.
    #[cfg(test)]
    resolved: tokio::sync::Notify,
}

#[derive(Default)]
struct VariantSet {
    entries: Vec<VariantEntry>,
    /// Retired workers that may still be running. Bounded by the limits: a worker only retires
    /// from `entries` (whose slot it keeps), and each is pruned once it has exited.
    retiring: Vec<RetiringWorker>,
    cap_logged: bool,
    /// Every worker this set started, to measure concurrent workers in tests.
    #[cfg(test)]
    spawned: Vec<watch::Receiver<bool>>,
    #[cfg(test)]
    peak_workers: usize,
}

/// A retired variant worker that still holds its slot on `session` and in the stream total.
struct RetiringWorker {
    session: Weak<StreamSession>,
    done: watch::Receiver<bool>,
}

impl VariantSet {
    /// Forget retired workers that have exited (or whose cleanup ended without confirmation).
    fn prune_retiring(&mut self) {
        self.retiring
            .retain(|worker| !*worker.done.borrow() && worker.done.has_changed().is_ok());
    }

    /// Move `gone` out of the live set, keeping its workers counted until they exit.
    fn retire(&mut self, gone: Vec<VariantEntry>) -> Vec<HlsEntry> {
        gone.into_iter()
            .map(|variant| {
                self.retiring.push(RetiringWorker {
                    done: variant.entry.control.completion_receiver(),
                    session: variant.session,
                });
                variant.entry
            })
            .collect()
    }
}

/// Result of one admission attempt.
enum Admission {
    Pinned(HlsVariantPin),
    /// At a limit with no retiring worker to wait for: serve the default packager.
    Full,
    /// A limit is held only by a retiring worker: wait for this completion, then retry.
    Retiring(watch::Receiver<bool>),
    /// The source has ended: a new variant would only see EOF, so serve the default packager.
    Ended,
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

    /// Pin the `pid` variant of `session`, creating it under the limits. Failed variants, and
    /// unpinned variants of any session, are retired first and returned for the caller to fence
    /// and cancel; their workers keep counting against the limits until they exit.
    fn acquire(
        &self,
        key: &StreamKey,
        pid: u16,
        session: &Arc<StreamSession>,
        source_ended: bool,
        create: impl FnOnce() -> HlsEntry,
    ) -> (Admission, Vec<HlsEntry>) {
        let mut sets = self.sets.lock().unwrap();
        let set = sets.entry(key.clone()).or_default();
        set.prune_retiring();
        let (keep, gone): (Vec<_>, Vec<_>) = std::mem::take(&mut set.entries)
            .into_iter()
            .partition(|v| !v.failed() && (v.pinned() || (v.on(session) && v.pid == pid)));
        set.entries = keep;
        let retired = set.retire(gone);
        // Reuse works at any limit: it starts no worker.
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
            return (Admission::Pinned(HlsVariantPin { id, _pin: pin }), retired);
        }
        if source_ended {
            return (Admission::Ended, retired);
        }
        let on_session = set.entries.iter().filter(|v| v.on(session)).count();
        let total = set.entries.len();
        let retiring_on =
            |w: &&RetiringWorker| std::ptr::eq(w.session.as_ptr(), Arc::as_ptr(session));
        let retiring_on_session = set.retiring.iter().filter(retiring_on).count();
        if !admits(on_session + retiring_on_session, total + set.retiring.len()) {
            if admits(on_session, total) {
                // Only retiring workers hold the limit: wait for one that frees the binding slot.
                let session_bound = on_session + retiring_on_session >= MAX_AUDIO_VARIANTS;
                let worker = set
                    .retiring
                    .iter()
                    .find(|w| !session_bound || retiring_on(w))
                    .expect("a retiring worker holds the limit");
                return (Admission::Retiring(worker.done.clone()), retired);
            }
            if !std::mem::replace(&mut set.cap_logged, true) {
                crate::alog!(
                    "[hls] audio variant limit reached ({MAX_AUDIO_VARIANTS} per session, \
                     {MAX_STREAM_AUDIO_VARIANTS} per stream); further languages get the \
                     default audio track"
                );
            }
            return (Admission::Full, retired);
        }
        let pin = Arc::new(());
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let entry = create();
        #[cfg(test)]
        {
            set.spawned.push(entry.control.completion_receiver());
            let running = set.spawned.iter().filter(|done| !*done.borrow()).count();
            set.peak_workers = set.peak_workers.max(running);
        }
        set.entries.push(VariantEntry {
            id,
            pid,
            session: Arc::downgrade(session),
            entry,
            pins: Arc::downgrade(&pin),
        });
        (Admission::Pinned(HlsVariantPin { id, _pin: pin }), retired)
    }

    /// Remove every variant of a stream. Their workers keep counting against the stream's
    /// limits until they exit, so a restart right after a stop cannot exceed them.
    pub(super) fn take_stream(&self, key: &StreamKey) -> Vec<HlsEntry> {
        let mut sets = self.sets.lock().unwrap();
        let Some(set) = sets.get_mut(key) else {
            return Vec::new();
        };
        set.prune_retiring();
        let gone = std::mem::take(&mut set.entries);
        let retired = set.retire(gone);
        set.cap_logged = false;
        if set.retiring.is_empty() {
            sets.remove(key);
        }
        retired
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

    /// Remove every variant that has failed or that no lease pins any more. A pinned variant
    /// survives even when its session was removed (by EOF or by the idle sweep): it keeps
    /// serving its leases until they end, and only counts toward the per-stream total.
    pub(super) fn take_idle(&self) -> Vec<(StreamKey, HlsEntry)> {
        let mut sets = self.sets.lock().unwrap();
        let mut idle = Vec::new();
        sets.retain(|key, set| {
            set.prune_retiring();
            let (live, gone): (Vec<_>, Vec<_>) = std::mem::take(&mut set.entries)
                .into_iter()
                .partition(|variant| variant.pinned() && !variant.failed());
            set.entries = live;
            let retired = set.retire(gone);
            idle.extend(retired.into_iter().map(|entry| (key.clone(), entry)));
            !set.entries.is_empty() || !set.retiring.is_empty()
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
        let id = &*self.canonical_id(network, id);
        let key = (network.to_string(), id.to_string());
        let deadline = tokio::time::Instant::now() + self.hls_startup_timeout();
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let crate::hls::AudioChoice::Track(pid) =
                default.wait_audio_choice(language, remaining).await
            else {
                return None;
            };
            #[cfg(test)]
            self.hls_variants.resolved.notify_waiters();
            let mut retiring = {
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
                // The PMT may have moved the language while this request waited for the lock.
                match default.audio_choice(language) {
                    crate::hls::AudioChoice::Track(current) if current == pid => {}
                    crate::hls::AudioChoice::Default => return None,
                    // Bounded: a PMT that keeps moving cannot hold the request past its deadline.
                    _ if tokio::time::Instant::now() >= deadline => return None,
                    _ => continue,
                }
                let (admission, retired) =
                    self.hls_variants
                        .acquire(&key, pid, session, session.source_ended(), || {
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
                // Request cancellation now; dropping the controls transfers their joins.
                drop(
                    self.retire_hls_variants(retired.into_iter().map(|entry| (key.clone(), entry))),
                );
                match admission {
                    Admission::Pinned(pin) => return Some(pin),
                    Admission::Full | Admission::Ended => return None,
                    Admission::Retiring(done) => done,
                }
            };
            // A retiring worker still holds the slot: wait for it to exit, then retry.
            if tokio::time::timeout_at(deadline, wait_cleanup(&mut retiring))
                .await
                .is_err()
            {
                return None;
            }
        }
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
        tracks: Tracks,
        /// Switch to the layout once the flag is set (test-controlled timing).
        switch_on: Option<(Arc<std::sync::atomic::AtomicBool>, Tracks)>,
    }

    impl Default for Provider {
        fn default() -> Self {
            Self {
                opens: Arc::default(),
                end_after: None,
                switch_after: None,
                tracks: &AUDIO,
                switch_on: None,
            }
        }
    }

    struct Source {
        generator: Generator,
        frames: u64,
        end_after: Option<u64>,
        switch_after: Option<(u64, Tracks)>,
        switch_on: Option<(Arc<std::sync::atomic::AtomicBool>, Tracks)>,
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
            if let Some((flag, tracks)) = &self.switch_on {
                if flag.swap(false, Ordering::SeqCst) {
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
                generator: Generator::new(self.tracks),
                frames: 0,
                end_after: self.end_after,
                switch_after: self.switch_after,
                switch_on: self.switch_on.clone(),
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
            ..Provider::default()
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
    async fn a_request_after_the_source_ended_gets_the_default_not_an_empty_variant() {
        const TRACKS: [(u16, u8, &[u8; 3]); 3] = [
            (0x101, 0x0f, b"eng"),
            (0x102, 0x0f, b"rus"),
            (0x103, 0x0f, b"fra"),
        ];
        let manager = manager(Provider {
            end_after: Some(100),
            tracks: &TRACKS,
            ..Provider::default()
        });
        let support = manager.acquire_support("multi", "a").await.unwrap();
        let (pkg, session, _lease) = manager
            .get_or_start_compatibility_hls("multi", "a")
            .await
            .unwrap();
        let existing = manager
            .compatibility_hls_audio("multi", "a", &pkg, &session, rus())
            .await
            .expect("a variant while the source is live");
        let mut completed = session.completion_receiver();
        assert!(wait_cleanup(&mut completed).await);
        assert!(session.source_ended());
        // A default packager that has not processed the EOF yet still resolves `fra`; a new
        // variant would subscribe at the tail and only see EOF, so the default is served.
        let lagging = HlsPackager::create_with_history(
            &session,
            manager.hls,
            false,
            manager.hls_history,
            manager.hls_history_budget.clone(),
        );
        let mut out = Vec::new();
        Generator::new(&TRACKS).frame(&mut out);
        lagging.feed_for_test(&out);
        let fra = AudioLanguage::parse("fra").unwrap();
        assert_eq!(
            lagging.audio_choice(fra),
            crate::hls::AudioChoice::Track(0x103)
        );
        assert!(manager
            .compatibility_hls_audio("multi", "a", &lagging, &session, fra)
            .await
            .is_none());
        assert_eq!(manager.hls_variant_count("multi", "a"), 1);
        // An existing variant is still shared, pinned to its exact entry.
        let again = manager
            .compatibility_hls_audio("multi", "a", &lagging, &session, rus())
            .await
            .expect("the existing variant");
        assert_eq!(again.id(), existing.id());
        drop(support);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn retired_variant_memory_is_freed_after_cleanup_and_its_last_reader() {
        let manager = manager(Provider {
            opens: Arc::default(),
            end_after: None,
            switch_after: None,
            ..Provider::default()
        });
        let (pkg, session, _lease) = manager
            .get_or_start_compatibility_hls("multi", "a")
            .await
            .unwrap();
        let pin = manager
            .compatibility_hls_audio("multi", "a", &pkg, &session, rus())
            .await
            .expect("a variant");
        // An in-flight segment request holds the packager across retirement.
        let reader = manager.get_hls_variant("multi", "a", pin.id()).unwrap();
        let weak = Arc::downgrade(&reader);
        drop(pin);
        manager.sweep_idle_for_test().await;
        assert_eq!(
            manager.hls_variant_count("multi", "a"),
            0,
            "retired at once"
        );
        assert!(weak.upgrade().is_some(), "the reader keeps it alive");
        drop(reader);
        // Once the cancelled worker has finished cleanup, nothing else keeps it.
        let freed = tokio::time::timeout(Duration::from_secs(5), async {
            while weak.strong_count() > 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(freed.is_ok(), "retired packager still referenced");
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
            ..Provider::default()
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
        // The default packager must have seen the same PMT before the retry resolves.
        wait_for_choice(&pkg, crate::hls::AudioChoice::Track(0x104)).await;
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

    async fn wait_for_choice(pkg: &HlsPackager, expected: crate::hls::AudioChoice) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while pkg.audio_choice(rus()) != expected {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("default packager never resolved `rus` to {expected:?}"));
    }

    fn variant_pid(manager: &StreamManager, key: &str, id: u64) -> Option<u16> {
        let sets = manager.hls_variants.sets.lock().unwrap();
        sets.get(&("multi".to_string(), key.to_string()))?
            .entries
            .iter()
            .find(|v| v.id == id)
            .map(|v| v.pid)
    }

    #[tokio::test]
    async fn pinned_variant_survives_a_sweep_that_removes_its_session_without_eof() {
        let manager = manager(Provider::default());
        let (pkg, first, lease) = manager
            .get_or_start_compatibility_hls("multi", "a")
            .await
            .unwrap();
        let pin = manager
            .compatibility_hls_audio("multi", "a", &pkg, &first, rus())
            .await
            .unwrap();
        let media = manager.get_hls_variant("multi", "a", pin.id()).unwrap();
        // No subscriber, support pin or native access remains: the sweep removes the live
        // (not ended) session, but the still-pinned variant keeps serving its lease.
        drop(lease);
        manager.sweep_idle_for_test().await;
        assert!(
            manager.get("multi", "a").await.is_none(),
            "session swept, not ended"
        );
        let kept = manager.get_hls_variant("multi", "a", pin.id()).unwrap();
        assert!(Arc::ptr_eq(&kept, &media));
        // A replacement session gets its own variant; the old one does not count toward it.
        let (pkg, second, _lease) = manager
            .get_or_start_compatibility_hls("multi", "a")
            .await
            .unwrap();
        let fresh = manager
            .compatibility_hls_audio("multi", "a", &pkg, &second, rus())
            .await
            .unwrap();
        assert_ne!(fresh.id(), pin.id());
        assert_eq!(manager.hls_variant_count("multi", "a"), 2);
        // Once its lease ends, the next sweep retires it.
        drop(pin);
        manager.sweep_idle_for_test().await;
        assert!(manager.get_hls_variant("multi", "a", fresh.id()).is_some());
        assert_eq!(manager.hls_variant_count("multi", "a"), 1);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn admission_re_resolves_when_the_pmt_moves_during_the_lock_wait() {
        // `rus` moves from 0x102 to 0x104, and 0x102 stays as Spanish.
        const MOVED: [(u16, u8, &[u8; 3]); 3] = [
            (0x101, 0x0f, b"eng"),
            (0x102, 0x0f, b"spa"),
            (0x104, 0x0f, b"rus"),
        ];
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let manager = manager(Provider {
            switch_on: Some((flag.clone(), &MOVED)),
            ..Provider::default()
        });
        let (pkg, session, _lease) = manager
            .get_or_start_compatibility_hls("multi", "a")
            .await
            .unwrap();
        // An existing lease on 0x102, admitted before the move.
        let existing = manager
            .compatibility_hls_audio("multi", "a", &pkg, &session, rus())
            .await
            .unwrap();
        assert_eq!(variant_pid(&manager, "a", existing.id()), Some(0x102));
        // A second request resolves 0x102, then blocks on the admission lock.
        let guard = manager.hls_finalization.lock().await;
        // Registered before the spawn, so only this request's resolution can complete it.
        let resolved = manager.hls_variants.resolved.notified();
        tokio::pin!(resolved);
        resolved.as_mut().enable();
        let request = {
            let (manager, pkg, session) = (manager.clone(), pkg.clone(), session.clone());
            tokio::spawn(async move {
                manager
                    .compatibility_hls_audio("multi", "a", &pkg, &session, rus())
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(5), resolved)
            .await
            .expect("the request resolved before the lock");
        flag.store(true, Ordering::SeqCst);
        wait_for_choice(&pkg, crate::hls::AudioChoice::Track(0x104)).await;
        drop(guard);
        let pin = request
            .await
            .unwrap()
            .expect("a variant for the moved track");
        assert_eq!(variant_pid(&manager, "a", pin.id()), Some(0x104));
        // The existing lease stays on its original PID, which is still in the PMT.
        assert_eq!(variant_pid(&manager, "a", existing.id()), Some(0x102));
        assert!(manager
            .get_hls_variant("multi", "a", existing.id())
            .is_some());
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn stopped_variant_workers_keep_counting_until_they_exit() {
        let manager = manager(Provider::default());
        let (pkg, session, _lease) = manager
            .get_or_start_compatibility_hls("multi", "a")
            .await
            .unwrap();
        let _pin = manager
            .compatibility_hls_audio("multi", "a", &pkg, &session, rus())
            .await
            .unwrap();
        let key = ("multi".to_string(), "a".to_string());
        let stopped = manager.hls_variants.take_stream(&key);
        assert_eq!(stopped.len(), 1);
        assert_eq!(
            manager.hls_variants.sets.lock().unwrap()[&key]
                .retiring
                .len(),
            1,
            "a stopped worker still holds its slot in the stream total"
        );
        let mut done: Vec<_> = stopped
            .iter()
            .map(|entry| entry.control.completion_receiver())
            .collect();
        drop(manager.retire_hls_variants(stopped.into_iter().map(|e| (key.clone(), e))));
        for done in &mut done {
            assert!(wait_cleanup(done).await);
        }
        // Once it has exited, the next sweep forgets the empty set.
        drop(manager.hls_variants.take_idle());
        assert!(!manager.hls_variants.sets.lock().unwrap().contains_key(&key));
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn a_retiring_worker_holds_its_slot_until_it_exits() {
        const SIX: [(u16, u8, &[u8; 3]); 6] = [
            (0x101, 0x0f, b"eng"),
            (0x102, 0x0f, b"rus"),
            (0x103, 0x0f, b"spa"),
            (0x104, 0x0f, b"fra"),
            (0x105, 0x0f, b"deu"),
            (0x106, 0x0f, b"ita"),
        ];
        let manager = manager(Provider {
            tracks: &SIX,
            ..Provider::default()
        });
        let (pkg, session, _lease) = manager
            .get_or_start_compatibility_hls("multi", "a")
            .await
            .unwrap();
        let language = |code| AudioLanguage::parse(code).unwrap();
        let mut pins = Vec::new();
        for code in ["rus", "spa", "fra", "deu"] {
            pins.push(
                manager
                    .compatibility_hls_audio("multi", "a", &pkg, &session, language(code))
                    .await
                    .unwrap(),
            );
        }
        // Reusing a pinned variant at the limit still works.
        let again = manager
            .compatibility_hls_audio("multi", "a", &pkg, &session, language("rus"))
            .await
            .unwrap();
        assert_eq!(again.id(), pins[0].id());
        // Unpin `spa` and ask for `ita`: its worker must not start before `spa`'s has exited.
        drop(pins.remove(1));
        let ita = manager
            .compatibility_hls_audio("multi", "a", &pkg, &session, language("ita"))
            .await
            .expect("admitted once the retired worker exited");
        assert_eq!(variant_pid(&manager, "a", ita.id()), Some(0x106));
        let key = ("multi".to_string(), "a".to_string());
        let peak = manager.hls_variants.sets.lock().unwrap()[&key].peak_workers;
        assert_eq!(
            peak, MAX_AUDIO_VARIANTS,
            "never more workers than the limit"
        );
        manager.shutdown().await;
    }
}
