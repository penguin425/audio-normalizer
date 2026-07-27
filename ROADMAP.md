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
- WAVE/RF64/BW64, AIFF/AIFC, CAF, AU, FLAC, MP3, Ogg Opus/Vorbis, and
  ISO-BMFF/fMP4 structural QC.
- Dependency-free ADTS and LOAS/LATM AAC QC, including ASC SBR/PS signalling,
  decoded timing, and ISO-BMFF edit-list/sample-group gapless reconciliation.
- HLS, DASH, and CMAF package checks; ISO loudness and MPEG-D DRC metadata.
- ADM, S-ADM, presentation-aware rendering/QC, C2PA validation, CI comparison,
  real-time processing, LV2, and CLAP integration.
- EBU QC Items for channel count, clipping, duration, dropouts, loudness,
  phase reversal, test tones, clicks, minimum average level, silence, true
  peak, hum/buzz, band-limited noise, cross-talk, panning, LFE/centre
  assignment, and mono delivery.
- Forge-specific decoded-audio rules for DC offset, inter-channel sample
  delay, stuck samples, and discontinuities, with bounded/coalesced evidence.
- SPDX/CycloneDX release SBOMs, verified SLSA provenance, byte-reproducible
  Linux archives, dependency policy, Rust 1.87 MSRV checks, cargo-binstall
  metadata, and generated Homebrew/Scoop/WinGet manifests.

## P0 — next correctness and interoperability work

These are the strongest candidates for the next releases.

### Matroska and WebM

- Bounded EBML variable-integer parser with element depth, count, and size
  limits.
- Segment/Info/Tracks/Cluster ordering and uniqueness.
- Track UID/number consistency, codec IDs/private data, audio geometry, block
  lacing, timestamps, and duration.
- SeekHead and Cue target/offset cross-checks.
- CRC-32 validation and unknown-size rules for files versus live streams.
- Opus `CodecDelay`, `SeekPreRoll`, and `DiscardPadding` validation, including
  decoded-duration cross-checks.
- FLAC, Vorbis, PCM, and MP3-in-Matroska codec-private validation by reusing
  native Forge parsers.

The normative target is [RFC 9559](https://www.rfc-editor.org/rfc/rfc9559.html).

### Codec-output verification

- Codec-output true-peak iteration using the final AAC/HE-AAC/xHE-AAC decoder
  result rather than PCM-only prediction.

## P1 — professional delivery expansion

### Broadcast and mastering containers

- MXF OP1a/OP-Atom KLV bounds, partitions, RIP, index tables, essence
  containers, descriptors, channel assignment, and AS-11/DPP constraints.
- IMF CPL/PKL/AssetMap integrity, hash verification, virtual-track timing,
  MCA labels, and Application Profile constraints.
- BWF MetaEdit-compatible checks for `bext`, UMID, coding history, originator
  fields, time reference, and XML schemas.
- iXML track-list/channel-index reconciliation and ADM `chna` cross-checks.
- AES31/AAF interchange validation where authoritative fixtures are available.
- DSD/DFF/DSF read-only analysis, with an explicit low-pass/decimation policy
  before BS.1770 measurement.

### Codec-specific QC

- AC-3/E-AC-3 syncframe, `dialnorm`, DRC profile, channel mode, dependent
  substream, Atmos/JOC signalling, and decoded-presentation checks.
- AC-4 presentation and loudness metadata through a licensed/reference
  decoder adapter.
- MPEG-H MHAS packets, scene metadata, profile/level, loudness, and
  presentation rendering through an external conforming decoder.
- DTS core/HD metadata and decoded presentation checks through an optional
  adapter.
- WavPack, Monkey's Audio, and ALAC native frame/checksum validation.
- MP3 protected-frame CRC validation, VBRI parsing, ReplayGain-in-LAME-field
  validation, and safe support for documented free-format streams.
- Opus projection mapping family 2 and ambisonics validation.

### Streaming and platform delivery

- LL-HLS parts, preload hints, rendition reports, server-control, delta
  updates, blocking reload, and discontinuity-state validation.
- DASH dynamic MPDs, availability windows, UTC timing, period continuity,
  event streams, content protection, and low-latency constraints.
- CMAF switching-set alignment across audio renditions and languages.
- MPEG-DASH loudness/DRC descriptors and HLS timed-ID3 loudness metadata.
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
- Before/after audible-difference report with gain envelope, limiting amount,
  clipped-sample count, and codec drift.
- Reference comparison with sample alignment, null residual, spectral error,
  and channel permutation detection.

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
