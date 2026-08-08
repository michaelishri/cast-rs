# Cast user guide

Cast plays local video files and sends a macOS display to a Google Cast device such as Chromecast
or Google Nest Hub. It is a command-line app for macOS 13 or newer.

## Before you start

- Put the Mac and Google Cast device on the same trusted local network. Guest networks and client isolation commonly prevent discovery or streaming.
- Desktop casting is video-only. Audio already present in a compatible local video file is played.

## Install

Homebrew is the recommended installation method:

```sh
brew install michaelishri/tap/cast
cast --version
```

Homebrew selects the correct bottle for supported Apple Silicon and Intel Macs and keeps Cast up
to date with `brew upgrade`.

### Install a release archive manually

Alternatively, download the archive that matches the Mac from the GitHub Releases page:

- `macos-arm64` for Apple Silicon (M1, M2, M3, M4, and later);
- `macos-x86_64` for Intel Macs.

In Terminal, extract the downloaded archive and enter its folder:

```sh
tar -xzf cast-<version>-macos-arm64.tar.gz
cd cast-<version>-macos-arm64
```

Verify the download before running it when a `.sha256` file is provided:

```sh
shasum -a 256 -c cast-<version>-macos-arm64.tar.gz.sha256
```

Check that the app runs:

```sh
./cast --version
```

Releases are not yet Developer-ID signed or notarized. If macOS blocks the downloaded app after you have verified its checksum, remove the archive's quarantine attribute:

```sh
xattr -dr com.apple.quarantine .
```

The remaining examples assume a Homebrew installation. When running from an extracted release
archive, replace `cast` with `./cast`.

## First cast

1. Ask Cast to find nearby receivers:

   ```sh
   cast devices
   ```

   Copy the address in the `ADDRESS` column for the device you want to use.

2. Start mirroring with that address:

   ```sh
   cast desktop --host 192.168.1.50
   ```

3. On first use, macOS asks for Screen Recording permission for the terminal app running Cast. Allow it in **System Settings → Privacy & Security → Screen Recording**, then run the command again.

4. Press `Ctrl-C` in Terminal to stop casting.

Cast uses its low-latency mirroring transport by default. The first connection can take a few seconds while the receiver starts.

## Use the receiver as a second display

Create an independent desktop space instead of casting an existing display:

```sh
cast desktop --host 192.168.1.50 --extend
```

Cast creates a temporary display, places it to the right of the existing desktop, and sends that
display to the receiver. Move a window past the right edge of the Mac's desktop to put it on the
receiver. The display defaults to 1280x720 at 30 fps; choose another mode with `--width`, `--height`,
and `--fps`:

```sh
cast desktop \
  --host 192.168.1.50 \
  --extend \
  --width 1920 \
  --height 1080 \
  --fps 30
```

This mode does not install a display driver or system extension. A helper process owns the virtual
display only while the cast is active, and Cast removes it during normal or error shutdown. The mode
uses a private macOS API, however, so it is experimental and could stop working after a macOS
update. Screen Recording permission is still required. `--extend` cannot be combined with
`--display`.

The temporary display works with the HLS fallback as well:

```sh
cast desktop --host 192.168.1.50 --extend --transport hls
```

Profile this exact path with `cast profile --host 192.168.1.50 --extend`. Synthetic profiling and
`--auto-tune` do not use a display and therefore cannot be combined with `--extend`.

## Cast a local video

Play a local video by passing its path and the receiver address:

```sh
cast video \
  --host 192.168.1.50 \
  ~/Movies/example.mp4
```

Keep Cast running while the video plays. The Chromecast fetches the file directly from the Mac,
so both devices must remain on the same reachable network. In a terminal, Cast shows playback
progress and accepts these controls without requiring Enter:

- Left/Right: seek backward/forward 10 seconds;
- Down/Up: seek backward/forward 60 seconds;
- Space: pause or resume;
- Escape or `Ctrl-C`: stop playback, close the Cast session, and return the receiver to its home
  screen.

Seeking while paused leaves the video paused. Interactive controls are disabled with `-v` or `-vv`
and when input or output is redirected, so diagnostic and scripted output remains line-oriented.

Begin at a particular position, in seconds:

```sh
cast video \
  --host 192.168.1.50 \
  --start-at 90 \
  ~/Movies/example.mp4
```

Cast first inspects the container, video, and audio. Conservative H.264/AAC MP4 and VP8/VP9 WebM
files are served unchanged. H.264/AAC in another container is remuxed without quality loss. Other
decodable codecs are converted to an at-most-1080p H.264/AAC MP4 using the Mac's VideoToolbox
encoder. Compatible tracks are copied when only video or audio needs conversion, so an H.264 movie
with E-AC-3 audio keeps its original video and converts only the audio to AAC. Cast normally starts
after the first fragmented-MP4 segment is ready and keeps converting in the background while the
receiver plays. For `--start-at`, it first prepares enough segments to cover the requested position.
Progress is printed in the terminal and `Ctrl-C` cancels preparation.
Background conversion stays at most about two minutes ahead of the receiver. It pauses when that
lookahead is full—including while receiver playback is paused—and resumes as playback advances or
seeks forward. Already prepared segments remain available for backward seeking until Cast exits,
when the temporary media directory is removed.

To reject media that would require preparation, or to normalize every input, use:

```sh
./cast video --host 192.168.1.50 --transcode never movie.mp4
./cast video --host 192.168.1.50 --transcode always movie.mkv
```

If a receiver rejects incremental fragmented-MP4 HLS, use the full-file compatibility path:

```sh
./cast video --host 192.168.1.50 --transcode-delivery complete movie.mkv
```

DRM-protected and corrupt files cannot be converted. Only the best video stream and best audio
stream are selected; embedded subtitles and alternate audio tracks are not included yet.

The local server normally selects an available port automatically. If a firewall rule requires a
fixed port, set one explicitly:

```sh
cast video \
  --host 192.168.1.50 \
  --http-port 8080 \
  ~/Movies/example.mp4
```

## Everyday commands

Cast a particular display for 30 seconds:

```sh
cast displays
cast desktop --host 192.168.1.50 --display 1 --seconds 30
```

Use a smaller receiver buffer when the network is reliable and you want lower latency:

```sh
cast desktop --host 192.168.1.50 --target-delay-ms 150
```

Use a larger receiver buffer when playback stutters:

```sh
cast desktop --host 192.168.1.50 --target-delay-ms 400
```

Reduce the network load on busy Wi-Fi:

```sh
cast desktop --host 192.168.1.50 --bitrate 3000000
```

Use the HLS compatibility transport if the default mirroring transport is rejected by a receiver. HLS is normally several seconds behind live video:

```sh
cast desktop --host 192.168.1.50 --transport hls
```

## Find good latency settings

Run a normal one-minute profile while using the desktop as you expect to use it:

```sh
cast profile --host 192.168.1.50
```

The final report recommends a `desktop` command. Copy that command as the starting point for everyday use.

For a repeatable automated comparison of latency controls, use the synthetic workload:

```sh
cast profile --host 192.168.1.50 --synthetic --auto-tune
```

This takes 60 seconds of measurements across six short trials, then prints a recommended command. It tunes sender latency and network reliability; it does not measure camera-observed glass-to-glass latency or image quality.

## Troubleshooting

**No devices found**

Confirm that both devices are on the same LAN, disable guest/client isolation, and try the receiver's IP address directly if you know it.

**macOS asks for permission or the display is blank**

Grant Screen Recording permission to the terminal app. Quit and reopen Terminal after changing the permission if macOS does not apply it immediately.

**The temporary second display cannot be created**

The `--extend` switch relies on a private macOS API that is detected at runtime. If Cast reports
that `CGVirtualDisplay` is unavailable, use an existing display without `--extend`. If creation
succeeds but capture does not, re-check Screen Recording permission and restart the terminal after
granting it.

**The receiver connects but playback stutters**

First try `--target-delay-ms 400`. If that helps, reduce the bitrate to `3000000`. Re-run `profile` after changing the network, receiver, resolution, or frame rate.

**The receiver rejects the stream**

Try the compatibility fallback:

```sh
cast desktop --host 192.168.1.50 --transport hls
```

**A local video does not start**

If Cast says the receiver never requested the video, allow incoming connections through the
macOS firewall and confirm that guest/client isolation is disabled. If the receiver requested the
file but rejected or could not decode a prepared H.264/AAC MP4, run with `-v` and report the receiver
model and error. Use `--transcode always` to normalize a file that was initially selected for direct
playback.

**Seeking in a local video fails**

Run `cast -v video ...` and look for `206 Partial Content` range responses. A fixed
firewall port can be chosen with `--http-port`, but no port forwarding is needed on a normal home
LAN.

**Need more detail**

Add `-v` before the command for diagnostics, or `-vv` for protocol-level tracing:

```sh
cast -v desktop --host 192.168.1.50
```

Run `cast --help` or `cast <command> --help` for every available option.
