# HLS Output

Outpace packages live streams into HLS itself. No FFmpeg is needed for this. A player opens
`/streams/ace/<id>.m3u8` (or the compatibility `/ace/manifest.m3u8`), and Outpace cuts the
incoming MPEG-TS into segments, keeps a sliding window of them and serves a live playlist.

HLS viewers of one source share one packager, and every packager shares the download with any
direct MPEG-TS viewers. Compatibility viewers that ask for a different
[audio language](#choosing-an-audio-language) can add up to four more packagers to a source,
so one source normally runs up to five. When a source restarts or is cleaned up while viewers
still hold language packagers, those packagers keep serving them until their leases end, so for
a while a source can run more, but never more than nine packagers in total. A language packager
that is being shut down still counts until it has stopped.

## When to use HLS instead of MPEG-TS

| Use HLS when... | Use direct MPEG-TS when... |
| --- | --- |
| the player or device only does HLS (browsers, many TVs, Apple devices) | the player handles a continuous stream (VLC, mpv, Jellyfin, ffmpeg) |
| clients reconnect often, since HLS tolerates short drops | you want the lowest delay to the live edge |
| you need [timeshift](Timeshift) | you are recording or piping to another tool |

## How segments are made

- Segments target `OUTPACE_HLS_SEGMENT_DURATION_MS` (default 5 seconds) on the stream's own
  clock, and cut on the **next keyframe** after that. H.264 and HEVC keyframes are detected.
  Segments are therefore usually a little longer than the target, and each one starts where a
  player can begin decoding.
- A segment can never exceed `OUTPACE_HLS_SEGMENT_PACKETS` 188-byte packets (default 65536,
  about 12.3 MB). This is a memory safety limit. A stream that hits it gets a forced cut that
  doesn't start on a keyframe. Raise the limit only for very high bitrates, such as 4K HEVC.
- Streams without usable timing are cut by packet count instead.
- The playlist keeps the last `OUTPACE_HLS_WINDOW_SEGMENTS` (default 8) segments. Older segments
  return `404`. Worst-case memory per packager is about (window + 1) × the segment ceiling. Each
  audio-language packager keeps its own window and, with [timeshift](Timeshift), its own
  retained history, so plan for up to five times that per source when compatibility clients
  request languages, and briefly more right after a source restart. Outpace runs at most nine
  packagers per source, but a stopped packager's segments are freed only once its cleanup ends
  and any segment request still reading it finishes, so memory can trail that limit briefly.

## Startup

A new playlist request waits until `OUTPACE_HLS_STARTUP_SEGMENTS` (default 6) segments are
complete, so the player starts with a healthy buffer. With the defaults that is about 30 seconds
of media after the swarm starts delivering.

- If that doesn't happen within `OUTPACE_HLS_STARTUP_TIMEOUT_MS` (default 45 seconds), the
  request returns **`503` with `Retry-After: 1`**. Most players retry on their own.
- The playlist's `EXT-X-START` points near the beginning of the startup window. Players may
  choose a different position.
- `0` startup segments behaves as `1`.

For faster starts at the cost of resilience:

```bash
export OUTPACE_HLS_STARTUP_SEGMENTS=2
export OUTPACE_HLS_SEGMENT_DURATION_MS=4000
```

Then start (or restart) Outpace from the same shell. With `docker run`, pass each setting
as `-e NAME=value`; with Compose, see [Deployment](Deployment#passing-other-settings).

Constraints the daemon checks at startup:

- startup segments ≤ window segments;
- startup timeout ≥ segment duration;
- segment ceiling ≥ 3 packets;
- the whole window must fit the platform's memory limit (relevant on 32-bit).

The [startup prebuffer](Configuration#playback-and-startup-buffering) applies to the shared
live source, so HLS goes through it as well. When the prebuffer releases, its queued media
arrives in one burst, and several segments complete almost at once. With the defaults, an HLS
start is roughly discovery time plus up to 15 seconds of prebuffering, or less when the peers
hold a shorter backlog than the target. Setting
`OUTPACE_PREBUFFER_MS=0` removes the prebuffer stage, and the startup segment count alone then
decides how long HLS waits.

## Recovery and errors

| Situation | Playlist response |
| --- | --- |
| Still starting, before the timeout | The request waits. |
| Not ready at the timeout | `503`, `Retry-After: 1` |
| Recovering from a source gap | The playlist waits for fresh segments after the gap; completed segments stay available. |
| Packaging failed (for example audio conversion failed) | `503`, `Retry-After: 1`, until the stream is stopped |
| Source ended | Closed playlist with `#EXT-X-ENDLIST` (see below) |

Segment requests never start a stream. A segment that isn't produced yet, or has left the
window, returns `404`.

## When a source ends

When a live source stops (the broadcaster ends, or the swarm reaches the end):

- the completed segments stay available;
- the playlist ends with `#EXT-X-ENDLIST`, so players play out what they have and stop instead
  of polling forever;
- the unfinished last segment is dropped;
- about 30 seconds after the last request the stream is cleaned up. The next request to the URL
  starts a fresh session.

For [support sources](Support-Sources-and-Operations), which restart on their own, HLS clients
must reopen the playlist URL to join the new run.

## Audio normalisation

By default Outpace doesn't touch audio. HLS takes the first AAC (ADTS) audio track; streams
without one keep their audio as it is. Many streams carry AC3 or MPEG audio (MP2/MP3), which
browsers, Apple devices and many TVs can't play over HLS. Normalisation fixes that.

| Setting | Converts | Avail. |
| --- | --- | --- |
| `OUTPACE_HLS_AUDIO_NORMALIZE=1` | Live **HLS** only. Direct MPEG-TS stays untouched. | all |
| `OUTPACE_LIVE_AUDIO_NORMALIZE=1` | The shared live source, once, before it goes to MPEG-TS viewers, HLS and support sources. | all |

With both on, only the shared conversion runs. VOD is never converted, whichever switch is on.

### What conversion does

- Selected AC3, MP2 or MP3 audio becomes **AAC-LC stereo, 48 kHz, 128 kbit/s**.
- AC3 is recognised both as DVB signals it (stream type `0x06` with an AC-3 descriptor or
  `AC-3` registration) and as ATSC stream type `0x81`, which FFmpeg's MPEG-TS muxer writes by
  default.
- Video is copied, never re-encoded.
- AAC audio passes through without conversion, and video-only streams start no converter.
- Everyone watching a source gets the same converted audio. There is no per-viewer track or
  language choice: with either switch on, HLS ignores
  [`preferred_audio_language`](#choosing-an-audio-language), and with
  `OUTPACE_LIVE_AUDIO_NORMALIZE=1` MPEG-TS ignores it too.
- Stereo downmix loses surround channels, and re-encoding costs some quality.
- At most **four** conversions run at once across the daemon.

### What it requires

- FFmpeg installed (see [Installation](Installation#ffmpeg)), and `OUTPACE_FFMPEG_PATH` if it
  isn't on `PATH`. Outpace tests it at startup and refuses to start if the test fails.
- An unencrypted 188-byte MPEG-TS stream with:
  - one program;
  - one MPEG-2, H.264 or HEVC video stream;
  - valid PCR timing;
  - simple (single-packet) PAT/PMT tables.

Audio-only streams, multiple programs or video streams, fragmented tables, MPEG Layer I and
audio tracks whose codec signals contradict each other are rejected. LATM, E-AC3 (DVB or ATSC
`0x87`) and DTS tracks are skipped when the stream also has an AAC, AC3 or MPEG audio track, and
rejected when it doesn't. Turning normalisation on can therefore refuse streams that play fine with it off.

### Errors with normalisation on

| Response | Meaning |
| --- | --- |
| `422` | The stream's layout isn't supported for conversion. |
| `503` + `Retry-After: 1` | All four conversion slots are busy. |
| `504` | The conversion didn't become ready within `OUTPACE_HLS_STARTUP_TIMEOUT_MS`. |
| `502` | The conversion failed. |
| Body ends with an error | A failure after playback started (direct TS). |

Short source interruptions (upstream reconnects, lag, layout or transport changes) do not end
playback: the conversion restarts at the next keyframe and the player sees one discontinuity.
A restart counts toward a limit unless the output before it had been playing for at least
10 seconds. A fourth counted restart within 60 seconds stops the session with `recovery_limit`.
The daemon log records each restart and failure under `[audio]`.

A conversion failure is final for that session. HLS keeps answering `503` and nothing retries
on its own. Stop the stream (`DELETE /streams/ace/<id>`) or let it go idle, and the next request
starts fresh. The stream's `/status` shows the state under `normalization`, for example
`{"generation":2,"state":"failed","failure":"child_exit"}`.

## Compatibility HLS

`/ace/manifest.m3u8` uses the same packager and settings as `/streams/.../.m3u8`. It adds
tokenised playlist and segment URLs (`/ace/m/...`, `/ace/c/...`) tied to a
[lease](Playing-Streams#leases), and [timeshift](Timeshift) controls. Segment URLs in its
playlists are absolute and built from the request's `Host` header.

Native playlists use root-relative paths (`/streams/ace/<id>/seg/<n>.ts`). They work through a
reverse proxy on the same host name, but not under a sub-path.

### Choosing an audio language

Add `preferred_audio_language=<code>` to `/ace/manifest.m3u8`, with a three-letter ISO 639-2 code
such as `rus`:

```text
http://127.0.0.1:6878/ace/manifest.m3u8?id=0123456789abcdef0123456789abcdef01234567&preferred_audio_language=rus
```

- The code is case-insensitive, and `ger`/`deu`, `fre`/`fra` and the other bibliographic and
  terminology pairs are treated as the same language. Any other value is ignored.
- It is matched against the language tags of the stream's audio tracks. If the stream has AAC
  audio, only an AAC track in that language is picked, so the codec you get never changes. If
  the stream has no AAC, a track in that language in any recognised codec is kept on its own.
- No match, an invalid code or no parameter gives the normal audio: the first AAC track, or
  every track when there is no AAC.
- Viewers asking for different languages still share **one download**. Each extra track costs
  one more packager, with its own retained segment window and history. A stream session runs at
  most **four** extra language packagers; later languages get the normal audio. An extra packager
  stops about 30 seconds after its last viewer's lease ends. Once a source has ended, a language
  that has no packager yet gets the normal audio, because a new packager would have nothing left
  to play.
- A language packager keeps playing the track it started with. If that track disappears from
  the stream, the packager stops and its viewers get `404`; requesting the manifest again picks
  the language afresh. Stopping the stream also ends its language packagers, so their viewers
  need a new manifest.
- With [audio normalisation](#audio-normalisation) on, the parameter is ignored.
- Native `/streams/...` playlists ignore the parameter.

The full rules are in the
[compatibility contract](https://github.com/outpace-stream/outpace-engine/blob/main/docs/protocol/compat-matrix.md#preferred-audio-language).

## Related pages

- [Timeshift](Timeshift): rewind HLS viewers.
- [Managed HLS Origins](Managed-HLS-Origins): descriptors that already point at HLS.
- [Configuration](Configuration#hls): all HLS settings.
