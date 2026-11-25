# OpenWrt Router

Run the engine on the home router, so every device on the LAN can play streams with no computer
or set-top box left on. The reporter's goal was LG and Samsung televisions, which cannot run
the engine themselves.

| Report | Details |
| --- | --- |
| **Source** | [4PDA forum, Ace Stream \[Android\] thread](https://4pda.to/forum/index.php?showtopic=737440&st=14240), posts of 2026-10-30 and 2026-10-31 |
| **Reported by** | 4PDA user blackwrist |
| **Versions** | Not stated. The newest release then was v0.8.0, a 0.x development build. Use 1.0.0 or later. |
| **Device** | ASUS TUF-AX6000 (MediaTek Filogic, 64-bit ARM, 512 MiB RAM) |
| **OS** | OpenWrt 25.12.5, kernel 6.12 |
| **Players** | Intended for LG and Samsung televisions on the LAN |
| **Works on** | OpenWrt routers with a 64-bit ARM, 32-bit ARM, x86_64 or RISC-V 64 CPU; not MIPS |

The reporter wrote their own boot script in `/etc/init.d/` and a watchdog in `/usr/bin/`. The
repository now ships both as an OpenWrt service: procd starts the engine at boot and restarts it
if it exits. If it keeps exiting within an hour of starting, procd gives up after five tries,
and the engine stays down until you run `/etc/init.d/outpace restart`. The steps below use that service. The installer is tested on OpenWrt 24.10;
25.12 matches the reporter's router but is not part of that testing.

## What you need

- root SSH access to the router;
- enough free space in `/usr/bin` for the binary (about 11 MiB). `df -h /overlay` shows it;
- the router's LAN address. The examples use `192.168.1.1`.

## 1. Install the service

```sh
cd /tmp
wget -O outpace-src.tar.gz https://github.com/outpace-stream/outpace-engine/archive/refs/heads/main.tar.gz
tar -xzf outpace-src.tar.gz
sh outpace-engine-main/packaging/openwrt/install.sh 1.2.1
```

The installer reads the architecture from `/etc/openwrt_release` (`aarch64_cortex-a53` on the
TUF-AX6000), downloads the matching release, checks it against `SHA256SUMS`, and installs:

| Path | What it is |
| --- | --- |
| `/usr/bin/outpace` | the engine |
| `/etc/init.d/outpace` | procd service with automatic restart |
| `/etc/config/outpace` | settings (UCI) |
| `/etc/outpace/` | node identity, kept across `sysupgrade` |

It also creates an unprivileged `outpace` user and leaves the service disabled.

## 2. Configure it

The engine listens on `127.0.0.1` by default, which the televisions cannot reach. Bind it to
the LAN address and keep the low-memory defaults the installer set:

```sh
uci set outpace.main.bind='192.168.1.1:6878'
uci set outpace.main.enabled=1
uci commit outpace
```

`/etc/config/outpace` then sets these engine variables:

| Variable | Value | Why |
| --- | --- | --- |
| `OUTPACE_BIND` | `192.168.1.1:6878` | Reachable from the LAN, never from the WAN. |
| `OUTPACE_MAX_SESSIONS` | `1` | One stream at a time. |
| `OUTPACE_SEED_STORE_BYTES` | `33554432` (32 MiB) | Caps the seeding pool. |
| `OUTPACE_PREBUFFER_BYTES` | `16777216` (16 MiB) | Caps the startup buffer. |
| `OUTPACE_SESSION_BUFFER` | `64` | Smaller per-viewer queue. |
| `OUTPACE_CACHE_TYPE` | `memory` | Keeps cache writes off the internal flash. |

With 512 MiB of RAM, the TUF-AX6000 can afford more: raise `max_sessions` to `2` to serve two
televisions. Add any other variable as a `list env` line, for example to let the TVs
[cast](Casting) from a phone:

```sh
uci add_list outpace.main.env='OUTPACE_ENABLE_CAST=1'
uci commit outpace
```

## 3. Start it

These commands run the service script the installer added, `/etc/init.d/outpace`, which every
OpenWrt service has. `enable` and `start` are not `outpace` subcommands; the script runs
`outpace serve` for you.

```sh
/etc/init.d/outpace enable     # start at boot
/etc/init.d/outpace start
logread -e outpace             # look for "listening on http://192.168.1.1:6878"
```

Check it from another machine on the LAN:

```sh
curl http://192.168.1.1:6878/streams
```

## 4. Play on the television

Open a stream URL in the television's player or IPTV app:

```text
http://192.168.1.1:6878/streams/ace/0123456789abcdef0123456789abcdef01234567
```

Replace the placeholder with a real content id. Add `.m3u8` to the URL for players that
prefer HLS. Televisions with a DLNA renderer can also receive the stream with [Casting](Casting).
Players that only speak the AceStream API work too, with `OUTPACE_ACE_COMPAT=1` in the daemon environment for v1.2.1. Restart Outpace after setting it. See
[Playing Streams](Playing-Streams#acestream-compatible-api).

## Notes

- **Without the installer:** the same thing by hand is a binary and one command. Download the
  `aarch64-unknown-linux-musl` archive from [Installation](Installation#release-binaries), copy
  `outpace` to `/usr/bin/`, and run:

  ```sh
  OUTPACE_BIND=192.168.1.1:6878 OUTPACE_DATA_DIR=/etc/outpace OUTPACE_MAX_SESSIONS=1 \
  OUTPACE_SEED_STORE_BYTES=33554432 OUTPACE_PREBUFFER_BYTES=16777216 \
  OUTPACE_SESSION_BUFFER=64 /usr/bin/outpace serve
  ```

  You then need your own boot and restart scripts, as the reporter wrote.
- **Faster channel changes:** another user in the same thread shortened the startup buffers and
  turned off buffering in the player, since the engine already buffers. On a router, start from:

  ```sh
  uci add_list outpace.main.env='OUTPACE_PREBUFFER_MS=6000'
  uci commit outpace
  /etc/init.d/outpace restart
  ```

  See [Configuration](Configuration#playback-and-startup-buffering) before changing more.
- **Peers:** to upload as well as download, open TCP 8621 on the WAN or enable
  [port mapping](NAT-and-Port-Mapping). Never open 6878 on the WAN: the API has no password.
- **Flash wear:** don't point `OUTPACE_CACHE_DIR` at internal flash; use USB storage for a disk
  cache.
- **Other routers:** the architecture table and MIPS limits are in
  [Routers and Small Devices](Routers-and-Small-Devices#openwrt).
