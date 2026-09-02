# Changelog

Notable user-visible changes are recorded here. Forge follows semantic version
tags and keeps public compatibility commitments in
[COMPATIBILITY.md](COMPATIBILITY.md).

## Unreleased

- No user-visible changes yet.

## 0.189.1 - 2026-09-03

### Fixed

- Apply the ITU-R BS.1770 absolute and relative integrated-loudness gates with
  strict threshold comparisons in track, rolling, and album measurements, and
  combine album block populations without an additional aggregate allocation.
- Treat the true-peak ceiling as a strict maximum in single-file, album,
  multi-delivery, and segment verification instead of widening it by the
  loudness-target tolerance.
- Reject NaN and infinite PCM before any analysis state changes, and fail
  closed on corrupt decoder packets instead of silently shortening the
  programme and measuring different audio.

## 0.189.0 - 2026-09-02

### Added

- Validate S-ADM frame structure by its normative XML paths and require the
  exact ITU-R BS.2125-1 root-frame version declaration.
- Reconstruct ADM definitions across logical S-ADM frames and verify declared
  `new`, `changed`, `extended`, and `expired` ID status transitions after
  Divided-Frame chunks have been combined.

### Changed

- Clarify that current IAMF conformance fixtures are structural checks rather
  than automated OAR renders, and record the provisional standards-first
  implementation sequence through v0.203.

### Fixed

- Reject misplaced frame headers, frame formats, transport mappings, payloads,
  and malformed or duplicate `changedIDs` entries without confusing ordinary
  ADM payload references for change declarations.
- Reject foreign-namespace lookalikes, ambiguous payload-container shapes,
  multiple XML roots, out-of-root content, and misplaced XML declarations;
  bound frame count and XML bytes, depth, elements, attributes, and text.
- Compare canonical ADM definitions so harmless attribute ordering,
  formatting whitespace, comments, processing instructions, entities, and
  namespace-prefix spelling do not create false state changes. Treat legacy
  `ltime` as `lstart` and compare supported timing spellings with exact
  rational arithmetic, including signed local start values.
- Enforce `new` as the first appearance in the complete flow, including after
  an earlier `expired` declaration, and include bounded state-error evidence in
  the report.
- Reject XML 1.0 well-formedness edge cases not rejected by the streaming
  tokenizer, preserve meaningful whitespace during state comparison, and
  bound namespace expansion and canonical state memory. Recurrence and
  divided-frame grouping now remain linear in the number of supplied frames.

## 0.188.0 - 2026-09-02

### Added

- Classify complete S-ADM flows as Full-Frame, Intermediate-Frame,
  Mixed-Frame, or Divided-Frame and validate type-specific `countToFull`,
  `numMetadataChunks`, and `countToSameChunk` declarations.
- Validate decimal, sample-based, and legacy date-bearing S-ADM times with
  exact rational arithmetic instead of floating-point tolerances.

### Fixed

- Group ordered Divided-Frame chunks by their shared base `frameFormatID`
  before checking frame indices and timing, so conforming sparse chunk flows
  are no longer rejected as duplicate or non-contiguous frames.
- Accept a `divided` first frame in a Divided-Frame flow and reject invalid,
  duplicate, out-of-order, or missing final chunk indices.

## 0.187.0 - 2026-09-02

### Added

- Add bounded ADM semantics QC for dialogue kinds, alternative-value-set
  selection references, deterministic default programme selection, explicit
  presentation intent, importance-threshold planning, and non-authoritative
  tag inventories.
- Separate normative metadata failures from opt-in operator policies and mark
  all reports as metadata-only evidence rather than renderer or audio
  compliance proof.

## 0.186.0 - 2026-09-02

### Added

- Add bounded ADM personalization metadata QC for parent and
  `alternativeValueSet` gain/position ranges, including explicit unbounded
  gain detection and an optional, explicitly scoped ITU-R BS.2168-0
  emission-profile range subset.
- Mark reports as metadata-only evidence so continuous loudness and true-peak
  compliance cannot be inferred without independently measured endpoint
  renders.

## 0.185.0 - 2026-09-02

### Added

- Add metadata-repair schema v2 for deriving ISO-BMFF album `alou` from the
  combined population of complete BS.1770 gating blocks across bounded decoded
  references, with aggregate byte/sample limits and per-reference hash
  evidence.

## 0.184.0 - 2026-09-01

### Added

- Add security reporting, contribution, compatibility, deprecation, and
  curated changelog policies.
- Add complete crates.io discovery metadata and a bounded package-content and
  registry-size validator.
- Point installation at the currently available GitHub Release instead of an
  unpublished crates.io package.

### Changed

- Cancel superseded pull-request CI, supply-chain, and fuzz workflows, and
  start optional cross-platform jobs only after the required Rust gate passes.
- Publish releases through a tag-restricted GitHub environment and include the
  maintenance policies in release archives.
- Select the highest published semantic version as GitHub's Latest release,
  independent of concurrent workflow completion order.

## 0.183.0 - 2026-09-01

### Added

- Write native ISO-BMFF `ludt/tlou` track loudness and `alou` album loudness
  for M4A and ALAC normalization and metadata-only workflows.
- Verify quantized native values after writing while preserving every `mdat`
  payload by hash.

## 0.182.0 - 2026-09-01

### Fixed

- Derive album ReplayGain, Sound Check, and native loudness metadata from the
  final decoded outputs instead of the pre-encode analysis.

## 0.181.0 - 2026-09-01

### Added

- Add `forge-doctor` capability reporting for formats, encoders, optional
  runtimes, and CPU acceleration.

## 0.180.0 - 2026-09-01

### Fixed

- Enforce Apple HLS APAC loudness rules and reject conflicting ISO-BMFF
  container loudness metadata.

## 0.179.0 - 2026-09-01

### Added

- Add conservative ISO-BMFF `ludt/tlou` metadata writing with offset repair,
  media-payload hashing, and post-write audit.

Earlier release notes and exact comparisons are available on the
[GitHub Releases page](https://github.com/penguin425/audio-normalizer/releases).
