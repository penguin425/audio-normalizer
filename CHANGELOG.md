# Changelog

Notable user-visible changes are recorded here. Forge follows semantic version
tags and keeps public compatibility commitments in
[COMPATIBILITY.md](COMPATIBILITY.md).

## Unreleased

- No user-visible changes yet.

## 0.189.3 - 2026-09-03

### Added

- Add bounded `StableInput` snapshots with versioned SHA-256
  `InputContentBinding`, typed capture/verification errors, in-memory input
  support, and live-source identity checks.
- Add `BoundAnalysis` and bound single-file/album normalization APIs so a
  reusable measurement carries its input bytes, decoder route, effective
  channel layout, resampling request, and measurement revision.
- Add checked gain and PCM-protection entry points, plus side-effect-free
  `Plan` and output-format request validation for applications that need
  explicit diagnostics.

### Fixed

- Keep analysis, cache hits, metadata copying, rendering, and codec correction
  on one immutable input snapshot. Single-file, album,
  batch/watch, segment, and multi-delivery publication now rejects source
  replacement, same-length mutation, and protected hard-link aliases before
  committing an output.
- Stop trusting legacy unbound precomputed `Analysis` values for rendering;
  compatibility entry points remeasure the captured bytes and require an
  exact match, while the additive bound APIs safely reuse cache results.
- Include the decoder route and measurement revision in analysis-cache
  addressing, and preserve explicit channel-layout requests through
  post-encode verification.
- Validate every plan-wide finite value, limiter setting, codec bitrate,
  encoder quality, output sample rate, channel geometry, and public PCM
  buffer before decoding, starting an optional encoder, or touching a
  destination. Non-finite PCM rejection is transactional.
- Derive multi-delivery source hashes from the exact snapshot that was
  normalized, and bind segment request, manifest, and audio reads to bounded
  snapshots instead of independently reopening mutable paths.
- Avoid durability flushes for process-local input snapshots while retaining
  complete content hashing and live-source verification, reducing the latency
  added by stable input capture.

## 0.189.2 - 2026-09-03

### Fixed

- Follow EBU Tech 3342 loudness-range population rules at the end of finite
  programmes, including the 1.5-second post-signal silence, inclusive gates,
  and rank-based percentiles, without extending integrated loudness or other
  measurements. Explicit dialogue ranges no longer append that programme-level
  LRA tail when deriving dialogue loudness.
- Measure finite-signal True Peak with zero-valued boundaries and a complete
  FIR tail, and select the smallest integer oversampling factor that reaches at
  least 192 kHz for supported 8–384 kHz inputs. Arbitrary factors retain a
  scalar oracle and use SIMD where available; stereo scheduling avoids the
  parallel-pass overhead at the common 48 kHz rate. Offline limiting now
  protects the same boundary response without emitting or counting virtual
  samples, and timeline reports attribute the EOF response only to their final
  real interval.
- Preserve decoder channel-layout provenance and reject ambiguous or
  scene-based speaker weighting unless the caller supplies an explicit layout.
  Route 6.1 stereo downmixes through their centre-back-aware matrix.
  Publish analysis-cache schema/layout v2 and segment plan/report schemas v2,
  preserving the immutable v1 schemas while requiring old cache entries and
  pre-0.189.2 segment plans to be regenerated.
- Inspect RFC 9639 FLAC channel-mask comments across metadata revisions and
  use every complete standard mask as authoritative speaker evidence during
  analysis. Malformed, conflicting, partial, or reserved-bit masks remain
  ambiguous, while normalization fails before creating an output when its
  writer cannot preserve the measured speaker order.
- Preserve exact channel roles for conventional multichannel Ogg Opus and
  uncompressed DSF/DSDIFF inputs instead of replacing known positions with a
  generic channel count. FFmpeg-backed AAC and ALAC output is limited to mono
  or stereo until its written layout can be recovered authoritatively;
  Vorbis validates its sample-rate, channel-count, and managed-bitrate tuple
  against the libvorbis setup tables before starting FFmpeg.
- Reject placeholder channel layouts synthesized for ISO-BMFF PCM/FLAC and
  legacy multichannel ALAC, and keep native and browser decode limits aligned.
  Legacy public decode/load adapters now reject layouts whose provenance they
  cannot return, while additive layout-aware adapters preserve that evidence.
  C ABI v1, Python, and browser analysis now document their lack of a layout
  override; browser limits distinguish the 32-channel decoder ceiling from
  the two-channel implicit-layout ceiling.
- Detect RIFF, RF64, and BW64 WAVE input by its file signature as well as its
  suffix. Reject zero-channel, structurally inconsistent, partial-frame,
  data-before-format, and duplicate-format/data inputs before decoding, and
  apply decoded-sample limits to the same data chunk that is actually read.
  Honour the declared RIFF or `ds64` boundary, require `ds64` first in large
  containers, and accept valid extensible-format payloads longer than the
  mandatory 22-byte extension. Emit and account for the required RIFF pad byte
  after odd-length PCM data, including in RF64/BW64 `ds64.riffSize`.
- Treat every complete standard WAVE speaker mask as authoritative during
  analysis, restoring exact positional roles even for sparse layouts. Output
  still fails closed unless the selected writer can preserve that order.
  Public writers validate sample rate, buffer geometry, header arithmetic,
  metadata chunk identifiers, and codec configuration before touching the
  destination; FFmpeg-backed buffer writes publish from a staging file.
- Detect duplicate hard-linked album references and same-size content changes
  around decoded-reference measurement. WAVE metadata repair now rejects
  malformed declared boundaries or out-of-container bytes rather than copying
  an unverifiable byte range.
- Bind metadata-repair sources and decoded album references to private,
  hash-verified snapshots, apply aggregate snapshot limits, and reject output
  aliases, symlinks, multiply-linked destinations, and no-clobber races before
  publication.
- Distinguish MPEG dual-channel audio from conventional stereo and joint
  stereo. Dual-channel inputs now require an explicit layout, and a semantic
  mode change within one stream is rejected before the changed PCM is exposed.
  AAC and MP3 writers reject sample-rate or bitrate requests that their native
  encoders would otherwise silently resample, clamp, or round. AAC excludes
  its 7.35 kHz mode, and ALAC/FLAC reject rates outside Forge's shared
  8–384 kHz decode and measurement contract before opening the destination.
- Reject non-finite IEEE-float samples in browser and C-ABI WAVE analysis
  before analyzer state changes, matching the native streaming contract.
- Omit undefined ReplayGain and RFC 7845 R128 gain fields for silent
  programmes or material too short for integrated loudness, instead of
  serializing infinity or a misleading zero gain.
- Generate release SBOMs with a checksum-pinned Syft binary instead of
  executing a mutable installer inside the privileged publication job. Tag
  builds must resolve to protected `main` and are rechecked against the remote
  immediately before publication; future GitHub Releases are immutable.
- Correct the EBU R 128 s2 adapted-streaming compliance range to −20…−16 LUFS,
  and treat the AES77 music-track and interstitial `+0.2 LU` values as upper
  tolerances rather than symmetric tolerances.

## 0.189.1 - 2026-09-03

### Fixed

- Apply the ITU-R BS.1770 absolute and relative integrated-loudness gates with
  strict threshold comparisons in track, rolling, and album measurements, and
  combine album block populations without an additional aggregate allocation.
- Treat the true-peak ceiling as a strict maximum in single-file, album,
  multi-delivery, and segment verification instead of widening it by the
  loudness-target tolerance.
- Renew the true-peak limiter hold throughout sustained limiting so release
  modulation cannot create a new inter-sample peak, and invalidate analysis
  caches produced by the previous gate implementation.
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
