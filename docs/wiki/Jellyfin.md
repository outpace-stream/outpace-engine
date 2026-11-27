# Jellyfin

Jellyfin's Live TV can use Outpace as an **M3U tuner**. You write a playlist with one native
Outpace URL per channel, add it to Jellyfin, and Jellyfin opens the URL when someone tunes in.
No add-on, plugin or compatibility mode is needed.

Examples use the placeholder id `0123456789abcdef0123456789abcdef01234567` and an Outpace host
called `outpace-host`. Replace both.

## What was tested

| | |
| --- | --- |
| Jellyfin | `jellyfin/jellyfin:12.4.20261026-211555` container (reports server version 12.4.0) |
| Outpace | 1.3.0, built from `main`, default settings, on the Docker host |
| Layout | Jellyfin in a container on its own Docker network, reaching Outpace at the host's address on that network (not loopback) |
| Date | November 2026, with two current public live streams |

Setup was done through Jellyfin's HTTP API (startup wizard, `POST /LiveTv/TunerHosts`, guide
refresh), which is what the web UI calls. Playback was checked by requesting each channel's
`PlaybackInfo` and reading at least 10 seconds of the resulting stream with `ffmpeg`. No
Jellyfin app was used, so smoothness on a particular client wasn't measured.

| Channel URL | Result |
| --- | --- |
| `/streams/ace/<id>` (MPEG-TS) | Played. Jellyfin offered direct play and direct stream, and read Outpace through one shared connection buffered on its own disk. |
| `/streams/ace/<id>.m3u8` (HLS) | Played, but only through Jellyfin's own FFmpeg remux: Jellyfin doesn't direct-play an HLS tuner source. |
| `tvg-logo` attribute | Imported as the channel logo. |

## 1. Make Outpace reachable from Jellyfin

Outpace listens on `127.0.0.1:6878` by default, which only works when Jellyfin runs on the same
machine **outside** a container. In every other case, bind Outpace to an address Jellyfin can
reach:

- **Different hosts:** make Outpace listen on the LAN and firewall the port to the Jellyfin
  host. Run directly, set `OUTPACE_BIND=<lan-address>:6878`. With the shipped `compose.yml`,
  leave `OUTPACE_BIND` alone and set `OUTPACE_HTTP_HOST=<lan-address>`, which publishes the
  container's port there. The API has no authentication. See
  [Deployment](Deployment#exposing-the-api-safely).
- **Jellyfin in Docker, Outpace on the host:** bind Outpace to the Docker bridge address (for
  example `172.17.0.1:6878`) and use that address in the playlist. `127.0.0.1` inside the
  Jellyfin container is the container itself.
- **Both in Docker Compose:** put them on the same network and use the service name, for
  example `http://outpace:6878`.

Check from the Jellyfin host or container before going further:

```bash
curl http://outpace-host:6878/healthz
```

## 2. Write the playlist

One entry per channel. Use the plain native MPEG-TS URL:

```text
#EXTM3U
#EXTINF:-1 tvg-id="my-channel" tvg-name="My Channel" tvg-logo="http://example.com/logo.png" group-title="Outpace",My Channel
http://outpace-host:6878/streams/ace/0123456789abcdef0123456789abcdef01234567
```

- `tvg-id` is what Jellyfin matches against your guide data (see [EPG](#channel-logos-and-epg)).
- `tvg-logo` sets the channel logo. Jellyfin fetches it itself, so the URL must be reachable
  from the Jellyfin server.
- `group-title` becomes the channel's category.

Save it where Jellyfin can read it: a local path inside its container (for example a mounted
`/m3u/outpace.m3u`) or an HTTP URL.

## 3. Add the tuner

In Jellyfin: **Dashboard → Live TV → Tuner Devices → +**, choose **M3U Tuner**, and enter the file
path or URL. The settings used in the test:

| Setting | Value |
| --- | --- |
| Simultaneous stream limit | `0` (unlimited). Outpace shares one download per channel, so a limit only matters for your own bandwidth. |
| Allow stream sharing | On (the default). Jellyfin reuses an open channel instead of connecting to Outpace again. |
| Ignore DTS | On |
| Enable stream looping | Off. A live Outpace stream doesn't end, and looping hides real errors. |
| Read input at native frame rate | Off |

Then run **Refresh Guide** (Dashboard → Scheduled Tasks, or the button on the Live TV page).
The channels appear under Live TV.

## TS or HLS

Use the **MPEG-TS URL** (`/streams/ace/<id>`) for Jellyfin. With it:

- Jellyfin opens one shared connection to Outpace and buffers it on disk;
- clients that can play the stream get it with direct play or direct stream, without
  re-encoding;
- Jellyfin still remuxes or transcodes for clients that need it.

The HLS URL (`.m3u8`) also works, but Jellyfin always pushes it through its own FFmpeg, even for
clients that could play the original. That costs CPU on the Jellyfin server and adds Jellyfin's
own HLS segmenting on top of Outpace's. In the test, the first `PlaybackInfo` on an HLS channel
took about 40 seconds while Jellyfin probed it, against 3 to 5 seconds for the same stream over
TS. Later starts reused the cached probe result. Choose HLS only if you have a reason to.

## Startup times and timeouts

When a channel is tuned, three things happen in order:

1. Outpace resolves the id, finds peers and fills its startup buffer
   (`OUTPACE_PREBUFFER_MS`, 30 seconds of media by default, released early when the peers hold
   less history). With a healthy swarm this took 1 to 1.5 seconds in the test.
2. Jellyfin waits 3 seconds, then probes the stream with
   `ffprobe -analyzeduration 3000000 -probesize 1G`.
3. Jellyfin answers the client and playback starts.

Step 2 depends on the source more than on Outpace. For one of the two test streams the probe
finished in about 3 seconds. For the other, the same `ffprobe` command ran until it hit its
probe size limit, without returning, because analysis didn't stop at 3 seconds for that source,
and Jellyfin's `PlaybackInfo` didn't answer within two minutes. Reading that channel through
Jellyfin's stream URL still worked. To find out whether a channel is affected, run the same
probe against Outpace:

```bash
time ffprobe -analyzeduration 3000000 -probesize 1G \
  "http://outpace-host:6878/streams/ace/0123456789abcdef0123456789abcdef01234567"
```

If that returns in a few seconds, Jellyfin's probe will too. If it doesn't, the channel will be
slow to start in Jellyfin whatever Outpace does. Try the channel in another player such as VLC
before reporting it as an Outpace problem.

A stream with no peers fails on Outpace's side, with a JSON error and a `404`, `422` or `503`
status (see [Playing Streams](Playing-Streams#native-error-responses)). Jellyfin reports that as
a failed tune. Check `GET /streams/ace/<id>/status` to see why.

Lowering `OUTPACE_PREBUFFER_MS` (for example to `5000`) shortens step 1, but leaves less buffer
on the server for a struggling swarm. See
[Configuration](Configuration#playback-and-startup-buffering).

## Channel logos and EPG

Outpace provides **no EPG** and no channel catalogue. Jellyfin gets channel names, numbers and
logos from your playlist, and programme data from a guide source you add yourself:

- **Logos:** set `tvg-logo` in the playlist, or upload an image on the channel in Jellyfin.
- **Guide:** Dashboard → Live TV → **TV Guide Data Providers** → XMLTV, pointing at an XMLTV file
  or URL from your own tools. Jellyfin matches programmes to channels by `tvg-id`, or by the
  mapping you set on the provider. A tool such as [Dispatcharr](Dispatcharr) can produce both
  the playlist and an XMLTV feed.

Without a guide, channels still play; Jellyfin just shows no programme information.

## Monitoring

Watch Outpace with `GET /streams` and `GET /streams/ace/<id>/status`. Never point a health check
at a playback URL: that starts or keeps alive a real stream. See
[Support Sources and Operations](Support-Sources-and-Operations#health-and-status-endpoints).

When the last Jellyfin viewer stops, Jellyfin closes its connection, and Outpace tears the
stream down after its 30-second grace period.

## Related pages

- [Playing Streams](Playing-Streams): URL forms, status and errors.
- [HLS Output](HLS-Output): HLS startup settings.
- [Dispatcharr](Dispatcharr): a proxy and playlist manager that can sit between Outpace and
  Jellyfin.
- [Deployment](Deployment): Docker Compose and exposing the API.
