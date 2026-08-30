# Linux release validation

Run this matrix for a release candidate built by `.github/workflows/release.yml`. The ordinary CI workflow uses the same `scripts/package-linux-release.sh` path before release and retains the x86_64/aarch64 archives, checksums, and `ldd` reports as `validation-linux-*` artifacts. Linux source checks compile against both the Ubuntu 22.04 header floor and Ubuntu 24.04 current-header baseline. The native aarch64 archive uses GitHub's supported Ubuntu 24.04 ARM64 image; the x86_64 Ubuntu 22.04 job establishes the glibc 2.35 binary floor. Desktop rows require Linux Mint Cinnamon/X11 plus the existing GNOME and KDE Wayland portal coverage and a Cast video receiver.

## Automated release gate

- [ ] `cargo fmt -- --check`
- [ ] `cargo clippy --locked --all-targets --all-features -- -D warnings`
- [ ] `cargo test --locked`
- [ ] `cargo build --locked --release`
- [ ] Both archives start with `LD_LIBRARY_PATH` unset and report the tagged version.
- [ ] `readelf` reports `$ORIGIN/lib` for the executable and `$ORIGIN` for bundled libraries.
- [ ] Both `ldd` audit artifacts contain no `not found` entries and resolve FFmpeg from the archive.
- [ ] Both `.sha256` files verify.
- [ ] Neither archive contains an OpenH264 module, GPU driver, portal, PipeWire, WirePlumber, or glibc.
- [ ] Neither archive links a native X11/XCB client library; the X11 protocol client remains pure Rust.

Record the successful CI run URL and both artifact names in the release issue. Do not check these rows from an x86_64-only local run; the native aarch64 job is part of the gate.

## Real-desktop matrix

Repeat applicable portal rows on current GNOME Wayland and KDE Plasma Wayland, and native X11 rows on current Linux Mint Cinnamon/X11. Record desktop/compositor version, GPU/driver, receiver model/firmware, command, result, and relevant `-v` output in the release issue.

| Area | Required cases |
| --- | --- |
| Encoder | `auto`, explicit NVENC, explicit VA-API, explicit OpenH264; unavailable explicit providers fail before receiver startup |
| Source | remembered normal source, `--select-source`, portal denial, expired restore token, window and monitor, `--extend` capability success/failure |
| X11 source | auto detection, explicit `--backend x11`, primary and named RandR monitors, invalid name, missing DISPLAY/Xauthority, MIT-SHM and core GetImage fallback, `--extend` temporary output creation and cleanup |
| Mirror | audio on/off, one receiver, receiver group, extended receiver group, adaptive and `--fixed-bitrate`, speed-default and `--quality-priority` encoder/scaler paths |
| HLS | audio on/off, one/multiple receivers, normal/extended sources |
| Diagnostics | `displays`, `capture`, normal `profile`, synthetic profile and synthetic `--auto-tune` |
| Audio | default sink monitor only, no microphone, receiver fan-out, local volume/mute forwarding, output-device switch |
| Cleanup | normal exit, Ctrl-C, portal close, forced capture/encoder/network failure, panic harness; verify source sessions close and output state restores |
| Codec setup | interactive denial, `--yes`, `--check`, corrupt/missing module, checksum failure, non-interactive missing-codec path |

For X11 `--extend`, record the GPU/Xorg driver, connector and CRTC chosen, generated mode, expanded framebuffer geometry, and final restored geometry. Exercise normal exit, Ctrl-C, partial multi-output startup, outputs ending in a different order, and a real display hotplug where available. Confirm that unsupported drivers or exhausted connectors/CRTCs fail without leaving a temporary mode or enlarged framebuffer.

Wayland-native capture outside the existing portal path and V4L2 M2M encoding are explicitly unsupported. File playback and the TUI remain usable when their other system requirements are met.
