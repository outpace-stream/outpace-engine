//! `bouncecast`: an endless synthetic test feed. The Outpace logo drifts across the frame,
//! bouncing off every edge and changing colour each time, over an animated Julia-set fractal and
//! a Game of Life, with a generated chiptune soundtrack that reacts to the bounces.
//!
//! It is a standalone program that shares no code with Outpace. It writes one Matroska stream
//! (uncompressed video plus 16-bit PCM) to stdout, for any encoder to consume:
//!
//! ```text
//! bouncecast | ffmpeg -i - -c:v libx264 -c:a aac -f flv rtmp://127.0.0.1:1935/live/my-channel
//! bouncecast | ffplay -
//! ```
//!
//! For convenience, `--output URL` runs that ffmpeg step itself: FLV to `rtmp://`, MPEG-TS over a
//! chunked PUT to `http(s)://` (Outpace's raw broadcast ingest), or MPEG-TS to a file.
//! Frames are paced to the wall clock unless `--no-pace` is given.
//!
//! Rendering runs on CPU threads, or with `--renderer gpu` (feature `gpu`) in a Vulkan compute
//! shader. Encoding uses libx264, or with `--encoder vaapi` the GPU's H.264 encoder.
//!
//! ```text
//! bouncecast --output rtmp://127.0.0.1:1935/live/my-channel
//! bouncecast --renderer gpu --encoder vaapi --output rtmp://127.0.0.1:1935/live/my-channel
//! bouncecast --frames 300 --no-pace > /dev/null      # rendering speed check
//! ```

mod bounce;
#[cfg(feature = "gpu")]
mod gpu;
mod life;
mod logo;
mod mkv;
mod music;
mod render;

use bounce::{Bouncer, Hit};
use mkv::{MkvWriter, PixelFormat, AUDIO_TRACK, VIDEO_TRACK};
use music::{Music, SAMPLE_RATE};
use render::{frame_len, Overlay, Renderer};
use std::io::{BufWriter, Write};

/// Operational log line on stderr with a UTC `HH:MM:SS.mmm` prefix. stdout carries the stream.
macro_rules! log {
    ($($arg:tt)*) => {{
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let secs = now.as_secs();
        eprintln!(
            "{:02}:{:02}:{:02}.{:03} {}",
            (secs / 3600) % 24,
            (secs / 60) % 60,
            secs % 60,
            now.subsec_millis(),
            format_args!($($arg)*)
        )
    }};
}
use std::process::{Child, ChildStdin, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

const USAGE: &str = "\
usage: bouncecast [options]

  --output URL       publish via ffmpeg: rtmp://..., http(s)://... (PUT) or a file path;
                     without it, a Matroska stream (raw video + PCM) goes to stdout
  --width N          frame width, even; a multiple of 4 for --renderer gpu (default 1920)
  --height N         frame height, even (default 1080)
  --fps N            frames per second, 1-120 (default 60)
  --bitrate KBPS     video bitrate cap in kbit/s (default 8000)
  --renderer R       cpu or gpu (gpu needs a build with --features gpu; default cpu)
  --encoder E        x264 (software) or vaapi (GPU H.264) (default x264)
  --vaapi-device P   render node for --encoder vaapi (default /dev/dri/renderD128)
  --bpm N            music tempo, 60-200 (default 125)
  --volume F         music volume, 0-2 (default 1)
  --mute             silent audio track
  --logo-size F      logo height as a fraction of frame height (default 0.24)
  --speed F          logo speed in frame heights per second (default 0.25)
  --threads N        CPU render threads (default: available cores, at most 4)
  --seed N           start position and colour sequence (default: from the clock)
  --frames N         stop after N frames (default: run forever)
  --no-pace          render as fast as possible instead of in real time
  --ffmpeg PATH      ffmpeg binary (default: ffmpeg)
";

struct Args {
    output: Option<String>,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    logo_size: f32,
    speed: f32,
    threads: usize,
    seed: u64,
    frames: Option<u64>,
    pace: bool,
    ffmpeg: String,
    renderer: RendererKind,
    encoder: Encoder,
    vaapi_device: String,
    bpm: f32,
    volume: f32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RendererKind {
    Cpu,
    Gpu,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Encoder {
    X264,
    Vaapi,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        output: None,
        width: 1920,
        height: 1080,
        fps: 60,
        bitrate: 8000,
        logo_size: 0.24,
        speed: 0.25,
        threads: std::thread::available_parallelism().map_or(2, |n| n.get().min(4)),
        seed: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |d| d.as_nanos() as u64),
        frames: None,
        pace: true,
        ffmpeg: "ffmpeg".into(),
        renderer: RendererKind::Cpu,
        encoder: Encoder::X264,
        vaapi_device: "/dev/dri/renderD128".into(),
        bpm: 125.0,
        volume: 1.0,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        fn num<T: std::str::FromStr>(flag: &str, v: String) -> Result<T, String> {
            v.parse()
                .map_err(|_| format!("{flag}: invalid value {v:?}"))
        }
        match flag.as_str() {
            "--output" => a.output = Some(val()?),
            "--width" => a.width = num(&flag, val()?)?,
            "--height" => a.height = num(&flag, val()?)?,
            "--fps" => a.fps = num(&flag, val()?)?,
            "--bitrate" => a.bitrate = num(&flag, val()?)?,
            "--logo-size" => a.logo_size = num(&flag, val()?)?,
            "--speed" => a.speed = num(&flag, val()?)?,
            "--threads" => a.threads = num(&flag, val()?)?,
            "--seed" => a.seed = num(&flag, val()?)?,
            "--frames" => a.frames = Some(num(&flag, val()?)?),
            "--no-pace" => a.pace = false,
            "--ffmpeg" => a.ffmpeg = val()?,
            "--renderer" => {
                a.renderer = match val()?.as_str() {
                    "cpu" => RendererKind::Cpu,
                    "gpu" => RendererKind::Gpu,
                    other => return Err(format!("--renderer: expected cpu or gpu, got {other:?}")),
                }
            }
            "--encoder" => {
                a.encoder = match val()?.as_str() {
                    "x264" => Encoder::X264,
                    "vaapi" => Encoder::Vaapi,
                    other => {
                        return Err(format!("--encoder: expected x264 or vaapi, got {other:?}"))
                    }
                }
            }
            "--vaapi-device" => a.vaapi_device = val()?,
            "--bpm" => a.bpm = num(&flag, val()?)?,
            "--volume" => a.volume = num(&flag, val()?)?,
            "--mute" => a.volume = 0.0,
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}\n\n{USAGE}")),
        }
    }
    if a.width < 64 || a.height < 64 || !a.width.is_multiple_of(2) || !a.height.is_multiple_of(2) {
        return Err("--width and --height must be even and at least 64".into());
    }
    if !(1..=120).contains(&a.fps) {
        return Err("--fps must be 1..=120".into());
    }
    if !(0.02..=0.9).contains(&a.logo_size) {
        return Err("--logo-size must be 0.02..=0.9".into());
    }
    if !(0.0..=5.0).contains(&a.speed) || a.threads == 0 || a.bitrate < 100 {
        return Err("--speed must be 0..=5, --threads at least 1, --bitrate at least 100".into());
    }
    if !(60.0..=200.0).contains(&a.bpm) || !(0.0..=2.0).contains(&a.volume) {
        return Err("--bpm must be 60..=200 and --volume 0..=2".into());
    }
    if a.renderer == RendererKind::Gpu {
        if !cfg!(feature = "gpu") {
            return Err("--renderer gpu needs a build with --features gpu".into());
        }
        if !a.width.is_multiple_of(4) {
            return Err("--renderer gpu needs a width divisible by 4".into());
        }
    }
    Ok(a)
}

/// Builds the FFmpeg command line that encodes the Matroska stream on its stdin and publishes
/// it to `output`. The stream declares its own size, pixel format, colour and audio format.
fn ffmpeg_args(a: &Args, output: &str) -> Vec<String> {
    let gop = (a.fps * 2).to_string();
    let rate = format!("{}k", a.bitrate);
    let buffer = format!("{}k", a.bitrate * 2);
    let mut v: Vec<String> = Vec::new();
    let mut push = |items: &[&str]| v.extend(items.iter().map(|s| s.to_string()));
    push(&["-hide_banner", "-nostdin", "-loglevel", "warning"]);
    if a.encoder == Encoder::Vaapi {
        push(&["-vaapi_device", &a.vaapi_device]);
    }
    push(&[
        "-f", "matroska", "-i", "pipe:0", "-map", "0:v:0", "-map", "0:a:0",
    ]);
    match a.encoder {
        Encoder::X264 => push(&[
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-profile:v",
            "high",
            "-pix_fmt",
            "yuv420p",
            "-keyint_min",
            &gop,
            "-sc_threshold",
            "0",
        ]),
        // Frames are uploaded to the GPU as NV12 (a no-op conversion when the renderer
        // already produces NV12). No B-frames: simplest for live and for every VCN generation.
        Encoder::Vaapi => push(&[
            "-vf",
            "format=nv12,hwupload",
            "-c:v",
            "h264_vaapi",
            "-profile:v",
            "high",
            "-rc_mode",
            "CBR",
            "-bf",
            "0",
        ]),
    }
    push(&[
        "-g", &gop, "-b:v", &rate, "-maxrate", &rate, "-bufsize", &buffer, "-c:a", "aac", "-b:a",
        "128k",
    ]);
    if output.starts_with("rtmp://") || output.starts_with("rtmps://") {
        push(&["-f", "flv", "-flvflags", "no_duration_filesize"]);
    } else if output.starts_with("http://") || output.starts_with("https://") {
        push(&["-f", "mpegts", "-method", "PUT"]);
    } else {
        push(&["-f", "mpegts", "-y"]);
    }
    push(&[output]);
    v
}

/// The selected frame renderer.
enum Backend {
    Cpu(Renderer),
    #[cfg(feature = "gpu")]
    Gpu(Box<gpu::GpuRenderer>),
}

impl Backend {
    fn pixel_format(&self) -> PixelFormat {
        match self {
            Backend::Cpu(_) => PixelFormat::I420,
            #[cfg(feature = "gpu")]
            Backend::Gpu(_) => PixelFormat::Nv12,
        }
    }

    fn render(&mut self, t: f32, overlay: &Overlay, frame: &mut [u8]) -> Result<(), String> {
        match self {
            Backend::Cpu(r) => {
                r.render(t, overlay, frame);
                Ok(())
            }
            #[cfg(feature = "gpu")]
            Backend::Gpu(r) => r.render(t, overlay, frame),
        }
    }
}

/// Where the Matroska stream goes, and the ffmpeg child to reap when it is ours.
struct Sink {
    mkv: MkvWriter<BufWriter<Box<dyn Write>>>,
    child: Option<Child>,
}

impl Sink {
    fn open(args: &Args, pixel_format: PixelFormat) -> Result<Self, String> {
        let (out, child): (Box<dyn Write>, Option<Child>) = match &args.output {
            Some(output) => {
                let mut child = Command::new(&args.ffmpeg)
                    .args(ffmpeg_args(args, output))
                    .stdin(Stdio::piped())
                    .spawn()
                    .map_err(|e| format!("cannot start {}: {e}", args.ffmpeg))?;
                let stdin: ChildStdin = child.stdin.take().ok_or("ffmpeg stdin unavailable")?;
                (Box::new(stdin), Some(child))
            }
            None => (Box::new(std::io::stdout()), None),
        };
        let stream = mkv::Stream {
            width: args.width,
            height: args.height,
            fps: args.fps,
            pixel_format,
            sample_rate: SAMPLE_RATE,
            channels: 2,
        };
        let mkv = MkvWriter::new(BufWriter::with_capacity(1 << 16, out), &stream)
            .map_err(|e| format!("output closed: {e}"))?;
        Ok(Self { mkv, child })
    }

    fn finish(self) -> Result<(), String> {
        let Sink { mut mkv, child } = self;
        let flushed = mkv.flush().map_err(|e| e.to_string());
        drop(mkv);
        match child {
            Some(mut child) => {
                let status = child.wait().map_err(|e| e.to_string())?;
                flushed?;
                status
                    .success()
                    .then_some(())
                    .ok_or_else(|| format!("ffmpeg exited with {status}"))
            }
            None => flushed,
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log!("[bouncecast] {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    let logo_h = (args.height as f32 * args.logo_size).round() as u32;
    let sprite = logo::Sprite::new(logo_h)?;
    let speed = args.height as f32 * args.speed;
    let mut bouncer = Bouncer::new(
        (args.width, args.height),
        (sprite.width, sprite.height),
        speed,
        args.seed,
    );
    let mut life = life::Life::new(
        args.width,
        args.height,
        args.fps,
        args.seed,
        bouncer.tint.primary,
    );
    let mut renderer = match args.renderer {
        RendererKind::Cpu => Backend::Cpu(Renderer::new(
            args.width,
            args.height,
            life.cell,
            args.threads,
        )),
        #[cfg(feature = "gpu")]
        RendererKind::Gpu => {
            let r = gpu::GpuRenderer::new(args.width, args.height, &sprite, &life)?;
            log!("[bouncecast] GPU renderer on {}", r.adapter);
            Backend::Gpu(Box::new(r))
        }
        #[cfg(not(feature = "gpu"))]
        RendererKind::Gpu => unreachable!("rejected by parse_args"),
    };
    let (max_x, max_y) = (
        args.width.saturating_sub(sprite.width) as f32,
        args.height.saturating_sub(sprite.height) as f32,
    );
    let mut frame = vec![0u8; frame_len(args.width, args.height)];

    let mut sink = Sink::open(&args, renderer.pixel_format())?;
    let mut music = Music::new(args.seed, args.bpm, args.volume);
    let mut pcm: Vec<i16> = Vec::new();
    let mut pcm_bytes: Vec<u8> = Vec::new();
    let mut samples_done = 0u64;
    // Never log the output URL: an RTMP key or HTTP token may be in it.
    let destination = match (&args.output, args.encoder) {
        (Some(_), Encoder::X264) => "ffmpeg libx264",
        (Some(_), Encoder::Vaapi) => "ffmpeg h264_vaapi",
        (None, _) => "Matroska on stdout",
    };
    log!(
        "[bouncecast] {}x{}@{}, {:?} renderer, {} bpm music, logo {}x{}, seed {}, to {destination}",
        args.width,
        args.height,
        args.fps,
        args.renderer,
        args.bpm,
        sprite.width,
        sprite.height,
        args.seed
    );

    let dt = 1.0 / args.fps as f32;
    let frame_time = Duration::from_secs_f64(1.0 / args.fps as f64);
    let fps = args.fps as f64;
    let start = Instant::now();
    let mut report_at = start + Duration::from_secs(600);
    let mut render_time = Duration::ZERO;
    let mut late_frames = 0u64;
    let mut flash = 0.0f32;
    let mut n = 0u64;
    let result = loop {
        if args.frames.is_some_and(|max| n >= max) {
            break Ok(());
        }
        // The scene clock is the frame count, so output timestamps and motion always agree.
        let t = n as f32 * dt;
        let t0 = Instant::now();
        let overlay = Overlay {
            life: &life,
            sprite: &sprite,
            x: bouncer.x,
            y: bouncer.y,
            tint: bouncer.tint,
            flash,
        };
        // Wrap the fractal clock so f32 keeps enough precision over months of uptime.
        if let Err(e) = renderer.render(t % 86_400.0, &overlay, &mut frame) {
            break Err(e);
        }
        render_time += t0.elapsed();
        // Audio for this frame's duration; the sample clock is derived from the frame count.
        let samples_end = (n + 1) * SAMPLE_RATE as u64 / args.fps as u64;
        pcm.clear();
        music.render((samples_end - samples_done) as usize, &mut pcm);
        pcm_bytes.clear();
        pcm_bytes.extend(pcm.iter().flat_map(|s| s.to_le_bytes()));
        let video_ns = ns(n, args.fps);
        let audio_ns = ns(samples_done, SAMPLE_RATE);
        samples_done = samples_end;
        let written = sink
            .mkv
            .block(VIDEO_TRACK, video_ns, &frame)
            .and_then(|()| sink.mkv.block(AUDIO_TRACK, audio_ns, &pcm_bytes));
        if let Err(e) = written {
            break Err(format!("output closed: {e}"));
        }

        let hit = bouncer.step(dt);
        match hit {
            Hit::Corner => flash = 1.0,
            Hit::Edge | Hit::None => flash = (flash - dt * 1.2).max(0.0),
        }
        match hit {
            Hit::Corner => music.corner(),
            Hit::Edge => music.bounce(if max_x > 0.0 {
                bouncer.x / max_x * 2.0 - 1.0
            } else {
                0.0
            }),
            Hit::None => {}
        }
        if hit != Hit::None {
            // Splash new cells, in the logo's new colour, where it touched the edge.
            let edge = |p: f32, max: f32, size: u32, frame: u32| {
                if p <= 1.0 {
                    0.0
                } else if p >= max - 1.0 {
                    frame as f32
                } else {
                    p + size as f32 / 2.0
                }
            };
            life.splash(
                edge(bouncer.x, max_x, sprite.width, args.width),
                edge(bouncer.y, max_y, sprite.height, args.height),
                bouncer.tint.primary,
            );
        }
        life.tick(bouncer.tint.primary);
        n += 1;

        if args.pace {
            let due = start + Duration::from_secs_f64(n as f64 / fps);
            let now = Instant::now();
            if due > now {
                std::thread::sleep(due - now);
            } else if now - due > frame_time * 2 {
                late_frames += 1;
            }
        }
        if Instant::now() >= report_at {
            report_at += Duration::from_secs(600);
            log!(
                "[bouncecast] {} frames, avg render {:.1} ms, {} late, {} bounces, {} corners",
                n,
                render_time.as_secs_f64() * 1e3 / n as f64,
                late_frames,
                bouncer.bounces,
                bouncer.corners
            );
        }
    };

    let elapsed = start.elapsed().as_secs_f64();
    let finished = sink.finish();
    log!(
        "[bouncecast] stopped after {n} frames in {elapsed:.1} s ({:.1} fps, avg render {:.1} ms), {} bounces, {} corners",
        n as f64 / elapsed.max(1e-9),
        render_time.as_secs_f64() * 1e3 / n.max(1) as f64,
        bouncer.bounces,
        bouncer.corners
    );
    result.and(finished)
}

/// `count` ticks of a `rate` Hz clock in nanoseconds. The product goes through u128 because at
/// 48 kHz `samples * 1e9` passes `u64::MAX` after about 4.4 days of uptime.
fn ns(count: u64, rate: u32) -> u64 {
    (count as u128 * 1_000_000_000 / rate as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_survive_long_uptime() {
        // A year of 48 kHz audio: the u64 product would have overflowed after about 4.4 days.
        let samples = 365 * 86_400 * 48_000u64;
        assert_eq!(ns(samples, 48_000), 365 * 86_400 * 1_000_000_000);
        assert_eq!(ns(1, 60), 16_666_666);
    }

    fn args(encoder: Encoder) -> Args {
        Args {
            output: None,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate: 8000,
            logo_size: 0.24,
            speed: 0.25,
            threads: 4,
            seed: 1,
            frames: None,
            pace: true,
            ffmpeg: "ffmpeg".into(),
            renderer: RendererKind::Cpu,
            encoder,
            vaapi_device: "/dev/dri/renderD128".into(),
            bpm: 125.0,
            volume: 1.0,
        }
    }

    fn after<'a>(v: &'a [String], flag: &str) -> &'a str {
        let i = v
            .iter()
            .position(|a| a == flag)
            .unwrap_or_else(|| panic!("{flag} missing"));
        &v[i + 1]
    }

    #[test]
    fn x264_command() {
        let v = ffmpeg_args(&args(Encoder::X264), "rtmp://127.0.0.1:1935/live/x");
        // One self-describing input carries both video and audio.
        assert_eq!(after(&v, "-f"), "matroska");
        assert_eq!(after(&v, "-i"), "pipe:0");
        assert_eq!(v.iter().filter(|a| *a == "-i").count(), 1);
        assert!(!v.iter().any(|a| a.contains("anullsrc") || a == "-shortest"));
        assert_eq!(after(&v, "-c:v"), "libx264");
        assert_eq!(after(&v, "-pix_fmt"), "yuv420p");
        assert_eq!(after(&v, "-g"), "120", "2 s GOP at 60 fps");
        assert_eq!(after(&v, "-c:a"), "aac");
        assert!(!v.iter().any(|a| a.contains("vaapi")));
        assert_eq!(v.last().unwrap(), "rtmp://127.0.0.1:1935/live/x");
        assert_eq!(
            v[v.len() - 3..v.len() - 1],
            ["-flvflags", "no_duration_filesize"]
        );
    }

    #[test]
    fn vaapi_command() {
        let v = ffmpeg_args(&args(Encoder::Vaapi), "http://h/broadcast/x");
        assert_eq!(after(&v, "-vaapi_device"), "/dev/dri/renderD128");
        // The device must be configured before the input that the filter uploads from.
        let device = v.iter().position(|a| a == "-vaapi_device").unwrap();
        assert!(device < v.iter().position(|a| a == "-i").unwrap());
        assert_eq!(after(&v, "-vf"), "format=nv12,hwupload");
        assert_eq!(after(&v, "-c:v"), "h264_vaapi");
        assert_eq!(after(&v, "-b:v"), "8000k");
        assert!(!v.iter().any(|a| a == "libx264" || a == "-preset"));
        assert_eq!(after(&v, "-method"), "PUT");
    }
}
