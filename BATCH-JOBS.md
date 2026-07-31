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

## Resume and integrity rules

The state is a bounded JSON document following
[`batch-job-v1`](schema/batch-job-v1.schema.json). Forge:

- resolves every input and output path and binds the ordered job to the
  normalization settings, output formats, and SHA-256 of every input;
- writes the state beside a temporary file, synchronizes it, and atomically
  replaces the previous checkpoint after each output succeeds;
- records an output as complete only after hashing the committed output;
- hashes every completed output again before skipping it on a later run;
- returns a missing completed output to `pending` and rebuilds only that asset;
- rejects a changed completed output unless `--overwrite` is supplied, in
  which case only changed or otherwise pending assets are rebuilt; and
- rejects reuse when an input, output path, ordered asset list, or
  normalization setting differs.

The v1 schema is the compatibility boundary. The recorded generator version is
provenance, so a later Forge version may resume a v1 document when it still
supports exactly the same operation contract. Unsupported schema versions are
rejected.

State documents are limited to 16 MiB and 100,000 assets. Input and output
hashing is streaming. Resumable paths must be valid UTF-8. The state path and
progress path must be different and cannot overwrite an audio input or output.

If a run fails, the state retains all earlier completed assets. Repeating the
same command skips those hash-verified outputs and retries the first pending
asset. If a process stops while encoding, Forge's transactional audio output
keeps any previous destination intact and the asset remains pending.

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
| `asset_started` | One pending asset is about to run |
| `asset_completed` | Output succeeded and its checkpoint was committed |
| `asset_skipped` | Existing output matched its checkpoint hash |
| `asset_failed` | Asset failed; includes a non-empty `error` |
| `job_completed` | All assets are complete |

Asset events include zero-based `index`, `input`, and `output`. Every event
contains `completed`, `total`, a monotonic per-invocation `sequence`, the
schema identifier, and generator version. `job_completed` is not emitted after
an asset failure.

Example shell monitoring:

```sh
forge input/*.flac -o output/ --job-state job.json --progress - |
  jq -c 'select(.event == "asset_completed" or .event == "asset_failed")'
```
