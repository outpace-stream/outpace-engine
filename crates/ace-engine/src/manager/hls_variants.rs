//! Bounded native HLS audio-language variants for compatibility `preferred_audio_language`.
//!
//! A variant is an extra [`HlsPackager`] on the stream's one shared [`StreamSession`]: it reads
//! the same broadcast (no second upstream download) and differs only in which audio track its
//! native selection keeps. Variants are keyed by the selected elementary PID, not by the
//! requested string, so every language that resolves to the default track shares the default
//! packager and synonyms share one variant. At most [`MAX_AUDIO_VARIANTS`] run per stream; a
//! variant lives while a compatibility lease pins it and is retired by the idle sweep (or on
//! stop/shutdown) once its last lease is gone.

use super::*;
use ace_media::audio::AudioLanguage;

/// Distinct non-default audio variants per stream. Each costs one packager: its retained segment
/// window (bounded like the default packager's) plus one broadcast receiver.
pub(crate) const MAX_AUDIO_VARIANTS: usize = 4;

#[derive(Default)]
pub(super) struct HlsVariants {
    sets: StdMutex<HashMap<StreamKey, VariantSet>>,
}

#[derive(Default)]
struct VariantSet {
    entries: Vec<VariantEntry>,
    cap_logged: bool,
}

struct VariantEntry {
    pid: u16,
    entry: HlsEntry,
    pins: Weak<()>,
}

/// Held by a compatibility HLS lease: keeps its variant packager from being retired.
pub struct HlsVariantPin {
    pid: u16,
    _pin: Arc<()>,
}

impl HlsVariantPin {
    pub(crate) fn pid(&self) -> u16 {
        self.pid
    }
}

impl HlsVariants {
    fn get(&self, key: &StreamKey, pid: u16) -> Option<Arc<HlsPackager>> {
        let sets = self.sets.lock().unwrap();
        let set = sets.get(key)?;
        set.entries
            .iter()
            .find(|variant| variant.pid == pid)
            .map(|variant| variant.entry.media.clone())
    }

    /// Pin the `pid` variant, creating it under the cap. Unpinned variants are retired first so
    /// the cap is a hard bound; they are returned for the caller to fence and cancel.
    fn acquire(
        &self,
        key: &StreamKey,
        pid: u16,
        create: impl FnOnce() -> HlsEntry,
    ) -> (Option<HlsVariantPin>, Vec<HlsEntry>) {
        let mut sets = self.sets.lock().unwrap();
        let set = sets.entry(key.clone()).or_default();
        if let Some(variant) = set.entries.iter_mut().find(|v| v.pid == pid) {
            let pin = variant.pins.upgrade().unwrap_or_else(|| {
                let pin = Arc::new(());
                variant.pins = Arc::downgrade(&pin);
                pin
            });
            return (Some(HlsVariantPin { pid, _pin: pin }), Vec::new());
        }
        let (live, idle): (Vec<_>, Vec<_>) = std::mem::take(&mut set.entries)
            .into_iter()
            .partition(|variant| variant.pins.strong_count() > 0);
        set.entries = live;
        let retired = idle.into_iter().map(|variant| variant.entry).collect();
        if set.entries.len() >= MAX_AUDIO_VARIANTS {
            if !std::mem::replace(&mut set.cap_logged, true) {
                crate::alog!(
                    "[hls] audio variant limit reached ({MAX_AUDIO_VARIANTS} per stream); \
                     further languages get the default audio track"
                );
            }
            return (None, retired);
        }
        let pin = Arc::new(());
        set.entries.push(VariantEntry {
            pid,
            entry: create(),
            pins: Arc::downgrade(&pin),
        });
        (Some(HlsVariantPin { pid, _pin: pin }), retired)
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

    /// Remove every variant of a retired session and every variant no lease pins any more.
    pub(super) fn take_idle(&self, retained: &HashSet<StreamKey>) -> Vec<(StreamKey, HlsEntry)> {
        let mut sets = self.sets.lock().unwrap();
        let mut idle = Vec::new();
        sets.retain(|key, set| {
            let keep_stream = retained.contains(key);
            let (live, gone): (Vec<_>, Vec<_>) = std::mem::take(&mut set.entries)
                .into_iter()
                .partition(|variant| keep_stream && variant.pins.strong_count() > 0);
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
        let (pin, retired) = self.hls_variants.acquire(&key, pid, || {
            let media = HlsPackager::create_with_history(
                session,
                self.hls,
                false,
                self.hls_history,
                self.hls_history_budget.clone(),
            );
            media.set_audio_language(language);
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

    /// The running variant packager a lease pinned, if the stream still has it.
    pub(crate) fn get_hls_variant(
        &self,
        network: &str,
        id: &str,
        pid: u16,
    ) -> Option<Arc<HlsPackager>> {
        let id = &*self.canonical_id(network, id);
        self.hls_variants
            .get(&(network.to_string(), id.to_string()), pid)
    }

    #[cfg(test)]
    pub(crate) fn hls_variant_count(&self, network: &str, id: &str) -> usize {
        self.hls_variants
            .count(&(network.to_string(), id.to_string()))
    }

    /// Run one idle sweep now, as the reaper would after its grace period.
    #[cfg(test)]
    pub(crate) async fn sweep_idle_for_test(&self) {
        self.reap_idle_at(Instant::now()).await;
    }
}
