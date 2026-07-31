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
  forge-compare \
  forge-audio-compare \
  forge-container-qc \
  forge-streaming-qc \
  forge-presentation-qc \
  forge-sadm-qc \
  forge-dialogue-provider \
  forge-provenance-qc \
  forge-imf-qc \
  forge-aes31-qc \
  forge-rtp-qc \
  forge-nmos-qc \
  forge-st2022-7-qc \
  forge-report
do
  cp "target/${target}/release/${binary}" "$staging/"
done

cp "target/${target}/release/libforge_normalizer.so" "$staging/forge-live.clap"
cp "target/${target}/release/libforge_normalizer.so" "$staging/"
cp -R plugins/forge-live.lv2 "$staging/"
cp "target/${target}/release/libforge_normalizer.so" \
  "$staging/forge-live.lv2/forge_live.so"
mkdir "$staging/include"
cp include/forge_normalizer.h "$staging/include/"
cp README.md BATCH-JOBS.md WATCH-FOLDERS.md ANALYSIS-CACHE.md CATALOGUE.md LICENSE "$staging/"

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
