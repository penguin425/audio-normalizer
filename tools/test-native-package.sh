#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 ROOT EXPECTED_VERSION" >&2
  exit 2
fi

root_input="$1"
expected_version="$2"

die() {
  echo "native package test: $*" >&2
  exit 1
}

if [[ -z "$expected_version" ||
      ! "$expected_version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
  die "EXPECTED_VERSION must use numeric MAJOR.MINOR.PATCH without leading zeros: $expected_version"
fi

if [[ ! -d "$root_input" ]]; then
  die "ROOT is not an existing directory: $root_input"
fi
root_abs="$(cd -- "$root_input" && pwd -P)"
if [[ "$root_abs" == "/" ]]; then
  die "ROOT must not be a filesystem root"
fi

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_root="$(cd -- "$script_dir/.." && pwd -P)"
fixture_dir="$repo_root/tests/fixtures/native_package"

for command_name in cmake pkg-config python3 cc; do
  if ! command -v "$command_name" >/dev/null 2>&1; then
    die "missing required command: $command_name"
  fi
done
cc_path="$(command -v cc)"

temporary=""
cleanup() {
  if [[ -n "$temporary" && -d "$temporary" ]]; then
    find "$temporary" -depth -delete
  fi
}
trap cleanup EXIT

temporary_parent="$(cd -- "${TMPDIR:-/tmp}" && pwd -P)"
temporary="$(mktemp -d "$temporary_parent/forge-native-package.XXXXXX")"
case "$temporary" in
  "$temporary_parent"/forge-native-package.*) ;;
  *) die "mktemp returned an unexpected path: $temporary" ;;
esac
prefix="$temporary/forge native package-日本語"
mkdir -p "$prefix/include" "$prefix/lib"
# Only copy the C API payload needed by these consumers.  `cp -R -P` is
# available on both GNU and BSD systems and does not require GNU `cp -a`.
cp "$root_abs/include/forge_normalizer.h" "$prefix/include/"
cp -R -P "$root_abs/lib"/. "$prefix/lib/"
if [[ "$(uname -s)" == "Windows_NT" ]]; then
  mkdir -p "$prefix/bin"
  cp "$root_abs/bin/forge_normalizer.dll" "$prefix/bin/"
fi

cmake_config_dir="$prefix/lib/cmake/ForgeNormalizer"
pkgconfig_dir="$prefix/lib/pkgconfig"
cmake_config="$cmake_config_dir/ForgeNormalizerConfig.cmake"
cmake_version_config="$cmake_config_dir/ForgeNormalizerConfigVersion.cmake"
pkgconfig_file="$pkgconfig_dir/forge-normalizer.pc"

for metadata_file in "$cmake_config" "$cmake_version_config" "$pkgconfig_file"; do
  [[ -f "$metadata_file" ]] || die "missing package metadata: $metadata_file"
done

# A relocated package must not retain the path of either the original staged
# prefix or this source checkout.  Keep this check byte-oriented so it does not
# depend on the locale chosen by a caller.
metadata_files=()
while IFS= read -r -d '' metadata_file; do
  metadata_files+=("$metadata_file")
done < <(find "$cmake_config_dir" "$pkgconfig_dir" -type f -print0)
for metadata_file in "${metadata_files[@]}"; do
  if LC_ALL=C grep -aFq -- "$root_abs" "$metadata_file"; then
    die "metadata retains original prefix $root_abs: $metadata_file"
  fi
  if LC_ALL=C grep -aFq -- "$repo_root" "$metadata_file"; then
    die "metadata retains workspace path $repo_root: $metadata_file"
  fi
  if LC_ALL=C grep -aEq -- '@[A-Za-z_][A-Za-z0-9_]*@' "$metadata_file"; then
    die "metadata contains an unexpanded template token: $metadata_file"
  fi
done

cmake_build="$temporary/cmake-build"
cmake \
  -S "$fixture_dir" \
  -B "$cmake_build" \
  -DCMAKE_PREFIX_PATH="$prefix" \
  -DFORGE_EXPECTED_VERSION="$expected_version" \
  -DFORGE_TEST_PREFIX="$prefix" \
  -DCMAKE_SKIP_RPATH=ON \
  -DCMAKE_BUILD_TYPE=Release
cmake --build "$cmake_build" --config Release --target native-package-cmake-consumer

cmake_consumer=""
while IFS= read -r -d '' candidate; do
  if [[ -x "$candidate" ]]; then
    cmake_consumer="$candidate"
    break
  fi
done < <(find "$cmake_build" -type f -name 'native-package-cmake-consumer' -print0)
[[ -n "$cmake_consumer" ]] || die "CMake consumer executable was not produced"

run_with_library_path() {
  case "$(uname -s)" in
    Linux)
      LD_LIBRARY_PATH="$prefix/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" "$@"
      ;;
    Darwin)
      DYLD_LIBRARY_PATH="$prefix/lib${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}" "$@"
      ;;
    *)
      die "unsupported Unix host: $(uname -s)"
      ;;
  esac
}

run_with_library_path "$cmake_consumer"

pkgconfig_path="$pkgconfig_dir"
if [[ -n "${PKG_CONFIG_PATH:-}" ]]; then
  pkgconfig_path+=":${PKG_CONFIG_PATH}"
fi
pkgconfig_consumer="$temporary/pkgconfig-consumer"
if ! python3 - "$pkgconfig_path" "$prefix" \
    "$fixture_dir/pkgconfig-consumer.c" "$pkgconfig_consumer" \
    "$cc_path" "$expected_version" <<'PY'
import os
import shlex
import subprocess
import sys
from pathlib import Path


pkgconfig_path, prefix_text, source, output, cc_path, expected_version = sys.argv[1:]
prefix = Path(prefix_text).resolve(strict=True)
environment = os.environ.copy()
environment["PKG_CONFIG_PATH"] = pkgconfig_path


def pkg_config(*arguments: str) -> str:
    result = subprocess.run(
        ["pkg-config", *arguments],
        env=environment,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    # pkgconf (the implementation shipped by several BSD/macOS package
    # managers) may put a backslash in front of each byte of a non-ASCII path
    # while rendering compiler flags.  Remove that quoting only for bytes
    # outside ASCII before decoding; retain shell quoting such as ``\ `` so
    # shlex.split below still keeps paths containing spaces as one argument.
    raw = result.stdout
    unquoted = bytearray()
    index = 0
    while index < len(raw):
        if (
            raw[index] == ord("\\")
            and index + 1 < len(raw)
            and raw[index + 1] >= 0x80
        ):
            index += 1
            continue
        unquoted.append(raw[index])
        index += 1
    try:
        return bytes(unquoted).decode("utf-8").strip()
    except UnicodeDecodeError as error:
        raise SystemExit(f"pkg-config emitted non-UTF-8 output: {error}") from error


version = pkg_config("--modversion", "forge-normalizer")
if version != expected_version:
    raise SystemExit(
        f"pkg-config version is {version!r}, expected {expected_version!r}"
    )


def resolved_directory(value: str) -> Path:
    try:
        directory_tokens = shlex.split(value)
    except ValueError as error:
        raise SystemExit(f"pkg-config emitted malformed directory: {error}") from error
    if len(directory_tokens) != 1:
        raise SystemExit(f"pkg-config emitted an invalid directory: {value!r}")
    path = Path(directory_tokens[0]).resolve(strict=True)
    if not path.is_dir():
        raise SystemExit(f"pkg-config path is not a directory: {value}")
    return path


include_dir = resolved_directory(
    pkg_config("--variable=includedir", "forge-normalizer")
)
library_dir = resolved_directory(
    pkg_config("--variable=libdir", "forge-normalizer")
)
if include_dir != prefix / "include":
    raise SystemExit(f"pkg-config includedir escaped moved prefix: {include_dir}")
if library_dir != prefix / "lib":
    raise SystemExit(f"pkg-config libdir escaped moved prefix: {library_dir}")

try:
    flags = shlex.split(pkg_config("--cflags", "--libs", "forge-normalizer"))
except ValueError as error:
    raise SystemExit(f"pkg-config emitted malformed flags: {error}") from error
if not flags:
    raise SystemExit("pkg-config returned no compiler/linker flags")


def flag_values(short_option: str) -> list[str]:
    values: list[str] = []
    index = 0
    while index < len(flags):
        flag = flags[index]
        if flag == short_option:
            index += 1
            if index >= len(flags):
                raise SystemExit(f"pkg-config emitted a dangling {short_option}")
            values.append(flags[index])
        elif flag.startswith(short_option) and len(flag) > len(short_option):
            values.append(flag[len(short_option) :])
        index += 1
    return values


include_flags = [Path(value).resolve(strict=True) for value in flag_values("-I")]
library_flags = [Path(value).resolve(strict=True) for value in flag_values("-L")]
if prefix / "include" not in include_flags:
    raise SystemExit("pkg-config flags omit the moved include prefix")
if prefix / "lib" not in library_flags:
    raise SystemExit("pkg-config flags omit the moved library prefix")
if "-lforge_normalizer" not in flags:
    raise SystemExit("pkg-config flags omit -lforge_normalizer")
if any(
    flag == "-Wl,-rpath" or flag.startswith("-Wl,-rpath,")
    for flag in flags
):
    raise SystemExit("pkg-config metadata injects a fixed runtime path")

subprocess.run(
    [
        cc_path,
        "-std=c11",
        "-Wall",
        "-Wextra",
        "-Werror",
        source,
        *flags,
        "-o",
        output,
    ],
    check=True,
)
PY
then
  die "pkg-config consumer build failed"
fi
run_with_library_path "$pkgconfig_consumer"

echo "native package: OK ($prefix)"
