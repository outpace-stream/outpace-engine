# 75 — Where a public content id comes from

Date: 2026-11-14. Issues: #177 and #178. Builds on notes 35, 45, 62, and 63.

## Executive summary

A public AceStream content id is not a hash Outpace can compute from a transport
descriptor, a broadcaster key, or the wrapped file. Engine 3.2.11 treats it as a
server-assigned string.

The path that was actually executed, on a synthetic HLS publish, is an unsigned
HTTP upload. The engine base64-encodes the wrapped transport and posts it to
`http://api.torrentstream.net/upload/raw`. The content id is the top-level JSON
string `content_id` in the response. The client copies that string and does not
check it against the descriptor. With the network blocked, the same file still
has a swarm infohash and no content id. The id is not written into the transport.

A second path, recovered from the same binary but not executed against a mock,
registers a live BT torrent. `create_transport_file_wrapper` calls `_update_torrent`,
which calls `TSService.send_torrent` with the method name `puttorrent`. The XML
response tag `id` becomes `player_id` and is stored in the local `ts_players`
table. The lookup key is the transport checksum or the swarm infohash, plus
developer, affiliate, and zone ids. That is a publishing record, not a hash of
the broadcaster.

A provider content id supplied to `TorrentDef.create_live` is a third value.
The stream-node parser allows `[A-Za-z0-9_-]+` and requires a 32-hex provider
key. It is not the 40-hex catalog id.

No production registration was sent. Speaking the client protocol is not the
same as the catalog accepting the upload or an original client later resolving
it through `/gettorrent`.

## Build

Linux engine 3.2.11, version code 3021100, revision `py3@445e0ed`, dated
2025-09-22 in `engine_version.json`. The offsets below match notes 62 and 63
(`get_key` `0x12e530`, `check_response` `0x5599a0`,
`create_transport_file_wrapper` `0x651c60`), so this is that same build.

The engine tree is the swarmtest cache and is not in the repository. Dynamic
runs used a loopback mock. Connects to catalog ports were redirected there.
No production host was contacted for a registration, update, or lookup of a
real content id.

## Call graph

```text
--publish-hls-stream
--get-file-info-with-id
    upload_transport_file          core/Core.pyx, traceback lines 1473 and 1525
        POST /upload/raw                         lookup
        POST /upload/raw?publish=1&infohash=     publish
        JSON field content_id

create_transport_file_wrapper      CoreApp.so 0x651c60
    keyword create_content_id
    _update_torrent                wrapper 0x7546c0, body 0x74ed20
        send_torrent_to_server     0x535350
            TSService.send_torrent 0x4bf000
                send_request("puttorrent", ...)   0x7a9170
                XML tag id  ->  player_id
        save_player_data_to_db     0x35bba0
            table ts_players

get_content_id_from_transport_file 0x22fb00
    get_player_id_from_db          0x37e8f0
    else _update_torrent

PlaylistManager.check_hls_broadcasts
    _update_torrent(tfile, 0, 0, 0, True)
    webui_broadcast_list_set_item_content_id(infohash, content_id)
```

`_update_torrent` takes the torrent plus developer, affiliate, and zone ids.
Those three are required. `force_parse` and `creator` are optional. The broadcast
checker passes integer zeros and `force_parse=True`. The wrapper's keyword names
are `tdef`, `developer_id`, `affiliate_id`, `zone_id`, `force_parse`, and `creator`.

`create_transport_file_wrapper` also carries the exact error string
`create_transport_file_wrapper: error while getting content id`, and the keyword
`create_content_id`. Note 63 had inferred that option. It is a real string in
this build.

## HLS upload, confirmed dynamically

`--publish-hls-stream` and `--get-file-info-with-id` were run on a synthetic
HLS manifest served from loopback. The engine wrote a wrapped transport whose
descriptor type is `TransportDescriptorDASH`.

Lookup, from `--get-file-info-with-id`:

```text
POST /upload/raw HTTP/1.1
Host: api.torrentstream.net
Content-Type: application/octet-stream

<base64 of the wrapped AceStreamTransport bytes>
```

Publish, from `--publish-hls-stream`:

```text
POST /upload/raw?publish=1&infohash=<swarm infohash> HTTP/1.1
Host: api.torrentstream.net
Content-Type: application/octet-stream

<the same base64>
```

The body decodes to the file the engine wrote, byte for byte. There is no
catalog `_s` parameter and no authorization header on either request. The
infohash query value is the engine's `--get-infohash` output. It is not
SHA-1 of the wrapped file.

The mock answered with JSON. Three responses were substituted, one field at a
time:

| Response body | Reported content id |
| --- | --- |
| `{"content_id": "<40 hex supplied only by the mock>"}` | that exact string |
| `{"player_id": "<other 40 hex>"}` | field omitted |
| `{"id": "<other 40 hex>"}` | field omitted |

A combined object that also contained `player_id`, `id`, and
`result.content_id` still produced the top-level `content_id` string. The
client does not rehash the descriptor and does not prefer the other names.

With every `connect` forced to fail, `--get-file-info-with-id` still printed
`live`, `infohash`, and `trackers`, and omitted `content_id`. The log line is
`upload_transport_file: error`. The wrapped file does not contain a content-id
field. The id is not a property of the bytes.

`tools/research/content_id_upload.py` locks this request and response shape.
It does not open a socket.

## Live BT registration, static

`TSService.send_torrent` builds a parameter dict and calls `send_request`.
The method name is the string `puttorrent`. Parameters set in that function
are `d`, `a`, `z`, `multi` (`1` when the torrent is multi), `infohash`,
`protected`, and `force_parse`. The body content type is
`application/octet-stream`. The response is parsed with `domutils.get_tag_value`
on the tag `id`.

`send_torrent_to_server` logs the checksum, the three ids, and the resulting
`player_id`. `save_player_data_to_db` inserts into `ts_players` when that
`player_id` is new. The table, from `SQLiteCacheDBV2`, is:

```text
player_id     TEXT PRIMARY KEY
infohash      TEXT NOT NULL
developer_id  INTEGER
affiliate_id  INTEGER
zone_id       INTEGER
checksum      TEXT NOT NULL DEFAULT ''
```

`TsPlayersDBHandler.getPlayerId` selects `player_id` by checksum plus the three
ids, or, if no checksum is given, by infohash plus the three ids. It does not
hash those inputs. `get_content_id_from_transport_file` calls
`get_player_id_from_db` and otherwise `_update_torrent`.

`send_request` is the shared signer used by `/gettorrent`. On this build a
synthetic content-id playback signed exactly the string already documented in
note 45, with no `_t`:

```text
_n=3.2.11#_p=linux#_r=<nonce>#_v=3021100#pid=<content id>
```

followed by `TSService.get_key(1)`. Startup requests to `/c` and `/a` on the
`s*.torrentstream` hosts use a different secret and do include `_t`. They are
not the content-id assignment calls.

This `puttorrent` description is the client code. The function was not invoked
under the mock. `--stream-source-node --publish-stream` exited in the streamer
(`The result of the subtraction would be negative`) before any catalog socket
was opened. The XML the production server returns, and the rules it uses to
mint or reuse a `player_id`, are not known.

## What the id is, and what it is not

| Hypothesis | Result |
| --- | --- |
| Local hash of the wrapped transport | Rejected. The upload infohash differed from SHA-1 of the file, and the content id was absent when the server could not be reached. |
| Local hash of the broadcaster public key | Rejected for this build. Note 35's candidates still do not match a public id, and neither upload path hashes the key to produce one. |
| Hash of the selected infohash fields | Rejected. That hash is the swarm infohash. The content id is a separate column. |
| Random locally, then persisted | Rejected for the HLS path. A clean failure omits the id. Nothing in the file retains it. |
| Server assigns it | Confirmed for the HLS upload. The client echoed an id that existed only in the mock JSON. Strongly supported for live BT: the code stores the XML `id` as `player_id`. |
| Stable while the swarm infohash changes | Consistent with a server-side row, not demonstrated by mutating a production channel. The publish request does not send a previous content id. Reuse, if it happens, is the server's decision. |
| Provider content id | A different string. `createlivestream` passes `--content-id` into `create_live` only when a 32-hex provider key is set, and the alphabet is `[A-Za-z0-9_-]+`. |

Changing the stream name, bitrate, or piece size changes the live swarm
infohash, because those fields are in the infohash selector. That does not by
itself mint a public content id. A new public id appears when one of the
registration calls above returns one. The HLS client does not send the old id
along with the new infohash, so this investigation cannot say the catalog
updates a row rather than inserting another.

## Evidence

| Claim | Evidence | Confidence | Limits | Reproduction |
| --- | --- | --- | --- | --- |
| HLS content id is the JSON field `content_id` | Mock substitution on `--get-file-info-with-id`; `player_id` and `id` alone produced no content id | Confirmed | One descriptor type (`TransportDescriptorDASH`) and one engine build | Loopback manifest, `LD_PRELOAD` redirect of port 80 to a mock, then the CLI |
| The id is not in the transport and not available offline | Network-blocked rerun omitted `content_id`; the file has no such field | Confirmed | Same file and build | `connect` returns `ENETUNREACH`, rerun `--get-file-info-with-id` |
| Upload body is base64 of the wrapped bytes | Decoded body equalled the written `AceStreamTransport` file | Confirmed | The publish and the lookup both did this | Capture `POST /upload/raw` and decode the body |
| Publish query is `publish=1` plus the swarm infohash, not the raw file hash | `--publish-hls-stream` request line versus `--get-infohash` and SHA-1 of the file | Confirmed | Query infohash was not independently recomputed with the live selector in this note | Compare the query value, `--get-infohash`, and SHA-1 of the file |
| Upload is unsigned HTTP to `api.torrentstream.net:80` | Request headers captured on the mock | Confirmed | Production may still reject, rate-limit, or require a later authenticated call this client did not make | Read `Host` and the absence of `_s` |
| Live BT id is the `puttorrent` XML `id`, stored as `player_id` | `send_torrent` strings, `ts_players` schema, `getPlayerId` bytecode, `_update_torrent` log strings | Strongly supported | The HTTP exchange was not captured | Read `CoreApp.so` at the offsets above and `SqliteCacheDBHandler.pyc` |
| Public id is not a broadcaster-key hash | No such hash in either path; note 35 candidates unmatched | Confirmed as a negative for these code paths | Does not survey every historical engine build | Notes 35 and this note |
| Production catalog will resolve an id obtained this way | Not tested | Unresolved | Requires an authorized registration | Do not send one without maintainer approval |

## Outpace

Outpace broadcast content ids stay `SHA1` of the wrapped transport. Catalog
resolution, response authentication, and `OUTPACE_BEP9_CONTENT_ID_FALLBACK`
stay as they are. Nothing in this note is a reason to treat a catalog transport
as bound to the requested content id; note 62's gap remains. The upload does
not fix that. A server that returns a `content_id` is not proving that
`/gettorrent` for that id returns the uploaded bytes.

No wiki page changes. No operator-visible command, route, or default changes.

### Proposal, not implemented

A later change could add an explicit publish action, off unless asked:

1. Keep `ContentId`, `InfoHash`, and `TransportHash` distinct. The broadcast
   registry continues to use the transport hash.
2. Build the wrapped transport with the existing encoder. Post it with
   `tools/research/content_id_upload.py`'s body and target. Read `content_id`.
3. Store the returned id, the infohash, and the transport hash as registration
   state owned by that broadcast. Do not infer one from another.
4. Treat a second publish as a new request. The client does not send the old
   content id, so duplicate or update behavior has to be learned from the
   server and then implemented on purpose.
5. Do not attach catalog request signing to this upload. The captured client
   did not sign it. Do not weaken `/gettorrent` verification to match.
6. Leave the live `puttorrent` client unbuilt until a mock run captures the
   request bytes and the XML `id`. The static recovery is not a license to
   guess the body.
7. Gate the call behind an operator opt-in. Default broadcasts remain
   reachable through the transport hash and, with the existing flag, BEP-9.

That is a client. It becomes interop only if an authorized experiment shows
the production catalog storing the descriptor and an unmodified engine playing
the resulting `acestream://` link.

## Verdict

1. The public content id is created by the server and returned to the client.
   The HLS client sends the transport and reads JSON `content_id`. The live BT
   client, on the code path, sends `puttorrent` and reads XML `id`.
2. It is assigned, then optionally remembered. It is not a deterministic
   function of the descriptor. The HLS client does not remember it in the
   file. The live BT client remembers `player_id` in `ts_players`.
3. The swarm infohash is the selector hash from `transport-file.md`. The
   content id is a different value stored or returned beside it. SHA-1 of the
   wrapped bytes is a third value and is what Outpace uses for its own links.
4. The client can publish a descriptor whose infohash differs from an older
   one. The captured publish does not name the old content id. Whether the
   catalog rewrites one row is unresolved.
5. Outpace can form the HLS upload without the original engine. Whether the
   production server accepts it is unresolved and was not tried.
6. An original client can play such a broadcast only if that server then
   answers `/gettorrent` for the returned id with the uploaded transport.
   That was not shown.
7. The missing experiment is a mock run of `puttorrent` for a live BT
   transport, then one authorized publish against the production catalog with
   a test broadcaster. The first shows the live request bytes. The second
   shows whether either client protocol actually registers an id that
   `/gettorrent` will resolve.
