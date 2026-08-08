# cast

An early Rust macOS CLI for casting local video and a desktop to Google Cast devices.

End users should start with the bundled [Cast user guide](docs/USER_GUIDE.md). This README also documents the implementation and release process for contributors.

The CLI currently supports these Cast paths:

- discover Cast devices with mDNS;
- control Google Cast receivers with `rust-cast`;
- inspect, remux, or transcode local media through linked FFmpeg libraries;
- capture a macOS display with ScreenCaptureKit and hardware-encode it with VideoToolbox;
- cast one captured desktop to multiple receivers, or create one temporary extended display per receiver without installing a display driver;
- send encrypted H.264 over Cast Streaming RTP with receiver feedback and retransmission;
- keep capture non-blocking with a latest-frame-wins encoder queue, adaptive bitrate, and bounded packet pacing;
- retain fragmented-MP4 HLS as a compatibility fallback.

## Install

Install the current stable release with Homebrew:

```sh
brew install michaelishri/tap/cast
cast --version
```

Architecture-specific archives remain available from the GitHub Releases page. See the
[Cast user guide](docs/USER_GUIDE.md) for manual installation and first-use instructions.

## Requirements

- macOS 13 or newer;
- Mac and Chromecast on the same network;
- Screen Recording permission for your terminal when using capture commands.

The optional `--extend` mode uses Apple's private `CGVirtualDisplay` API. It is experimental and
may stop working on a future macOS release, but it does not install a driver or system extension.

Building from source additionally requires Rust 1.85 or newer, Xcode 15 or newer, `nasm`, and
`pkg-config`. The native capture bindings use Apple's Swift toolchain, while the compatibility
pipeline links pinned FFmpeg libraries built by the helper below.

## Build

```sh
brew install nasm pkg-config
./scripts/build-ffmpeg-libraries.sh "$PWD/.build/ffmpeg-dist"
PKG_CONFIG_PATH="$PWD/.build/ffmpeg-dist/lib/pkgconfig" \
  cargo build --release
```

The helper builds pinned FFmpeg 8.0.1 shared libraries without its command-line programs. Release
archives include these libraries, so end users do not need FFmpeg or Homebrew installed.

## Releases

`Cargo.toml` is the single source of truth for the CLI version; Clap exposes that same version through `cast --version`. Releases use matching annotated Git tags: package version `0.6.0` is released as `v0.6.0`.

Follow the [release checklist](docs/RELEASE_CHECKLIST.md) for the complete, handoff-ready process:
version bump and validation, annotated tagging, GitHub release verification, Homebrew formula update,
bottle publication, installation testing, and failure recovery.

After the version pull request is merged, check out that exact `origin/main` commit in a clean
worktree and run:

```sh
./scripts/release.sh
```

The helper requires a clean worktree, runs formatting, Clippy, and tests, then pushes the matching tag. GitHub Actions verifies that the tag and package version agree, builds separate Apple Silicon and Intel macOS archives, adds SHA-256 checksum files, and publishes them to the corresponding GitHub Release.

The generated binaries are not Developer-ID signed or notarized. If macOS quarantines a downloaded archive, inspect the checksum first and then remove quarantine explicitly with `xattr -dr com.apple.quarantine <extracted-directory>`.

## Commands

```sh
# Discover Chromecasts for three seconds
cargo run -- devices

# Find macOS display IDs
cargo run -- displays

# Cast the first display over the default low-latency mirroring transport
cargo run --release -- desktop --host 192.168.1.50

# Mirror one shared desktop source to two receivers
cargo run --release -- desktop \
  --host 192.168.1.50 \
  --host 192.168.1.51

# Create a temporary second display and cast that independent desktop space
cargo run --release -- desktop --host 192.168.1.50 --extend

# Create a separate temporary extended display for each receiver, in --host order
cargo run --release -- desktop \
  --host 192.168.1.50 \
  --host 192.168.1.51 \
  --extend

# Profile the real capture/network path for one minute and recommend a delay
cargo run --release -- profile --host 192.168.1.50

# Profile two receivers and emit one worst-common recommendation
cargo run --release -- profile \
  --host 192.168.1.50 \
  --host 192.168.1.51

# Run the repeatable synthetic stress workload instead of capturing the desktop
cargo run --release -- profile --host 192.168.1.50 --synthetic

# Compare the latency controls and print a measured winning cast command
cargo run --release -- profile --host 192.168.1.50 --synthetic --auto-tune

# Test whether the receiver actually negotiates 60 fps
cargo run --release -- profile --host 192.168.1.50 --synthetic --fps 60

# Reduce or increase the requested receiver playout buffer (default: 200 ms)
cargo run --release -- desktop \
  --host 192.168.1.50 \
  --target-delay-ms 150

# Use the fragmented-MP4 HLS compatibility path instead
cargo run --release -- desktop \
  --host 192.168.1.50 \
  --transport hls

# Include Cast, transport, and skipped-sample diagnostics
cargo run -- -v desktop --host 192.168.1.50

# Include trace-level Cast and encoder diagnostics
cargo run -- -vv desktop --host 192.168.1.50

# Cast a particular display for 30 seconds
cargo run -- desktop \
  --host 192.168.1.50 \
  --display 1 \
  --seconds 30

# Override the default 1280x720 compatibility output for a more capable receiver
cargo run -- desktop \
  --host 192.168.1.50 \
  --width 1920 \
  --height 1080

# Prove capture + hardware H.264 encoding for ten seconds
cargo run -- capture --display 1 --seconds 10 --output capture.avcc

# Play a local video, automatically converting it when necessary
cargo run --release -- video \
  --host 192.168.1.50 \
  ~/Movies/example.mp4

# Begin local video playback at 90 seconds
cargo run --release -- video \
  --host 192.168.1.50 \
  --start-at 90 \
  ~/Movies/example.mp4

# Play an existing HLS live stream through the Cast Default Media Receiver
cargo run -- url \
  --host 192.168.1.50 \
  --url http://192.168.1.20:8080/master.m3u8 \
  --hls-video-segment-format fmp4 \
  --monitor-seconds 30
```

Run `cargo run -- --help` or add `--help` after a subcommand for every option.

## How local video casting works

`video` opens one local file, selects the Mac's receiver-facing LAN address, and exposes only
that already-open file at a fresh random URL. It launches Google's Default Media Receiver, loads the
URL as buffered media, and keeps the local server alive until playback finishes or `Ctrl-C` is
pressed. On `Ctrl-C`, it stops the media, closes the Cast channels, and terminates the Default Media
Receiver application. The command does not report success merely because Cast accepted the load
request: it waits until the receiver reports `PLAYING`.

In a terminal at the default verbosity, `video` displays playback progress and accepts player
controls without requiring Enter. Press Left/Right to seek backward/forward 10 seconds, Down/Up to
seek backward/forward 60 seconds, Space to pause or resume, and Escape to stop cleanly. Seeking while
paused leaves the video paused. Verbose output and redirected input or output retain line-oriented
status messages and do not enable keyboard controls.

The local server implements full, open-ended, and suffix HTTP byte ranges, so the receiver can seek
and can read MP4 metadata stored near the end of a large file. File data is read at explicit offsets
in fixed-size chunks rather than loaded into memory. The listener binds only to the LAN interface
used to reach the selected receiver and uses an automatic free port unless `--http-port` is set.

Cast inspects the selected container and its best video and audio streams through linked FFmpeg
libraries. Conservative H.264/AAC MP4 and VP8/VP9 WebM inputs are served directly. Compatible
H.264/AAC streams in another container are remuxed losslessly. Other decodable inputs are converted
to at-most-1080p H.264 Main/AAC stereo using VideoToolbox for video encoding. If only one selected
track is incompatible, Cast converts that track and copies the other without quality loss; for
example, H.264/E-AC-3 becomes copied H.264 with AAC audio. By default Cast publishes fragmented-MP4
HLS segments atomically and starts the receiver as soon as the first segment—or the segment covering
`--start-at`—is ready. Conversion continues in the background while the video plays and the playlist
becomes a finished VOD playlist at end of input. Background preparation is paced to stay within two
minutes of the receiver's current position, pauses when the lookahead is full, and wakes when playback
advances or seeks forward. Published segments remain available for backward seeking until the
temporary directory is removed at shutdown.

Use `--transcode never` to reject anything outside the direct-play set, or `--transcode always` to
normalize even directly playable input. `--content-type` remains an expert direct-play override and
bypasses automatic preparation unless `--transcode always` is also supplied. Use
`--transcode-delivery complete` to retain the original full-file preparation path if a receiver has
trouble with incremental fMP4 HLS. DRM-protected or corrupt inputs cannot be converted.

## How live casting works

By default, `desktop` launches each receiver's built-in Chrome Mirroring application and performs a Cast Streaming `OFFER`/`ANSWER` exchange. It converts VideoToolbox's AVCC output to Annex B, prepends SPS/PPS to keyframes, encrypts each frame with session-specific AES-128-CTR material, and sends it as Cast RTP over UDP. RTCP sender reports establish the media clock; receiver checkpoints and loss fields drive history cleanup and packet retransmission. The default offer requests a 200 ms receiver playout delay. Actual glass-to-glass latency also includes capture, encode, Wi-Fi, decode, and display time.

Repeat `--host` to cast to a receiver group. Without `--extend`, Cast captures and encodes the selected display once, then fans each encoded frame out through an independent encrypted RTP session. The slowest negotiated receiver frame rate caps the shared pipeline, and congestion reported by any receiver lowers the common adaptive bitrate. Each receiver has a bounded sender queue, so a stalled or failed target stops the complete group instead of allowing the outputs to drift apart. Startup and teardown are likewise all-or-nothing.

Add `--extend` to `desktop` when each receiver should be an independent desktop rather than a copy
of an existing screen. Cast creates one non-HiDPI virtual display per receiver, with modes matching
the effective `--width`, `--height`, and requested `--fps`, and gives each display its own capture
and encoder pipeline. Displays are placed to the right of the existing desktop in repeated
`--host` order: the first host receives extended display 1, the second receives display 2, and so
on. It works with both the default mirroring transport and `--transport hls`. Move windows onto the
new displays while Cast is running. Helper processes own the displays and release them on normal
shutdown, errors, or loss of the parent process; no display driver or system extension is installed.
`--extend` and `--display` are mutually exclusive.

The same switch is available on `profile`, so `cast profile --host 192.168.1.50 --extend` measures
the temporary-display path and prints a recommendation that retains `--extend`. It cannot be mixed
with the synthetic profiler or `--auto-tune` because those modes intentionally bypass display
capture.

`--transport hls` uses the earlier compatibility path. It determines the Mac's LAN address for each receiver and starts one HTTP listener per distinct local interface, with a private random route for every target. Receivers on the same interface share a listener. Without `--extend`, all routes serve one shared fragmented-MP4 capture; with `--extend`, each route serves its receiver's independent capture. Owned Default Media Receiver sessions are monitored for the lifetime of the group. Its one-second HLS target duration typically leaves playback roughly three seconds behind the encoder's live edge. `--serve-only` remains a single-host diagnostic mode.

Live output defaults to aspect-preserved 1280x720 H.264 Baseline Level 3.1 for compatibility with Google Nest Hub receivers. The mirroring path selects the minimum valid H.264 level from resolution, frame rate, and bitrate: 720p60 uses Level 3.2 rather than incorrectly advertising Level 3.1. If the Cast `ANSWER` selects a lower display frame rate than requested, capture and encoding are capped to the receiver's rate instead of wasting bandwidth on frames it cannot display. ScreenCaptureKit presentation timestamps drive the HLS timeline even when macOS emits metadata-only samples between video frames.

ScreenCaptureKit may omit the pixel buffer when nothing on the display changed. On those idle capture ticks, `cast` retains and re-encodes the most recent IOSurface so VideoToolbox continues producing a steady timeline and periodic keyframes. The HLS transport uses keyframes roughly half a second apart; the mirroring transport uses a one-second recovery interval.

The mirroring capture callback never waits for VideoToolbox or the network. It publishes into a one-frame mailbox: if encoding is busy, a newer raw IOSurface replaces the pending one. Frames that exceed the raw-frame deadline are dropped before encoding, while already encoded H.264 reference frames are never discarded arbitrarily. By default the deadline is two frame periods; override it with `--max-frame-age-ms`, or pass `0` to retain the mailbox but disable deadline expiry.

The media playlist advertises a short four-second live window for latency, while the HTTP server retains a longer back buffer so delayed receiver requests do not fail as segments roll out of the manifest.

The current desktop stream is video-only. Desktop audio is not included yet; audio already present
in a file passed to `video` is either sent unchanged or converted to AAC. The low-latency transport is an
early implementation of Google's documented Cast Streaming protocol and has so far been exercised
against a Nest Hub-class receiver. Use `--transport hls` if another receiver rejects the mirroring
offer.

## Latency profiling

`profile` runs the same ScreenCaptureKit, VideoToolbox, encrypted RTP, and receiver-feedback path as `desktop`, using a deliberately small 10 ms receiver probe buffer by default. A single-receiver profile redraws a terminal graph of the most recent per-second p95 latency and displays cumulative p50, p95, p99, and packet-loss measurements. Use the desktop normally—especially scrolling, animation, and window changes—so keyframe and bitrate pressure resemble the intended workload.

Repeat `--host` to profile a receiver group. Cast reports each receiver independently and emits one
command using the worst observed tail latency, loss, negotiated frame rate, and bitrate outcome.
Synthetic profiling and auto-tuning also support receiver groups. With `--extend`, each receiver is
profiled against its own temporary display and capture pipeline.

Add `--synthetic` for repeatable comparisons between encoder or transport settings. This bypasses ScreenCaptureKit and writes deterministic `420v` content into a small reusable IOSurface pool, then follows the same latest-frame queue, VideoToolbox, encryption, RTP, feedback, and receiver path as normal mirroring. Reports identify this stable workload as `synthetic-v1`. Its ten-second cycle contains four equal 2.5-second phases:

- static desktop-like content;
- a moving region that changes only part of the frame;
- deterministic full-frame high motion;
- abrupt full-scene cuts every half second.

The live graph names the current phase. The final report breaks down acknowledged frame size, p95/p99 latency, network latency, and retransmission rate by phase. It also reports generator render time and skipped schedule slots. Synthetic drawing completes before the frame-ready timestamp, so drawing cost is shown separately rather than being folded into the latency recommendation.

The final report separates screen-composite age, raw queue wait, VideoToolbox encode, H.264 preparation, sender-lock wait, UDP send, and receiver feedback. It also reports raw-frame replacements/expiry, frames and bytes in flight, packet pacing, adaptive-bitrate changes, and which VideoToolbox latency controls the hardware accepted. The report still provides aggressive, balanced, and resilient receiver-delay settings; balanced covers the measured p99 plus one frame of decode headroom and an additional margin when packet loss occurred.

Add `--auto-tune` alongside `--synthetic` to compare the sender latency controls automatically. The default 60-second budget is divided into six ten-second trials: defaults, fixed bitrate, VideoToolbox quality priority, a one-frame raw deadline, deadline expiry disabled, and a final combined validation trial. Each trial starts the same complete `synthetic-v1` cycle and relaunches the receiver; negotiation time is not counted against `--seconds`. The final table ranks the trials and emits a `desktop` command containing the measured winner's `--max-frame-age-ms`, `--fixed-bitrate`, and `--quality-priority` switches when applicable.

The auto-tune score favors a low p95/p99 latency tail while penalizing retransmissions, raw-frame drops, and failure to sustain the negotiated frame rate. A candidate must improve the score by more than 5 ms before the tuner prefers it over a less customized configuration. This deliberately prevents small run-to-run differences from turning into fragile recommendations. Use a larger `--seconds` value for more confidence; the tuner optimizes sender latency and transport reliability, not image quality or camera-measured glass-to-glass delay.

```sh
# Default 60-second profile at 1280x720, 30 fps, 6 Mbit/s
cargo run --release -- profile --host 192.168.1.50

# Repeatable 60-second stress profile; no Screen Recording permission required
cargo run --release -- profile \
  --host 192.168.1.50 \
  --synthetic

# Six repeatable ten-second trials, followed by a recommended cast command
cargo run --release -- profile \
  --host 192.168.1.50 \
  --synthetic \
  --auto-tune

# Profile the exact workload intended for 1080p casting
cargo run --release -- profile \
  --host 192.168.1.50 \
  --width 1920 \
  --height 1080 \
  --bitrate 10000000 \
  --seconds 120
```

This is a sender-to-receiver reliability estimate, not a camera-measured glass-to-glass latency measurement. Re-profile after changing resolution, frame rate, bitrate, receiver, or Wi-Fi conditions.

`capture` remains a diagnostic command. It writes the length-prefixed H.264 samples emitted by VideoToolbox in AVCC form, not a standalone playable file.

## Performance controls

The low-latency mirroring path enables these behaviors by default:

- latest-frame-wins raw capture with a two-frame deadline;
- VideoToolbox real-time encoding, no B-frame reordering, and speed-over-quality priority;
- feedback-driven bitrate adaptation with fast 20% decreases and slower recovery;
- frame-aware RTP pacing capped at 5 ms per frame, with larger keyframe bursts;
- automatic H.264 level selection and receiver frame-rate capping.

For controlled A/B profiles, use `--max-frame-age-ms 0` to disable deadline expiry, `--fixed-bitrate` to disable feedback adaptation, or `--quality-priority` to disable VideoToolbox's speed-priority hint. Use `profile --synthetic --auto-tune` to have the profiler compare those controls and put the measured winner into its recommended command. Explicit latency-control switches cannot be combined with `--auto-tune`, because the tuner needs to vary them itself. Receiver-requested packet retransmission always remains active.

Apple's hardware encoder may report `MaxFrameDelayCount=0` as unsupported in this real-time mode. This is recorded as a capability result rather than treated as a failure; the current VideoToolbox wrapper completes every submitted frame synchronously, so no hidden multi-frame encoder queue is left outstanding. Apple's dedicated low-latency rate-control mode is not enabled because it requires High profile, an infinite GOP, and temporal layers, which conflicts with the built-in receiver's Baseline/recovery-IDR path.

### Cursor latency boundary

ScreenCaptureKit currently composites the cursor into each captured video frame. A genuinely late-latched cursor would require sending cursor position and shape separately and rendering it after video decode. The closed built-in Chrome Mirroring receiver exposes no independent cursor overlay stream, so sender-only code cannot deliver that optimization. It becomes practical only with a custom receiver; until then, keeping the cursor in the hardware-encoded frame is the compatible behavior.

## Troubleshooting

- Ensure the Mac and receiver are on the same LAN and client isolation is disabled.
- Allow incoming connections if the macOS firewall prompts for `cast`.
- For local files, start with an H.264/AAC MP4; direct playback does not transcode unsupported media.
- If `video` reports that the receiver never requested the file, check the firewall and guest/client-isolation settings.
- Try a lower bitrate for congested Wi-Fi: `--bitrate 3000000`.
- Use `--transport hls --serve-only` to test HLS packaging without contacting a receiver; for safety this mode binds only to loopback.
- Port 8080 must be available on every HLS listener interface, or select another one with `--http-port`.
- If `--extend` reports that `CGVirtualDisplay` is unavailable, that macOS build is not compatible
  with the experimental private API; cast an existing display instead.
- If a temporary display is blank or cannot be captured, re-check Screen Recording permission for
  the terminal running Cast.
- If low-latency playback stutters, first try `--target-delay-ms 400`, then a lower bitrate such as `--bitrate 3000000`.
- Add `-v` before the command for useful diagnostics, or `-vv` for trace-level protocol details.
