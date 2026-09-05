# Changelog

Notable user-visible changes are recorded here. Forge follows semantic version
tags and keeps public compatibility commitments in
[COMPATIBILITY.md](COMPATIBILITY.md).

## Unreleased

- No user-visible changes yet.

## 0.189.10 - 2026-09-05

### Added

- Add a self-registering JSON contract catalogue covering all 82 shipped JSON
  Schemas and two governed data documents. Every entry records its family,
  integer version, exact document ID, instance discriminator, lifecycle,
  evolution policy, producers, consumers, and validators.
- Add dependency-free registry checks and negative tests for missing, stale,
  duplicate, non-local, path-traversing, and broken-successor contracts, plus
  meta-schema compilation and registered-sample validation in Rust.

### Changed

- Validate the complete offline schema closure before crates, GitHub Pages, or
  release archives are produced, and derive the archive's governed JSON file
  set from the registry.
- Preserve the exact 15 historical schema IDs through a closed allowlist, and
  distinguish supported older wire/report contracts from retired,
  invalidatable analysis-cache formats.

## 0.189.9 - 2026-09-05

### Added

- Add a versioned exact channel-layout descriptor across Rust, C, Python,
  WebAssembly, REST v3, and the additive gRPC `ForgeAnalysisV3` service. It
  retains per-plane speaker/CICP geometry, explicit overrides, source evidence,
  and renderer bindings while leaving the original gRPC messages unchanged.
- Parse selected-track ISO-BMFF `chnl` and related `dmix` evidence, and bind
  AC-4, MPEG-H, and DTS presentation measurements to the exact renderer
  executable, settings response, and output configuration.
- Publish channel-layout schema v1, analysis-cache schema v5, catalogue report
  v3, service analysis v3, and renderer-adapter report v2 schemas.

### Changed

- Reduce normal `f32` streaming-analysis work by fusing finite-sample
  validation with the per-channel peak reduction reused by True Peak pruning,
  without changing measurement or normalized output bytes.
- Preserve non-default and partial RFC 9639 channel masks through native FLAC
  and WAVE decode, normalization, output, and re-verification instead of
  reducing them to conventional role labels.
- Include the declared and effective exact layout in descriptor-bound request
  identities, cache addresses, and catalogue evidence. Historical report and
  cache schemas remain immutable.

### Fixed

- Apply BS.1770-5 Annex 3 positional weighting from exact speaker geometry and
  exclude LFE while failing closed for unknown or scene-based layouts.
- Preserve the constructible AC-4, MPEG-H, and DTS adapter report v1 Rust APIs
  while their CLIs opt in to additive v2 reports carrying exact channel-layout
  evidence.
- Allow otherwise valid non-canonical WAVE/FLAC speaker masks through output
  preflight when the selected writer can preserve their exact plane order.
- Package the complete public documentation, protocol, and versioned JSON
  schema sets in every native release archive; keep text bytes stable across
  operating systems and verify committed bytes plus EBU checksums before
  creating each archive.

## 0.189.8 - 2026-09-05

### Added

- Add a content-probed codec/container/track registry and immutable
  `InputDescriptor`, plus `--audio-track` for independent normalization,
  analysis, and EBU QC of a selected programme.
- Publish analysis-cache schema v4 and catalogue report/database v2. Their
  identities now include source bytes, decoder route, track, exact frame
  range, channel layout, renderer, and effective processing plan; v1
  catalogues migrate transactionally without losing rows.
- Add a bounded one-response remote snapshot API and evidence schema for
  origins that cannot provide a strong range validator.

### Changed

- Make normalization and QC consume the same descriptor-bound stream, time
  range, selected track, and layout evidence. Native 32-bit integer and 64-bit
  floating-point WAVE analysis now avoids narrowing to `f32`; normalized S24
  remains exact on the existing SIMD analyzer path.
- Bind multi-request remote range sessions to one final representation URI and
  strong ETag, send `If-Range`, and reject encoded or changed responses.
- Choose implicit output formats from the detected codec, preserve lossless
  defaults, and verify the exact FFmpeg encoder and muxer before decoding or
  creating output.
- Keep the existing Rust catalogue record/report API on schema v1 while the
  descriptor-bound API and CLI emit the new request-bound v2 evidence.

### Fixed

- Keep descriptor-bound parallel batches independently restartable when a
  later asset fails, while preserving input-ordered cache and progress output.
- Make decoder selection and PCM contracts independent of misleading file-name
  suffixes.
- Preserve descriptor-bound normalization throughput by hashing the live input
  at transaction boundaries instead of around every immutable-snapshot decode.

## 0.189.7 - 2026-09-04

### Added

- Add bounded EBU QC Data Model 2026-04 semantic validation with optional
  Scenario 1 constraints, exposed through the Rust API and
  `forge-report ebu-qc-validate`. Generated generic and Scenario 1 reports are
  validated before publication.
- Ship the hash-pinned EBU 2026-04 core, timing-extension, and Catalogue API
  schemas with their CC BY 4.0 attribution and a combined XSD validation
  wrapper. Publish and package the EBU report schemas, v2 catalogue pins, and
  Scenario 1 documentation on every supported platform.

### Fixed

- Stop emitting the obsolete `Output/Name=CheckResult` representation. Keep
  `CheckResult` only on check-mode ItemResults and omit report/profile results
  when a generic report contains no checks, matching the 2026-04 data-model
  semantics.
- Publish non-JSON schema assets through GitHub Pages so the catalogue pin URL
  embedded in Scenario 1 reports resolves instead of returning 404.

## 0.189.6 - 2026-09-04

### Added

- Add whole-invocation output planning for the main normalizer, rejecting
  duplicate routes and path, symlink/reparse-point, hard-link, protected-file,
  and conservative platform case aliases before decode. Recursive discovery
  is now deterministic, shared by CLI and watch workflows, and bounded by
  file, entry, and depth limits.
- Add explicit `CreateNew` and `ReplaceUnchanged` publication policies to the
  Rust normalization API, including staged corrected-normalization entry
  points for durable coordinators.
- Publish batch-job schema v2 with a hash-bound `ready_to_publish` checkpoint,
  automatic v1 migration, and process-lifetime sibling locks for batch and
  watch state. A restart can recognize an output committed immediately before
  interruption without re-encoding it.
- Add `--warm-cache` as the explicit opt-in for populating the analysis cache
  during `--dry-run`.

### Fixed

- Publish final normalization audio, reports, and state documents with atomic
  no-clobber semantics for new destinations or identity/length/SHA-256
  compare-and-swap checks for requested replacement. Synchronize the committed
  file and containing directory on Unix, and use write-through moves on
  Windows.
- Keep ordinary dry runs from creating, repairing, or evicting analysis-cache
  entries. Harden state reads and recursive traversal against final-component
  links, Windows reparse points, non-regular files, and unbounded allocation.

## 0.189.5 - 2026-09-04

### Added

- Add an opt-in `--analysis-engine reference` path with committed K-weighting
  and True Peak coefficient bits, scalar fixed-order processing, canonical
  nanodecibel reports, explicit engine IDs, and engine-isolated cache entries.
- Add transactional U8, S16, S24, S32, and F64 streaming-analysis ingress and
  shared non-finite PCM preflight for offline and real-time processors.
- Publish analysis-cache schema v3, delivery-manifest schema v4, and bounded
  service-analysis response v2. Core JSON measurement contracts now
  distinguish finite numbers, measured digital silence (`"-inf"`), and
  undefined values (`null`).

### Changed

- Derive all 100 ms loudness-grid boundaries from exact rational indices and
  use fixed-order numerically stable sums across window, channel, gate, RMS,
  album, dialogue, and real-time energy reductions. The common `f32` path uses
  chunk-independent partials with compensated aggregation and periodic
  rolling-window rebasing; the reference path remains strictly compensated
  per value. The measurement revision is now `forge-bs1770-5-r4`.
- Keep the reproducible stereo fast path within its performance budget by
  sharing the absolute frame ordinal across paired energy reductions and
  amortizing compensated rolling-window rebases over `2^24` frames. A full
  interval remains within one nanoloudness unit of a compensated rebase.

## 0.189.4 - 2026-09-03

### Added

- Add one-shot `prepare_versioned_file`, `VersionedMetadataRepairPlan`, and
  `ExecutedVersionedMetadataRepair` APIs for preflighted metadata repair and
  atomic report publication.

### Fixed

- Bind atomic normalization publication to the staged file actually produced
  by trusted path-based metadata writers. Unexpected staging-path replacement
  is rejected, including in single-track, album, multi-delivery, and segment
  workflows.
- Validate rewritten Opus tags before replacement, synchronize the replacement
  file, preserve basic platform permissions, and keep a failed rewrite from
  publishing malformed metadata.
- Reject `forge-metadata-repair --output` aliases observed during preflight
  and immediately before publication, including normalized paths, hard links,
  symlinks, Windows reparse points, and common platform filename aliases to the
  request, media source, repair destination, or decoded references. Reports are
  staged before repair and atomically replaced so failures detected before
  replacement preserve existing report bytes.
- Keep gRPC worker permits and cancellation registrations until detached
  blocking analysis actually exits after a timeout or client disconnect. Add
  bounded cancellation checkpoints and classify disconnected requests as
  cancellations instead of server errors.

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
