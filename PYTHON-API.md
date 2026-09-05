# Forge Python API

Forge provides a dependency-free Python wrapper over the versioned C ABI v1.
The wrapper performs bounded local-file loudness analysis; it does not encode,
normalize, or modify audio.

Official platform wheels are published as GitHub Release assets starting with
v0.97.0. Each wheel bundles the matching Forge shared library:

| Wheel platform tag | Host requirement | Bundled library |
| --- | --- | --- |
| `manylinux_2_34_x86_64` | 64-bit x86 Linux with glibc 2.34 or newer | `libforge_normalizer.so` |
| `macosx_11_0_arm64` | Apple silicon macOS 11 or newer | `libforge_normalizer.dylib` |
| `macosx_10_12_x86_64` | 64-bit Intel macOS 10.12 or newer | `libforge_normalizer.dylib` |
| `win_amd64` | 64-bit x86 Windows | `forge_normalizer.dll` |

The wheels support CPython and other Python 3 implementations that provide
the standard `ctypes` module, beginning with Python 3.10. They have no Python
runtime dependencies and are tagged `py3-none-<platform>`. Forge does not
currently publish this package to PyPI.

The Linux wheel is compiled with generic x86-64 flags in the digest-pinned
official `manylinux_2_28_x86_64` image. This avoids the x86-64-v2 system
package baseline of the AlmaLinux 9-based `manylinux_2_34` image while keeping
the published `manylinux_2_34_x86_64` contract. The build first emits a
`linux_x86_64` wheel; only the result of `auditwheel repair` is eligible for a
release. The release gate scans every ELF member for GLIBC symbol versions,
dynamic dependencies, C++ ABI references, declared ISA requirements, text
relocations, and executable stacks.

Forge contains CPUID-guarded AVX2 fast paths, so the release does not claim
that its ELF has no AVX instructions. Instead, the repaired wheel is loaded
and used to analyze a WAV under QEMU with every x86-64-v2 feature disabled.
Property-free ELF controls check both CPUID bits and executable instructions
for LAHF/SAHF, CX16, POPCNT, SSE3, SSSE3, SSE4.1, SSE4.2, AVX, and AVX2. Each
control is also rerun with its feature enabled to prove that the negative is
sensitive. A separate fresh glibc 2.34 container installs and exercises the
wheel. Both container smokes run with no network namespace and read-only
source and wheel mounts; a shared runner rejects any image without a full
SHA256 digest at execution time.

The complete Python build-tool closure is recorded in
`tools/python-wheel-build-requirements.lock`. It was resolved for CPython 3.10
from the PyPI Simple API on 2026-09-06, and its wheel digests were cross-checked
against each PyPI release JSON document. Release jobs use `--require-hashes`
and `--only-binary=:all:` to create a local wheelhouse, then install only from
that wheelhouse with `--no-index`. QEMU comes from Ubuntu's versioned
`qemu-user-static` archive package and is checked against the SHA256 in the
workflow. An independent job repeats the native build and repair and requires
the candidate and reproduced wheels to be byte-identical before publication.

## Installation

Download the wheel matching the host from the
[GitHub Release](https://github.com/penguin425/audio-normalizer/releases) and
install the local file:

```bash
python -m pip install ./forge_normalizer-0.97.0-py3-none-manylinux_2_34_x86_64.whl
```

Replace the version and platform tag with the selected release asset. Verify
the asset against `SHA256SUMS`; the same release also contains a GitHub SLSA
provenance bundle covering every wheel.

For development from a source checkout, put `python/src` on `PYTHONPATH` and
select a compatible native library:

```bash
export PYTHONPATH="$PWD/python/src"
export FORGE_NORMALIZER_LIBRARY="$PWD/target/release/libforge_normalizer.so"
```

## Analyze a file

```python
from forge_normalizer import analyze_file

result = analyze_file(
    "programme.wav",
    max_decoded_samples=48_000 * 2 * 60 * 60,
)

print(result.integrated_lufs)
print(result.true_peak_dbtp)
```

`max_decoded_samples` is mandatory. It counts decoded frames multiplied by
channels, not bytes, and must be an integer in `1..=2**64 - 1`. The example
allows at most one hour of 48 kHz stereo audio. Forge fails the operation if
the decoded input exceeds the limit.

`analyze_file` requires an authoritative physical-speaker layout. Classic
mono/stereo WAVE and supported formats with canonical layouts work directly;
ambiguous or scene-based inputs raise `AnalysisError`.

Use `analyze_file_with_layout` to obtain the effective exact descriptor or to
supply an external speaker assignment:

```python
from forge_normalizer import analyze_file_with_layout

result = analyze_file_with_layout(
    "maskless.wav",
    max_decoded_samples=48_000 * 6 * 60,
    channel_layout={
        "version": 1,
        "assignments": [
            {"kind": "legacy-role", "role": role}
            for role in ["main", "main", "main", "lfe", "surround", "surround"]
        ],
        "provenance": "known-speakers",
        "origin": "explicit-override",
    },
)
print(result.channel_layout)
```

The override must conform to
[`channel-layout-v1`](schema/channel-layout-v1.schema.json), identify every
physical speaker, contain the decoded channel count, and use
`explicit-override` without source or renderer evidence. Passing no override
returns the exact source-derived descriptor. This additive function requires a
v0.189.9-or-newer native library; the original `analyze_file` remains usable
with earlier compatible C ABI v1 libraries. Descriptor JSON is bounded at
16 MiB; the binding automatically retries with the exact returned output size
when the normal 256 KiB buffer is insufficient.

`path` and an optional `library` argument accept `str` or string-valued
`os.PathLike` objects. Paths cross the C boundary as UTF-8. Byte paths and
paths containing a NUL character are rejected.

The returned `Analysis` is an immutable dataclass with these fields.
`AnalysisWithLayout` adds a `channel_layout` dictionary:

| Field | Type | Unit or meaning |
| --- | --- | --- |
| `sample_rate_hz` | `int` | Decoded sample rate in hertz |
| `channels` | `int` | Decoded channel count |
| `frames` | `int` | Decoded frames per channel |
| `integrated_lufs` | `float` | Gated integrated loudness, LUFS |
| `max_momentary_lufs` | `float` | Maximum 400 ms momentary loudness, LUFS |
| `max_short_term_lufs` | `float` | Maximum 3 s short-term loudness, LUFS |
| `loudness_range_lu` | `float` | Loudness range, LU |
| `rms_dbfs` | `float` | Full-programme RMS, dBFS |
| `sample_peak_dbfs` | `float` | Maximum decoded sample peak, dBFS |
| `true_peak_dbtp` | `float` | Oversampled true peak, dBTP |

Measurement behaviour and channel interpretation are defined by the same
Forge analysis engine used by the CLI. See the main README and conformance
jobs for the supported formats and standards evidence.

## Version and ABI inspection

```python
import forge_normalizer

print(forge_normalizer.__version__)
print(forge_normalizer.c_api_version())
print(forge_normalizer.native_version())
```

`__version__` is the Python package version. `native_version()` is the Forge
version embedded in the loaded library. Official wheels keep them equal.
Source users may load another release if it implements the exact required C
ABI major and 80-byte result layout. `C_API_VERSION` and
`ANALYSIS_V1_SIZE` expose those requirements.

Library resolution uses this order:

1. The explicit `library=` argument.
2. `FORGE_NORMALIZER_LIBRARY`.
3. The library bundled in the installed wheel.
4. A system library found as `forge_normalizer`.

Explicit and environment paths must identify an existing file. Loaded handles
are cached by resolved path.

## Errors

All Forge-specific exceptions derive from `ForgeError`:

- `LibraryNotFoundError`: no library was found or the selected file could not
  be loaded.
- `AbiMismatchError`: required symbols, ABI version, structure size, or UTF-8
  native output violates the C ABI v1 contract.
- `AnalysisError`: the native analyser rejected or could not decode the file.
  Its `status` attribute is a `ForgeStatus` value when recognized, and
  `message` contains the native UTF-8 diagnostic.

Invalid Python argument types and ranges raise `TypeError` or `ValueError`
before native analysis begins.

## Concurrency and trust boundary

`ctypes.CDLL` releases the Python global interpreter lock while the native
analysis function runs. Independent calls may therefore run concurrently;
each call owns its result and error buffers. The wrapper does not expose
mutable native state.

Treat an explicitly selected shared library as executable code. The Python
wrapper verifies the Forge ABI contract, but it cannot make an untrusted
library safe. Official release wheels are built from the tagged source,
smoke-tested without an external library override, checksum-listed, and
covered by release provenance.
