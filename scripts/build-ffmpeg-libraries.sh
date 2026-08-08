#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <absolute-install-prefix>" >&2
  exit 2
fi

install_prefix="$1"
if [[ "$install_prefix" != /* || "$install_prefix" == "/" ]]; then
  echo "FFmpeg install prefix must be an absolute, non-root path" >&2
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source_root="$repo_root/.build/ffmpeg-8.0.1"
build_root="$repo_root/.build/ffmpeg-build"
ffmpeg_commit="894da5ca7d742e4429ffb2af534fcda0103ef593"

mkdir -p "$repo_root/.build" "$build_root" "$install_prefix"
if [[ ! -d "$source_root/.git" ]]; then
  git clone --filter=blob:none --depth=1 --branch n8.0.1 https://github.com/FFmpeg/FFmpeg.git "$source_root"
fi

actual_commit="$(git -C "$source_root" rev-parse HEAD)"
if [[ "$actual_commit" != "$ffmpeg_commit" ]]; then
  echo "Refusing to build unexpected FFmpeg source at $actual_commit" >&2
  exit 1
fi

configure_options=(
  "--prefix=$install_prefix"
  --enable-shared
  --disable-static
  --disable-programs
  --disable-avdevice
  --disable-avfilter
  --disable-doc
  --disable-debug
  --disable-gpl
  --disable-nonfree
  --disable-network
  --disable-autodetect
  --disable-encoders
  --enable-encoder=aac
  --disable-muxers
  --enable-muxer=mp4
)
if ! command -v nasm >/dev/null 2>&1; then
  configure_options+=(--disable-x86asm)
fi
if [[ "$(uname -s)" == "Darwin" ]]; then
  configure_options+=(
    --enable-videotoolbox
    --enable-audiotoolbox
    --enable-encoder=h264_videotoolbox
    --install-name-dir=@rpath
  )
fi

(
  cd "$build_root"
  "$source_root/configure" "${configure_options[@]}"
  make -s -j"$(getconf _NPROCESSORS_ONLN)"
  make -s install
)
