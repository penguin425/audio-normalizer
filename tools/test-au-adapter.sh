#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "Audio Unit adapter: SKIP (macOS only)"
  exit 0
fi

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target_dir="${FORGE_TARGET_DIR:-${root_dir}/target/release}"
library_name="${FORGE_NORMALIZER_LIBRARY_NAME:-libforge_normalizer.dylib}"
library_path="${FORGE_NORMALIZER_LIBRARY:-${target_dir}/${library_name}}"
tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/forge-au-adapter.XXXXXX")"

cleanup() {
  if [[ -d "$tmp_dir" ]]; then
    find "$tmp_dir" -depth -delete
  fi
}
trap cleanup EXIT

for command_name in cargo cmake xcodebuild xcrun git; do
  if ! command -v "$command_name" >/dev/null 2>&1; then
    echo "missing required Audio Unit adapter command: $command_name" >&2
    exit 1
  fi
done

xcodebuild -version
xcrun --sdk macosx --show-sdk-path >/dev/null

if [[ "${FORGE_SKIP_CARGO_BUILD:-0}" != "1" ]]; then
  # ring's aarch64-apple-darwin build intentionally requires the baseline
  # AES/NEON/SHA2 feature set.  The repository's native-CPU Cargo config can
  # add newer features (notably SHA3), so use the same conservative flags as
  # the macOS ABI CI job unless the caller supplied RUSTFLAGS explicitly.
  if [[ -z "${RUSTFLAGS+x}" && "$(uname -m)" == "arm64" ]]; then
    RUSTFLAGS="-Ctarget-feature=+neon,+aes,+sha2" \
      cargo build --locked --manifest-path "$root_dir/Cargo.toml" --release --lib
  else
    cargo build --locked --manifest-path "$root_dir/Cargo.toml" --release --lib
  fi
fi

if [[ ! -f "$library_path" ]]; then
  echo "Forge shared library was not found at $library_path" >&2
  echo "build the library or set FORGE_NORMALIZER_LIBRARY" >&2
  exit 1
fi

cmake -S "$root_dir/integrations/au" -B "$tmp_dir/build" -G Xcode \
  -DCMAKE_OSX_DEPLOYMENT_TARGET="${FORGE_AU_DEPLOYMENT_TARGET:-10.13}" \
  -DCMAKE_OSX_ARCHITECTURES="${FORGE_AU_ARCHITECTURES:-$(uname -m)}" \
  -DFORGE_NORMALIZER_LIBRARY="$library_path" \
  -DFORGE_NORMALIZER_INCLUDE_DIR="$root_dir/include"
cmake --build "$tmp_dir/build" --config Release \
  --target ForgeLiveAu --parallel "${FORGE_BUILD_JOBS:-2}"

bundle="$(find "$tmp_dir/build" -type d -name 'ForgeLive.component' -print -quit)"
if [[ -z "$bundle" || ! -d "$bundle" ]]; then
  echo "Audio Unit component was not produced" >&2
  exit 1
fi

if [[ ! -f "$bundle/Contents/Info.plist" ]]; then
  echo "Audio Unit Info.plist was not found in $bundle" >&2
  exit 1
fi
if ! plutil -lint "$bundle/Contents/Info.plist" >/dev/null; then
  echo "Audio Unit Info.plist is invalid: $bundle/Contents/Info.plist" >&2
  exit 1
fi

plugin_link="$bundle/Contents/Resources/plugin.vst3"
if [[ ! -e "$plugin_link" ]]; then
  echo "Audio Unit wrapper does not contain its VST3 resource: $plugin_link" >&2
  exit 1
fi

au_binary="$(find "$bundle/Contents/MacOS" -type f -print -quit)"
if [[ -z "$au_binary" || ! -s "$au_binary" ]]; then
  echo "Audio Unit binary was not found in $bundle" >&2
  exit 1
fi

echo "Audio Unit adapter: OK ($bundle)"
