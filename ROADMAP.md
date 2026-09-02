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
  bounded work-stealing parallelism across album tracks and independent batch
  files, limiter, sample-rate conversion, integer PCM dither, ReplayGain, and
  BWF loudness metadata.
- Album-mode ReplayGain Gain/Peak and RFC 7845 R128 gain use the combined
  population of final decoded outputs, so lossy codec drift and unequal track
  durations are reflected consistently in every tagged track.
- M4A/ALAC normalization and metadata-only tagging create native ISO-BMFF
  `ludt/tlou`; album mode also writes `alou` from the combined final decoded
  gating blocks and album-wide sample/true peaks. Media payload hashes and the
  exact quantized metadata are verified before atomic replacement.
- `forge-doctor` reports the exact compile-time features, bounded runtime
  probes, CPU SIMD support, and effective read/write format matrix; repeatable
  `--require` checks make missing deployment capabilities fail with exit 1.
- Corrected BWF and ReplayGain finalization reuses the already-verified output
  analysis under container-default channel roles, while explicit custom roles
  retain the independent container re-analysis needed for compatibility.
- Multi-delivery fans one decoded, resampled, gained, true-peak-protected
  stream into every requested codec writer while retaining independent encoder
  state, staged post-metadata verification, and atomic publication.
- EBU Tech 3285 v2 BWF `bext` QC for fixed production fields, date/time,
  sample-based TimeReference, version/UMID/reserved-byte consistency, loudness
  metadata ranges, and CodingHistory line structure.
- WAVE/RF64/BW64, AIFF/AIFC, CAF, AU, FLAC, MP3, Ogg Opus/Vorbis, and
  ISO-BMFF/fMP4 structural QC.
- Bounded RFC 9559 Matroska/WebM QC for EBML structure, tracks, blocks,
  SeekHead/Cues, CRC-32, codec-private data, and Opus gapless timing.
- Dependency-free ADTS and LOAS/LATM AAC QC, including ASC SBR/PS signalling,
  decoded timing, and ISO-BMFF edit-list/sample-group gapless reconciliation.
  ISO-BMFF xHE-AAC additionally parses bounded MPEG-D USAC Audio Object Type 42,
  UniDRC, and `loudnessInfoSet` syntax; validates IDs, downmix/dependency/gain-set
  references, sample timing, and Apple's Basic DRC Metadata Profile (Anchor
  Loudness, BS.1770 peak metadata, four required effect sets, and target
  coverage). HLS QC verifies Immediate Playout Frames from resolved first-sample
  bytes and reconciles `EXT-X-INDEPENDENT-SEGMENTS`; container QC can compare
  programme, anchor, sample-peak, and true-peak metadata with independent
  decoded PCM renders.
- Dependency-free AC-3/E-AC-3 elementary-stream QC for bounded syncframes,
  complete BSI, `dialnorm`, interpreted DRC gain words, channel mode, strict
  Atmos/JOC Extension Type A signalling, six-block access-unit and
  independent/dependent presentation grouping, plus authoritative external
  E-AC-3 JOC render checks.
- Native, bounded MPEG-H MHAS packet framing, label, SYNC, packet inventory,
  configuration profile/level, and ordering checks, plus a versioned external
  conforming-decoder adapter for complete audio-scene group/switch/preset
  validation and independently measured presentation loudness/true peak.
- Native, bounded DTS core/HD elementary-stream framing for all four core wire
  representations and DTS-HD extension substreams, plus a versioned optional
  licensed/reference-decoder adapter that enumerates every asset and
  presentation and independently measures each WAVE render.
- Dependency-free WavPack 4/5 block QC for bounded header and metadata framing,
  multichannel sequence/sample continuity, stable stream format, and exact
  16/32-bit WavPack 5 encoded-block checksum verification.
- Dependency-free current-format Monkey's Audio 3.98/3.99 QC for bounded
  descriptors and regions, overflow-aware seek-table frame boundaries,
  decoded-PCM CRC-slot presence, and exact descriptor-MD5 quick verification.
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
- Apple HLS fMP4 loudness rules now evaluate every audio track: non-APAC
  tracks receive the `ludt` recommendation, while APAC tracks fail if a
  container loudness box would override their required in-stream metadata.
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
- ATSC A/85:2026 Annex L multi-asset streaming-service QC with explicit
  long-form dialogue anchors, short-form full-programme measurement,
  metadata/non-metadata target reconciliation, insertion and accompanying
  programme checks, mixed-mode evidence, and the Annex M -2 dBTP ceiling.
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
- April 2026 EBU QC Scenario 1 XML reports with deterministic identifiers,
  sample edit units, pinned Catalogue API v2 definition hashes, exact
  catalogue-specific input/output vocabularies, one-to-one Item results, and
  correct check-versus-report semantics.
- September 2025 EBU Tech 3393 reading/writing structural audits covering the
  profile declaration, bounded core ADM graph/cardinalities, identifiers,
  references, labels, object nesting, tag groups, and `chna` reconciliation.
- ITU-T H.872 game and handheld compliance profiles using exact gated loudness
  extrema over every complete rolling 30-minute window and a −1 dBTP ceiling.
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
- Versioned external audio-anomaly provider v1 with source/model SHA-256
  provenance, bounded time-sorted noise/pop/dropout/lip-noise/phase-cancellation
  findings, confidence/severity thresholds, and a separate non-normative audit
  layer that never changes EBU/ITU compliance results.
- Delivery-manifest `model_qc` bridge with complete audit revalidation and
  `forge-report explain` model findings using stable
  `FORGE-MODEL-ANOMALY-*` IDs; model evidence remains outside normative pass
  totals.
- Opt-in CPU ONNX anomaly-provider reference adapter with explicit runtime and
  model SHA-256 selection, licence/dataset/calibration evidence, bounded
  feature-frame input, exact tensor contracts, fail-closed fallback, and
  coalesced `audio-anomaly-provider-v1` output. The default build remains free
  of ONNX Runtime and model weights.
- Explicitly allow-listed, seekable HTTP Range access for HTTPS, S3, and GCS
  objects with redirect reauthorization, request/byte/object/time limits,
  strict 206/Content-Range validation, redacted fetch evidence, and the
  `forge-remote-qc` header/prefix probe. Remote access is never implicit in
  local normalization or QC commands.
- Bounded stateless REST upload/analyze service via `forge-service`, with
  loopback-by-default binding, bearer-token enforcement for non-loopback
  deployments, strict HTTP framing, upload/decoded-sample/concurrency/time
  limits, and versioned health/analysis/error schemas. The service never
  accepts a local filesystem path or performs implicit remote access.
- Optional tonic gRPC service on the same `forge-service` binary, with a
  versioned `Analyze`/`Cancel`/`Health` protocol, explicit bounded request IDs,
  the REST limits/authentication policy, deadline/disconnect cancellation, and
  cooperative decode/analysis checkpoints. The default build remains free of
  the async runtime and HTTP/2 stack.
- Opt-in bounded service observability with fixed Prometheus counters,
  duration histogram buckets, analysis aggregates, REST `GET /metrics`, gRPC
  `Metrics`, and a local JSONL bridge for OpenTelemetry-compatible server-span
  attributes. User paths, filenames, request IDs, and model payloads are never
  labels or span fields.
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
  corruption recovery, read-only operation, bounded FIFO eviction, and
  ordered file-level parallel hit/miss integration for album and batch jobs.
- Stable-file watch folders with symlink-safe bounded discovery, durable
  atomic processing state, settings and input/output SHA-256 binding,
  restart recovery, explicit failed-item retry, and one-shot scheduling.
- Versioned SQLite catalogue with source/output SHA-256, BS.1770-5/EBU R 128
  measurement and algorithm revisions, selected profile, Forge version,
  bounded structured provenance, transactional deduplication, and atomic
  per-invocation JSON evidence export.
- Versioned, standard-library-only performance harness for generated one-hour
  stereo WAVE analysis, same-rate/resampled stereo, independent eight-file
  batch, album, and 7.1 WAVE normalization, FLAC/MP3 analysis and normalization,
  and bounded pathological WAVE QC, with repeated samples, median timing,
  maximum RSS evidence, and compatible-host baseline regression gates.
- Channel-contiguous AVX2 gain/ceiling fusion, byte-exact AVX2 PCM16 and PCM24
  mono/stereo quantization and interleaving, reusable WAVE chunk storage, and
  scalar/SIMD equivalence coverage for exceptional values and quantizer
  boundaries.
- Deterministic LLVM instrumentation PGO for the generic Linux and Apple
  Silicon `forge` CLIs, with bounded serial training, order-independent branch
  counters, removal of nondeterministic value profiles and empty function
  records, recorded canonical profiles as explicit Linux rebuild inputs,
  executable reproduction gates, and an optional x86-64-v3 PGO CLI for
  compatible CPUs. Package managers, libraries, plug-ins, wheels, and the
  remaining tools stay on their portable CPU baselines.
- A stereo-specialized streaming analyzer that keeps both K-weighting filters
  and true-peak meters borrowed across the frame loop, removes dynamic channel
  iteration for the dominant delivery layout, preserves the generic timeline
  and multichannel path, and produces byte-identical normalized audio.
- Bounded stereo analysis overlap that advances the exact paired True Peak
  meters concurrently with the unchanged K-weighting, RMS, sample-peak, and
  gating pass for long non-timeline chunks, retains the fused path for short
  packets and one-worker runs, and preserves every reported bit and output
  byte.
- Bounded verified-WAVE writer overlap that transfers quantization and the
  lossless-verification tee to one worker while decode/DSP advances, is gated
  to all-WAVE outputs with captured statistics and verification, leaves FLAC's
  existing frame parallelism and nested file work untouched, and preserves
  output bytes plus verification evidence exactly.
- Top-level DSF/DSDIFF channel parallelism that advances independent FIR
  pipelines on the configured Rayon pool, preserves each channel's byte, bit,
  and floating-point operation order, avoids nested file-level scheduling,
  and retains an exact serial fallback for one-worker execution.
- Adaptive native-WAVE streaming chunks: a frame-aligned 1 MiB read/decode
  buffer for stereo and multichannel inputs, the established 64 KiB buffer for
  mono, and one reusable planar allocation across the stream. The policy is
  selected from measured latency, memory, and scheduler evidence rather than
  file duration, and preserves every supported PCM representation exactly.
- Paired stereo true-peak processing that shares immutable polyphase
  coefficient loads and exposes both meter states in one hot loop while
  retaining separate histories, per-channel FMA/reduction order, runtime SIMD
  fallbacks, and bit-identical 48/96/192 kHz measurements.
- Exact analysis-only true-peak pruning based on the polyphase FIR triangle
  bound, with sparse complete-window probes, bit-identical 48/96/192 kHz and
  chunked results, unchanged timeline/limiter frame detection, and a measured
  dense-signal fallback cost disclosed alongside the dynamic-signal gain.
- SIMD block-level true-peak pruning after that exact proof is active, with
  one maximum/NaN reduction per safe decoder chunk, direct reconstruction of
  the final 16-sample FIR history, independent stereo-channel decisions, and
  byte-identical analysis/normalization evidence. Unsafe and dense chunks
  retain the established per-sample paired interpolation path.
- Bounded pure-Rust FLAC frame parallelism through the existing global
  `--jobs` pool, with short-packet coalescing, at most eight active encoder
  tasks, a 1 MiB quantized-sample bound, parallel frame serialization, ordered
  MD5/STREAMINFO/file finalization, a serial one-worker path, and byte-identical
  16/24-bit dithered and undithered output. Existing file-level Rayon work keeps
  the inner encoder serial to avoid nested fan-out.
- Runtime-dispatched AVX2 FLAC sample staging for undithered 16/24-bit output,
  with eight-frame mono/stereo quantization, lane-correct stereo interleaving,
  vectorized 3-to-8-channel frame groups, exact exceptional-value and half-LSB
  behavior, unchanged dither RNG sequencing, and portable scalar fallbacks.
- Pipelined FLAC source-context processing that overlaps the serial ordered MD5
  pass with bounded independent-frame encoding in the existing worker pool,
  retains the exact per-frame `Context` update boundaries and STREAMINFO bytes,
  adds no sample copy or codec-owned pool, and leaves one-worker/nested-file
  encoding on the established serial path.
- Multichannel true-peak pair passes that retain meter state across each
  channel-contiguous decoder chunk, share immutable polyphase coefficients for
  adjacent channels, handle odd channel counts with the scalar tail, and leave
  K-weighting, energy, gating, channel order, and reported results unchanged.
- Bounded long-chunk multichannel true-peak parallelism over those independent
  channel pairs, using the existing global `--jobs` work-stealing pool and a
  measured packet-size threshold while retaining the sequential low-overhead
  path for short chunks and single-worker runs.
- Optional bounded CUDA true-peak analysis for published Linux and Windows
  builds: dynamically loaded Driver API and checked-in PTX, transfer/kernel
  overlap with exact CPU K-weighting, one process-wide device worker, retained
  history for runtime CPU recovery, explicit opt-in, and byte-identical CPU/GPU
  measurements and WAVE output. Context/JIT-inclusive benchmarks keep the
  already-faster 7.1 CPU path as the default instead of claiming a universal
  GPU advantage.
- Allocation-stable streaming look-ahead limiting: caller-owned output channels
  are reused for every decoded chunk and the delayed tail, while adjacent True
  Peak meters share the paired SIMD kernel through dedicated stereo and ordered
  multichannel paths. Stereo and 7.1 WAVE plus difference-report hashes remain
  byte-identical to the allocating scalar-detector pipeline.
- Allocation-free multichannel K-weighting through persistent four-channel AVX2
  f64 state banks, explicit inter-stage f32 rounding, scalar-order energy and
  role reductions, fixed sub-kilobyte 7.1 state, and unchanged scalar fallbacks.
  CPU/CUDA 7.1 JSON remains byte-identical while measured analysis wall time
  falls on both backends; slower AoS pair and per-frame gather prototypes were
  measured and rejected.
- Byte-exact multichannel PCM16/PCM24 AVX2 quantization and frame-major
  interleaving for full and partial channel groups, with ordered scalar tails,
  unchanged TPDF RNG behavior, exceptional-value/half-LSB coverage, portable
  fallbacks, and matching lossless verification reports. Measured 5.1 and 7.1
  WAVE renders improve at both integer depths.
- Allocation-stable MP3 and Opus encoder feeds: LAME consumes existing planar
  f32 channels without a stereo interleave allocation, while Opus resampler
  input and interleaved packet queues use bounded cursors and one retained
  resampler output buffer. Codec bytes remain identical; slower FLAC retention
  and higher-RSS Opus packet-scratch variants were measured and rejected.
- Bounded zero-copy MP3/Opus render pipelines that create host-bound writers on
  one global-pool worker, transfer two recycled planar channel allocations by
  ownership, and overlap encoding with the next decode/DSP chunk. Synchronous
  one-worker and nested-file paths remain unchanged; limiter and uncached
  converter output retain a bounded copy fallback. Encoder errors preserve
  source-order precedence, and codec bytes remain identical.
- Versioned multi-delivery optimization for two to 32 codec/profile outputs,
  using one conservative target, ceiling, and iteratively corrected gain;
  one shared decode/DSP/statistics pass per correction attempt, staged
  post-metadata re-decoding, explicit infeasibility, path-alias and overwrite
  safety, and schema-validated hash/measurement/profile evidence.
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
- Native ALAC-in-ISO-BMFF QC with complete magic-cookie/channel-layout
  validation, exact unfragmented and fragmented sample-table expansion,
  bounded `mdat`/`maxFrameBytes` ranges, strict per-access-unit decoding, and
  explicit evidence that ALAC defines no native packet checksum.

## P0 — next correctness and interoperability work

Output verification measures the exact quantized PCM accepted by native
WAVE/FLAC encoders and re-decodes every codec-dependent completed output,
measures final true peak, and can iteratively re-render from the original input
without compounding lossy generations.
S-ADM flow QC groups Divided-Frame chunks into logical frames, validates FF,
IF, MF, and DF type sequences and recurrence declarations, and compares frame
timing with exact decimal/sample arithmetic, including legacy date-bearing
timestamps.
The remaining S-ADM correctness work is path-aware XML structure and
`frame/@version` validation followed by changed-ID state reconstruction.

## P1 — professional delivery expansion

### Loudness workflow depth

- `forge-metadata-repair` now derives ISO-BMFF `ludt/tlou` programme loudness,
  sample peak, and true peak from bounded decoded PCM. It preserves `mdat`
  payloads by hash, adjusts affected `stco/co64` offsets, and fails closed for
  presentation codecs and unsupported offset mechanisms. Schema v2 derives
  `alou` from the combined population of complete 400 ms blocks across a
  bounded, de-duplicated album reference list.
- Extend metadata-only normalization only when another container has a
  standardized, widely honoured mechanism; avoid private gain fields.

### Immersive, personalized, and accessible audio

- `forge-adm-presentation-qc` now enumerates every ADM `audioProgramme` and the
  Cartesian product of its complementary-object groups, renders each selection
  with the EBU `ear-render` BS.2127 implementation, and independently measures
  loudness, true peak, duration, and channel geometry. Expansion, process
  output, timeout, decoded samples, and retained files are bounded; hashes bind
  the input, renderer, and every render to the versioned evidence report.
- `forge-adm-interactivity-qc` resolves parent and `alternativeValueSet`
  interaction metadata, rejects implicit or incomplete gain envelopes, checks
  default gains and position-pair structure, and optionally applies the
  BS.2168-0 emission-profile interactivity subset. Reports explicitly state
  that metadata inspection has not verified continuous rendered audio.
- `forge-adm-semantics-qc` validates dialogue-kind enumerations and
  alternative-value-set ownership, resolves the lowest-ID fallback programme,
  distinguishes fixed and complementary interactive authoring patterns, and
  produces a conservative importance-threshold plan. Tags and metadata object
  counts are never presented as authoritative render evidence.
- `forge-downmix-qc` now measures deterministic WAVE-order stereo/5.1/7.1.4
  profiles with explicit matrices, loudness/true-peak deltas, and clip-risk
  gates. User-selected speaker layouts remain future renderer-adapter work.
  `forge-binaural-qc` now verifies externally
  rendered stereo output with mandatory renderer/model hash evidence and
  optional trusted-reference drift gates; it does not bundle an HRTF renderer.
- `forge-remediate` produces a bounded dry-run plan for true-peak and LRA
  remediation, binding source/settings hashes and requiring a fresh render and
  remeasurement for every dynamic action. It never rewrites audio.
- `forge-metadata-repair` provides bounded copy-to-new-file BWF/ADM and
  ISO-BMFF loudness repair with pre/post validators, source/output hashes, and
  byte-preservation evidence. MXF remains validate-and-copy only. Source
  replacement is not exposed; `atomic_replace` applies only to the destination.
- Separate dialogue, effects, music, audio-description, and clean-audio stem
  loudness checks.
- Add a bounded personalization renderer adapter and independently measure its
  declared interaction cases. Treat true-peak upper bounds and gated
  integrated-loudness coverage separately; do not infer a continuous loudness
  guarantee from metadata or a single default render.
- Hearing-accessibility profiles should use explicit audiograms and validated
  fitting rules; keep them separate from mastering normalization.
- Safe-listening exposure estimates must identify headphone calibration and
  uncertainty and must not infer SPL from digital level alone.

The relevant open references include
[ITU-R BS.2076-3](https://www.itu.int/rec/R-REC-BS.2076-3-202502-I/en),
[Report ITU-R BS.2388-7](https://www.itu.int/pub/R-REP-BS.2388-7-2026),
[ITU-R BS.2168-0](https://www.itu.int/rec/R-REC-BS.2168-0-202502-I/en),
[ITU-R BS.2127-1](https://www.itu.int/rec/R-REC-BS.2127-1-202311-I/en), and the
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
[C2PA 2.4 specifications](https://spec.c2pa.org/specifications/specifications/2.4/index.html).
