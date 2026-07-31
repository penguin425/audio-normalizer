# Multi-delivery optimization

`forge-multi-delivery` renders one source into two to 32 delivery formats with
one shared linear gain. It selects the most conservative integrated-loudness
target and true-peak ceiling from the requested profiles, encodes every output,
re-decodes each staged file, and publishes the outputs only when one common
gain satisfies every codec.

This is the versioned, non-normative `forge-multi-delivery-v1` engineering
method. It does not assert that every platform requires the same master or that
a quieter delivery will be promoted by a playback service. Profile-specific
loudness and true-peak headroom remain visible in the report. Per-profile
checks are conservative upper-bound checks, not certification that an output
meets an exact platform or broadcast acceptance window.

## Request and command

Create `delivery/multi-delivery.json`:

```json
{
  "schema": "https://penguin425.github.io/audio-normalizer/schema/multi-delivery-request-v1",
  "verify_tolerance_lu_db": 0.5,
  "verify_retries": 2,
  "max_gain_db": 12.0,
  "deliveries": [
    {
      "id": "streaming-wav",
      "output": "outputs/streaming.wav",
      "format": "wav",
      "preset": "spotify"
    },
    {
      "id": "broadcast-flac",
      "output": "outputs/broadcast.flac",
      "format": "flac",
      "preset": "ebu-r128"
    },
    {
      "id": "podcast-opus",
      "output": "outputs/podcast.opus",
      "format": "opus",
      "preset": "podcast-stereo"
    }
  ]
}
```

Relative output paths are resolved from the request file's directory. Run:

```sh
forge-multi-delivery master.wav \
  --request delivery/multi-delivery.json \
  --report delivery/multi-delivery-report.json
```

JSON is the default request syntax; a `.toml` request is also accepted. Use
`--channel-layout` when source layout metadata is missing or incorrect. Existing
outputs and reports are refused unless `--overwrite` is explicit.

Format and output-extension pairs are checked before encoding: `wav`/`.wav`,
`flac`/`.flac`, `mp3`/`.mp3`, `opus`/`.opus`, `m4a` or `alac`/`.m4a` or `.mp4`,
and `vorbis`/`.ogg` or `.oga`. Optional encoders have the same build/runtime
requirements as the main `forge` command.

## Deterministic method

For a request with profile targets `Tᵢ` and ceilings `Cᵢ`, Forge uses:

```text
common target  = min(Tᵢ)
common ceiling = min(Cᵢ)
```

Because LUFS and dBTP values are negative in normal use, `min` selects the
quietest target and lowest ceiling. Every format is rendered from the original
source with the same gain; lossy outputs are never used as the next pass's
input. After each pass, Forge intersects every decoded output's allowed
loudness-correction interval with all true-peak and maximum-gain upper bounds.
The quietest feasible point is used for the next pass. An empty intersection is
an explicit error—Forge never silently changes to per-output gain.

All encodes are staged beside their destinations. No delivery becomes visible
until every staged output has passed re-decoding, loudness, true-peak, and
post-metadata verification. Each final replacement is atomic; the set of
sequential replacements is not a filesystem transaction.

## Evidence and limits

The JSON report records source/output SHA-256 hashes, measurements, the shared
gain, common constraints, the fixed verification target, the final pre-codec
loudness after any codec correction, encoding-pass count, resolved versioned
profiles, profile provenance, per-profile headroom, and every PASS/FAIL
decision. Validate requests and reports with:

- `schema/multi-delivery-request-v1.schema.json`
- `schema/multi-delivery-report-v1.schema.json`

Requests are limited to 1 MiB, two to 32 unique delivery IDs and output paths,
at most ten correction retries, and finite bounded settings. Lexical and
symlink path aliases are rejected against the input, request, report, and other
outputs. A successful run exits 0; an evidence FAIL exits 1; invalid input,
configuration, encoder availability, or processing errors exit 2.
