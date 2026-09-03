# Segment-aware catalogue normalization

`forge-segment-normalize` is a bounded two-pass workflow for an ordered set of
audio segments. Pass one measures and hashes every source, then writes a
deterministic gain plan. Pass two verifies every source binding before it
publishes anything, applies a continuous boundary envelope one segment at a
time, re-decodes each encoded output, and writes a versioned evidence report.

The `forge-segment-normalization-v1` method is non-normative engineering. It
does not claim that a streaming service requires independently normalized
segments, that standalone codec files concatenate without codec delay, or that
the result is a platform compliance certificate. Apply it to ordered programme
segments or catalogue chunks whose transitions are intentionally related.

## Pass one: create a content-bound plan

Create `catalogue/request.json`:

```json
{
  "schema": "https://penguin425.github.io/audio-normalizer/schema/segment-normalization-request-v1",
  "target_lufs": -16.0,
  "ceiling_dbtp": -1.0,
  "max_gain_db": 18.0,
  "smoothing_ms": 500.0,
  "verification_tolerance_lu_db": 0.5,
  "duration_tolerance_ms": 100.0,
  "boundary_review_threshold_db": 6.0,
  "max_decoded_samples_per_segment": 50000000,
  "format": "flac",
  "segments": [
    {
      "id": "programme-0001",
      "input": "source/0001.wav",
      "output": "normalized/0001.flac"
    },
    {
      "id": "programme-0002",
      "input": "source/0002.wav",
      "output": "normalized/0002.flac"
    }
  ]
}
```

Paths are resolved relative to the request. Create the plan:

```sh
forge-segment-normalize plan \
  --request catalogue/request.json \
  --manifest catalogue/segment-plan.json
```

Use `--channel-layout` when all sources share a known layout that is missing or
incorrect in their containers. Accepted names are `mono`, `stereo`, `5.1`,
`6.1`, `7.1`, `5.1.4`, and `7.1.4`. Pass one rejects mixed sample rates,
channel counts, or channel roles because such files do not form one continuous
boundary domain. A segment must also contain enough non-silent programme audio
to produce finite BS.1770 integrated loudness from complete gating blocks.

The plan stores absolute resolved paths, source byte lengths and SHA-256
digests, BS.1770 measurements, the exact gain envelope, algorithm revision,
resource limits, and manual-review flags. Existing plans are refused unless
`--overwrite` is explicit. JSON requests are standard; a `.toml` request is
also accepted.

Plans created before Forge 0.189.2 must be regenerated. Earlier plans could
record fallback channel roles when a container did not identify its speaker
layout; accepting them would bypass the current fail-closed layout check.

## Deterministic boundary method

For segment `i`, Forge first computes the ordinary level gain and its maximum
safe static gain:

```text
level_gain_i = target_lufs - source_lufs_i
safe_gain_i  = min(ceiling_dbtp - source_true_peak_i, max_gain_db)
desired_i    = min(level_gain_i, safe_gain_i)
```

When `max_gain_db` is absent, only the true-peak bound limits positive gain.
The common gain at boundary `i` between adjacent segments is:

```text
boundary_i = min((desired_i + desired_(i+1)) / 2,
                 safe_gain_i,
                 safe_gain_(i+1))
```

Both sides store exactly the same boundary value. Within each segment, a cubic
smoothstep curve in the dB domain joins the boundary value and desired gain:

```text
s(t) = 3t² - 2t³,  0 <= t <= 1
gain(t) = start_db + (end_db - start_db) * s(t)
```

Smoothstep has zero slope at both endpoints. The requested smoothing interval
is capped at half the segment length, so the entrance and exit ramps never
overlap. A two-frame segment still preserves both boundary endpoints. The
first segment starts at its desired gain and the final segment ends at its
desired gain.

`boundary_review_threshold_db` does not alter audio or fail the plan. It marks
large adjacent desired-gain differences for human review in both the segment
and plan summary.

## Pass two: render and verify

```sh
forge-segment-normalize render \
  --manifest catalogue/segment-plan.json \
  --report catalogue/segment-report.json
```

Pass two validates the complete plan before decoding. It re-hashes and parses
the bound request, checks every setting, ID, input, and output mapping against
that request, then hashes every input before publishing any destination. Each
segment is decoded under the recorded sample bound, measured again, rendered
from its original source, encoded to a sibling temporary file, re-decoded, and
checked for:

- codec loudness deviation from the exact smoothed pre-codec signal;
- final decoded true peak against the ceiling;
- decoded duration drift;
- unchanged input bytes before and after source decoding.

A passing destination is atomically replaced. Processing is deliberately
bounded to one decoded segment and one staged output at a time, rather than
holding an entire large catalogue in memory or duplicating all outputs on
disk. Consequently, the ordered set is not a filesystem transaction: if a
late I/O or codec error occurs, earlier verified segments can already be
visible. The report records the published count. Obvious stale-input,
collision, overwrite, and plan-integrity failures are checked before the first
publication.

Existing outputs and reports are refused unless `--overwrite` is explicit.
The request, plan, report, every input, and every output must remain distinct
after lexical normalization, existing-parent symlink resolution, and
conservative case folding on Windows and macOS.

## Formats and resource limits

The request uses one format for the ordered set: `wav`, `flac`, `mp3`, `opus`,
`m4a`, `alac`, or `vorbis`, with the matching conventional extension. MP3
requires the `mp3-encoding` build feature, Opus requires `opus-encoding`, and
M4A/ALAC/Vorbis require the `ffmpeg-encoding` feature plus `ffmpeg` at runtime.

Limits are part of the method and manifest:

- request: at most 4 MiB;
- pass-one plan: at most 16 MiB when consumed;
- encoded audio input: at most 4 GiB per segment;
- ordered segments: 2 to 4096;
- decoded samples per segment: default 50,000,000, hard maximum 200,000,000;
- smoothing: 1 to 10,000 ms, capped to half of each segment;
- verification tolerance: 0 to 5 LU/dB;
- duration tolerance: 0 to 10,000 ms;
- identifiers: 1 to 64 ASCII letters, digits, `.`, `_`, or `-`.
- channel count: 1 to 32; current plan sample rate: 8,000 to 384,000 Hz.

The decoded-sample count is frames multiplied by channels. Choose a lower
request value when processing untrusted or memory-constrained catalogues.

## Schemas and exit status

Validate each phase with:

- `schema/segment-normalization-request-v1.schema.json`
- `schema/segment-normalization-plan-v1.schema.json`
- `schema/segment-normalization-report-v1.schema.json`
- `schema/segment-normalization-plan-v2.schema.json`
- `schema/segment-normalization-report-v2.schema.json`

New plans and reports use v2 so channel-layout provenance is part of the
method contract. The immutable v1 schemas remain published for validating
historical documents; v1 plans must be regenerated before rendering.

Both plan and report are written atomically as pretty JSON. A successful plan
or passing render exits 0, a completed render with failed decoded evidence
exits 1, and invalid input, stale bindings, unsupported codecs, collisions, or
I/O failures exit 2.
