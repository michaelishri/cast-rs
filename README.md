# castscreen

An early, all-Rust macOS CLI for experimenting with desktop-to-Chromecast streaming.

The CLI can now join the platform boundaries into a live desktop stream:

- discover Cast devices with mDNS;
- control Google Cast receivers with `rust-cast`;
- capture a macOS display with ScreenCaptureKit and hardware-encode it with VideoToolbox;
- send encrypted H.264 over Cast Streaming RTP with receiver feedback and retransmission;
- keep capture non-blocking with a latest-frame-wins encoder queue, adaptive bitrate, and bounded packet pacing;
- retain fragmented-MP4 HLS as a compatibility fallback.

## Requirements

- macOS 13 or newer;
- Rust 1.85 or newer (edition 2024);
- Xcode (the native capture bindings use Apple's Swift runtime);
- Mac and Chromecast on the same network;
- Screen Recording permission for your terminal when using capture commands.

## Build

```sh
cargo build --release
```

## Releases

`Cargo.toml` is the single source of truth for the CLI version; Clap exposes that same version through `castscreen --version`. Releases use matching annotated Git tags: package version `0.1.0` is released as `v0.1.0`.

After changing the package version and pushing its commit, run:

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
cargo run --release -- cast-desktop --host 192.168.1.50

# Profile the real capture/network path for one minute and recommend a delay
cargo run --release -- profile --host 192.168.1.50

# Run the repeatable synthetic stress workload instead of capturing the desktop
cargo run --release -- profile --host 192.168.1.50 --synthetic

# Compare the latency controls and print a measured winning cast command
cargo run --release -- profile --host 192.168.1.50 --synthetic --auto-tune

# Test whether the receiver actually negotiates 60 fps
cargo run --release -- profile --host 192.168.1.50 --synthetic --fps 60

# Reduce or increase the requested receiver playout buffer (default: 200 ms)
cargo run --release -- cast-desktop \
  --host 192.168.1.50 \
  --target-delay-ms 150

# Use the fragmented-MP4 HLS compatibility path instead
cargo run --release -- cast-desktop \
  --host 192.168.1.50 \
  --transport hls

# Include Cast, transport, and skipped-sample diagnostics
cargo run -- -v cast-desktop --host 192.168.1.50

# Include trace-level Cast and encoder diagnostics
cargo run -- -vv cast-desktop --host 192.168.1.50

# Cast a particular display for 30 seconds
cargo run -- cast-desktop \
  --host 192.168.1.50 \
  --display 1 \
  --seconds 30

# Override the default 1280x720 compatibility output for a more capable receiver
cargo run -- cast-desktop \
  --host 192.168.1.50 \
  --width 1920 \
  --height 1080

# Prove capture + hardware H.264 encoding for ten seconds
cargo run -- capture --display 1 --seconds 10 --output capture.avcc

# Play an existing HLS live stream through the Cast Default Media Receiver
cargo run -- cast-url \
  --host 192.168.1.50 \
  --url http://192.168.1.20:8080/master.m3u8 \
  --hls-video-segment-format fmp4 \
  --monitor-seconds 30
```

Run `cargo run -- --help` or add `--help` after a subcommand for every option.

## How live casting works

By default, `cast-desktop` launches the receiver's built-in Chrome Mirroring application and performs the Cast Streaming `OFFER`/`ANSWER` exchange. It converts VideoToolbox's AVCC output to Annex B, prepends SPS/PPS to keyframes, encrypts each frame with session-specific AES-128-CTR material, and sends it as Cast RTP over UDP. RTCP sender reports establish the media clock; receiver checkpoints and loss fields drive history cleanup and packet retransmission. The default offer requests a 200 ms receiver playout delay. Actual glass-to-glass latency also includes capture, encode, Wi-Fi, decode, and display time.

`--transport hls` uses the earlier compatibility path. It determines the Mac's LAN address from the route to the receiver, binds an HTTP server only to that address, generates a random session path, and serves a rolling fragmented-MP4 HLS stream through the Default Media Receiver. Its one-second HLS target duration typically leaves playback roughly three seconds behind the encoder's live edge.

Live output defaults to aspect-preserved 1280x720 H.264 Baseline Level 3.1 for compatibility with Google Nest Hub receivers. The mirroring path selects the minimum valid H.264 level from resolution, frame rate, and bitrate: 720p60 uses Level 3.2 rather than incorrectly advertising Level 3.1. If the Cast `ANSWER` selects a lower display frame rate than requested, capture and encoding are capped to the receiver's rate instead of wasting bandwidth on frames it cannot display. ScreenCaptureKit presentation timestamps drive the HLS timeline even when macOS emits metadata-only samples between video frames.

ScreenCaptureKit may omit the pixel buffer when nothing on the display changed. On those idle capture ticks, `castscreen` retains and re-encodes the most recent IOSurface so VideoToolbox continues producing a steady timeline and periodic keyframes. The HLS transport uses keyframes roughly half a second apart; the mirroring transport uses a one-second recovery interval.

The mirroring capture callback never waits for VideoToolbox or the network. It publishes into a one-frame mailbox: if encoding is busy, a newer raw IOSurface replaces the pending one. Frames that exceed the raw-frame deadline are dropped before encoding, while already encoded H.264 reference frames are never discarded arbitrarily. By default the deadline is two frame periods; override it with `--max-frame-age-ms`, or pass `0` to retain the mailbox but disable deadline expiry.

The media playlist advertises a short four-second live window for latency, while the HTTP server retains a longer back buffer so delayed receiver requests do not fail as segments roll out of the manifest.

The current live stream is video-only. Desktop audio is not included yet. The low-latency transport is an early implementation of Google's documented Cast Streaming protocol and has so far been exercised against a Nest Hub-class receiver. Use `--transport hls` if another receiver rejects the mirroring offer.

## Latency profiling

`profile` runs the same ScreenCaptureKit, VideoToolbox, encrypted RTP, and receiver-feedback path as `cast-desktop`, using a deliberately small 10 ms receiver probe buffer by default. For 60 seconds it redraws a terminal graph of the most recent per-second p95 latency and displays cumulative p50, p95, p99, and packet-loss measurements. Use the desktop normally—especially scrolling, animation, and window changes—so keyframe and bitrate pressure resemble the intended workload.

Add `--synthetic` for repeatable comparisons between encoder or transport settings. This bypasses ScreenCaptureKit and writes deterministic `420v` content into a small reusable IOSurface pool, then follows the same latest-frame queue, VideoToolbox, encryption, RTP, feedback, and receiver path as normal mirroring. Reports identify this stable workload as `synthetic-v1`. Its ten-second cycle contains four equal 2.5-second phases:

- static desktop-like content;
- a moving region that changes only part of the frame;
- deterministic full-frame high motion;
- abrupt full-scene cuts every half second.

The live graph names the current phase. The final report breaks down acknowledged frame size, p95/p99 latency, network latency, and retransmission rate by phase. It also reports generator render time and skipped schedule slots. Synthetic drawing completes before the frame-ready timestamp, so drawing cost is shown separately rather than being folded into the latency recommendation.

The final report separates screen-composite age, raw queue wait, VideoToolbox encode, H.264 preparation, sender-lock wait, UDP send, and receiver feedback. It also reports raw-frame replacements/expiry, frames and bytes in flight, packet pacing, adaptive-bitrate changes, and which VideoToolbox latency controls the hardware accepted. The report still provides aggressive, balanced, and resilient receiver-delay settings; balanced covers the measured p99 plus one frame of decode headroom and an additional margin when packet loss occurred.

Add `--auto-tune` alongside `--synthetic` to compare the sender latency controls automatically. The default 60-second budget is divided into six ten-second trials: defaults, fixed bitrate, VideoToolbox quality priority, a one-frame raw deadline, deadline expiry disabled, and a final combined validation trial. Each trial starts the same complete `synthetic-v1` cycle and relaunches the receiver; negotiation time is not counted against `--seconds`. The final table ranks the trials and emits a `cast-desktop` command containing the measured winner's `--max-frame-age-ms`, `--fixed-bitrate`, and `--quality-priority` switches when applicable.

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
- Allow incoming connections if the macOS firewall prompts for `castscreen`.
- Try a lower bitrate for congested Wi-Fi: `--bitrate 3000000`.
- Use `--transport hls --serve-only` to test HLS packaging without contacting a receiver; for safety this mode binds only to loopback.
- Port 8080 must be available for HLS, or select another one with `--http-port`.
- If low-latency playback stutters, first try `--target-delay-ms 400`, then a lower bitrate such as `--bitrate 3000000`.
- Add `-v` before the command for useful diagnostics, or `-vv` for trace-level protocol details.
