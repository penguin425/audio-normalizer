#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 TARGET" >&2
  exit 2
fi

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target="$1"
pgo_root="${FORGE_PGO_ROOT:-${root_dir}/target/forge-pgo-${target}}"
profile_dir="${pgo_root}/profiles"
instrumented_target="${pgo_root}/instrumented"
optimized_target="${pgo_root}/optimized"
training_report="${pgo_root}/training.json"
features="${FORGE_PGO_FEATURES:-}"
base_rustflags="${FORGE_PGO_RUSTFLAGS:-}"
duration="${FORGE_PGO_DURATION_SECONDS:-12}"

mkdir -p "$profile_dir"
if find "$profile_dir" -mindepth 1 -maxdepth 1 -print -quit | grep -q .; then
  echo "PGO profile directory must be empty: $profile_dir" >&2
  echo "use a new FORGE_PGO_ROOT or clean its Cargo target explicitly" >&2
  exit 1
fi

host="$(rustc -vV | sed -n 's/^host: //p')"
sysroot="$(rustc --print sysroot)"
llvm_profdata="${sysroot}/lib/rustlib/${host}/bin/llvm-profdata"
if [[ ! -x "$llvm_profdata" && -x "${llvm_profdata}.exe" ]]; then
  llvm_profdata="${llvm_profdata}.exe"
fi
if [[ ! -x "$llvm_profdata" ]]; then
  echo "matching llvm-profdata was not found; install rustup component llvm-tools-preview" >&2
  exit 1
fi

cargo_args=(
  build
  --locked
  --release
  --target "$target"
  --bin forge
)
if [[ -n "$features" ]]; then
  cargo_args+=(--features "$features")
fi

generate_flags="${base_rustflags:+${base_rustflags} }-Cprofile-generate=${profile_dir}"
CARGO_TARGET_DIR="$instrumented_target" \
RUSTFLAGS="$generate_flags" \
  cargo "${cargo_args[@]}"

executable_name="forge"
if [[ "$target" == *windows* ]]; then
  executable_name="forge.exe"
fi
instrumented_forge="${instrumented_target}/${target}/release/${executable_name}"
python3 "$root_dir/tools/train-pgo.py" \
  --forge "$instrumented_forge" \
  --profile-dir "$profile_dir" \
  --duration-seconds "$duration" \
  --output "$training_report"

shopt -s nullglob
raw_profiles=("$profile_dir"/*.profraw)
shopt -u nullglob
if (( ${#raw_profiles[@]} == 0 )); then
  echo "PGO training produced no raw profiles" >&2
  exit 1
fi
raw_profile="${pgo_root}/raw.profdata"
"$llvm_profdata" merge \
  --failure-mode=all \
  --output="$raw_profile" \
  "${raw_profiles[@]}"
text_profile="${pgo_root}/raw-profile.txt"
canonical_profile="${pgo_root}/canonical-profile.txt"
"$llvm_profdata" merge --text --output="$text_profile" "$raw_profile"
python3 "$root_dir/tools/canonicalize-pgo-profile.py" \
  --input "$text_profile" \
  --output "$canonical_profile"
merged_profile="${pgo_root}/merged.profdata"
"$llvm_profdata" merge --output="$merged_profile" "$canonical_profile"
"$llvm_profdata" show --detailed-summary "$merged_profile"

use_flags="${base_rustflags:+${base_rustflags} }-Cprofile-use=${merged_profile} -Cllvm-args=-pgo-warn-missing-function"
CARGO_TARGET_DIR="$optimized_target" \
RUSTFLAGS="$use_flags" \
  cargo "${cargo_args[@]}"

optimized_forge="${optimized_target}/${target}/release/${executable_name}"
if [[ ! -x "$optimized_forge" ]]; then
  echo "PGO-optimized Forge was not created: $optimized_forge" >&2
  exit 1
fi
printf '%s\n' "$optimized_forge"
