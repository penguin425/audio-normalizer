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
| **WAV** (PCM 8/16/24/32-bit, float 32/64-bit) | Forge's own fast parallel demuxer | Forge's own muxer |
| **MP3** | symphonia (pure Rust) | LAME via FFI |
| **FLAC** (16/24-bit, up to 8 channels) | symphonia (pure Rust) | flacenc (pure Rust) |
| **Ogg Opus** (mono/stereo) | libopus + pure-Rust Ogg | libopus + pure-Rust Ogg |
| **AAC / ALAC** (.m4a/.mp4) | symphonia (pure Rust) | — (output as WAV) |
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
* WAV stays on the fast hand-written path; other inputs transparently route
  through the universal decoder and produce the same planar-f32 buffer the DSP
  engine consumes.
* Common metadata fields and embedded artwork are preserved across
  normalization and remapped to the destination container's primary tag type.

By default the output container follows the input where Forge can encode it
(FLAC → FLAC, MP3 → MP3), and otherwise falls back to lossless WAV.
`--format wav|flac|mp3|opus` and
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
* **EBU Mode analysis** reports Integrated, maximum Momentary (400 ms), maximum
  Short-term (3 s), and Loudness Range (LRA) measurements.
* **EBU R128 compliance reports** evaluate programme loudness against
  −23.0 ±0.2 LU and maximum true peak against −1.0 dBTP, with separate and
  aggregate PASS/FAIL fields in JSON and CSV.
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

Reads and writes PCM 8/16/24/32-bit and IEEE float 32/64-bit WAV, including
`WAVE_FORMAT_EXTENSIBLE` files. Output format can be kept or overridden.

## Build

```sh
# The default build supports every input format plus WAV/FLAC output without LAME:
cargo build --release

# Optional MP3 output (MP3 input needs only the default Rust crates):
sudo apt-get install -y libmp3lame-dev
cargo build --release --features mp3-encoding

# Optional statically linked Ogg Opus input/output:
cargo build --release --features opus-encoding

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

Prebuilt binaries use the dependency-free default feature set and support all
input formats plus WAV/FLAC output. MP3 output requires a source build with
`--features mp3-encoding` and an installed LAME library.

### EBU conformance tests

Run the optional conformance suite against the official EBU Loudness Test Set
v5. The script downloads the archive from a public mirror, verifies its
SHA-256 checksum, and runs the Tech 3341 integrated-loudness and Tech 3342
Loudness Range cases:

```sh
./tools/test-ebu-conformance.sh
```

The EBU material is cached outside the repository and is used only for
technical testing under the terms included in its archive.

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

# Machine-readable EBU R128 delivery checks
forge --analyze programme.wav --compliance ebu-r128 --json

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
| `-m, --mode` | `lufs` | `lufs`, `peak`, or `rms` |
| `--preset` | none | Named playback/delivery loudness target (see below) |
| `--recursive` | off | Recursively process input directories |
| `--dry-run` | off | Analyze and show output paths without writing |
| `--overwrite` | off | Replace output files that already exist |
| `--target` | `-16` | Target LUFS (`--mode lufs`) |
| `--target-peak` | `-0.1` | Target sample peak dBFS (`--mode peak`) |
| `--target-rms` | `-18` | Target RMS dBFS (`--mode rms`) |
| `--ceiling` | `-1.0` | True-peak ceiling dBFS (gain is reduced to respect it) |
| `--max-gain` | none | Cap on applied gain (dB), a boost safety limit |
| `--format` | inferred | `wav`, `flac`, `mp3`, or `opus` output container |
| `--bitrate` | `192` | Lossy encoder bitrate in kbps (MP3/Opus output) |
| `--quality` | `2` | MP3 encoder quality 0(best)…9(fastest) |
| `--album` | off | One shared gain for all inputs (requires `--mode lufs`) |
| `--analyze` | off | Measure only; do not write files |
| `--json` | off | Write analyze results as JSON to stdout |
| `--csv` | none | Write analyze results as CSV to a file or `-` |
| `--compliance` | none | Evaluate analysis against `ebu-r128` delivery limits |
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
| `-j, --jobs` | all cores | Worker thread count |

> Negative values need `=`: `--target=-16` (clap parses `-16` as a flag otherwise).

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

## Architecture

```
src/
  lib.rs            public engine API (decoder, audio I/O, DSP, normalize)
  main.rs           CLI wrapper (format resolution + dispatch)
  cli.rs            clap definition
  decoder.rs        full-buffer and streaming universal decoders
  flacenc.rs        bounded-memory pure-Rust FLAC encoder
  mp3enc.rs         MP3 encoder via LAME FFI (interleaved f32 -> MP3 bytes)
  wav/
    format.rs       PcmKind / WaveFormat
    reader.rs       RIFF/WAVE demuxer
    writer.rs       RIFF/WAVE muxer
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

The live API is causal and reports zero processing latency for gain smoothing.
It deliberately does not label a changing live estimate as final Integrated
LUFS; programme-integrated normalization remains the two-pass file workflow.

## Limitations

* MP3 **encoding** requires the `mp3-encoding` feature and LAME
  (`libmp3lame`) at build/run time. MP3 **decoding** and all other input
  formats need only the Rust crates (symphonia).
* AAC/ALAC/Vorbis can be read but are written as WAV/FLAC (or MP3 with its
  optional feature); Forge does not encode those source containers directly.
* Ogg Opus currently supports mono and stereo streams. Multichannel mapping
  families and chained logical streams are not yet supported.
* By default, the true-peak ceiling is enforced transparently by reducing
  global gain. `--limiter` opts into dynamic look-ahead limiting when reaching
  the loudness target matters more than preserving dynamics unchanged.
* MP3 is lossy: re-encoding shifts loudness by ~0.2–0.4 LU per pass. For
  production work, normalize to WAV/FLAC and encode to MP3 once at the end.
  `--verify --verify-retries N` compensates codec drift by rendering every
  retry from the original input, avoiding generation loss between attempts.
