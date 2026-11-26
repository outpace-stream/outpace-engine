# Configuration reference

Every setting is an `OUTPACE_*` environment variable read once at daemon startup. The
[user wiki](https://github.com/outpace-stream/outpace-engine/wiki/Configuration) groups the same
settings by task with defaults and examples; this page keeps the detailed contract.

[Outbound-only private peer mode](private-peer-mode.md) restricts peer traffic to
an exact operator allowlist and disables public discovery and advertising.

Audio normalization settings (`OUTPACE_HLS_AUDIO_NORMALIZE`,
`OUTPACE_LIVE_AUDIO_NORMALIZE`, and `OUTPACE_FFMPEG_PATH`) are documented in
[live audio](live-audio.md). SRT ingest settings (`OUTPACE_SRT_*`) are documented
in [SRT ingest](srt-ingest.md).

Other environment variables parsed by the daemon include:

- `OUTPACE_PRIVATE_PEERS` - unset preserves public mode; a nonempty bounded numeric
  IPv4 endpoint list selects strict outbound-only private mode.
- `OUTPACE_PRIVATE_DESCRIPTORS` - operator-owned local descriptor alias/path JSON
  admitted at startup in private mode; see the guide for file and size limits.

- `OUTPACE_BEP9_CONTENT_ID_FALLBACK` - raw transport-hash BEP-9 fallback for content-id
  resolution after catalog failure, default off. Set to `1` on the receiving daemon or
  standalone `play` process for Outpace broadcast Ace links from another daemon. Applies to
  live and VOD; peer results do not populate the catalog-provenance infohash index.
- `OUTPACE_BIND` - HTTP bind address, default `127.0.0.1:6878`.
- `OUTPACE_RTMP_BIND` - RTMP ingest bind address, default `127.0.0.1:1935`.
- `OUTPACE_BROADCAST_OPTIONS` - strict named startup profile JSON, default `{}`; at most
  64 names and 262144 UTF-8 bytes. Optional title, bitrate in bytes/second, piece length,
  quality and ordered categories; invalid values fail with `invalid_broadcast_options`.
  Defaults: path name, `8375`, `65536`, `auto`, `["other"]`. No hot reload or profile-only mint.
  See [bounds and immutable resume](native-api.md#broadcast-startup-profiles).
- `OUTPACE_BROADCAST_SOURCES` - startup source JSON map, default `{}`; at most 64 names,
  256 KiB UTF-8, URLs up to 8,192 bytes. Duplicate/unknown fields fail startup. Source timing
  defaults: connect 5s, read 15s, reconnect initial 1s, maximum 30s; see the native API limits.
- `OUTPACE_BROADCAST_CATALOG_REGISTER` - opt-in AceStream catalog registration for broadcasts,
  default off; strict boolean. Once a public broadcast is announcing, the daemon asks the
  catalog whether its descriptor is known and uploads it if not, so the catalog-assigned
  `acestream://` id plays on stock clients. This publishes the transport descriptor (trackers
  and public key) to the catalog operator, with no unregister call. Failures retry with
  exponential backoff from 30 seconds up to 30 minutes. Never runs in private peer mode. The raw
  `PUT /broadcast/<name>` response reports `catalog_content_id`, `null` until registration
  succeeds.
- `OUTPACE_ALLOW_NON_GLOBAL_BROADCAST_SOURCE` - independent operator-only source address
  opt-in; only exact `1` enables it, default off. It cannot be set in an API request.
- `OUTPACE_DATA_DIR` - persistent identity/cache root, default platform data dir
  plus `outpace`.
- `OUTPACE_PEER_LISTEN` - inbound peer listener bind, default `0.0.0.0:8621`.
- `OUTPACE_SEED_STORE_BYTES` - byte budget for retained piece data (sizes both cache backends),
  default `134217728` (128 MiB). A hard safety cap; on a live stream
  `OUTPACE_SEED_RETENTION_SECS` is the primary limiter.
- `OUTPACE_SEED_RETENTION_SECS` - age bound (seconds) for a live reseed store, default `45`:
  retain roughly this much recent downloaded data for reseeding instead of filling
  `OUTPACE_SEED_STORE_BYTES`, so RAM tracks bitrate rather than always growing to the byte cap.
  `0` disables the age bound (byte-only, the pre-0.2 behavior). VOD stores are always byte-only.
- `OUTPACE_CACHE_TYPE` - where the seed store keeps piece data: `memory` (default) or `disk`.
  `disk` trades RAM for capacity, mirroring Acestream's disk-cache option.
- `OUTPACE_CACHE_DIR` - root dir for disk-mode piece files (one subdir per served stream; see
  below), default `<data_dir>/cache`. Only used when `OUTPACE_CACHE_TYPE=disk`. Startup wipes
  this directory only when it is empty or already contains a regular `.outpace-cache` sentinel
  file (written automatically on first successful disk-cache start). A symlink is not accepted
  as a sentinel. An unmarked non-empty path is refused rather than deleted.
- `OUTPACE_VOD_DISK_RESERVE_BYTES` - free space (bytes) a disk-mode VOD start must leave on the
  `OUTPACE_CACHE_DIR` filesystem, default `268435456` (256 MiB). The start needs
  `min(file size, OUTPACE_SEED_STORE_BYTES)` minus bytes already cached, plus the larger of this
  reserve and 5% of that; otherwise it fails with `507` `no_space`. One free-space query per
  VOD start; an unknown free space allows the start. Memory mode skips the check.
- `OUTPACE_PREFETCH_PIECES` - optional exact number of pieces behind the live edge to start at.
  When unset, Outpace derives the depth from the startup target and advertised bitrate, falling
  back to `32` pieces when the bitrate is unavailable.
- `OUTPACE_SESSION_BUFFER` - per-client fan-out channel depth, default `256`;
  must be at least `1`.
- `OUTPACE_PREBUFFER_MS` - target server-resident live startup queue duration, default `30000`.
  The server intentionally waits before sending the first media so clients begin with more
  tolerance for swarm jitter. `0` disables startup prebuffering and preserves the previous
  immediate-release behavior.
- `OUTPACE_PREBUFFER_BYTES` - hard byte ceiling for the startup queue, default `134217728`
  (128 MiB). Reaching it releases the available media early instead of exceeding the reservoir
  budget. Queue metadata and the rest of the daemon add memory overhead beyond this payload cap.
- `OUTPACE_PREBUFFER_TIMEOUT_MS` - deadline after the first media byte for reaching the
  target duration, default `15000`. It is absolute: a source gap or clock reset discards the
  queued media but does not restart the deadline. On expiry, Outpace degrades gracefully by
  releasing the available queue; it does not turn a short advertised live window into a playback
  failure.
- `OUTPACE_REQUEST_TIMEOUT_MS` - per-piece request timeout before re-requesting or skipping an
  evicted gap, default `1500`; must be lower than `OUTPACE_STALE_UPSTREAM_TIMEOUT_MS`. A live
  player drains in realtime, so raising this leaves a stuck piece to be healed by the much slower
  `OUTPACE_STALE_UPSTREAM_TIMEOUT_MS` pool teardown instead, which is a visible playback gap.
- `OUTPACE_STALE_UPSTREAM_TIMEOUT_MS` - whole-upstream no-progress timeout before reconnecting,
  default `12000`.
- `OUTPACE_REQUEST_CHECK_INTERVAL_MS` - request timeout sweep interval, default `1000`; must be
  lower than or equal to `OUTPACE_REQUEST_TIMEOUT_MS`.
- `OUTPACE_MAX_ACTIVE_UPSTREAMS` - active upstream peer cap for one live follower, default `4`;
  max `256`.
- `OUTPACE_MAX_PARALLEL_CONNECT` - peer connect race batch size, default `12`; max `1024`.
- `OUTPACE_MAX_PIECE_ADVANCE` - max pieces scheduled in one forward step, default `256`;
  max `16384`.
- `OUTPACE_MAX_REASM_PIECES_AHEAD` - max pieces accepted ahead of the emit cursor, default `512`;
  must be at least `OUTPACE_MAX_PIECE_ADVANCE`, max `65536`.
- `OUTPACE_HLS_SEGMENT_PACKETS` - hard MPEG-TS packet ceiling per HLS segment and packet-count
  fallback for streams without usable PCR, default `65536` (about 12.3 MB). This is a
  memory-safety bound, not the primary cut mechanism: segments normally cut on a keyframe once
  the target duration elapses, so the ceiling must comfortably hold one target-duration segment
  of a peaky high-bitrate stream (a 2160p HEVC GOP can burst well past its average).
- `OUTPACE_HLS_WINDOW_SEGMENTS` - retained HLS live window size, default `8`.
- `OUTPACE_HLS_SEGMENT_DURATION_MS` - requested PCR-timed HLS segment duration, default `5000`.
- `OUTPACE_HLS_STARTUP_SEGMENTS` - completed segments retained before a new HLS playlist request
  becomes ready, default `6`. `0` is accepted as a compatibility setting and behaves as `1`.
- `OUTPACE_HLS_STARTUP_TIMEOUT_MS` - maximum HLS playlist startup wait, default `45000`; must be
  at least `OUTPACE_HLS_SEGMENT_DURATION_MS`. If the startup segment count is not reached, the
  request returns a retryable HTTP `503` at the deadline.
- `OUTPACE_HISTORY_SECONDS` - retained live-HLS replay (timeshift) span, default `0` (disabled),
  max `3600`. Active native HLS packagers retain history, and with `OUTPACE_MANAGED_HLS=1` so do
  eligible managed origin-HLS TS viewers. It is in-memory, ends with its packager or viewer, and is
  not a persistent or UTC-indexed archive. Retention obeys elapsed time,
  accepted media duration, both byte budgets and a 4096-segment cap; the first limit wins.
- `OUTPACE_HISTORY_BYTES` - retained-history backing-allocation budget per HLS source, default
  `134217728` (128 MiB). Must be at least `188` and within the target's safe in-memory pool
  limit. At 8 Mbit/s, 128 MiB holds roughly two minutes.
- `OUTPACE_HISTORY_TOTAL_BYTES` - retained-history budget shared by all HLS sources and managed
  origin viewers, default
  `268435456` (256 MiB). Must be at least `OUTPACE_HISTORY_BYTES`. This bounds history only, not
  total process memory. See [timeshift](timeshift.md) for the per-viewer seek and
  return-to-live controls.
- `OUTPACE_MAX_UNCHOKED` - stable desired unchokes per exact served key, default `8` (U).
  For I interested peers, the coordinator selects `min(I, U + 1)` desired states in
  interest-entry order, with one rotating tail peer on nominal 10-second ticks. `0` still
  selects one optimistic peer. Watch updates and pending writes make this a desired selection,
  not an instantaneous wire-visible bound. Metadata replies bypass Interested/Unchoke.
- `OUTPACE_SEED_TTL_SECS` - idle-TTL (seconds, default 300; 0 disables) after which an *ownerless*
  seed-registry entry (one with no live producer lease) is force-evicted by the reaper. A backstop
  only: normal teardown rides the lease drop, and entries held by a live producer or a broadcast
  are never reaped. This TTL limits registry lifetime, not connection lifetime.
- `OUTPACE_MAX_INBOUND` - retained inbound connection limit, default `64` (G); `0` acts as `1`.
  Unidentified/incomplete handshakes consume this capacity too. Excess accepted sockets close
  immediately without queuing behind retained connections.
- `OUTPACE_MAX_INBOUND_PER_STREAM` - admitted connections per canonical stream, default `16`
  (N), serialized in Config as `max_inbound_per_stream`. Broadcast media and metadata aliases
  share this allowance, including idle, uninterested, choked and metadata-only connections.
  At defaults the 17th combined admission closes before a handshake response. `0` restores
  global-only admission and preserves U; positive N greater than G is valid and redundant for
  occupancy. Empty, negative, malformed, overflow and non-Unicode environment values fail
  startup; absence retains 16. Changes require restart/recreation. For rollback set
  `OUTPACE_MAX_INBOUND_PER_STREAM=0`; Compose requires an explicit service environment override,
  shown in [deployment.md](deployment.md), rather than `.env` interpolation alone.
- `OUTPACE_SUPPORT_SOURCES` - opt-in bounded JSON declarations for autonomous public live
  support. Unset is off; shares provider admission with playback and creates no viewers.
  See [support operation](support-operation.md) for controls and cleanup semantics.
- `OUTPACE_METRICS` - serves Prometheus text metrics at `GET /metrics` on the HTTP bind,
  default off; strict boolean. Off, `/metrics` returns `404`. See the
  [route reference](native-api.md#http-routes).
- `OUTPACE_SEED_DEBUG` - when present with any value (even empty), logs every inbound seeding
  message as `[seed-session]` lines. Very verbose; diagnostics only. Unlike the boolean gates,
  `0` does not turn it off: unset it instead.
- `OUTPACE_ENABLE_SEEDING` (default: on) - reciprocal upload gate over outbound leech
  connections (answering peers' chunk requests); strict boolean. Self-announce is gated on
  `OUTPACE_ENABLE_INBOUND`. Outbound reciprocal serving is outside the inbound
  admission/coordinator budgets.
- `OUTPACE_ENABLE_INBOUND` (default: on) - inbound peer serving (S2) gate. On by default,
  intentionally matching the Acestream engine's out-of-the-box behavior: a full P2P participant
  that binds its peer port (`OUTPACE_PEER_LISTEN`), accepts inbound peers, seeds, and
  self-announces to trackers + DHT. Only the HTTP API (`OUTPACE_BIND`) stays on localhost by
  default; the exposed surface is the peer port, as with Acestream. Set
  `OUTPACE_ENABLE_INBOUND=0` for a pure-leecher deployment (no inbound listener, no seeder
  self-announce).
- `OUTPACE_ENABLE_PORT_MAPPING` - best-effort UPnP/NAT-PMP mapping for the inbound TCP peer
  listener, default off. Use only when the process owns the host/LAN-facing network address;
  normal bridge-mode containers should use a manual router-to-Docker-host TCP forward.
- `OUTPACE_PORT_MAP_BACKEND` - `auto`, `upnp`, `natpmp`, or `none`; default `auto`.
- `OUTPACE_PORT_MAP_EXTERNAL_PORT` - optional external TCP port requested from the gateway.
- `OUTPACE_PROXY` - optional SOCKS5 proxy endpoint, `socks5://[user:password@]host:port`
  (`socks5h://` is accepted as the same thing; both resolve target names at the proxy). An IPv6
  host is bracketed; port must be nonzero; credentials are percent-decoded and each must be
  1-255 bytes; no path, query or fragment. Unset or empty means no proxy. Invalid values fail
  startup without echoing the value, and the endpoint is never logged with its credentials.
  Credentials sent to a non-loopback proxy travel unencrypted and are warned about at startup.
  Setting it while `OUTPACE_PROXY_MODE` is `off` logs that it is unused.
- `OUTPACE_PROXY_MODE` - `off` (default), `on` or `fallback`; unset or empty means `off`, other
  values fail startup. `on` and `fallback` require `OUTPACE_PROXY`. `on` proxies outgoing peer
  connections, UDP tracker announces (SOCKS5 `UDP ASSOCIATE`, turned off if the proxy refuses
  it) and catalog/search requests from the start, and turns inbound serving and port mapping off;
  an explicit `OUTPACE_ENABLE_INBOUND=1` or `OUTPACE_ENABLE_PORT_MAPPING=1` is a startup
  conflict, and `outpace broadcast` refuses to run. `fallback` stays direct until a live start
  finds no peers or none connects within 10 seconds, then proxies engine-wide until restart.
  While the proxy is active the DHT is off, the advertised listen port is `0` and there is no
  self-announce. `GET /network-policy` reports the mode and current path, never the endpoint.
- `OUTPACE_MANAGED_HLS` - serve descriptors that resolve to an HTTP(S) HLS origin through a
  bounded local relay, default off. Accepts exactly `1`, `true`, `0`, or `false`; anything
  else fails startup. Off, playback routes redirect the player straight to the origin. On,
  the daemon itself fetches origin playlists and media (public destinations only, with DNS
  pinning, deadlines and byte limits). With `OUTPACE_HISTORY_SECONDS` also set, qualifying TS
  origins gain bounded replay. See [origin HLS](native-api.md#origin-hls).
- `OUTPACE_ENABLE_CAST` - adds the `/cast/*` routes for DLNA/UPnP media renderers, default
  off; strict boolean. Off, those paths return `404`. Discovery runs only when
  `GET /cast/targets` asks for it and is cached for 60 seconds (`?refresh=1` searches again);
  there is no background discovery. `play` needs an `OUTPACE_BIND` the renderer can reach: a
  loopback bind fails with `409` `loopback_bind`. See the [cast routes](native-api.md#casting).
- `OUTPACE_ACE_COMPAT` - enables the supported Ace HTTP subset, default on. Accepts
  exactly `1`, `true`, `0`, or `false`. A present value takes precedence over the legacy
  alias, including `0`/`false`; an empty, malformed, or non-Unicode selected value fails
  startup. Set `0` to disable compatibility routes. The default was off up to 1.2.1.
- `OUTPACE_EXPERIMENTAL_ACE_COMPAT` - deprecated fallback, parsed only when
  `OUTPACE_ACE_COMPAT` is unset, with the same strict boolean rules. Rename it to
  `OUTPACE_ACE_COMPAT` when migrating; an unused invalid alias is ignored.
- `OUTPACE_COMPAT_ACESTREAM_SERVICES` - enables `GET /search` and its external stream-name index
  requests, default off; strict boolean. Independent of `OUTPACE_ACE_COMPAT`. Enabling it in
  private peer mode fails startup. Queries and the server address are exposed to the external
  plain-HTTP search service; see [stream search](native-api.md#stream-search).
- `OUTPACE_ACESTREAM_IDENTITY` - present one official Linux AceStream engine profile to peers,
  trackers, search, catalog and UPnP, default off; strict boolean. Off, the
  `OUTPACE_ACESTREAM_IDENTITY_*` overrides below are not read at all. On, every override is
  validated at startup and a bad value fails startup naming the variable; non-Unicode values
  are errors.
- `OUTPACE_ACESTREAM_IDENTITY_VERSION` - claimed engine version, default `3.2.11`. Exactly
  `X.Y.Z` with one- or two-digit parts, `X` 1-9 and `Y`/`Z` 0-99; the numeric code is
  `X*1000000 + Y*10000 + Z*100`. Used by handshakes, search, catalog and the User-Agent.
- `OUTPACE_ACESTREAM_IDENTITY_PLATFORM` - claimed platform, default `linux`; only `linux` is
  accepted.
- `OUTPACE_ACESTREAM_IDENTITY_DEVICE_ID` - 40-hex device id (case-insensitive). When unset, a
  random id is generated once and persisted as 20 raw bytes in
  `<OUTPACE_DATA_DIR>/acestream-device-id` (mode `0600` on Unix); a file of the wrong length
  fails startup instead of being replaced.
- `OUTPACE_ACESTREAM_IDENTITY_HARDWARE_KEY` - 40-hex hardware key (case-insensitive). Default is
  derived the way the official engine does on Linux, from `MemTotal` in `/proc/meminfo`; hosts
  where that cannot be read must set it or startup fails.
- `OUTPACE_ACESTREAM_IDENTITY_ASN` / `OUTPACE_ACESTREAM_IDENTITY_COUNTRY` - autonomous system
  number (positive 32-bit integer, optional `AS`/`as` prefix) and ISO 3166 alpha-2 country (two
  ASCII letters, case-insensitive). Set both or neither: one alone fails startup. With both set
  the location lookup is skipped; otherwise, once two distinct peers echo the same globally
  routable address, one bounded RIPEstat lookup fills them in (never while a proxy is active).
- `OUTPACE_ACESTREAM_IDENTITY_USER_AGENT` - User-Agent for search and AceStream web hosts,
  default the engine's fixed User-Agent for the claimed version. 1-256 bytes of printable ASCII.
  Catalog requests keep a per-request Chrome User-Agent.
- `OUTPACE_ACE_PEERS` - comma-separated bootstrap peer list for the live path.
- `OUTPACE_TRACKERS` - comma-separated `scheme://…` (typically `udp://host:port/announce`)
  tracker URLs minted into fresh broadcast descriptors for every producer path. Self-announce
  uses that stored list for both keys on resume; process changes affect future identities only,
  and empty stored lists remain empty.
  When set (non-empty after trimming) it fully replaces the built-in public default
  (`udp://t1.torrentstream.org:2710/announce`); unset leaves that default in place. Entries with
  no `scheme://` prefix (or longer than 256 bytes) are dropped with a warning, and the list is
  clamped to 64 entries. Note Outpace only **self-announces** to lowercase `udp://` trackers;
  entries with any other scheme are kept in the minted descriptor for other clients but warned
  about, since Outpace itself never announces there. Use this to point a broadcast at a
  private/local tracker (announcing to a private-address tracker also requires the non-global
  tracker allowance, tracked separately).
- `OUTPACE_TRACKER_ALLOW_NON_GLOBAL` - set to `1` to allow announcing to trackers that resolve
  to non-globally-routable addresses (private/LAN/loopback/link-local/multicast). Default deny;
  the SSRF guard on tracker destinations stays closed for normal use. Opt in only for controlled
  self-hosted or offline/test swarms on a trusted network. Scoped to tracker traffic.
- `OUTPACE_ALLOW_NON_GLOBAL_TRANSPORT` - set to `1` to allow fetching a transport descriptor
  file from a private/LAN/loopback host. Default deny; this relaxes the SSRF guard on the
  transport-file fetch **specifically** (not peer or tracker traffic), and even when enabled it
  still blocks link-local (e.g. the `169.254.169.254` cloud-metadata endpoint), unspecified,
  multicast, broadcast, and documentation ranges. Opt in only for controlled self-hosted / LAN
  swarms. If that transport file also points at a private *tracker*, announcing to it
  additionally needs `OUTPACE_TRACKER_ALLOW_NON_GLOBAL`.

These two `*_ALLOW_NON_GLOBAL*` knobs enable on the exact string `1` only (unlike the general
boolean gates below, `true` does **not** enable them).

Boolean gates accept exactly `1`, `true`, `0`, or `false`; other values are configuration errors
rather than silently disabling a feature.

Inbound admission grants no bandwidth/equal-share or class reservation guarantee; metadata-only
clients can fill a stream's allowance. DELETE removes registry/producer ownership, while existing
connections retain data and coordinator Arcs; their old coordinators stop receiving registry
rotations. Aborting only the listener leaves detached handlers running. The serve loop's 250 ms
polling can cancel reads and their inner timeouts, so those per-operation timeouts do not establish
a whole-session idle deadline. See [shared seeding](protocol/shared-seeding.md) for the
admission, coordination and lifetime boundaries.

The disk cache is **ephemeral**: a dedicated cache directory is cleared when a store is created and
never reloaded across restarts (live piece data goes stale), which also avoids serving
evicted-stale pieces. Disk I/O is currently synchronous. The first disk-cache start writes a
`.outpace-cache` sentinel so later wipes can distinguish a dedicated cache from an accidental
operator path. An existing non-empty cache without that regular sentinel file fails startup
instead of being deleted; use an empty directory or create the file only after verifying that
all existing contents may be deleted.

Disk mode never silently converts `OUTPACE_SEED_STORE_BYTES` into an equal per-stream RAM
allocation. An invalid/unwritable cache root fails daemon startup. If a new per-stream directory
cannot be created later (for example after a permission change), Outpace logs an escalating
`disk store creation failure #N` error and keeps playback running with **zero piece retention** for
that stream: no memory cache fallback is allocated, and reciprocal/inbound seeding has no retained
pieces to serve. This favors the operator's RAM bound over transient seeding capacity.

In disk mode each served stream keeps its pieces under
`<OUTPACE_CACHE_DIR>/<infohash_hex>-<generation>` (a process-unique suffix per store instance).
The directory is removed automatically when the stream is torn down (leech consumer disconnects,
broadcast `DELETE`, or process exit), and a dedicated cache root is wiped on startup only if it
is empty or contains `.outpace-cache`, so no per-stream directories accumulate.

Variables named `OUTPACE_TEST_*`, `OUTPACE_RUN_*` or `OUTPACE_DHT_COMPARE_*`, and
`OUTPACE_CAST_LIVE`, belong to the test suite and harnesses and do nothing in normal operation.
`tools/docs/check_reference.py` checks that every other `OUTPACE_*` name the daemon source
reads is documented on this page or in a page it delegates to.

## Provider session admission

Provider admission is unlimited by default. Set `OUTPACE_MAX_SESSIONS=8` for
an opt-in daemon-wide cap; unset or `0` keeps it unlimited. Negative, malformed
or overflowing values fail startup. Pending live opens and VOD resolutions count
alongside retained resources. Direct TS and HLS viewers of one live source share
one slot; cached VOD content and all of its ranges share one slot. Existing
same-key viewers can join at capacity. Metadata-only negotiation can succeed
before later playback admission fails.

New resources at capacity receive HTTP 503 with `Retry-After: 1`; compatibility
JSON start errors retain the response/error envelope. Native stream status and
service `get_status` expose numeric `occupied_sessions` and `max_sessions`
(`null` for unlimited), without changing `active_sessions`. Occupancy includes
pending work and retained owners after stop or cache expiry. Pump abort is
asynchronous: reuse is possible only after actual source destruction, and VOD
ranges retain their content owner until the last range is dropped. This limits
provider ownership, not RSS or exact socket counts during asynchronous cleanup.
There is no separate aggregate seed-store byte budget. CLI `outpace play` is
unaffected.
