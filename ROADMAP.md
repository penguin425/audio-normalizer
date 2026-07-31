# Forge implementation roadmap

This roadmap separates standards-backed delivery work from experimental
signal processing. Priority is based on correctness risk, user reach,
availability of normative specifications and test material, implementation
cost, and the ability to produce auditable evidence.

## Shipped foundation

Forge already provides:

- ITU-R BS.1770-5 / EBU R 128 integrated, momentary, short-term, LRA, and
  true-peak measurement with official EBU and ITU conformance jobs.
- Track and duration-weighted album normalization, codec re-verification,
  limiter, sample-rate conversion, integer PCM dither, ReplayGain, and BWF
  loudness metadata.
- EBU Tech 3285 v2 BWF `bext` QC for fixed production fields, date/time,
  sample-based TimeReference, version/UMID/reserved-byte consistency, loudness
  metadata ranges, and CodingHistory line structure.
- WAVE/RF64/BW64, AIFF/AIFC, CAF, AU, FLAC, MP3, Ogg Opus/Vorbis, and
  ISO-BMFF/fMP4 structural QC.
- Bounded RFC 9559 Matroska/WebM QC for EBML structure, tracks, blocks,
  SeekHead/Cues, CRC-32, codec-private data, and Opus gapless timing.
- Dependency-free ADTS and LOAS/LATM AAC QC, including ASC SBR/PS signalling,
  decoded timing, and ISO-BMFF edit-list/sample-group gapless reconciliation.
- Dependency-free AC-3/E-AC-3 elementary-stream QC for bounded syncframes,
  complete BSI, `dialnorm`, interpreted DRC gain words, channel mode, strict
  Atmos/JOC Extension Type A signalling, six-block access-unit and
  independent/dependent presentation grouping, plus authoritative external
  E-AC-3 JOC render checks.
- Bounded standalone, unfragmented, fragmented, and CENC-protected ISO-BMFF
  AOMedia IAMF v1.1
  QC with
  `iamf` brand/sample-entry/`iacb` configuration validation, streaming sample
  table or `moof`/`traf`/`trun` decapsulation, supported codec-config
  4CC/frame/roll/decoder semantics, complete Audio Element parameter,
  scalable-channel/expanded-layout and Ambisonics validation, descriptor ID
  uniqueness, audio-frame linking, plus external OAR v1.0.0
  presentation-render loudness, true-peak, duration, and reference checks.
  Fragmented carriage resolves `trex`/`tfhd` defaults, signed data offsets,
  sample-description changes, decode-time continuity, fragment sample groups,
  sync/CTS constraints, and pinned AOMedia interoperability vectors. Encrypted
  carriage validates `enca`/`sinf`/`frma`/`schm`/`schi`/`tenc`,
  `cenc`/`cbcs` full-sample policy, IV geometry, and `senc` or paired
  `saiz`/`saio` sample coverage without accepting keys or parsing ciphertext.
- Pinned AOMedia OAR/IAMF verification across 42 artifacts from 14 libiamf
  v1.1.0 vectors, covering standalone, unfragmented, and fragmented carriage,
  LPCM, channel-based and Ambisonics elements, localized annotations, anchored
  loudness, parameter animation, intentionally invalid metadata, and exact
  retained upstream packaging findings without claiming renderer parity.
- Bounded MPEG-TS/M2TS QC for packet layout, continuity, PAT/PMT CRC and
  programme maps, audio PES headers, and PTS continuity.
- Bounded SMPTE ST 377-1 MXF QC for KLV framing, partitions and links,
  OP1a/OP-Atom, RIP, index declarations, Generic Container essence, sound
  descriptors, channel assignment evidence, and detected AS-11/DPP
  structural/audio constraints.
- Bounded, local-only IMF package QC for AssetMap chunk containment, PKL
  SHA-1/SHA-256 and size verification, CPL references, virtual-track timing,
  MCA label evidence, ApplicationIdentification, and auditable common
  structural/audio constraints.
- HLS MPEG-TS segment audits with discontinuity-aware programme/PID/codec
  stability and cross-segment audio PTS continuity.
- HLS 2nd Edition draft-22 Low-Latency profile checks for Partial Segments,
  preload hints, server-control and blocking-reload declarations, delta
  updates, Rendition Reports, live-edge values, and cross-rendition
  discontinuity state.
- Dynamic and low-latency DASH `SegmentTemplate` QC for availability-window
  inputs, UTC timing, Period continuity/connectivity, MPD events, CENC
  protection, ServiceDescription latency, and local chunked-CMAF evidence.
- DASH `SegmentList`/`SegmentBase` inheritance, addressing-mode consistency,
  list timeline/count/range checks, bounded local `sidx` parsing, and dynamic
  availability-completeness geometry.
- Successive full-MPD update QC for identity, publish-time monotonicity,
  Period/AdaptationSet ordering, fixed Representation sets, inherited
  functional properties, and overlapping segment-reference equivalence.
- Bounded MPD Patch application and successive-update QC with namespace-aware
  RFC 5261 element, attribute, text, comment, processing-instruction, and
  namespace selectors plus add/replace/remove positioning.
- Explicitly allowlisted DASH clock/origin observation with per-request time
  and byte limits, redirect reauthorization, request caps, clock-offset
  checks, and a redacted fetch manifest.
- Cross-language CMAF audio alignment for HLS Rendition Groups and
  DASH Adaptation Set Switching descriptors, including normalized
  timescale/presentation-offset boundaries and discontinuity state.
- HLS, DASH, and CMAF package checks; ISO loudness and MPEG-D DRC metadata.
- DASH CICP ProgramLoudness/AnchorLoudness descriptor syntax, inheritance,
  and local ISO-BMFF measurement reconciliation; optional paired MPEG-D DRC
  container evidence; and HLS timed-ID3 validation for MPEG-TS PES and CMAF
  `emsg`/`aid3`, including bounded ID3v2/RVA2 and timestamp checks.
- ADM, S-ADM, presentation-aware rendering/QC, C2PA validation, CI comparison,
  real-time processing, LV2, and CLAP integration.
- Bit-exact plugin parity checks compare the streaming CLI, CLAP host adapter,
  and LV2 ABI with the shared live processor on Linux, macOS, and Windows,
  including nonuniform block sizes, automated gain, true-peak limiting, and
  the LV2 Core-designated 5 ms latency output.
- Rust source compatibility from the v0.94.0 baseline, with an explicit
  pre-1.0 stability contract, all-feature `cargo-semver-checks` gating against
  the latest release tag, and a downstream-style public API contract test.
- Versioned C ABI v1 for bounded local-file analysis with a fixed
  caller-owned result layout, UTF-8 error contract, decoded-sample limit,
  packaged header/shared libraries, and real C consumer tests on Linux,
  macOS, and Windows.
- Dependency-free Python 3.10+ bindings over C ABI v1, with immutable typed
  results, explicit decoded-sample bounds, deterministic native-library
  selection, concurrent-call coverage, and self-contained platform wheels for
  Linux x86-64, macOS ARM64/x86-64, and Windows x86-64.
- Dependency-free browser WebAssembly analysis over the shared Rust DSP core,
  with WAVE and interleaved Float32 entry points, TypeScript declarations,
  fixed resource limits, and no filesystem, network, normalization, or encoder
  capabilities.
- EBU QC Items for channel count, clipping, duration, dropouts, loudness,
  phase reversal, test tones, clicks, minimum average level, silence, true
  peak, hum/buzz, band-limited noise, cross-talk, panning, LFE/centre
  assignment, and mono delivery.
- Forge-specific decoded-audio rules for DC offset, inter-channel sample
  delay, stuck samples, and discontinuities, with bounded/coalesced evidence.
- SPDX/CycloneDX release SBOMs, verified SLSA provenance, byte-reproducible
  Linux archives, dependency policy, Rust 1.89 MSRV checks, cargo-binstall
  metadata, and generated Homebrew/Scoop/WinGet manifests.
- Delivery-manifest v1/v2-to-v3 migration with count/schema validation and
  atomic replacement; `forge-report explain` covers failed compliance, EBU QC,
  container, codec, ADM/profile, and presentation rules with stable category
  and rule IDs, source, exact structured observation/location, and remediation.
  The v1 compliance-only output remains available while v2 is the all-QC
  default contract.
- Versioned platform loudness profiles with canonical identifiers, first-party
  source and verification dates, explicit published-policy versus engineering-
  reference classification, stable pinned aliases, and runtime caveats.
- Apple Sound Check compatibility metadata for MP4/M4A, MP3, AIFF, and raw AAC,
  with strict ten-word `iTunNORM` parsing, container-native read/write, exact
  post-write round-trip verification, and an explicit non-normative
  R128/ReplayGain engineering mapping because Apple does not publish the field
  layout or analyser.
- Metadata-only loudness normalization dispatch with dependency-free RFC 7845
  `R128_TRACK_GAIN`/`R128_ALBUM_GAIN` rewriting for existing Ogg Opus files,
  ReplayGain 2.0 for other supported tag containers, exact post-write readback,
  and chained-Opus coverage without requiring the optional encoder.
- Content-class compliance profiles with inclusive LRA and PLR ranges,
  machine-readable boundary semantics, and a strict EBU R 128 s2 v3.0
  mostly-music `PLR < 15 dB` gate for the −16 LUFS alternative.
- Bounded iXML XML/root/track-list validation, one-based source/interleave
  index checks, PCM channel reconciliation, and ADM `chna` track-map
  cross-checks.
- ITU-R BS.2088-2 `axml`/`bxml`/`sxml` QC with bounded UTF-8/gzip parsing,
  EBUCore envelope checks, serial subchunk/alignment/sample validation,
  ADM/S-ADM placement and independence checks, and byte-preserving output.
- MP3 protected-frame CRC-16 validation, Fraunhofer VBRI seek-table and
  stream-count reconciliation, and LAME ReplayGain/tag-CRC validation with
  bounded mismatch evidence.
- Bounded MP3 free-format structural QC with unique three-frame geometry
  inference, per-frame padding and full-stream boundary validation, and
  explicit separation from unsupported decode/normalization paths.
- RFC 8486 Ogg Opus mapping-family-2 Ambisonics QC for allowed order/channel
  geometry, ACN/SN3D declarations, optional non-diegetic stereo, mixed-order
  inactive ACNs, and consistent chained layouts.
- Bounded decoded reference-audio comparison with sample alignment, optimal
  channel assignment/permutation and polarity evidence, full-overlap null
  residual/peak, exact-sample ratio, and excerpted one-third-octave spectral
  error, explicitly classified as non-normative engineering QC.
- Read-only DSF and uncompressed DSDIFF analysis with bounded structural QC,
  declared bit-order/channel mapping, a versioned cascaded half-band and
  21 kHz low-pass decimation policy, 88.2/96 kHz BS.1770 measurement, and
  explicit rejection of DST-compressed payloads.
- Versioned before/after normalization evidence for track, batch, and album
  workflows, including the exact intended pre-codec measurement, bounded gain
  envelope, limiter amount, clipping/ceiling counts, decoded-output endpoints,
  SHA-256 provenance, and codec loudness/peak/duration drift.
- Resumable independent-track batch jobs with atomic per-output checkpoints,
  input/settings/output SHA-256 binding, missing-output recovery,
  changed-output rejection, and schema-validated lifecycle NDJSON.
- Opt-in content-addressed core analysis cache with streaming input SHA-256,
  request and algorithm revision binding, atomic schema-validated entries,
  corruption recovery, read-only operation, and bounded FIFO eviction.
- Stable-file watch folders with symlink-safe bounded discovery, durable
  atomic processing state, settings and input/output SHA-256 binding,
  restart recovery, explicit failed-item retry, and one-shot scheduling.
- Versioned SQLite catalogue with source/output SHA-256, BS.1770-5/EBU R 128
  measurement and algorithm revisions, selected profile, Forge version,
  bounded structured provenance, transactional deduplication, and atomic
  per-invocation JSON evidence export.
- Versioned, standard-library-only performance harness for generated one-hour
  stereo and 7.1 WAVE normalization, FLAC/MP3 analysis, and bounded
  pathological WAVE QC, with normalized CPU/RSS evidence and compatible-host
  baseline regression gates.
- Versioned multi-delivery optimization for two to 32 codec/profile outputs,
  using one conservative target, ceiling, and iteratively corrected gain;
  staged post-metadata re-decoding, explicit infeasibility, path-alias and
  overwrite safety, and schema-validated hash/measurement/profile evidence.
- Versioned two-pass segment-aware catalogue normalization for two to 4096
  ordered sources, with SHA-256-bound plans, identical adjacent boundary
  gains, capped cubic smoothstep dB ramps, per-segment memory limits,
  re-decoded codec/true-peak/duration evidence, and explicit sequential
  atomic-publication semantics.
- Bounded AC-4 licensed/reference-decoder adapter protocol for complete
  presentation enumeration, current ETSI TS 103 190-1/-2 dialnorm source and
  loudness-correction metadata, input/adapter/render SHA-256 evidence,
  independent BS.1770 measurement, and process/response/PCM safety limits.
- Bounded AAF stored-format, dynamic MetaDictionary, and core object-model/Edit
  Protocol QC with CFB/file/root identity, property/reference-index decoding,
  dynamically declared class/property/type graphs and extension values,
  exactly-one-owner object graphs, inherited required properties, unique
  identifiers, Mob/Slot/SourceClip derivation, Sequence/Transition timing,
  effects, locators, protocol-labelled material/track constraints, all 20
  AS-01 and three AS-05 effect-dictionary profiles, scalar parameter type and
  range checks, documented import fallbacks, fixed safety limits, and pinned
  pyaaf2/Avid plus AAF SDK reference interoperability fixtures.
- Bounded AES31-3 EDML ADL project QC with plain-ASCII and record limits,
  core section/header validation, source and event identity/reference checks,
  channel-map consistency, integer/fractional/drop-frame sample timing,
  source bounds, automation timing, and explicit extension evidence.

## P0 — next correctness and interoperability work

No known P0 correctness item remains open. Codec-output verification already
re-decodes every completed supported output, measures its final true peak, and
can iteratively re-render from the original input without compounding lossy
generations.

## P1 — professional delivery expansion

### Codec-specific QC

- MPEG-H MHAS packets, scene metadata, profile/level, loudness, and
  presentation rendering through an external conforming decoder.
- DTS core/HD metadata and decoded presentation checks through an optional
  adapter.
- WavPack, Monkey's Audio, and ALAC native frame/checksum validation.

### Loudness workflow depth

- Extend metadata-only normalization when additional containers acquire a
  standardized, widely honoured gain mechanism; avoid inventing private gain
  fields where none exists.

### Immersive, personalized, and accessible audio

- Validate every ADM programme/content/object presentation, not only the
  default render.
- Render-and-measure binaural, 5.1, 7.1.4, and user-selected speaker layouts.
- Separate dialogue, effects, music, audio-description, and clean-audio stem
  loudness checks.
- Personalization-range safety: verify that user gain/interactivity limits
  cannot violate loudness or true-peak constraints.
- Hearing-accessibility profiles should use explicit audiograms and validated
  fitting rules; keep them separate from mastering normalization.
- Safe-listening exposure estimates must identify headphone calibration and
  uncertainty and must not infer SPL from digital level alone.

The relevant open references include
[ITU-R BS.2127-1](https://www.itu.int/rec/R-REC-BS.2127-1-202311-I/en) and the
[2025 EBU Tech 3393 ADM Production Profile](https://tech.ebu.ch/publications/tech3393).

## P2 — research and optional perceptual features

These should remain opt-in until datasets, licences, model provenance, and
failure behaviour are documented.

- ISO 532-1/532-2 loudness, sharpness, roughness, fluctuation strength,
  tonality, and psychoacoustic annoyance reports.
- Speech intelligibility estimators such as STOI/ESTOI/PESQ/POLQA only within
  their validated domains and licensing terms.
- Perceptual codec comparison using an independently validated model and
  clearly versioned weights.
- Content classification for choosing a profile, with confidence and manual
  override; classification must never silently redefine the user's target.
- Blind compression estimation and restoration as a separate restoration
  tool, never as transparent “normalization”.
- Dialogue enhancement/source separation with artifact metrics and a
  reversible preview workflow.
- Preference learning stored locally, exportable/deletable, and isolated from
  standards-compliance results.
- Device/headphone compensation as a separate convolution/EQ stage with
  calibration provenance.
- Environmental adaptation only in the live player path, with microphone
  permission, privacy controls, attack/retrigger limits, and safe maximum gain.
- Causal/low-lookahead gain models benchmarked against the deterministic live
  processor for latency, pumping, overshoot, and CPU use.
- GPU/NPU acceleration only after scalar/SIMD equivalence and deterministic
  fallback tests.

## Product and developer experience

- TUI/GUI views for waveform, loudness timeline, true peak, QC events, channel
  correlation, and before/after comparison.
- Internationalized CLI diagnostics and accessible visualizations.

## Acceptance rules

A feature is ready to ship only when:

1. Its normative source or explicitly non-normative method is recorded.
2. Thresholds, units, channel mapping, and edge behaviour are observable.
3. Clean negative controls and defective positive fixtures are present.
4. Parsing and analysis have byte/count/depth/time limits.
5. JSON evidence is schema-validated and uses stable rule identifiers.
6. Default, optional-codec, cross-platform, fuzz, EBU, and ITU jobs remain
   green where applicable.
7. Release archives have checksums and verifiable attestations.

Core loudness work continues to track
[ITU-R BS.1770-5](https://www.itu.int/rec/R-REC-BS.1770-5-202311-I),
[EBU R 128 and its supplements](https://tech.ebu.ch/loudness/), and
[AES TD1008](https://aes.org/wp-content/uploads/2024/01/20210924_TD1008_v3.13.pdf).
Provenance work tracks the
[C2PA 2.2 specifications](https://spec.c2pa.org/specifications/specifications/2.2/index.html).
