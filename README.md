# Cast

Cast plays local video files and sends a macOS or Linux desktop to Google Cast devices such as Chromecast and Google Nest Hub. It supports macOS 13 or newer, X11 desktops with RandR, and GNOME/KDE Wayland desktops on glibc 2.35 or newer.

## Before you start

- Put the computer and Google Cast device on the same trusted local network. Guest networks and client isolation commonly prevent discovery or streaming.
- Desktop system audio is opt-in with `cast desktop --audio`; microphone audio is never captured. Audio already present in a compatible local video file is played normally.

## Install

Homebrew is the recommended installation method:

```sh
brew install michaelishri/tap/cast
cast --version
```

Homebrew selects the correct bottle for supported Apple Silicon and Intel Macs and keeps Cast up to date with `brew upgrade`.

On Linux, download `cast-linux-x86_64.tar.gz` or `cast-linux-aarch64.tar.gz` from GitHub Releases, verify its adjacent `.sha256` file, and extract it. The archive contains Cast and its redistributable media libraries; it deliberately leaves glibc, GPU drivers, PipeWire, WirePlumber, the XDG desktop portal, and Cisco OpenH264 to the system.

Ubuntu 22.04 is the binary compatibility baseline. Install the desktop services used by your GNOME or KDE session (package names vary by distribution); `wpctl` from WirePlumber is needed only to suppress and restore local playback during low-latency audio mirroring. Then install or check the separately distributed software encoder:

```sh
sha256sum -c cast-linux-x86_64.tar.gz.sha256
tar -xzf cast-linux-x86_64.tar.gz
cd cast-linux-x86_64
./cast setup
./cast setup --check
./cast --version
```

`cast setup` asks before downloading, verifies a pinned checksum, and installs Cisco's OpenH264 2.3 module in the current user's XDG data directory. The module is not bundled in Cast's archive. Use `--yes` for an explicitly approved non-interactive install; `--check` never changes files.

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

The examples below assume `cast` is on `PATH`. Use `./cast` when running from an extracted archive.

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

3. On macOS, allow Screen Recording for the terminal app in **System Settings → Privacy & Security → Screen Recording**, then run the command again. On Linux, Cast uses native X11 capture when `XDG_SESSION_TYPE=x11`; other sessions use the privacy-preserving desktop portal. Cast stores only the portal's opaque restore token and retries the chooser if that token expires.

4. Press `Ctrl-C` to stop casting.

Cast uses its low-latency mirroring transport by default. The first connection can take a few seconds while the receiver starts.

To include system and application audio, add `--audio`:

```sh
cast desktop --host 192.168.1.50 --audio
```

Desktop audio is AAC-LC stereo at 48 kHz. Cast does not capture the microphone. On Linux it captures the default PipeWire sink monitor, then low-latency mirror mode mutes physical playback without muting that monitor. Local volume/mute changes are sent to every receiver, and the original output state is restored after normal exit, Ctrl-C, failure, or panic. HLS captures audio without changing local output. If capture or AAC startup is unavailable, Cast warns and continues video-only; an audio failure after capture starts ends the session cleanly.

### Linux capture and encoder selection

List the active X11 monitors or check portal capabilities before casting:

```sh
cast displays
cast displays --backend x11
cast displays --backend portal --select-source
```

`--backend auto` is the default: Cast selects X11 when `XDG_SESSION_TYPE=x11` or `DISPLAY` is present and reachable, and otherwise uses the portal. Use `--backend x11 --display eDP-1` to select a named RandR monitor, or `--backend portal` to override detection. X11 commands must run as the logged-in desktop user with valid `DISPLAY` and Xauthority access; SSH sessions normally need those values forwarded from the desktop session.

`--select-source` forces the portal chooser; otherwise Cast asks the portal to restore the previous choice and falls back to a new prompt when needed. On X11, `--extend` temporarily activates an unused disconnected RandR output and free CRTC for each receiver, places it to the right of the current desktop, and restores the layout on exit. On portal-backed sessions it requests one virtual source per receiver when the compositor advertises support. Wayland capture continues to use the existing portal path.

Linux chooses H.264 in this order: NVIDIA NVENC, VA-API, then OpenH264. Override it with `--encoder nvenc`, `--encoder vaapi`, or `--encoder openh264` on `video`, `capture`, `profile`, and `desktop`. An explicit unavailable encoder fails with an actionable diagnostic instead of silently selecting another. X11 uses RandR monitor geometry, MIT-SHM frame transfer with a core GetImage fallback, and XFixes cursor images; the portal path continues to use PipeWire. GPU encoding still requires a working vendor driver and device permissions.

## Cast a local video

```sh
cast video --host 192.168.1.50 ~/Movies/example.mp4
```

Keep Cast running while the video plays: the receiver fetches the file directly from the Mac, so both devices must stay on the same reachable network. In an interactive terminal, Cast shows progress and supports these controls:

- Left/Right: seek backward 10 seconds/forward 30 seconds
- Shift-Left/Shift-Right: seek backward/forward 60 seconds
- Space: pause or resume
- `M`: mute or unmute the receiver
- `-`/`+`: lower or raise receiver volume by 5%
- Escape: stop playback and return the receiver to its home screen
- `q` or `Ctrl-C`: quit the player

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

The upper half is split between the file explorer and the session playlist. The full-width player below them shows preparation or playback progress, transport controls, volume/mute, status, and the active receiver. Tab and Shift-Tab cycle focus between File Explorer and Playlist. Press `?` for the in-app help and `l` for the scrollable warning/diagnostic log. `-v` and `-vv` increase the detail captured there without writing over the alternate screen.

Keyboard controls:

- Global: `q`/`Ctrl-C` quits, Space plays or pauses, Escape stops, `[`/`]` selects previous/next, `M` mutes, and `+`/`-` changes volume by 5%.
- File explorer: arrows, Page Up/Down, Home/End select; Enter opens a directory or enqueues a file; Backspace opens the parent; `p` plays now; `f` toggles all regular files. Hidden entries remain hidden.
- Playlist: arrows, Page Up/Down, Home/End select; Enter plays; Delete or Backspace removes; Alt-Up/Down reorders.
- Player: Left seeks backward 10 seconds, Right seeks forward 30 seconds, and Shift-Left/Shift-Right seeks backward/forward 60 seconds. The larger seek controls appear in the player only while Shift is held.
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

Move windows onto the new display while Cast is running. With `--audio`, every receiver gets the same encoded system mix. On macOS, `--extend` is experimental because it uses Apple’s private `CGVirtualDisplay` API, which may stop working in a future macOS version. On Linux X11, it requires an unused disconnected RandR output, a free CRTC, and enough framebuffer space; driver stacks that reject modes on disconnected outputs fail before receiver startup. On portal-backed Linux sessions it requests portal virtual sources and fails when the compositor does not advertise that capability. It cannot be combined with `--display`.

## Everyday commands

Choose a macOS display and stop after 30 seconds:

```sh
cast displays
cast desktop --host 192.168.1.50 --display 1 --seconds 30
```

On Linux X11, select a monitor by its RandR name; use the portal chooser on portal-backed sessions:

```sh
cast displays --backend x11
cast desktop --host 192.168.1.50 --backend x11 --display eDP-1 --seconds 30
cast desktop --host 192.168.1.50 --backend portal --select-source --seconds 30
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

**Linux X11 cannot connect or lists no monitors** — Run Cast as the logged-in desktop user and verify `XDG_SESSION_TYPE=x11`, `DISPLAY`, and `XAUTHORITY`. From SSH, copy the values from the desktop session rather than guessing them. Run `cast -v displays --backend x11`; an unreachable display or missing RandR support is reported directly and never silently falls back to the portal.

**Linux X11 extended display is unavailable** — `--extend` needs one disconnected RandR connector and one free CRTC per receiver. Some GPU drivers refuse to activate disconnected connectors; Cast reports that failure and leaves the existing layout intact. Cast does not install a virtual DRM driver or modify Xorg configuration. It stops capture before removing temporary modes and recomputes the framebuffer from the outputs that remain active so unrelated hotplug changes are preserved.

**Linux portal is missing or the chooser does not open** — Install the portal backend matching the GNOME or KDE session plus PipeWire, then run `cast -v displays --backend portal`. Use `--backend x11` only in an X11 session.

**Linux has no usable H.264 encoder** — Run `cast setup --check`, then `cast setup` for the OpenH264 fallback. NVENC needs the proprietary NVIDIA driver; VA-API needs a working render device and driver. V4L2 M2M encoders are unsupported.

**Linux audio plays locally or volume forwarding is unavailable** — Ensure WirePlumber and `wpctl` are installed. Cast continues streaming audio if local-output redirection cannot start, and reports that limitation. HLS intentionally leaves local playback unchanged.

**A remembered Linux source no longer exists** — Run with `--select-source`. Cast normally detects an expired portal restore token, forgets it, and opens the chooser automatically.

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
