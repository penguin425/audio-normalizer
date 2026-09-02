# Changelog

Notable user-visible changes are recorded here. Forge follows semantic version
tags and keeps public compatibility commitments in
[COMPATIBILITY.md](COMPATIBILITY.md).

## Unreleased

- No user-visible changes yet.

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
