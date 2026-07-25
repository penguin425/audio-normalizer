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
| **FLAC** | symphonia (pure Rust) | — (output as WAV) |
| **AAC / ALAC** (.m4a/.mp4) | symphonia (pure Rust) | — (output as WAV) |
| **Vorbis** (.ogg) | symphonia (pure Rust) | — (output as WAV) |

* Decoding of MP3/FLAC/AAC/ALAC/Vorbis is done in pure Rust by
  [`symphonia`](https://github.com/pdelanoe/symphonia) — no system codecs.
* Optional MP3 encoding uses **LAME** (the reference MP3 encoder) through a
  tiny FFI. Enable it with the `mp3-encoding` Cargo feature.
* WAV stays on the fast hand-written path; other inputs transparently route
  through the universal decoder and produce the same planar-f32 buffer the DSP
  engine consumes.

By default the output container follows the input where Forge can encode it
(mp3 → mp3), and otherwise falls back to lossless WAV. `--format wav|mp3` and
the `-o` extension override this.

## Why it's fast

* **AVX2 + FMA SIMD** for the gain and energy-summation hot loops, with
  runtime feature detection and a portable scalar fallback (so the binary runs
  anywhere but flies on modern x86-64).
* **Multi-threaded** via rayon — channels and files are processed in parallel.
* **Prefix-sum block energies** make the 75%-overlapping LUFS gating blocks
  O(1) each instead of re-scanning 400 ms windows.
* **Single sequential I/O pass**: the whole file is read once, then decoded,
  measured, gained, and written.
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
* **True peak** is measured by 4× polyphase FIR oversampling (Kaiser-windowed
  lowpass, unity DC gain), so inter-sample peaks that exceed sample peaks are
  caught — and the gain is reduced so the output never clips after DAC
  reconstruction.
* **TPDF dither** is available for integer output to eliminate quantization
  distortion when reducing word length.

## Formats

Reads and writes PCM 8/16/24/32-bit and IEEE float 32/64-bit WAV, including
`WAVE_FORMAT_EXTENSIBLE` files. Output format can be kept or overridden.

## Build

```sh
# The default build supports every input format and WAV output without LAME:
cargo build --release

# Optional MP3 output (MP3 input needs only the default Rust crates):
sudo apt-get install -y libmp3lame-dev
cargo build --release --features mp3-encoding

cargo test
```

When `mp3-encoding` is enabled, `build.rs` finds `libmp3lame` via pkg-config,
then standard library paths, and prints a clear install hint if it is missing.

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

# EBU R128 broadcast target (-23 LUFS)
forge program.wav -o out.wav --target=-23

# Classic peak normalization to -1 dBFS
forge track.wav -o out.wav --mode=peak --target-peak=-1

# Album mode: one shared gain across all tracks (mixed formats allowed)
forge --album a.wav b.mp3 c.flac -o ./normalized/

# Just measure any file, don't write
forge --analyze song.mp3

# Print the gain that would be applied, write nothing
forge --gain-only track.wav --target=-14

# Re-encode 16-bit input as 24-bit WAV with dither
forge in.wav -o out.wav --bits=24 --dither
```

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `-m, --mode` | `lufs` | `lufs`, `peak`, or `rms` |
| `--target` | `-16` | Target LUFS (`--mode lufs`) |
| `--target-peak` | `-0.1` | Target sample peak dBFS (`--mode peak`) |
| `--target-rms` | `-18` | Target RMS dBFS (`--mode rms`) |
| `--ceiling` | `-1.0` | True-peak ceiling dBFS (gain is reduced to respect it) |
| `--max-gain` | none | Cap on applied gain (dB), a boost safety limit |
| `--format` | inferred | `wav` or `mp3` output container |
| `--bitrate` | `192` | MP3 CBR bitrate in kbps (MP3 output) |
| `--quality` | `2` | MP3 encoder quality 0(best)…9(fastest) |
| `--album` | off | One shared gain for all inputs (requires `--mode lufs`) |
| `--analyze` | off | Measure only; do not write files |
| `--gain-only` | off | Print the gain; write nothing |
| `--dither` | off | TPDF dither for integer PCM output |
| `--bits` | input's | `8`/`16`/`24`/`32`/`32f`/`64f` output format |
| `-j, --jobs` | all cores | Worker thread count |

> Negative values need `=`: `--target=-16` (clap parses `-16` as a flag otherwise).

## Architecture

```
src/
  lib.rs            public engine API (decoder, WAV I/O, DSP, MP3 enc, normalize)
  main.rs           CLI wrapper (format resolution + dispatch)
  cli.rs            clap definition
  decoder.rs        universal decoder: WAV fast path + symphonia (mp3/flac/aac/vorbis)
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
    truepeak.rs     4x polyphase FIR true-peak meter
  normalize.rs      analyze -> gain (ceiling-protected) -> apply -> write; album mode
build.rs            optionally links libmp3lame for MP3 encoding
tests/
  integration.rs    in-memory round-trip tests (WAV LUFS/peak/album/silence + MP3)
```

## Limitations

* MP3 **encoding** requires the `mp3-encoding` feature and LAME
  (`libmp3lame`) at build/run time. MP3 **decoding** and all other input
  formats need only the Rust crates (symphonia).
* Forge only *encodes* WAV and MP3. FLAC/AAC/ALAC/Vorbis can be read and
  normalized, but the output is WAV (or MP3 with `--format mp3`).
* Single-pass linear normalization (no dynamic/look-ahead limiter). The
  true-peak ceiling is enforced by reducing the global gain, which is the
  transparent, artifact-free approach used for loudness normalization.
* MP3 is lossy: re-encoding shifts loudness by ~0.2–0.4 LU per pass. For
  production work, normalize to WAV/FLAC and encode to MP3 once at the end.
