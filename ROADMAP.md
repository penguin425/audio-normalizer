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
- EBU QC Items for channel count, clipping, duration, dropouts, loudness,
  phase reversal, test tones, clicks, minimum average level, silence, true
  peak, hum/buzz, band-limited noise, cross-talk, panning, LFE/centre
  assignment, and mono delivery.
- Forge-specific decoded-audio rules for DC offset, inter-channel sample
  delay, stuck samples, and discontinuities, with bounded/coalesced evidence.
- SPDX/CycloneDX release SBOMs, verified SLSA provenance, byte-reproducible
  Linux archives, dependency policy, Rust 1.89 MSRV checks, cargo-binstall
  metadata, and generated Homebrew/Scoop/WinGet manifests.
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

- AC-4 presentation and loudness metadata through a licensed/reference
  decoder adapter.
- MPEG-H MHAS packets, scene metadata, profile/level, loudness, and
  presentation rendering through an external conforming decoder.
- DTS core/HD metadata and decoded presentation checks through an optional
  adapter.
- WavPack, Monkey's Audio, and ALAC native frame/checksum validation.
- CENC `seig` key rotation/sample-group overrides and broader OAR conformance
  vectors. Standalone
  OBU bounds/order, supported codec configurations, full Audio Element and Mix
  Presentation semantics, profile element/channel limits, descriptor/substream
  linking, Parameter Block syntax, exact parameter/audio-frame timeline
  reconciliation, trimming/delimiter validation, unfragmented and fragmented
  `iamf`/`enca` sample entry, configuration, sample-table/fragment
  decapsulation and CENC signaling, and externally rendered presentation QC
  have shipped.

### Streaming and platform delivery

- Remote-resource auditing only behind explicit allowlists, byte/time limits,
  redirect controls, and a recorded fetch manifest.
- Apple Sound Check metadata read/write and round-trip checks.
- Platform policy data as versioned profiles with source/date fields; never
  hard-code a service name as a timeless fixed LUFS rule.

### Loudness workflow depth

- Loudness-to-dialogue ratio, speech-gated loudness confidence, and manual
  review ranges.
- Loudness-range and peak-to-loudness targets per content class.
- Multi-delivery optimization: derive one conservative master gain/ceiling for
  several codec/profile outputs and verify each result.
- Segment-aware normalization with boundary smoothing and a two-pass manifest
  for large streaming catalogues.
- Metadata-only normalization for every container that has a standardized,
  widely honoured gain mechanism.
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

- Stable Rust library API with semantic-versioning checks and C/Python
  bindings.
- WASM analysis build for local browser use; encoding and filesystem features
  remain capability-gated.
- Resumable batch jobs, content-addressed analysis cache, watch folders, and
  machine-readable progress.
- SQLite catalogue with source/output hashes, measurement standard/version,
  profile, tool version, and provenance report.
- TUI/GUI views for waveform, loudness timeline, true peak, QC events, channel
  correlation, and before/after comparison.
- JSON Schema migration tooling and a command that explains each failed rule,
  its source, observation, and remediation.
- Plugin parity tests across CLI, CLAP, LV2, and live-stream processing.
- CPU/memory benchmarks for hour-long stereo, multichannel, lossless, lossy,
  and pathological inputs.
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
