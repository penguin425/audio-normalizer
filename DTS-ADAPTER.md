# DTS core/HD reference-decoder adapter protocol v1

Forge does not distribute or dynamically load a DTS decoder. `forge-dts-qc`
first validates a raw DTS core/DTS-HD elementary stream itself, then invokes an
explicitly selected licensed or reference decoder as a separate process. The
adapter boundary is versioned, bounded, and records hashes for the input,
adapter, and every rendered presentation.

The normative bitstream reference is **ETSI TS 102 114 V1.6.1 (2019-08)**.
DTS-UHD is standardized separately by ETSI TS 103 491 and is intentionally not
claimed by this protocol.

## Invocation

```text
adapter --request /absolute/work/request.json \
        --response /absolute/work/response.json
```

The request conforms to `schema/dts-adapter-request-v1.schema.json`. The
adapter must write one response conforming to
`schema/dts-adapter-response-v1.schema.json` and place all WAVE renders beneath
the supplied `output_directory`. It must not replace the input or adapter.

The adapter must:

1. bind its response to `input_sha256` and claim the exact ETSI version above;
2. enumerate every declared asset and presentation exactly once;
3. give every asset a stable ID plus its `(extension_substream_index,
   asset_index)` location, and identify the DTS profile and each coding
   component (`core`, `xch`, `xxch`, `x96`, `xbr`, `lbr`, or `xll`);
4. render each presentation to a distinct relative `.wav` path;
5. disable dialog normalization and DRC when supported, otherwise report
   `applied` or `not-supported` explicitly; and
6. declare each render's sample rate and channel count.

Forge independently decodes every WAVE, confirms its declared geometry,
measures integrated loudness and true peak with ITU-R BS.1770-5, and optionally
enforces a true-peak ceiling. DTS dialog-normalization metadata is preserved as
metadata only: it is not programme loudness and is never compared with LUFS.
The atomic `schema/dts-adapter-report-v2.schema.json` result adds an exact
output-layout descriptor bound to the decoder name/version, output
configuration, adapter executable hash, and exact adapter response. Report v1
remains available for historical validation.

## Native checks

Before launching the adapter, Forge walks every frame boundary without sync
scanning or resynchronization. It accepts the four standardized core wire
representations (16-bit BE/LE and 14-bit BE/LE), validates core header reserved
values and lengths, checks zero DWORD padding, and validates every DTS-HD EXSS
header CRC-16/CCITT, header/frame length, index, static presentation count, and
static asset count.
Malformed or truncated input never reaches the external decoder.

## Bounds and trust boundary

- at most 1,000,000 native frames;
- EXSS headers at most 4,096 bytes;
- at most 32 adapter assets and 32 presentations;
- adapter timeout 1–3,600 seconds;
- adapter stdout, stderr, and response are bounded;
- decoded interleaved PCM is capped per presentation;
- response and render paths cannot escape the temporary workspace;
- symlink escapes and duplicate render paths are rejected; and
- reports are written atomically and never replaced without `--overwrite`.

The adapter may wrap a local commercial decoder or reference implementation.
It should not send source material to a network service unless the operator has
separately authorized that transfer.
