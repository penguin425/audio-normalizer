#!/usr/bin/env bash
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target_dir="${FORGE_TARGET_DIR:-${root_dir}/target/debug}"
tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/forge-host-adapters.XXXXXX")"

cleanup() {
  if [[ -d "$tmp_dir" ]]; then
    find "$tmp_dir" -type f -delete
    find "$tmp_dir" -depth -type d -empty -delete
  fi
}
trap cleanup EXIT

for command_name in pkg-config cc gst-inspect-1.0 gst-launch-1.0; do
  if ! command -v "$command_name" >/dev/null 2>&1; then
    echo "missing required host-adapter command: $command_name" >&2
    exit 1
  fi
done

for module in libavutil gstreamer-1.0 gstreamer-base-1.0 gstreamer-audio-1.0; do
  if ! pkg-config --exists "$module"; then
    echo "missing pkg-config module: $module" >&2
    exit 1
  fi
done

if [[ "${FORGE_SKIP_CARGO_BUILD:-0}" != "1" ]]; then
  cargo build --locked --manifest-path "$root_dir/Cargo.toml" --lib
fi

if [[ ! -f "$target_dir/libforge_normalizer.so" ]]; then
  echo "Forge shared library was not found at $target_dir/libforge_normalizer.so" >&2
  echo "build the library or set FORGE_TARGET_DIR to its directory" >&2
  exit 1
fi

version="$(sed -n '/^version = /{s/.*= *"\([^"]*\)".*/\1/p;q;}' "$root_dir/Cargo.toml")"
if [[ -z "$version" ]]; then
  echo "could not determine Forge package version" >&2
  exit 1
fi

read -r -a av_cflags <<< "$(pkg-config --cflags libavutil)"
read -r -a av_libs <<< "$(pkg-config --libs libavutil)"
read -r -a gst_cflags <<< "$(pkg-config --cflags gstreamer-1.0 gstreamer-base-1.0 gstreamer-audio-1.0)"
read -r -a gst_libs <<< "$(pkg-config --libs gstreamer-1.0 gstreamer-base-1.0 gstreamer-audio-1.0)"

cc -std=c11 -Wall -Wextra -Werror -fPIC \
  -I "$root_dir/include" -I "$root_dir/integrations/ffmpeg" \
  "${av_cflags[@]}" \
  -c "$root_dir/integrations/ffmpeg/forge_ffmpeg_bridge.c" \
  -o "$tmp_dir/forge_ffmpeg_bridge.o"
cc -shared "$tmp_dir/forge_ffmpeg_bridge.o" \
  -L "$target_dir" -Wl,-rpath,"$target_dir" -lforge_normalizer \
  "${av_libs[@]}" -o "$tmp_dir/libforge_ffmpeg_bridge.so"

cc -std=c11 -Wall -Wextra -Werror \
  -I "$root_dir/include" -I "$root_dir/integrations/ffmpeg" \
  "${av_cflags[@]}" "$root_dir/tests/fixtures/ffmpeg_bridge_consumer.c" \
  -L "$tmp_dir" -Wl,-rpath,"$tmp_dir" -lforge_ffmpeg_bridge \
  -L "$target_dir" -Wl,-rpath,"$target_dir" -lforge_normalizer \
  "${av_libs[@]}" -o "$tmp_dir/ffmpeg-bridge-consumer"

cc -std=c11 -Wall -Wextra -Werror -fPIC \
  -DPACKAGE=\"forge-normalizer\" \
  "-DFORGE_PLUGIN_VERSION=\"${version}\"" \
  -I "$root_dir/include" "${gst_cflags[@]}" \
  -c "$root_dir/integrations/gstreamer/gstforge.c" \
  -o "$tmp_dir/gstforge.o"
cc -shared "$tmp_dir/gstforge.o" \
  -L "$target_dir" -Wl,-rpath,"$target_dir" -lforge_normalizer \
  "${gst_libs[@]}" -o "$tmp_dir/libgstforge.so"

LD_LIBRARY_PATH="$tmp_dir:$target_dir${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
  "$tmp_dir/ffmpeg-bridge-consumer"

GST_PLUGIN_PATH="$tmp_dir" \
GST_REGISTRY_1_0="$tmp_dir/inspect-registry.bin" \
  gst-inspect-1.0 forge_normalizer >"$tmp_dir/gst-inspect.txt"
grep -q 'Forge low-latency loudness gain processor' "$tmp_dir/gst-inspect.txt"
grep -q 'F32LE' "$tmp_dir/gst-inspect.txt"

GST_PLUGIN_PATH="$tmp_dir" \
GST_REGISTRY_1_0="$tmp_dir/launch-registry.bin" \
  gst-launch-1.0 -q \
    audiotestsrc num-buffers=8 wave=sine ! \
    audioconvert ! \
    audio/x-raw,format=F32LE,layout=interleaved,rate=48000,channels=2 ! \
    forge_normalizer gain-db=0 ceiling-dbtp=-1 ! \
    fakesink sync=false

echo "host adapters: OK"
