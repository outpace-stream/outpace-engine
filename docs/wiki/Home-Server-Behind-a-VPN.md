# Home Server Behind a VPN

Run one engine on a home server, with its peer-to-peer traffic leaving through a VPN, and play
from any device on the LAN: VLC on a laptop, or the [Android app](Android-App) on a phone or
television using it as a remote engine. Client devices need no engine and no VPN of their own.

| Report | Details |
| --- | --- |
| **Source** | Outpace maintainers |
| **Reported** | 2026-11-15 |
| **Versions** | Outpace v1.2.1 (`.deb` package); Android app v1.0.0 |
| **Host** | Proxmox VE, unprivileged Debian LXC container |
| **Network** | A VPN client inside the container; the LAN stays reachable outside the tunnel |
| **Players** | VLC on desktop and Android; the Android app with a remote engine |
| **Works on** | Any Linux machine: a Proxmox container or VM, another hypervisor, or bare metal. The steps use Debian or Ubuntu with systemd. |

Proxmox is only how the machine is split up. On bare metal, or in any other VM, skip
[step 1](#1-create-the-container) and do the rest on the host. Keep the engine in the same network
namespace as the VPN, so its peer traffic follows the tunnel while players reach it on the LAN.

## 1. Create the container

Create an unprivileged Debian container with a static LAN address, outside the router's DHCP
pool. VPN clients need the TUN device, so pass it through in `/etc/pve/lxc/<id>.conf`:

```ini
unprivileged: 1
net0: name=eth0,bridge=vmbr0,gw=192.168.1.1,ip=192.168.1.20/24,type=veth
lxc.cgroup2.devices.allow: c 10:200 rwm
lxc.mount.entry: /dev/net dev/net none bind,create=dir
```

The examples use `192.168.1.20` for the container. Add `features: nesting=1,keyctl=1` only if you
run the engine in Docker inside it.

## 2. Set up the VPN

Install and connect your VPN client inside the container, then check two things:

- **Traffic leaves through the tunnel:** `curl https://ifconfig.me` shows the VPN's address, not
  your home one.
- **The LAN still answers:** the container can ping another device on `192.168.1.0/24`. Most
  clients keep local routes by default or have an "allow LAN access" switch; without it,
  players cannot reach the engine.

Make the VPN start at boot and reconnect on its own, and turn on its **kill switch** (also called
"block connections without VPN" or "lockdown"). Without one, peer traffic falls back to your home
connection whenever the tunnel drops, and the `ifconfig.me` check only proves the route while
the tunnel is up. If your client has no kill switch, add a firewall rule that drops outgoing
traffic on `eth0` except to the LAN and to the VPN server. Cover IPv6 too: block it, or make
sure the VPN tunnels it.

## 3. Install the engine

Download the package for the container's architecture from the
[releases page](https://github.com/outpace-stream/outpace-engine/releases) and install it. On
x86_64:

```bash
curl -LO https://github.com/outpace-stream/outpace-engine/releases/download/v1.2.1/outpace_1.2.1_amd64.deb
sudo apt install ./outpace_1.2.1_amd64.deb
sudoedit /etc/outpace/outpace.env
```

On ARM, use `outpace_1.2.1_arm64.deb` or `outpace_1.2.1_armhf.deb`. On Fedora and other RPM
distributions, use the `.rpm` with `dnf`; see
[Installation](Installation#debian-ubuntu-and-fedora-packages). Without systemd, run the
[release binary](Installation#release-binaries) with the same settings.

Set at least:

```bash
OUTPACE_BIND=192.168.1.20:6878      # reachable from the LAN
OUTPACE_ACE_COMPAT=true             # app shows the engine version; AceStream-API players work
```

Add `OUTPACE_COMPAT_ACESTREAM_SERVICES=true` if you want [Stream Search](Stream-Search) in the
app, and `OUTPACE_ENABLE_CAST=1` to [cast](Casting) to televisions. Then:

```bash
sudo systemctl enable --now outpace
journalctl -u outpace -f            # look for "listening on http://192.168.1.20:6878"
```

The [Docker image](Installation#container-image) works too; run it with `--network host` so it
shares the container's VPN.

## 4. Play from other devices

**VLC**, on any computer or phone on the LAN: open a network stream (**Media → Open Network
Stream** on desktop, **Browse → New stream** on Android) with

```text
http://192.168.1.20:6878/streams/ace/0123456789abcdef0123456789abcdef01234567
```

Replace the placeholder with a real content id. Append `.m3u8` for HLS.

**Android app**, on a phone or [Android TV](Android-TV): **Settings → Engine connection → Add a
remote engine**, address `192.168.1.20:6878`. The app then lists the engine's streams and hands
the player the server's URL, so video goes straight from the server to the player. See
[Remote engines](https://github.com/outpace-stream/outpace-android/blob/main/docs/Android-App.md#remote-engines)
in the app guide.

## Notes

- **Inbound peers:** behind most VPNs the engine downloads normally but no peer can connect in,
  so it uploads less. If your VPN forwards a port, set
  `OUTPACE_PEER_LISTEN=0.0.0.0:<forwarded port>`; see [Seeding and Peers](Seeding-and-Peers).
  Cloudflare WARP needs an extra fix, covered in
  [Deployment](Deployment#lxc-guest-behind-cloudflare-warp).
- **Security:** the API has no password. Keep the bind on the LAN address, and never forward
  6878 on the router. See [Exposing the API safely](Deployment#exposing-the-api-safely).
- **One engine, many viewers:** viewers of the same stream share one download. Each extra stream
  adds its own buffers; [Configuration](Configuration) lists the limits.
