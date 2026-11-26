# Dispatcharr

[Dispatcharr](https://github.com/Dispatcharr/Dispatcharr) manages IPTV streams and channels and
proxies them to players and media servers. Outpace URLs go into it as ordinary HTTP streams,
from an M3U playlist or entered by hand. Dispatcharr then serves them from its own proxy and its
own M3U and XMLTV outputs, so Jellyfin, Plex or a player can use Dispatcharr's channel list.

Examples use the placeholder id `0123456789abcdef0123456789abcdef01234567`. Replace it.

## What was tested

| | |
| --- | --- |
| Dispatcharr | `ghcr.io/dispatcharr/dispatcharr:0.34.1`, all-in-one mode (`DISPATCHARR_ENV=aio`) |
| Outpace | 1.3.0, built from `main`, default settings, on the Docker host |
| Layout | Dispatcharr in a container on its own Docker network, reaching Outpace at the host's address on that network |
| Date | November 2026, with two current public live streams |

Setup was done through Dispatcharr's HTTP API (first user, M3U account from a playlist file,
channels created from the imported streams), which is what its web UI calls. Each channel was
then read for at least 10 seconds through Dispatcharr's proxy URL, `/proxy/ts/stream/<channel>`,
with `ffmpeg`, starting with Outpace's stream stopped.

| Outpace URL | Stream profile | Result |
| --- | --- | --- |
| `/streams/ace/<id>` (MPEG-TS) | FFmpeg (Dispatcharr's default) | Played. The first 10 seconds of media had arrived about 2 seconds after the request. |
| `/streams/ace/<id>` (MPEG-TS) | Proxy | Played. Lowest overhead: Dispatcharr relays the bytes without starting FFmpeg. |
| `/streams/ace/<id>.m3u8` (HLS) | Proxy or FFmpeg | Played. With the Proxy profile Dispatcharr detects HLS and switches to FFmpeg on its own. |

Dispatcharr's M3U output (`/output/m3u`) listed the channels with its proxy URLs.

## 1. Make Outpace reachable from Dispatcharr

Dispatcharr connects to Outpace itself, so the playlist must use an address Dispatcharr can
reach. `127.0.0.1` inside the Dispatcharr container is the container, not the host.

- **Same Compose project:** use the service name, for example `http://outpace:6878`
  ([below](#running-both-with-docker-compose)).
- **Outpace on the host:** bind it to the Docker bridge address (for example
  `OUTPACE_BIND=172.17.0.1:6878`) and use that address.
- **Different machines:** bind to the LAN address and firewall the port to the Dispatcharr host.
  The API has no authentication; see [Deployment](Deployment#exposing-the-api-safely).

## 2. Add the streams

**As an M3U account** (several channels): write a playlist with one native URL per channel and
add it under **M3U & EPG Manager → Add M3U**, as a URL or an uploaded file.

```text
#EXTM3U
#EXTINF:-1 tvg-id="my-channel" tvg-name="My Channel" tvg-logo="http://example.com/logo.png" group-title="Outpace",My Channel
http://outpace:6878/streams/ace/0123456789abcdef0123456789abcdef01234567
```

Set **Max streams** to `0` (unlimited) for the account. Outpace shares one download per
channel, so a limit only matters for your own bandwidth. After the first refresh Dispatcharr
shows the playlist's groups; refresh again or select the groups to finish the import.

**By hand** (one channel): add a stream with the same URL in **Streams**, then create a channel
from it.

Use the MPEG-TS URL. The HLS URL works, but Dispatcharr always runs FFmpeg for it, and Outpace
packages HLS segments only for FFmpeg to turn them back into MPEG-TS.

## 3. Choose a stream profile

The stream profile decides how Dispatcharr fetches the URL. Both tested profiles work with
Outpace's MPEG-TS URL:

- **Proxy**: Dispatcharr reads the HTTP stream and relays it. The cheapest option.
- **FFmpeg**: Dispatcharr runs `ffmpeg -i <url> -c copy -f mpegts`. Use it if a client needs
  FFmpeg's rewrapping.

Set the profile per channel, or change the default under **Settings → Stream Settings**.

Dispatcharr's proxy settings (under **Settings**) were left at their 0.34.1 defaults
in the test, and needed no change:

| Dispatcharr setting | Default |
| --- | --- |
| Buffering timeout | 15 s |
| Channel init grace period | 60 s |
| Channel shutdown delay | 0 s |

A shutdown delay of 0 is fine: when Dispatcharr disconnects, Outpace keeps the stream for its own
30-second grace period, so a quick re-tune doesn't start the swarm again.

The first data arrives once Outpace's startup buffer releases (`OUTPACE_PREBUFFER_MS`, 30
seconds of media by default, released early when the peers hold less history). Outpace then
sends what it queued in one burst, so Dispatcharr's buffer fills quickly.

## Health monitoring

Monitor **Outpace's status routes**, not the playback URL:

- `GET /healthz`: the daemon is up;
- `GET /streams`: running streams and their viewer counts;
- `GET /streams/ace/<id>/status`: one stream's `phase`, `error`, peers, bitrate and recovery
  counters.

None of these starts a stream. Requesting a playback URL as a health check starts a real stream
(joining the swarm and downloading) or keeps an idle one alive. See
[Support Sources and Operations](Support-Sources-and-Operations#health-and-status-endpoints).

Dispatcharr's own stream health monitor works on the bytes it receives. In one test run the
swarm behind a channel stalled for about 13 seconds. Dispatcharr logged the channel as unhealthy,
looked for an alternate stream, found none, and reconnected to the same Outpace URL; playback
then continued. If a channel has a second source (for example a backup id), add it as another
stream on the same channel so Dispatcharr has somewhere to switch to (failover itself wasn't
tested here).

When a channel looks stuck, check `GET /streams/ace/<id>/status` first. A rising `recovery`
count or `peers: 0` means the swarm is struggling, not Dispatcharr. See
[Playing Streams](Playing-Streams#watching-a-streams-status).

## Running both with Docker Compose

Add Dispatcharr to the project that runs Outpace's
[`compose.yml`](Deployment#docker-compose), with a `compose.override.yml` next to it:

```yaml
services:
  dispatcharr:
    image: ghcr.io/dispatcharr/dispatcharr:X.Y.Z   # pin a release
    restart: unless-stopped
    environment:
      DISPATCHARR_ENV: aio
    ports:
      - "127.0.0.1:9191:9191"
    volumes:
      - dispatcharr-data:/data
    depends_on:
      outpace:
        condition: service_healthy

volumes:
  dispatcharr-data:
```

Both services share the project's default network, so Dispatcharr reaches Outpace at
`http://outpace:6878`. Outpace's HTTP port needn't be published to the host for this; its
`ports` entry only controls access from outside Docker. The tested setup used a dedicated Docker
network and a fixed address rather than this file, but the connection is the same plain HTTP.

## Related pages

- [Playing Streams](Playing-Streams): URL forms, status and errors.
- [Jellyfin](Jellyfin): using Outpace, or Dispatcharr's output, as a Jellyfin M3U tuner.
- [Deployment](Deployment): Compose, exposing the API and reverse proxies.
