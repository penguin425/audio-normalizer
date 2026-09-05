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
asset="forge-v${version}-linux-x86_64"
staging="${output_dir}/${asset}"

if [[ -e "$staging" || -e "${staging}.tar.gz" ]]; then
  echo "refusing to overwrite existing release output: $staging" >&2
  exit 1
fi

mkdir -p "$staging"
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
  cp "target/${target}/release/${binary}" "$staging/"
done

cp "target/${target}/release/libforge_normalizer.so" "$staging/forge-live.clap"
cp "target/${target}/release/libforge_normalizer.so" "$staging/"
cp -R plugins/forge-live.lv2 "$staging/"
cp "target/${target}/release/libforge_normalizer.so" \
  "$staging/forge-live.lv2/forge_live.so"
mkdir "$staging/include" "$staging/proto"
cp include/forge_normalizer.h "$staging/include/"
cp proto/* "$staging/proto/"
mkdir -p "$staging/integrations/ffmpeg" "$staging/integrations/gstreamer"
cp integrations/ffmpeg/forge_ffmpeg_bridge.c \
   integrations/ffmpeg/forge_ffmpeg_bridge.h "$staging/integrations/ffmpeg/"
cp integrations/gstreamer/gstforge.c "$staging/integrations/gstreamer/"
mkdir -p "$staging/integrations/au"
cp integrations/au/CMakeLists.txt integrations/au/au-info.plist \
   "$staging/integrations/au/"
mkdir -p "$staging/integrations/vst3/external/vst3sdk"
cp integrations/vst3/CMakeLists.txt integrations/vst3/*.h \
   integrations/vst3/*.cpp "$staging/integrations/vst3/"
cp integrations/vst3/external/CMakeLists.txt \
   "$staging/integrations/vst3/external/"
cp integrations/vst3/external/vst3sdk/CMakeLists.txt \
   "$staging/integrations/vst3/external/vst3sdk/"
mkdir "$staging/tools" "$staging/schema"
cp tools/benchmark.py tools/build-pgo-forge.sh tools/train-pgo.py \
   tools/canonicalize-pgo-profile.py tools/package-linux-v3-release.sh \
   tools/test-vst3-adapter.sh tools/test-au-adapter.sh "$staging/tools/"
# Every released binary shares this schema directory. Copying the complete
# top-level set prevents one platform or a newly versioned report from being
# silently omitted from an archive.
cp schema/*.json "$staging/schema/"
cp -R schema/ebu-qc-2026-04 "$staging/schema/"
cp ./*.md LICENSE "$staging/"
python3 tools/check-release-content.py "$staging"

find "$staging" -exec touch -h -d "@${source_date_epoch}" {} +
tar \
  --sort=name \
  --mtime="@${source_date_epoch}" \
  --owner=0 \
  --group=0 \
  --numeric-owner \
  -C "$output_dir" \
  -cf - "$asset" |
  gzip -n >"${staging}.tar.gz"
