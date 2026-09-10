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
npm_repro_dir="$(mktemp -d)"
trap 'find "$staging" -depth -delete; find "$npm_repro_dir" -depth -delete' EXIT
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
output_dir="$(cd "$output_dir" && pwd -P)"

# npm pack intentionally runs against the isolated staging tree.  This keeps
# the registry tarball independent of the checkout (and prevents generated
# files or a pre-existing output tarball from being included).  npm normalizes
# tar metadata, so invoking this helper twice with the same source and
# SOURCE_DATE_EPOCH produces byte-identical .tgz files.
npm_json="$(
  cd "$staging"
  npm pack --ignore-scripts --json --pack-destination "$output_dir"
)"
npm_filename="$(python3 -c '
import json
import sys

items = json.loads(sys.stdin.read())
if not isinstance(items, list) or len(items) != 1:
    raise SystemExit("npm pack returned an unexpected result")
filename = items[0].get("filename")
if not isinstance(filename, str) or not filename:
    raise SystemExit("npm pack did not report a filename")
print(filename)
' <<<"$npm_json")"
expected_npm_filename="forge-normalizer-wasm-${version}.tgz"
if [[ "$npm_filename" != "$expected_npm_filename" ]]; then
  echo "npm pack produced ${npm_filename}; expected ${expected_npm_filename}" >&2
  exit 1
fi
npm_archive="$output_dir/$npm_filename"
if [[ ! -f "$npm_archive" ]]; then
  echo "npm pack did not create $npm_archive" >&2
  exit 1
fi

# Exercise npm pack a second time in an isolated destination and compare the
# bytes.  This catches a toolchain that accidentally reintroduces wall-clock
# mtimes or host-specific tar metadata even when one invocation looks valid.
npm_repro_json="$(
  cd "$staging"
  npm pack --ignore-scripts --json --pack-destination "$npm_repro_dir"
)"
npm_repro_filename="$(python3 -c '
import json
import sys

items = json.loads(sys.stdin.read())
if not isinstance(items, list) or len(items) != 1:
    raise SystemExit("reproducibility npm pack returned an unexpected result")
filename = items[0].get("filename")
if not isinstance(filename, str) or not filename:
    raise SystemExit("reproducibility npm pack did not report a filename")
print(filename)
' <<<"$npm_repro_json")"
if [[ "$npm_repro_filename" != "$npm_filename" ]] ||
  ! cmp -s "$npm_archive" "$npm_repro_dir/$npm_repro_filename"; then
  echo "npm pack is not reproducible for ${npm_filename}" >&2
  exit 1
fi

# Keep the exact package-member and metadata policy in one read-only checker;
# this also catches accidental npm defaults such as a private package or a
# changed repository URL before the artifact reaches a publisher job.
python3 tools/verify-registry-artifacts.py \
  --verify-npm "$npm_archive" \
  --version "$version" >/dev/null

archive="$output_dir/forge-v${version}-wasm-web.tar.gz"
tar --sort=name --mtime="@${SOURCE_DATE_EPOCH:-0}" --owner=0 --group=0 \
  --numeric-owner -czf "$archive" -C "$staging" .
printf '%s\n' "$archive"
printf '%s\n' "$npm_archive"
