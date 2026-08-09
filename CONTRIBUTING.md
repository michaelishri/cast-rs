# Contributing to Cast

Thanks for helping improve Cast. This guide covers the local development setup, the project’s main implementation boundaries, and the release process. End-user installation and usage live in the [README](README.md).

## Prerequisites

Developing Cast requires:

- macOS 13 or newer;
- Rust 1.88 or newer;
- Xcode 15 or newer, including its Swift toolchain;
- Homebrew packages `nasm` and `pkg-config`.

The project links pinned FFmpeg libraries for media inspection and compatibility conversion. Build them once into a local directory, then point Cargo at their pkg-config metadata:

```sh
brew install nasm pkg-config
./scripts/build-ffmpeg-libraries.sh "$PWD/.build/ffmpeg-dist"
PKG_CONFIG_PATH="$PWD/.build/ffmpeg-dist/lib/pkgconfig" \
  cargo build --release
```

The helper builds FFmpeg 8.0.1 shared libraries without FFmpeg command-line programs. Release archives include those libraries, so end users do not need this setup.

## Run and verify

Use `cargo run -- --help` or `cargo run -- <command> --help` to inspect the CLI while developing. Useful manual checks include:

```sh
cargo run -- devices
cargo run -- displays
cargo run --release -- desktop --host 192.168.1.50
cargo run --release -- desktop --host 192.168.1.50 --audio
cargo run --release -- profile --host 192.168.1.50 --synthetic
cargo run --release -- tui ~/Movies
```

Before opening a pull request, run the local quality gate:

```sh
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
cargo build --locked --release
```

`profile --synthetic` deliberately bypasses ScreenCaptureKit and generates a deterministic workload, which makes it the preferred regression check for capture, encoder, and transport changes. `capture` is a diagnostic command that writes length-prefixed H.264 samples in AVCC form, not a playable video file.

## Architecture notes

Cast discovers receivers with mDNS and controls them through a pinned revision of the [`rust-cast` fork](https://github.com/michaelishri/rust-cast/tree/cast-rs). Local video is served only from the receiver-facing LAN interface at a fresh random URL; the server supports HTTP ranges so receivers can seek and read MP4 metadata efficiently.

The `video` path either serves compatible media directly, remuxes it, or prepares fragmented-MP4 HLS while playback proceeds. It retains prepared segments until shutdown so backward seeking works. The `desktop` path uses ScreenCaptureKit, VideoToolbox, and (when `--audio` is set) an AudioToolbox AAC-LC encoder. The low-latency path performs the Cast Streaming offer/answer exchange and sends H.264 and AAC as separately encrypted Cast RTP streams. HLS remains a compatibility fallback, with fMP4 video and an alternate packed-AAC rendition.

The reusable local-video playback controller owns inspection, preparation, the private HTTP/HLS server, Cast control, and cleanup on a worker thread. Presentation adapters consume structured preparation, status, volume, completion, and categorized-failure events. Receiver handoff deliberately keeps the prepared source alive, interpolates the latest receiver position, and loads it on the new receiver with the same paused/playing intent. Keep the existing line-oriented `video` adapter compatible when changing this layer.

The Ratatui application keeps all Crossterm input and drawing on the main thread with a 100 ms tick. Receiver discovery and playback run in cancellable workers; TUI logging uses a bounded nonblocking channel and retains the newest 500 entries. The terminal guard must remain idempotent and restore raw mode, cursor visibility, mouse capture, and the primary screen on every exit path, including unwind. Reducer and TestBackend tests should cover UI behavior without real devices.

For TUI changes, manually check a normal and sub-60×18 terminal, direct/remux/incremental media, multi-item advancement, pause/seek/stop, mouse progress and volume, receiver rescan and handoff, media/network failures, and clean exit on a real macOS Cast setup.

The mirroring capture callback must never wait for VideoToolbox or the network. It uses a latest-frame-wins, one-frame mailbox; old raw frames can expire before encoding, but encoded H.264 reference frames are not discarded arbitrarily. RTP sender reports establish the media clock, and receiver feedback drives retransmission, history cleanup, and adaptive bitrate. Preserve these properties when modifying the capture, encoder, or network pipeline.

Audio capture also stays nonblocking, but uses a bounded FIFO because dropping arbitrary PCM buffers would shift the audio timeline. The audio worker fills timestamp gaps with silence and shares the capture epoch with video. Audio RTP feedback is isolated from video adaptive bitrate. The HLS store waits until matching audio and video segment ranges are ready before publishing either playlist entry; packed-AAC segments begin with Apple’s transport-stream timestamp ID3 `PRIV` tag.

ScreenCaptureKit can omit a pixel buffer for idle display ticks. Cast re-encodes the latest IOSurface in that case to retain a steady timeline and periodic keyframes. The current built-in receiver path has no independent cursor overlay, so the cursor stays composited in the encoded frame.

## Releases

`Cargo.toml` is the single source of truth for the CLI version; `cast --version` exposes that version. Use matching annotated tags: package version `X.Y.Z` is released as `vX.Y.Z`.

Follow the full [release checklist](docs/RELEASE_CHECKLIST.md). After the version pull request is merged, check out that exact clean `origin/main` commit and run:

```sh
./scripts/release.sh
```

The helper runs formatting, Clippy, and tests, then pushes the matching tag. GitHub Actions verifies the tag/version agreement, builds separate Apple Silicon and Intel archives, creates SHA-256 files, and publishes the GitHub release. The checklist also covers the Homebrew formula, bottles, verification, and recovery.
