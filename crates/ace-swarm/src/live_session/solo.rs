//! Attribution of live pieces that fail their in-band source signature (#561).
//!
//! A piece's chunks can arrive from more than one peer (a timed-out request retried
//! elsewhere, or a partial retained across a reconnect), so a failed signature does not by
//! itself name the peer that sent the bad bytes. After a failure the piece goes *solo*: the
//! next peer assigned to it must deliver every chunk alone, and chunks of that piece from
//! any other peer are ignored. A failure of that solo fetch is attributable to its owner,
//! which is disconnected and avoided. A first failure penalises nobody, even when one peer
//! sent every block, so a lone upstream that was transiently wrong keeps serving.
//!
//! Only pieces that already failed verification have an entry, so the tracker is empty on
//! the normal path, where every hook is a single `is_empty` check. It is capped and pruned
//! below the playback cursor.

use super::output_clock::EffectiveInstant;
use std::net::SocketAddrV4;

/// Upper bound on concurrently solo pieces. A swarm producing more distinct bad pieces
/// than this inside the reassembly window falls back to ordinary retries for the excess.
pub(super) const MAX_SOLO_PIECES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SoloOwner {
    peer_id: u64,
    addr: SocketAddrV4,
    /// Assignment time, then the time of the owner's latest contributing chunk.
    last_chunk: EffectiveInstant,
}

impl SoloOwner {
    fn is(&self, peer_id: u64, addr: SocketAddrV4) -> bool {
        self.peer_id == peer_id && self.addr == addr
    }
}

/// At most [`MAX_SOLO_PIECES`] entries, so a linear scan beats a tree here.
#[derive(Default)]
pub(crate) struct SoloPieces {
    pieces: Vec<(u64, Option<SoloOwner>)>,
}

impl SoloPieces {
    #[inline]
    pub(super) fn is_empty(&self) -> bool {
        self.pieces.is_empty()
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.pieces.len()
    }

    #[cfg(test)]
    pub(super) fn contains(&self, piece: u64) -> bool {
        self.entry(piece).is_some()
    }

    fn entry(&self, piece: u64) -> Option<&Option<SoloOwner>> {
        self.pieces
            .iter()
            .find(|(p, _)| *p == piece)
            .map(|(_, owner)| owner)
    }

    fn entry_mut(&mut self, piece: u64) -> Option<&mut Option<SoloOwner>> {
        self.pieces
            .iter_mut()
            .find(|(p, _)| *p == piece)
            .map(|(_, owner)| owner)
    }

    /// Put `piece` (back) into solo mode with no owner yet. Returns false, leaving the
    /// piece on the ordinary path, when the tracker is full.
    pub(super) fn mark(&mut self, piece: u64) -> bool {
        if let Some(owner) = self.entry_mut(piece) {
            *owner = None;
            return true;
        }
        if self.pieces.len() >= MAX_SOLO_PIECES {
            return false;
        }
        self.pieces.push((piece, None));
        true
    }

    /// The scheduler gave `piece` to a peer. Returns true when a different owner held it,
    /// so the caller must discard that owner's partial bytes before the new fetch.
    pub(super) fn assign(
        &mut self,
        piece: u64,
        peer_id: u64,
        addr: SocketAddrV4,
        now: EffectiveInstant,
    ) -> bool {
        let Some(owner) = self.entry_mut(piece) else {
            return false;
        };
        let replaced = owner.is_some_and(|old| !old.is(peer_id, addr));
        if replaced || owner.is_none() {
            *owner = Some(SoloOwner {
                peer_id,
                addr,
                last_chunk: now,
            });
        }
        replaced
    }

    /// Whether a chunk of `piece` from this peer may enter reassembly: any peer for an
    /// ordinary piece, only the owner for a solo one.
    pub(super) fn accepts(&self, piece: u64, peer_id: u64, addr: SocketAddrV4) -> bool {
        match self.entry(piece) {
            None => true,
            Some(owner) => owner.is_some_and(|owner| owner.is(peer_id, addr)),
        }
    }

    /// The owner's chunk added bytes to the piece: restart its silence timer. Duplicates
    /// do not count, so an owner can hold a piece for at most one timeout per chunk.
    pub(super) fn touch(&mut self, piece: u64, now: EffectiveInstant) {
        if let Some(Some(owner)) = self.entry_mut(piece) {
            owner.last_chunk = now;
        }
    }

    /// A solo piece whose owner holds it is governed by the silence timer, not by the
    /// ordinary request timeout, which would retry it on a peer whose chunks are ignored.
    pub(super) fn owned(&self, piece: u64) -> bool {
        self.entry(piece).is_some_and(Option::is_some)
    }

    /// Whether this peer owns solo `piece`, so every byte of it came from that peer.
    pub(super) fn owned_by(&self, piece: u64, peer_id: u64, addr: SocketAddrV4) -> bool {
        self.entry(piece)
            .is_some_and(|owner| owner.is_some_and(|owner| owner.is(peer_id, addr)))
    }

    /// The piece verified: it leaves solo mode.
    pub(super) fn complete(&mut self, piece: u64) {
        self.pieces.retain(|(p, _)| *p != piece);
    }

    /// Clear owners matching `release`, returning the pieces whose partial bytes must
    /// be discarded before another peer fetches them.
    fn release(&mut self, mut release: impl FnMut(&SoloOwner) -> bool) -> Vec<u64> {
        let mut released = Vec::new();
        for (piece, owner) in &mut self.pieces {
            if owner.as_ref().is_some_and(&mut release) {
                *owner = None;
                released.push(*piece);
            }
        }
        released
    }

    /// The owning peer left the pool.
    pub(super) fn release_peer(&mut self, peer_id: u64) -> Vec<u64> {
        self.release(|owner| owner.peer_id == peer_id)
    }

    /// A new pool reuses peer ids, so no previous owner survives a reconnect.
    pub(super) fn release_all(&mut self) -> Vec<u64> {
        self.release(|_| true)
    }

    /// Owners silent for at least `timeout` since assignment or their last chunk.
    pub(super) fn release_silent(
        &mut self,
        now: EffectiveInstant,
        timeout: std::time::Duration,
    ) -> Vec<u64> {
        self.release(|owner| now.duration_since(owner.last_chunk) >= timeout)
    }

    pub(super) fn prune_below(&mut self, floor: u64) {
        if !self.pieces.is_empty() {
            self.pieces.retain(|&(piece, _)| piece >= floor);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::time::{Duration, Instant};

    fn addr(port: u16) -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)
    }

    #[test]
    fn solo_piece_accepts_only_its_owner_and_tracks_silence() {
        let t0 = EffectiveInstant(Instant::now());
        let mut solo = SoloPieces::default();
        assert!(solo.accepts(7, 1, addr(1)), "ordinary pieces accept anyone");
        assert!(solo.mark(7));
        assert!(
            !solo.accepts(7, 1, addr(1)),
            "unowned solo piece accepts nobody"
        );
        assert!(!solo.assign(7, 2, addr(2), t0));
        assert!(solo.owned(7) && solo.owned_by(7, 2, addr(2)));
        assert!(!solo.owned_by(7, 1, addr(1)));
        assert!(!solo.accepts(7, 1, addr(1)));
        assert!(
            !solo.accepts(7, 2, addr(3)),
            "a reused id must match its endpoint"
        );
        assert!(solo.accepts(7, 2, addr(2)));
        let t1 = t0 + Duration::from_secs(2);
        solo.touch(7, t1);
        assert!(solo
            .release_silent(t0 + Duration::from_secs(4), Duration::from_secs(3))
            .is_empty());
        assert_eq!(
            solo.release_silent(t1 + Duration::from_secs(3), Duration::from_secs(3)),
            vec![7]
        );
        assert!(!solo.owned(7) && solo.contains(7));
        assert!(!solo.assign(7, 1, addr(1), t1));
        assert!(
            solo.assign(7, 2, addr(2), t1),
            "a new owner discards the old partial"
        );
        assert_eq!(solo.release_peer(2), vec![7]);
        solo.complete(7);
        assert!(solo.is_empty());
    }

    #[test]
    fn solo_bookkeeping_is_capped_and_pruned_below_the_cursor() {
        let mut solo = SoloPieces::default();
        for piece in 0..MAX_SOLO_PIECES as u64 {
            assert!(solo.mark(piece));
        }
        assert!(
            !solo.mark(1_000),
            "a full tracker leaves new pieces ordinary"
        );
        assert!(solo.mark(3), "an existing entry can always be re-armed");
        assert_eq!(solo.len(), MAX_SOLO_PIECES);
        solo.prune_below(60);
        assert_eq!(solo.len(), MAX_SOLO_PIECES - 60);
        let t0 = EffectiveInstant(Instant::now());
        solo.assign(60, 1, addr(1), t0);
        solo.assign(61, 2, addr(2), t0);
        assert_eq!(solo.release_all(), vec![60, 61]);
    }

    #[test]
    fn solo_silence_timeout_stays_inside_the_stale_upstream_timeout() {
        let options = |request_timeout_ms, stale_upstream_timeout_ms| {
            super::super::LiveRecoveryOptions {
                request_timeout_ms,
                stale_upstream_timeout_ms,
                ..Default::default()
            }
            .solo_silence_timeout()
        };
        let check = super::super::LiveRecoveryOptions::default().request_check_interval();
        assert_eq!(options(1_500, 12_000), Duration::from_secs(3));
        assert_eq!(
            options(7_000, 12_000),
            Duration::from_secs(12) - check,
            "the handover must run before the pool is declared stale"
        );
        assert_eq!(
            options(11_990, 12_000),
            Duration::from_millis(11_990),
            "never shorter than the request timeout"
        );
    }
}
