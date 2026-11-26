# Kodi

Kodi can play Outpace streams three ways:

| Way | Works? |
| --- | --- |
| [PVR IPTV Simple Client](#pvr-iptv-simple-client-recommended) with an M3U playlist of native URLs | Yes. Recommended. |
| [Opening a URL directly](#opening-a-url-directly) (native TS or HLS, or `/ace/getstream?id=`) | Yes |
| [Horus](#horus) and other add-ons written for the Ace Stream engine | Yes, including streams that aren't running yet. Outpace 1.3.0 played only a running stream; see [Horus](#horus). |

Examples use the placeholder id `0123456789abcdef0123456789abcdef01234567` and an Outpace host
called `outpace-host`. Replace both.

## What was tested

| | |
| --- | --- |
| Kodi | 21.2 (Debian trixie package `2:21.2+dfsg-4`) |
| PVR IPTV Simple Client | 21.10.2 (Debian package `kodi-pvr-iptvsimple`) |
| Horus | `script.module.horus` 1.3.2, the logon84 fork (the original is by Caperucitaferoz), commit of 2026-11-24 |
| Outpace | 1.3.0, built from `main`, default settings (compatibility API on) |
| Date | November 2026, with two current public live streams |

Kodi ran in a container under a virtual X display (Xvfb) and was driven through its JSON-RPC
API: `Player.Open` with a URL, a PVR channel or a Horus `plugin://` URL, then the player's
properties were polled for about 40 seconds. Playback counted as working when Kodi's video player
was active, decoding H.264, and its playback time advanced. The container had no sound device,
so audio output wasn't checked, and no picture was looked at. Kodi on a real TV box, Android or
LibreELEC wasn't tested.

## PVR IPTV Simple Client (recommended)

This is Kodi's own M3U client. It needs no Ace Stream add-on and talks only to Outpace's native
URLs.

1. Make Outpace reachable from the Kodi device, and firewall the port to your own devices. The
   API has no authentication; see [Deployment](Deployment#exposing-the-api-safely).
   - Outpace run directly: set `OUTPACE_BIND` to the Outpace machine's LAN address, for example
     `OUTPACE_BIND=192.168.1.10:6878`.
   - The shipped `compose.yml`: leave `OUTPACE_BIND` alone (it is `0.0.0.0:6878` inside the
     container) and set `OUTPACE_HTTP_HOST` to the host's LAN address, which publishes the port
     there instead of on loopback.
2. Write a playlist with one native URL per channel:

   ```text
   #EXTM3U
   #EXTINF:-1 tvg-id="my-channel" group-title="Outpace",My Channel
   http://outpace-host:6878/streams/ace/0123456789abcdef0123456789abcdef01234567
   ```

3. Install **PVR IPTV Simple Client** (Add-ons → Install from repository → PVR clients), open its
   settings, and set **M3U playlist** to the file or its URL.
4. Open **TV** in the main menu and play a channel.

| Channel URL | Result |
| --- | --- |
| `/streams/ace/<id>` (MPEG-TS) | Played. |
| `/streams/ace/<id>.m3u8` (HLS) | Played. |
| `/ace/getstream?id=<id>` (compatibility API) | Played. |

Video started about 20 seconds after `Player.Open` in the test, most of it Kodi opening and
analysing the stream on a container with software decoding only. Expect it to be shorter on a
device with hardware decoding, but this wasn't measured.

Outpace provides no EPG. For a guide, point the client's **EPG** setting at an XMLTV source from
your own tools, matched by `tvg-id`. Without one, channels still play.

## Opening a URL directly

Kodi plays a native Outpace URL like any other network stream, from a `.strm` file, a favourite
or JSON-RPC. JSON-RPC over HTTP needs Kodi's web server: turn on **Settings → Services →
Control → Allow remote control via HTTP**, use the port set there (8080 by default), and add
`-u user:password` to the command below if you set a username and password.

```bash
curl -s -H 'Content-Type: application/json' http://kodi-host:8080/jsonrpc -d '{
  "jsonrpc": "2.0", "id": 1, "method": "Player.Open",
  "params": {"item": {"file": "http://outpace-host:6878/streams/ace/0123456789abcdef0123456789abcdef01234567"}}}'
```

Native TS, native HLS (`.m3u8`) and `/ace/getstream?id=<id>` all played in the test.

One attempt at the HLS URL failed while the stream had no working upstream peer: Kodi's request
to the playlist timed out after 20 seconds and Kodi gave up. The same URL played on a retry once
the swarm recovered. If a channel fails in Kodi, check `GET /streams/ace/<id>/status` on Outpace
before blaming Kodi.

## Horus

Horus is a common Kodi add-on for Ace Stream links. It talks to the engine over HTTP, so it can
use Outpace's [compatibility API](Playing-Streams#acestream-compatible-api), which is on by
default.

**Settings:** set Horus's engine IP address to the Outpace host and the port to `6878`. Horus
only starts an engine of its own when nothing answers on that address, so with Outpace running it
uses Outpace. Its source also shows a first-run step that downloads the official engine on
Windows and some Linux setups (ARM, or x86 running as root); Outpace doesn't need that engine.
That step didn't run in the test (x86 Linux, not root) and wasn't tested.

**What it calls** (from its source, with a content id):

| Request | Outpace |
| --- | --- |
| `GET /webui/api/service?method=get_version&format=json` | Answers with Outpace's version. Horus uses it to detect the engine. |
| `GET /ace/getstream?format=json&id=<id>&sid=<hash>` | Answers with `playback_url`, `stat_url` and `command_url`. Outpace's version is below 3.1.29, so Horus sends `sid` rather than `pid`. |
| `GET /server/api?method=get_media_files&api_version=3&infohash=...` | Answers with one synthetic file entry. |
| `GET <stat_url>` every second | The first read starts the stream. Horus waits for `status` to become `dl`, for at most its **time limit** setting (30 seconds by default) while the status is anything else, and with no limit while it is `prebuf`. |
| Opens `<playback_url>` in Kodi's player | Plays. |
| `GET <command_url>?method=stop` | Stops this lease. |

**Cold starts.** Outpace doesn't start a TS stream on the `format=json` request itself, but
the first read of `stat_url` starts it ([details](Playing-Streams#json-sessions)). `stat_url`
then reports `prebuf` while the stream starts and `dl` once it runs, and Horus opens
`playback_url`. Outpace 1.3.0 and earlier started the stream only when `playback_url` opened, so
`stat_url` stayed `idle` and Horus gave up after its time limit with "timeout" for any stream that
wasn't already running.

The fix was checked by running Horus's own engine client code (its `lib/acestream` module and the
same wait-for-`dl` rule) against a local Outpace build on one current public live stream: `dl`
came after about 2 seconds and the playback URL returned media. It hasn't been re-run in Kodi
itself yet.

If Kodi or Horus is closed before playback opens, Outpace drops the stream once `stat_url`
has gone unread for 60 seconds.

If the stream can't start (for example, no peer is sharing it), `stat_url` answers with an
`error`. Horus ignores stat errors and keeps showing its last status, so its dialog can stay on
"prebuffering" until you cancel it.

## Add-ons that weren't tested

Other Kodi add-ons that target the Ace Stream engine weren't tested. One that uses only
`/ace/getstream`, `/ace/manifest.m3u8` and `/server/api` has a good chance of working, including
one that waits for `dl` on `stat_url` before opening the stream. Add-ons that use
the engine's legacy TCP API on port 62062 instead of HTTP won't work: Outpace doesn't implement
it.

If you test one, please report the add-on, its version and what happened in
[Discussions](https://github.com/outpace-stream/outpace-engine/discussions), without real
content ids.

## Related pages

- [Playing Streams](Playing-Streams): native URLs and the compatibility API.
- [Migrating from AceStream Engine](Migrating-from-AceStream-Engine): pointing existing clients at
  Outpace.
- [Troubleshooting](Troubleshooting): slow starts, stalls and errors.
