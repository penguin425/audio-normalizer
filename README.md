# Forge — a SIMD-accelerated EBU R128 / ITU-R BS.1770-5 loudness normalizer

Forge is a fast, standards-correct audio loudness normalizer written in Rust.
It measures loudness the way broadcasters and streaming services do (**EBU R128
LUFS** with the full ITU-R BS.1770-5 K-weighting and two-stage gating) and
applies a single linear gain so the output hits your target — while guaranteeing
the **inter-sample true peak** never exceeds a ceiling, the way Spotify/Apple
mastering does.

See [ROADMAP.md](ROADMAP.md) for the standards-backed implementation backlog,
research candidates, and acceptance criteria.

## Formats

Forge reads and writes a wide range of formats through a format-agnostic engine:

| | Read (decode) | Write (encode) |
|---|---|---|
| **WAV / RF64 / BW64** (PCM 8/16/24/32-bit, float 32/64-bit) | Forge's own fast parallel demuxer | Forge's own muxer |
| **DSF / DSDIFF** (uncompressed 1-bit DSD, read-only) | Forge's bounded parser + FIR decimator | — |
| **MP3** | symphonia (pure Rust) | LAME via FFI |
| **FLAC** (16/24-bit, up to 8 channels) | symphonia (pure Rust) | flacenc (pure Rust) |
| **Ogg Opus** (1–8 channels, through 7.1) | libopus + pure-Rust Ogg | libopus + pure-Rust Ogg |
| **AAC / ALAC** (.m4a/.mp4) | symphonia (pure Rust) | AAC-LC or ALAC via optional FFmpeg runtime |
| **Vorbis** (.ogg) | symphonia (pure Rust) | libvorbis via optional FFmpeg runtime |

* Decoding of MP3/FLAC/AAC/ALAC/Vorbis is done in pure Rust by
  [`symphonia`](https://github.com/pdelanoe/symphonia) — no system codecs.
* Optional MP3 encoding uses **LAME** (the reference MP3 encoder) through a
  tiny FFI. Enable it with the `mp3-encoding` Cargo feature.
* FLAC encoding is pure Rust, streaming, and available in the default build.
* Optional Ogg Opus input/output uses statically linked libopus, bounded-memory
  sinc resampling to 48 kHz, and a pure-Rust Ogg container. Release binaries
  include it; source builds enable it with the `opus-encoding` feature.
* Opus output and metadata-only updates follow
  [RFC 7845 section 5.2](https://www.rfc-editor.org/rfc/rfc7845#section-5.2):
  `R128_TRACK_GAIN` and, in album mode, `R128_ALBUM_GAIN` are written in
  signed Q7.8 dB units relative to −23 LUFS and read back exactly.
* AAC-LC, ALAC, and Vorbis output use the optional FFmpeg runtime. MP4 gapless
  timing is preserved and Forge writes ReplayGain loudness/peak metadata after
  measuring the encoded result.
* Optional Apple Sound Check compatibility tags use the observed `iTunNORM`
  representation in MP4/M4A, MP3, AIFF, and raw AAC. Forge strictly reads ten
  hexadecimal words, writes the container-native field, and re-reads it for an
  exact round trip. Apple does not publish this field layout or analyser, so
  Forge labels its R128-to-`iTunNORM` conversion as a non-normative engineering
  mapping rather than an Apple loudness target.
* Multichannel Opus uses RFC 7845 Mapping Family 1 and preserves standardized
  3.0 through 7.1 speaker assignments.
* Chained Ogg Opus applies each logical stream's pre-skip, final-granule trim,
  output gain, and channel mapping independently before concatenating samples.
* WAV stays on the fast hand-written path; other inputs transparently route
  through the universal decoder and produce the same planar-f32 buffer the DSP
  engine consumes.
* DSF and uncompressed DSDIFF inputs use their specified bit order and channel
  layout. Forge maps 1-bit samples to ±1, applies cascaded 31-tap
  Blackman-windowed half-band filters to 88.2/96 kHz, then a 127-tap
  Blackman-sinc low-pass with a 21 kHz cutoff before BS.1770 measurement.
  `forge-dsd-pcm-v1` is an explicit, non-normative engineering policy; it is
  recorded in container and delivery-manifest evidence. DST-compressed DSDIFF
  is rejected rather than decoded approximately.
* Common metadata fields and embedded artwork are preserved across
  normalization and remapped to the destination container's primary tag type.
  DSF ID3 and DSDIFF DIIN/COMT are left read-only because no lossless,
  standardized mapping exists for every PCM destination.
* Broadcast Wave output can preserve `bext`, `axml`, `bxml`, `sxml`, `chna`,
  and iXML chunks. BWF v2 measured loudness fields are updated from the
  normalized output.
* Container QC validates bounded iXML documents, reconciles `TRACK_COUNT` and
  one-based `INTERLEAVE_INDEX` values with PCM channels, preserves non-contiguous
  recorder `CHANNEL_INDEX` source numbers, and cross-checks ADM `chna` mappings.
* ITU-R BS.2088-2 XML QC validates unique `axml`/`bxml`/`sxml` chunks,
  UTF-8 XML, bounded gzip expansion, serial-XML tables/alignment/sample spans,
  EBUCore envelopes, ADM/S-ADM placement, required `chna`, and metadata
  independence.

By default the output container follows the input where Forge can encode it
(FLAC → FLAC, MP3 → MP3, Ogg Vorbis → Ogg Vorbis, and M4A → AAC/M4A when
FFmpeg encoding is enabled), and otherwise falls back to lossless WAV.
`--format wav|flac|mp3|opus|m4a|alac|vorbis` and
the `-o` extension override this.

## Why it's fast

* **AVX2 + FMA SIMD** for the gain and energy-summation hot loops, with
  runtime feature detection and a portable scalar fallback (so the binary runs
  anywhere but flies on modern x86-64).
* **Fused render/write hot path** applies gain and the safety ceiling in one
  channel-contiguous SIMD pass, vectorizes byte-exact PCM16 mono/stereo
  quantization and interleaving, and reuses the WAVE writer's chunk storage.
  Dither, exceptional samples, limiter processing, and verification retain
  their established semantics.
* **Lossless verification tee** measures the exact quantized PCM accepted by
  the native WAVE and FLAC writers during encoding, avoiding an otherwise
  redundant completed-file read. PCM scratch is reused between chunks and
  implicit multichannel roles match the persisted container layout. MP3, AAC,
  ALAC, Opus, and Vorbis outputs retain completed-file codec re-decode, and
  multi-delivery still re-decodes after final metadata mutation.
* **Sample-rate-aware true-peak analysis** uses a copy-free circular history,
  SIMD polyphase interpolation, and the BS.1770 measurement domain: 4× below
  96 kHz, 2× below 192 kHz, and direct samples at 192 kHz and above. The common
  stereo path advances both independent meters together, sharing immutable FIR
  coefficient loads without changing either channel's FMA or maximum order.
  Multichannel analysis processes adjacent meter pairs in channel-contiguous
  passes while preserving the established K-weighting and channel-sum order.
  Long four-or-more-channel chunks distribute those independent pairs across
  the existing `--jobs` pool; short decoder packets remain sequential so task
  coordination cannot dominate useful work.
* **Multi-threaded** via rayon — channels, independent album tracks, and
  ordinary multi-file normalization share one work-stealing pool bounded by
  `--jobs`. Independent files render in waves of at most 32, then publish in
  input order.
* **Rolling block energies** make the 75%-overlapping LUFS gating blocks O(1)
  each while retaining only three seconds of filtered energy.
* **Specialized stereo streaming analysis** keeps both K-weighting filters and
  true-peak meters in the hot loop without dynamic channel iteration. The
  generic channel-layout path remains unchanged, and the optimized path keeps
  the same floating-point operation order and byte-identical normalized output.
* **Adaptive WAVE streaming chunks** keep the low-latency 64 KiB read size for
  mono while using a frame-aligned 1 MiB chunk for stereo and multichannel
  inputs. The planar decode buffer is reused between reads, reducing allocator,
  scheduler, and I/O-call overhead without changing decoded samples.
* **Bounded-memory streaming** decodes analysis and normalization in chunks.
  Normalization uses two sequential passes so gain is known before encoding,
  without retaining the complete audio file in RAM. Single-source render paths
  reuse temporary output-domain PCM for resampled audio and decode-heavy
  lossless/DSD inputs, avoiding a second decode or resample; fast same-rate
  inputs and multi-track albums retain the lower-I/O/bounded-resource re-decode
  path.
  Standard-input audio is spooled to a temporary file so the same correct
  two-pass algorithm remains available in shell pipelines.
* Release profile uses `lto = "fat"`, `codegen-units = 1`, and
  `panic = "abort"`. The published Linux `forge` CLI adds deterministic,
  branch-counter-only PGO while retaining a generic x86-64 baseline and
  runtime-dispatched AVX2/FMA kernels. A supplemental PGO `x86-64-v3` CLI is
  available for compatible CPUs. Local Cargo builds use `target-cpu=native`
  and are not portable.

See [PERFORMANCE.md](PERFORMANCE.md) for the primary research basis, measured
release results, rejected experiments, and implementation order.

### Benchmark

Forge includes a reproducible, versioned CPU/memory benchmark harness:

```sh
cargo build --locked --release --bin forge --bin forge-container-qc
python3 tools/benchmark.py --forge target/release/forge --iterations 5 --output benchmark.json
```

The suite covers one-hour stereo WAVE analysis, same-rate and resampled stereo
normalization, native WAVE/FLAC verification, 7.1 WAVE normalization, lossless
FLAC and lossy MP3 analysis and normalization, and bounded rejection of a
pathological WAVE chunk population.
Fixture generation is excluded from
measurement. Reports retain repeated samples and include median wall/CPU time,
maximum peak RSS, real-time factor, host identity, workload settings, and
optional same-host regression checks. See [BENCHMARKS.md](BENCHMARKS.md) for
the contract, safety limits, and short smoke command.

### Multi-delivery optimization

`forge-multi-delivery` derives one conservative target and true-peak ceiling
from two to 32 versioned delivery profiles, renders every requested codec with
the same linear gain, and re-decodes every staged output before publishing it.
It emits schema-validated JSON evidence with hashes, measurements, resolved
profile provenance, and per-profile headroom:

```sh
forge-multi-delivery master.wav \
  --request delivery/multi-delivery.json \
  --report delivery/multi-delivery-report.json
```

The method is explicitly non-normative and fails when no shared gain can meet
all codec constraints; it never substitutes independent output gains. See
[MULTI-DELIVERY.md](MULTI-DELIVERY.md) for the request contract, algorithm,
safety rules, examples, optional codec requirements, and report schema.

### Segment-aware catalogue normalization

`forge-segment-normalize` provides a content-bound two-pass workflow for two
to 4096 ordered segments. Pass one records source hashes, measurements, and a
shared-boundary gain plan; pass two verifies every binding, renders one segment
at a time with smoothstep dB ramps, re-decodes each output, and publishes only
segments whose codec loudness, true peak, and duration checks pass:

```sh
forge-segment-normalize plan \
  --request catalogue/request.json \
  --manifest catalogue/segment-plan.json
forge-segment-normalize render \
  --manifest catalogue/segment-plan.json \
  --report catalogue/segment-report.json
```

The method is explicitly non-normative, memory is bounded per segment, and the
report states that the sequential output set is not a filesystem transaction.
See [SEGMENT-NORMALIZATION.md](SEGMENT-NORMALIZATION.md) for the formulas,
limits, path safety, optional codec requirements, schemas, and exit status.

## Why it's correct

* **K-weighting** is implemented from the ITU-R BS.1770-5 design equations
  (`K = tan(π·f0/fs)` analog-prototype + bilinear transform). The unit test
  `kweight_48k_matches_itu` asserts the resulting 48 kHz coefficients match the
  reference (libebur128 / FFmpeg / pyloudnorm "DeMan") to 1e-9 — the shelf
  coefficients reproduce the ITU published values to full double precision.
* **Gated loudness** uses 400 ms blocks, 75% overlap, the −70 LUFS absolute
  gate and the −10 dB relative gate, with the relative gate applied in the
  linear mean-square domain (numerically exact, no repeated log/exp).
* **Channel-layout-aware weighting** reads WAVE_FORMAT_EXTENSIBLE channel masks
  and codec layouts, excludes LFE channels wherever they occur, and applies the
  BS.1770 surround-channel weighting by role instead of guessed channel index.
  BS.1770-5 Annex 3 position-dependent weights are used for 7.1 and height
  channels; `--channel-layout` can override missing or incorrect metadata.
* **EBU Mode analysis** reports Integrated, maximum Momentary (400 ms), maximum
  Short-term (3 s), and Loudness Range (LRA) measurements.
* **Delivery compliance reports** include EBU R 128, ATSC A/85 short- and
  long-form, and AES77 presets plus custom JSON/TOML profiles. Every evaluated
  rule and the aggregate PASS/FAIL result are available in machine-readable
  reports.
* **True peak** is measured by 4× polyphase FIR oversampling (Kaiser-windowed
  lowpass, unity DC gain), so inter-sample peaks that exceed sample peaks are
  caught — and the gain is reduced so the output never clips after DAC
  reconstruction.
* **TPDF dither** is available for integer output to eliminate quantization
  distortion when reducing word length.
* **Transactional output** stages every encode beside its destination and only
  replaces the requested path after encoding, metadata, and verification
  succeed. Failed jobs leave an existing destination untouched.

## Formats

Reads and writes PCM 8/16/24/32-bit and IEEE float 32/64-bit RIFF/WAVE, RF64,
and BW64, including `WAVE_FORMAT_EXTENSIBLE` files. `auto` switches from RIFF
to RF64 when the output would exceed 4 GiB.

## Build

```sh
# The default build supports every input format plus WAV/FLAC output without LAME:
cargo build --release

# Optional MP3 output (MP3 input needs only the default Rust crates):
sudo apt-get install -y libmp3lame-dev
cargo build --release --features mp3-encoding

# Optional statically linked Ogg Opus input/output:
cargo build --release --features opus-encoding

# Optional AAC-LC, ALAC, and Vorbis output (`ffmpeg` on PATH at runtime):
cargo build --release --features ffmpeg-encoding

# Optional cross-platform CLAP and Linux LV2 plug-ins:
cargo build --release --features clap-plugin,lv2-plugin

# Optional VST3 host wrapper (requires CMake 3.20+, C++17, and Git):
tools/test-vst3-adapter.sh

# Optional macOS Audio Unit v2 wrapper (requires Xcode and macOS):
tools/test-au-adapter.sh

cargo test
```

The VST3 and Audio Unit adapters are optional source integrations. See
[VST3-ADAPTER.md](VST3-ADAPTER.md) and [AU-ADAPTER.md](AU-ADAPTER.md) for
SDK, signing, and host-installation details.

When `mp3-encoding` is enabled, `build.rs` finds `libmp3lame` via pkg-config,
then standard library paths, and prints a clear install hint if it is missing.

## Releases

Versioned tags automatically publish GitHub Releases containing portable Forge
binaries for Linux x86-64, Windows x86-64, macOS Intel, and macOS Apple
Silicon. Archives also contain the cross-platform `forge-live.clap` plug-in.
The full `linux-x86_64` archive remains the compatible default and contains a
generic PGO `forge` CLI. Linux releases also provide a supplemental
`linux-x86_64-v3` archive containing only a faster PGO `forge` CLI; it requires
the x86-64-v3 ISA level (including AVX2, BMI2, FMA, and OS AVX state support).
Use the generic archive if compatibility is uncertain. Other Linux tools,
shared libraries, wheels, and package-manager installations remain generic.
Each release includes generated release notes, `SHA256SUMS`, SPDX and
CycloneDX SBOMs, an offline SLSA provenance bundle, and generated Homebrew,
Scoop, and WinGet manifests. GitHub artifact attestations provide verifiable
build provenance for every checksummed release asset. Publication is blocked
unless an independent Linux rebuild is byte-for-byte identical and every
checksum and attestation verifies.

Install a release directly with
[`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall):

```sh
cargo binstall forge-normalizer
```

Verify a downloaded release before using it:

```sh
tag=v0.43.0
mkdir "forge-${tag}" && cd "forge-${tag}"
gh release download "$tag" --repo penguin425/audio-normalizer
sha256sum -c SHA256SUMS
gh attestation verify "forge-${tag}-linux-x86_64.tar.gz" \
  --repo penguin425/audio-normalizer \
  --bundle "forge-${tag}.slsa.jsonl" \
  --signer-workflow penguin425/audio-normalizer/.github/workflows/release.yml
jq -e . "forge-${tag}.spdx.json" "forge-${tag}.cdx.json" >/dev/null
```

The same attestation command can be run for any filename listed in
`SHA256SUMS`. Release inputs are pinned in the workflow; dependency advisory,
licence, source, duplicate-version, and Rust 1.89 MSRV policies run on every relevant
change. Documented advisory exceptions remain visible in `deny.toml`.

Release tags must exactly match the version in `Cargo.toml`:

```sh
# After merging the Cargo.toml version change into main:
git switch main
git pull --ff-only
git tag -a v0.2.0 -m "Forge v0.2.0"
git push origin v0.2.0
```

Prebuilt binaries include statically linked Ogg Opus and the FFmpeg codec
adapter. WAV/FLAC/Opus output is self-contained; AAC-LC/ALAC/Vorbis output
additionally requires `ffmpeg` on `PATH`. MP3 output requires a source build with
`--features mp3-encoding` and an installed LAME library.

### CI baseline comparison

Release archives include `forge-compare`, a deterministic quality gate for two
Forge delivery manifests. It detects missing assets or evidence, format
changes, newly failing compliance/ADM/codec/EBU QC rules, and measurement drift
beyond configurable tolerances:

```sh
forge-compare baseline.json candidate.json \
  --loudness-tolerance-lu 0.1 \
  --true-peak-tolerance-db 0.1 \
  --format sarif --output forge-results.sarif
```

The command exits with 0 for a pass, 1 for a detected regression, and 2 for an
input/configuration error. `--format json`, `junit`, and `sarif` provide
machine-readable CI evidence. Tolerances can also be stored in a JSON or TOML
file passed with `--config`; findings have stable `FORGE-COMPARE-*` rule IDs
and deterministic asset/rule/metric ordering.

### Reference audio comparison

`forge-audio-compare` decodes, aligns, and directly compares a candidate audio
file with a reference. It reports the signed sample offset, optimal one-to-one
channel assignment, polarity inversions, full-overlap null residual and peak,
exact-sample ratio, channel correlation, and an excerpted one-third-octave
spectral error:

```sh
forge-audio-compare master.wav delivery.flac \
  --alignment-search-ms 1000 \
  --max-offset-samples 0 \
  --min-null-depth-db 60 \
  --output audio-comparison.json
```

Exit codes are 0 for a passing gate, 1 for measured differences outside the
configured tolerances, and 2 for an input/configuration error. JSON or TOML
configuration can be supplied with `--config`. Channel permutation and
polarity correction affect pass/fail only when explicitly enabled with
`--allow-channel-permutation` and `--allow-polarity-inversion`; the original
mapping, polarity, and uncorrected residual remain visible.

This is deterministic, non-normative engineering QC—not a PEAQ conformance
implementation. [ITU-R BS.1387-2](https://www.itu.int/rec/R-REC-BS.1387-2-202305-I/en)
requires reference/test time alignment but leaves synchronization to the
implementation. Forge records its bounded block-energy/sample-correlation
method in every report. Inputs default to 4 GiB each, decoded audio to 400
million samples, 32 channels, and a 10-second hard maximum alignment search.
The output follows
[`audio-comparison-v1.schema.json`](schema/audio-comparison-v1.schema.json)
and uses stable `FORGE-AUDIO-COMPARE-*` rule identifiers.

### Container quality control

`forge-container-qc` audits the original delivery bytes. Structural checks do
not decode media except for ALAC, whose format defines no frame checksum and
therefore requires strict native access-unit decoding for payload validity:

```sh
forge-container-qc master.bw64 --output container-qc.json
forge-container-qc programme.opus
forge-container-qc archive.aiff
forge-container-qc capture.caf
forge-container-qc archive.dsf
forge-container-qc master.dff
forge-container-qc archive.wv
forge-container-qc archive.ape
forge-container-qc lossless.m4a
forge-container-qc master.flac
forge-container-qc delivery.mp3
forge-container-qc broadcast.aac
forge-container-qc contribution.loas
forge-container-qc delivery.ac3
forge-container-qc delivery.eac3
forge-container-qc immersive.iamf
forge-container-qc broadcast.ts
forge-container-qc camera.m2ts
forge-container-qc programme.mxf
forge-container-qc track.opatom.mxf
forge-container-qc edit-project.aaf
forge-container-qc programme.mka
forge-container-qc delivery.webm
```

For WavPack 4/5 files, the audit walks bounded `wvpk` blocks without decoding,
checks header versions, sizes, metadata word lengths and padding, multichannel
`INITIAL_BLOCK`/`FINAL_BLOCK` grouping, 40-bit sample-index continuity, declared
total samples, and stable rate/channel/numeric format. WavPack 5 encoded-block
checksums are recomputed over the exact little-endian 16-bit words using the
reference algorithm; 16-bit hybrid/correction-style and 32-bit lossless
checksums are both supported. The separate header CRC covers decoded samples,
so reports identify it without claiming verification when no decoder ran.

For current-format Monkey's Audio 3.98/3.99 files, the audit locates a bounded
`MAC `/`MACF` descriptor, validates every declared region and PCM/frame field,
unwraps 32-bit seek offsets across files larger than 4 GiB, and requires a
strict in-range boundary for every frame. It recomputes the descriptor MD5 in
the reference format order over the original header data, encoded frames,
terminating data, APE header, and complete seek table. Each frame must reserve
at least the four bytes where its stored decoded-PCM CRC begins; the report
counts these slots but does not emit CRC values or claim equality without
running a decoder.

For ALAC in MP4/M4A, the audit validates every 24/48-byte Apple Lossless magic
cookie, version-0 configuration box, channel layout, bit depth, frame length,
sample rate, and sample-entry/timescale cross-check. It expands `stsz`/`stz2`,
`stsc`, and `stco`/`co64` (or fragmented `trun`) into exact access-unit ranges,
requires each range to stay inside `mdat` and `maxFrameBytes`, and strictly
decodes every unencrypted packet without skipping undecodable frames. ALAC
defines no native packet CRC, so reports state `strict_decode_no_native_checksum`
instead of claiming checksum equality; encrypted packets report that payload
validation requires decryption keys. Configuration semantics follow Apple's
[ALAC magic-cookie description](https://github.com/macosforge/alac/blob/master/ALACMagicCookieDescription.txt)
and its [reference codec](https://github.com/macosforge/alac); packet decoding
uses the hardened Symphonia 0.6 ALAC implementation.

AAF audits identify the AMWA stored format by its Compound File Binary header
and root CLSIDs, validate the CFB allocation/directory structure, require the
root property and weak-reference streams plus exactly one Header and
MetaDictionary, and validate every bounded AAF stored-property stream. They
also require Header ContentStorage, Dictionary, and Identification objects and
MetaDictionary class/type definitions.

The `forge-aaf-effect-profiles-metadictionary-object-model-edit-protocol-v3`
layer
additionally decodes the stored-property table and strong-reference indexes
into a bounded ownership graph. It interprets the file's self-describing
MetaDictionary class inheritance, property definitions, and type-reference
graph, then validates dynamically assigned extension property identifiers and
values. Supported dynamic types include integers, strong and weak object
references, enumerations, fixed and variable arrays, sets, strings, streams,
records, renames, extendible enumerations, indirect and opaque values, and
character types. Type/class cycles, unresolved references, incompatible
stored forms, invalid reference targets, malformed values, duplicate IDs, and
excessive definition counts or graph depths are reported.

The same layer verifies one-owner object containment, required inherited
properties for the supported standard classes, primitive/AUID/MobID/rational
shapes, unique Mob and definition identifiers, Mob/Slot mappings, positive
edit rates, component and Sequence length arithmetic, Transition placement
and cut points, SourceClip derivation references and cycles, OperationGroup
definition/data/input consistency, unique parameters, ordered VaryingValue
control points, NestedScope geometry, and local-only NetworkLocator URI
syntax. Files labelled with `Header::OperationalPattern=OpEditProtocol` also
receive the supported normative material-role, track-name,
PhysicalTrackNumber, primary-timecode, common-audio-rate, template/subclip,
and file-source checks. Protocol-only rules are not imposed on an unlabelled
general AAF file.

The effect-profile layer covers all 20 operations in the AMWA AS-01 Effects
Dictionary and the three AS-05 Video Color, Video Title, and Video Opacity
operations. It checks standard operation metadata, declared and required
parameters, ParameterDefinition types, ConstantValue and VaryingValue
Indirect payload types, rational/boolean/integer/string/enumeration ranges,
and the five permitted interpolation definitions. Reports also expose the
normative fallback profiles for unsupported effects/parameters,
interpolation, time variation, and unavailable title fonts; Forge records
these actions as QC evidence and does not render or modify the AAF.

Entry count, object-path depth, individual property/index streams, aggregate
property/index bytes, MetaDictionary definitions/depth, fixed-array length,
and reported failures are capped. Extension stream properties are checked for
existence without loading their content, indirect/opaque payloads are
preserved, and locators are never fetched. CI downloads three SHA-256-pinned
MIT-licensed pyaaf2 files. Its public Avid-origin extension fixture exercises
79 class definitions, 146 type definitions, 71 extension property
definitions, 1,116 interpreted extension values, standard-AUID/vendor-
parameter fallback evidence, and known protocol violations. A pinned,
essence-free output from the official AAF SDK `ExportAS05Effects` reference
example independently exercises all three AS-05 profiles and 25 constant
parameters. This is reference-fixture interoperability evidence, not full AAF
SDK certification or semantic interpretation of every vendor-specific
payload. The checks follow the
[AAF Object Specification](https://aafassociation.org/specs/object_spec.html),
[AAF Edit Protocol](https://static.amwa.tv/as-01-aaf-edit-protocol-spec.pdf),
[AAF Effects Protocol](https://static.amwa.tv/as-05-aaf-effects-protocol-spec.pdf),
[AAF stored/low-level format specifications](https://aafassociation.org/html/techinfo/index.html),
and [MS-CFB](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-cfb/53989ce4-7b05-4f8d-829b-d08d6148375b).

MPEG-TS/M2TS audits detect 188/192-byte packet layout, transport and
adaptation-field errors, payload continuity gaps, exact retransmissions,
CRC-protected PAT/PMT programme maps, declared audio codecs, bounded PES
headers, and monotonic 90 kHz audio PTS values. The report keeps programme,
PID, language, PCR, and timing evidence without decoding the audio.

MXF audits use a bounded SMPTE ST 377-1 KLV scanner. They verify BER/value
bounds, run-in limits, Header/Body/Footer Partition Pack fields and links,
stable OP1a/OP-Atom labels, KAG and SID use, Index Table declarations, Generic
Container essence, terminal Random Index Pack entries, and sound descriptor
sampling/channel/quantization geometry. If the registered AS-11 Core Framework
key is present, Forge additionally applies the auditable AMWA AS-11 UK DPP
structural/audio subset: one Core Framework, OP1a, closed-complete Header,
KAG size 1, RIP, index-before-essence, and mono 24-bit/48 kHz PCM descriptors.
Absence of that explicit marker is reported as “not detected,” never as an
AS-11 compliance claim. See
[SMPTE ST 377-1](https://pub.smpte.org/latest/st377-1/st377-1-2019.pdf) and the
[AMWA AS-11 UK DPP HD rules](https://amwa-tv.github.io/AS-11_UK_DPP_HD/AMWA_AS_11_UK_DPP_HD.html).

AC-3/E-AC-3 elementary-stream audits scan bounded syncframes without FFmpeg.
They validate frame sizes, sample rates, bitstream IDs, channel mode/LFE,
`dialnorm`, complete bitstream-information syntax, and stable codec
configuration. The normative `compr` word is interpreted as its RF-mode
decoder gain in dB; Forge also reports the encoded gain ranges for `dynrng`
line mode and `compr` RF mode without inventing an authoring-preset name.
E-AC-3 groups syncframes into six-block access units, reports every independent
presentation and combined dependent-substream channel map, and enforces stable
presentation membership, sequential substream IDs, compatible block geometry,
and complete-mix compression-word placement. A legacy AC-3 independent core
followed by E-AC-3 dependent substreams is grouped as one E-AC-3 access unit,
with core and extension bitstream IDs validated independently.

Dolby Digital Plus
[JOC](https://professionalsupport.dolby.com/s/article/What-is-Dolby-Digital-Plus-JOC-Joint-Object-Coding)
is asserted only when the two-byte Extension Type A `addbsi` payload has zero
reserved bits and a complexity index from 1 through 16, appears consistently
on independent substream I0, and the core stream is 48 kHz 5.1. A malformed or
intermittent JOC claim fails a stable
`FORGE-EAC3-ATMOS-JOC` check. Object rendering remains deliberately external:
render every delivered presentation with an authoritative JOC decoder and
audit the WAVE outputs with `forge-presentation-qc` using
`"codec": "eac3-joc"`; the report retains renderer name/version evidence.
Syntax follows
[ATSC A/52:2018](https://www.atsc.org/wp-content/uploads/2021/04/A52-2018.pdf)
and
[ETSI TS 102 366 V1.4.1](https://www.etsi.org/deliver/etsi_ts/102300_102399/102366/01.04.01_60/ts_102366v010401p.pdf).

Standalone IAMF audits follow
[AOMedia IAMF v1.1](https://aomediacodec.github.io/iamf/v1.1.0.html).
They enforce bounded LEB128 OBU framing, the normative 2 MiB OBU limit,
sequence headers and profiles, descriptor ordering, data redundancy/trimming
flags, and complete-file consumption without decoding codec payloads. Stable
`FORGE-IAMF-CODEC-CONFIG`, `FORGE-IAMF-AUDIO-ELEMENT`,
`FORGE-IAMF-MIX-PRESENTATION`, `FORGE-IAMF-PROFILE-CONSTRAINTS`,
`FORGE-IAMF-DESCRIPTOR-LINKS`, `FORGE-IAMF-AUDIO-FRAME-LINKS`,
`FORGE-IAMF-PARAMETER-BLOCK`, and `FORGE-IAMF-TIMELINE` checks parse the Codec
Config prefix and full Opus, FLAC STREAMINFO, or LPCM decoder
configuration, enforce AAC-LC frame and roll declarations, and parse complete
Audio Element and Mix Presentation syntax. The Audio Element check validates
bounded and unique parameter definitions, parameter/frame timing,
one-to-six-layer scalable channel geometry, standard and expanded loudspeaker
layouts, output/recon-gain signalling, and codec-dependent reconstruction-gain
requirements. Scene-based elements validate MONO and PROJECTION Ambisonics
geometry, complete matrices, and
[RFC 8486](https://www.rfc-editor.org/rfc/rfc8486.html) channel mappings.
Mix Presentation validation covers bounded BCP 47 annotations, sub-mixes,
rendering modes and extensions, equivalent shared Mix Gain definitions,
Sound System and binaural layouts, Q7.8 loudness and peak fields, unique
anchored-loudness types, bounded layout extensions, and IAMF v1.1 tags.
Profile filtering resolves referenced elements and enforces the Simple,
Base, and Base-Enhanced limits of 1/2/28 elements and 16/18/28 input channels,
including the single-sub-mix and expanded-layout restrictions. At least one
recognized presentation must conform to the primary profile; alternative
presentations may instead conform to the declared additional profile.
Descriptor and frame checks reject duplicate or missing
codec/element/parameter/substream IDs and require every implicit or explicit
Audio Frame OBU to resolve to a declared substream. Parameter Block validation
covers descriptor- and block-defined timing, constant and explicit subblocks,
STEP/LINEAR/BEZIER Mix Gain data, reserved Demixing modes, per-layer
Reconstruction Gain flags, bounded extension data, and the required
ignore-on-unknown-ID behavior. Exact rational timelines enforce contiguous
parameter coverage, frame-aligned Demixing/Reconstruction Gain, equal audio
substream frame counts and trimming, parameter-before-audio ordering at equal
timestamps, and consistent optional Temporal Delimiters while allowing a Mix
Gain block to span multiple audio frames. Counts, layouts, channel counts,
frame lengths, roll distances, sample rates, sample sizes, parameter
animations, temporal units, and bounded error evidence remain visible in JSON.
Validation is exercised against pinned AOMedia `libiamf` v1.1.0 and
`iamf-tools` v2.1.0 streams in addition to clean and intentionally defective
local fixtures.

ISO-BMFF IAMF files reuse the same OBU validator after bounded, streaming
decapsulation. The container audit requires the `iamf` compatible brand,
zero-valued IAMF `AudioSampleEntry` channel-count/sample-rate fields, exactly
one version-1 `iacb` box, descriptor-only `configOBUs`, and sample addressing
inside `mdat`. Unfragmented files expand complete
`stsc`/`stsz`/`stz2`/`stco`/`co64` tables. Fragmented files resolve
`trex`/`tfhd` defaults, signed `trun` data offsets, per-sample duration, size,
flags, sample-description changes, `tfdt` continuity, and track- or
fragment-level `roll` groups; initialization segments are valid without media
samples. Every IA Sample must contain one descriptor-free Temporal Unit
without a Temporal Delimiter OBU. Stable `FORGE-ISOBMFF-IAMF-*` checks
reconcile `stts` or `trun` duration with codec frame lengths and end trimming,
require `roll` groups matching Opus/AAC `audio_roll_distance`, reject
`stss`/`ctts`, fragment composition offsets, and non-sync sample flags, and
verify that start/end trim is represented exactly by `edts`/`elst`. The
fragmented path is continuously exercised against checksum-pinned AOMedia
`libiamf` v1.1.0 positive and negative vectors.

Encrypted IA tracks recognize an `enca` sample entry only when `sinf/frma`
recovers the original `iamf` format. `FORGE-ISOBMFF-IAMF-CENC-SIGNALING`
validates one `schm`/`schi`/`tenc` chain, scheme version 1, a protected default
KID, 8/16-byte per-sample or constant IVs, and the IAMF-permitted `cenc` or
`cbcs` scheme without pattern skipping. `FORGE-ISOBMFF-IAMF-CENC` reconciles
`senc` or paired `saiz`/`saio` sample counts and IV sizes for both sample
tables and track fragments, and rejects subsample encryption. Version-1/2
`sgpd` `seig` descriptions and `sbgp` runs are resolved per sample, including
version-2 defaults, track-level key rotation, and fragment-local `0x10001`
references. Each effective KID, 8/16-byte per-sample or constant IV, and
full-sample policy is checked against the auxiliary-data geometry and exposed
as bounded JSON evidence. Ciphertext
ranges must remain bounded and disjoint inside `mdat`, but Forge deliberately
does not accept keys or claim OBU, trim, or decoded-audio validation for
encrypted payloads; JSON reports this boundary as
`ciphertext_obu_validation = "requires_keys"`.

Rendering is deliberately separate: render every Mix Presentation and target
layout with an
[Open Audio Renderer v1.0.0](https://aomedia.org/specifications/oar/)
implementation, then list the WAVE outputs under `codec = "iamf"` for
`forge-presentation-qc`. This measures integrated loudness, true peak,
duration, optional compliance, and reference drift while retaining renderer
name/version evidence.

WAVE/RF64/BW64 chunk tables are scanned with bounded memory: audio payloads are
seeked over rather than loaded, including files larger than 4 GiB. Oversized
control chunks and pathological chunk counts fail closed with stable rule IDs.

DSF audits follow Sony's
[DSF File Format Specification 1.01](https://dsd-guide.com/sites/default/files/white-papers/DSFFileFormatSpec_E.pdf):
fixed DSD/fmt/data order and sizes, little-endian fields, channel type/count,
declared file and metadata offsets, sample/data/block geometry, bit order, and
zero-filled unused block data are checked before decoding. DSDIFF audits follow
Philips'
[DSDIFF 1.5](https://dsd-guide.com/sites/default/files/white-papers/DSDIFF_1.5_Spec.pdf):
64-bit big-endian chunk bounds and padding, required FVER/PROP/FS/CHNL/CMPR
ordering, channel identifiers, and interleaved DSD sound geometry are checked
with fixed chunk, channel, rate, and allocation limits. Read-only PCM analysis
accepts only uncompressed `DSD ` coding. Conversion output then feeds the same
[ITU-R BS.1770-5](https://www.itu.int/rec/R-REC-BS.1770-5-202311-I/en)
measurement path as PCM and codec inputs.

Matroska/WebM audits use a bounded RFC 9559 EBML parser. They validate element
sizes/depth/counts, Header/Segment/Info/Tracks/Cluster ordering, track identity
and audio geometry, Block timestamps and all lacing modes, SeekHead/Cue
positions, streaming CRC-32, and Opus codec delay/pre-roll/private data without
buffering complete media payloads. Normative failures and RFC recommendations
use distinct stable rule IDs.

AIFF/AIFF-C audits validate `FORM` and even-byte chunk boundaries, unique
`COMM`/`SSND`, the 80-bit sample rate, PCM frame geometry, and AIFF-C `FVER`,
compression type, and Pascal-string bounds. CAF audits follow Apple's Core
Audio Format 1.0 rules for the versioned header, required `desc`/`data`
placement, variable-packet `pakt` requirements, channel layouts, and
constant-packet byte alignment. Sun/NeXT AU audits validate its big-endian
header, annotation/data boundary, declared data size, audio description, and
linear-PCM frame alignment. All three scanners skip audio payloads and cap the
number of control chunks.

Native FLAC audits follow [RFC 9639](https://www.rfc-editor.org/rfc/rfc9639):
they validate the bounded metadata chain, unique
`STREAMINFO`/`SEEKTABLE`/`VORBIS_COMMENT`/`CUESHEET` blocks, padding, seek
points, comments, cuesheet tracks/indexes, and picture fields. A strict
streaming decode checks frame/header CRCs, decoded sample count and format, and
the PCM MD5 digest when present, without buffering the complete decoded
programme.

Native MP3 audits validate ID3v2.2/2.3/2.4 headers, extended headers, frames,
padding, optional footer bounds, ID3v1/APEv2 trailing metadata, and every
MPEG-1/2/2.5 Layer III frame boundary. Stream version, sample rate, channel
count, CBR/VBR bitrates, and CRC-protected frame counts are exposed without
requiring LAME. Protected-frame CRC-16 values are recomputed over the MPEG
header's protected bits and Layer III side information, with bounded mismatch
evidence. Optional Xing/Info and Fraunhofer VBRI frame/byte counts, seek-table
geometry, and mutual exclusion are cross-checked against the scanned stream.
LAME peak/ReplayGain name, origin, sign, and range fields plus the complete tag
CRC are validated alongside encoder delay/padding. Forge MP3 output backpatches
the Info/LAME tag so gapless duration is preserved; mono encoding uses LAME's
planar API.

Native structural QC also accepts free-format Layer III streams only when a
unique unpadded frame size no larger than 3,456 bytes is demonstrated by three
complete, configuration-matching frames. The inferred base size is then used
to verify every remaining frame once, including per-frame padding, and an
estimated bitrate is reported as non-signalled evidence. This bounded
look-ahead follows the pinned
[mpg123 free-format parser](https://github.com/libsdl-org/mpg123/blob/e36b4a88648ed288932c82e8aab98e1fc08fa409/src/libmpg123/parse.c#L730-L865)
while requiring an extra matching frame and a unique candidate. Free-format
decoding and normalization remain unsupported because the bundled Symphonia
decoder rejects non-indexed bitrates; container QC reports them without
claiming decode support.

The MPEG frame and protection model follows
[ISO/IEC 11172-3](https://www.iso.org/standard/22412.html). Xing/LAME extension
layout and CRC behaviour are recorded against the pinned
[LAME VBR tag implementation](https://github.com/lameproject/lame/blob/1f5cc9487284d5950343aa5d4f70de433468070a/libmp3lame/VbrTag.c);
the non-normative Fraunhofer VBRI layout is cross-checked against the pinned
[OpenCORE parser](https://android.googlesource.com/platform/external/opencore/+/61bf9af643abf0011dcf82ae8a436aeb7e8aae97/fileformats/mp3/parser/src/mp3parser.cpp).

Dependency-free AAC elementary-stream audits cover ADTS and LOAS/LATM without
requiring FFmpeg. ADTS checks every fixed and variable header, frame boundary,
CRC-field presence, buffer-fullness mode, profile, sample rate, channel
configuration, and configuration continuity. LOAS/LATM checks bounded
AudioMuxElement payloads, in-band StreamMuxConfig reuse, AudioSpecificConfig,
explicit and backward-compatible SBR/PS signalling, output rate/channels, and
decoded sample timing.

For RIFF/WAVE, RF64, and BW64 it checks declared sizes, chunk bounds and
alignment, required/unique `fmt` and `data` chunks, `ds64` placement/table/data
sizes/sample counts, byte rate, and block alignment. BWF `bext` audits follow
[EBU Tech 3285 v2](https://tech.ebu.ch/docs/tech/tech3285.pdf): fixed ASCII
fields and null termination, calendar/time ranges, sample-based `TimeReference`,
versions 0–2, UMID/version consistency, zeroed reserved bytes, v2 loudness
ranges and unavailable sentinels, and CR/LF-terminated ASCII
`CodingHistory`. The parsed production fields are exposed in JSON.

XML metadata follows
[ITU-R BS.2088-2](https://www.itu.int/rec/R-REC-BS.2088) and
[EBU Tech 3285 Supplement 5](https://tech.ebu.ch/docs/tech/tech3285s5.pdf).
Forge validates bounded, well-formed UTF-8 `axml`; uncompressed or gzip `bxml`
with bounded expansion; and the complete `sxml` subchunk/alignment layout,
including each subdocument and its sample span. It rejects duplicate XML
chunks, misplaced ADM/S-ADM, ADM without `chna`, and cross-references between
co-located ADM and S-ADM. Recognized `ebuCoreMain` documents must declare an
EBUCore namespace and exactly one direct `coreMetadata` element. The parsed
classification, sizes, compression, roots, subchunks, alignment count, and
sample totals are exposed under `properties.xml_metadata`. Normalization
preserves `axml`, `bxml`, `sxml`, and `chna` byte-for-byte.

Dependency-free Ogg QC follows [RFC 3533](https://www.rfc-editor.org/rfc/rfc3533)
for page bounds, CRCs, sequences, continuation state, and sequential chains.
Ogg Opus adds RFC 7845 headers/tags, mapping-family tables, packet durations,
granules, pre-skip, and end-trim checks. Mapping family 2 additionally follows
[RFC 8486](https://www.rfc-editor.org/rfc/rfc8486): Forge validates the
permitted zeroth- through fourteenth-order channel counts, optional
non-diegetic stereo, and mixed-order inactive ACNs; it reports the declared
ACN/SN3D semantics, stream/coupled counts, and complete channel map as bounded
JSON evidence.
Family 3 uses a distinct demixing-matrix header and is not misinterpreted as a
family-2 mapping table. Ogg Vorbis adds Vorbis I identification/comment/setup
header validation, PCM granules, and strict streaming decode verification.

For ISO-BMFF MP4, M4A, and fragmented MP4 it scans the complete nested box
structure with bounded memory, validates 32/64-bit box sizes and file roles,
extracts audio sample descriptions, verifies `stts`/`stsz`/`stz2`/`stsc` and
`stco`/`co64` sample tables, cross-checks counts, durations, sample bytes, and
`mdat` offsets, and checks `moof` sequence numbers and per-track `tfdt`
timelines. Complete files, fragmented initialization segments, and standalone
media segments are distinguished explicitly. `mdat` payloads are seeked over,
not loaded. AAC sample entries additionally expose and validate their
AudioSpecificConfig, reconcile access-unit timing with the core/output sample
rate, and cross-check gapless encoder delay/end padding from edit lists and
roll/prol sample groups.

Track user data is also inspected for ISO/IEC 14496-12 `ludt`, `tlou`, and
`alou` loudness metadata. Version 0 and version 1 layouts, reserved bits,
entry counts, track/album uniqueness, peak provenance, and every MPEG-D DRC
measurement tuple are validated and exposed in JSON. MPEG-D DRC `udc1`/`udi1`
and `udc2`/`udi2` sample-entry boxes are structurally checked and required to
occur as coefficient/instruction pairs when present. This supplies the
container evidence needed by later Apple HLS and xHE-AAC delivery profiles
without treating optional metadata as mandatory for every MP4 file.
Top-level CMAF `emsg` boxes using `https://aomedia.org/emsg/ID3` are also
validated as version 1 event messages with a positive timescale and one
complete ID3v2.4 tag. ID3 frame bounds and `RVA2` channel adjustments are
checked, and the recommended `aid3` compatible brand is reported.

Results use versioned JSON with stable `FORGE-*` rule IDs and separate
`wrapper`, `bitstream`, and `x-check` layers. Exit status is 0 for pass, 1 for a
QC failure, and 2 for an I/O or unsupported-format error.

### C2PA provenance QC

`forge-provenance-qc` validates audio Content Credentials using the official
Content Authenticity Initiative `c2patool` implementation:

```sh
forge-provenance-qc signed.wav --output provenance.json
forge-provenance-qc signed.m4a --policy trusted \
  --trust-anchors ./C2PA-TRUST-LIST.pem
forge-provenance-qc asset.flac --external-manifest asset.c2pa
```

The command records the exact verifier version and preserves its JSON evidence.
The default `integrity` policy requires a manifest whose signature and content
hard binding validate, but reports an untrusted signing certificate separately.
`--policy trusted` additionally requires an empty C2PA validation-problem list;
configure a current trust anchor or allowed list when using that gate. This
distinction prevents a cryptographically intact self-signed credential from
being presented as a trusted identity.

Validation is delegated instead of partially reimplementing C2PA's COSE, JUMBF,
certificate, timestamp, and evolving assertion rules. `c2patool` is optional
and only required when this command is invoked. Execution has a configurable
timeout, output is spooled with a bounded size, standard input is closed, and
trust-list network access occurs only when the user explicitly supplies a URL.
The report follows `schema/provenance-qc-v1.schema.json`; exit status is 0 for a
policy pass, 1 for missing/invalid/untrusted credentials, and 2 for tool,
configuration, or I/O errors. See the
[C2PA 2.2 specification](https://spec.c2pa.org/specifications/specifications/2.2/specs/C2PA_Specification.html)
and [official c2patool documentation](https://github.com/contentauth/c2pa-rs/blob/main/cli/docs/usage.md).

### HLS and CMAF package QC

`forge-streaming-qc master.m3u8 --profile rfc8216` validates Media and
Multivariant Playlists, singleton and URI-bearing tags, attribute uniqueness,
required rendition attributes, protocol versions, target/segment durations,
local resource presence, discontinuity placement, sequence numbers, and fMP4
initialization signaling. Local fMP4/CMAF headers and segments are passed
through the container auditor; fragment sequence numbers and `tfdt` decode
times are then cross-checked across segment boundaries. Local MPEG-TS/M2TS
segments receive bounded packet, continuity, PAT/PMT, PES, and PTS audits.
Outside an explicit `EXT-X-DISCONTINUITY`, programme/PID/codec/language
configuration must remain stable and audio PTS must advance across segment
boundaries.
Timed metadata elementary streams (`stream_type` `0x15`) are assembled across
TS packets with bounded PES sizes; their ID3v2 tags, `RVA2` relative-volume
entries, and 90 kHz presentation timestamps are validated. For CMAF, Forge
recognizes the AOMedia timed-ID3 `emsg` scheme, requires ID3v2.4, warns when
the recommended `aid3` brand is absent, and checks event-time ordering across
local segments. `RVA2` expresses a relative playback adjustment rather than
an absolute LUFS measurement, and the report preserves that distinction.

For local fMP4 audio alternatives declared by `EXT-X-MEDIA`, Forge resolves
each `GROUP-ID` and verifies that every language rendition uses CMAF, covers
the same duration, exposes aligned segment boundaries, and carries identical
discontinuity state. Remote renditions remain out of scope unless they are
made locally available.

Use `--profile apple-hls` to add current Apple authoring requirements:
six-second target and aligned-boundary recommendations, equal rendition target
and content durations, playlist-type consistency, the 0.5-second segment ceiling,
`CODECS`, and fMP4 `ludt` evidence. Normative violations fail the command;
recommendations remain explicit warnings and do not change exit status. Remote
resources are never fetched implicitly. Results conform to the published
`schema/hls-qc-v1.schema.json` contract.

Use `--profile ll-hls` for the Low-Latency Server Configuration Profile.
It validates `EXT-X-PART` geometry and exceptions, `PART-TARGET`, hold-back
and skip-boundary relationships, delta-update versions, preload-hint types
and byte ranges, `CAN-BLOCK-RELOAD`, `PROGRAM-DATE-TIME`, and Rendition
Report live-edge values. For local Multivariant packages it also requires
the complete Rendition Report set and aligned Discontinuity Sequence state.
This is a static package audit: it does not claim to test the origin/CDN's
blocking response, HTTP/2 or HTTP/3 behavior, cache policy, or range support.

The profiles track [RFC 8216](https://www.rfc-editor.org/rfc/rfc8216),
Apple's current [HLS authoring specification](https://developer.apple.com/documentation/http-live-streaming/hls-authoring-specification-for-apple-devices),
the [HLS 2nd Edition draft-22 dated 2026-05-01](https://datatracker.ietf.org/doc/draft-pantos-hls-rfc8216bis/22/)
(work in progress, not an RFC), and the segmented-media model in
ISO/IEC 23000-19:2024 CMAF.

### DASH and CMAF package QC

`forge-streaming-qc stream.mpd --profile iso23009` performs bounded-memory
MPEG-DASH MPD checks for the required namespace and timing attributes, Period
and AdaptationSet structure, unique Representation identifiers, bandwidth,
inherited content/codec/audio properties, and inherited `SegmentTemplate`,
`SegmentList`, or `SegmentBase` addressing. `SegmentTimeline` expansion also
applies to lists, and static presentations must have an explicit duration
bound.
When `--profile` is omitted, `.mpd` inputs select `iso23009` and other inputs
select `rfc8216`.

Use `--profile dash-if-iop` to additionally require one addressing mode across
an AdaptationSet, check aligned segment boundaries across representations, and
report missing audio language declarations. Local initialization and media
templates/lists are expanded with a fixed resource cap. Segment-list URL counts
and byte ranges are checked.
For local indexed `SegmentBase` media, the bounded `indexRange` must contain
exactly one valid `sidx`; its timescale is reconciled with the MPD, and
initialization/index ranges are checked against the resource size.
CMAF/fMP4 resources are passed through the ISO-BMFF auditor, including
zero-duration initialization headers, `mvex` order, movie-fragment-relative
addressing, sequence continuity, and monotonic `tfdt` decode times. Remote,
absolute, parent-directory, and unresolved template references are never
fetched implicitly. Results conform to
`schema/dash-qc-v1.schema.json`.

Registered CICP `ProgramLoudness` and `AnchorLoudness` Essential/Supplemental
Properties are accepted at AdaptationSet or Representation scope. Values must
use an explicit `LKFS` or `LUFS` unit and remain consistent through
inheritance. When initialization media is local, Forge compares each claim
with the corresponding ISO-BMFF `ludt` MPEG-D loudness measurement. When local
`udc1`/`udi1` or `udc2`/`udi2` MPEG-D DRC boxes are present, Forge reports
their paired container evidence. It does not invent an MPD-only DRC value or
treat those optional boxes as a substitute for codec-specific in-stream DRC.

Adaptation Set Switching descriptors using
`urn:mpeg:dash:adaptation-set-switching:2016` are resolved within their
Period. Forge rejects malformed, duplicate, self, dangling, or cross-media
references and inconsistent `segmentAlignment`/`subsegmentAlignment` claims.
When template or list timing is bounded, it compares every Representation
across the referenced sets after exact timescale and
`presentationTimeOffset` normalization, so equivalent language renditions
with different MPD timescales compare correctly. A warning records when
`SegmentBase` or an unbounded timeline does not expose enough MPD-level
boundary evidence for that comparison.

Use `--profile dash-live` for dynamic and low-latency presentations. It
requires a timezone-qualified availability anchor, a
positive update cadence, and a supported `UTCTiming` source, then checks the
time-shift availability window inputs, suggested presentation delay,
derivable Period starts, Period continuity/connectivity references,
EventStream ordering and identifiers, CENC protection schemes/default KIDs
and embedded `pssh` boxes, and ServiceDescription latency/playback ranges.
ProducerReferenceTime identifiers, timing pairs, and latency references are
cross-checked; connected representations must retain their addressing mode.
`BaseURL` and segment-addressing availability offsets are combined. When
`availabilityTimeComplete="false"` is effective, Forge also checks finite ATO
and segment/target-latency geometry for templates and lists; locally available
template segments must contain multiple CMAF movie fragments. Indexed
`SegmentBase` resources must remain complete, with non-negative effective
availability offsets.

Pass the preceding full snapshot as
`--previous-mpd previous.mpd` to audit a successive MPD update. Forge requires
both snapshots to pass the selected profile, stable MPD identity and
`availabilityStartTime`, strictly increasing
timezone-qualified `publishTime`, stable relative Period/AdaptationSet order,
an unchanged Representation ID set/order in retained AdaptationSets, and
functionally equivalent inherited media properties. Explicit segment
timelines and lists may drop expired prefixes and append new references, but
overlapping segment times must retain their duration and resource identity.

Pass an RFC 5261 / MPEG-DASH patch as `--mpd-patch update.mpp`, with the base
MPD as the positional input, to apply each operation in document order and
audit the derived MPD as a successive update. Forge checks `mpdId`,
`originalPublishTime`, and `publishTime`, requires every selector to resolve
to exactly one node, and supports namespace-URI-aware element and attribute
QNames; text, comment, and processing-instruction node tests; prefixed
namespace-axis selection; element, attribute, namespace, and mixed-node
`add`, `replace`, and `remove`; `prepend`, `before`, and `after`; one-based
position predicates; and attribute, child-value, or string-value predicates.
Inputs are bounded by the MPD size, element, depth, and operation limits.
Unsupported selector functions such as XPath `id()` fail closed.

Remote access remains disabled by default. Add `--observe-remote` and repeat
`--allow-origin` for every exact HTTP(S) scheme/host/port that Forge may
contact:

```bash
forge-streaming-qc live.mpd --profile dash-live \
  --observe-remote \
  --allow-origin https://time.example.net \
  --allow-origin https://cdn.example.net
```

Forge then performs a bounded, one-shot observation of supported remote
`UTCTiming` sources and up to one advertised media, initialization, or indexed
resource per Representation (with duplicate URIs collapsed). Origin resources
use a `bytes=0-0` range GET;
HTTP HEAD clock sources use the response `Date`, and xsdate/ISO clock sources
use their bounded response body. Every initial and redirected URI must match
an explicit origin allowlist entry. Environment proxy settings are ignored,
credentials and URL fragments are rejected, and query values are redacted
from the report. The defaults are a 5-second timeout per HTTP transaction,
64-KiB response limit, two redirects per target, 32 total transactions, and a
5-second maximum absolute clock offset. The corresponding
`--observation-*` options can adjust these values only within hard safety
ceilings.

`properties.remote_observation` records the policy, selected response
headers, redirect chains, status, elapsed time, retained byte count, and clock
offset for every target. This is not a sustained availability-window or live
cadence test, and it does not claim to validate HTTP chunk delivery.

The profiles track
[ISO/IEC 23009-1:2026 MPEG-DASH](https://www.iso.org/standard/23009-1), the
[DASH-IF Conformance Software](https://github.com/Dash-Industry-Forum/DASH-IF-Conformance),
the DASH-IF [IOP v5 publications](https://dashif.org/guidelines/iop-v5/),
[current Low-Latency Live change request](https://dashif.org/docs/CR-Low-Latency-Live-r8.pdf),
[restricted timing model](https://dashif.org/Guidelines-TimingModel/),
[MPD Patch guidelines](https://dashif.org/DASH-IF-IOP/mpd-patch/),
[RFC 5261 XML Patch operations](https://www.rfc-editor.org/rfc/rfc5261),
[events guidelines](https://dashif.org/docs/IOP-Guidelines/DASH-IF-IOP-Part10-v5.0.0.pdf),
[content-protection guidelines](https://dashif.org/Guidelines-Security/),
the DASH-IF [audio-source metadata registry](https://dashif.org/identifiers/audio_source_metadata/),
and the AOMedia [ID3 timed-metadata carriage specification](https://storage.googleapis.com/downloads.aomedia.org/assets/pdf/CarriageOfID3TimedMetadataCMAF.pdf),
plus ISO/IEC 23000-19:2024 CMAF. DASH-IF recommendations are kept distinct
from normative ISO failures.

### IMF package QC

`forge-imf-qc /path/to/package` performs a bounded, local-only audit of a
SMPTE ST 2067 Interoperable Master Format package. It accepts a package
directory or its `ASSETMAP`/`ASSETMAP.xml`, then verifies:

- AssetMap UUID uniqueness, chunk extents, volume declarations, regular-file
  status, and canonical containment inside the package root;
- PKL membership, assembled sizes, and Base64 SHA-1 or SHA-256 hashes over the
  exact AssetMap chunks;
- CPL, PKL, AssetMap, EssenceDescriptor, and MXF Track File references;
- edit-rate conversion, resource bounds/repeats, segment duration alignment,
  and stable virtual-track identity/type;
- common application-identification and audio descriptor homogeneity
  constraints; and
- auditable MCA channel IDs, tag symbols, label-dictionary ULs, language tags,
  and soundfield group links.

```sh
forge-imf-qc ./feature-imf --output imf-qc.json
forge-imf-qc ./feature-imf/ASSETMAP --compact
```

Remote assets, absolute paths, parent traversal, symbolic links, DTDs, and
unbounded XML are rejected. Referenced Track Files must pass Forge's native
OP1a MXF audit. The report follows
`schema/imf-qc-v1.schema.json`; exit status is 0 for pass, 1 for a QC failure,
and 2 for malformed input or I/O failure.

This is an auditable structural/audio subset, not a claim of full XSD, RegXML,
picture-essence, XML Signature, or individual Application conformance.
SHA-1 is verified when declared for IMF interoperability, but is explicitly
reported as accidental-corruption detection rather than protection against a
malicious substitution. The implementation tracks the
[SMPTE ST 2067 standards family](https://www.smpte.org/standards/st2067) and
the [SMPTE IMF SHA-1 advisory](https://www.smpte.org/standards/advisory-note-imfcontent).

### AES31-3 ADL project QC

`forge-aes31-qc project.adl` audits the plain-ASCII EDML form of an AES31-3
Audio Decision List. The bounded parser accepts records split across lines or
packed together, as emitted by different workstations, and verifies:

- the ADL root, balanced and ordered core sections, required project/version
  fields, and unique singleton fields;
- sample rate, integer or fractional frame rate, ADL level, destination start,
  and sample remainders, including 29.97/59.94 drop-frame skipped labels;
- optional track-list continuity, source/event numbering, URL locator syntax,
  source references, channel-map widths, and declared destination tracks;
- positive edit durations and, when source start/duration are supplied, edit
  containment within those source bounds; and
- bounded PAN, GAIN, MUTE, and MARK automation timestamps, destination
  overlaps/crossfade evidence, and producer-specific extension sections.

```sh
forge-aes31-qc project.adl --output aes31-qc.json
forge-aes31-qc project.adl --compact
```

Input is capped at 16 MiB, 250,000 EDML tokens, and 64 KiB per record value.
Referenced resource URLs are parsed but never fetched or decoded. Reports
follow `schema/aes31-qc-v1.schema.json`; exit status is 0 for pass, 1 for a QC
failure, and 2 for an I/O or safety-limit error.

The method identifier is `forge-aes31-3-edml-structural-v1`. Its scope is
structural/interchange QC, not complete normative certification, and it does
not validate the separate AES31-4 XML representation. The implementation
tracks the official [AES31-3-2021 preview](https://www.aes.org/publications/standards/)
and cross-checks real-world section and timing layouts documented by
[Sound Directions](https://kennisbank.avanet.nl/wp-content/uploads/2019/05/sound-directions.pdf).

### RTP / AES67 / ST 2110 audio QC

`forge-rtp-qc session.sdp [capture.pcap|capture.pcapng]` audits an RTP audio
session description and optionally correlates it with a saved classic PCAP or
PCAPNG capture:

- RFC 8866 session/media structure, RTP/AVP payload mappings, destination,
  port, clock rate, channels, packet time, and RFC 7273 clock attributes;
- AES67 linear PCM (`L16`/`L24`) and SMPTE ST 2110-30 rate, dynamic payload,
  receiver-capability, payload-duration, and `SMPTE2110` channel-order
  evidence;
- SMPTE ST 2110-31 AM824 dynamic payload, permitted packet times, even
  subframe-sequence counts, reserved AM824 bits, Marker/CSRC constraints, and
  payload geometry; and
- Ethernet (including VLAN), raw IPv4/IPv6, and Linux cooked frames from
  classic PCAP or PCAPNG Enhanced Packet Blocks, including multiple sections
  and interfaces, per-section byte order, decimal/binary `if_tsresol`, and
  signed `if_tsoffset`;
- UDP-flow correlation; RTP header bounds; SSRC/source stability; sequence
  gaps, reorder and duplicates; timestamp steps; sample counts; and arrival
  jitter evidence.

```sh
forge-rtp-qc programme.sdp capture.pcap --profile smpte2110-30
forge-rtp-qc aes3.sdp capture.pcap --profile smpte2110-31 --output rtp-qc.json
forge-rtp-qc session.sdp --profile aes67
```

The report follows `schema/rtp-audio-qc-v1.schema.json`; exit status is 0 for
pass, 1 for a QC failure, and 2 for malformed input or I/O failure. Inputs are
bounded and read locally. PCAPNG Simple Packet Blocks are rejected because
they carry no arrival timestamp; obsolete Packet Blocks are ignored. Live
capture, IP fragment reassembly, encrypted RTP, RTCP quality analysis, PTP
packet/lock verification, and full device or managed-network conformance are
explicitly outside this audit's scope. Clock signaling is checked as evidence,
not as proof that the sender was locked to the declared reference.

The profile checks track [RFC 3550](https://www.rfc-editor.org/rfc/rfc3550),
[RFC 7273](https://www.rfc-editor.org/rfc/rfc7273),
[IETF PCAPNG](https://datatracker.ietf.org/doc/draft-ietf-opsawg-pcapng/),
[SMPTE ST 2110-30](https://pub.smpte.org/doc/st2110-30/20170918-pub/st2110-30-2017.pdf),
and [SMPTE ST 2110-31:2022](https://pub.smpte.org/pub/st2110-31/st2110-31-2022.pdf).

### SMPTE ST 2022-7 redundant RTP QC

`forge-st2022-7-qc primary.sdp primary.pcap secondary.sdp secondary.pcapng`
audits a redundant RTP audio pair from classic PCAP and/or PCAPNG inputs and
simulates the packet union available to a seamless-protection receiver. It
checks:

- equivalent RTP payload type, encoding, clock, channels, packet time, and
  channel order, with distinct leg addressing;
- one identical SSRC plus matching sequence/timestamp identities and complete
  RTP datagram bytes across both legs;
- malformed, fragmented, wrong-version, wrong-payload, and duplicate packets
  independently on each leg;
- packets recoverable from either leg and sequence continuity after merging;
  and
- maximum and 95th-percentile inter-leg arrival skew, optionally enforced
  against a receiver-specific budget.

```sh
forge-st2022-7-qc red.sdp red.pcap blue.sdp blue.pcap \
  --profile smpte2110-30 --max-skew-ms 0.15 --output protection-qc.json
```

The report follows `schema/st2022-7-qc-v1.schema.json`; exit status is 0 for
pass, 1 for a QC failure, and 2 for malformed input or I/O failure. Omitting
`--max-skew-ms` reports measured skew with a warning because ST 2022-7 receiver
classes and deployment budgets differ. The EBU recommends reporting late or
lost packets on each redundant stream and specifies an ultra-low-skew profile
for low-latency ST 2110-30 deployments. Skew results assume the two capture
timestamps share a synchronized timebase.

This bounded, offline analysis cannot prove physical/network path diversity,
live receiver buffer behavior, PTP lock, or complete device conformance. It
tracks [SMPTE ST 2022-7 seamless packet redundancy](https://www.smpte.org/past-events/standards-smpte-st-2022)
and the [EBU Tech 3371 media-node requirements](https://tech.ebu.ch/docs/tech/tech3371.pdf).

### AMWA NMOS snapshot QC

`forge-nmos-qc snapshot.json` performs a bounded, offline audit of an AMWA
NMOS IS-04/IS-05 snapshot. A bundle JSON contains `nodes`, `devices`,
`sources`, `flows`, `senders`, and `receivers` arrays, plus optional
`sender_connections` and `receiver_connections` objects keyed by resource ID.
Alternatively, pass a directory containing `nodes.json`, `devices.json`,
`sources.json`, `flows.json`, `senders.json`, `receivers.json`,
`sender-connections.json`, and `receiver-connections.json`.

The audit checks:

- globally unique UUIDs, NMOS TAI versions, base metadata, and tag structure;
- Node API versions/endpoints, interfaces, internal/PTP clocks, and HTTP(S)
  hrefs;
- the complete Node–Device–Source–Flow–Sender/Receiver reference graph and
  reciprocal Device membership;
- audio Source channels and clocks, plus Flow media type, sample rate, bit
  depth, and source coherence;
- IS-05 active/staged/activation/constraint structure and RTP transport
  addresses and ports;
- reciprocal IS-04 subscriptions and IS-05 active connections; and
- embedded `application/sdp` sender transport files using Forge's
  ST 2110-30 or ST 2110-31 RTP audio auditor.

```sh
forge-nmos-qc ./nmos-snapshot.json --output nmos-qc.json
forge-nmos-qc ./nmos-snapshot-directory --compact
```

The report follows `schema/nmos-qc-v1.schema.json`; exit status is 0 for pass,
1 for a QC failure, and 2 for malformed input or I/O failure. Individual files
are limited to 32 MiB, the aggregate snapshot to 128 MiB, and resource and
connection counts to 100,000 each. Directory inputs do not follow symbolic
links.

This is an offline consistency audit, not a claim of live API, registry,
DNS-SD, authorization, PTP, or network conformance. The implementation tracks
[AMWA IS-04 v1.3.3](https://specs.amwa.tv/is-04/) and
[AMWA IS-05 v1.1.2](https://specs.amwa.tv/is-05/).

### Standards conformance tests

Run the optional conformance suite against the official EBU Loudness Test Set
v5. The script downloads the archive from a public mirror, verifies its
SHA-256 checksum, and runs the Tech 3341 integrated-loudness and Tech 3342
Loudness Range cases:

```sh
./tools/test-ebu-conformance.sh
```

The EBU material is cached outside the repository and is used only for
technical testing under the terms included in its archive.

Run the independent ITU-R BS.2217-2 core compliance set (frequency response,
absolute and relative gating, LFE exclusion, channel weighting, summation, and
programme material) with:

```sh
./tools/test-itu-conformance.sh
```

The script downloads the official attachments directly from ITU, pins every
archive by SHA-256, and keeps the copyrighted WAV files outside the repository.
CI runs the EBU and ITU suites as separate required evidence.

Run the AOMedia OAR/IAMF interoperability matrix with:

```sh
./tools/test-iamf-conformance.sh
```

The matrix pins 42 artifacts from 14 libiamf v1.1.0 conformance vectors by
commit and SHA-256. It exercises standalone IA Sequences, unfragmented MP4, and
fragmented MP4 across LPCM, channel-based and Ambisonics elements, localized
annotations, anchored loudness, and STEP/LINEAR/BEZIER parameter animation.
It also preserves exact expected findings for one intentionally invalid vector
and for upstream MP4 variants with missing or inconsistent packaging evidence.
This provides the input-side vector coverage requested by OAR v1.0.0 section 9;
it does not claim perceptual parity for a renderer.

### Parser hardening

Property tests exercise arbitrary WAVE and delivery-manifest bytes during the
normal Rust test suite. Dedicated `cargo-fuzz` targets cover the WAVE decoder,
delivery-container QC including DSF/DSDIFF, AAC ADTS/LOAS, FLAC, MP3, and Ogg
Opus/Vorbis, ADM XML, HLS, DASH, IMF XML/package resolution, and
delivery-manifest comparison:

```sh
cargo fuzz run wave_reader
cargo fuzz run container_qc
cargo fuzz run adm_profile
cargo fuzz run manifest_compare
cargo fuzz run dsd
cargo fuzz run imf_qc
```

CI builds and smoke-runs every target on relevant pull requests and `main`;
a scheduled workflow repeats the bounded run weekly.

The versioned JSON Schema for `--manifest` output is published at
<https://penguin425.github.io/audio-normalizer/schema/delivery-manifest-v3>.
Schema v3 embeds versioned container and EBU QC evidence while retaining the
flat analysis fields used by JSON, NDJSON, and CSV integrations.

## Usage

```sh
# Normalize a WAV to -16 LUFS (Spotify-like), true-peak ceiling -1 dBFS (default)
forge track.wav -o track_norm.wav --target=-16

# Normalize an MP3 and write MP3 out (mp3 -> mp3 by default)
forge song.mp3                       # writes song_normalized.mp3

# Transcode/normalize: MP3 in, WAV out
forge song.mp3 -o song.wav --target=-14

# Transcode/normalize: WAV in, MP3 out at 320 kbps
forge track.wav -o track.mp3 --format=mp3 --bitrate=320

# Lossless streaming FLAC output (16 or 24 bit)
forge track.wav -o track.flac --format=flac --bits=24 --dither

# Convert before output-domain loudness and true-peak decisions
forge programme.wav -o master.wav --sample-rate 44100 \
  --resample-quality best --bits 24 --dither --verify

# AAC-LC in a gapless M4A container; verify codec loudness/peak drift
forge track.wav -o track.m4a --format=m4a --bitrate=160 --verify

# EBU R128 broadcast target (-23 LUFS)
forge program.wav -o out.wav --target=-23

# Classic peak normalization to -1 dBFS
forge track.wav -o out.wav --mode=peak --target-peak=-1

# Album mode: one shared gain across all tracks (mixed formats allowed)
forge --album a.wav b.mp3 c.flac -o ./normalized/ --jobs 8

# Recursively normalize a library while preserving subdirectories
forge ./library --recursive -o ./normalized

# Checkpoint each completed track and emit versioned lifecycle NDJSON
forge ./library --recursive -o ./normalized \
  --job-state ./work/library-job.json \
  --progress ./work/library-progress.ndjson

# Preview outputs without writing, and explicitly allow replacement when ready
forge ./library --recursive -o ./normalized --dry-run
forge ./library --recursive -o ./normalized --overwrite

# Just measure any file, don't write
forge --analyze song.mp3

# Machine-readable reports
forge --analyze album/*.flac --json
forge --analyze album/*.flac --csv report.csv
forge --analyze album/*.flac --ndjson

# Pipe audio without mixing diagnostics into the encoded byte stream
cat input.flac | forge - --input-format flac -o - --format opus > output.opus

# Stream one JSON object per input to jq, log collectors, or job runners
forge --analyze album/*.flac --ndjson | jq -c 'select(.true_peak_dbtp > -1)'

# Machine-readable EBU R128 delivery checks
forge --analyze programme.wav --compliance ebu-r128 --json

# Published EBU QC: structure, signal health, loudness, and true peak
forge --analyze programme.wav --ebu-qc --expected-duration 1800 \
  --expected-channels 2 \
  --manifest delivery.json

# Measure explicit dialogue/anchor regions for ATSC A/85 long-form delivery
forge --analyze programme.wav --compliance atsc-a85-long \
  --dialogue-ranges dialogue.toml --json

# Locate momentary, short-term, and true-peak violations on a 100 ms timeline
forge --analyze programme.wav --compliance ebu-r128-short \
  --timeline qc.ndjson

# Measure only a source-time range
forge --analyze programme.wav --start 60 --duration 30 --json

# Run a repeatable TOML job; explicit CLI options override the file
forge programme.wav --config forge.toml

# Short-form broadcast QC and a station-specific JSON/TOML profile
forge --analyze commercial.wav --compliance ebu-r128-short --json
forge --analyze programme.wav --compliance station-qc.toml --csv qc.csv

# Produce a BW64 Broadcast Wave master while preserving ADM/iXML metadata
forge programme.wav -o delivery.wav --format wav --bwf --wav-container bw64

# Write container-native loudness metadata without changing encoded audio.
# FLAC uses ReplayGain 2.0; Ogg Opus uses RFC 7845 R128_GAIN.
forge song.flac --write-tags
forge speech.opus --write-tags

# Add shared album tags to every track (audio remains untouched)
forge album/*.flac --album --write-tags

# Also write and verify Apple Sound Check compatibility metadata.
# Supported containers: MP4/M4A/ALAC, MP3, AIFF, and raw AAC.
forge song.m4a --write-tags --sound-check

# Verify exact encoded PCM (or re-decode codec-dependent output) and fail on drift
forge track.wav -o track.flac --verify

# Automatically compensate for codec-induced loudness or true-peak drift,
# re-encoding from the original source at most twice
forge track.wav -o track.mp3 --verify --verify-retries 2

# Reach the loudness target through isolated peaks with a true-peak limiter
forge track.wav -o track.flac --limiter --verify

# Preserve auditable before/after evidence for one file or an entire album
forge track.wav -o track.m4a --limiter \
  --difference-report reports/track-normalization.json
forge --album album/*.wav -o normalized/ \
  --difference-report reports/album-normalization.json

# Use a named playback or broadcast target
forge song.wav -o song.flac --preset spotify --verify
forge programme.wav -o programme.wav --preset ebu-r128

# Print the gain that would be applied, write nothing
forge --gain-only track.wav --target=-14

# Re-encode 16-bit input as 24-bit WAV with dither
forge in.wav -o out.wav --bits=24 --dither
```

Sound Check itself is a current Apple playback feature that adjusts perceived
level between songs; Apple's public documentation does not define a numeric
target or the private analysis/tag algorithm. Forge's compatibility writer
uses the field geometry observed by interoperable tagging tools: the first
gain pair is referenced to 1000, the second to 2500, the peak pair uses a
16-bit full-scale reference, and undocumented pairs are zero. The measurement
input is BS.1770-5 integrated loudness translated through the ReplayGain 2.0
-18 LUFS reference. This policy was checked on 2026-07-30 against
[Apple's Sound Check documentation](https://support.apple.com/en-us/109331);
the reverse-engineered mapping remains explicitly non-normative and may not
match values generated by Apple software.

### Normalization difference evidence

`--difference-report PATH` writes one
[`normalization-difference-v1`](schema/normalization-difference-v1.schema.json)
JSON document containing every output from the job. Each asset records:

- SHA-256 and byte size for the source and completed output;
- source, intended pre-codec, and verified output loudness/peak measurements;
- static gain and a time-resolved effective-gain envelope;
- limiter duration, mean reduction, maximum reduction, and configuration;
- full-scale and ceiling-exceeding sample counts before and after protection;
- decoded-output full-scale endpoint count; and
- loudness, LRA, RMS, sample-peak, true-peak, frame, and duration codec drift.

The intended signal is measured from the exact protected planar-f32 stream
passed to the encoder, so codec drift is separated from normalization and
limiting. Limiter evidence is sampled at 100 ms or a coarser interval when
needed and is bounded to 10,000 points per asset. The report is deterministic
engineering evidence, not a perceptual quality score or PEAQ conformance
claim. It is available for track, recursive/batch, album, verified, and
automatically corrected renders; it cannot be combined with analysis-only,
gain-only, tag-only, or dry-run modes.

### Resumable batch jobs

For an independent multi-file normalization run, `--job-state PATH`
atomically checkpoints every committed output. Repeating the identical command
hash-verifies and skips completed outputs, rebuilds missing outputs, and rejects
changed outputs unless `--overwrite` explicitly authorizes rebuilding them.
Inputs, ordered paths, output formats, and normalization settings are bound to
the state with SHA-256 evidence.

Independent files render concurrently in bounded waves selected by `--jobs`.
Outputs, catalogue rows, checkpoints, and completion events are still
published in input order; a failed asset cannot expose a later staged output.
Use `--jobs 1` when temporary-disk pressure matters more than throughput.

`--progress PATH` writes schema-validated lifecycle NDJSON with job start,
asset start/completion/skip/failure, and job completion events; use `-` for
stdout. See [BATCH-JOBS.md](BATCH-JOBS.md) for recovery rules, limits, event
semantics, and the versioned
[`batch-job-v1`](schema/batch-job-v1.schema.json) and
[`batch-progress-v1`](schema/batch-progress-v1.schema.json) contracts.

### Watch folders

`--watch` continuously discovers supported regular audio files below one input
directory and processes them only after size and modification time remain
unchanged for `--watch-stable-seconds` (default 5). A required
`--watch-state PATH` atomically records observations, in-progress work,
input/output SHA-256, failures, and restart recovery:

```sh
forge incoming/ --watch --recursive \
  --watch-state work/incoming-watch.json \
  --watch-stable-seconds 10 \
  --output normalized/
```

`--watch-once` supports schedulers; `--watch-retry-failed` explicitly requeues
unchanged failures once without creating an automatic retry loop. See
[WATCH-FOLDERS.md](WATCH-FOLDERS.md) for recovery, collision, symlink, bounds,
and edge behavior and the versioned
[`watch-folder-v1`](schema/watch-folder-v1.schema.json) contract.

### Content-addressed analysis cache

`--analysis-cache DIR` reuses core BS.1770-5 / EBU R 128 measurements across
analysis, dry-run, tag, track, verified, and album workflows:

```sh
forge library/*.flac -o normalized/ \
  --analysis-cache .forge-analysis-cache \
  --analysis-cache-max-mib 2048
```

The complete input byte stream and all measurement-changing options are
SHA-256-bound; paths and modification times are not cache identity. Entries
are atomically committed, schema validated, and invalid entries are visibly
recomputed. Multi-input album and independent-file jobs perform cache hit/miss
work in the bounded `--jobs` pool, then report observations, failures, and
commits in input order. `--analysis-cache-read-only` permits hits without
creating, repairing, or evicting data. See
[ANALYSIS-CACHE.md](ANALYSIS-CACHE.md) for scope, eviction and corruption
behavior, bounds, measurement provenance, and the versioned
[`analysis-cache-v1`](schema/analysis-cache-v1.schema.json) contract.

### SQLite catalogue

`--catalogue PATH` records content-addressed source/output evidence,
BS.1770-5/EBU R 128 measurements, the selected profile, Forge/algorithm
versions, and structured invocation provenance:

```sh
forge library/*.flac -o normalized/ \
  --catalogue work/library.sqlite \
  --catalogue-report work/last-run.json
```

Rows are committed with bounded SQLite transactions and deduplicated by
content, method, profile, and tool version. `--catalogue-report` atomically
exports records committed by the current invocation under the versioned
[`catalogue-report-v1`](schema/catalogue-report-v1.schema.json) contract. See
[CATALOGUE.md](CATALOGUE.md) for durability, concurrency, privacy, bounds, and
backup behavior.

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `--config` | none | Repeatable TOML job settings; explicit CLI options win |
| `-m, --mode` | `lufs` | `lufs`, `peak`, or `rms` |
| `--preset` | none | Named playback/delivery loudness target (see below) |
| `--recursive` | off | Recursively process input directories |
| `--job-state` | none | Atomically checkpoint and resume an identical multi-file normalization job |
| `--progress` | none | Write versioned lifecycle events as NDJSON (`-` for stdout) |
| `--watch` | off | Continuously normalize stable files discovered below one input directory |
| `--watch-state` | none | Required atomic observation and processing state for `--watch` |
| `--watch-stable-seconds` | `5` | Required unchanged size/mtime interval |
| `--watch-poll-seconds` | `2` | Delay between continuous scans |
| `--watch-once` | off | Perform one durable scan and exit |
| `--watch-retry-failed` | off | Requeue unchanged failed entries once at startup |
| `--analysis-cache` | none | Reuse content-addressed core loudness analyses from a directory |
| `--analysis-cache-read-only` | off | Permit cache hits without writes, repairs, or eviction |
| `--analysis-cache-max-mib` | `1024` | Maximum recognized cache-entry storage when the cache is enabled |
| `--catalogue` | none | Record hash-bound measurements, profile, tool version, and provenance in SQLite |
| `--catalogue-report` | none | Atomically export catalogue rows committed by this invocation |
| `--input-format` | none | Container hint required for stdin (`-`) |
| `--dry-run` | off | Analyze and show output paths without writing |
| `--overwrite` | off | Replace output files that already exist |
| `--target` | `-16` | Target LUFS (`--mode lufs`) |
| `--target-peak` | `-0.1` | Target sample peak dBFS (`--mode peak`) |
| `--target-rms` | `-18` | Target RMS dBFS (`--mode rms`) |
| `--ceiling` | `-1.0` | True-peak ceiling dBTP (gain is reduced to respect it) |
| `--max-gain` | none | Cap on applied gain (dB), a boost safety limit |
| `--format` | inferred | `wav`, `flac`, `mp3`, `opus`, or `m4a` output container |
| `--sample-rate` | source rate | Output sample rate from 8000 to 384000 Hz |
| `--resample-quality` | `balanced` | Windowed-sinc quality: `fast`, `balanced`, or `best` |
| `--bitrate` | `192` | Lossy encoder bitrate in kbps (MP3/Opus/AAC output) |
| `--quality` | `2` | MP3 encoder quality 0(best)…9(fastest) |
| `--album` | off | One shared gain for all inputs (requires `--mode lufs`) |
| `--channel-layout` | metadata | Override channel order: `mono`, `stereo`, `5.1`, `6.1`, `7.1`, `5.1.4`, or `7.1.4` |
| `--dual-mono` | off | Measure mono intended for two-speaker reproduction with −3.01 dB pan-law compensation |
| `--analyze` | off | Measure only; do not write files |
| `--json` | off | Write analyze results as JSON to stdout |
| `--ndjson` | off | Write one compact JSON analysis object per line |
| `--csv` | none | Write analyze results as CSV to a file or `-` |
| `--compliance` | none | Built-in delivery profile name or custom `.json`/`.toml` profile |
| `--codec-qc` | off | Extract codec metadata automatically and run decoded-delivery QC |
| `--codec-prober` | `ffprobe` | ffprobe-compatible metadata extractor used by `--codec-qc` |
| `--codec-reference` | none | Unencoded reference for loudness/peak/duration round-trip comparison |
| `--codec-qc-tolerance` | `0.5` | Allowed codec/dialnorm deviation in LU/dB |
| `--ebu-qc` | off | Run published EBU baseband QC Items |
| `--silence-threshold` | `-60` | EBU 0078B silence threshold in dBFS |
| `--silence-duration` | `1` | Minimum silence duration in seconds |
| `--clipping-samples` | `3` | Consecutive full-scale samples for EBU 0005B |
| `--tone-frequency` | `1000` | EBU 0014B test-tone frequency in Hz |
| `--tone-threshold` | `-30` | Minimum test-tone level in dBFS |
| `--tone-duration` | `0.5` | Minimum test-tone duration in seconds |
| `--expected-duration` | none | Expected duration for EBU 0009F |
| `--duration-tolerance` | `0.01` | Allowed duration deviation in seconds |
| `--expected-channels` | none | Expected decoded channel count for EBU 0004F |
| `--dropout-threshold` | `-70` | Maximum short-dropout level for EBU 0008B |
| `--dropout-duration` | `0.002` | Minimum interior dropout duration in seconds |
| `--dropout-max-duration` | `0.1` | Maximum interior dropout duration in seconds |
| `--phase-correlation-threshold` | `-0.5` | Reversed stereo-pair correlation threshold for EBU 0012B |
| `--phase-window` | `0.5` | Stereo correlation window in seconds |
| `--click-threshold` | `0.5` | Local impulse threshold in full-scale units for EBU 0057B |
| `--minimum-average-level` | `-50` | Minimum whole-programme RMS dBFS for EBU 0077B |
| `--hum-threshold` | `-50` | Minimum fitted 50/60 Hz harmonic level for EBU 0088B |
| `--hum-duration` | `1` | Minimum continuous hum/buzz duration in seconds |
| `--noise-threshold` | `-60` | Minimum EBU 0086B band-limited noise level in dBFS |
| `--noise-gate` | `-35` | Maximum programme RMS level at which noise is evaluated |
| `--noise-duration` | `1` | Minimum continuous EBU 0086B noise duration |
| `--noise-low-hz` / `--noise-high-hz` | `200` / `15000` | Declared noise-measurement bandwidth |
| `--crosstalk-coherence` | `0.95` | Minimum time-frequency coherence for EBU 0170B |
| `--crosstalk-level-delta` | `18` | Minimum source/victim level delta in dB |
| `--crosstalk-duration` | `1` | Minimum continuous cross-talk duration |
| `--panning-imbalance` | `18` | Stereo-pair imbalance for EBU 0230B in dB |
| `--panning-duration` | `2` | Minimum continuous panning-anomaly duration |
| `--lfe-cutoff` | `120` | Highest expected LFE frequency for EBU 0095B |
| `--lfe-out-of-band-ratio` | `0.25` | Maximum accepted LFE energy above the cutoff |
| `--expect-mono` | off | Require mono or dual-mono for EBU 0124B |
| `--mono-difference-threshold` | `1/32768` | Maximum dual-mono sample difference |
| `--dc-offset-threshold` | `-40` | Forge DC mean limit in dBFS |
| `--interchannel-delay-samples` | `1` | Maximum accepted stereo-pair delay |
| `--stuck-sample-duration` | `0.05` | Minimum active constant-sample run |
| `--discontinuity-threshold` | `0.75` | Adjacent-sample full-scale delta limit |
| `--adm-profile` | none | Validate `ebu-production` ADM profile rules |
| `--adm-profile-mode` | `read` | Apply Tech 3393 `read` or `write` requirements |
| `--adm-profile-report` | none | Write rule IDs, ADM paths, observations and results as JSON |
| `--dialogue-ranges` | none | Explicit dialogue/anchor regions from JSON/TOML |
| `--start` | `0` | Analysis start time in source seconds |
| `--duration` | to end | Maximum analysis duration in seconds |
| `--timeline` | none | Time-resolved QC report (`.json`, `.ndjson`, `.jsonl`, or `.csv`) |
| `--timeline-interval` | `100` | Timeline interval in milliseconds |
| `--gain-only` | off | Print the gain; write nothing |
| `--write-tags` | off | Write and re-read container-native loudness metadata without changing audio (RFC 7845 R128_GAIN for Opus; ReplayGain 2.0 otherwise) |
| `--sound-check` | off | With `--write-tags`, write and exactly re-read non-normative Apple `iTunNORM` compatibility metadata |
| `--verify` | off | Verify achieved level/true peak from exact native WAVE/FLAC PCM or a completed-file codec re-decode |
| `--verify-tolerance` | `0.5` | Allowed post-encode deviation in LU/dB |
| `--verify-retries` | `0` | Automatically correct gain and re-encode up to N times |
| `--difference-report` | none | Versioned JSON gain/limiting/clipping/codec-drift evidence |
| `--limiter` | off | Look-ahead, sample-rate-aware oversampled true-peak limiter |
| `--limiter-lookahead` | `5` | Limiter look-ahead in milliseconds |
| `--limiter-release` | `100` | Limiter release time in milliseconds |
| `--dither` | off | TPDF dither for integer PCM output |
| `--bits` | input's | `8`/`16`/`24`/`32`/`32f`/`64f` output format |
| `--wav-container` | `auto` | `auto`, `riff`, `rf64`, or `bw64` WAV container |
| `--bwf` | off | Preserve/write BWF v2 metadata and measured loudness fields |
| `-j, --jobs` | all cores | Shared channel, album-track, and independent-file worker count |

The signal-health checks use bounded, deterministic PCM analysis: dropouts are
short interior low-level runs, phase reversal is measured over consecutive
stereo pairs, clicks are isolated local impulses, average level is
whole-programme RMS per channel, and hum/buzz fits 50 Hz and 60 Hz plus their
first four harmonics. Noise uses a declared high-pass/low-pass bandwidth and a
quiet-programme gate. Cross-talk combines windowed correlation with
multi-band spectral similarity; panning and delay evidence names both channels.
LFE checks report the energy ratio above the configured cutoff. Forge-specific
rules cover DC offset, sample delay, stuck samples, and discontinuities.

Every rule retains at most 10,000 coalesced events and reports
`events_truncated` when the cap is reached. All thresholds are explicit so a
delivery profile can trade sensitivity against false positives. EBU Item
versions and `source_url` fields follow the current
[EBU QC API v2](https://qc.ebu.io/help/api) catalogue.

> Negative values need `=`: `--target=-16` (clap parses `-16` as a flag otherwise).

### Repeatable job configuration

`--config` accepts TOML. Paths in the file are resolved relative to the
configuration file, and options written explicitly on the command line take
precedence:

```toml
[normalization]
preset = "ebu-r128"
max_gain_db = 6.0
limiter = true
limiter_lookahead_ms = 5.0
limiter_release_ms = 100.0
dither = true

[analysis]
enabled = false
# compliance = "ebu-r128-short"
# dialogue_ranges = "dialogue.toml"
# start_seconds = 60.0
# duration_seconds = 30.0
# timeline = "reports/programme.ndjson"
# timeline_interval_ms = 100.0

[output]
format = "flac"
sample_rate_hz = 48000
resample_quality = "balanced"
bits = "24"
verify = true
verify_tolerance = 0.5
verify_retries = 2
difference_report = "reports/programme-normalization.json"
```

### Presets

| Name | Integrated target | True-peak ceiling | Profile revision / evidence |
|------|-------------------|-------------------|-----------------------------|
| `spotify` | −14 LUFS | −1 dBTP | `spotify-normal-2026-07-30`; published platform policy |
| `apple-music` | −16 LUFS | −1 dBTP | `apple-music-reference-2026-07-30`; Forge engineering reference |
| `youtube` | −14 LUFS | −1 dBTP | `youtube-reference-2026-07-30`; Forge engineering reference |
| `podcast-stereo` | −16 LUFS | −1 dBTP | Common stereo podcast delivery |
| `podcast-mono` | −19 LUFS | −1 dBTP | Common mono podcast delivery |
| `ebu-r128` | −23 LUFS | −1 dBTP | EBU R 128 programme delivery |
| `atsc-a85` | −24 LUFS | −2 dBTP | ATSC A/85 television delivery |
| `arib-tr-b32` | −24 LKFS | −1 dBTP | ARIB TR-B32 Japanese digital television delivery |

The short platform names resolve to the exact revision shown above. The
versioned identifiers are also accepted directly, so repeatable jobs can pin a
revision. These compatibility aliases remain pinned to the listed revision;
future policy observations receive a new identifier rather than changing an
existing job. At startup Forge prints the canonical identifier, evidence
class, first-party source, source publication date when available, verification
date, and caveat.

Spotify, EBU R 128, and ATSC A/85 values follow their published guidance:
[Spotify loudness normalization](https://support.spotify.com/artists/article/loudness-normalization/),
[EBU Tech 3343](https://tech.ebu.ch/docs/tech/tech3343.pdf), and
[ATSC A/85](https://www.atsc.org/atsc-documents/a85-techniques-for-establishing-and-maintaining-audio-loudness-for-digital-television/).
The Japanese broadcast preset follows
[ARIB TR-B32 1.6](https://www.arib.or.jp/english/std_tr/broadcasting/desc/tr-b32.html).
[Apple documents Sound Check](https://support.apple.com/en-us/109331) but does not
publish the numeric target or ceiling used by Forge's reference.
[YouTube documents variable audio enhancement](https://support.google.com/youtube/answer/16619284)
but likewise does not publish Forge's numeric values. Those two profiles and
the podcast entries are engineering references, not platform acceptance
guarantees. Service behaviour can also differ by playback setting, client, and
device; always prefer a distributor's current delivery specification.

### Delivery compliance profiles

| Name | Integrated loudness | Additional limits |
|------|---------------------|-------------------|
| `ebu-r128` | −23.0 ±0.2 LUFS | true peak ≤ −1 dBTP |
| `ebu-r128-live` | −23.0 ±1.0 LUFS | EBU R 128 v5 live-programme allowance; true peak ≤ −1 dBTP |
| `ebu-r128-creative` | ≤ −22.8 LUFS | explicitly signalled lower-target exception; true peak ≤ −1 dBTP |
| `ebu-r128-s2-streaming` | −23.0 ±0.2 LUFS | EBU R 128 s2 v3.0 unchanged stream; true peak ≤ −1 dBTP |
| `ebu-r128-s2-streaming-adapted` | −18.0 ±0.2 LUFS | EBU R 128 s2 v3.0 interim adaptation; true peak ≤ −1 dBTP |
| `ebu-r128-s2-music-low-plr` | −16.0 ±0.2 LUFS | EBU R 128 s2 v3.0 allowance for mostly-music PLR < 15 LU (strictly enforced); true peak ≤ −1 dBTP |
| `ebu-r128-s3-radio` | −23.0 ±0.2 LUFS | EBU R 128 s3:2023 production/exchange; true peak ≤ −1 dBTP |
| `ebu-r128-short` | −23.0 ±0.2 LUFS | true peak ≤ −1 dBTP; max short-term ≤ −18 LUFS |
| `atsc-a85-short` | −24 ±2 LUFS | true peak ≤ −2 dBTP |
| `atsc-a85-long` | −24 ±2 LKFS/LUFS, explicit dialogue regions | true peak ≤ −2 dBTP |
| `arib-tr-b32` | −24 ±1 LKFS/LUFS | ARIB TR-B32 1.6 completed-programme delivery; true peak ≤ −1 dBTP |
| `arib-tr-b32-creative` | ≤ −23 LKFS/LUFS | explicitly signalled creative lower-target exception; true peak ≤ −1 dBTP |
| `aes77-assorted` | ≤ −16 LUFS (target −18, upper tolerance +2) | true peak ≤ −1 dBTP |
| `aes77-music-track` | −16.0 ±0.2 LUFS | true peak ≤ −1 dBTP |
| `aes77-interstitial` | −18.0 ±0.2 LUFS | true peak ≤ −1 dBTP |

ARIB TR-B32 1.6 extends measurement guidance to object-based programmes.
The built-in ARIB profiles evaluate the selected linear PCM presentation; an
object master must first be rendered into every delivered presentation and
each render audited (for ADM workflows, use `forge-presentation-qc` or the
external BS.2127 renderer adapter). Forge does not label an unrendered object
bed as compliant.

ATSC long-form programme compliance uses dialogue/anchor loudness and is
therefore deliberately not implied by `atsc-a85-short`. Supply deterministic,
reviewable regions with `--dialogue-ranges`. Per ATSC A/85:2026-07 Annex M,
the dialogue selection acts as the gate and Forge averages K-weighted energy
without the BS.1770-2+ relative-level gate. Region energies are duration
weighted. Reports identify the measurement standard and method explicitly.
Forge does not silently guess dialogue: use explicit regions or opt into the
auditable detector with `--auto-dialogue`.

For cinematic EBU R 128 s4 QC, use
`--compliance ebu-r128-cinematic --dialogue-ranges dialogue.json`. This selects
BS.1770 absolute/relative gating and enforces a Loudness-to-Dialogue Ratio
(LDR = programme loudness minus dialogue loudness) of at most 5 LU. Dialogue
can come from the mix, the centre channel, or a separate stem via
`--dialogue-source mix|center|stem` and `--dialogue-stem`.

Codec delivery QC can extract metadata directly with an optional
ffprobe-compatible executable:

```bash
forge delivery.eac3 --analyze --json --codec-qc
forge delivery.m4a --analyze --manifest delivery.json \
  --codec-qc --codec-reference master.wav
```

The report records codec/profile, container, bitrate, sample rate, channel
layout, dialnorm, downmix and DRC metadata when the prober exposes them. With
`--codec-reference`, Forge decodes and measures both files and gates loudness,
true-peak, and sample-accurate duration drift. The exact prober path and
`ffprobe-json-v1` extraction schema are preserved for auditability. `ffprobe`
is not a build or runtime requirement unless `--codec-qc` is requested.

For Ogg Opus inputs, container QC validates CRCs, mandatory header-page
boundaries, RFC 6716 packet duration, and RFC 7845 granule increments without
requiring libopus. Initial granule offsets, pre-skip, chained streams, and
final-page end trimming are reported without decoding the audio payload. Ogg
Vorbis QC validates the three mandatory Vorbis I headers, comments, page
boundaries, monotonic PCM granules, and a strict packet-by-packet decode.

AC-4, E-AC-3 JOC, IAMF/OAR, and MPEG-H workflows can audit every externally
rendered presentation
without claiming that Forge is a normative immersive renderer:

```bash
forge-presentation-qc presentations.json -o presentation-qc.json
```

The JSON/TOML specification records `codec`, renderer name/version, and a
unique ID plus rendered WAVE path for every Presentation. Optional reference
paths gate loudness, true-peak, and sample-accurate duration drift; optional
compliance profiles gate each Presentation independently. The report identifies
ETSI TS 103 190 for AC-4, AOMedia IAMF v1.1/OAR v1.0.0 for IAMF, or
ISO/IEC 23008-3 for MPEG-H and preserves renderer provenance as audit evidence.

For channel-based immersive masters, `forge-downmix-qc` applies explicit,
fixture-backed WAVE-order profiles and reports the matrix, loudness delta,
true-peak delta, sample clipping, and ceiling risk. It supports stereo and 5.1
downmix targets plus a 7.1.4 identity verification profile:

```bash
forge-downmix-qc downmix.json --output downmix-qc.json
```

The request names the source layout (`mono`, `stereo`, `5.1`, `6.1`, `7.1`,
`5.1.4`, or `7.1.4`) and one or more profiles. Coefficients and channel
mapping are included in the report; the default true-peak ceiling is 0 dBTP
and optional loudness/peak-delta limits can be used as gates. This is a
non-normative deterministic engineering profile, not an object renderer or a
binaural/HRTF renderer. See [IMMERSIVE-DOWNMIX.md](IMMERSIVE-DOWNMIX.md) and
the versioned [request schema](schema/downmix-qc-request-v1.schema.json).

For a binaural file produced by an external HRTF/object renderer,
`forge-binaural-qc` verifies the stereo output against the immersive source
and, when supplied, a trusted reference render:

```bash
forge-binaural-qc binaural.json --output binaural-report.json
```

The request requires the selected renderer's name/version, model/version, and
lowercase SHA-256 evidence for both renderer and model. Reports gate source
and reference duration drift, reference loudness/true-peak drift, output
true-peak ceiling, and sample clipping. Forge does not bundle or execute an
HRTF renderer; the profile is an auditable engineering boundary. See
[BINAURAL-QC.md](BINAURAL-QC.md) and the versioned schemas in `schema/`.

For sources that need a conservative correction plan, `forge-remediate` emits
a dry-run report without writing audio:

```bash
forge-remediate remediation.json --output remediation-report.json
```

The report binds the source and effective settings with SHA-256 evidence,
projects the minimum static gain, and identifies any true-peak limiter or LRA
compressor action that would still require a fresh render and remeasurement.
It never rewrites in place. Exit status 0 means the bounded plan is feasible;
status 1 preserves a JSON report with infeasibility reasons; status 2 denotes a
request, decode, or resource error. See [REMEDIATION.md](REMEDIATION.md) and
the versioned request/report schemas in `schema/`.

For delivery metadata that needs a conservative, auditable correction,
`forge-metadata-repair` validates the source and writes a separate output
file. It can normalize RIFF/WAVE BWF `bext` v2 loudness fields and the ADM
`audioFormatExtended/@version` declaration while preserving unknown chunks,
XML, and audio bytes:

```bash
forge-metadata-repair metadata-repair.json --output metadata-repair-report.json
```

Use `mode: "validate"` for an exact validate-and-copy operation. MXF is
validate-and-copy only; mutation requests fail closed until a partition/index
table writer is available. `atomic_replace: true` atomically replaces the
destination after the post-write container/ADM validators pass. See
[METADATA-REPAIR.md](METADATA-REPAIR.md) and the versioned metadata repair
schemas.

For AC-4, `forge-ac4-qc` can also drive an explicitly selected licensed or
reference decoder through a bounded, versioned adapter protocol. It requires
the adapter to enumerate every presentation, records AC-4 dialnorm and
loudness-correction metadata, hashes the input/adapter/render bytes, and
independently measures every rendered WAVE output:

```bash
forge-ac4-qc programme.ac4 --adapter /opt/vendor/forge-ac4-adapter \
  --output ac4-qc.json --dialnorm-tolerance-lu 1.0
```

See [AC4-ADAPTER.md](AC4-ADAPTER.md) for the request/response contract,
current ETSI versions, resource limits, and trust boundary.

For a raw MPEG-H Audio Stream, `forge-mpegh-qc` first parses every MHAS packet
itself, including escaped type/label/length fields, SYNC, configuration labels,
and `mpegh3daProfileLevelIndication`. It then uses an explicitly selected
conforming/reference decoder adapter to enumerate the audio scene and render
the default scene and every preset with loudness normalization and DRC
disabled:

```bash
forge-mpegh-qc programme.mhas --adapter /opt/vendor/forge-mpegh-adapter \
  --output mpegh-qc.json --loudness-tolerance-lu 1.0
```

Forge validates group, switch-group and preset references, binds the native
MHAS profile/level to the decoder response, and independently measures every
WAVE render. See [MPEGH-ADAPTER.md](MPEGH-ADAPTER.md) for the versioned JSON
contract, current ISO standards, limits, and trust boundary.

For raw DTS core or DTS-HD elementary streams, `forge-dts-qc` validates every
frame boundary itself, including 16-bit/14-bit BE/LE core forms, core metadata,
DWORD padding, and DTS-HD extension-substream sizes and static counts. It then
uses an explicitly selected licensed or reference adapter to enumerate every
asset and presentation and render each presentation once:

```bash
forge-dts-qc programme.dtshd --adapter /opt/vendor/forge-dts-adapter \
  --output dts-qc.json --max-true-peak-dbtp -1.0
```

Forge checks each render's declared sample rate and channels, records the exact
dialog-normalization/DRC policy, and independently measures integrated LUFS and
true peak. DTS dialog normalization remains separate metadata and is never
treated as programme loudness. See [DTS-ADAPTER.md](DTS-ADAPTER.md) for the
ETSI TS 102 114 protocol contract and safety bounds.

Ordered S-ADM XML frames can be checked as a live-flow capture:

```bash
forge-sadm-qc frame-0001.xml frame-0002.xml -o sadm-qc.json
```

The ITU-R BS.2125-1 audit checks mandatory frame structure, frameFormat ID
syntax and sequence, valid types and times, flowID/timeReference stability,
contiguous non-overlapping frame timing, transportTrackFormat presence, and
changedIDs status values. It validates captured metadata frames; transport and
audio/metadata synchronization remain the responsibility of the carrying
interface.

Optional ASR/VAD engines can feed dialogue QC through a versioned, reviewable
adapter instead of being trusted implicitly:

```bash
forge-dialogue-provider provider.json --threshold 0.6 \
  --ranges-output dialogue.json --output provider-audit.json
forge programme.wav --analyze --compliance ebu-r128-cinematic \
  --dialogue-ranges dialogue.json
```

Provider JSON records the ASR/VAD/hybrid engine and version, model name,
model version, model SHA-256, source duration, and confidence for every sorted
non-overlapping segment. Forge validates that provenance and the time bounds,
then exports only accepted ranges. Transcripts are deliberately not copied
into the audit report; their presence is recorded for privacy review.

Optional audio-quality models use a separate, non-normative anomaly contract so
that model findings never redefine LUFS or EBU compliance:

```bash
forge-anomaly-provider anomaly-provider.json \
  --confidence-threshold 0.6 --severity-threshold 0.5 \
  --output anomaly-audit.json
```

The v1 contract records the source/model SHA-256, provider and model versions,
bounded time-sorted findings (`noise`, `pop`, `dropout`, `lip-noise`,
`phase-cancellation`, `clipping`, or `other`), and the thresholds used for
review. It accepts optional external ONNX/Demucs-like providers without adding
a model runtime to the default build. See
[ANOMALY-ADAPTER.md](ANOMALY-ADAPTER.md) for the schema, limits, trust
boundary, and future model acceptance requirements.

An explicit CPU ONNX reference adapter is available only with the
`onnx-provider` feature. It requires a caller-supplied runtime shared library,
model manifest, model SHA-256, and bounded feature-frame sidecar; it never
downloads weights or silently falls back to a passing result. See the
`Explicit ONNX reference adapter` section in
[ANOMALY-ADAPTER.md](ANOMALY-ADAPTER.md).

Remote media probing is a separate, explicit network operation. The
`forge-remote-qc` command reads only an allow-listed HTTP Range response,
supports public `s3://bucket/key`, `gs://bucket/key`, and HTTPS object syntax,
and emits a redacted fetch manifest. It rejects credentials, unauthorized
redirects, origins that ignore Range, and responses beyond the configured
request/byte/time/object limits:

```bash
forge-remote-qc https://cdn.example.test/audio.wav \
  --allow-origin https://cdn.example.test \
  --range 0-131072 --output remote-qc.json
```

The result is a header/prefix probe, not a loudness measurement or a
full-object download. Plain HTTP requires the explicit
`--allow-insecure-http` flag. Local `forge` and other QC commands never make
network requests implicitly. See
`schema/remote-qc-v1.schema.json` and
`schema/remote-range-v1.schema.json` for the machine-readable contracts.

### Bounded REST service

`forge-service` provides an optional stateless HTTP analysis endpoint for
workers and upload gateways. It accepts audio bytes, never accepts a local
filesystem path, and writes the upload only to a temporary file before using
the same decoder and report contract as the CLI:

```bash
forge-service --bind 127.0.0.1:8080
curl --fail-with-body -X POST \
  -H 'Content-Type: audio/wav' \
  -H 'X-Forge-Filename: programme.wav' \
  --data-binary @programme.wav \
  http://127.0.0.1:8080/v1/analyze?profile=ebu-r128
```

`GET /healthz` and `GET /readyz` return a small health contract. `POST
/v1/analyze` returns the normal `AnalysisReport` inside
`service-analysis-v1`; built-in programme profiles can be selected with the
`profile` query parameter. The service rejects chunked requests and duplicate
headers, and has explicit defaults of 64 MiB upload bytes, 100 million decoded
samples, four workers, and a 30-second I/O timeout. Override them with
`--max-body-mib`, `--max-decoded-samples`, `--workers`, and `--timeout-ms`.

Pass `--metrics` to expose bounded Prometheus text at `GET /metrics`:

```bash
forge-service --bind 127.0.0.1:8080 --metrics
curl --fail http://127.0.0.1:8080/metrics
```

For a local OpenTelemetry adapter, add `--otel-jsonl PATH`. It appends fixed,
bounded server-span records as JSONL; it is intentionally not an OTLP client.
Both options preserve the same authentication policy as the analysis
endpoints. See [`SERVICE-METRICS.md`](SERVICE-METRICS.md) for metric names,
bucket units, trace metadata, and deployment guidance.

The default bind address is loopback. A non-loopback bind is rejected unless
the environment variable named by `--auth-token-env` (default
`FORGE_SERVICE_BEARER_TOKEN`) contains a bearer token. Send it as
`Authorization: Bearer <token>`. TLS termination, rate limiting, and durable
job storage belong at the deployment gateway; this reference service is
intentionally bounded and synchronous. See `schema/service-analysis-v1.schema.json`,
`schema/service-error-v1.schema.json`, and `schema/service-health-v1.schema.json`.

### Optional gRPC service and cancellation

Build the same `forge-service` binary with the `grpc-service` feature to expose
an HTTP/2 endpoint:

```bash
cargo build --locked --release --features grpc-service --bin forge-service
forge-service --grpc-bind 127.0.0.1:9090
```

The protocol is versioned in [`proto/forge_service.proto`](proto/forge_service.proto)
and provides `Analyze`, `Cancel`, `Health`, and opt-in `Metrics` RPCs. `Analyze` carries the
audio bytes and an explicit `request_id`; IDs are bounded and must be unique
while active. `Cancel` marks a running request for cooperative cancellation at
decode/analysis checkpoints, and client disconnects and request deadlines also
trigger the same cancellation flag. The gRPC endpoint uses the REST service's
bearer-token policy and upload, decoded-sample, worker, and timeout limits.
The separate `ForgeMetrics` service's `Metrics` RPC returns the same Prometheus
text when metrics are enabled. Keeping observability in a separate service
preserves compatibility for existing `ForgeAnalysis` implementations. The
feature is opt-in, so the default library/REST build does not include the
Tokio or tonic runtime.

An existing audit can be attached to a batch delivery manifest in input order:

```bash
forge one.wav --analyze --manifest delivery.json \
  --anomaly-audit anomaly-audit.json
```

Repeat `--anomaly-audit` once per input when analyzing multiple files. Forge
revalidates the audit before embedding it under each asset's `model_qc` layer.
That layer is explicitly `non-normative-model-evidence`; a finding is visible
to downstream review but does not change the manifest's EBU/ITU pass totals.
`forge-report explain delivery.json` emits stable
`FORGE-MODEL-ANOMALY-*` findings with time locations, provider/model hashes,
thresholds, and corrective guidance.

The reviewable JSON/TOML sidecar flow remains available through
`--codec-metadata`. Fields include `codec`, `dialnorm_lkfs`,
`encoded_loudness_lufs`, `downmix_mode`, and `tolerance_lu`. Dialnorm is
checked against measured dialogue loudness when available, otherwise
programme loudness. Use `--downmix-qc` to also render and measure the
conventional WAVE-order Lo/Ro presentation (centre/surround at -3.01 dB, LFE
omitted).

Purpose-based compliance profiles avoid assuming that a named platform target
will never change: `streaming-music` (-14 LUFS), `streaming-speech-stereo`
(-16 LUFS), `streaming-speech-mono` (-19 LUFS), and `radio-ebu` (-23 LUFS).
Use `--manifest delivery.json` with batch analysis to write a versioned,
per-asset delivery record containing measurements, metadata checks, compliance
rules, and pass/fail totals.

`forge-report` upgrades older delivery manifests and turns failed compliance,
EBU QC, container, codec, ADM/profile, externally rendered presentation, and
non-normative model-QC rules into actionable, auditable explanations:

```sh
# Exit 1 when migration is needed, without changing the file
forge-report migrate delivery.json --check

# Preserve asset evidence while migrating v1/v2 to v3 atomically
forge-report migrate delivery.json --in-place

# Show source, exact observation/location, requirement, and remediation
forge-report explain delivery.json
forge-report explain delivery.json --format json -o explanations.json

# Retain the original compliance-only rule-explanations-v1 contract
forge-report explain delivery.json --scope compliance --format json

# Standalone audit reports are accepted too
forge-container-qc broken.mkv -o container-qc.json
forge-report explain container-qc.json
```

Migration validates the declared asset/pass/fail counts, rejects unknown
manifest or embedded QC schemas, converts legacy flat
`ebu_qc_results_json` evidence into the v3 `qc` envelope, and is idempotent.
Inputs are bounded to 64 MiB, 100,000 assets, 10,000 QC results per asset, and
1,000 compliance rules per asset.
The default machine-readable explanation format is
`rule-explanations-v2`, with stable rule/category IDs, a JSON Pointer-like
evidence location, the exact structured observation, source identity,
requirement, and remediation. Output is bounded to 20,000 findings per asset
and 100,000 per report. The `--scope compliance` compatibility mode emits
`rule-explanations-v1`. Custom profiles remain identified by profile and
standard fields rather than being presented as normative first-party sources.

Versioned EBU profiles also record `compliance_standard` and
`compliance_standard_version` in reports and manifests. The s2 adapted and
low-PLR profiles are conditional alternatives, not replacements for the
recommended unchanged −23 LUFS stream.

ADM/BW64 presentation QC uses `--adm-presentations presentations.json`. Forge
checks for both `axml` and `chna`, verifies each supplied ADM presentation ID is
referenced by `axml`, and measures its explicit one-based channel selection.
Reports label this as `direct-channel-map (no ADM object renderer)` so a channel
selection is never misrepresented as a full object-based render.

EBU Tech 3393 Production Profile auditing is available without an external
renderer:

```sh
forge --analyze programme.bw64 --adm-profile ebu-production \
  --adm-profile-mode write --adm-profile-report tech3393.json --json
```

The validator distinguishes the profile's reading and writing requirements. It
checks well-formed `axml`, profileList/profile cardinality, the `EBU Tech 3393`
identifier, required profile name/version/level attributes, unique ADM IDs, and
the Table 49 audioTrackFormat stream reference rule. The same audit validates
the current ITU-R BS.2076-3 model version, element-specific ID syntax, local
content/track references, decimal and fractional-sample time syntax, tagList
constraints, removal of the deprecated `audioMXFLookUp`, and `chna` structure,
track coverage, UID uniqueness, and `axml` cross-references. Every result
records its rule ID, ADM path, requirement, observation, validator version, and
pass/fail state. External common-definition references are reported but are not
incorrectly rejected merely because they are not embedded in `axml`.

Full object/scene presentation QC uses the EBU ADM Toolbox reference
implementation:

```sh
forge --analyze programme.bw64 --adm-render \
  --adm-layout 4+5+0 --adm-profile-level 0 --json
```

Forge first validates the document against the selected ITU-R BS.2168 emission
profile level, then invokes the ITU-R BS.2127 renderer for DirectSpeakers,
Matrix, Objects, HOA, and Binaural rendering items, and finally measures the
rendered BS.2051 loudspeaker signals with Forge's BS.1770 engine. Install
`eat-process` from the
[EBU ADM Toolbox](https://github.com/ebu/ebu-adm-toolbox), or select an
executable with `--adm-renderer`. `--adm-rendered-output rendered.wav` retains
the rendered signals for audition and downstream QC. The renderer remains an
optional runtime dependency; ordinary PCM/ADM preservation does not require it.

`--auto-dialogue` provides deterministic dialogue candidates when reviewed
ranges are not yet available. Detector v2 evaluates 250 ms frames using
adaptive noise floor/SNR, centre or mid focus, zero-crossing rate, 80 Hz–4 kHz
speech-band energy, short-frame amplitude modulation, and voiced periodicity.
Threshold hysteresis and a one-frame hangover avoid fragmented regions.
`--dialogue-confidence` controls selection. Reports preserve the
detector/version, threshold, selected ranges, and every frame's raw features,
confidence, and decision; `--dialogue-detection-report` writes the audit record
separately. This remains a reviewable deterministic detector, not a claim of
AI transcription or semantic understanding.

Loudness Range (LRA) and Peak-to-Loudness Ratio (PLR = maximum true peak in
dBTP minus integrated programme loudness in LUFS) are reported for every
analysis. Custom compliance profiles can apply inclusive minimum/maximum LRA
and PLR limits. `peak_to_loudness_ratio_max_exclusive = true` changes only the
PLR upper boundary from `<=` to `<`; the evaluated rule records its boundary
inclusivity so a value exactly on the limit is auditable. The built-in
`ebu-r128-s2-music-low-plr` profile uses that strict boundary because
[EBU R 128 s2 v3.0](https://tech.ebu.ch/publications/r128s2) permits the
−16 LUFS mostly-music alternative only when PLR is lower than 15 dB.

Reports also include `loudness_range_stable`. EBU Tech 3341 notes that LRA is
not stable during the first 60 seconds, so shorter measurements are marked
provisional instead of being presented as a settled programme characteristic.

Dialogue range files use JSON or TOML:

```toml
[[ranges]]
start_seconds = 12.5
duration_seconds = 8.0

[[ranges]]
start_seconds = 95.0
duration_seconds = 20.0
```

Ranges must be sorted, non-overlapping, and contain audio. Machine-readable
reports include programme `integrated_lufs` separately from `dialogue_lufs`,
dialogue duration, range count, measurement standard/method, and the
compliance loudness basis.

Custom profiles use JSON or TOML. All fields except `name` are optional; a
profile must define at least one rule:

```toml
name = "station-qc"
loudness_basis = "programme" # or "dialogue"
target_lufs = -23.0
lower_tolerance_lu = 1.0
upper_tolerance_lu = 0.5
max_true_peak_dbtp = -1.0
max_short_term_lufs = -18.0
max_momentary_lufs = -15.0
min_loudness_range_lu = 3.0
max_loudness_range_lu = 18.0
min_peak_to_loudness_ratio_lu = 8.0
max_peak_to_loudness_ratio_lu = 15.0
peak_to_loudness_ratio_max_exclusive = true
```

ADM chunks are carried through unchanged during normal normalization. Forge
does not modify individual ADM objects; `--adm-render` provides standards-based
render-and-measure QC through the EBU reference implementation.

## Architecture

```
src/
  lib.rs            public engine API (decoder, audio I/O, DSP, normalize)
  main.rs           CLI wrapper (format resolution + dispatch)
  cli.rs            clap definition
  adm.rs            optional EBU BS.2127 renderer + BS.2168 validation adapter
  decoder.rs        full-buffer and streaming universal decoders
  dsd.rs            bounded DSF/DSDIFF parser + read-only DSD FIR decimator
  aaf_qc.rs         bounded AAF CFB/stored-property reader and structural QC
  aaf_meta_qc.rs    dynamic AAF MetaDictionary and extension-value validation
  aaf_object_qc.rs  AAF object graph, timeline, source, and Edit Protocol QC
  aes31_qc.rs       bounded AES31-3 EDML project/reference/timing QC
  ac4_adapter.rs    bounded licensed/reference AC-4 decoder adapter + evidence
  anomaly_provider.rs external audio-quality anomaly adapter + audit contract
  onnx_provider.rs  opt-in bounded ONNX model/feature contract + reference adapter
  remote_range.rs   allow-listed HTTP Range reader + redacted remote probe
  service.rs         bounded HTTP upload/analyze service and response contracts
  report.rs          analysis/delivery manifest and advisory model-QC envelope
  report_tools.rs    versioned migration and explainable model/compliance findings
  mpegh_adapter.rs  native MHAS framing + bounded conforming decoder adapter
  dts_adapter.rs    native DTS core/HD framing + bounded decoder adapter
  pcm_spool.rs      temporary output-domain PCM reuse between exact stages
  wavpack_qc.rs     native WavPack block/metadata/checksum structural QC
  monkeys_audio_qc.rs native Monkey's Audio frame-boundary + descriptor-MD5 QC
  alac_qc.rs        ALAC magic-cookie + strict native access-unit decode QC
  flacenc.rs        bounded-memory pure-Rust FLAC encoder
  opus.rs           RFC 7845 mono/stereo and Mapping Family 1 Ogg Opus I/O
  mp3enc.rs         mono/stereo LAME encoder with gapless Info/LAME backpatch
  mp3_qc.rs         ID3, MPEG Layer III, Xing/Info, and LAME structural QC
  wav/
    format.rs       PcmKind / WaveFormat
    reader.rs       RIFF/WAVE, RF64, and BW64 demuxer
    writer.rs       RIFF/WAVE, RF64, and BW64 muxer
    mod.rs          AudioBuffer (planar f32)
  dsp/
    convert.rs      PCM <-> f32, parallel decode, TPDF-dithered encode
    simd.rs         AVX2+FMA / scalar gain, sum-of-squares, abs-max
    kwfilter.rs     BS.1770 K-weighting (two biquads, sample-rate independent)
    lufs.rs         gated integrated loudness + RMS/peak
    limiter.rs      streaming look-ahead true-peak limiter
    truepeak.rs     sample-rate-aware SIMD polyphase FIR true-peak meter
  downmix.rs        explicit WAVE-order stereo/5.1/7.1.4 matrices
  downmix_qc.rs     bounded immersive downmix evidence and clip-risk reports
  binaural_qc.rs    external renderer/model evidence and binaural drift gates
  remediation.rs    dry-run true-peak/LRA remediation planner
  metadata_repair.rs bounded BWF/ADM metadata repair and validate-and-copy
  normalize.rs      analyze -> gain (ceiling-protected) -> apply -> write; album mode
  realtime.rs       allocation-free live M/S meter + smoothed gain processor
  bin/forge-live.rs raw f32le real-time pipeline and NDJSON meter
  bin/forge-compare.rs delivery-manifest regression gate for CI
  bin/forge-audio-compare.rs decoded reference/candidate signal comparison
  bin/forge-downmix-qc.rs explicit immersive downmix matrix/QC CLI
  bin/forge-binaural-qc.rs external binaural renderer verification CLI
  bin/forge-remediate.rs bounded smart-remediation dry-run planner
  bin/forge-metadata-repair.rs bounded copy-to-new-file metadata repair CLI
  bin/forge-container-qc.rs wrapper/bitstream/metadata audit CLI
  bin/forge-provenance-qc.rs C2PA integrity and trust-policy audit CLI
  bin/forge-aes31-qc.rs AES31-3 EDML project audit CLI
  bin/forge-ac4-qc.rs licensed/reference AC-4 presentation audit CLI
  bin/forge-mpegh-qc.rs MHAS/scene/preset/render MPEG-H audit CLI
  bin/forge-remote-qc.rs bounded S3/GCS/HTTPS header and Range probe
  service.rs         bounded REST upload/analyze service
  service_metrics.rs Prometheus counters and OpenTelemetry-compatible spans
  bin/forge-service.rs bounded stateless REST analysis service
  bin/forge-dts-qc.rs DTS core/HD asset/presentation audit CLI
  bin/forge-anomaly-provider.rs external audio-quality anomaly audit CLI
  bin/forge-onnx-provider.rs opt-in CPU ONNX anomaly-provider adapter CLI
  lv2.rs            hard-real-time-capable LV2 stereo plugin ABI
  clap_plugin.rs    CLAP stereo effect, automation, state, and latency ABI
  preset.rs         named playback and broadcast loudness targets
build.rs            optionally links libmp3lame for MP3 encoding
tests/
  integration.rs    in-memory round-trip tests (WAV LUFS/peak/album/silence + MP3)
```

## Rust library API stability

The `forge_normalizer` crate can be embedded directly. Starting with v0.94.0,
Forge treats all documented public Rust items as source-compatible across
releases, even while the package version remains below 1.0. Optional-feature
APIs are covered when their feature is enabled, and removing or renaming a
feature is also a compatibility break.

Every pull request compares the all-feature public API with the latest
reachable release tag using `cargo-semver-checks`. A separate downstream-style
integration test compiles and exercises the core analysis, WAV I/O, preset,
and real-time APIs. This gate catches many structural API regressions, but it
does not prove behavioural equivalence or every possible Rust type change;
review remains required. The exact guarantees, exclusions, MSRV policy, and
intentional-breaking-change process are recorded in
[API-STABILITY.md](API-STABILITY.md).

## C API

Release archives contain the versioned Forge C ABI v1 shared library and
`include/forge_normalizer.h`. It provides bounded local-file analysis and a
host-neutral, interleaved-f32 streaming processor without passing Rust-owned
memory across the language boundary. The caller supplies a fixed 80-byte
analysis result, an optional error buffer, and an explicit maximum number of
decoded samples; streaming hosts own their input/output blocks and flush the
processor's fixed look-ahead tail at end-of-stream.

The ABI version, status values, field layout, units, ownership rules, compile
examples, streaming latency/flush contract, and compatibility policy are
documented in [C-API.md](C-API.md). FFmpeg and GStreamer adapters can use the
same streaming symbols without depending on Rust internals.

Optional host adapters are documented in [HOST-ADAPTERS.md](HOST-ADAPTERS.md).
The FFmpeg integration is a public-`AVFrame` bridge for an application-owned
filter (FFmpeg does not provide a stable external `AVFilterPad` plug-in ABI),
while GStreamer provides a dynamic `forge_normalizer` element. Both preserve
the C ABI's fixed latency and explicit end-of-stream flush semantics.
CI compiles the header as strict C11, links a real C consumer to the generated
shared library, and runs it on Linux, macOS ARM, and Windows x86-64.

## Python API

GitHub Releases starting with v0.97.0 include dependency-free Python 3.10+
platform wheels for Linux x86-64, macOS ARM64 and x86-64, and Windows x86-64.
Each wheel bundles the matching C ABI v1 library and exposes bounded local-file
analysis through `forge_normalizer.analyze_file`. The decoded-sample limit is
mandatory, and the immutable result includes integrated, momentary, short-term,
LRA, RMS, sample-peak, and true-peak measurements.

Installation, supported wheel tags, the full result schema, exception and
library-resolution contracts, concurrency behaviour, and supply-chain
verification are documented in [PYTHON-API.md](PYTHON-API.md).

## Browser WebAssembly API

GitHub Releases include `forge-vVERSION-wasm-web.tar.gz`, a dependency-free ES
module for local browser analysis. It measures decoded interleaved Float32 PCM
or in-memory PCM/IEEE-float WAVE, RF64, and BW64 without network, filesystem,
normalization, or encoding capabilities.

```js
import init, { analyzeWav } from "./index.js";

await init();
const result = analyzeWav(new Uint8Array(await file.arrayBuffer()));
console.log(result.integratedLufs, result.truePeakDbtp);
```

The result contains integrated, maximum momentary, maximum short-term, LRA,
RMS, sample-peak, true-peak, and PLR measurements. Non-finite measurements for
silence or insufficient duration are represented as `null`. Fixed limits of
128 MiB input, 24 million decoded samples, 32 channels, and 768 kHz are
enforced before DSP; `limits()` returns the same machine-readable contract.
Use `analyzeInterleaved` with Web Audio decoding for formats outside the WAVE
reader.

## Real-time DSP API

Release archives include `forge-live`, a streaming CLI for shells, OBS/FFmpeg
filter graphs, and audio-pipe integrations. It reads and writes interleaved
little-endian `f32` PCM and sends versioned meter NDJSON to stderr, keeping
stdout binary-clean:

```bash
ffmpeg -i input.wav -f f32le -ac 2 -ar 48000 - \
  | forge-live --sample-rate 48000 --channels 2 --gain-db=3 --ceiling-dbtp=-1 \
  | ffmpeg -f f32le -ac 2 -ar 48000 -i - output.wav
```

The report includes Momentary and Short-term LUFS, sample/true peak, current
gain, maximum limiter reduction, processed frames, and exact latency. Linux
release archives also contain the `forge-live.lv2` stereo plugin bundle; copy
it to an LV2 search directory such as `$HOME/.lv2/`. Its audio callback uses
only preallocated Forge DSP state and exposes a ±24 dB gain control with a
fixed −1 dBTP, 5 ms look-ahead limiter. The bundle exposes that delay through
the current
[LV2 Core `lv2:latency` designation](https://lv2plug.in/ns/lv2core#latency),
so hosts can compensate it without relying on the deprecated
`lv2:reportsLatency` port property.

Every release archive contains `forge-live.clap`, a CLAP 1.x stereo effect for
Linux, Windows, and macOS. Copy it to a CLAP directory scanned by your host
(for example `$HOME/.clap` on Linux or
`$HOME/Library/Audio/Plug-Ins/CLAP` on macOS). It exposes automatable Gain,
True Peak Ceiling, Attack, Release, and Bypass parameters, persists host
state, reports the exact 5 ms look-ahead latency, and supports both real-time
and offline render modes.

The library exposes callback-safe primitives that allocate their working
buffers at construction:

```rust
use forge_normalizer::realtime::{
    RealtimeGainConfig, RealtimeGainProcessor, RealtimeMeter,
};
use forge_normalizer::wav::ChannelRole;

let mut meter =
    RealtimeMeter::new(48_000, vec![ChannelRole::Main, ChannelRole::Main])?;
meter.process_planar(&[left, right])?;
let current = meter.measurement(); // Momentary, Short-term, sample/true peak

let mut gain = RealtimeGainProcessor::new(48_000, 2, RealtimeGainConfig::default())?;
gain.set_target_gain_db(-3.0)?;
gain.process_interleaved(interleaved)?;
# Ok::<(), String>(())
```

The live gain API uses a fixed 5 ms look-ahead true-peak limiter and reports
its exact processing latency through `latency_frames()`.
CI compares deterministic sample output from the streaming CLI, CLAP host
adapter, and LV2 ABI with this processor bit-for-bit on Linux, macOS, and
Windows, including nonuniform host block sizes, gain automation, limiting, and
latency reporting.
The optional FFmpeg AVFrame bridge and GStreamer `forge_normalizer` element
reuse the same processor; build and host-lifecycle requirements are in
[HOST-ADAPTERS.md](HOST-ADAPTERS.md).
The optional VST3 wrapper uses the same C ABI and is built outside Cargo; it
supports mono/stereo float32 processing, host automation, state persistence,
and exact latency reporting. See [VST3-ADAPTER.md](VST3-ADAPTER.md) for the
pinned SDK build and redistribution guidance.
It deliberately does not label a changing live estimate as final Integrated
LUFS; programme-integrated normalization remains the two-pass file workflow.

Named channel layouts use WAVE_FORMAT_EXTENSIBLE order. Forge rejects an
override whose number of channels does not match the input.

## Limitations

* MP3 **encoding** requires the `mp3-encoding` feature and LAME
  (`libmp3lame`) at build/run time. MP3 **decoding** and all other input
  formats need only the Rust crates (symphonia).
* AAC-LC, ALAC, and Vorbis encoding require the `ffmpeg-encoding` feature and
  `ffmpeg` at runtime. `aac-encoding` remains an alias that enables the shared
  FFmpeg adapter for compatibility.
* Ogg Opus encoding, decoding, and normalization support mapping family 0
  mono/stereo and mapping family 1 layouts through 7.1. Dependency-free
  structural QC additionally validates RFC 8486 mapping family 2 Ambisonics;
  it does not claim to decode, render, or measure those Ambisonic channels.
  Sequential chained streams must keep an identical channel mapping layout;
  multiplexed concurrent Ogg streams are rejected.
* By default, the true-peak ceiling is enforced transparently by reducing
  global gain. `--limiter` opts into dynamic look-ahead limiting when reaching
  the loudness target matters more than preserving dynamics unchanged.
* MP3 is lossy: re-encoding shifts loudness by ~0.2–0.4 LU per pass. For
  production work, normalize to WAV/FLAC and encode to MP3 once at the end.
  `--verify --verify-retries N` compensates codec drift by rendering every
  retry from the original input, avoiding generation loss between attempts.
