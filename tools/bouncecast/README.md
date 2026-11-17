# bouncecast

An endless synthetic test feed for [broadcasting](../../docs/wiki/Broadcasting.md). The Outpace
logo drifts across the frame and bounces off every edge. Each bounce changes its colour and
splashes Game of Life cells where it hit. Underneath is an animated Julia-set fractal. A logo
that lands in a corner glows.

Layers, back to front:

1. **Fractal**: a Julia set drifting through the |c| = 0.7885 family, with slow zoom, rotation
   and palette cycling. It is drawn at half resolution and bilinearly upscaled.
2. **Life**: a wrapping Conway grid, about 60 cells tall, at about six generations a second.
   Cells keep the logo colour they were born under, and fade over about a second when they
   die. Gliders come in from the edges every 3 s, and a sparse board is reseeded, so it never
   dies out.
3. **Logo**: the mark from `outpace-assets`, cropped to its visible edges, with a drop shadow.

The soundtrack is generated too. It's an NES-style chiptune in A minor at 125 BPM:
- **Instruments:** a pulse-wave lead playing generated 2-bar motifs (A A B A'), a pulse
  arpeggio, a stepped triangle bass, and noise and sine drums.
- **Arrangement:** the sections cycle through intro, groove, full, lifted, breakdown and
  drive. Every cycle picks a new chord progression and new motifs, so the music doesn't
  loop.
- **Tied to the picture:** each bounce plays a blip on a tone of the current chord, panned
  to where the logo hit. A corner hit plays a rising arpeggio with a crash.

The scene clock is the frame counter, so a given `--seed` always renders the same video.
Motion, life speed, fades and the 2 s keyframe interval all scale with `--fps`.

Rendering runs on CPU threads by default. With `--renderer gpu`, a wgpu compute shader
(`src/scene.wgsl`, Vulkan) draws the same scene at full resolution and writes NV12 directly.
Encoding uses libx264 by default. With `--encoder vaapi`, the GPU's H.264 encoder is used.

## How it fits with Outpace

bouncecast shares no code with Outpace and doesn't link it. It writes one Matroska stream
(uncompressed video plus 16-bit PCM) to stdout. The stream declares its own frame size, pixel
format, BT.709 colour and audio format, so any encoder can read it without format flags. An
encoder then publishes the result to a separate `outpace broadcast` process over RTMP or
HTTP:

```text
bouncecast ──Matroska──▶ ffmpeg ──RTMP / HTTP PUT──▶ outpace broadcast ──▶ peers
 (frames + music)     (H.264 + AAC)                    (separate process)
```

```bash
outpace broadcast my-channel &
bouncecast | ffmpeg -i - -c:v libx264 -preset veryfast -b:v 8000k -c:a aac \
  -f flv rtmp://127.0.0.1:1935/live/my-channel
bouncecast | ffplay -          # preview locally
```

`--output URL` is a shortcut that runs that ffmpeg step itself, with sensible encoder
settings.

## Usage

```bash
cargo build --release -p bouncecast
# Publish to a broadcast over RTMP (ffmpeg must be on PATH)
bouncecast --output rtmp://127.0.0.1:1935/live/my-channel
# or over Outpace's raw HTTP ingest (MPEG-TS, chunked PUT)
bouncecast --output http://127.0.0.1:6878/broadcast/my-channel
# Everything on the GPU (needs a build with --features gpu and a VAAPI/Vulkan render node)
bouncecast --renderer gpu --encoder vaapi --output rtmp://127.0.0.1:1935/live/my-channel
# The Matroska stream to stdout, rendered as fast as possible
bouncecast --frames 300 --no-pace > /dev/null
```

| Flag | Default | |
| --- | --- | --- |
| `--width`, `--height` | 1920x1080 | Even; the width must be a multiple of 4 for the GPU renderer |
| `--fps` | 60 | 1–120 |
| `--bitrate` | 8000 | Video cap in kbit/s (`-maxrate`, 2x `-bufsize`) |
| `--renderer` | `cpu` | `cpu` or `gpu` |
| `--encoder` | `x264` | `x264` (`veryfast`) or `vaapi` (`h264_vaapi`, CBR, no B-frames) |
| `--vaapi-device` | `/dev/dri/renderD128` | Render node for `--encoder vaapi` |
| `--threads` | cores, at most 4 | CPU renderer threads |
| `--bpm` | 125 | Music tempo, 60–200 |
| `--volume` | 1 | Music volume, 0–2; `--mute` sets 0 |

With `--output`, audio is encoded as AAC at 128 kbit/s. Run `bouncecast --help` for every option. The tool exits
when ffmpeg or the ingest goes away, so run it under a supervisor that restarts it.

### GPU build

```bash
cargo build --release -p bouncecast --features gpu
```

wgpu sits behind the `gpu` feature so that normal workspace builds don't compile it. The
binary loads the Vulkan driver at runtime, so it can't be a static musl build. Build it
against the target's glibc, for example in a `rust:1-trixie` container for Debian 13.
At runtime it needs `libvulkan1` and `mesa-vulkan-drivers`, plus `mesa-va-drivers` for
VAAPI, and read/write access to the render node.

## Cost

20 s of 1080p60 at 8000 kbit/s, encoded unpaced on a Ryzen 7 8745HS with a Radeon 780M:

| Renderer + encoder | Render per frame | CPU time | Cores in real time | Peak RSS |
| --- | --- | --- | --- | --- |
| `cpu` + `x264` | 12.2 ms (4 threads) | 85.6 s | 4.3 | 2.5 GB |
| `gpu` + `x264` | 0.9 ms | 35.9 s | 1.8 | 2.5 GB |
| `gpu` + `vaapi` | 0.9 ms | 3.3 s | 0.16 | 162 MB |

Live at 1080p60 with `gpu` + `vaapi`, bouncecast and ffmpeg each use about 7% of one core and
together hold about 210 MB; the iGPU is about 4% busy.

## 24/7 deployment

`deploy/` holds a systemd setup for one Linux host (for example a Proxmox LXC). It runs
`outpace broadcast outpace-bounce`, with bouncecast feeding it over loopback RTMP. The unit
uses the GPU renderer and encoder. Without a GPU, drop `--renderer` and `--encoder` from it
and `SupplementaryGroups=render`. In an unprivileged LXC, pass the render node with
`pct set <id> --dev0 /dev/dri/renderD128,gid=<container render gid>`.

```bash
apt-get install ffmpeg
useradd --system --home-dir /var/lib/outpace --shell /usr/sbin/nologin outpace
install -m 755 outpace bouncecast /usr/local/bin/
install -m 640 -g outpace deploy/outpace-bounce.env /etc/outpace-bounce.env
install -m 644 deploy/*.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now outpace-broadcast.service bouncecast.service
journalctl -u outpace-broadcast -o cat | grep -E 'Content ID|Transport URL'
```

Edit `--public-host` in `outpace-broadcast.service` to the host's address. Forward external
TCP 8623 to it so peers outside the LAN can connect. Restarting `outpace-broadcast` also
restarts the generator. The broadcast identity in `/var/lib/outpace/broadcasts/` survives
restarts; back it up, because it holds the signing key.
