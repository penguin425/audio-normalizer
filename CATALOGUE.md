# SQLite catalogue

Forge can maintain an opt-in SQLite catalogue that binds audio measurements to
the exact source and output byte streams:

```sh
forge input.wav --output normalized.wav \
  --catalogue work/library.sqlite \
  --catalogue-report work/last-run.json
```

`--catalogue` works with analysis-only, independent-track, batch, and album
normalization. It is intentionally incompatible with dry-run, gain-only,
in-place tag writing, stdin/stdout audio, and watch folders because those modes
do not provide the same completed source/output transaction boundary.

## Recorded evidence

Each row contains:

- source path, byte count, and streaming SHA-256;
- optional normalized-output path, byte count, and streaming SHA-256;
- source sample rate, channels, frames, duration, integrated loudness, LRA,
  RMS, sample peak, and true peak;
- `ITU-R BS.1770-5 / EBU R 128` and Forge algorithm revision
  `forge-bs1770-5-r4`;
- selected preset, compliance profile, or explicit custom target;
- the content-probed container/codec, selected track index and ID, exact frame
  range, declared/effective layout, renderer, and complete resolved plan;
- a SHA-256 over that canonical request evidence;
- Forge package version and a structured invocation-provenance object.

Forge hashes every source before measurement. Measurement and catalogue commit
share a private immutable `InputDescriptor`; path-only compatibility calls
capture one at commit. Forge rebinds the live path and verifies the source
again after output inspection. A mismatch aborts the record. Outputs must
produce the same size and SHA-256 in two consecutive inspections before their
row is committed.

The v2 identity key includes both content hashes, operation, canonical request
hash, measurement standard/revision, profile, and Forge version. Repeating an
identical measurement refreshes its evidence row instead of growing
duplicates. A different track, range, layout, renderer, plan, output, profile,
algorithm revision, or Forge version creates a distinct row.
The catalogue is an audit index, not an analysis cache; use
`--analysis-cache` when avoiding repeated decoding is the goal.

## Durability and compatibility

The database uses a Forge application ID, `PRAGMA user_version = 2`, WAL
journaling, `synchronous = FULL`, a five-second busy timeout, and immediate
write transactions. Forge rejects non-empty unrecognized SQLite databases,
newer schema versions, catalogue symlinks, and paths that alias audio
inputs/outputs. A failed catalogue transaction is never marked complete in a
resumable batch checkpoint.

Opening a Forge v1 catalogue migrates it transactionally to v2. Existing rows
are retained with an explicit legacy request marker because v1 did not record
enough information to reconstruct track/range/renderer identity.

SQLite may create `-wal` and `-shm` files next to an open database. Use the
SQLite backup API or checkpoint/close all Forge processes before copying the
database as a standalone file. Concurrent readers are supported; concurrent
writers wait for the bounded busy timeout and then fail visibly.

## Provenance reports and bounds

`--catalogue-report PATH` atomically writes only the records committed by the
current invocation. New reports use
[`catalogue-report-v2`](schema/catalogue-report-v2.schema.json) and include the
canonical request evidence. The immutable
[`catalogue-report-v1`](schema/catalogue-report-v1.schema.json) schema remains
available for historical validation. The Rust compatibility methods
`Catalogue::record` and `Catalogue::write_report` continue to return and write
v1 records; descriptor-bound callers use `record_bound` and `write_report_v2`.
Existing report paths require `--overwrite`.

Limits are explicit:

- 1 MiB of structured provenance per row;
- 4,096 UTF-8 bytes for paths and profiles;
- 256 UTF-8 bytes for operation identifiers;
- 100,000 records in one exported report;
- 1,024 channels and sample rates up to 384 kHz.

Catalogue and report files contain user-selected paths and processing
settings. Treat them as potentially sensitive local data and apply normal
filesystem access controls.
