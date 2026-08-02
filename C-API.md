# Forge C ABI v1

Forge release archives contain a versioned C interface for bounded local-file
loudness analysis and a bounded real-time gain processor:

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
- File analysis returns no allocation that a caller must free. Streaming
  creation returns an opaque `ForgeLiveV1` handle which the caller must release
  exactly once with `forge_normalizer_live_destroy_v1`.
- Paths are NUL-terminated UTF-8, including on Windows.
- The result must be aligned for `ForgeAnalysisV1` and writable for the
  advertised size for the duration of the call.
- The optional error buffer must be writable for its advertised capacity and
  must not overlap either the path string or the result.
- `max_decoded_samples` bounds decoded frames multiplied by channels and must
  be greater than zero.
- Error text is UTF-8, always NUL-terminated when capacity is positive, may be
  truncated on a character boundary, and is empty on success.
- File-analysis calls are independent and may run concurrently. A streaming
  handle is single-threaded; serialize calls that use the same handle.
- The interface uses the non-unwinding C calling convention. An unexpected
  Rust panic does not unwind into C; release builds use `panic=abort`.

`ForgeAnalysisV1` is exactly 80 bytes on supported 64-bit release targets.
The header and Rust implementation both assert its size and key offsets.
Callers should still pass `forge_normalizer_analysis_v1_size()` as
`result_size` so a mismatched library/header pair fails explicitly.

## Streaming processor

`ForgeLiveV1` is the host-neutral streaming contract used by integrations such
as FFmpeg and GStreamer. It processes interleaved IEEE-754 `float` samples in
place and is deliberately limited to a predictable, allocation-free processing
path. Creation allocates the handle; `process` and `flush` do not allocate.

```c
if (forge_normalizer_live_config_v1_size() != sizeof(ForgeLiveConfigV1)) {
    /* Header/library mismatch: stop before creating a handle. */
}
ForgeLiveConfigV1 config = {
    .struct_size = sizeof(config),
    .api_version = FORGE_NORMALIZER_C_API_VERSION,
    .sample_rate_hz = 48000u,
    .channels = 2u,
    .initial_gain_db = 0.0,
    .ceiling_dbtp = -1.0,
    .attack_ms = 10.0,
    .release_ms = 100.0,
};
char error[256];
ForgeLiveV1 *live = forge_normalizer_live_create_v1(
    &config, error, sizeof(error));
```

The v1 configuration limits are 8--384 kHz, 1--64 channels, initial gain
`-120..120 dB`, ceiling `-120..0 dBTP`, and attack/release `0.01..10000 ms`.
The handle is single-threaded; callers must serialize operations on one handle,
while separate handles can run concurrently. `forge_normalizer_live_latency_frames_v1`
reports the fixed five-millisecond look-ahead (at least 16 frames).

For each input block, call
`forge_normalizer_live_process_interleaved_f32_v1(live, samples, frames, ...)`.
The first latency-sized output prefix is zero while the look-ahead fills. A
zero-frame call is a no-op before end-of-stream and may pass a null sample
pointer. At end-of-stream, allocate caller-owned interleaved storage for at
least `latency_frames * channels` floats and call
`forge_normalizer_live_flush_interleaved_f32_v1`. It writes exactly
`latency_frames` frames, stores that count in `written_frames`, and is
one-shot. Processing, setters, or a second flush after it returns
`FORGE_STATUS_INVALID_ARGUMENT`; destroy the handle with
`forge_normalizer_live_destroy_v1`.

The gain and ceiling setters are safe between blocks and take effect through
the processor's smoothing envelope. Error buffers follow the same optional,
NUL-terminated UTF-8 contract as file analysis. Adapters should map
`FORGE_STATUS_BUFFER_TOO_SMALL`, `FORGE_STATUS_NULL_POINTER`, and
`FORGE_STATUS_INVALID_ARGUMENT` to their host's explicit negotiation or
end-of-stream errors rather than silently dropping samples.

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
