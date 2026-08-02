#!/usr/bin/env bash
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target_dir="${FORGE_TARGET_DIR:-${root_dir}/target/debug}"
library_name="${FORGE_NORMALIZER_LIBRARY_NAME:-libforge_normalizer.so}"
library_path="${FORGE_NORMALIZER_LIBRARY:-${target_dir}/${library_name}}"
tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/forge-vst3-adapter.XXXXXX")"

cleanup() {
  if [[ -d "$tmp_dir" ]]; then
    find "$tmp_dir" -depth -delete
  fi
}
trap cleanup EXIT

for command_name in cargo cmake c++ git; do
  if ! command -v "$command_name" >/dev/null 2>&1; then
    echo "missing required VST3 adapter command: $command_name" >&2
    exit 1
  fi
done

if [[ "${FORGE_SKIP_CARGO_BUILD:-0}" != "1" ]]; then
  cargo build --locked --manifest-path "$root_dir/Cargo.toml" --lib
fi

if [[ ! -f "$library_path" ]]; then
  echo "Forge shared library was not found at $library_path" >&2
  echo "build the library or set FORGE_NORMALIZER_LIBRARY" >&2
  exit 1
fi

generator_args=()
if [[ -n "${FORGE_CMAKE_GENERATOR:-}" ]]; then
  generator_args=(-G "$FORGE_CMAKE_GENERATOR")
elif command -v make >/dev/null 2>&1; then
  # Unix Makefiles are available on all supported Linux CI runners and avoid
  # requiring Ninja merely to compile this optional adapter.
  generator_args=(-G "Unix Makefiles")
fi

cmake -S "$root_dir/integrations/vst3" -B "$tmp_dir/build" \
  "${generator_args[@]}" \
  -DCMAKE_BUILD_TYPE=Release \
  -DFORGE_NORMALIZER_LIBRARY="$library_path" \
  -DFORGE_NORMALIZER_INCLUDE_DIR="$root_dir/include"
cmake --build "$tmp_dir/build" --parallel "${FORGE_BUILD_JOBS:-2}"

bundle="$(find "$tmp_dir/build/VST3" -type d -name 'ForgeLiveVst3.vst3' -print -quit)"
if [[ -z "$bundle" || ! -d "$bundle" ]]; then
  echo "VST3 bundle was not produced" >&2
  exit 1
fi

plugin_binary="$(find "$bundle/Contents" -type f \( -name 'ForgeLiveVst3.so' -o -name 'ForgeLiveVst3.dylib' -o -name 'ForgeLiveVst3.vst3' \) -print -quit)"
if [[ -z "$plugin_binary" || ! -s "$plugin_binary" ]]; then
  echo "VST3 plugin binary was not found in $bundle" >&2
  exit 1
fi

if [[ "$(uname -s)" == "Linux" ]] && command -v nm >/dev/null 2>&1; then
  nm -D "$plugin_binary" | grep -Eq '(^|[[:space:]])T?GetPluginFactory$' || {
    echo "VST3 plugin does not export GetPluginFactory: $plugin_binary" >&2
    exit 1
  }
fi

echo "VST3 adapter: OK ($bundle)"
