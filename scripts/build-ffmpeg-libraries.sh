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
openh264_root="$repo_root/.build/openh264-2.3.0"
openh264_commit="2e637867315ffeda3cd8970825ec86acc3fc4a30"
nvcodec_root="$repo_root/.build/nv-codec-headers-12.1.14.0"
nvcodec_commit="1889e62e2d35ff7aa9baca2bceb14f053785e6f1"

mkdir -p "$repo_root/.build" "$build_root" "$install_prefix"
if [[ ! -d "$source_root/.git" ]]; then
  git clone --filter=blob:none --depth=1 --branch n8.0.1 https://github.com/FFmpeg/FFmpeg.git "$source_root"
fi

actual_commit="$(git -C "$source_root" rev-parse HEAD)"
if [[ "$actual_commit" != "$ffmpeg_commit" ]]; then
  echo "Refusing to build unexpected FFmpeg source at $actual_commit" >&2
  exit 1
fi

if [[ "$(uname -s)" == "Linux" ]]; then
  if [[ ! -d "$openh264_root/.git" ]]; then
    git clone --filter=blob:none --depth=1 --branch v2.3.0 https://github.com/cisco/openh264.git "$openh264_root"
  fi
  if [[ "$(git -C "$openh264_root" rev-parse HEAD)" != "$openh264_commit" ]]; then
    echo "Refusing to use unexpected OpenH264 headers" >&2
    exit 1
  fi
  if [[ ! -d "$nvcodec_root/.git" ]]; then
    git clone --filter=blob:none --depth=1 --branch n12.1.14.0 https://github.com/FFmpeg/nv-codec-headers.git "$nvcodec_root"
  fi
  if [[ "$(git -C "$nvcodec_root" rev-parse HEAD)" != "$nvcodec_commit" ]]; then
    echo "Refusing to use unexpected nv-codec-headers" >&2
    exit 1
  fi

  mkdir -p "$install_prefix/include/wels" "$install_prefix/lib/pkgconfig"
  cp "$openh264_root"/codec/api/wels/*.h "$install_prefix/include/wels/"
  c++ -std=c++17 -O2 -fPIC -shared \
    -I"$install_prefix/include/wels" \
    -Wl,-soname,libcast_openh264_loader.so.0 \
    "$repo_root/native/openh264_loader.cpp" -ldl \
    -o "$install_prefix/lib/libcast_openh264_loader.so.0"
  ln -sfn libcast_openh264_loader.so.0 "$install_prefix/lib/libcast_openh264_loader.so"
  sed \
    -e "s|@PREFIX@|$install_prefix|g" \
    "$repo_root/native/openh264-loader.pc.in" \
    > "$install_prefix/lib/pkgconfig/openh264.pc"
  make -s -C "$nvcodec_root" install PREFIX="$install_prefix"
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
  --enable-muxer=hls
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
else
  export PKG_CONFIG_PATH="$install_prefix/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
  configure_options+=(
    --enable-libopenh264
    --enable-encoder=libopenh264
    --enable-ffnvcodec
    --enable-nvenc
    --enable-encoder=h264_nvenc
  )
  if pkg-config --exists libva; then
    configure_options+=(
      --enable-vaapi
      --enable-encoder=h264_vaapi
    )
  fi
fi

(
  cd "$build_root"
  "$source_root/configure" "${configure_options[@]}"
  make -s -j"$(getconf _NPROCESSORS_ONLN)"
  make -s install
)
