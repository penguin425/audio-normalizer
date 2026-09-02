# Content-addressed analysis cache

Forge can reuse its core loudness and peak measurement without trusting a file
path or modification timestamp:

```sh
forge input/*.wav -o output/ --analysis-cache work/analysis-cache
```

The cache is opt-in. It is used by analysis-only, range/timeline, gain-only,
dry-run, metadata tagging, ordinary track normalization, corrected
normalization, and album normalization. Codec QC, dialogue detection and
measurement, downmix, ADM presentation rendering, output re-verification, and
other specialized or newly encoded-output measurements remain uncached.

## Identity and measurement provenance

Every lookup streams the complete input file through SHA-256. The address also
contains a canonical request descriptor:

- an explicit channel-role override, including positioned-channel azimuth and
  elevation;
- source start and duration in seconds;
- timeline interval in milliseconds; or
- requested output sample rate and `fast`, `balanced`, or `best` resampling
  quality.

Consequently identical bytes at different paths share an entry, while changed
bytes or measurement-changing options cannot reuse one. The request hash,
input hash, canonical result-payload hash, generator version, measurement
standard, and algorithm revision are retained in the entry. Cache v1 records
normative ITU-R BS.1770-5 / EBU R 128 measurements; caching does not alter
gating, channel weighting, units, or normalization targets.
`forge-bs1770-5-r2` is the current implementation revision and changes
whenever a result-affecting core algorithm changes.

The JSON compatibility boundary is
[`analysis-cache-v1`](schema/analysis-cache-v1.schema.json). Decibel silence
values that are mathematically negative infinity are represented as JSON
`"-inf"`; on a validated hit Forge restores negative infinity. Linear peaks
and gating-block mean squares use finite, non-negative values. Timeline levels
that do not yet have a complete measurement window are `null`, so incomplete
windows remain distinct from complete silent windows.

## Layout, atomicity, and concurrency

Recognized entries use this fixed-depth layout:

```text
DIR/v1/aa/<64-character-input-sha256>/<64-character-request-sha256>.json
```

Forge creates a sibling temporary file, writes and synchronizes the complete
entry, then atomically replaces the destination. Concurrent producers of the
same address therefore publish only complete documents. Multi-input album and
independent-file jobs hash, validate, or compute cache results in the shared
`--jobs` pool; observations and failures are resolved in input order. Within
one Forge process, entry commits and capacity pruning are serialized while the
expensive hashing and analysis remain parallel. Separate processes may share a
cache; eviction is best-effort FIFO by file modification time, so a concurrent
process can legitimately turn an expected hit into a miss.
Inputs are expected to remain stable while Forge runs. Writable and read-only
misses hash again after measurement and fail instead of publishing when the
ordinary start/end content hashes differ.

The cache never recursively interprets arbitrary files. Capacity accounting
and eviction recognize only regular JSON files at the exact v1 layout with
lower-case SHA-256 names. Unrecognized files and directories are left alone.

## Corruption and read-only behavior

Every hit revalidates the schema identifier, measurement and algorithm
versions, input/request/result hashes, exact request descriptor, channel
geometry, PCM kind, numeric domains, ordering, and collection limits. A
truncated, payload-modified, malformed, or semantically invalid entry is an
observable miss:

```text
analysis cache invalid; repaired: input.wav
analysis cache warning: cache entry is not valid v1 JSON: ...
```

Writable mode recomputes and atomically repairs it. With
`--analysis-cache-read-only`, Forge computes the requested result for the
current invocation but leaves a missing or invalid entry untouched. An absent
read-only cache directory is not created. Filesystem access or commit failures
are errors rather than silent cache bypasses.

## Resource limits and eviction

- One entry is limited to 64 MiB.
- Gating-block and timeline arrays are each limited to 1,000,000 values.
- Channel-role arrays are limited to 1,024 values.
- Eviction scans at most 100,000 recognized entries at the fixed layout depth.
- Input hashing uses a 1 MiB buffer and analysis remains streaming.
- `--analysis-cache-max-mib MIB` bounds recognized entry bytes and defaults to
  1024 MiB. A result larger than either the per-entry or cache limit is used
  but not stored.

After a successful write, Forge removes the oldest recognized entries until
the configured bound is met. It never removes the entry just written, an
unrecognized path, an input, or an audio output. Use read-only mode when cache
mutation or eviction is undesirable.

Hashing and decoding are linear in input duration and intentionally have no
wall-clock timeout: local storage speed and audio duration determine runtime.
Byte, count, and traversal-depth limits prevent a cache document or directory
shape from causing unbounded allocation or recursive parsing.
