# Third-party media libraries

Cast links FFmpeg 8.0.1 libraries built from commit
`894da5ca7d742e4429ffb2af534fcda0103ef593` without FFmpeg command-line programs, GPL components,
non-free components, network protocols, or automatically discovered optional dependencies.

FFmpeg is licensed under the GNU Lesser General Public License version 2.1 or later. Its source,
license texts, and build instructions are available from:

- <https://github.com/FFmpeg/FFmpeg/tree/894da5ca7d742e4429ffb2af534fcda0103ef593>
- `scripts/build-ffmpeg-libraries.sh` in this repository

Cast itself remains licensed under the terms declared in `Cargo.toml`. This notice does not change
the licenses applicable to the separately distributed FFmpeg libraries.

Linux FFmpeg builds enable the native AAC encoder and HLS/MP4 muxers plus H.264 adapters for
OpenH264, NVIDIA NVENC, and VA-API. Release archives contain FFmpeg and Cast's loader library, but
not GPU drivers, VA-API driver implementations, or codec implementations supplied by vendors.

## OpenH264 integration

Linux builds include OpenH264 2.3.0 API headers from commit
`2e637867315ffeda3cd8970825ec86acc3fc4a30` and Cast's own delayed-loading shim. The OpenH264
project source and BSD license are available from <https://github.com/cisco/openh264/tree/v2.3.0>.

Cast release archives do not contain Cisco's OpenH264 implementation. On request, `cast setup`
downloads the architecture-specific OpenH264 2.3.0 module directly from
<https://ciscobinary.openh264.org/> after user confirmation and verifies its pinned SHA-256
checksum. Cisco distributes those separately downloaded binaries under the terms at
<https://www.openh264.org/BINARY_LICENSE.txt>. Users may instead provide a compatible system
OpenH264 2.3.x module.

Linux builds also use the MIT-licensed FFmpeg nv-codec-headers 12.1.14.0 package from commit
`1889e62e2d35ff7aa9baca2bceb14f053785e6f1`; no NVIDIA driver or codec implementation is included.

## Linux desktop integration

Cast uses the MIT-licensed `ashpd` Rust crate to call the freedesktop ScreenCast portal and the
MIT-licensed `pipewire-rs`/`libspa` crates to consume portal video and the default sink monitor.
Those crates bind to system XDG portal, PipeWire, SPA, and WirePlumber services; the services and
their shared libraries are not redistributed in Cast archives.

VA-API support is compiled against the MIT-licensed libva interface. The system's libva shared
libraries, DRM stack, and vendor VA driver remain external dependencies. NVENC support is compiled
from the MIT-licensed nv-codec-headers noted above and likewise leaves the NVIDIA driver external.

`native/openh264_loader.cpp` is Cast project code under this repository's MIT license. OpenH264
headers are used to compile that delayed loader, but Cisco's binary module is never included in a
Cast release archive.
