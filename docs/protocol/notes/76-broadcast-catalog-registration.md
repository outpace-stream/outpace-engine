# 76 — Registering a live broadcast with the catalog

Date: 2026-11-17. Builds on notes 45, 62 and 75; resolves the live-BT gap left open in
note 75.

## Executive summary

Engine 3.2.11 registers a live BitTorrent broadcast with two signed plain-HTTP calls:

1. `GET /checktorrent`, keyed by the SHA-1 of the wrapped transport. A reply with an `<id>`
   means the transport is already registered.
2. If there is no id, `POST /puttorrent` with the **raw** wrapped transport as the body. The
   reply's `<id>` is the catalog content id.

Both requests were captured from the unmodified engine against an isolated mock. Outpace
reproduces them byte for byte, and its signatures match the engine's own for the same nonce.
One maintainer-approved registration of an Outpace test broadcast was then made against the
production catalog. It returned a content id, and a stock Outpace daemon with
`OUTPACE_BEP9_CONTENT_ID_FALLBACK` off resolved that id through `/gettorrent` and played the
broadcast.

Outpace now does this on request, with `OUTPACE_BROADCAST_CATALOG_REGISTER=1`.

## Trigger

The original engine does not register a broadcast when it starts. The web UI's share and
content-id views call the client API method `get_content_id` with a `playlist_item_id`.
For a transport with no cached `player_id`, `get_content_id_from_transport_file` calls
`_update_torrent`, which performs the two requests below (note 75's call graph).
`get_content_id` with an `infohash` only performs a `/gettorrent` lookup and never registers.

## Capture

`tools/research/catalog-capture/` runs the engine as a console client inside `unshare -rnm`.
The namespace's only interface is loopback, every name resolves to a local mock, and catalog
ports on any address are redirected to it. The mock signs replies with the response key
(note 62) and echoes each request's nonce. The playlist item was
`tests/vectors/transport/synthetic-live-512k.bin`, so the golden vectors use no real
identifier.

The engine sent both calls to the same pool it uses for `/gettorrent` by name:
`s1`–`s3` under `torrentstream.net`, `.org` and `.info`, on port 80. Those names resolve to
two of the four catalog addresses Outpace already dials on port 8081. Registration has only
been exercised on port 80.

## Requests

Client fields are as in note 45: `_n` (version name), `_p` (platform), `_r` (nonce), `_v`
(version code).

| Call | Method | Parameters, in sent order | Body |
| --- | --- | --- | --- |
| `/checktorrent` | `GET` | `_n _p _r _v a checksum d z _s` | none |
| `/puttorrent` | `POST` | `_n _p _r _v a d infohash protected z _s` | raw wrapped transport, `Content-Type: application/octet-stream` |

- `checksum` is lowercase hex SHA-1 of the wrapped transport bytes.
- `infohash` is the lowercase hex swarm infohash.
- `a`, `d` and `z` are the affiliate, developer and zone ids. They are `0`.
- `protected` was `1` for both a fresh Outpace broadcast and the synthetic RSA-signed live
  vector. Its meaning for other transports is not established.

The puttorrent body is the transport itself, not Base64. This differs from the HLS
`/upload/raw` path in note 75.

### Signature

The parameters other than `_s` are sorted by key and joined as `k=v` with `#`. For
`puttorrent`, `#` and the raw body follow. The request key (`get_key(1)`, note 45) is
appended, and `_s` is the lowercase hex SHA-1 of the result:

```text
checktorrent: SHA1("_n=…#_p=…#_r=…#_v=…#a=0#checksum=<hex>#d=0#z=0" + key1)
puttorrent:   SHA1("_n=…#_p=…#_r=…#_v=…#a=0#d=0#infohash=<hex>#protected=1#z=0" + "#" + body + key1)
```

The `/gettorrent` formula in note 45 is the same rule with a single parameter.
`catalog_register.rs` pins both captured signatures over the synthetic vector.

## Responses

Replies are authenticated exactly as `/gettorrent` replies are (note 62): a `response` root
whose `sig` attribute signs the exact text with the response key, and whose `r` echoes the
request nonce. The engine accepted a mock reply carrying `<id>` and reported that id as the
content id.

Production shapes, observed once each:

```text
not registered:  <?xml version="1.0"?>\n<response sig="…" r="…"/>\n
puttorrent:      <?xml version="1.0"?>\n<response sig="…" r="…"><id>0123456789abcdef0123456789abcdef01234567</id></response>\n
registered:      <?xml version="1.0"?>\n<response sig="…" r="…"><id>…</id><infohash>Base64(20-byte infohash)</infohash></response>\n
```

A second `checktorrent` for the same transport, sent to a different host in the pool,
returned the same id together with the Base64 swarm infohash. The catalog therefore keys
registrations by transport, so re-registration is idempotent.

## Outpace behavior

`ace_swarm::catalog_register::register_live_transport` sends `checktorrent`, then
`puttorrent` only when no id comes back, trying each host in the pool. It requires a live
transport. It rejects replies that are unauthenticated, carry a different nonce, have a
malformed id, or report an `<infohash>` other than the transport's.

With `OUTPACE_BROADCAST_CATALOG_REGISTER=1`, a broadcast starts registering when it starts
announcing. Failures retry with backoff from 30 s up to 30 min. The task is cancelled with the
broadcast. The id is kept in memory, keyed by the transport hash, and is not persisted; after a
restart, `checktorrent` returns the same id. The setting is off by default and is ignored in
private peer mode.

The descriptor-hash content id and the BEP-9 fallback policy (note 63) are unchanged. A catalog
id is a separate value from the descriptor hash Outpace prints.

## Limits

- Registration publishes the descriptor, including its trackers and public key, to the catalog
  operator. No unregister call is known.
- `puttorrent` is unsigned by the server beyond the shared response key (note 62). The
  forgery limits described there still apply.
- Only live BT transports on engine 3.2.11's signing profile were exercised. VOD,
  multi-file and HLS registrations are out of scope.
- The production test used one maintainer-approved test broadcast. Its content id is kept
  in the private registry, not in this note.
