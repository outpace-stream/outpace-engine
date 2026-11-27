//! Synthetic multi-audio MPEG-TS for engine tests and bounded benchmarks (mirrors
//! `ace-media`'s private test stream). No public media is used.
//!
//! One program: PMT PID 0x1000, H.264 video on 0x100 and three audio tracks, each tagged with an
//! ISO 639 language descriptor: ADTS AAC `eng` (0x101), ADTS AAC `rus` (0x102) and MPEG audio
//! `spa` (0x103). A 40 ms frame carries one PCR, 130 video packets and three packets per audio
//! track. PAT/PMT repeat every third frame; every 25th frame is a video access point.
use ace_media::mpegts::mpeg_crc32;

const N: usize = 188;
pub(crate) const PMT_PID: u16 = 0x1000;
pub(crate) const VIDEO_PID: u16 = 0x100;
pub(crate) const AUDIO: [(u16, u8, &[u8; 3]); 3] = [
    (0x101, 0x0f, b"eng"),
    (0x102, 0x0f, b"rus"),
    (0x103, 0x03, b"spa"),
];

pub(crate) fn psi(pid: u16, mut section: Vec<u8>) -> [u8; N] {
    let len = section.len() + 1; // excludes the first three bytes, includes the CRC
    section[1] = 0xb0 | (len >> 8) as u8;
    section[2] = len as u8;
    let crc = mpeg_crc32(&section);
    section.extend_from_slice(&crc.to_be_bytes());
    let mut packet = [0xff; N];
    packet[..5].copy_from_slice(&[0x47, 0x40 | (pid >> 8) as u8, pid as u8, 0x10, 0]);
    packet[5..5 + section.len()].copy_from_slice(&section);
    packet
}

pub(crate) fn pat() -> [u8; N] {
    psi(
        0,
        vec![
            0,
            0,
            0,
            0,
            1,
            0xc1,
            0,
            0,
            0,
            1,
            0xe0 | (PMT_PID >> 8) as u8,
            PMT_PID as u8,
        ],
    )
}

/// PMT with the video entry first, then `tracks` as `(pid, stream_type, language)`.
pub(crate) fn pmt(tracks: &[(u16, u8, &[u8; 3])]) -> [u8; N] {
    let mut s = vec![
        2,
        0,
        0,
        0,
        1,
        0xc1,
        0,
        0,
        0xe0 | (VIDEO_PID >> 8) as u8,
        VIDEO_PID as u8,
        0xf0,
        0,
    ];
    s.extend_from_slice(&[
        0x1b,
        0xe0 | (VIDEO_PID >> 8) as u8,
        VIDEO_PID as u8,
        0xf0,
        0,
    ]);
    for (pid, ty, lang) in tracks {
        s.extend_from_slice(&[*ty, 0xe0 | (pid >> 8) as u8, *pid as u8, 0xf0, 6, 0x0a, 4]);
        s.extend_from_slice(*lang);
        s.push(0);
    }
    psi(PMT_PID, s)
}

/// Generate `frames` 40 ms frames of synthetic multi-audio TS.
pub(crate) fn stream(frames: usize) -> Vec<u8> {
    let mut generator = Generator::new(&AUDIO);
    let mut out = Vec::with_capacity(frames * 142 * N);
    for _ in 0..frames {
        generator.frame(&mut out);
    }
    out
}

/// Endless frame source with its own audio track layout, continuity and PCR.
pub(crate) struct Generator {
    tracks: Vec<(u16, u8, [u8; 3])>,
    table: [u8; N],
    frame: u64,
    cc: Vec<u8>,
}

impl Generator {
    pub(crate) fn new(tracks: &[(u16, u8, &[u8; 3])]) -> Self {
        Self {
            tracks: tracks.iter().map(|(p, t, l)| (*p, *t, **l)).collect(),
            table: pmt(tracks),
            frame: 0,
            cc: vec![0; 0x2000],
        }
    }

    /// Switch the audio layout from the next frame on. Each call advances the PMT version
    /// (modulo 32) from the previous table's and recomputes the CRC.
    pub(crate) fn set_tracks(&mut self, tracks: &[(u16, u8, &[u8; 3])]) {
        const VERSION: usize = 5 + 5; // section byte 5 of the PMT packet
        let version = ((self.table[VERSION] >> 1) & 31).wrapping_add(1) & 31;
        self.tracks = tracks.iter().map(|(p, t, l)| (*p, *t, **l)).collect();
        self.table = pmt(tracks);
        self.table[VERSION] = (self.table[VERSION] & 0xc1) | (version << 1);
        let len = (((self.table[6] & 15) as usize) << 8) | self.table[7] as usize;
        let crc = mpeg_crc32(&self.table[5..5 + 3 + len - 4]);
        self.table[5 + 3 + len - 4..5 + 3 + len].copy_from_slice(&crc.to_be_bytes());
    }

    /// The PMT version and section CRC check of the current table, for tests.
    fn table_version(&self) -> (u8, bool) {
        let len = (((self.table[6] & 15) as usize) << 8) | self.table[7] as usize;
        (
            (self.table[5 + 5] >> 1) & 31,
            mpeg_crc32(&self.table[5..5 + 3 + len]) == 0,
        )
    }

    fn next_cc(&mut self, pid: u16) -> u8 {
        let c = self.cc[pid as usize];
        self.cc[pid as usize] = (c + 1) & 15;
        c
    }

    /// Append one 40 ms frame.
    pub(crate) fn frame(&mut self, out: &mut Vec<u8>) {
        let frame = self.frame;
        self.frame += 1;
        if frame.is_multiple_of(3) {
            let mut p = pat();
            p[3] |= self.next_cc(0);
            out.extend_from_slice(&p);
            let mut p = self.table;
            p[3] |= self.next_cc(PMT_PID);
            out.extend_from_slice(&p);
        }
        let key = frame.is_multiple_of(25);
        let pcr = frame * 3600;
        let mut p = [0xab; N];
        p[..4].copy_from_slice(&[
            0x47,
            0x40 | (VIDEO_PID >> 8) as u8,
            VIDEO_PID as u8,
            0x30 | self.next_cc(VIDEO_PID),
        ]);
        p[4] = 7;
        p[5] = 0x10 | if key { 0x40 } else { 0 };
        p[6..12].copy_from_slice(&[
            (pcr >> 25) as u8,
            (pcr >> 17) as u8,
            (pcr >> 9) as u8,
            (pcr >> 1) as u8,
            ((pcr & 1) << 7) as u8 | 0x7e,
            0,
        ]);
        p[12..21].copy_from_slice(&[0, 0, 1, 0xe0, 0, 0, 0x80, 0, 0]);
        p[21..26].copy_from_slice(&[0, 0, 0, 1, if key { 0x65 } else { 0x41 }]);
        out.extend_from_slice(&p);
        for _ in 0..129 {
            let mut p = [0xcd; N];
            p[..4].copy_from_slice(&[
                0x47,
                (VIDEO_PID >> 8) as u8,
                VIDEO_PID as u8,
                0x10 | self.next_cc(VIDEO_PID),
            ]);
            out.extend_from_slice(&p);
        }
        for t in 0..self.tracks.len() {
            let pid = self.tracks[t].0;
            for i in 0..3 {
                let mut p = [pid as u8; N];
                p[..4].copy_from_slice(&[
                    0x47,
                    if i == 0 { 0x40 } else { 0 } | (pid >> 8) as u8,
                    pid as u8,
                    0x10 | self.next_cc(pid),
                ]);
                if i == 0 {
                    p[4..13].copy_from_slice(&[0, 0, 1, 0xc0, 0, 0, 0x80, 0, 0]);
                }
                out.extend_from_slice(&p);
            }
        }
    }
}

/// Audio PIDs (from `AUDIO`-style layouts, PIDs 0x101..) present in `ts`.
pub(crate) fn audio_pids(ts: &[u8]) -> Vec<u16> {
    let mut pids: Vec<u16> = ts
        .as_chunks::<N>()
        .0
        .iter()
        .map(|p| ace_media::mpegts::ts_pid(p))
        .filter(|pid| (0x101..0x200).contains(pid))
        .collect();
    pids.sort_unstable();
    pids.dedup();
    pids
}

/// Elementary PIDs, in PMT order, of the first PMT packet in `ts`.
pub(crate) fn first_pmt_order(ts: &[u8]) -> Option<Vec<u16>> {
    let p = ts
        .as_chunks::<N>()
        .0
        .iter()
        .find(|p| ace_media::mpegts::ts_pid(*p) == PMT_PID && p[1] & 0x40 != 0)?;
    let off = 5 + p[4] as usize;
    let len = (((p[off + 1] & 15) as usize) << 8) | p[off + 2] as usize;
    let s = &p[off..off + 3 + len];
    assert_eq!(mpeg_crc32(s), 0, "PMT CRC must stay valid");
    let mut pos = 12 + ((((s[10] & 15) as usize) << 8) | s[11] as usize);
    let mut pids = Vec::new();
    while pos + 5 <= s.len() - 4 {
        pids.push((((s[pos + 1] & 31) as u16) << 8) | s[pos + 2] as u16);
        pos += 5 + ((((s[pos + 3] & 15) as usize) << 8) | s[pos + 4] as usize);
    }
    Some(pids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_tracks_advances_the_pmt_version_modulo_32_with_a_valid_crc() {
        let mut generator = Generator::new(&AUDIO);
        assert_eq!(generator.table_version(), (0, true));
        for expected in (1..32).chain([0, 1]) {
            generator.set_tracks(if expected % 2 == 0 {
                &AUDIO
            } else {
                &AUDIO[..2]
            });
            assert_eq!(generator.table_version(), (expected, true));
        }
    }
}
