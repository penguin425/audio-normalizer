#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 4 ]]; then
  echo "usage: $0 VERSION TARGET SOURCE_DATE_EPOCH OUTPUT_DIR" >&2
  exit 2
fi

version="$1"
target="$2"
source_date_epoch="$3"
output_dir="$4"

# Keep release names and filesystem arguments in a deliberately small ASCII
# subset.  Apart from making the archive name unambiguous, this prevents an
# argument from becoming a path component or an option to one of the tools
# below.
export LC_ALL=C
if [[ ! "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
  echo "invalid release version: $version" >&2
  exit 2
fi
if [[ ! "$target" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]]; then
  echo "invalid Rust target triple: $target" >&2
  exit 2
fi
if [[ ! "$source_date_epoch" =~ ^[0-9]+$ ]]; then
  echo "invalid SOURCE_DATE_EPOCH: $source_date_epoch" >&2
  exit 2
fi

# Resolve the output directory before changing directory to the repository.
# Relative output arguments have historically been relative to the caller's
# working directory, while all repository inputs below are intentionally
# rooted from this script's own location.
caller_root="$(pwd -P)"
if [[ -z "$output_dir" || "$output_dir" =~ (^|/)\.\.(/|$) ]]; then
  echo "output directory must not contain path traversal: $output_dir" >&2
  exit 2
fi
if [[ "$output_dir" == /* ]]; then
  output_candidate="$output_dir"
else
  output_candidate="${caller_root}/${output_dir}"
fi
if [[ ! -d "$output_candidate" ]]; then
  echo "output directory does not exist: $output_dir" >&2
  exit 2
fi
if ! output_root="$(cd -- "$output_candidate" && pwd -P)"; then
  echo "cannot resolve output directory: $output_dir" >&2
  exit 2
fi
if [[ "$output_root" == "/" ]]; then
  echo "output directory must not be the filesystem root" >&2
  exit 2
fi

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_root="$(cd -- "${script_dir}/.." && pwd -P)"
cd -- "$repo_root"

asset="forge-v${version}-linux-x86_64"
staging="${output_root}/${asset}"

if [[ -e "$staging" || -L "$staging" || -e "${staging}.tar.gz" || -L "${staging}.tar.gz" ]]; then
  echo "refusing to overwrite existing release output: $staging" >&2
  exit 1
fi

mkdir "$staging"
native_library="target/${target}/release/libforge_normalizer.so"
for binary in \
  forge \
  forge-live \
  forge-doctor \
  forge-compare \
  forge-audio-compare \
  forge-container-qc \
  forge-streaming-qc \
  forge-presentation-qc \
  forge-adm-presentation-qc \
  forge-adm-interactivity-qc \
  forge-adm-semantics-qc \
  forge-downmix-qc \
  forge-binaural-qc \
  forge-remediate \
  forge-metadata-repair \
  forge-sadm-qc \
  forge-dialogue-provider \
  forge-anomaly-provider \
  forge-onnx-provider \
  forge-provenance-qc \
  forge-imf-qc \
  forge-aes31-qc \
  forge-rtp-qc \
  forge-nmos-qc \
  forge-st2022-7-qc \
  forge-report \
  forge-multi-delivery \
  forge-segment-normalize \
  forge-ac4-qc \
  forge-mpegh-qc \
  forge-dts-qc \
  forge-remote-qc \
  forge-service
do
  cp -- "target/${target}/release/${binary}" "$staging/"
done

cp -- "$native_library" "$staging/forge-live.clap"
cp -- "$native_library" "$staging/"
mkdir "$staging/lib"
cp -- "$native_library" "$staging/lib/libforge_normalizer.so"
cp -R -- plugins/forge-live.lv2 "$staging/"
cp -- "$native_library" \
  "$staging/forge-live.lv2/forge_live.so"
mkdir "$staging/include" "$staging/proto"
cp -- include/forge_normalizer.h "$staging/include/"
cp -- proto/* "$staging/proto/"
mkdir -p "$staging/integrations/ffmpeg" "$staging/integrations/gstreamer"
cp -- integrations/ffmpeg/forge_ffmpeg_bridge.c \
   integrations/ffmpeg/forge_ffmpeg_bridge.h "$staging/integrations/ffmpeg/"
cp -- integrations/gstreamer/gstforge.c "$staging/integrations/gstreamer/"
mkdir -p "$staging/integrations/au"
cp -- integrations/au/CMakeLists.txt integrations/au/au-info.plist \
   "$staging/integrations/au/"
mkdir -p "$staging/integrations/vst3/external/vst3sdk"
cp -- integrations/vst3/CMakeLists.txt integrations/vst3/*.h \
   integrations/vst3/*.cpp "$staging/integrations/vst3/"
cp -- integrations/vst3/external/CMakeLists.txt \
   "$staging/integrations/vst3/external/"
cp -- integrations/vst3/external/vst3sdk/CMakeLists.txt \
   "$staging/integrations/vst3/external/vst3sdk/"
mkdir "$staging/tools" "$staging/schema"
cp -- tools/benchmark.py tools/build-pgo-forge.sh tools/train-pgo.py \
   tools/canonicalize-pgo-profile.py tools/package-linux-v3-release.sh \
   tools/test-vst3-adapter.sh tools/test-au-adapter.sh "$staging/tools/"
# Every released binary shares this schema directory. Copying the complete
# top-level set prevents one platform or a newly versioned report from being
# silently omitted from an archive.
cp -- schema/*.json "$staging/schema/"
cp -R -- schema/ebu-qc-2026-04 "$staging/schema/"
cp -- ./*.md LICENSE "$staging/"
python3 tools/native_package_metadata.py \
  --root "$staging" \
  --version "$version" \
  --platform linux
python3 tools/check-release-content.py \
  --native-platform linux \
  --version "$version" \
  "$staging"

find "$staging" -exec touch -h -d "@${source_date_epoch}" {} +
tar \
  --sort=name \
  --mtime="@${source_date_epoch}" \
  --owner=0 \
  --group=0 \
  --numeric-owner \
  -C "$output_root" \
  -cf - "$asset" |
  gzip -n >"${staging}.tar.gz"
