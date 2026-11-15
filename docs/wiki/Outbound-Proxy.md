# Outbound Proxy

Some networks (campus, hotel, mobile carriers, some ISPs) block or throttle peer-to-peer
traffic. Outpace then finds no peers and a stream never starts, even though it plays from
another network. Instead of routing the whole device through a VPN, you can send only
Outpace's own peer, tracker and catalog connections through a SOCKS5 proxy.

Any SOCKS5 endpoint works, including a local VPN client running in proxy mode. The proxy is
**off by default**; with it off, Outpace connects exactly as before.

**Avail.:** edge.

## Turning it on

```bash
export OUTPACE_PROXY=socks5://127.0.0.1:1080
export OUTPACE_PROXY_MODE=on
outpace serve
```

With a username and password:

```bash
export OUTPACE_PROXY='socks5://user:password@proxy.example.com:1080'
```

| Setting | Values | Meaning |
| --- | --- | --- |
| `OUTPACE_PROXY` | `socks5://[user:password@]host:port` | The proxy. `socks5h://` is accepted and means the same. Percent-encode `@`, `:` or `%` inside the username or password. IPv6 hosts go in brackets: `socks5://[::1]:1080`. |
| `OUTPACE_PROXY_MODE` | `off` (default), `on`, `fallback` | When the proxy is used. `on` and `fallback` require `OUTPACE_PROXY`. |

Setting `OUTPACE_PROXY` alone does nothing: the mode stays `off` and the log says so.

- **`on`**: every outgoing peer connection, tracker announce and catalog request goes through
  the proxy from the start.
- **`fallback`**: Outpace connects directly first and switches to the proxy the first time a
  live stream start gets no peers:
  - if discovery finds no peers at all, that start is retried once through the proxy;
  - if peers are found but none connects within 10 seconds, the start keeps going and its
    further connection attempts go through the proxy.

  A DHT lookup still running at the moment of the switch stops sending at once.

  The switch is engine-wide and lasts until restart; it never switches back on its own. VOD
  starts don't trigger it, but once it has happened they use the proxy too. A stream that has no
  peers anywhere also triggers it.

## What goes through the proxy

| Traffic | Proxy active |
| --- | --- |
| Outgoing TCP peer connections | SOCKS5 `CONNECT`. Once connected, data flows as on a direct connection. |
| Catalog lookups (content id to descriptor) | SOCKS5 `CONNECT`. Host names are resolved by the proxy, not locally. |
| AceStream search requests ([Stream Search](Stream-Search)) | SOCKS5 `CONNECT`, host names resolved by the proxy. |
| UDP trackers | SOCKS5 `UDP ASSOCIATE`, announcing port `0`. If the proxy refuses that command, UDP trackers are turned off for the rest of the process and never sent directly. Tracker host names are still resolved locally. |
| DHT | Off. It is never sent directly while the proxy is active. |
| Inbound peers and port mapping | See below. |
| Transport-file URLs, HLS origins and broadcast pull sources | Direct, as before. These are not peer traffic. |
| The network-location lookup for [Engine Camouflage](Engine-Camouflage) | Skipped. |

Without the DHT, and with UDP trackers only when the proxy supports `UDP ASSOCIATE`, discovery
has fewer sources than a direct connection. Bootstrap peers (`OUTPACE_ACE_PEERS`), peers
remembered from earlier sessions and peers learned from connected peers still work. Many
simple proxies, such as `ssh -D`, don't support `UDP ASSOCIATE`; look for
`proxy does not support UDP ASSOCIATE` in the log.

## Inbound peers and port mapping

Nobody can connect to you through a SOCKS5 proxy, so while the proxy is active Outpace does
not invite them to:

- the listen port it advertises to peers is `0`;
- it doesn't announce itself to trackers or the DHT.

With `OUTPACE_PROXY_MODE=on` inbound serving and port mapping are off by default. Setting
`OUTPACE_ENABLE_INBOUND=1` or `OUTPACE_ENABLE_PORT_MAPPING=1` together with it is a startup
error, and `outpace broadcast` refuses to run, because a broadcast needs inbound peers.

With `fallback`, inbound serving and port mapping keep their normal settings while Outpace is
direct. After the switch the listener stays open, so peers that already know your address can
still connect, but nothing new is advertised.

## Checking which path is in use

Each stream's status reports the path new connections take:

```bash
curl http://127.0.0.1:6878/streams/ace/cid:0123456789abcdef0123456789abcdef01234567/status
# {..., "peers": 3, "network_path": "proxy", ...}
```

`network_path` is `direct` or `proxy`. It is engine-wide: after a `fallback` switch, peers a
stream connected to earlier stay on their direct connections, while every new connection uses
the proxy.

`GET /network-policy` shows the settings without the endpoint or credentials:

```bash
curl http://127.0.0.1:6878/network-policy
# {"mode":"public","allowed_peer_count":null,
#  "proxy":{"mode":"fallback","path":"proxy","udp_trackers":true,"dht":false,"inbound_advertised":false}}
```

The log records the configuration once at startup and each change, under the `[proxy]` tag:

```text
[proxy] mode=on endpoint=socks5://127.0.0.1:1080 auth=yes
[proxy] fallback engaged: a live start found no peers directly; ...
[proxy] proxy unusable: proxy unreachable
```

## Security notes

- The username and password are never written to the log, to `/network-policy`, or to error
  messages. The log shows the proxy host and port and whether credentials are set.
- SOCKS5 username/password authentication is sent unencrypted to the proxy. Use a proxy on
  `localhost` or a trusted network. When credentials are set and the proxy host isn't loopback,
  the startup log carries a `[proxy] WARNING` line saying so.
- The proxy sees which peers and hosts Outpace connects to. Peer traffic itself is unchanged.
- Each proxy handshake has a deadline (the peer connect timeout for peers, 10 seconds
  otherwise), so a stalled proxy can't hang a start.

## Related pages

- [Configuration](Configuration#outbound-proxy)
- [Environment Variables](Environment-Variables)
- [NAT and Port Mapping](NAT-and-Port-Mapping#through-a-proxy)
- [Troubleshooting](Troubleshooting#no-peers-or-discovery-fails)
- [Private Peer Mode](Private-Peer-Mode)
