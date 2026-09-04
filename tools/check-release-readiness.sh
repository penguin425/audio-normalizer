#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

expected_version="${1:-}"
python3 tools/check-workflow-pins.py
IFS=$'\t' read -r version target_dir < <(
  python3 - "$expected_version" <<'PY'
import json
import pathlib
import subprocess
import sys

expected_version = sys.argv[1]
metadata = json.loads(
    subprocess.check_output(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
        text=True,
    )
)
packages = [item for item in metadata["packages"] if item["name"] == "forge-normalizer"]
if len(packages) != 1:
    raise SystemExit(f"expected one forge-normalizer package, found {len(packages)}")
package = packages[0]
version = package["version"]
if expected_version and version != expected_version:
    raise SystemExit(
        f"Cargo package version {version} does not match expected version {expected_version}"
    )

required = {
    "description": "Forge: a SIMD-accelerated EBU R128 / ITU-R BS.1770-5 loudness normalizer (WAV/MP3/FLAC/Opus/AAC/Vorbis)",
    "license": "MIT",
    "repository": "https://github.com/penguin425/audio-normalizer",
    "homepage": "https://penguin425.github.io/audio-normalizer/",
    "documentation": "https://docs.rs/forge-normalizer",
}
for field, expected in required.items():
    actual = package.get(field)
    if actual != expected:
        raise SystemExit(f"Cargo package {field} is {actual!r}; expected {expected!r}")

readme = package.get("readme")
if not readme or pathlib.Path(readme).name != "README.md":
    raise SystemExit("Cargo package readme must reference README.md")

keywords = set(package.get("keywords", []))
required_keywords = {"audio", "loudness", "lufs", "ebu-r128", "normalization"}
if keywords != required_keywords:
    raise SystemExit(
        f"Cargo package keywords are {sorted(keywords)!r}; expected {sorted(required_keywords)!r}"
    )

categories = set(package.get("categories", []))
required_categories = {"command-line-utilities", "multimedia::audio"}
if categories != required_categories:
    raise SystemExit(
        f"Cargo package categories are {sorted(categories)!r}; expected {sorted(required_categories)!r}"
    )

print(f"{version}\t{metadata['target_directory']}")
PY
)

package_args=(--locked --no-verify)
if [[ "${FORGE_ALLOW_DIRTY_PACKAGE:-0}" == "1" ]]; then
  package_args+=(--allow-dirty)
fi
(cd schema/ebu-qc-2026-04 && sha256sum --check SHA256SUMS)
cargo package "${package_args[@]}"

crate="$target_dir/package/forge-normalizer-${version}.crate"
if [[ ! -f "$crate" ]]; then
  echo "cargo package did not create $crate" >&2
  exit 1
fi

crate_bytes="$(wc -c <"$crate")"
max_crate_bytes=$((10 * 1024 * 1024))
if ((crate_bytes > max_crate_bytes)); then
  echo "crate is ${crate_bytes} bytes, above the ${max_crate_bytes}-byte registry limit" >&2
  exit 1
fi

prefix="forge-normalizer-${version}"
for file in README.md LICENSE CHANGELOG.md COMPATIBILITY.md SECURITY.md CONTRIBUTING.md; do
  if ! tar -tzf "$crate" "$prefix/$file" >/dev/null; then
    echo "crate is missing $file" >&2
    exit 1
  fi
done
for file in \
  ANALYSIS-CACHE.md \
  CATALOGUE.md \
  EBU-QC-SCENARIO1.md \
  schema/analysis-cache-v4.schema.json \
  schema/catalogue-report-v2.schema.json \
  schema/remote-materialization-v1.schema.json \
  schema/ebu-qc-results-v2.schema.json \
  schema/ebu-qc-catalogue-v2-pins.json \
  schema/ebu-qc-2026-04/README.md \
  schema/ebu-qc-2026-04/LICENSE.md \
  schema/ebu-qc-2026-04/SHA256SUMS \
  schema/ebu-qc-2026-04/forge-validation.xsd \
  schema/ebu-qc-2026-04/qc-data-model/qc.xsd \
  schema/ebu-qc-2026-04/qc-data-model/TimingExtensionMediaPlaybackEditUnits.xsd \
  schema/ebu-qc-2026-04/qc-catalogue-api/qc-catalogue-api-schema.xsd \
  schema/ebu-qc-2026-04/qc-reports/qc-report-generic-sample.xml
do
  if ! tar -tzf "$crate" "$prefix/$file" >/dev/null; then
    echo "crate is missing $file" >&2
    exit 1
  fi
done

echo "registry package ready: $crate (${crate_bytes} bytes)"
