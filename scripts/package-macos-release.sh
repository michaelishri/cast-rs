#!/usr/bin/env bash

set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <arm64|x86_64>" >&2
  exit 2
fi

archive_arch="$1"
case "$archive_arch" in
  arm64)
    expected_machine="arm64"
    ;;
  x86_64)
    expected_machine="x86_64"
    ;;
  *)
    echo "unsupported macOS archive architecture: $archive_arch" >&2
    exit 2
    ;;
esac

actual_machine="$(uname -m)"
if [[ "$actual_machine" != "$expected_machine" ]]; then
  echo "refusing to package $archive_arch on $actual_machine; macOS archives must be built natively" >&2
  exit 1
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$repo_root/Cargo.toml" | head -n 1)"
output_dir="${CAST_RELEASE_OUTPUT_DIR:-$repo_root/dist}"
binary="${CAST_RELEASE_BINARY:-$repo_root/target/release/cast}"
media_root="${CAST_MEDIA_ROOT:-$repo_root/.build/ffmpeg-dist}"
ffmpeg_source="${CAST_FFMPEG_SOURCE:-$repo_root/.build/ffmpeg-8.0.1}"
swift_scratch="${CAST_SWIFT_SCRATCH:-$repo_root/.build/macos-swift-$archive_arch}"
archive_root="cast-$version-macos-$archive_arch"
work_root="$(mktemp -d "${TMPDIR:-/tmp}/cast-macos-package.XXXXXX")"
trap 'rm -rf -- "$work_root"' EXIT
package_root="$work_root/package/$archive_root"
app="$package_root/Cast.app"
contents="$app/Contents"
runtime="$contents/Resources/runtime"
extract_root="$work_root/extracted"

test -x "$binary"
test -d "$media_root/lib"
test -f "$ffmpeg_source/COPYING.LGPLv2.1"
mkdir -p "$package_root/lib" "$contents/MacOS" "$runtime/lib" "$extract_root" "$output_dir"

swift build --package-path "$repo_root/desktop/macos" --configuration release --scratch-path "$swift_scratch"
swift_executable="$swift_scratch/release/CastDesktop"
test -x "$swift_executable"

install -m 0755 "$binary" "$package_root/cast"
install -m 0755 "$binary" "$runtime/cast"
install -m 0755 "$swift_executable" "$contents/MacOS/Cast"
cp -a "$media_root"/lib/*.dylib "$package_root/lib/"
cp -a "$media_root"/lib/*.dylib "$runtime/lib/"
cp "$repo_root/README.md" "$repo_root/LICENSE" "$repo_root/THIRD_PARTY_NOTICES.md" "$package_root/"
cp "$ffmpeg_source/COPYING.LGPLv2.1" "$package_root/FFMPEG-LICENSE-LGPL-2.1.txt"
cp "$repo_root/desktop/macos/Resources/Info.plist" "$contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" "$contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $version" "$contents/Info.plist"

icon_work="$work_root/AppIcon.iconset"
mkdir -p "$icon_work"
icon_generator="$work_root/generate-icon"
xcrun swiftc "$repo_root/desktop/macos/Resources/generate-icon.swift" -o "$icon_generator"
"$icon_generator" "$work_root/AppIcon-1024.png"
for size in 16 32 128 256 512; do
  double=$((size * 2))
  sips -z "$size" "$size" "$work_root/AppIcon-1024.png" --out "$icon_work/icon_${size}x${size}.png" >/dev/null
  sips -z "$double" "$double" "$work_root/AppIcon-1024.png" --out "$icon_work/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$icon_work" -o "$contents/Resources/AppIcon.icns"

fix_runtime_paths() {
  local root="$1"
  local executable="$2"
  local library
  local object
  local reference
  local basename

  while IFS= read -r -d '' library; do
    install_name_tool -id "@rpath/$(basename "$library")" "$library"
  done < <(find "$root/lib" -type f -name '*.dylib' -print0)

  while IFS= read -r -d '' object; do
    while IFS= read -r reference; do
      basename="$(basename "$reference")"
      if [[ -f "$root/lib/$basename" && "$reference" != "@rpath/$basename" ]]; then
        install_name_tool -change "$reference" "@rpath/$basename" "$object"
      fi
    done < <(otool -L "$object" | tail -n +2 | awk '{print $1}')
  done < <(find "$root/lib" -type f -name '*.dylib' -print0)

  while IFS= read -r reference; do
    basename="$(basename "$reference")"
    if [[ -f "$root/lib/$basename" && "$reference" != "@rpath/$basename" ]]; then
      install_name_tool -change "$reference" "@rpath/$basename" "$executable"
    fi
  done < <(otool -L "$executable" | tail -n +2 | awk '{print $1}')

  if ! otool -l "$executable" | grep -A2 LC_RPATH | grep -Fq '@executable_path/lib'; then
    install_name_tool -add_rpath '@executable_path/lib' "$executable"
  fi
}

fix_runtime_paths "$package_root" "$package_root/cast"
fix_runtime_paths "$runtime" "$runtime/cast"

while IFS= read -r -d '' code; do
  codesign --force --sign - "$code"
done < <(find "$package_root/lib" "$runtime/lib" -type f -name '*.dylib' -print0)
codesign --force --sign - "$package_root/cast"
codesign --force --sign - "$runtime/cast"
codesign --force --sign - "$contents/MacOS/Cast"
codesign --force --deep --sign - "$app"

plutil -lint "$contents/Info.plist"
test "$(plutil -extract CFBundleIdentifier raw "$contents/Info.plist")" = "io.github.michaelishri.cast"
test "$(plutil -extract LSUIElement raw "$contents/Info.plist")" = "true"
codesign --verify --deep --strict --verbose=2 "$app"

audit_file="$output_dir/$archive_root.otool.txt"
{
  otool -L "$package_root/cast"
  otool -L "$runtime/cast"
  find "$runtime/lib" -type f -name '*.dylib' -exec otool -L {} \;
} | tee "$audit_file"
if grep -E '/(opt/homebrew|usr/local|\.build)/' "$audit_file"; then
  echo "packaged code retains a build-machine library path" >&2
  exit 1
fi

env -u DYLD_LIBRARY_PATH "$package_root/cast" --version
env -u DYLD_LIBRARY_PATH "$runtime/cast" --help >/dev/null

"$contents/MacOS/Cast" &
app_pid=$!
sleep 2
kill -TERM "$app_pid"
wait "$app_pid" || true

archive_path="$output_dir/$archive_root.tar.gz"
checksum_path="$archive_path.sha256"
tar -C "$work_root/package" -czf "$archive_path" "$archive_root"
(
  cd "$output_dir"
  shasum -a 256 "$(basename "$archive_path")" > "$(basename "$checksum_path")"
  shasum -a 256 -c "$(basename "$checksum_path")"
)

tar -xzf "$archive_path" -C "$extract_root"
extracted="$extract_root/$archive_root"
codesign --verify --deep --strict --verbose=2 "$extracted/Cast.app"
plutil -lint "$extracted/Cast.app/Contents/Info.plist"
env -u DYLD_LIBRARY_PATH "$extracted/cast" --version
env -u DYLD_LIBRARY_PATH "$extracted/Cast.app/Contents/Resources/runtime/cast" --version
