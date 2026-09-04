# Resumable batch jobs

Forge can checkpoint an ordinary multi-file normalization run after every
completed output and resume the same job later:

```sh
forge album/*.wav -o normalized/ \
  --job-state work/album-job.json \
  --progress work/album-progress.ndjson
```

`--job-state` requires at least two expanded input files. It applies to
independent track normalization, not `--album`, analysis-only, dry-run,
gain-only, metadata-only, or normalization-difference-report workflows.
Only one Forge process may own a particular state path at a time.
The persistent sibling `<state-name>.lock` is intentionally retained between
runs so two processes can never lock different inodes after state replacement.

## Resume and integrity rules

The state is a bounded JSON document following
[`batch-job-v2`](schema/batch-job-v2.schema.json). Existing
[`batch-job-v1`](schema/batch-job-v1.schema.json) documents are validated and
migrated on first use. Forge:

- resolves every input and output path and binds the ordered job to the
  normalization settings, output formats, and SHA-256 of every input;
- writes the state beside a temporary file, synchronizes it, and atomically
  replaces the previous checkpoint after each output succeeds;
- records the hash of final staged bytes as `ready_to_publish`, then records an
  output as complete only after publishing and hashing it;
- hashes every completed output again before skipping it on a later run;
- returns a missing completed output to `pending` and rebuilds only that asset;
- rejects a changed completed output unless `--overwrite` is supplied, in
  which case only changed or otherwise pending assets are rebuilt; and
- rejects reuse when an input, output path, ordered asset list, or
  normalization setting differs.

The v2 schema adds the recoverable publication checkpoint. The recorded
generator version is provenance; unsupported schema versions are rejected.

State documents are limited to 16 MiB and 100,000 assets. Input and output
hashing is streaming. Resumable paths must be valid UTF-8. The state path and
progress path must be different and cannot overwrite an audio input or output.

If a run fails, the state retains all earlier completed assets. Repeating the
same command skips those hash-verified outputs and retries the first pending
asset. If a process stops while encoding, Forge's transactional audio output
keeps any previous destination intact. If it stops after publication but
before the final checkpoint, the staged hash identifies the committed output
and the next run promotes it to complete without re-encoding.

Independent normalization uses the worker budget selected by `--jobs` in
bounded waves of at most 32 assets. Each worker renders to a sibling temporary
file. Forge then publishes successful outputs, catalogue records, and
checkpoints in input order. If asset `n` fails, already published earlier
assets remain resumable, asset `n` reports the failure, and every later staged
asset in that wave is discarded without replacing its destination. `--jobs 1`
selects one-at-a-time processing.

Post-encode verification remains serial, but it uses the same
ready-to-publish recovery point. Runs using the shared analysis cache can use
the bounded parallel waves when no later difference report is requested.

## Machine-readable progress

`--progress PATH` writes one
[`batch-progress-v1`](schema/batch-progress-v1.schema.json) JSON object per
line. Use `--progress -` for stdout. Binary audio output and progress cannot
both use stdout. A regular progress file is replaced for each invocation;
`sequence` starts at zero for that invocation.

Events are ordered as follows:

| Event | Meaning |
|---|---|
| `job_started` | Invocation began after state validation |
| `asset_started` | One pending asset is about to run; all starts in a parallel wave precede its completions |
| `asset_completed` | Output succeeded and its checkpoint was committed |
| `asset_skipped` | Existing output matched its checkpoint hash |
| `asset_failed` | Asset failed; includes a non-empty `error` |
| `job_completed` | All assets are complete |

Asset events include zero-based `index`, `input`, and `output`. Every event
contains `completed`, `total`, a monotonic per-invocation `sequence`, the
schema identifier, and generator version. `job_completed` is not emitted after
an asset failure.

Within a parallel wave, `asset_started` events are emitted in input order
before the workers start. `asset_completed` events are emitted in input order
after each atomic publication and checkpoint. This makes progress consumers
deterministic even when the corresponding renders finish in another order.

Example shell monitoring:

```sh
forge input/*.flac -o output/ --job-state job.json --progress - |
  jq -c 'select(.event == "asset_completed" or .event == "asset_failed")'
```
