# Test Retransmission

A round-the-clock test broadcast, like a television test card, published from a home server
and played by stock AceStream clients anywhere. The picture is the Outpace logo bouncing like
the old DVD screensaver, changing colour at every edge, over an animated fractal and a Game of
Life. The soundtrack is a generated chiptune that reacts to the bounces. Everything is
synthetic, so the stream is safe to publish and can run forever as a reference signal for
the whole path: ingest, signing, swarm, catalog and players.

| Report | Details |
| --- | --- |
| **Source** | Outpace maintainers |
| **Reported** | 2026-11-17 |
| **Versions** | Outpace v1.2.1 (static musl binary) as the broadcaster; [bouncecast](https://github.com/outpace-stream/outpace-engine/tree/main/tools/bouncecast) from `main` as the generator |
| **Host** | Proxmox VE 9, unprivileged Debian 13 LXC, 6 cores, 3 GB; AMD Ryzen 7 8745HS with Radeon 780M graphics |
| **Signal** | 1920x1080 at 60 fps, H.264 High at 8 Mbit/s (constant bit rate), stereo AAC |
| **Players** | The Outpace Android app on another network; a second Outpace engine using only a catalog lookup |
| **Works on** | Any Linux machine. The GPU steps need an AMD or Intel GPU with VAAPI and Vulkan; without one, bouncecast uses the CPU. |

## How it fits together

Three separate programs, joined by a pipe and a socket. bouncecast shares no code with Outpace.

```text
bouncecast ──Matroska──▶ ffmpeg ──RTMP──▶ outpace broadcast ──▶ peers ──▶ players
(picture + music)      (H.264 + AAC)      (signs and serves)
```

bouncecast writes one self-describing stream (raw video plus PCM) to stdout. ffmpeg encodes it
and publishes it over RTMP to `outpace broadcast`. That process cuts the stream into signed
pieces, announces the swarm and serves peers.

## 1. Create the container

An unprivileged Debian 13 container with a static LAN address outside the DHCP pool. To
encode and render on the GPU, pass the render node through with the container's `render`
group id (`getent group render` inside the container; 992 on Debian 13):

```bash
pct create 120 local:vztmpl/debian-13-standard_13.1-1_amd64.tar.zst --hostname outpace-bounce \
  --cores 6 --memory 3072 --swap 512 --rootfs local-lvm:8 --unprivileged 1 --features nesting=1 \
  --net0 name=eth0,bridge=vmbr0,gw=192.168.1.1,ip=192.168.1.21/24,type=veth --onboot 1
pct set 120 --dev0 /dev/dri/renderD128,gid=992
pct start 120
```

Inside the container, install ffmpeg and the Mesa drivers, then create a service user:

```bash
apt-get install --no-install-recommends ffmpeg mesa-va-drivers mesa-vulkan-drivers libvulkan1
useradd --system --home-dir /var/lib/outpace --shell /usr/sbin/nologin outpace
```

## 2. Install the broadcaster

Install the static `outpace` binary from the
[release archive](https://github.com/outpace-stream/outpace-engine/releases) (check it against
`SHA256SUMS`) in `/usr/local/bin`. Then use the files from `tools/bouncecast/deploy/`:

- **`/etc/outpace-bounce.env`** binds the API to the LAN, RTMP to loopback, and the peer port to
  `8623`. It also sets the broadcast profile: title, bitrate (in bytes per second), piece size
  and quality.
- **`outpace-broadcast.service`** runs `outpace broadcast --public-host 192.168.1.21
  outpace-bounce`. Its identity, including the signing key, lives in `/var/lib/outpace/broadcasts/`.
  Back that directory up.

## 3. Install the generator

The GPU build loads Vulkan at runtime, so it is a normal glibc build rather than a static
one. Build it against the container's glibc, for example in a `rust:1-trixie` container for
Debian 13:

```bash
cargo build --release -p bouncecast --features gpu
```

`bouncecast.service` runs it with the RTMP shortcut, which starts ffmpeg for you. It is
`PartOf` the broadcaster, so restarting the broadcast restarts the feed:

```bash
bouncecast --renderer gpu --encoder vaapi --fps 60 \
  --output rtmp://127.0.0.1:1935/live/outpace-bounce
```

Enable both units: `systemctl enable --now outpace-broadcast bouncecast`.

## 4. Open the peer port

Peers outside the LAN must reach the broadcaster. Forward TCP `8623` on the router to
`192.168.1.21:8623`. This router answered neither UPnP nor NAT-PMP, so the forward is manual;
see [NAT and Port Mapping](NAT-and-Port-Mapping).

## 5. Register with the AceStream catalog

The `acestream://` link that `outpace broadcast` prints is the hash of its descriptor. Stock
AceStream clients can't resolve it, because they look ids up in the AceStream catalog. To get
a link that plays anywhere, register the broadcast as the original engine does. Add this to
the env file:

```dotenv
OUTPACE_BROADCAST_CATALOG_REGISTER=1
```

The catalog content id then appears in the log:

```bash
journalctl -u outpace-broadcast -o cat | grep 'catalog content id'
# [broadcast] outpace-bounce: catalog content id 0123456789abcdef0123456789abcdef01234567
```

This publishes the descriptor to the catalog operator, and there is no known way to undo it.
See [Broadcasting](Broadcasting#registering-with-the-acestream-catalog).

## 6. Watch it

Play `acestream://<catalog content id>` in the [Android app](Android-App) or any AceStream
client. Any Outpace engine works too, without extra settings:

```bash
outpace play acestream://0123456789abcdef0123456789abcdef01234567 | mpv -
```

## Results

The broadcast resolved through the catalog in about 2 seconds and played in the Android app
on a different network. A second Outpace daemon with no special settings decoded every frame
after its first keyframe at 1080p60, with audio averaging −16 dB and peaking at −3 dB.

Moving the work to the GPU made it cheap enough to run all day. Each line is 20 seconds of
1080p60 video encoded as fast as possible:

| Renderer and encoder | Time per frame | CPU time | Cores in real time | Peak memory |
| --- | --- | --- | --- | --- |
| CPU, libx264 | 12.2 ms | 85.6 s | 4.3 | 2.5 GB |
| GPU, libx264 | 0.9 ms | 35.9 s | 1.8 | 2.5 GB |
| GPU, VAAPI | 0.9 ms | 3.3 s | 0.16 | 162 MB |

Live, the container runs at about 7% of a core for bouncecast and 10% for ffmpeg, with about
365 MB resident across bouncecast, ffmpeg and Outpace. The integrated GPU is about 4% busy.

## What didn't work at first

- **A link only Outpace could play.** The printed link worked only for Outpace daemons that
  set `OUTPACE_BEP9_CONTENT_ID_FALLBACK=1`. Catalog registration (step 5) fixed it for every
  client.
- **Playing your own broadcast on the same daemon.** The engine dials its own peer port and
  the handshake times out. Test from a second daemon or another machine.
- **No automatic port mapping.** UPnP and NAT-PMP went unanswered, hence the manual forward.
- **A memory-hungry encoder.** At 1080p, libx264's lookahead held 1.5 GB. Hardware encoding
  brought ffmpeg down to about 140 MB.
- **A static binary can't load GPU drivers.** The GPU build needs glibc, and a build on a
  newer distribution required a newer glibc than Debian 13 has. Build in a container that
  matches the target.
- **Slow boot.** `systemd-networkd-wait-online` held the services for up to two minutes after
  a container restart. It clears by itself.
- **A brief jump at every generator restart.** Each ffmpeg run starts its timestamps from zero.
  Players such as VLC and mpv follow it, sometimes with a short hiccup.

## Adapting it

- **No GPU:** drop `--renderer`, `--encoder` and `SupplementaryGroups=render`. The CPU path needs
  about four cores at 1080p60; `--fps 30` or `--width 1280 --height 720` reduces that.
- **Your own feed:** anything that writes MPEG-TS or RTMP can replace bouncecast. See
  [Broadcasting](Broadcasting).
- **A private test:** skip steps 4 and 5 and play the transport URL on the LAN.
