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
cp proto/forge_service.proto "$staging/proto/"
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
cp schema/performance-benchmark-v1.schema.json "$staging/schema/"
cp schema/doctor-report-v1.schema.json "$staging/schema/"
cp schema/analysis-cache-v1.schema.json \
   schema/analysis-cache-v2.schema.json \
   schema/analysis-cache-v3.schema.json \
   schema/analysis-cache-v4.schema.json \
   schema/catalogue-report-v1.schema.json \
   schema/catalogue-report-v2.schema.json "$staging/schema/"
cp schema/batch-job-v1.schema.json \
   schema/batch-job-v2.schema.json \
   schema/batch-progress-v1.schema.json \
   schema/watch-folder-v1.schema.json "$staging/schema/"
cp schema/delivery-manifest-v1.schema.json \
   schema/delivery-manifest-v2.schema.json \
   schema/delivery-manifest-v3.schema.json \
   schema/delivery-manifest-v4.schema.json "$staging/schema/"
cp schema/multi-delivery-request-v1.schema.json \
   schema/multi-delivery-report-v1.schema.json "$staging/schema/"
cp schema/segment-normalization-request-v1.schema.json \
   schema/segment-normalization-plan-v1.schema.json \
   schema/segment-normalization-report-v1.schema.json \
   schema/segment-normalization-plan-v2.schema.json \
   schema/segment-normalization-report-v2.schema.json "$staging/schema/"
cp schema/ac4-adapter-request-v1.schema.json \
   schema/ac4-adapter-response-v1.schema.json \
   schema/ac4-adapter-report-v1.schema.json "$staging/schema/"
cp schema/mpegh-adapter-request-v1.schema.json \
   schema/mpegh-adapter-response-v1.schema.json \
   schema/mpegh-adapter-report-v1.schema.json "$staging/schema/"
cp schema/dts-adapter-request-v1.schema.json \
   schema/dts-adapter-response-v1.schema.json \
   schema/dts-adapter-report-v1.schema.json "$staging/schema/"
cp schema/adm-presentation-report-v1.schema.json \
   schema/adm-interactivity-report-v1.schema.json \
   schema/adm-semantics-report-v1.schema.json "$staging/schema/"
cp schema/downmix-qc-request-v1.schema.json \
   schema/downmix-qc-report-v1.schema.json "$staging/schema/"
cp schema/binaural-qc-request-v1.schema.json \
   schema/binaural-qc-report-v1.schema.json "$staging/schema/"
cp schema/remediation-request-v1.schema.json \
   schema/remediation-report-v1.schema.json "$staging/schema/"
cp schema/metadata-repair-request-v1.schema.json \
   schema/metadata-repair-report-v1.schema.json \
   schema/metadata-repair-request-v2.schema.json \
   schema/metadata-repair-report-v2.schema.json "$staging/schema/"
cp schema/audio-anomaly-provider-v1.schema.json \
   schema/anomaly-provider-audit-v1.schema.json \
   schema/model-qc-v1.schema.json \
   schema/onnx-anomaly-model-v1.schema.json \
   schema/onnx-feature-frames-v1.schema.json \
   schema/remote-range-v1.schema.json \
   schema/remote-materialization-v1.schema.json \
   schema/remote-qc-v1.schema.json \
   schema/service-analysis-v1.schema.json \
   schema/service-analysis-v2.schema.json \
   schema/service-error-v1.schema.json \
   schema/service-health-v1.schema.json "$staging/schema/"
cp schema/ebu-qc-results-v2.schema.json \
   schema/ebu-qc-catalogue-v2-pins.json "$staging/schema/"
cp -R schema/ebu-qc-2026-04 "$staging/schema/"
cp README.md CHANGELOG.md COMPATIBILITY.md CONTRIBUTING.md SECURITY.md DOCUMENTATION.md \
   PERFORMANCE.md BATCH-JOBS.md WATCH-FOLDERS.md ANALYSIS-CACHE.md CATALOGUE.md \
   BENCHMARKS.md MULTI-DELIVERY.md SEGMENT-NORMALIZATION.md AC4-ADAPTER.md \
   MPEGH-ADAPTER.md DTS-ADAPTER.md ADM-PRESENTATION-QC.md ADM-INTERACTIVITY-QC.md ADM-SEMANTICS-QC.md ANOMALY-ADAPTER.md \
   SERVICE-METRICS.md C-API.md HOST-ADAPTERS.md NEXT-GENERATION-PLAN.md \
   VST3-ADAPTER.md AU-ADAPTER.md IMMERSIVE-DOWNMIX.md BINAURAL-QC.md REMEDIATION.md METADATA-REPAIR.md EBU-QC-SCENARIO1.md ROADMAP.md LICENSE "$staging/"

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
