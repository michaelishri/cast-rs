# Caster user guide

Caster sends a macOS display to a Google Cast device such as Chromecast or Google Nest Hub. It is a command-line app for macOS 13 or newer.

## Before you start

- Put the Mac and Google Cast device on the same trusted local network. Guest networks and client isolation commonly prevent discovery or streaming.
- Download the archive that matches the Mac:
  - `macos-arm64` for Apple Silicon (M1, M2, M3, M4, and later);
  - `macos-x86_64` for Intel Macs.
- Caster streams video only. Desktop audio is not included.

## Install

In Terminal, extract the downloaded archive and enter its folder:

```sh
tar -xzf caster-<version>-macos-arm64.tar.gz
cd caster-<version>-macos-arm64
```

Verify the download before running it when a `.sha256` file is provided:

```sh
shasum -a 256 -c caster-<version>-macos-arm64.tar.gz.sha256
```

Check that the app runs:

```sh
./caster --version
```

Releases are not yet Developer-ID signed or notarized. If macOS blocks the downloaded app after you have verified its checksum, remove the archive's quarantine attribute:

```sh
xattr -dr com.apple.quarantine .
```

## First cast

1. Ask Caster to find nearby receivers:

   ```sh
   ./caster devices
   ```

   Copy the address in the `ADDRESS` column for the device you want to use.

2. Start mirroring with that address:

   ```sh
   ./caster cast-desktop --host 192.168.1.50
   ```

3. On first use, macOS asks for Screen Recording permission for the terminal app running Caster. Allow it in **System Settings → Privacy & Security → Screen Recording**, then run the command again.

4. Press `Ctrl-C` in Terminal to stop casting.

Caster uses its low-latency mirroring transport by default. The first connection can take a few seconds while the receiver starts.

## Everyday commands

Cast a particular display for 30 seconds:

```sh
./caster displays
./caster cast-desktop --host 192.168.1.50 --display 1 --seconds 30
```

Use a smaller receiver buffer when the network is reliable and you want lower latency:

```sh
./caster cast-desktop --host 192.168.1.50 --target-delay-ms 150
```

Use a larger receiver buffer when playback stutters:

```sh
./caster cast-desktop --host 192.168.1.50 --target-delay-ms 400
```

Reduce the network load on busy Wi-Fi:

```sh
./caster cast-desktop --host 192.168.1.50 --bitrate 3000000
```

Use the HLS compatibility transport if the default mirroring transport is rejected by a receiver. HLS is normally several seconds behind live video:

```sh
./caster cast-desktop --host 192.168.1.50 --transport hls
```

## Find good latency settings

Run a normal one-minute profile while using the desktop as you expect to use it:

```sh
./caster profile --host 192.168.1.50
```

The final report recommends a `cast-desktop` command. Copy that command as the starting point for everyday use.

For a repeatable automated comparison of latency controls, use the synthetic workload:

```sh
./caster profile --host 192.168.1.50 --synthetic --auto-tune
```

This takes 60 seconds of measurements across six short trials, then prints a recommended command. It tunes sender latency and network reliability; it does not measure camera-observed glass-to-glass latency or image quality.

## Troubleshooting

**No devices found**

Confirm that both devices are on the same LAN, disable guest/client isolation, and try the receiver's IP address directly if you know it.

**macOS asks for permission or the display is blank**

Grant Screen Recording permission to the terminal app. Quit and reopen Terminal after changing the permission if macOS does not apply it immediately.

**The receiver connects but playback stutters**

First try `--target-delay-ms 400`. If that helps, reduce the bitrate to `3000000`. Re-run `profile` after changing the network, receiver, resolution, or frame rate.

**The receiver rejects the stream**

Try the compatibility fallback:

```sh
./caster cast-desktop --host 192.168.1.50 --transport hls
```

**Need more detail**

Add `-v` before the command for diagnostics, or `-vv` for protocol-level tracing:

```sh
./caster -v cast-desktop --host 192.168.1.50
```

Run `./caster --help` or `./caster <command> --help` for every available option.
