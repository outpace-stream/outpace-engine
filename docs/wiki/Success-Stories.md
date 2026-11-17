# Success Stories

Outpace running somewhere new, reported by the people who did it. Each story is written up so you
can repeat it on your own hardware, and each starts with the same summary: where the report came
from, who made it, the versions and devices used, and what else it works on. Steps and notes for
adapting it follow.

| Story | Engine runs on | Plays on | Source |
| --- | --- | --- | --- |
| [Android TV](Android-TV) | the television, through the companion app | the same television (VLC) | Maintainers, TCL 55T69C |
| [OpenWrt Router](OpenWrt-Router) | the home router | LG and Samsung televisions on the LAN | 4PDA user, ASUS TUF-AX6000 |
| [NanoPi Zero2 with DietPi](NanoPi-Zero2-DietPi) | a single-board computer under systemd | not stated; any player on the LAN can use it | 4PDA user, NanoPi Zero2 |
| [Home Server Behind a VPN](Home-Server-Behind-a-VPN) | a Proxmox container with a VPN, or any Linux machine | VLC and the Android app on any LAN device | Maintainers, Proxmox VE |
| [Test Retransmission](Test-Retransmission) | a Proxmox container that broadcasts a generated 24/7 test signal | stock AceStream clients anywhere, by catalog id | Maintainers, Proxmox VE with a Radeon 780M |

Which one fits:

- **One Android television, nothing else running:** [Android TV](Android-TV).
- **Televisions that cannot run apps, and a capable router:** [OpenWrt Router](OpenWrt-Router).
- **A small always-on board you already own, or a spare one:**
  [NanoPi Zero2 with DietPi](NanoPi-Zero2-DietPi).
- **An always-on server, several viewers, traffic through a VPN:**
  [Home Server Behind a VPN](Home-Server-Behind-a-VPN). Televisions and phones then use it as a
  remote engine.
- **Broadcasting your own always-on stream to the whole network:**
  [Test Retransmission](Test-Retransmission).

The shared details live in [Installation](Installation), [Deployment](Deployment) and
[Playing Streams](Playing-Streams).

## Share your story

Got Outpace running on a NAS, a set-top box, a car, a boat, or anything we haven't listed? Tell
us, even if it is rough. Unusual hardware and odd setups are exactly what this page is for, and
a short report is enough to start: we can write it up with you.

Fill in the
[success story form](https://github.com/outpace-stream/outpace-engine/issues/new?template=success_story.yml),
post in
[Discussions → Show and tell](https://github.com/outpace-stream/outpace-engine/discussions/categories/show-and-tell),
or open a pull request that adds a page under `docs/wiki/`. Include what you can of:

- **Device and OS:** model, CPU architecture, RAM, operating system and version;
- **Versions:** Outpace engine, and the Android app if you used it;
- **What you changed:** `OUTPACE_*` settings, service or boot scripts, firewall or VPN rules;
- **How you play:** player, television or app, and on which devices;
- **What didn't work:** problems you hit and how you got past them;
- **Where you first posted it,** if anywhere, and how you'd like to be credited.

Leave out content ids, infohashes and stream names; use the placeholder id
`0123456789abcdef0123456789abcdef01234567` in examples.
