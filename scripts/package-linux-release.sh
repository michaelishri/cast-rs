#!/usr/bin/env bash

set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <x86_64|aarch64>" >&2
  exit 2
fi

archive_arch="$1"
case "$archive_arch" in
  x86_64)
    expected_machine="x86_64"
    ;;
  aarch64)
    expected_machine="aarch64"
    ;;
  *)
    echo "unsupported Linux archive architecture: $archive_arch" >&2
    exit 2
    ;;
esac

actual_machine="$(uname -m)"
if [[ "$actual_machine" != "$expected_machine" ]]; then
  echo "refusing to package $archive_arch on $actual_machine; Linux archives must be built natively" >&2
  exit 1
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
output_dir="${CAST_RELEASE_OUTPUT_DIR:-$repo_root/dist}"
binary="${CAST_RELEASE_BINARY:-$repo_root/target/release/cast}"
media_root="${CAST_MEDIA_ROOT:-$repo_root/.build/ffmpeg-dist}"
ffmpeg_source="${CAST_FFMPEG_SOURCE:-$repo_root/.build/ffmpeg-8.0.1}"
archive="cast-linux-$archive_arch"
work_root="$(mktemp -d "${TMPDIR:-/tmp}/cast-linux-package.XXXXXX")"
trap 'rm -rf -- "$work_root"' EXIT
package_root="$work_root/package/$archive"
extract_root="$work_root/extracted"

mkdir -p "$package_root/lib" "$extract_root" "$output_dir"
install -m 0755 "$binary" "$package_root/cast"
mkdir -p "$package_root/desktop"
cp -a "$repo_root/desktop/cinnamon" "$package_root/desktop/"
cp -a "$media_root/lib/libavcodec.so"* "$package_root/lib/"
cp -a "$media_root/lib/libavformat.so"* "$package_root/lib/"
cp -a "$media_root/lib/libavutil.so"* "$package_root/lib/"
cp -a "$media_root/lib/libswresample.so"* "$package_root/lib/"
cp -a "$media_root/lib/libswscale.so"* "$package_root/lib/"
cp -a "$media_root/lib/libcast_openh264_loader.so"* "$package_root/lib/"
cp "$repo_root/README.md" "$repo_root/LICENSE" "$repo_root/THIRD_PARTY_NOTICES.md" "$package_root/"
cp "$ffmpeg_source/COPYING.LGPLv2.1" "$package_root/FFMPEG-LICENSE-LGPL-2.1.txt"

patchelf --set-rpath '$ORIGIN/lib' "$package_root/cast"
while IFS= read -r -d '' library; do
  patchelf --set-rpath '$ORIGIN' "$library"
done < <(find "$package_root/lib" -type f -name '*.so*' -print0)

archive_path="$output_dir/$archive.tar.gz"
checksum_path="$archive_path.sha256"
tar -C "$work_root/package" -czf "$archive_path" "$archive"
(
  cd "$output_dir"
  sha256sum "$(basename "$archive_path")" > "$(basename "$checksum_path")"
  sha256sum --check "$(basename "$checksum_path")"
)

tar -xzf "$archive_path" -C "$extract_root"
extracted="$extract_root/$archive"
readelf -d "$extracted/cast" | grep -F '[$ORIGIN/lib]'
while IFS= read -r -d '' library; do
  readelf -d "$library" | grep -F '[$ORIGIN]'
done < <(find "$extracted/lib" -type f -name '*.so*' -print0)

env -u LD_LIBRARY_PATH ldd "$extracted/cast" | tee "$output_dir/$archive.ldd.txt"
if grep -F 'not found' "$output_dir/$archive.ldd.txt"; then
  echo "packaged executable has unresolved shared libraries" >&2
  exit 1
fi
grep -F "$extracted/lib/libavcodec.so" "$output_dir/$archive.ldd.txt"
if grep -E 'lib(X11|xcb|Xfixes|Xrandr)' "$output_dir/$archive.ldd.txt"; then
  echo "packaged executable unexpectedly depends on a native X11 client library" >&2
  exit 1
fi
env -u LD_LIBRARY_PATH "$extracted/cast" --version
env -u LD_LIBRARY_PATH "$extracted/cast" --help >/dev/null

test -x "$extracted/desktop/cinnamon/scripts/install.sh"
test -f "$extracted/desktop/cinnamon/applet/cast@cast-rs/applet.js"
test -f "$extracted/desktop/cinnamon/settings/cs_cast.py"
PYTHONDONTWRITEBYTECODE=1 PYTHONPATH="$extracted/desktop/cinnamon/settings" \
  python3 -m unittest discover -s "$extracted/desktop/cinnamon/tests"
glib-compile-schemas --strict --dry-run "$extracted/desktop/cinnamon/schemas"
cinnamon_stage="$work_root/cinnamon-stage"
DESTDIR="$cinnamon_stage" "$extracted/desktop/cinnamon/scripts/install.sh"
test -f "$cinnamon_stage/usr/share/cinnamon/cinnamon-settings/modules/cs_cast.py"
test -f "$cinnamon_stage/usr/share/cinnamon/applets/cast@cast-rs/applet.js"
DESTDIR="$cinnamon_stage" "$extracted/desktop/cinnamon/scripts/uninstall.sh"
test ! -e "$cinnamon_stage/usr/share/cinnamon/applets/cast@cast-rs"

if tar -tzf "$archive_path" | grep -E '/(libopenh264\.so|libcuda\.so|libnvidia|libva\.so|dri/|libpipewire|libwireplumber|libc\.so)'; then
  echo "archive contains a host codec, GPU driver, desktop service, or glibc library" >&2
  exit 1
fi
