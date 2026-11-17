//! A minimal streaming Matroska writer: one uncompressed video track and one 16-bit PCM audio
//! track, interleaved by time. Segment and clusters have unknown size, so the stream can be
//! written to a pipe forever. Any Matroska reader (ffmpeg, ffplay, mpv) can consume it with
//! no further flags: frame size, pixel format, colour and audio format are all declared.

use std::io::{self, Write};

/// Timestamp unit: 0.1 ms. Block timestamps are 16-bit offsets from their cluster, so a new
/// cluster starts every second (10,000 units), well inside the limit.
const TIMESTAMP_SCALE_NS: u64 = 100_000;
const CLUSTER_SPAN: u64 = 10_000;
const UNKNOWN_SIZE: [u8; 8] = [0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];

pub const VIDEO_TRACK: u64 = 1;
pub const AUDIO_TRACK: u64 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// Planar Y, U, V (yuv420p).
    I420,
    /// Y plane, then interleaved UV. Produced by the GPU renderer.
    #[cfg_attr(not(feature = "gpu"), allow(dead_code))]
    Nv12,
}

impl PixelFormat {
    fn fourcc(self) -> &'static [u8; 4] {
        match self {
            PixelFormat::I420 => b"I420",
            PixelFormat::Nv12 => b"NV12",
        }
    }
}

pub struct Stream {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub pixel_format: PixelFormat,
    pub sample_rate: u32,
    pub channels: u8,
}

pub struct MkvWriter<W: Write> {
    out: W,
    cluster: Option<u64>,
}

/// EBML variable-length size.
fn vint(n: u64) -> Vec<u8> {
    let mut len = 1;
    while len < 8 && n >= (1u64 << (7 * len)) - 1 {
        len += 1;
    }
    let marked = n | (1u64 << (7 * len));
    marked.to_be_bytes()[8 - len..].to_vec()
}

fn element(id: u32, body: &[u8]) -> Vec<u8> {
    let id_bytes = id.to_be_bytes();
    let skip = id_bytes.iter().take_while(|&&b| b == 0).count();
    let mut v = id_bytes[skip..].to_vec();
    v.extend(vint(body.len() as u64));
    v.extend_from_slice(body);
    v
}

fn uint(id: u32, n: u64) -> Vec<u8> {
    let bytes = n.to_be_bytes();
    let skip = bytes.iter().take_while(|&&b| b == 0).count().min(7);
    element(id, &bytes[skip..])
}

fn string(id: u32, s: &str) -> Vec<u8> {
    element(id, s.as_bytes())
}

fn float(id: u32, f: f64) -> Vec<u8> {
    element(id, &f.to_be_bytes())
}

fn master(id: u32, children: &[Vec<u8>]) -> Vec<u8> {
    element(id, &children.concat())
}

impl<W: Write> MkvWriter<W> {
    /// Writes the EBML header, the segment start, segment info and track list.
    pub fn new(mut out: W, s: &Stream) -> io::Result<Self> {
        let header = master(
            0x1A45DFA3,
            &[
                uint(0x4286, 1), // EBMLVersion
                uint(0x42F7, 1), // EBMLReadVersion
                uint(0x42F2, 4), // EBMLMaxIDLength
                uint(0x42F3, 8), // EBMLMaxSizeLength
                string(0x4282, "matroska"),
                uint(0x4287, 4), // DocTypeVersion
                uint(0x4285, 2), // DocTypeReadVersion
            ],
        );
        out.write_all(&header)?;
        out.write_all(&[0x18, 0x53, 0x80, 0x67])?; // Segment
        out.write_all(&UNKNOWN_SIZE)?;
        let app = concat!("bouncecast ", env!("CARGO_PKG_VERSION"));
        out.write_all(&master(
            0x1549A966, // Info
            &[
                uint(0x2AD7B1, TIMESTAMP_SCALE_NS),
                string(0x4D80, app), // MuxingApp
                string(0x5741, app), // WritingApp
            ],
        ))?;
        let colour = master(
            0x55B0, // Colour: BT.709 matrix, transfer and primaries, limited range
            &[
                uint(0x55B1, 1),
                uint(0x55B9, 1),
                uint(0x55BA, 1),
                uint(0x55BB, 1),
            ],
        );
        let video = master(
            0xAE, // TrackEntry
            &[
                uint(0xD7, VIDEO_TRACK),
                uint(0x73C5, VIDEO_TRACK), // TrackUID
                uint(0x83, 1),             // TrackType: video
                uint(0x9C, 0),             // FlagLacing
                string(0x86, "V_UNCOMPRESSED"),
                uint(0x23E383, 1_000_000_000 / s.fps as u64), // DefaultDuration (ns)
                master(
                    0xE0, // Video
                    &[
                        uint(0xB0, s.width as u64),
                        uint(0xBA, s.height as u64),
                        element(0x2EB524, s.pixel_format.fourcc()), // ColourSpace
                        colour,
                    ],
                ),
            ],
        );
        let audio = master(
            0xAE,
            &[
                uint(0xD7, AUDIO_TRACK),
                uint(0x73C5, AUDIO_TRACK),
                uint(0x83, 2), // TrackType: audio
                uint(0x9C, 0),
                string(0x86, "A_PCM/INT/LIT"),
                master(
                    0xE1, // Audio
                    &[
                        float(0xB5, s.sample_rate as f64),
                        uint(0x9F, s.channels as u64),
                        uint(0x6264, 16), // BitDepth
                    ],
                ),
            ],
        );
        out.write_all(&master(0x1654AE6B, &[video, audio]))?; // Tracks
        Ok(Self { out, cluster: None })
    }

    /// Writes one frame or audio chunk at `ts_ns` nanoseconds. Timestamps must not decrease.
    pub fn block(&mut self, track: u64, ts_ns: u64, data: &[u8]) -> io::Result<()> {
        let ts = ts_ns / TIMESTAMP_SCALE_NS;
        let start = match self.cluster {
            Some(start) if ts >= start && ts - start < CLUSTER_SPAN => start,
            _ => {
                self.out.write_all(&[0x1F, 0x43, 0xB6, 0x75])?; // Cluster
                self.out.write_all(&UNKNOWN_SIZE)?;
                self.out.write_all(&uint(0xE7, ts))?; // Timestamp
                self.cluster = Some(ts);
                ts
            }
        };
        let relative = (ts - start) as i16;
        let mut head = vec![0xA3]; // SimpleBlock
        head.extend(vint(data.len() as u64 + 4));
        head.push(0x80 | track as u8);
        head.extend(relative.to_be_bytes());
        head.push(0x80); // keyframe
        self.out.write_all(&head)?;
        self.out.write_all(data)
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }

    #[cfg(test)]
    pub fn into_inner(self) -> W {
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vint_lengths() {
        assert_eq!(vint(0), [0x80]);
        assert_eq!(vint(126), [0xFE]);
        // 127 is reserved in one byte (all ones), so it takes two.
        assert_eq!(vint(127), [0x40, 0x7F]);
        assert_eq!(vint(3_110_400), [0x10, 0x2F, 0x76, 0x00]);
    }

    #[test]
    fn elements_and_blocks() {
        assert_eq!(uint(0xD7, 1), [0xD7, 0x81, 0x01]);
        assert_eq!(
            uint(0x2AD7B1, 100_000),
            [0x2A, 0xD7, 0xB1, 0x83, 0x01, 0x86, 0xA0]
        );
        let stream = Stream {
            width: 64,
            height: 32,
            fps: 60,
            pixel_format: PixelFormat::Nv12,
            sample_rate: 48_000,
            channels: 2,
        };
        let mut w = MkvWriter::new(Vec::new(), &stream).unwrap();
        let header_len = w.out.len();
        w.block(VIDEO_TRACK, 0, &[7; 3]).unwrap();
        w.block(AUDIO_TRACK, 16_666_667, &[9; 4]).unwrap();
        // Over a second later: a new cluster.
        w.block(VIDEO_TRACK, 1_016_666_667, &[7; 3]).unwrap();
        let out = w.into_inner();
        assert!(out.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]));
        let body = &out[header_len..];
        let clusters = body
            .windows(4)
            .filter(|w| *w == [0x1F, 0x43, 0xB6, 0x75])
            .count();
        assert_eq!(clusters, 2);
        // Second block: track 2 at +166 units (16.6 ms), keyframe, 4 bytes.
        let block = [0xA3, 0x88, 0x82, 0x00, 0xA6, 0x80, 9, 9, 9, 9];
        assert!(body.windows(block.len()).any(|w| w == block));
        assert!(out.windows(4).any(|w| w == b"NV12"));
    }
}
