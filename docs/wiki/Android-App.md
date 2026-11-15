# Android App

**Outpace for Android** runs the Outpace engine on a phone, tablet or Android TV box, with no
root and no Termux, and hands streams to the video player you already use, such as VLC, MX
Player, Just Player or Kodi. It can also act as a remote control for an Outpace engine running
on another device on your network. The app is developed in
[outpace-stream/outpace-android](https://github.com/outpace-stream/outpace-android).

**Status:** stable. Signed APKs are published on the
[app releases page](https://github.com/outpace-stream/outpace-android/releases), from v1.0.0.
App versions are independent of engine versions.

## What it does

- **Runs the engine for you.** It downloads the engine release you choose from
  [engine releases](https://github.com/outpace-stream/outpace-engine/releases), checks it against
  its `.sha256`, and keeps it running in the background from a foreground service. The APK
  itself contains no engine.
- **Uses an engine elsewhere.** Add a remote engine (a home server, NAS or TV box) by address,
  and the app plays, searches and shows what is playing there.
- **Plays from anywhere.** `acestream://` links, the share menu, a paste box, apps that call a
  local `/ace/getstream`, [stream search](Stream-Search), recently played and now playing.
- **Casts to TVs.** With engine v1.2.0 or newer, **Play on TV** sends a stream to a DLNA/UPnP
  renderer through the engine's [casting](Casting).
- **Exposes the engine settings that matter on a phone:** startup buffer and HLS presets,
  low-memory mode, LAN access, seeding and upload limits, inbound peers and port mapping, the
  [outbound proxy](Outbound-Proxy), [engine camouflage](Engine-Camouflage) and more. Each maps
  to an `OUTPACE_*` variable from [Configuration](Configuration).
- **Works with a remote.** Every screen is usable with a D-pad, and the layout switches to a
  side rail on TVs, tablets and landscape phones.
- **Updates itself** from the releases page, after you confirm, and manages up to three engine
  versions side by side.

## Requirements

- Android 5.0 or newer.
- 64-bit ARM (`arm64-v8a`) or 32-bit ARM (`armeabi-v7a`). Many TV boxes run 32-bit Android on a
  64-bit chip; the app picks the matching engine build. From engine v1.2.1, there is also an
  `x86_64` build, for emulators, ChromeOS and Intel/AMD TV boxes.
- A video player app. Outpace has no player of its own.
- For a local engine: engine v1.0.0 or newer for 64-bit ARM, v1.0.2 or newer for 32-bit ARM,
  v1.2.1 or newer for x86_64.
  Search, casting, the proxy and camouflage need the engine versions listed in the
  [Configuration](Configuration) reference.

## Install

1. Download `outpace-android-<version>.apk` and its `.sha256` from the
   [app releases page](https://github.com/outpace-stream/outpace-android/releases), and check the
   file.
2. Open it on the device and allow installing from that source when Android asks. On a
   television, follow [Android TV](Android-TV) to install it from a computer with `adb`.
3. Open the app, choose **Configure** on Home, then **Manage versions** and **Install** on an
   engine release. Press **Start engine** and play something.

The app is sideloaded only. It isn't on the Play Store, because Play policy doesn't allow apps
that download programs to run.

## More

- [App user guide](https://github.com/outpace-stream/outpace-android/blob/main/docs/Android-App.md):
  every screen and setting, battery use, logs and troubleshooting.
- [Android TV](Android-TV): sideloading onto a television step by step.
- [App releases](https://github.com/outpace-stream/outpace-android/releases) and
  [issues](https://github.com/outpace-stream/outpace-android/issues).

APK releases belong to the Android repository. Engine archives, including the Android targets
(`aarch64-linux-android`, `armv7-linux-androideabi` from v1.0.2, and `x86_64-linux-android`
from v1.2.1), remain in
[engine releases](https://github.com/outpace-stream/outpace-engine/releases).
[Termux](Installation#android-with-termux-best-effort) remains a way to run the engine binary
yourself.
