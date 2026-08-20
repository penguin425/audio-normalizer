# Forge performance benchmarks

`tools/benchmark.py` measures Forge with generated, deterministic inputs and
writes a versioned JSON report. It covers the roadmap's long-duration stereo,
multichannel, lossless, lossy, and pathological-input workloads.

The harness is an engineering regression tool, not a performance guarantee.
Compare results only on a stable, otherwise idle host with the same operating
system, architecture, CPU model, CPU count, duration, sample rate, and case
set.

For optimization work, pass `--iterations 5` or more. Fixtures are generated
once, then every command is run repeatedly against the same input. The report
retains every sample, uses the median for timing and CPU summaries, and uses
the maximum observed RSS so a fast outlier cannot hide a memory regression.

## Workloads

| Case | Input and operation |
| --- | --- |
| `wav-stereo-analyze` | 48 kHz stereo PCM16 WAVE, loudness and true-peak analysis |
| `wav-stereo-normalize` | 48 kHz stereo PCM16 WAVE, two-pass WAVE normalization |
| `wav-stereo-resample-normalize` | Stereo PCM16 WAVE, normalization to the alternate 44.1/48 kHz rate with output-domain PCM reuse |
| `wav-stereo-batch-normalize` | Eight independent stereo PCM16 WAVE tracks normalized with one bounded eight-worker batch wave |
| `wav-stereo-album-normalize` | Eight stereo PCM16 WAVE tracks, one shared album gain with parallel track analysis and rendering |
| `wav-7.1-normalize` | 48 kHz 8-channel PCM16 WAVE, explicit `7.1` layout normalization |
| `flac-stereo-analyze` | FFmpeg-generated lossless FLAC, JSON analysis |
| `flac-stereo-normalize` | FFmpeg-generated lossless FLAC, decoded-PCM reuse and WAVE normalization |
| `mp3-stereo-analyze` | FFmpeg-generated 320 kbit/s MP3, JSON analysis |
| `mp3-stereo-normalize` | FFmpeg-generated 320 kbit/s MP3, same-rate re-decode control and WAVE normalization |
| `pathological-wave-qc` | 100,001 empty WAVE chunks, bounded container-QC rejection |

Fixture generation and codec encoding happen before the measured command.
Fixtures are streamed to disk rather than assembled in memory. Each case is
removed before the next one unless `--keep-fixtures` is specified. A supplied
`--work-dir` is only used as the parent of a new, uniquely named run directory;
the harness never reuses or removes an existing case directory.

## Run

Build optimized binaries, then run the complete one-hour-per-track suite:

```sh
cargo build --locked --release --bin forge --bin forge-container-qc
python3 tools/benchmark.py \
  --forge target/release/forge \
  --iterations 5 \
  --output benchmark.json
```

FFmpeg is required for the FLAC and MP3 cases. Use `--ffmpeg PATH` if it is not
on `PATH`. A short smoke run, suitable for validating the harness, is:

```sh
python3 tools/benchmark.py \
  --forge target/release/forge \
  --duration-seconds 2 \
  --output benchmark-smoke.json
```

Repeat `--case NAME` to select cases. The maximum duration is 3,600 seconds,
the sample-rate range is 8–192 kHz, and each measured process has a default
two-hour timeout. The harness checks disk capacity before large WAVE fixtures
and retains a 1 GiB reserve. A full 7.1 normalization needs roughly 5.6 GB at
48 kHz because its input and output coexist; each eight-track stereo batch or
album case needs roughly 11.1 GB under the one-hour default.

## Report and regression checks

Reports use
`schema/performance-benchmark-v1.schema.json`. Times are monotonic wall time
and child-process CPU time. Peak RSS is normalized to bytes on supported POSIX
hosts; CPU and RSS fields are `null` where the operating system cannot provide
per-child usage. Commands contain placeholders rather than local paths.

Use a prior report as a baseline:

```sh
python3 tools/benchmark.py \
  --forge target/release/forge \
  --output candidate.json \
  --baseline baseline.json \
  --max-wall-regression-percent 10 \
  --max-rss-regression-percent 10
```

Baseline comparison is refused when host identity or workload configuration
differs. The report is written even on a workload, execution, or regression
failure; setup/execution details appear in its nullable `error` field.
Exit status is 0 for a passing run, 2 for a setup/execution error, and 3 for a
completed run with an unexpected process result or performance regression.

The pathological case intentionally expects `forge-container-qc` status 1:
the bounded audit must reject the excessive chunk population without crashing,
hanging, or consuming input-proportional memory.
