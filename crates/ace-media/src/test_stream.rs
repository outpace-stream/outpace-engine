//! Synthetic multi-audio MPEG-TS for tests and bounded benchmarks. No public media is used.
//!
//! One program: PMT PID 0x1000, H.264 video on 0x100 and three audio tracks, each tagged with an
//! ISO 639 language descriptor: ADTS AAC `eng` (0x101), ADTS AAC `rus` (0x102) and MPEG audio
//! `spa` (0x103). A 40 ms frame carries one PCR, 130 video packets and three packets per audio
//! track. PAT/PMT repeat every third frame; every 25th frame is a video access point.
use crate::mpegts::mpeg_crc32;

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
    let mut out = Vec::with_capacity(frames * 142 * N);
    let mut cc = [0u8; 0x2000];
    let mut next = |pid: u16| {
        let c = cc[pid as usize];
        cc[pid as usize] = (c + 1) & 15;
        c
    };
    let table = pmt(&AUDIO);
    for frame in 0..frames {
        if frame % 3 == 0 {
            let mut p = pat();
            p[3] |= next(0);
            out.extend_from_slice(&p);
            let mut p = table;
            p[3] |= next(PMT_PID);
            out.extend_from_slice(&p);
        }
        let key = frame % 25 == 0;
        let pcr = frame as u64 * 3600;
        let mut p = [0xab; N];
        p[..4].copy_from_slice(&[
            0x47,
            0x40 | (VIDEO_PID >> 8) as u8,
            VIDEO_PID as u8,
            0x30 | next(VIDEO_PID),
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
                0x10 | next(VIDEO_PID),
            ]);
            out.extend_from_slice(&p);
        }
        for (pid, _, _) in AUDIO {
            for i in 0..3 {
                let mut p = [pid as u8; N];
                p[..4].copy_from_slice(&[
                    0x47,
                    if i == 0 { 0x40 } else { 0 } | (pid >> 8) as u8,
                    pid as u8,
                    0x10 | next(pid),
                ]);
                if i == 0 {
                    p[4..13].copy_from_slice(&[0, 0, 1, 0xc0, 0, 0, 0x80, 0, 0]);
                }
                out.extend_from_slice(&p);
            }
        }
    }
    out
}
