#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 TARGET" >&2
  exit 2
fi

target="$1"
target_dir="target/${target}/debug"
temporary="$(mktemp -d /tmp/forge-c-api.XXXXXX)"
trap 'find "$temporary" -depth -delete' EXIT

cargo build --locked --target "$target" --lib

case "$(uname -s)" in
  Darwin)
    library="libforge_normalizer.dylib"
    runtime_variable="DYLD_LIBRARY_PATH"
    ;;
  Linux)
    library="libforge_normalizer.so"
    runtime_variable="LD_LIBRARY_PATH"
    ;;
  *)
    echo "unsupported Unix host for C API test" >&2
    exit 2
    ;;
esac

test -f "${target_dir}/${library}"
cc \
  -std=c11 \
  -Wall \
  -Wextra \
  -Werror \
  -I include \
  tests/fixtures/c_api_consumer.c \
  -L "$target_dir" \
  -lforge_normalizer \
  -o "${temporary}/c-api-consumer"

env "${runtime_variable}=${target_dir}" "${temporary}/c-api-consumer"
