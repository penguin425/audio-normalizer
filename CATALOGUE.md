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
  `forge-bs1770-5-r1`;
- selected preset, compliance profile, or explicit custom target;
- Forge package version and a structured invocation-provenance object.

Forge hashes every source before measurement and hashes it again immediately
before the database transaction. A mismatch aborts the record. Outputs must
produce the same size and SHA-256 in two consecutive inspections before their
row is committed.

The identity key includes both content hashes, operation, measurement
standard/revision, profile, and Forge version. Repeating an identical
measurement refreshes its evidence row instead of growing duplicates. A new
output, profile, algorithm revision, or Forge version creates a distinct row.
The catalogue is an audit index, not an analysis cache; use
`--analysis-cache` when avoiding repeated decoding is the goal.

## Durability and compatibility

The database uses a Forge application ID, `PRAGMA user_version = 1`, WAL
journaling, `synchronous = FULL`, a five-second busy timeout, and immediate
write transactions. Forge rejects non-empty unrecognized SQLite databases,
newer schema versions, catalogue symlinks, and paths that alias audio
inputs/outputs. A failed catalogue transaction is never marked complete in a
resumable batch checkpoint.

SQLite may create `-wal` and `-shm` files next to an open database. Use the
SQLite backup API or checkpoint/close all Forge processes before copying the
database as a standalone file. Concurrent readers are supported; concurrent
writers wait for the bounded busy timeout and then fail visibly.

## Provenance reports and bounds

`--catalogue-report PATH` atomically writes only the records committed by the
current invocation. The report follows
[`catalogue-report-v1`](schema/catalogue-report-v1.schema.json). Existing
reports require `--overwrite`.

Limits are explicit:

- 1 MiB of structured provenance per row;
- 4,096 UTF-8 bytes for paths and profiles;
- 256 UTF-8 bytes for operation identifiers;
- 100,000 records in one exported report;
- 1,024 channels and sample rates up to 384 kHz.

Catalogue and report files contain user-selected paths and processing
settings. Treat them as potentially sensitive local data and apply normal
filesystem access controls.
