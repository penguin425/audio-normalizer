#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 4 ]]; then
  echo "usage: $0 VERSION FORGE_BINARY SOURCE_DATE_EPOCH OUTPUT_DIR" >&2
  exit 2
fi

version="$1"
forge_binary="$2"
source_date_epoch="$3"
output_dir="$4"
asset="forge-v${version}-linux-x86_64-v3"
staging="${output_dir}/${asset}"

if [[ ! -f "$forge_binary" || ! -x "$forge_binary" ]]; then
  echo "x86-64-v3 Forge binary is not executable: $forge_binary" >&2
  exit 1
fi
if [[ -e "$staging" || -e "${staging}.tar.gz" ]]; then
  echo "refusing to overwrite existing release output: $staging" >&2
  exit 1
fi

mkdir -p "$staging"
cp "$forge_binary" "$staging/forge"
cp README.md CHANGELOG.md COMPATIBILITY.md SECURITY.md PERFORMANCE.md LICENSE "$staging/"

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
