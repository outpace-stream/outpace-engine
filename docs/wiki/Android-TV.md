# Android TV

Sideload the [companion app](Android-App) onto an Android TV or Google TV set, let it download
an engine for the television, and play streams from the remote in an installed player.

| | |
| --- | --- |
| **Source** | Outpace maintainers |
| **Reported** | 2026-11-14 |
| **Versions** | Android app v0.1.0 at the time; the steps below are for app v1.0.0 and later. Engine downloaded by the app (v1.0.2 or later on 32-bit sets). |
| **Device** | TCL 55T69C, Google TV (Android 12) |
| **Player** | VLC for Android |
| **Works on** | Any Android TV or Google TV set or box that allows USB debugging |

The app is not on the Play Store, so a computer on the same network installs it with `adb`.
This is not the [Termux](Installation#android-with-termux-best-effort) install: the app manages
the engine for you.

## What you need

- the television and a computer on the same LAN;
- [Android platform-tools](https://developer.android.com/tools/releases/platform-tools) (`adb`)
  on the computer;
- a video player on the television, such as VLC. The app has no player of its own.

## 1. Download and check the APK

From the [Android app releases](https://github.com/outpace-stream/outpace-android/releases),
download `outpace-android-<version>.apk` and its `.sha256` file. The examples use v1.0.0; use
the version you downloaded.

```bash
sha256sum -c outpace-android-1.0.0.apk.sha256            # Linux
shasum -a 256 -c outpace-android-1.0.0.apk.sha256        # macOS
```

Windows PowerShell prints `True` when the hash matches (the comparison ignores case):

```powershell
$hash = (Get-FileHash .\outpace-android-1.0.0.apk -Algorithm SHA256).Hash
$expected = (Get-Content .\outpace-android-1.0.0.apk.sha256 -Raw).Split()[0]
$hash -eq $expected
```

## 2. Turn on debugging

On the television:

1. Open **Settings → System → About**.
2. Select **Android TV OS build** and press OK seven times, until the set says you are a developer.
3. Go back to **System → Developer options** and turn on **USB debugging**.

Then check whether Developer options lists **Wireless debugging**. Which path you take depends
on it:

| Developer options | How the computer connects |
| --- | --- |
| No **Wireless debugging** row (the TCL 55T69C) | `adb connect` on port **5555**; see [3a](#3a-connect-on-port-5555). |
| **Wireless debugging** listed | Pair first, then connect on the port the set shows; see [3b](#3b-pair-with-wireless-debugging). |

Find the television's address under **Settings → Network & Internet**, or in the router's client
list. The examples use `192.0.2.10`.

## 3a. Connect on port 5555

```bash
adb connect 192.0.2.10:5555
adb devices
```

`device` means the television accepted this computer. `unauthorized` means it is waiting on an
**Allow USB debugging?** dialog.

That dialog is not drawn while the panel is off: a connection made in standby queues a prompt
you cannot see. Turn the panel on, press **Home**, then **Back** until the dialog is in front,
and choose **Allow** (tick **Always allow from this computer** if you will install again).

If Back never shows it, open **Developer options → Revoke USB debugging authorizations**, return
to **Home**, and run `adb connect` once. Each extra `adb connect` stacks another hidden prompt,
so dismiss the stack with Back before trying again.

## 3b. Pair with Wireless debugging

Turn **Wireless debugging** on and open **Pair device with pairing code**. The screen shows a
pairing address, a pairing port and a code. The ports below are examples; the pairing port and
the connection port are different.

```bash
adb pair 192.0.2.10:37123          # enter the code when asked
adb connect 192.0.2.10:39555       # the connection port, not the pairing port
adb devices
```

The connection is ready when the line says `device`; this path shows no **Allow USB debugging?**
dialog. Don't use **Revoke USB debugging authorizations** here: it removes the pairing, and you
pair again instead.

## 4. Install the app and the engine

```bash
adb install -r outpace-android-1.0.0.apk
```

Open the app from the television's app row. The APK does not contain the engine, so Home shows
**Local engine not set up** with a **Configure** button:

1. **Configure** opens **Settings**. Under **Local engine**, choose **Manage versions**.
2. Choose **Install** on the newest release. The app downloads the engine for this device
   (64-bit or 32-bit), checks it and selects it.
3. Back in Settings, choose **Start engine**.

**Start on boot** is on by default on a television, so the engine comes back after the set
restarts. If an Outpace engine already runs elsewhere on your network, for example the
[home server](Home-Server-Behind-a-VPN) setup, choose **Add a remote engine** under
**Engine connection** instead of installing one.

## 5. Play

Play from **Home** or **Search** and pick the player in the chooser. **Open as HLS** suits
players that handle `.m3u8` better than MPEG-TS. **Recently played** on Home restarts a stream
from the remote. The other screens are in the
[app user guide](https://github.com/outpace-stream/outpace-android/blob/main/docs/Android-App.md).

## Afterwards

USB debugging leaves port 5555 open on the LAN, and anyone who can reach the television can try
to connect. Turn **USB debugging** (and **Wireless debugging**, if you used it) off once you are
done installing. Playback needs neither.

## Notes for other televisions

- Menu names vary by brand. Look for **Developer options** under **System** or
  **Device Preferences**, and for the build number under **About**.
- A set whose OS is not Android (LG webOS, Samsung Tizen) cannot run the app. Run the engine on
  another device on the LAN, such as a [router](OpenWrt-Router) or a
  [home server](Home-Server-Behind-a-VPN). Then [cast](Casting) to the television, or open the
  stream URL in its player.
- On a set with little RAM, turn on **Low-memory mode** in the app's engine settings. It applies
  the smaller buffers described in
  [Routers and Small Devices](Routers-and-Small-Devices#memory-settings).
- For quicker channel changes, set **Startup buffer** to Balanced or Fast. Shorter buffers stutter
  more on weak swarms.
