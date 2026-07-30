#!/usr/bin/env bash
set -euo pipefail

version="${1:?usage: build-wasm-package.sh VERSION OUTPUT_DIR}"
output_dir="${2:?usage: build-wasm-package.sh VERSION OUTPUT_DIR}"
package_version="$(awk -F'"' '$1 == "version = " { print $2; exit }' wasm/Cargo.toml)"
if [[ "$package_version" != "$version" ]]; then
  echo "WASM package version ${package_version} does not match ${version}" >&2
  exit 1
fi

cargo build --locked --manifest-path wasm/Cargo.toml --target wasm32-unknown-unknown --release

staging="$(mktemp -d)"
trap 'find "$staging" -depth -delete' EXIT
wasm-bindgen \
  --target web \
  --typescript \
  --out-dir wasm/package \
  "wasm/target/wasm32-unknown-unknown/release/forge_normalizer_wasm.wasm"
cp wasm/package/index.js wasm/package/index.d.ts wasm/package/package.json \
  wasm/package/README.md wasm/package/forge_normalizer_wasm.js \
  wasm/package/forge_normalizer_wasm.d.ts \
  wasm/package/forge_normalizer_wasm_bg.wasm \
  wasm/package/forge_normalizer_wasm_bg.wasm.d.ts LICENSE "$staging/"

python3 - "$staging/package.json" "$version" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
data = json.loads(path.read_text())
data["version"] = sys.argv[2]
path.write_text(json.dumps(data, indent=2) + "\n")
PY

mkdir -p "$output_dir"
archive="$output_dir/forge-v${version}-wasm-web.tar.gz"
tar --sort=name --mtime="@${SOURCE_DATE_EPOCH:-0}" --owner=0 --group=0 \
  --numeric-owner -czf "$archive" -C "$staging" .
printf '%s\n' "$archive"
