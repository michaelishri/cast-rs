# Cast

Cast plays local video files and sends a macOS display to Google Cast devices such as Chromecast and Google Nest Hub. It is a command-line app for macOS 13 or newer.

## Before you start

- Put the Mac and Google Cast device on the same trusted local network. Guest networks and client isolation commonly prevent discovery or streaming.
- Desktop system audio is opt-in with `cast desktop --audio`; microphone audio is never captured. Audio already present in a compatible local video file is played normally.

## Install

Homebrew is the recommended installation method:

```sh
brew install michaelishri/tap/cast
cast --version
```

Homebrew selects the correct bottle for supported Apple Silicon and Intel Macs and keeps Cast up to date with `brew upgrade`.

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
./cast --version
```

Releases are not yet Developer-ID signed or notarized. After verifying the checksum, remove quarantine if macOS blocks the downloaded app:

```sh
xattr -dr com.apple.quarantine .
```

The examples below assume a Homebrew installation. Use `./cast` instead of `cast` when running from an extracted archive.

## First cast

1. Find nearby receivers:

   ```sh
   cast devices
   ```

   Copy the address in the `ADDRESS` column for the device you want to use. The
   `CAPABILITY` column identifies audio-only receivers and receivers that can
   display video.

2. Start mirroring:

   ```sh
   cast desktop --host 192.168.1.50
   ```

3. On first use, allow Screen Recording for the terminal app in **System Settings → Privacy & Security → Screen Recording**, then run the command again.

4. Press `Ctrl-C` to stop casting.

Cast uses its low-latency mirroring transport by default. The first connection can take a few seconds while the receiver starts.

To include system and application audio, add `--audio`:

```sh
cast desktop --host 192.168.1.50 --audio
```

Desktop audio is AAC-LC stereo at 48 kHz. Cast excludes its own process audio to avoid feedback and does not capture the microphone. If the AAC encoder is unavailable, or a receiver rejects audio during startup, Cast warns and continues with video only; an audio failure after capture has started stops the session.

## Cast a local video

```sh
cast video --host 192.168.1.50 ~/Movies/example.mp4
```

Keep Cast running while the video plays: the receiver fetches the file directly from the Mac, so both devices must stay on the same reachable network. In an interactive terminal, Cast shows progress and supports these controls:

- Left/Right: seek backward/forward 10 seconds
- Down/Up: seek backward/forward 60 seconds
- Space: pause or resume
- Escape or `Ctrl-C`: stop playback and return the receiver to its home screen

Seeking while paused leaves playback paused. Interactive controls are disabled with `-v` or `-vv`, and when input or output is redirected, so diagnostic and scripted output remains line-oriented.

Start at a particular position, in seconds:

```sh
cast video --host 192.168.1.50 --start-at 90 ~/Movies/example.mp4
```

Cast serves conservative H.264/AAC MP4 and VP8/VP9 WebM files unchanged, remuxes compatible H.264/AAC streams in other containers, and converts other decodable inputs to at-most-1080p H.264/AAC. It starts playback as soon as the first prepared segment is ready and continues conversion in the background.

To reject media that requires conversion, normalize every file, or use the full-file fallback:

```sh
cast video --host 192.168.1.50 --transcode never movie.mp4
cast video --host 192.168.1.50 --transcode always movie.mkv
cast video --host 192.168.1.50 --transcode-delivery complete movie.mkv
```

DRM-protected and corrupt files cannot be converted. Embedded subtitles and alternate audio tracks are not included yet. To use a fixed local-server port for a firewall rule, pass `--http-port 8080`.

## Browse and play with the TUI

Open the full-screen local-video interface in the current directory, or pass a starting directory:

```sh
cast tui
cast tui ~/Movies
```

Cast discovers receivers without blocking the interface. To preselect a known receiver and skip the initial scan, use `cast tui --host 192.168.1.50`. The TUI accepts the same local-server and compatibility choices as `video`: `--cast-port`, `--http-port`, `--transcode auto|never|always`, and `--transcode-delivery incremental|complete`. It requires interactive terminal input and output and shows a safe reduced screen below 60×18.

The upper half is split between the file explorer and the session playlist. The full-width player below them shows preparation or playback progress, transport controls, volume/mute, status, and the active receiver. Tab cycles File Explorer → Playlist → Player → Receiver; Shift-Tab reverses the cycle. Press `?` for the in-app help and `l` for the scrollable warning/diagnostic log. `-v` and `-vv` increase the detail captured there without writing over the alternate screen.

Keyboard controls:

- Global: `q`/`Ctrl-C` quits, Space plays or pauses, Escape stops, `[`/`]` selects previous/next, `M` mutes, and `+`/`-` changes volume by 5%.
- File explorer: arrows, Page Up/Down, Home/End select; Enter opens a directory or enqueues a file; Backspace opens the parent; `p` plays now; `f` toggles all regular files. Hidden entries remain hidden.
- Playlist: arrows, Page Up/Down, Home/End select; Enter plays; Delete or Backspace removes; Alt-Up/Down reorders.
- Player: Left/Right seeks backward/forward 10 seconds, Shift-Left/Shift-Right seeks backward/forward 60 seconds, and Down/Up changes volume by 5%. The larger seek controls appear in the player only while Shift is held.
- Receiver: Enter opens the picker and `r` rescans. Escape closes any overlay.

The mouse can focus panes, select rows, double-click to activate, scroll the pane under the pointer, choose a receiver, and set playback position or volume by clicking their gauges. The playlist is session-only: duplicates are allowed, there is no repeat or shuffle, natural completion advances linearly, and stopping retains the current item. Media-specific failures skip to the next item; receiver/network failures pause the queue. Switching receivers reuses the prepared source, resumes near the current position, preserves paused/playing intent, and reads the new receiver's own volume.

## Multiple receivers and extended displays

Repeat `--host` to cast one captured desktop to a receiver group:

```sh
cast desktop --host 192.168.1.50 --host 192.168.1.51
```

Use `--extend` when each receiver should display an independent desktop. Cast creates a temporary extended display per receiver, without installing a display driver or system extension:

```sh
cast desktop --host 192.168.1.50 --extend
```

Move windows onto the new display while Cast is running. With `--audio`, each receiver gets the system/application audio selected by its corresponding display capture filter. `--extend` is experimental because it uses Apple’s private `CGVirtualDisplay` API, which may stop working in a future macOS version. It cannot be combined with `--display`.

## Everyday commands

Choose a display and stop after 30 seconds:

```sh
cast displays
cast desktop --host 192.168.1.50 --display 1 --seconds 30
```

Tune desktop casting for your network:

```sh
# Lower latency on a reliable network
cast desktop --host 192.168.1.50 --target-delay-ms 150

# More resilient playback
cast desktop --host 192.168.1.50 --target-delay-ms 400

# Lower network load on busy Wi-Fi
cast desktop --host 192.168.1.50 --bitrate 3000000

# Compatibility fallback (normally several seconds behind live video)
cast desktop --host 192.168.1.50 --transport hls

# Compatibility fallback with desktop audio
cast desktop --host 192.168.1.50 --transport hls --audio
```

## Find good latency settings

Run a one-minute profile while using the desktop as you normally would:

```sh
cast profile --host 192.168.1.50
```

The final report recommends a `desktop` command. For a repeatable automated comparison, use:

```sh
cast profile --host 192.168.1.50 --synthetic --auto-tune
```

This takes 60 seconds across six short trials and prints a recommended command. It tunes sender latency and network reliability; it does not measure camera-observed glass-to-glass latency or image quality.

## Troubleshooting

**No devices found** — Confirm both devices are on the same LAN, disable guest/client isolation, and try the receiver's IP address directly if known.

**macOS asks for permission or the display is blank** — Grant Screen Recording permission to the terminal app, then quit and reopen it if macOS does not apply the change immediately.

**Playback stutters** — Try `--target-delay-ms 400`, then reduce the bitrate to `3000000`. Re-run `profile` after changing the network, receiver, resolution, or frame rate.

**The receiver rejects the stream** — Try the compatibility fallback:

```sh
cast desktop --host 192.168.1.50 --transport hls
```

**A local video does not start** — Allow incoming connections through the macOS firewall and confirm that guest/client isolation is disabled. Use `--transcode always` to normalize a file initially selected for direct playback.

**The TUI cannot start or looks damaged** — Run it directly in a terminal rather than through a pipe or redirected file. Resize to at least 60×18. Cast restores raw mode, the cursor, mouse tracking, and the primary screen on exit; if the terminal itself was force-killed, run `reset`.

**An extended display is unavailable or blank** — Your macOS version may not support the experimental `CGVirtualDisplay` API. Re-check Screen Recording permission and cast an existing display instead.

**Need more detail** — Add `-v` before a command for diagnostics, or `-vv` for protocol-level tracing:

```sh
cast -v desktop --host 192.168.1.50
```

Run `cast --help` or `cast <command> --help` for every available option.

## Contributing

Development setup, architecture notes, tests, and release instructions are in [CONTRIBUTING.md](CONTRIBUTING.md).
