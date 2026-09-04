# Watch folders

Forge can continuously discover and normalize files after their size and
modification time have remained unchanged for a configured interval:

```sh
forge incoming/ --watch --recursive \
  --watch-state work/incoming-watch.json \
  --watch-stable-seconds 10 \
  --watch-poll-seconds 2 \
  --output normalized/
```

The input must be exactly one directory and `--output` must name a different
directory. Without `--recursive`, only direct children are observed. Recursive
processing preserves relative subdirectories below the output root. A nested
output root is excluded from discovery.

This is an explicitly non-normative operational ingestion policy. No broadcast,
streaming, or filesystem standard defines that an unchanged timestamp and size
means a producer has finished writing; the observable rule below is Forge's
deterministic safety threshold.

## Stable-file rule

The first scan records each supported regular file's byte length,
nanosecond-resolution modification time, and observation time. A file is
eligible only when both properties remain unchanged for the full
`--watch-stable-seconds` interval. Any observed change restarts the interval.
Forge never follows symbolic links or Windows reparse points below the
explicit input root.

Immediately before processing, Forge checks the fingerprint again, computes
the complete input SHA-256, and checks the fingerprint after hashing. A writer
that changes the file during hashing therefore cannot be silently accepted.
Producers should still use an atomic rename into the input directory where
possible; timestamps and sizes are not a cryptographic writer-completion
protocol.

## Durable state and recovery

`--watch-state PATH` is required. The bounded JSON document follows the
[`watch-folder-v1`](schema/watch-folder-v1.schema.json) contract and is
synchronized and atomically replaced after every transition. The state path
itself is excluded from discovery even if it has an audio-like extension:

1. `observing` records the stable-file window.
2. `processing` records input SHA-256 and the intended output before encoding.
3. `completed` records input and output SHA-256.
4. `failed` records a bounded diagnostic and is not retried in a tight loop.

One process owns the state through a persistent sibling
`<state-name>.lock`. The lock file is intentionally not deleted between runs;
this prevents another process from locking a new inode while an earlier owner
still holds the old one. State and lock paths are protected from output-path,
hard-link, reparse-point, and platform case aliases.

On restart, a `processing` entry with no newly committed output is safely
requeued. If the transactional output was committed before the state update,
Forge hashes and adopts it as completed. Missing completed outputs are
requeued. Modified completed outputs are rejected rather than overwritten.
When a completed input changes, its previously recorded output must still
match its SHA-256 before Forge will replace it. A first-time output is
published with atomic no-clobber semantics; replacement is allowed only for
the exact previously verified output and is rejected if its identity or bytes
change while normalization is running.

Failed inputs remain suppressed until their size or modification time changes.
`--watch-retry-failed` explicitly requeues all unchanged failures once at
startup. It does not enable an unbounded automatic retry loop.

The state is bound to canonical input root, absolute output root, recursion
mode, stability interval, Forge version, and every normalization setting.
Reusing it with a different configuration is rejected.

## One-shot operation

`--watch-once` performs one scan, processes entries that were already stable,
and exits. A newly observed file is intentionally not processed in that same
invocation. This is suitable for schedulers:

```sh
forge incoming/ --watch --watch-once \
  --watch-state work/incoming-watch.json \
  --watch-stable-seconds 30 \
  --output normalized/
```

Run it again after the stability interval. Exit status is nonzero if a stable
file fails; other eligible files are still attempted and checkpointed.

## Bounds and incompatible modes

- State is limited to 16 MiB and 100,000 entries.
- A scan is limited to 1,000,000 directory entries and 64 directory levels.
- Diagnostics are limited to 4,096 bytes.
- Only supported regular audio files are considered.
- Analysis-only, album, dry-run, gain-only, tag-only, range/timeline,
  compliance/QC manifest, difference-report, and resumable batch-control modes
  are rejected with `--watch`.
- Analysis caching can be enabled normally and remains independently bounded.
- Termination may occur between any steps; atomic audio outputs and state
  transitions define the recovery behavior above.
