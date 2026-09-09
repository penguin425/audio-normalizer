# Resumable batch and album generations

Forge's durable `--job-state PATH` workflow binds a multi-file normalization
invocation to its ordered inputs, outputs, operation settings, and semantic
runtime context. It stages every audio destination first, then uses a sibling
generation journal to publish the complete output set as one recoverable
generation.

```sh
forge album/*.wav -o normalized/ \
  --job-state work/album-job.json \
  --progress work/album-progress.ndjson \
  --verify
```

`--job-state` requires at least two expanded regular-file inputs. It supports
independent tracks and `--album`; album mode records the shared-gain operation
in the job descriptor. `--verify` is also part of the bound operation and runs
before any staged output is published. The state path and all generated
control paths must be distinct and must not alias an input or audio output.
Only one Forge process owns a state path at a time through its persistent
sibling lock.

## v3 state and semantic identity

The CLI accepts only [`batch-job-v3`](schema/batch-job-v3.schema.json) for
`--job-state`. The historical
[`batch-job-v1`](schema/batch-job-v1.schema.json) and
[`batch-job-v2`](schema/batch-job-v2.schema.json) documents remain registered
for compatibility inspection, but are not implicitly migrated: use a new
state path or an explicitly reviewed migration procedure. A v1/v2 per-asset
checkpoint cannot prove that a partially published generation was atomic.

The v3 document contains a deterministic `job_id`, a semantic fingerprint and
revision, the complete `normalization-semantic-context-v1` runtime context,
the failure policy, and an ordered input/output manifest. The semantic context
includes the analysis engine and revision, decoder/input-descriptor revision,
selected audio-track policy, and output writer capability evidence. It excludes
runtime paths (including FFmpeg executable paths), timestamps, process IDs, cache
locations, and cache hit/miss state, so cold and warm cache runs have the same
identity. Input/output paths are resolved and lexically normalized before they
are bound into the ordered manifest; path spelling such as `a/../b` cannot
create a second identity for the same normalized destination. The selected
true-peak backend is likewise not a separate semantic identity under the
current bit-exact policy: CPU/CUDA selection does not alter the bound writer
or normalization fingerprint. Exact writer/encoder/muxer identity changes
require a new fingerprint and cannot resume an old job.

The operation object records every byte-affecting normalization option. Its
stable fields include `album`, `analysis_engine`, and nullable `audio_track`;
when an explicit metadata-fidelity policy is active it additionally records
`metadata_policy` (`policy`, `strip_scope`, and exact `strip_locators`),
`metadata_registry_revision`, `metadata_timing_revision`, and
`metadata_fidelity_schema_version`. These fields are optional for compatibility
because the historical default operation does not emit metadata policy keys.

## Generation publication and recovery

The generation journal is always the sibling
`<job-state>.generation.json`; its persistent lock is
`<job-state>.generation.json.lock`. Before a journal is written, each pending
audio output is rendered into a private `.forge-*` stage and verified. The
journal then records the semantic fingerprint, generation ID, destination
preimage (missing or identity/length/SHA-256), stage evidence, private backup
path, and publication step. Members are published in canonical destination
path order (the same lexically normalized form used in the manifest) so
overlapping jobs encounter a shared destination before they can modify shared
paths in opposite orders. The journal phases are `ready`,
`publishing`, `committed`, and `rolled_back`.

Publication moves an existing destination to its journal-owned private backup,
publishes the exact staged bytes, and verifies identity and digest. A failure
restores verified preimages where possible and records `rolled_back`; a
process interruption in the middle of publication is recovered by the same
evidence checks. The filesystem may briefly expose a prefix while independent
renames are in flight, but a restart converges to the exact new generation or
the exact old preimage; it never silently treats an unauthenticated partial
set as complete. The batch state is marked complete only after every
generation destination is re-hashed and the v3 checkpoint is atomically
updated. If that final checkpoint write fails after the generation journal is
already committed, the audio remains committed and is never reported as an
asset failure. The command returns nonzero, emits truthful committed progress
when possible, and the next identical invocation revalidates the journal and
retries only the checkpoint.

`forge recovery inspect STATE` reads and validates the journal without making
directories, locks, phase changes, or other durable changes. It emits a
[`generation-recovery-report-v1`](schema/generation-recovery-report-v1.schema.json)
JSON report. `forge recovery reclaim STATE` is a dry-run unless `--yes` is
also supplied; its report distinguishes an active lock, a ready abandonment,
publishing recovery, committed-backup cleanup, and a no-op. With confirmation
it revalidates the journal under its lock and reclaims only the private
stage/backup paths proved to belong to that journal. Ready or interrupted
publishing work converges to the terminal `rolled_back` state; cleanup of a
fully published generation retains `committed`. Unknown `.forge-*` files,
including files from another job or an interrupted unjournaled operation, are
never removed by name alone. Recovery paths remain within trusted state and
destination-parent directories; the journal is integrity-validated but is not
an authentication boundary against a hostile owner of those paths.

## Failure policy

The default v3 policy is `fail_fast`: the first rendering, verification, or
control error aborts the generation, drops all newly staged outputs, and
leaves existing destinations unchanged. `--keep-going` is restricted to an
independent batch with `--job-state`; it is rejected for album, analysis-only,
dry-run, gain-only, metadata-only, watch, and difference-report workflows.
Every independent asset is attempted, failures are retained in input-index
order up to the bounded report limit, and no new audio generation is
published if any asset fails. Existing destinations from an earlier committed
generation are not replaced by a failed invocation. The command exits nonzero
and may write `--failure-report PATH` using
[`batch-failure-report-v1`](schema/batch-failure-report-v1.schema.json); the
report carries job ID, semantic fingerprint, counts, bounded error strings,
and truncation/drop counts. A later identical invocation retries the pending
generation with the same v3 identity.

Album normalization always uses one shared gain and is all-or-nothing under
the generation journal. Any analysis, render, or verification failure aborts
the entire album. A successful `--verify` run stages, decodes, and checks all
outputs before generation publication.

## Progress events

When a v3 state is active, `--progress PATH` emits one
[`batch-progress-v2`](schema/batch-progress-v2.schema.json) object per line.
`PATH` may be `-` for stdout, but binary audio cannot also use stdout. Each
event contains `job_id`, generation number, phase, ordered sequence, and
completed/total counts. Events are `job_started`, `asset_started`,
`asset_completed`, `asset_skipped`, `asset_failed`, `job_completed`, or
`job_failed`; asset events carry index/input/output, and failed events carry a
non-empty error. `job_completed` is emitted only after the generation is
committed. A mixed failure emits `job_failed` and never reports a successful
generation. These terminal events describe the audio generation boundary; an
invocation can still return a final batch-checkpoint or auxiliary
catalogue/report error after a truthful `job_completed`. An identical resume
can repair that control state or auxiliary artifact without republishing
audio.

For an invocation without `--job-state`, Forge retains the v1 progress shape
for compatibility and the historical per-asset publication behavior. It does
not provide the durable generation crash-recovery guarantee described above.

## Dry runs and auxiliary reports

`--dry-run` does not accept `--job-state`, `--progress`, `--keep-going`, or
failure-report controls. It performs planning/analysis and prints intended
outputs without creating output directories, audio stages, generation state,
progress, failure reports, catalogue databases, or external renderer output.
The sole explicit durable exception is
`--dry-run --analysis-cache DIR --warm-cache`, which may populate or evict
bounded cache entries. Without `--warm-cache`, the analysis cache is opened
read-only; cache hits, misses, repair, and eviction are not part of the job
identity.

The all-or-nothing guarantee covers the audio destination generation and its
journal. Opening a configured catalogue can initialize its database, schema,
or WAL before audio work begins. Catalogue asset-record updates and auxiliary
catalogue/fidelity report writes are a separate post-audio-commit boundary;
they occur only after audio publication and can require their own retry/repair
handling.

## Bounds and operational checks

- A v3 state and generation journal contain at most 100,000 assets/members
  and 16 MiB of durable JSON.
- Batch generator provenance is at most 256 UTF-8 bytes. v3 asset paths are
  non-empty UTF-8 strings without control characters; progress and failure
  input/output fields are capped at 4,096 UTF-8 bytes.
- Progress events use a lower-case SHA-256 `job_id`, one of the `rendering`,
  `committed`, or `failed` phases, and cap `phase` at 64 UTF-8 bytes. Their
  `completed` count is at most 100,000 and their error field is capped at
  16 KiB when present.
- Failure reports retain at most 4,096 errors and cap each retained error at
  16 KiB; the complete encoded report is capped at 4 MiB. Oversized errors are
  truncated at a UTF-8 character boundary, and oversized reports set
  `truncated` and count dropped entries. JSON Schema `maxLength` values count
  Unicode code points; the Rust validators enforce the stricter UTF-8 byte
  limits above.
- The published schemas define the portable structural layer. Forge's Rust
  validators are also normative for relationships JSON Schema cannot express,
  including dynamic count equality, ordered writer/format correspondence,
  operation/context alignment, canonical physical paths, live file evidence,
  and aggregate encoded-byte limits. v2 progress serialization validates the
  event, and failure reports expose `to_bytes`/`encoded_bytes` as their wire
  encoders so the 4 MiB truncation boundary cannot be bypassed by serializing
  a mutated report directly.
- Inputs and any live destinations are UTF-8 path records backed by
  regular-file, identity, length, and SHA-256 evidence; a missing destination
  is recorded explicitly as a preimage. Symlink/reparse and hard-link aliases
  are rejected at the relevant publication boundary.
- Parallel independent rendering uses bounded waves (at most 32 assets).
  Outcome/progress reporting remains in input order; generation publication
  uses deterministic canonical destination-path order.
