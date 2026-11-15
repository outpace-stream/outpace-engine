# Use cases

Real setups, reported by users and maintainers, written up so you can repeat them on your own
hardware. Each page starts with the same summary: where the report came from, who made it, the
versions and devices used, and what else it works on. The steps follow, then notes for adapting
them.

| Use case | Engine runs on | Plays on | Source |
| --- | --- | --- | --- |
| [Android TV](Android-TV) | the television, through the companion app | the same television (VLC) | Maintainers, TCL 55T69C |
| [OpenWrt Router](OpenWrt-Router) | the home router | LG and Samsung televisions on the LAN | 4PDA user, ASUS TUF-AX6000 |
| [Home Server Behind a VPN](Home-Server-Behind-a-VPN) | a Proxmox container with a VPN, or any Linux machine | VLC and the Android app on any LAN device | Maintainers, Proxmox VE |

Which one fits:

- **One Android television, nothing else running:** [Android TV](Android-TV).
- **Televisions that cannot run apps, and a capable router:** [OpenWrt Router](OpenWrt-Router).
- **An always-on server, several viewers, traffic through a VPN:**
  [Home Server Behind a VPN](Home-Server-Behind-a-VPN). Televisions and phones then use it as a
  remote engine.

The shared details live in [Installation](Installation), [Deployment](Deployment) and
[Playing Streams](Playing-Streams).

Have a setup worth adding? Open an issue with the same details: device, OS, Outpace and app
versions, the settings you changed, and where you first reported it. Leave out content ids and
stream names.
