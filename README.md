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
| **AAC / ALAC** (.m4a/.mp4) | symphonia (pure Rust) | AAC-LC/M4A via optional FFmpeg runtime |
| **Vorbis** (.ogg) | symphonia (pure Rust) | — (output as WAV) |

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
* AAC-LC/M4A output preserves MP4 gapless timing and writes ReplayGain
  loudness/peak metadata after measuring the encoded result.
* Multichannel Opus uses RFC 7845 Mapping Family 1 and preserves standardized
  3.0 through 7.1 speaker assignments.
* WAV stays on the fast hand-written path; other inputs transparently route
  through the universal decoder and produce the same planar-f32 buffer the DSP
  engine consumes.
* Common metadata fields and embedded artwork are preserved across
  normalization and remapped to the destination container's primary tag type.
* Broadcast Wave output can preserve `bext`, ADM `axml`/`chna`, and iXML chunks.
  BWF v2 measured loudness fields are updated from the normalized output.

By default the output container follows the input where Forge can encode it
(FLAC → FLAC, MP3 → MP3, and M4A → M4A when AAC encoding is enabled), and
otherwise falls back to lossless WAV.
`--format wav|flac|mp3|opus|m4a` and
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

# Optional AAC-LC/M4A output (requires `ffmpeg` on PATH at runtime):
cargo build --release --features aac-encoding

cargo test
```

When `mp3-encoding` is enabled, `build.rs` finds `libmp3lame` via pkg-config,
then standard library paths, and prints a clear install hint if it is missing.

## Releases

Versioned tags automatically publish GitHub Releases containing portable Forge
binaries for Linux x86-64, Windows x86-64, macOS Intel, and macOS Apple
Silicon. Each release includes generated release notes and `SHA256SUMS`.
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

Prebuilt binaries include statically linked Ogg Opus and the AAC/M4A adapter.
WAV/FLAC/Opus output is self-contained; AAC-LC/M4A output additionally requires
`ffmpeg` on `PATH`. MP3 output requires a source build with
`--features mp3-encoding` and an installed LAME library.

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

The versioned JSON Schema for `--manifest` output is published at
<https://penguin425.github.io/audio-normalizer/schema/delivery-manifest-v1>.

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
Forge does not guess dialogue with a classifier.

For cinematic EBU R 128 s4 QC, use
`--compliance ebu-r128-cinematic --dialogue-ranges dialogue.json`. This selects
BS.1770 absolute/relative gating and enforces a Loudness-to-Dialogue Ratio
(LDR = programme loudness minus dialogue loudness) of at most 5 LU. Dialogue
can come from the mix, the centre channel, or a separate stem via
`--dialogue-source mix|center|stem` and `--dialogue-stem`.

Codec delivery QC accepts a reviewable JSON/TOML sidecar with `--codec-metadata`.
Fields include `codec`, `dialnorm_lkfs`, `encoded_loudness_lufs`,
`downmix_mode`, and `tolerance_lu`. Dialnorm is checked against measured
dialogue loudness when available, otherwise programme loudness. Use
`--downmix-qc` to also render and measure the conventional WAVE-order Lo/Ro
presentation (centre/surround at -3.01 dB, LFE omitted).

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

`--auto-dialogue` provides deterministic dialogue candidates when reviewed
ranges are not yet available. The detector uses fixed one-second RMS,
centre/mid focus, and zero-crossing features; `--dialogue-confidence` controls
selection. Reports include the detector/version, threshold, every selected
range, and confidence. `--dialogue-detection-report` writes the same audit data
separately. This is deliberately described as a heuristic detector, not AI.

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

ADM chunks are carried through unchanged; Forge normalizes the rendered PCM
bed and does not currently render or modify individual ADM objects.

## Architecture

```
src/
  lib.rs            public engine API (decoder, audio I/O, DSP, normalize)
  main.rs           CLI wrapper (format resolution + dispatch)
  cli.rs            clap definition
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
  preset.rs         named playback and broadcast loudness targets
build.rs            optionally links libmp3lame for MP3 encoding
tests/
  integration.rs    in-memory round-trip tests (WAV LUFS/peak/album/silence + MP3)
```

## Real-time DSP API

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
* AAC can be written as AAC-LC/M4A when the `aac-encoding` feature is built and
  `ffmpeg` is available. ALAC/Vorbis remain decode-only.
* Ogg Opus supports mapping family 0 mono/stereo and mapping family 1 layouts
  through 7.1. Chained logical streams are not yet supported.
* By default, the true-peak ceiling is enforced transparently by reducing
  global gain. `--limiter` opts into dynamic look-ahead limiting when reaching
  the loudness target matters more than preserving dynamics unchanged.
* MP3 is lossy: re-encoding shifts loudness by ~0.2–0.4 LU per pass. For
  production work, normalize to WAV/FLAC and encode to MP3 once at the end.
  `--verify --verify-retries N` compensates codec drift by rendering every
  retry from the original input, avoiding generation loss between attempts.
