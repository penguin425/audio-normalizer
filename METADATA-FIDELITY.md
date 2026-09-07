# Metadata fidelity policy and field ledger

Forge's metadata writers can use the container-independent policy and ledger
primitives in src/metadata_fidelity.rs. The module does not parse RIFF, ID3,
Vorbis, ISO-BMFF, or another container. A container adapter discovers fields
and supplies one MetadataLedgerEntry for each field it considers. This keeps
unknown bytes and container-native fields visible to the same publication gate
without forcing them through a lossy generic tag model.

## Policies

MetadataPolicyConfig is explicit and validated before any output is published:

- preserve permits best-effort mappings, but every loss or omission must be
  reported.
- strict blocks publication when a field is dropped or has any non-none loss
  class. It is the lossless publication gate.
- strip permits drops only under strip_scope=all or
  strip_scope=selected. A selected scope must list exact locators from the
  source metadata inventory (the container-specific source locator, including
  its occurrence/ordinal where applicable), and every listed locator must have
  a corresponding dropped ledger entry. A strip locator is not a generic field
  name, a destination path, or a filesystem path. Fields outside the declared
  scope must remain lossless; a strip request does not authorize unrelated
  representation or semantic changes.
- legacy_generic names the historical primary/first generic-tag behavior. It
  is available for compatibility and does not claim full-container fidelity.

The CLI keeps `legacy_generic` as the behavior selected when no metadata policy
is supplied. Selecting `preserve`, `strict`, or `strip` opts into the explicit
field-fidelity contract; naming `legacy-generic` makes the compatibility choice
visible without changing its historical scope.

The strip_locators array is sorted in programmatically constructed policies;
duplicates are rejected rather than silently removed. Deserialized policies
must already be in that canonical order.
Non-strip policies must use strip_scope=none and an empty locator list.
Strip scope applies to discovered source metadata. Metadata deliberately
generated from authoritative output measurements (for example an explicitly
requested BWF loudness block or an output format's loudness tag) remains
visible as a destination-only `recomputed` ledger entry rather than being
misreported as preserved source data.

Writer evidence promotes only exact destination-only regions, exact
output-adapter preimage regions, and container ancestors whose complete direct
child set is independently bound. It does not declare an existing composite
tag block or packet lossless merely because ReplayGain or R128 fields inside
it changed. Until a container adapter can separate and verify every owned and
unowned child field, `preserve` reports that composite rewrite as a
representation loss and `strict` blocks publication.

A `dropped` entry is source-backed: it carries the source inventory locator and
before hash, has no destination locator, and has no after hash. This makes a
policy-driven removal distinct from a destination-only recomputed field.

## Strict support boundary in v0.189.14

The strict normalization claim is intentionally limited to the following
evidence-backed clean paths:

| Operation | Strict status | Evidence boundary |
| --- | --- | --- |
| clean WAVE -> FLAC | supported | Exact writer preimages and generated FLAC padding are bound in the field ledger. |
| clean WAVE -> Vorbis | supported | Exact output-adapter/writer preimages are bound in the field ledger. |
| clean WAVE -> Opus | supported | Exact output-adapter/writer preimages are bound in the field ledger. |
| clean WAVE -> M4A | supported | ISO-BMFF metadata descendants and their fully accounted ancestors are bound in the field ledger. |
| metadata-only clean FLAC | supported | The private stage is read back and the strict ledger is verified before the single-file commit. |

These rows describe clean, evidence-backed paths; they do not imply generic
full-container fidelity for every input. Strict metadata mutation involving
MP3 or WAVE, Sound Check, or a rewrite of an existing composite tag is
fail-closed until a field-specific adapter can prove every affected child and
representation. A generic tag rewrite or a changed loudness field is not
evidence of losslessness.

## Field ledger

Each entry records:

- a stable registry field name and the outcome (preserved, mapped, recomputed,
  or dropped);
- a loss class and non-empty reason;
- source and destination locators where applicable; and
- lowercase SHA-256 values for the source and destination field bytes.

Preserved fields require identical before/after hashes. Dropped fields require
a source locator and before hash and must not claim an after hash. Entries are
sorted by field, source locator, and destination locator; duplicate identities
are rejected. This makes reports reproducible even when adapters discover
fields in different traversal orders.

## Revision evidence and report contract

Reports carry both the metadata registry revision and sample-time conversion
revision. When a sample-indexed adapter runs, the evidence also records its
source and output rates, signed crop origin as a canonical decimal integer,
and exact rounding rule; metadata-only comparisons record a null transform.
The current constants are metadata-registry-v1 and sample-time-transform-v1;
a future incompatible registry or timing rule requires a new revision and a
separately reviewed migration.

Metadata-only transaction fingerprints also include the writer revision. The
current writer revision is `forge-metadata-writer-v1`; any change to the writer
plan or to writer meaning must bump that revision and receive a separately
reviewed migration.

The serialized report is governed by
schema/metadata-fidelity-report-v1.schema.json. It is a new contract and does
not change the existing metadata-repair-request-v1/v2 or
metadata-repair-report-v1/v2 contracts. MetadataFidelityReport::validate must
be called for reports received across a JSON/API boundary.
MetadataFidelityReport::require_publication must be called immediately before
a transaction commits its staged output.

The report's publication object records allowed or blocked and contains stable
field IDs for every blocking entry. A report may therefore be retained for
review even when strict or selected-strip policy prevents publication.

## Bounded inventory and transaction boundary

The companion registry-backed inventory is policy-independent: it discovers
metadata and structural regions for supported WAVE, FLAC, MP3, Ogg, and
ISO-BMFF containers, including repeated and unknown regions. Each region keeps
its physical order, container locator, extents, byte length, and SHA-256. Scan
issues remain visible in the inventory, and per-item, aggregate, entry-count,
Ogg-packet, nesting, and retained-raw-byte limits bound the discovery work.
Its public contract is
schema/metadata-inventory-v1.schema.json.
JSON consumers must decode through `MetadataInventory::from_json_slice`, which
caps the encoded document at 128 MiB before allocation and validates all
instance-relative limits. `MetadataInventory` intentionally does not expose an
unconstrained serde `Deserialize` implementation; the registry also applies
absolute 100,000-entry and 16 MiB retained-metadata ceilings in addition to
caller-selected lower limits.

The restartable transaction layer is deliberately single-file and
metadata-only. It binds the source identity and semantic fingerprint, copies a
stable source snapshot into a private stage, runs the caller's mutation and
readback verification against that stage, durably records a ready phase, and
performs one compare-and-swap publication. A process restart can recreate a
missing stage, resume a verified stage, or recognize a committed output. Its
state contract is schema/metadata-job-v1.schema.json. Generation-level
all-or-nothing album/batch publication is not part of this release; it is the
planned v0.189.15 follow-up.

The high-level loudness writer accepts only the library-issued stage capability
borrowed inside `MetadataTransaction::stage`; safe external code cannot
construct one for a live source pathname. Requested Sound Check data is read
back again after the complete writer set, so a later ReplayGain or ISO-BMFF
rewrite cannot silently remove an earlier verified value.

This transaction is a crash-recovery and cooperative-concurrency mechanism,
not a cryptographically authenticated journal. The state directory (including
the state file and staged artifacts) and the source file's containing directory
are a trusted boundary: they must stay under a trusted owner for the lifetime
of the job. A hostile writer that can replace both the JSON state and the
recorded stage can forge internally consistent evidence; likewise, portable
pathname-based publication cannot close the last rename race against a hostile
source-directory owner. Callers crossing that trust boundary need an
authenticated higher-level protocol and trusted storage.

When the CLI is also asked for `--metadata-report`, it validates, writes, and
synchronizes the report to a private sibling stage before committing the audio
path. It then publishes the audio and report in that order. Two destination
renames cannot form one portable filesystem transaction: a crash or competing
destination change in that final gap can leave committed audio without the
report, and the command reports that condition explicitly. A metadata-only job
keeps the report in its durable verification evidence so a rerun can publish
it without repeating the mutation. General multi-path recovery is part of the
v0.189.15 generation transaction rather than an implied guarantee here.

The report destination path is routing metadata, not operation semantics. It
must remain outside a metadata transaction's semantic fingerprint and job
identity: changing only the caller-selected report path must not cause the
audio mutation to run again or make an otherwise identical job incompatible.
After the audio transaction has produced validated, durable evidence, a caller
may republish that evidence-backed report to its selected destination path.
Before doing so, the caller must preflight the path as a safe report target
(regular-file/parent policy, no source/state alias or symlink escape, and an
atomic staged write); the path itself must not be accepted as evidence. A
rerun may therefore use another safe report path while retaining the same
semantic fingerprint, and must verify the report again before each write.

Without `--metadata-job-state`, the historical `--write-tags` compatibility
path applies its requested metadata writers sequentially to the live file. A
later writer failure can therefore leave earlier tag changes in place. Select
an explicit metadata policy together with `--metadata-job-state` when staged
readback verification and single-file crash recovery are required.

For resampling, the sample-time transform maps supported sample-indexed BWF/DAW
fields with checked integer rational arithmetic, an explicit tie rule, and the
source crop origin. XML-bearing timing is intentionally opaque and is reported
for adapter-specific handling rather than rewritten by this generic layer.

## Adapter integration

Container implementations should:

1. Discover all representable and unknown fields, including repeated fields and
   their ordinal in each locator.
2. Compute field-byte hashes before and after the transformation.
3. Use mapped only when the source and destination semantics are known, and
   recomputed for values deliberately regenerated from authoritative evidence.
4. Mark unsupported or policy-driven removal as dropped with the relevant loss
   class.
5. Attach the exact registry and timing revisions used for the operation.
6. Run the publication gate before the output transaction's final rename.

The module intentionally leaves actual container discovery and transaction
staging to the corresponding adapters and workflow layers.
