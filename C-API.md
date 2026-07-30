# Forge C ABI v1

Forge release archives contain a versioned C interface for bounded local-file
loudness analysis:

- `include/forge_normalizer.h`;
- `libforge_normalizer.so` on Linux;
- `libforge_normalizer.dylib` on macOS; and
- `forge_normalizer.dll` plus `forge_normalizer.lib` on Windows.

The normative public declaration is
[`include/forge_normalizer.h`](include/forge_normalizer.h).

## Compatibility

C ABI major version 1 starts with Forge v0.96.0. Existing v1 functions,
integer status values, structure size, field order, field types, and field
units will not change within ABI major 1. New functions may be added. A future
incompatible interface must use new symbol and type names and increment
`forge_normalizer_c_api_version()`.

CLAP and LV2 plugin entry points are separate host ABIs and are not part of
this contract.

## Ownership and safety

- The caller owns every input and output buffer.
- Forge returns no allocation that a caller must free.
- Paths are NUL-terminated UTF-8, including on Windows.
- The result must be aligned for `ForgeAnalysisV1` and writable for the
  advertised size for the duration of the call.
- The optional error buffer must be writable for its advertised capacity and
  must not overlap either the path string or the result.
- `max_decoded_samples` bounds decoded frames multiplied by channels and must
  be greater than zero.
- Error text is UTF-8, always NUL-terminated when capacity is positive, may be
  truncated on a character boundary, and is empty on success.
- Calls are independent and may run concurrently.
- The interface uses the non-unwinding C calling convention. An unexpected
  Rust panic does not unwind into C; release builds use `panic=abort`.

`ForgeAnalysisV1` is exactly 80 bytes on supported 64-bit release targets.
The header and Rust implementation both assert its size and key offsets.
Callers should still pass `forge_normalizer_analysis_v1_size()` as
`result_size` so a mismatched library/header pair fails explicitly.

## Minimal use

```c
#include <stdio.h>
#include "forge_normalizer.h"

int main(int argc, char **argv) {
    if (argc != 2) {
        return 2;
    }
    ForgeAnalysisV1 result = {0};
    char error[512];
    ForgeStatus status = forge_normalizer_analyze_file_v1(
        argv[1],
        48000u * 2u * 60u * 60u,
        &result,
        sizeof(result),
        error,
        sizeof(error));
    if (status != FORGE_STATUS_OK) {
        fprintf(stderr, "%s\n", error);
        return 1;
    }
    printf("%.2f LUFS, %.2f dBTP\n",
           result.integrated_lufs,
           result.true_peak_dbtp);
    return 0;
}
```

On Linux, compile from an extracted release directory with:

```sh
cc example.c -I include -L . -lforge_normalizer \
  -Wl,-rpath,'$ORIGIN' -o example
```
