# Forge performance plan

Forge optimizes the exact offline-normalization workflow. Performance changes
must preserve ITU-R BS.1770-5 gating and true-peak behavior, EBU Tech 3341/3342
file-meter results, output duration, channel order, and deterministic evidence.
The official EBU v5 and ITU-R BS.2217-2 suites remain release gates.

## Engineering basis

- [Amdahl's law](https://doi.org/10.1145/1465482.1465560) makes repeated
  sequential decode, resample, and encode passes the first target: accelerating
  only one DSP kernel cannot remove time spent in the other passes.
- Blumofe and Leiserson's
  [work-stealing analysis](https://doi.org/10.1145/324133.324234) supports
  dynamically scheduling independent track work on one bounded worker pool.
  Forge still collects indexed results and commits them in caller order so
  scheduling does not change album semantics or error precedence.
- The [Roofline model](https://digicoll.lib.berkeley.edu/record/136692/files/EECS-2008-134.pdf)
  motivates removing buffer allocation, copies, and full-signal memory traffic
  before adding more arithmetic parallelism.
- Crochiere and Rabiner's
  [multirate-filter design](https://web.ece.ucsb.edu/Faculty/Rabiner/ece259/Reprints/087_optimum%20fir%20digital%20filters.pdf)
  and [interpolation/decimation tutorial](https://web.ece.ucsb.edu/Faculty/Rabiner/ece259/Reprints/179_interpolation_decimation.pdf),
  together with Vaidyanathan's
  [polyphase tutorial](https://authors.library.caltech.edu/records/x720m-mr760),
  support the existing polyphase resampler and favor reusable fixed-size
  buffers over rebuilding or copying intermediate sequences.
- [ITU-R BS.1770-5](https://www.itu.int/rec/R-REC-BS.1770-5-202311-I/en),
  [EBU R 128](https://tech.ebu.ch/publications/r128), and
  [EBU Tech 3341](https://tech.ebu.ch/files/live/sites/tech/files/shared/tech/tech3341v2_0.pdf)
  constrain what may be fused or parallelized. In particular, complete 400 ms
  gating blocks and the combined gated-block population cannot be approximated
  by short windows or per-track averages.

These sources guide the architecture; benchmark evidence decides whether an
individual implementation is retained.

## Measured releases

Measurements below are five-run medians unless a section notes otherwise, on
the same Ryzen 9 3950X WSL2 host with deterministic inputs. Fixture generation
is excluded. They are engineering comparisons, not cross-machine performance
guarantees.

### v0.128.0: sample-rate-aware true peak

The true-peak meter now uses a copy-free circular history, SIMD polyphase
interpolation, and the BS.1770 measurement domain appropriate to the input
sample rate. These cases use 1,200-second stereo inputs.

| Input rate | Before | v0.128.0 | Change | Speedup |
| --- | ---: | ---: | ---: | ---: |
| 48 kHz | 5.013 s | 4.135 s | -17.5% | 1.21x |
| 96 kHz | 9.743 s | 8.314 s | -14.7% | 1.17x |
| 192 kHz | 19.230 s | 8.390 s | -56.4% | 2.29x |

### v0.129.0: pass and buffer reuse

Plan-aware analysis now decodes and resamples once instead of first measuring
the source domain and decoding again for output-domain measurement. For a
single input, resampled PCM and decode-heavy FLAC/DSD PCM may be held in an
ephemeral file between exact analysis and rendering. Fast same-rate formats
re-decode because raw temporary I/O was not faster. Multi-track albums do not
retain one raw file per track; this keeps temporary storage and file handles
bounded while still eliminating the redundant analysis decode.

The FFT resampler also fills a bounded input block without `Vec::drain`, keeps
its output buffers, and supports streams whose final duration is known only at
end-of-stream. These cases use 600-second stereo inputs.

| Workload | v0.128.0 | v0.129.0 | Change | Speedup |
| --- | ---: | ---: | ---: | ---: |
| WAVE 48→44.1 kHz normalize | 5.575 s | 3.145 s | -43.6% | 1.77x |
| WAVE 44.1→48 kHz normalize | 5.390 s | 3.342 s | -38.0% | 1.61x |
| Same-rate FLAC normalize | 4.274 s | 3.850 s | -9.9% | 1.11x |
| Same-rate MP3 normalize | 3.908 s | 3.924 s | +0.4% | 1.00x |
| Same-rate WAVE normalize | 2.915 s | 2.883 s | -1.1% | 1.01x |

The neutral MP3 result is why same-rate lossy inputs keep the established
re-decode path. A buffered temporary-file experiment was also rejected because
it was slower than direct OS-page-cache I/O on the measurement host.

### v0.130.0: album track parallelism

Independent album analyses, renders, and corrected-output re-analyses now use
the same Rayon work-stealing pool as channel DSP. `--jobs` remains the single
in-process concurrency budget. Indexed results are resolved in input order,
and final destination commits remain serial and occur only after every staged
track has succeeded.

This case normalizes eight independent 300-second stereo PCM16 WAVE tracks
(2,400 seconds of programme material total). Both versions used the same
deterministic generator. The v0.129.0 baseline is a five-run median; the
v0.130.0 result uses a fifteen-run median because concurrent filesystem writes
showed more run-to-run variance.

| Metric | v0.129.0 | v0.130.0 | Change |
| --- | ---: | ---: | ---: |
| Wall time | 11.362 s | 1.908 s | -83.2% (5.96x) |
| Child CPU utilization | 164% | 814% | work distributed across tracks |
| Peak RSS | 16.0 MiB | 29.3 MiB | +13.3 MiB |

The additional live-buffer memory is bounded by active workers and remains
small in absolute terms. A separate `--jobs 1` versus `--jobs 8` check produced
byte-identical WAVE files for every track. Album mode continues to avoid
retaining a raw PCM spool per track, so parallelism does not scale temporary
PCM storage with album duration.

### v0.131.0: independent-file batch parallelism

Ordinary multi-file normalization now uses the same bounded work-stealing pool
without weakening per-asset transactions. Up to `min(--jobs, 32)` assets render
to sibling temporary files in one wave. Forge then atomically publishes
outputs, catalogue rows, resumable checkpoints, and progress completions in
input order. A failure discards all later unpublished stages in that wave.

This case normalizes eight independent 300-second stereo PCM16 WAVE tracks
(2,400 seconds total) with `--jobs 8`. The v0.130.0 baseline is a five-run
median; the v0.131.0 result is a fifteen-run median. Both use the same optimized
build settings and deterministic fixture generator.

| Metric | v0.130.0 | v0.131.0 | Change |
| --- | ---: | ---: | ---: |
| Wall time | 11.620 s | 2.375 s | -79.6% (4.89x) |
| Child CPU utilization | 151% | 463% | work distributed across files |
| Peak RSS | 15.8 MiB | 25.5 MiB | +9.8 MiB |

`--jobs 1` and `--jobs 4` produced byte-identical WAVE outputs in the
integration test. The 32-asset cap bounds simultaneous file handles, encoder
state, memory, and temporary-output count; users can select a smaller
`--jobs` value when storage throughput or temporary capacity is the limit.
Post-encode verification, difference reports, and analysis-cache runs retain
their established serial paths for now.

### v0.132.0: render and writer hot-path fusion

The ordinary non-limiter, non-statistics render path now applies gain and the
hard ceiling in one channel-contiguous AVX2 pass. Default 16-bit mono/stereo
WAVE output uses a byte-exact AVX2 quantize/interleave kernel, and the streaming
writer reuses one encoded-byte buffer instead of allocating it for every
decoded chunk. Runtime detection retains scalar fallbacks on x86-64 hosts
without AVX2; other PCM kinds and dithered output keep the established scalar
quantizer.

A first experiment moved gain and ceiling arithmetic into the frame-major
scalar quantizer. It was rejected after a fifteen-run 300-second benchmark was
2.1% slower: removing memory passes did not offset losing the channel-contiguous
SIMD kernel. The retained design follows the Roofline motivation without
trading away vector throughput.

The release comparison uses one 1,200-second stereo PCM16 WAVE input and
seven-run medians from identically optimized builds. Fixture generation is
excluded.

| Metric | v0.131.0 | v0.132.0 | Change |
| --- | ---: | ---: | ---: |
| Wall time | 5.746 s | 5.186 s | -9.7% (1.11x) |
| User CPU time | 6.063 s | 5.407 s | -10.8% |
| Child CPU utilization | 165% | 172% | more work retained in vector kernels |
| Peak RSS | 15.8 MiB | 15.8 MiB | unchanged |

Normal and dithered end-to-end WAVE outputs were byte-identical to v0.131.0.
Unit tests also compare the combined gain/ceiling kernel bit-for-bit and the
S16 SIMD quantizer byte-for-byte against their scalar predecessors across
NaNs, infinities, signed zero, half-LSB boundaries, random bit patterns, mono,
stereo, and non-vector-length tails. Limiter and render-statistics paths retain
their established sequencing.

### v0.133.0: native lossless verification tee

`--verify` now measures the exact quantized PCM accepted by Forge's native
WAVE and FLAC writers during the encode pass. WAVE exposes the byte buffer it
actually wrote; FLAC exposes the signed integer samples handed to its verified
encoder. A reusable planar scratch buffer feeds the unchanged streaming
BS.1770 analyzer, eliminating the completed-file probe/read/decode pass while
preserving quantization, TPDF dither, channel roles, limiter output, and retry
semantics. Codec-dependent MP3, AAC, ALAC, Opus, and Vorbis outputs still
re-decode the completed file. Multi-delivery also retains its final re-decode
after metadata mutation before committing outputs.

A lower-memory experiment decoded interleaved PCM directly inside the
BS.1770 sample loop. It was rejected: removing the planar scratch also removed
parallel channel decode, increasing the 1,200-second WAVE verification median
by 4.4% and user CPU by 3.0%. The retained design follows the measured
Roofline tradeoff instead of assuming fewer buffers are automatically faster.

These comparisons use 1,200-second stereo PCM16 WAVE sources and seven-run
medians from identically optimized builds. Fixture generation is excluded.

| Workload / metric | v0.132.0 | v0.133.0 | Change |
| --- | ---: | ---: | ---: |
| WAVE verify wall time | 12.893 s | 12.751 s | -1.1% |
| WAVE verify user CPU | 13.970 s | 13.891 s | -0.6% |
| WAVE verify system CPU | 5.141 s | 4.975 s | -3.2% |
| WAVE -> FLAC verify wall time | 16.913 s | 16.548 s | -2.2% |
| WAVE -> FLAC verify user CPU | 18.622 s | 18.173 s | -2.4% |
| Peak RSS (both workloads) | 16.0 MiB | 16.0 MiB | unchanged |

Tests compare tee and completed-file decode measurements bit-for-bit for every
WAVE PCM kind, dithered and undithered PCM16, FLAC 16/24-bit with dither, and
stereo/5.1/7.1 channel layouts. The benchmark harness adds dedicated WAVE and
FLAC verification cases so this pass-removal remains a release gate.

## Next implementation order

1. Reuse content-addressed analyses across identical input/plan requests.
2. Train and compare profile-guided builds, then publish architecture-specific
   binaries only where reproducible gains justify the distribution cost.
3. Run a GPU proof of concept only after CPU pass/memory optimizations; retain
   it only if transfer and launch overhead improve realistic long-form and
   multichannel workloads.

Every phase adds a benchmark case or compatible baseline gate before release.
