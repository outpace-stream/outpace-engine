# NanoPi Zero2 with DietPi

Run the engine on a palm-sized single-board computer that sits on the LAN all day, so phones,
televisions and players can use it as a remote engine. The reporter ran it as a systemd service
on a FriendlyElec NanoPi Zero2 with DietPi, and posted the unit's settings in reply to the
[OpenWrt Router](OpenWrt-Router) story.

| Report | Details |
| --- | --- |
| **Source** | [4PDA forum, Ace Stream \[Android\] thread](https://4pda.to/forum/index.php?showtopic=737440&st=14260), post of 2026-11-17 |
| **Reported by** | 4PDA user MediaStream66 |
| **Versions** | Not stated. Posted two days after v1.2.1; use 1.0.0 or later. |
| **Device** | FriendlyElec NanoPi Zero2 (the post spells it "Zepo2"). The board has a Rockchip RK3528A, four 64-bit ARM Cortex-A53 cores, and 1 GB or 2 GB of RAM; the post doesn't say which. |
| **OS** | DietPi, latest build at the time (DietPi is Debian-based) |
| **Players** | Not stated |
| **Works on** | Any Debian-based single-board computer or small Linux box with systemd, on 64-bit ARM, 32-bit ARM, x86_64 or RISC-V 64 |

The reporter ran the binary from `/opt/outpace/outpace` under their own systemd unit. Only the
end of the unit was visible in the post: two `Environment=` lines, `ExecStart`, a restart policy
and the `[Install]` section.

```ini
Environment="OUTPACE_PEER_LISTEN=0.0.0.0:8622"
Environment="OUTPACE_BIND=192.168.1.2:6878"
ExecStart=/opt/outpace/outpace serve
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

Two settings matter here. `OUTPACE_BIND` puts the HTTP API on the board's LAN address instead
of the default `127.0.0.1`, which is what lets other devices play through it. `OUTPACE_PEER_LISTEN`
moves the peer port off the default 8621 to 8622; the post doesn't say why. The steps below
rebuild that setup on the packaged service, with a dedicated user and the hardening the repository's
unit already has.

## What you need

- a board running DietPi, or any Debian-based system with systemd, with a fixed LAN address (set
  a DHCP reservation on the router if you can). The examples use `192.168.1.2`;
- an architecture with a release build. Run `uname -m` on the board: `aarch64` means the ARM64
  package, `armv7l` the ARMv7 one. See [Installation](Installation#release-binaries);
- a few hundred MiB of free RAM. On a 1 GB board, use the memory limits in
  [Routers and Small Devices](Routers-and-Small-Devices#memory-settings).

## 1. Install

The `.deb` package installs the binary, an unprivileged `outpace` user and the systemd unit. For
an ARM64 board:

```sh
version=1.2.1
curl -LO "https://github.com/outpace-stream/outpace-engine/releases/download/v${version}/outpace_${version}_arm64.deb"
sudo apt install "./outpace_${version}_arm64.deb"
outpace --version
```

Check the download against `SHA256SUMS` from the same release first; the steps are in
[Installation](Installation#debian-ubuntu-and-fedora-packages). To run a bare binary under your
own unit, as the reporter did, use the unit in [Deployment](Deployment#systemd) with
`ExecStart` pointing at your copy.

## 2. Configure it

Edit `/etc/outpace/outpace.env`:

```sh
OUTPACE_BIND=192.168.1.2:6878
OUTPACE_PEER_LISTEN=0.0.0.0:8622
```

Leave `OUTPACE_PEER_LISTEN` out to keep the default 8621. If you move it, any router port
forward must use the same number; see [NAT and Port Mapping](NAT-and-Port-Mapping). On a small
board, add the low-memory settings from
[Routers and Small Devices](Routers-and-Small-Devices#memory-settings), starting with
`OUTPACE_MAX_SESSIONS=1` and a smaller `OUTPACE_SEED_STORE_BYTES`.

## 3. Start it

```sh
sudo systemctl enable --now outpace
```

`enable --now` starts the service only if it isn't running yet. If Outpace was already running,
for example after an upgrade or an earlier setup, restart it so it reads the new settings:

```sh
sudo systemctl restart outpace
```

Then follow the log (Ctrl+C to stop) and look for `listening on http://192.168.1.2:6878`:

```sh
journalctl -u outpace -f
```

From another machine on the LAN:

```sh
curl http://192.168.1.2:6878/streams
```

## 4. Play

Open a stream URL in any player or IPTV app on the LAN:

```text
http://192.168.1.2:6878/streams/ace/0123456789abcdef0123456789abcdef01234567
```

Replace the placeholder with a real content id. The
[Android app](Android-App) can use this board as its remote engine, and players that only speak
the AceStream API need `OUTPACE_ACE_COMPAT=1`; see
[Playing Streams](Playing-Streams#acestream-compatible-api).

## Notes

- **Only the LAN:** `192.168.1.2:6878` is reachable by anything on the network, and the API has
  no password. Never forward 6878 to the internet.
- **Peers:** the peer port is the only one worth forwarding, and only if you want to upload as
  well as download. See [Seeding and Peers](Seeding-and-Peers).
- **SD card wear:** keep the default memory cache. A disk cache on an SD card or eMMC is
  rewritten constantly; use external storage if you need one.
- **Other boards:** the architecture table is in
  [Routers and Small Devices](Routers-and-Small-Devices#which-binary).
