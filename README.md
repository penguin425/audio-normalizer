# Forge — a SIMD-accelerated EBU R128 / ITU-R BS.1770-5 loudness normalizer

Forge is a fast, standards-correct audio loudness normalizer written in Rust.
It measures loudness the way broadcasters and streaming services do (**EBU R128
LUFS** with the full ITU-R BS.1770-5 K-weighting and two-stage gating) and
applies a single linear gain so the output hits your target — while guaranteeing
the **inter-sample true peak** never exceeds a ceiling, the way Spotify/Apple
mastering does.

## Formats

Forge reads and writes a wide range of formats through a format-agnostic engine:

| | Read (decode) | Write (encode) |
|---|---|---|
| **WAV / RF64 / BW64** (PCM 8/16/24/32-bit, float 32/64-bit) | Forge's own fast parallel demuxer | Forge's own muxer |
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
* Opus output writes RFC 7845 `R128_TRACK_GAIN` and, in album mode,
  `R128_ALBUM_GAIN` comments in signed Q7.8 dB units.
* AAC-LC, ALAC, and Vorbis output use the optional FFmpeg runtime. MP4 gapless
  timing is preserved and Forge writes ReplayGain loudness/peak metadata after
  measuring the encoded result.
* Multichannel Opus uses RFC 7845 Mapping Family 1 and preserves standardized
  3.0 through 7.1 speaker assignments.
* Chained Ogg Opus applies each logical stream's pre-skip, final-granule trim,
  output gain, and channel mapping independently before concatenating samples.
* WAV stays on the fast hand-written path; other inputs transparently route
  through the universal decoder and produce the same planar-f32 buffer the DSP
  engine consumes.
* Common metadata fields and embedded artwork are preserved across
  normalization and remapped to the destination container's primary tag type.
* Broadcast Wave output can preserve `bext`, ADM `axml`/`chna`, and iXML chunks.
  BWF v2 measured loudness fields are updated from the normalized output.

By default the output container follows the input where Forge can encode it
(FLAC → FLAC, MP3 → MP3, Ogg Vorbis → Ogg Vorbis, and M4A → AAC/M4A when
FFmpeg encoding is enabled), and otherwise falls back to lossless WAV.
`--format wav|flac|mp3|opus|m4a|alac|vorbis` and
the `-o` extension override this.

## Why it's fast

* **AVX2 + FMA SIMD** for the gain and energy-summation hot loops, with
  runtime feature detection and a portable scalar fallback (so the binary runs
  anywhere but flies on modern x86-64).
* **Multi-threaded** via rayon — channels and files are processed in parallel.
* **Rolling block energies** make the 75%-overlapping LUFS gating blocks O(1)
  each while retaining only three seconds of filtered energy.
* **Bounded-memory streaming** decodes analysis and normalization in chunks.
  Normalization uses two sequential passes so gain is known before encoding,
  without retaining the complete audio file in RAM.
  Standard-input audio is spooled to a temporary file so the same correct
  two-pass algorithm remains available in shell pipelines.
* Release profile uses `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`,
  and `-C target-cpu=native` (auto-vectorized scalar fallbacks on top of the
  hand-written AVX2).

### Benchmark

On a 32-core host:

```
# 60 s 48 kHz stereo WAV -> WAV (read + K-weight + LUFS + 4x true-peak + gain + write)
Elapsed: 0.21 s   (~286x real-time)   CPU: 174%   RSS: 93 MB

# 60 s 48 kHz stereo WAV -> MP3 @320 kbps (read + analyze + gain + LAME encode)
Elapsed: 0.75 s   (~80x real-time)    CPU: 115%   RSS: 93 MB
```

Decoded MP3 output lands within ~0.3 LU of the target (lossy round-trip).

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

cargo test
```

When `mp3-encoding` is enabled, `build.rs` finds `libmp3lame` via pkg-config,
then standard library paths, and prints a clear install hint if it is missing.

## Releases

Versioned tags automatically publish GitHub Releases containing portable Forge
binaries for Linux x86-64, Windows x86-64, macOS Intel, and macOS Apple
Silicon. Archives also contain the cross-platform `forge-live.clap` plug-in.
Each release includes generated release notes and `SHA256SUMS`.
GitHub artifact attestations provide verifiable build provenance for every
archive and checksum manifest.

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

### Container quality control

`forge-container-qc` audits the original delivery bytes without decoding them:

```sh
forge-container-qc master.bw64 --output container-qc.json
forge-container-qc programme.opus
```

WAVE/RF64/BW64 chunk tables are scanned with bounded memory: audio payloads are
seeked over rather than loaded, including files larger than 4 GiB. Oversized
control chunks and pathological chunk counts fail closed with stable rule IDs.

For RIFF/WAVE, RF64, and BW64 it checks declared sizes, chunk bounds and
alignment, required/unique `fmt` and `data` chunks, `ds64` placement/table/data
sizes/sample counts, byte rate, block alignment, BWF `bext`, and paired ADM
`axml`/`chna` metadata. For Ogg Opus it verifies page CRCs, sequential chains,
headers/tags, mapping-family tables, monotonic granules, pre-skip/end trim, and
consistent layouts. Results use versioned JSON with stable `FORGE-*` rule IDs
and separate `wrapper`, `bitstream`, and `x-check` layers. Exit status is 0 for
pass, 1 for a QC failure, and 2 for an I/O or unsupported-format error.

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

### Parser hardening

Property tests exercise arbitrary WAVE and delivery-manifest bytes during the
normal Rust test suite. Four `cargo-fuzz` targets cover the WAVE decoder,
WAVE/RF64/BW64 and Ogg Opus container QC, ADM XML profile validation, and
delivery-manifest comparison:

```sh
cargo fuzz run wave_reader
cargo fuzz run container_qc
cargo fuzz run adm_profile
cargo fuzz run manifest_compare
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
forge --album a.wav b.mp3 c.flac -o ./normalized/

# Recursively normalize a library while preserving subdirectories
forge ./library --recursive -o ./normalized

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

# Published EBU baseband QC: silence, clipping, tones, duration, loudness and TP
forge --analyze programme.wav --ebu-qc --expected-duration 1800 \
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

# Write ReplayGain 2.0 track tags without changing encoded audio
forge song.flac --write-tags

# Add shared album tags to every track (audio remains untouched)
forge album/*.flac --album --write-tags

# Re-decode the completed file and fail if level/true-peak verification misses
forge track.wav -o track.flac --verify

# Automatically compensate for codec-induced loudness or true-peak drift,
# re-encoding from the original source at most twice
forge track.wav -o track.mp3 --verify --verify-retries 2

# Reach the loudness target through isolated peaks with a true-peak limiter
forge track.wav -o track.flac --limiter --verify

# Use a named playback or broadcast target
forge song.wav -o song.flac --preset spotify --verify
forge programme.wav -o programme.wav --preset ebu-r128

# Print the gain that would be applied, write nothing
forge --gain-only track.wav --target=-14

# Re-encode 16-bit input as 24-bit WAV with dither
forge in.wav -o out.wav --bits=24 --dither
```

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `--config` | none | Repeatable TOML job settings; explicit CLI options win |
| `-m, --mode` | `lufs` | `lufs`, `peak`, or `rms` |
| `--preset` | none | Named playback/delivery loudness target (see below) |
| `--recursive` | off | Recursively process input directories |
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
| `--adm-profile` | none | Validate `ebu-production` ADM profile rules |
| `--adm-profile-mode` | `read` | Apply Tech 3393 `read` or `write` requirements |
| `--adm-profile-report` | none | Write rule IDs, ADM paths, observations and results as JSON |
| `--dialogue-ranges` | none | Explicit dialogue/anchor regions from JSON/TOML |
| `--start` | `0` | Analysis start time in source seconds |
| `--duration` | to end | Maximum analysis duration in seconds |
| `--timeline` | none | Time-resolved QC report (`.json`, `.ndjson`, `.jsonl`, or `.csv`) |
| `--timeline-interval` | `100` | Timeline interval in milliseconds |
| `--gain-only` | off | Print the gain; write nothing |
| `--write-tags` | off | Write ReplayGain 2.0 metadata without changing audio |
| `--verify` | off | Re-decode output and verify achieved level and true peak |
| `--verify-tolerance` | `0.5` | Allowed post-encode deviation in LU/dB |
| `--verify-retries` | `0` | Automatically correct gain and re-encode up to N times |
| `--limiter` | off | Look-ahead 4× oversampled true-peak limiter |
| `--limiter-lookahead` | `5` | Limiter look-ahead in milliseconds |
| `--limiter-release` | `100` | Limiter release time in milliseconds |
| `--dither` | off | TPDF dither for integer PCM output |
| `--bits` | input's | `8`/`16`/`24`/`32`/`32f`/`64f` output format |
| `--wav-container` | `auto` | `auto`, `riff`, `rf64`, or `bw64` WAV container |
| `--bwf` | off | Preserve/write BWF v2 metadata and measured loudness fields |
| `-j, --jobs` | all cores | Worker thread count |

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
```

### Presets

| Name | Integrated target | True-peak ceiling | Intended context |
|------|-------------------|-------------------|------------------|
| `spotify` | −14 LUFS | −1 dBTP | Spotify Normal playback/mastering guidance |
| `apple-music` | −16 LUFS | −1 dBTP | Apple Music Sound Check playback reference |
| `youtube` | −14 LUFS | −1 dBTP | YouTube playback-normalization reference |
| `podcast-stereo` | −16 LUFS | −1 dBTP | Common stereo podcast delivery |
| `podcast-mono` | −19 LUFS | −1 dBTP | Common mono podcast delivery |
| `ebu-r128` | −23 LUFS | −1 dBTP | EBU R 128 programme delivery |
| `atsc-a85` | −24 LUFS | −2 dBTP | ATSC A/85 television delivery |

Spotify, EBU R 128, and ATSC A/85 values follow their published guidance:
[Spotify loudness normalization](https://support.spotify.com/artists/article/loudness-normalization/),
[EBU Tech 3343](https://tech.ebu.ch/docs/tech/tech3343.pdf), and
[ATSC A/85](https://www.atsc.org/atsc-documents/a85-techniques-for-establishing-and-maintaining-audio-loudness-for-digital-television/).
Service playback behavior can change and is not a substitute for a distributor's
delivery specification; Apple Music, YouTube, and podcast entries are practical
playback references rather than acceptance guarantees.

### Delivery compliance profiles

| Name | Integrated loudness | Additional limits |
|------|---------------------|-------------------|
| `ebu-r128` | −23.0 ±0.2 LUFS | true peak ≤ −1 dBTP |
| `ebu-r128-short` | −23.0 ±0.2 LUFS | true peak ≤ −1 dBTP; max short-term ≤ −18 LUFS |
| `atsc-a85-short` | −24 ±2 LUFS | true peak ≤ −2 dBTP |
| `atsc-a85-long` | −24 ±2 LKFS/LUFS, explicit dialogue regions | true peak ≤ −2 dBTP |
| `aes77-assorted` | ≤ −16 LUFS (target −18, upper tolerance +2) | true peak ≤ −1 dBTP |
| `aes77-music-track` | −16.0 ±0.2 LUFS | true peak ≤ −1 dBTP |
| `aes77-interstitial` | −18.0 ±0.2 LUFS | true peak ≤ −1 dBTP |

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

Loudness Range (LRA) is reported for every analysis, together with
`loudness_range_stable`. EBU Tech 3341 notes that LRA is not stable during the
first 60 seconds, so shorter measurements are marked provisional instead of
being presented as a settled programme characteristic.

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
  flacenc.rs        bounded-memory pure-Rust FLAC encoder
  opus.rs           RFC 7845 mono/stereo and Mapping Family 1 Ogg Opus I/O
  mp3enc.rs         MP3 encoder via LAME FFI (interleaved f32 -> MP3 bytes)
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
    truepeak.rs     4x polyphase FIR true-peak meter
  normalize.rs      analyze -> gain (ceiling-protected) -> apply -> write; album mode
  realtime.rs       allocation-free live M/S meter + smoothed gain processor
  bin/forge-live.rs raw f32le real-time pipeline and NDJSON meter
  bin/forge-compare.rs delivery-manifest regression gate for CI
  bin/forge-container-qc.rs wrapper/bitstream/metadata audit CLI
  lv2.rs            hard-real-time-capable LV2 stereo plugin ABI
  clap_plugin.rs    CLAP stereo effect, automation, state, and latency ABI
  preset.rs         named playback and broadcast loudness targets
build.rs            optionally links libmp3lame for MP3 encoding
tests/
  integration.rs    in-memory round-trip tests (WAV LUFS/peak/album/silence + MP3)
```

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
fixed −1 dBTP, 5 ms look-ahead limiter.

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
* Ogg Opus supports mapping family 0 mono/stereo and mapping family 1 layouts
  through 7.1. Sequential chained streams must keep a consistent channel
  layout; multiplexed concurrent Ogg streams are rejected.
* By default, the true-peak ceiling is enforced transparently by reducing
  global gain. `--limiter` opts into dynamic look-ahead limiting when reaching
  the loudness target matters more than preserving dynamics unchanged.
* MP3 is lossy: re-encoding shifts loudness by ~0.2–0.4 LU per pass. For
  production work, normalize to WAV/FLAC and encode to MP3 once at the end.
  `--verify --verify-retries N` compensates codec drift by rendering every
  retry from the original input, avoiding generation loss between attempts.
