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
- A 2026
  [zero-copy streaming pipeline study](https://doi.org/10.1016/j.ins.2026.123171)
  identifies redundant copies, contention, and unbounded storage as separate
  costs, and combines ownership-like frame reuse with a bounded processing
  pipeline. Forge applies the architectural principle to two planar PCM slots;
  its codec-specific result is established by the measurements below rather
  than inferred from that broader edge-streaming workload.
- Research on
  [ordered shared-memory stream processing](https://arxiv.org/abs/1803.11328)
  distinguishes pipeline, task, and data parallelism, while measured
  [batch-pipelining](https://doi.org/10.1016/j.jvcir.2012.03.009) shows that
  synchronization overhead can erase gains in an already optimized codec.
  Forge therefore activates its writer overlap only outside existing
  file-level Rayon work and keeps a one-worker bypass.
- [FLAC 1.5.0](https://xiph.org/flac/changelog.html) made independent-frame
  encoding multithreaded, and its
  [stream encoder API](https://www.xiph.org/flac/api/group__flac__stream__encoder.html)
  documents distributing filled blocks to workers while preserving ordered
  output callbacks. Forge applies that frame-level model to its pure-Rust
  writer through the existing bounded work-stealing pool instead of creating a
  second codec-owned thread pool.
- A 2026 study of
  [parallel cascaded recursive filtering](https://arxiv.org/abs/2607.14054)
  shows that block state transforms can expose parallelism across samples in
  cascaded IIR filters. Its large batched kernels and floating-point reduction
  order are not directly interchangeable with Forge's exact streaming f64
  metering, so Forge first specializes the common channel layout without
  changing arithmetic order. A time-parallel implementation remains a
  separately measured experiment.
- Its multicore/GPU follow-up,
  [Parallel Cascaded Recursive Filtering on Multi-Core CPUs and GPUs](https://arxiv.org/abs/2607.23763),
  reports 3.95x scaling on six performance cores and 38.2 GS/s on an RTX 3060
  for high-order batched filters. The authors'
  [reference implementation](https://github.com/Haotian-RA/matrix_form_recursive_filtering/tree/2026_07_15_arxiv)
  uses AVX2/FMA vector batches and fast floating-point transformations. Those
  results motivate a feasibility bound, not a direct speed forecast for two
  exact biquads with an intervening f32 rounding point.
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
- Rust's
  [profile-guided optimization procedure](https://doc.rust-lang.org/nightly/rustc/profile-guided-optimization.html)
  and LLVM's
  [instrumentation profile format](https://www.llvm.org/docs/InstrProfileFormat.html)
  define the instrument, train, merge, and profile-use pipeline. Forge uses
  the exact toolchain-matched `llvm-profdata`, identical code-generation flags
  in both builds, and an explicit target so build scripts do not pollute the
  profile. LLVM maintainers also note that
  [value-profile updates are order-dependent](https://discourse.llvm.org/t/pgo-profile-reproducibility/82861/2),
  even when ordinary counters use atomic updates; Forge therefore relies on
  deterministic branch counters rather than indirect-call or memory-size
  value profiles. Pettis and Hansen's
  [profile-guided code-positioning work](https://pages.cs.wisc.edu/~fischer/cs701.f05/code.positioning.pdf)
  provides the underlying hot-path layout motivation; its historical gains
  are not treated as a Forge performance prediction.
- The x86-64 psABI
  [micro-architecture levels](https://gitlab.com/x86-psABIs/x86-64-ABI/-/raw/master/x86-64-ABI/low-level-sys-info.tex)
  define the optional x86-64-v3 build's CPU contract. The generic release
  remains the fallback rather than executing a v3 binary on an unsupported
  processor.
- GPU work must include movement and orchestration, not just kernel timing.
  Gregg and Hazelwood's
  [data-movement study](https://web.stanford.edu/~cgregg/chris-gregg/pubs/WhereIsTheData.pdf),
  the NIME paper
  [There and Back Again](https://www.nime.org/proceedings/2020/nime2020_paper39.pdf),
  and the DAFx
  [General-Purpose GPU Audio Benchmark Framework](https://www.dafx.de/paper-archive/2024/papers/DAFx24_paper_56.pdf)
  all make transfer, launch, buffer size, and CPU/GPU data residency part of a
  meaningful audio benchmark. Forge therefore will not retain a GPU path based
  on kernel-only throughput.

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
At this release, post-encode verification, difference reports, and
analysis-cache runs retained their established serial paths. v0.134.0 removes
the analysis-cache exception while preserving the other safety boundaries.

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

### v0.134.0: parallel content-addressed cache integration

The content-addressed analysis cache predates the file-level parallel paths,
so enabling `--analysis-cache` had forced multi-input normalization back to a
serial loop. Cache hit validation and miss computation now run in the same
bounded Rayon pool selected by `--jobs`. Album rendering consumes the ordered
precomputed analyses, while independent batches stage their outputs in a
second parallel wave. Cache observations, errors, transactional commits,
catalogue rows, checkpoints, and progress events remain input ordered. Entry
commit plus capacity pruning is serialized within one process; hashing and
BS.1770 analysis remain parallel.

These cases use eight independent 1,200-second stereo PCM16 WAVE tracks (9,600
seconds total) and seven-run medians from identically optimized builds. Cache
warming is outside the measured interval for hit cases; miss cases remove the
cache before every measured invocation. Every invocation writes the complete
1,843,200,352-byte output set.

| Workload | v0.133.0 | v0.134.0 | Change | Speedup |
| --- | ---: | ---: | ---: | ---: |
| Independent batch, cache hit | 17.304 s | 4.261 s | -75.4% | 4.06x |
| Independent batch, cache miss | 22.072 s | 7.197 s | -67.4% | 3.07x |
| Album, cache hit | 8.247 s | 5.334 s | -35.3% | 1.55x |
| Album, cache miss | 13.900 s | 8.220 s | -40.9% | 1.69x |

Warm independent batches also reduce user CPU by 46.5% and system CPU by
80.1%, with peak RSS unchanged at 16.25 MiB. Cold-cache parallelism trades more
CPU and live analyzer state for latency: independent-batch user CPU rises
187%, while wall time falls 67.4% and peak RSS remains only 35.6 MiB. The album
miss case peaks at 37.8 MiB. `--jobs 1` retains the low-resource serial mode.
Tests exercise concurrent entry publication and bounded eviction, and compare
serial/parallel cache-hit outputs byte-for-byte while checking deterministic
cache messages and progress ordering.

### v0.135.0: deterministic PGO and an optional x86-64-v3 CLI

The Linux normalizer CLI now uses an LLVM instrumentation-PGO build. A bounded,
deterministic 12-second training corpus exercises 14 serial paths: WAVE and
FLAC analysis/normalization, native verification, resampling, dither, limiting,
7.1 audio, batch and album work, and analysis-cache hit/miss behavior. Every
training invocation uses `--jobs 1`; the profile directory must be empty, and
the profile generator and consumer use identical target/features/Rust flags.

Raw profiles from two initial same-host training runs differed only in three
cold mutex/registry functions. The v0.135.0 canonicalizer zeros an entire
function only when its maximum counter is below 10,000; hot counters and value
profiles were left unchanged. Those two canonical profiles and their indexed
LLVM profiles were byte-identical. A later cross-runner v3 release rebuild
showed that preserving value profiles was not sufficient; v0.136.0 hardens
that input as described below. Release CI independently repeats the complete
build and requires byte-identical generic and v3 archives before publication.

These measurements use 600-second deterministic inputs and seven-run medians.
The generic PGO column compares a generic x86-64 build with and without PGO.
The v3+PGO column compares a contemporary generic non-PGO build with the
optional `-Ctarget-cpu=x86-64-v3` PGO build on the same Ryzen 9 3950X host.

| Workload | Generic PGO wall change | v3+PGO wall change |
| --- | ---: | ---: |
| WAVE stereo analyze | -3.92% | -18.64% |
| WAVE stereo normalize | -5.54% | -14.92% |
| WAVE stereo verify | -6.30% | -19.69% |
| WAVE to FLAC verify | -3.50% | -46.75% |
| WAVE resample + normalize | -3.19% | -11.89% |
| WAVE 7.1 normalize | -4.84% | -11.67% |
| FLAC stereo analyze | -6.27% | -14.57% |
| FLAC stereo normalize | -5.37% | -11.20% |
| MP3 stereo analyze | -2.58% | -18.85% |
| MP3 stereo normalize | -4.89% | -11.88% |
| **Ten-case aggregate** | **-4.61%** | **-21.55%** |

Generic PGO reduced aggregate user CPU by 3.77% and the CLI size by 11.46%.
The v3+PGO build reduced aggregate user CPU by 22.02% versus generic and,
compared with v3 without PGO, reduced wall time by another 4.94%, user CPU by
6.48%, and binary size by 12.07%. All 17 benchmark cases passed, and SHA-256
comparison found all 55 generated audio files byte-identical to the generic
build.

Parallel wall time remains scheduler/storage sensitive: across six 600-second
batch/album cases with 15-run medians, generic PGO reduced aggregate user CPU
by 3.95% but wall time by only 0.72%. Twenty alternating album pairs produced a
median paired wall change of -0.017%, with large individual variation. The
release therefore claims the measured serial/CPU reduction, not a stable
parallel latency gain.

Only the Linux `forge` CLI receives PGO in this release. The full Linux archive
uses the generic CPU baseline; the supplemental `linux-x86_64-v3` archive
contains only the v3 CLI. Other Forge executables, shared libraries, wheels,
macOS, and Windows remain portable non-PGO builds until separately measured.
Developers can reproduce the generic pipeline with the toolchain-matched LLVM
component:

```sh
rustup component add llvm-tools-preview
FORGE_PGO_ROOT="$PWD/target/forge-pgo-generic" \
FORGE_PGO_RUSTFLAGS='-Ctarget-cpu=x86-64' \
tools/build-pgo-forge.sh x86_64-unknown-linux-gnu
```

### v0.136.0: counter-only PGO and stereo analyzer specialization

LLVM instrumentation value profiles record indirect-call targets and memory
operation sizes in addition to ordinary branch counters. Their bounded value
tables depend on runtime insertion order, so two runs can have identical
counter summaries yet produce different optimized layouts. Forge now passes
LLVM's `-disable-vp` option in both PGO phases, and the text-profile
canonicalizer removes any residual value records defensively. Hot branch
counters, the 10,000-count cold-function rule, training inputs, and the
independent release rebuild remain unchanged.

On the same 600-second stereo WAVE input, 20 alternating x86-64-v3 analysis
pairs measured 1.768 seconds with the original full profile and 1.776 seconds
with the counter-only profile (+0.45%). Fifteen normalization pairs measured
2.248 seconds for both profiles (counter-only was 0.03% lower before
rounding). These sub-percent differences are treated as noise rather than a
speed claim. Analysis JSON and normalized WAVE output were byte-identical,
while the counter-only CLI was 384 bytes smaller.

The dominant two-channel, non-timeline analysis path now borrows both
K-weighting filters and true-peak meters once outside the frame loop. This
removes the per-frame dynamic channel iterator and exposes the two independent
filter states to LLVM while retaining the exact operation and window-update
order. Mono, multichannel, and timeline analysis continue through the generic
path. No PCM or energy scratch buffer is added.

These measurements use the same deterministic 600-second stereo PCM16 WAVE
input. Both binaries were built from the v0.135.0 source with Rust 1.97.0,
`-Ctarget-cpu=x86-64`, fat LTO, and no PGO so the source change is isolated.
Analysis uses 20 alternating baseline/candidate pairs; normalization uses 15
alternating pairs. The table reports medians.

| Workload | v0.135.0 | v0.136.0 | Change |
| --- | ---: | ---: | ---: |
| WAVE stereo analyze, wall | 2.495 s | 2.385 s | -4.43% |
| WAVE stereo analyze, user CPU | 2.573 s | 2.503 s | -2.72% |
| WAVE stereo normalize, wall | 3.077 s | 2.980 s | -3.16% |
| WAVE stereo normalize, user CPU | 2.946 s | 2.858 s | -3.00% |

The analysis JSON and normalized WAVE output were SHA-256 identical between
the binaries. A regression test also compares chunked stereo analysis against
the whole-buffer implementation for integrated, momentary, and short-term
loudness, LRA, RMS, sample peak, and true peak.

A channel-contiguous scratch-buffer prototype was rejected: although it made
each recursive filter's input contiguous, copying and the extra pass increased
analysis wall time from 2.520 seconds to 2.618 seconds (+3.9%). This agrees with
the Roofline/data-movement basis: compiler visibility without added memory
traffic was the useful change.

### v0.137.0: adaptive native-WAVE streaming chunks

Native WAVE analysis and normalization had used one 64 KiB read/decode chunk
for every layout. On long stereo and multichannel inputs, each chunk enters a
bounded Rayon channel-decode region; the small fixed size therefore caused
thousands of avoidable scheduling transitions and I/O calls. The stream now
reuses one planar decode allocation and selects a frame-aligned 1 MiB chunk for
two or more channels. Mono retains 64 KiB because increasing it to 1 MiB was
about 1% slower and provided no parallel channel work to amortize. Both sizes
remain bounded and independent of file duration.

A chunk-size sweep first measured 256 KiB 9.38% faster than 64 KiB, then 1 MiB
4.08% faster than 256 KiB. Raising the size again to 4 MiB was 0.92% slower
than 1 MiB and increased peak RSS from about 13 MiB to 28 MiB, so 1 MiB is the
retained knee rather than an arbitrary maximum. These comparisons use the same
source, optimized build settings, and alternating runs.

The long-form cases below compare v0.136.0 with the retained implementation.
Outputs and analysis reports were compared exactly; fixture generation is
outside the measured interval.

| Workload | v0.136.0 | v0.137.0 | Change |
| --- | ---: | ---: | ---: |
| 600 s stereo WAVE normalize | 2.974 s | 2.381 s | -20.0% |
| 30 s 7.1 WAVE normalize | 1.009 s | 0.589 s | -41.7% |
| Eight 30 s stereo files, batch, `--jobs 8` | 0.281 s | 0.186 s | -33.9% |
| Eight 30 s stereo files, album, `--jobs 8` | 0.278 s | 0.188 s | -32.4% |

In the 600-second stereo case, user CPU fell 21.0%, system CPU fell 81.6%, and
voluntary context switches fell from 29,728 to 2,247. A one-second stereo case
was 0.83% slower, which is below the retained benchmark's noise floor. The
eight-file cases increased peak RSS from 25,344 KiB to 48,640 KiB: a bounded
22.7 MiB cost from larger buffers being live on several workers. Users can
continue to lower `--jobs` when memory is the binding resource.

U8, signed 16/24/32-bit, and float 32/64-bit WAVE fixtures produced identical
analysis and normalized output with the old and new chunk policies. Tests also
cover mono, stereo, and 7.1 frame alignment for every PCM representation.

A separate true-peak experiment cached a reference to the static polyphase
coefficient table inside each meter. Twenty CPU-pinned alternating pairs showed
a -0.05% wall median, a +0.15% paired median, and +0.74% user CPU, with exact
analysis output. It was rejected as no measurable improvement; a future stereo
kernel must remove shared coefficient loads or instructions rather than merely
relocate the lookup.

A fixed f64 energy-ring experiment replaced both streaming `VecDeque` windows
and their per-sample hop moduli with one three-second circular buffer and two
countdowns. It preserved the exact add/subtract and block-emission order, and
the 600-second analysis JSON was byte-identical. Twenty CPU-pinned alternating
pairs measured 1.482 seconds for the established path and 1.477 seconds for the
candidate (-0.28%), but the median paired change was only -0.06% and candidate
user CPU was 0.24% higher. The structural rewrite was rejected as benchmark
noise rather than retained on the strength of a sub-percent unpaired median.

### v0.138.0: paired stereo true-peak processing

The stereo-specialized analyzer had kept both true-peak meters borrowed across
the frame loop but still called them separately. Each 2×/4× interpolation
therefore loaded the same immutable 16-row phase table twice, and the compiler
could not keep both histories and factor branches visible in one unit. The
stereo path now advances both independent meters together. AVX2/FMA, AArch64
NEON, and scalar implementations load each coefficient row once, then update a
separate accumulator for each channel in the established 16-tap order. Sample
history, frame maximum, and accumulated meter peak remain independent.

These CPU-pinned comparisons use Rust 1.97.0, native host tuning, fat LTO, no
PGO, and deterministic PCM16 WAVE inputs containing the same 28.8 million
stereo frames: 600 seconds at 48 kHz, 300 seconds at 96 kHz, and 150 seconds at
192 kHz. The 48 kHz analysis uses a rotating three-way order across 20 groups;
normalization and the other rates use 15 alternating pairs. Fixture generation
is excluded.

| Workload | v0.137.0 | v0.138.0 | Change |
| --- | ---: | ---: | ---: |
| 48 kHz stereo analyze, wall | 1.477 s | 1.262 s | -14.60% |
| 48 kHz stereo analyze, user CPU | 1.439 s | 1.214 s | -15.69% |
| 48 kHz stereo normalize, wall | 1.745 s | 1.518 s | -13.01% |
| 48 kHz stereo normalize, user CPU | 1.509 s | 1.284 s | -14.94% |
| 96 kHz stereo analyze, wall | 1.526 s | 1.306 s | -14.42% |
| 96 kHz stereo analyze, user CPU | 1.441 s | 1.230 s | -14.63% |
| 192 kHz stereo analyze, wall | 0.686 s | 0.612 s | -10.73% |
| 192 kHz stereo analyze, user CPU | 0.525 s | 0.452 s | -13.86% |

The median paired wall changes were -14.32% for 48 kHz analysis, -13.20% for
48 kHz normalization, -14.21% at 96 kHz, and -10.73% at 192 kHz. Every paired
48 kHz analysis and normalization comparison improved. The 192 kHz path has no
FIR interpolation; its result shows the additional benefit of resolving both
meter control paths together rather than attributing the whole gain to
coefficient traffic.

Analysis JSON was byte-identical at 48, 96, and 192 kHz, and the normalized
48 kHz WAVE output was byte-identical. A regression test compares every
per-frame return and final meter peak bit-for-bit against two independent
meters at all three rates, exercising 4×, 2×, and direct-sample paths.

A one-line `#[inline]` experiment on the original single-meter method was also
measured in the same 20-group three-way run. Its wall median changed by -0.38%,
its paired median by +0.02%, and user CPU by -0.19%. It was rejected as noise;
the retained improvement comes from the paired state/dataflow, not an inlining
hint alone.

#### Block-parallel K-weighting feasibility screen

The published block-matrix IIR result is strongest for high-order cascades and
large batches. Its reference implementation uses AVX2 `Vec8f`, compiles with
`-ffast-math`, and reports its headline result for a 16th-order cascade on a
Meteor Lake core. Forge's K-weighting workload is only two biquads, retains f64
state, rounds the first stage to f32 before the second, and must carry exact
state across bounded decoder chunks. The per-stage rounding makes the existing
mapping nonlinear with respect to the state, so a superposition or prefix-scan
rewrite cannot be bit-identical to the established sample recurrence.

Before investing in a different-arithmetic matrix kernel, Forge measured its
absolute end-to-end opportunity. An intentionally invalid identity-filter
build removed both K-weighting biquads completely. Across 20 CPU-pinned,
alternating 600-second stereo analysis pairs, the established path had 1.255 s
wall and 1.210 s user-CPU medians; the no-K-weighting upper bound had 1.110 s
and 1.060 s medians, reductions of 11.55% and 12.40%. No implementable filter
optimization can exceed that wall-time bound on this workload.

An exact two-lane prototype then updated identically configured left/right
biquads together without changing either lane's operation order. It matched
independent filters bit-for-bit at 8, 44.1, 48, 96, and 192 kHz, and the full
analysis JSON was byte-identical. Nevertheless, its 20-pair wall median rose
from 1.250 s to 1.280 s (+2.40%), while user CPU rose from 1.215 s to 1.250 s
(+2.88%; paired median +2.46%). LLVM already exposes this small scalar cascade
efficiently in the stereo analyzer; the extra shape and coefficient checks
made it worse.

The paired K-weighting prototype was removed. A block-matrix implementation is
deferred unless Forge gains a higher-order recursive workload or an explicitly
non-bit-exact analysis mode. The current exact, low-order K-weighting path does
not justify altered rounding, block permutation, correction passes, and task
coordination for an end-to-end opportunity capped at 11.55% before overhead.

### v0.139.0: channel-contiguous multichannel true peak

The generic multichannel analyzer had interleaved one true-peak meter call and
one K-weighting call for every channel of every frame. With 5.1, 6.1, 7.1, and
immersive layouts this repeatedly displaced several recursive histories and
loaded the same immutable interpolation coefficients for each channel. The
multichannel path now walks each adjacent channel pair contiguously for true
peak, using the paired SIMD/scalar kernel introduced in v0.138.0, then performs
K-weighting, energy accumulation, and gating in the original frame/channel
order. Odd layouts retain one independent tail meter. Timeline analysis keeps
the established generic path because it needs per-interval reconstructed
peaks.

These comparisons use a deterministic 300-second, eight-channel PCM16 WAVE
input at 48 kHz and native host tuning with fat LTO and no PGO. The analysis
case uses 15 alternating pairs; normalization uses ten pairs and writes the
complete 230,400,068-byte output every time. Fixture generation is excluded.

| Workload / metric | v0.138.0 | v0.139.0 | Change |
| --- | ---: | ---: | ---: |
| 7.1 analyze, wall | 3.490 s | 2.550 s | -26.93% |
| 7.1 analyze, user CPU | 4.190 s | 3.220 s | -23.15% |
| 7.1 normalize, wall | 4.900 s | 3.920 s | -20.00% |
| 7.1 normalize, user CPU | 5.995 s | 5.365 s | -10.51% |

The paired wall medians improved by 27.30% for analysis and 19.23% for
normalization. Every analysis user-CPU pair improved by at least 18.3%, and
every normalization user-CPU pair improved by at least 8.3%. Peak RSS was
unchanged at about 13 MiB for analysis and 11 MiB for normalization. Analysis
JSON and normalized WAVE output were SHA-256 identical to v0.138.0. A
regression test compares chunked seven- and eight-channel streaming
measurements with the whole-buffer implementation across integrated,
momentary, and short-term loudness, LRA, RMS, sample peak, and true peak.

The same channel-contiguous split was tested on the fused stereo path. It was
also byte-identical, but 20 CPU-pinned alternating analysis pairs changed the
wall median from 1.280 s to 1.300 s (+1.56%) and user CPU from 1.240 s to
1.270 s (+2.42%; paired median +2.43%). Two channels do not create enough state
pressure to repay the extra PCM pass, so the experiment was removed and the
v0.138.0 fused stereo loop remains in place.

### v0.140.0: bounded multichannel true-peak parallelism

The channel-contiguous passes introduced in v0.139.0 make adjacent true-peak
meter pairs independent within a decoder chunk. For chunks of at least 16,384
frames and four or more channels, Forge now schedules those pairs on the
existing Rayon pool bounded by `--jobs`. It does not create a nested pool or
additional signal buffers. Short codec packets and `--jobs 1` retain the
sequential path, while K-weighting, energy accumulation, gating, and every
cross-channel reduction keep their established order.

These comparisons reuse the deterministic 300-second, eight-channel PCM16 WAVE
fixture from v0.139.0, native host tuning, fat LTO, and no PGO. Analysis uses 15
alternating pairs; normalization uses ten pairs and writes the complete
230,400,068-byte output every time. The baseline is v0.139.0, and fixture
generation is excluded.

| Workload / metric | v0.139.0 | v0.140.0 | Change |
| --- | ---: | ---: | ---: |
| 7.1 analyze, default jobs, wall | 2.540 s | 1.590 s | -37.40% |
| 7.1 analyze, default jobs, user CPU | 3.210 s | 4.150 s | +29.28% |
| 7.1 analyze, `--jobs 4`, wall | 2.720 s | 1.730 s | -36.40% |
| 7.1 analyze, `--jobs 4`, user CPU | 2.960 s | 3.330 s | +12.50% |
| 7.1 normalize, default jobs, wall | 3.950 s | 2.990 s | -24.30% |
| 7.1 normalize, default jobs, user CPU | 5.005 s | 6.160 s | +23.08% |

The paired wall medians improved by 37.45% for default-job analysis, 36.53%
with four jobs, and 24.07% for normalization. Peak RSS was effectively
unchanged: about 13 MiB for analysis and 11 MiB for normalization. Analysis
JSON and normalized WAVE output were SHA-256 identical to v0.139.0. A
regression test compares both a 137-frame sequential stream and a 20,000-frame
parallel stream for seven- and eight-channel layouts against the whole-buffer
implementation across all reported measurements.

This optimization exchanges aggregate CPU time for lower elapsed time. With
`--jobs 1`, 15 analysis pairs showed a -0.79% paired wall change and no paired
user-CPU change, both within benchmark noise. Users prioritizing battery,
thermal headroom, or concurrent workloads can therefore select one job; the
default prioritizes completion latency. When several independent files are
available, Rayon work stealing still shares one bounded pool rather than
oversubscribing it with per-file thread pools.

#### v0.140.0 GPU true-peak feasibility baseline (not shipped)

A transfer-inclusive CUDA proof of concept mapped one 48 kHz true-peak output
sample per thread, evaluated the same four 16-tap polyphase rows with ordered
f64 fused multiply-adds, reduced block maxima on-device, and returned one
peak. It was measured on an NVIDIA RTX 2080 Ti under WSL2. Runtime compilation,
fixture construction, and CUDA-context startup were excluded; ordinary
pageable host memory was used with CuPy 13.6.0, CUDA runtime 12.9, and driver
591.86. The timing fixture is constant f32 PCM, for which the kernel's memory
and instruction counts are signal-independent. The PoC transfers one contiguous
planar allocation; Forge's streaming decoder owns one `Vec` per channel, so a
production implementation must separately measure multiple DMA operations or
registered staging rather than assuming this transfer result. The bounded form
retained 15 samples of history per channel and used Forge's 65,536-frame 7.1
decoder chunk size for both layouts as a conservative common packet size.

| Input / GPU stage | Whole buffer | Bounded chunks |
| --- | ---: | ---: |
| 600 s stereo, host-to-device | 20.38 ms | 20.61 ms |
| 600 s stereo, kernel/reduction | 25.99 ms kernel | 31.50 ms |
| 600 s stereo, transfer-through-result total | 44.73 ms | 79.96 ms |
| 300 s 7.1, host-to-device | 40.81 ms | 52.64 ms |
| 300 s 7.1, kernel/reduction | 51.30 ms kernel | 54.90 ms |
| 300 s 7.1, transfer-through-result total | 89.84 ms | 103.09 ms |

The whole-buffer cases use seven-run medians. Bounded stereo uses 440 chunks
and bounded 7.1 uses 220 chunks, each with five-run medians. The latter moves
about 461 MB of decoded f32 samples, so the reported 103.09 ms includes the
data-movement cost that a kernel-only benchmark would hide. Small deterministic
finite-signal checks for two, seven, and eight channels produced the same final
f32 peak bits as a CPU reference using the same f64 FMA order.

This is a promising feasibility result, not an end-to-end Forge speed claim.
The current v0.140.0 7.1 analysis takes 1.590 seconds and also performs decode,
K-weighting, exact frame/channel reductions, rolling windows, and gating. Amdahl
back-solving from the measured four-pair CPU result estimates about 1.27
seconds in the old sequential true-peak portion, but that estimate ignores
parallel overhead and is not a substitute for an integrated comparison. A
production candidate still needs asynchronous overlap with CPU K-weighting,
context-startup and short-file thresholds, exact chunk-tail handling,
NaN/infinity/subnormal tests, multiple GPU generations, a dynamically detected
optional runtime, and an unchanged CPU fallback. No GPU dependency or product
path is added by v0.140.0.

### v0.141.0: optional bounded CUDA true-peak worker

The production path implements the feasibility gate above without making CUDA
a mandatory build or runtime dependency. Linux and Windows builds can enable
the `cuda-truepeak` Cargo feature; published binaries enable it and keep CPU as
the default. `--true-peak-backend cuda` probes device 0 through the dynamically
loaded Driver API, loads checked-in PTX generated by NVRTC 11.8 for
`compute_52`, and falls back to CPU with an explicit stderr reason when the
driver or device is absent. NVRTC and the CUDA toolkit are not runtime
requirements.

The CUDA probe also requires the host path to use the same fused arithmetic:
AVX2 plus FMA on x86-64, or mandatory fused Advanced SIMD on AArch64. Other
hosts remain on CPU instead of silently accepting last-bit differences between
the scalar multiply/add reference and the CUDA f64 FMA kernel.

For an eligible decoded chunk, Forge uploads the preceding 15 samples and the
current planar samples, launches one independent output-frame thread with the
same coefficient table and ordered f64 FMAs as the CPU meter, and queues a
four-byte peak result per channel. Transfer and kernel work start before CPU
K-weighting; synchronization occurs only after the exact established CPU
energy, channel, and gating reductions finish. One process-wide lease bounds
the CUDA worker and device allocation. If a later driver operation fails, the
worker reconstructs each CPU meter from its retained 15-sample history and
earlier peak before replaying the failing chunk.

The integrated benchmark uses the same RTX 2080 Ti/WSL2 host as the PoC, a
native-host fat-LTO release without PGO, deterministic PCM16 WAVE fixtures, and
alternating CPU/CUDA run order. Unlike the PoC table, every wall time below
includes process startup, CUDA context creation, PTX JIT/cache behavior,
decoder conversion to f32, all transfers, K-weighting, gating, and reporting.
Analysis uses seven-run medians, normalization five, and the four-file serial
batch three. RSS is the process maximum reported by `time(1)`.

| Workload / median | CPU | CUDA | Change |
| --- | ---: | ---: | ---: |
| 600 s stereo analysis, wall | 1.51 s | 1.05 s | -30.46% |
| 600 s stereo analysis, user CPU | 1.69 s | 0.70 s | -58.58% |
| 600 s stereo normalization, wall | 2.04 s | 1.64 s | -19.61% |
| 4 × 600 s stereo analysis, `--jobs 1`, wall | 4.93 s | 2.44 s | -50.51% |
| 300 s 7.1 analysis, wall | 1.63 s | 1.67 s | +2.45% |
| 300 s 7.1 analysis, user CPU | 3.94 s | 1.96 s | -50.25% |
| Stereo analysis peak RSS | 12.83 MiB | 126.85 MiB | +114.02 MiB |

CPU and CUDA analysis JSON hashes matched for stereo, 7.1, and the four-file
batch. The 115,200,044-byte normalized stereo WAVE also matched byte for byte.
An eight-channel two-chunk test compares final f32 peak bits directly and then
hands the completed first-chunk state to CPU meters; both stages match the CPU
reference. Hiding the GPU with `CUDA_VISIBLE_DEVICES=-1` produced the expected
fallback diagnostic and the same CPU JSON hash. A source SHA-256 embedded in
the PTX is checked by tests so edits cannot silently leave the kernel stale.

The result also defines the selection policy: CUDA remains explicit rather
than automatic. It wins on long stereo and repeated same-process workloads,
but the already-parallel v0.140.0 7.1 CPU path was 2.45% faster on this host and
used about 114 MiB less peak RSS. Mono, timeline analysis, first packets below
16,384 frames, and sample rates at or above 192 kHz therefore stay on CPU even
when CUDA is requested. Future device/layout heuristics require measurements on
more GPU generations rather than extrapolation from this host.

#### Caller-owned limiter output and paired detection

The streaming normalization pipeline now owns one planar limiter-output buffer
for the entire render. `TruePeakLimiter::process_into` clears each channel
without releasing its capacity, reserves only when a larger decoded chunk first
arrives, and writes the delayed tail back into the same storage. The existing
allocating `process`, `finish`, and `finish_with_statistics` methods remain
compatibility wrappers; explicit `finish_into` variants let other streaming
callers use the allocation-stable path too.

Within the detector loop, adjacent channels advance through the existing
byte-exact paired True Peak kernel. Stereo uses a fixed two-channel path without
dynamic chunk iterators; multichannel material reduces each pair's two frame
peaks in the original channel order, including an odd scalar tail. Gain
envelope, look-ahead queue, release, and statistics update order are unchanged.

An alternating seven-run benchmark used the same native-host fat-LTO build and
deterministic PCM16 fixtures as the CUDA measurements, `--jobs 1` to remove
analysis-pool scheduling variance, a 0 LUFS target, -1 dBTP ceiling, and the
limiter enabled. Times include decode, analysis, limiting, quantization, and
WAVE I/O.

| Limiter-heavy render / median | Previous | Reused + paired | Change |
| --- | ---: | ---: | ---: |
| 600 s stereo | 3.66 s | 3.37 s | -7.92% |
| 300 s 7.1 | 8.27 s | 6.87 s | -16.93% |

For both layouts, the old and new normalized WAVE SHA-256 hashes matched. The
versioned difference reports also matched byte for byte when generated at the
same paths, covering the entire gain-reduction envelope and all before/after
statistics. Unit tests additionally provision caller-owned capacity, process
multiple unequal limiter states, assert the allocation pointers remain stable,
and compare every emitted sample plus the flushed tail with the allocating API.

#### Persistent multichannel K-weighting lanes

The accepted multichannel path retains four independent K-weighting states in
AVX2 f64 lanes across decoder chunks. Both biquad stages keep their broadcast
coefficients and delay state in `__m256d`; only four planar input samples enter
and four f32 filtered values leave per frame. The required f32 rounding between
the shelf and RLB stages is explicit. Weighted energy, raw energy, sample peak,
and channel-role reductions remain scalar and left-to-right, preserving every
established result bit. A 7.1 analyzer owns two fixed banks allocated once at
construction (under 1 KiB total); processing itself allocates no scratch.

Two simpler prototypes were measured and rejected. Sharing coefficients in an
AoS pair loop was neutral on CPU 7.1 (2.57 to 2.59 seconds). Gathering four AoS
states into AVX2 and storing them back every frame was substantially worse (2.66
to 3.66 seconds on CPU and 1.46 to 2.03 seconds with CUDA). Neither rejected
form remains in the implementation. Non-AVX2 hosts, layouts below four channels,
and timeline analysis use the unchanged scalar filters.

The accepted comparison used a deterministic 300-second PCM16 7.1 fixture, a
native-host fat-LTO build, `--jobs 1`, analysis-only operation, and alternating
old/new order. CPU medians use nine runs; the CUDA overlap result was repeated
over eleven runs after an initial nine-run pass.

| 300 s 7.1 analysis / median | Scalar | Persistent lanes | Change |
| --- | ---: | ---: | ---: |
| CPU True Peak | 2.61 s | 2.53 s | -3.07% |
| CUDA True Peak | 1.38 s | 1.30 s | -5.80% |

Pre-change and accepted CPU/CUDA JSON matched byte for byte. The stereo CPU and
CUDA JSON also remained unchanged because stereo stays on its measured scalar
K-weighting path. Unit tests compare all four SIMD outputs bit for bit with four
scalar filters at 8, 44.1, 48, 96, 192, and 384 kHz, including subnormal, NaN,
and positive/negative infinity propagation across streaming state.

#### Multichannel PCM16 and PCM24 quantization

Undithered integer WAVE output now quantizes channels in frame-major AVX2 groups
of eight, writing directly into the already-reused interleaved byte buffer. A
layout with three to seven remaining channels uses a zero-filled partial vector;
one or two final channels retain the scalar tail. PCM16 packs eight clamped i32
codes to one contiguous i16 frame, while PCM24 stores the low three bytes of the
same exact SIMD-rounded i32 codes. The existing block-over-time mono/stereo
PCM16 path remains unchanged.

The SIMD quantizer duplicates scalar semantics deliberately: NaN maps to zero,
infinities clamp to the signed endpoints, multiplication by the power-of-two
full scale is exact for f32 input, halfway values round away from zero, and the
positive endpoint saturates one code below the negative magnitude. Dithered
output stays scalar so each channel's TPDF RNG call order and state are
unchanged. Non-x86 and non-AVX2 builds also keep the scalar fallback.

Alternating seven-run comparisons used deterministic 300-second PCM16 sources,
a native-host fat-LTO build, `--jobs 1`, ordinary normalization, and WAVE output
at the indicated depth. Times include decode, analysis, gain, quantization,
interleaving, and file I/O.

| Render / median | Scalar | Multichannel SIMD | Change |
| --- | ---: | ---: | ---: |
| 7.1 PCM16 | 3.40 s | 3.05 s | -10.29% |
| 7.1 PCM24 | 3.66 s | 3.35 s | -8.47% |
| 5.1 PCM16 | 2.64 s | 2.38 s | -9.85% |
| 5.1 PCM24 | 2.82 s | 2.55 s | -9.57% |

The old and new 7.1 WAVE SHA-256 hashes matched at both depths. Their versioned
difference reports also matched byte for byte, covering the exact lossless
verification tee measurements. Unit tests compare SIMD with the scalar encoder
for 3, 5, 6, 7, 8, 9, 10, 11, 12, and 16 channels over arbitrary f32 bit
patterns, half-LSB boundaries, subnormals, NaN, infinities, full-scale endpoints,
partial vectors, and scalar channel tails; RNG state is also asserted unchanged.

### v0.142.0: allocation-stable MP3 and Opus feeds

MP3 encoding now passes Forge's existing planar f32 channels directly to
LAME's planar IEEE-float API. The streaming writer no longer allocates and
fills an interleaved stereo vector for every decoder chunk, and the compatible
whole-buffer API no longer allocates a duration-sized interleaved copy. LAME
settings, chunk boundaries, input values, and tag backpatching are unchanged.

Opus resampling now advances through its retained planar input with Rubato's
input offset instead of draining and shifting every channel after each 1,024
frame resampler call. One retained planar output buffer is resized without
releasing capacity. The interleaved Opus queue likewise advances an offset
rather than allocating a 960-frame vector and shifting the remaining queue for
every packet; at each decoder boundary it compacts fewer than two packets once.
Ogg packet ownership and page assembly remain unchanged.

Alternating seven-pair comparisons used deterministic stereo f32 WAVE inputs,
a native-host fat-LTO build without PGO, ordinary -16 LUFS normalization, and
192 kb/s output. The 48 kHz cases are 600 seconds; the resampling case is 300
seconds at 44.1 kHz. Times include decode, analysis, gain, encoding, metadata,
and file I/O. Total CPU is the median of user plus system time for each run.

| Workload / median | v0.141.0 | v0.142.0 | Change |
| --- | ---: | ---: | ---: |
| MP3 48 kHz, wall | 15.70 s | 15.54 s | -1.02% |
| MP3 48 kHz, total CPU | 17.25 s | 17.16 s | -0.52% |
| MP3 48 kHz, peak RSS | 25,616 KiB | 24,724 KiB | -3.48% |
| Opus 48 kHz, wall | 7.41 s | 7.10 s | -4.18% |
| Opus 48 kHz, user CPU | 8.14 s | 7.73 s | -5.04% |
| Opus 48 kHz, total CPU | 8.55 s | 8.18 s | -4.33% |
| Opus 44.1→48 kHz, wall | 3.89 s | 3.87 s | -0.51% |
| Opus 44.1→48 kHz, user CPU | 4.36 s | 4.25 s | -2.52% |
| Opus 44.1→48 kHz, total CPU | 4.55 s | 4.45 s | -2.20% |

All seven paired total-CPU comparisons improved for both Opus fixtures. Peak
RSS rose by 524 KiB (2.38%) for the 600-second same-rate case and 440 KiB
(2.56%) for the 300-second resampling case, remaining bounded by the largest
stream chunk rather than programme duration. MP3, same-rate Opus, and
resampled Opus outputs matched their v0.141.0 SHA-256 hashes byte for byte.
Tests also assert that the pending Opus queue and resampler input/output
allocations remain stable across unequal writes.

Two measured variants were rejected. Retaining FLAC's large pending allocation
changed a 600-second render by +0.74% total CPU and +0.74% peak RSS, so the
existing small-tail allocation remains. Reusing a libopus output scratch and
copying each owned Ogg packet reduced CPU but raised Opus peak RSS by 10.55%;
the accepted path therefore retains the encoder's owned-packet API.

### v0.143.0: verified output analysis reuse

Corrected normalization already owns the completed output measurement used to
accept or reject an encode. BWF and ReplayGain finalization now borrow that
measurement instead of opening, decoding, and analyzing the same output again.
The optimization applies to single-file, album, and multi-delivery correction
when container-default channel roles are in use. Explicit custom roles retain
the previous independent container analysis so the interpretation and written
metadata cannot change. Multi-delivery also retains its final post-metadata
decode and verification before any destination is committed.

Alternating seven-pair comparisons used one deterministic 600-second, 48 kHz,
stereo PCM16 WAVE source, a native-host fat-LTO build without PGO, and
`--verify --bwf`. Times include source analysis, normalization, exact quantized
PCM verification, BWF copying and update, and atomic output replacement. The
ordinary `--verify` control used five alternating pairs. Medians are reported
except that peak RSS is the maximum of all runs.

| Workload / metric | v0.142.0 | v0.143.0 | Change |
| --- | ---: | ---: | ---: |
| Corrected BWF WAVE, wall | 6.75 s | 5.22 s | -22.67% |
| Corrected BWF WAVE, user CPU | 6.85 s | 5.12 s | -25.26% |
| Corrected BWF WAVE, system CPU | 0.84 s | 0.79 s | -5.95% |
| Corrected BWF WAVE, peak RSS | 124,656 KiB | 124,656 KiB | unchanged |
| Ordinary corrected WAVE, wall | 4.79 s | 4.75 s | -0.84% |

The v0.142.0 and v0.143.0 corrected BWF outputs matched byte for byte. The
completed file's independently measured LUFS and true peak also match the BWF
fields written from the borrowed verification result. Targeted tests cover the
borrowed/owned analysis paths, corrected BWF metadata, corrected album output,
and final multi-delivery verification.

### v0.144.0: single-pass multi-delivery fan-out

Multi-delivery previously invoked the complete decode, optional resample,
gain/ceiling or limiter, and render-statistics pipeline once per requested
container. It now creates every codec writer first and feeds each protected
planar chunk to all writers from one shared processing pass. Writer-specific
quantization, dither RNGs, codec state, finalization, lossless verification
tees, completed-codec re-decodes, metadata mutation checks, and atomic commit
remain independent. Every reported delivery receives a clone of the one
render-statistics result because its pre-codec signal is now identical by
construction.

Alternating comparisons used a deterministic 600-second, 48 kHz, stereo PCM16
pink-noise WAVE source and native-host fat-LTO builds without PGO. Seven pairs
were used for WAVE+FLAC and the single-output control; five pairs were used for
WAVE+FLAC+Opus. Each run includes source analysis, rendering, exact lossless or
decoded-codec verification, final post-metadata re-decode, and atomic output
replacement. Values are medians, including the per-run peak-RSS samples.

| Workload / metric | v0.143.0 | v0.144.0 | Change |
| --- | ---: | ---: | ---: |
| WAVE+FLAC multi-delivery, wall | 14.40 s | 12.65 s | -12.15% |
| WAVE+FLAC multi-delivery, user CPU | 13.93 s | 11.86 s | -14.86% |
| WAVE+FLAC multi-delivery, system CPU | 1.40 s | 1.31 s | -6.43% |
| WAVE+FLAC multi-delivery, peak RSS | 129,680 KiB | 137,132 KiB | +5.75% |
| WAVE+FLAC+Opus multi-delivery, wall | 26.83 s | 24.30 s | -9.43% |
| WAVE+FLAC+Opus multi-delivery, user CPU | 25.71 s | 23.21 s | -9.72% |
| WAVE+FLAC+Opus multi-delivery, system CPU | 1.66 s | 1.59 s | -4.22% |
| WAVE+FLAC+Opus multi-delivery, peak RSS | 134,076 KiB | 133,484 KiB | -0.44% |
| Single corrected WAVE control, wall | 4.97 s | 4.92 s | -1.01% |
| Single corrected WAVE control, user CPU | 4.76 s | 4.81 s | +1.05% |
| Single corrected WAVE control, peak RSS | 124,644 KiB | 124,696 KiB | +0.04% |

The single-output differences are noise-level and show no material regression.
WAVE, FLAC, and Opus outputs matched v0.143.0 byte for byte. A dedicated unit
test also compares separate and fan-out WAVE/FLAC encodes, exact lossless
verification measurements, and render statistics.

### v0.145.0: exact adaptive analysis true-peak pruning

Analysis previously reconstructed every oversampled FIR phase even after an
earlier peak was already impossible to exceed. The all-time-maximum paths now
use the triangle inequality
`abs(sum(c[i] * x[i])) <= max(abs(x[i])) * sum(abs(c[i]))`. The maximum phase
L1 coefficient norm is conservatively inflated for floating-point rounding.
After an exact 16-sample probe proves the complete history window is below that
bound, subsequent windows skip interpolation until a new sample invalidates
the proof. Failed probes are spaced 256 samples apart so dense material retains
the established paired SIMD path with little monitoring cost.

This shortcut is exact and limited to callers that need only the retained
maximum. Timeline interval peaks, true-peak limiter detection, and other
frame-level callers continue to reconstruct every phase. Tests compare exact
and pruned meters bit for bit at 48, 96, and 192 kHz across chunk boundaries,
stereo pairing, transients, quiet tails, NaN, infinities, and subnormal inputs.

Alternating comparisons used deterministic 600-second, 48 kHz stereo PCM16
WAVE fixtures and native-host fat-LTO builds without PGO. The pink-noise case
represents a common distribution where an early retained peak dominates later
windows. The rising two-tone case deliberately keeps challenging the retained
peak and is the disclosed dense worst case. Analysis used seven pairs; complete
normalization with `--verify` used five pink-noise pairs and a separate
seven-pair dense rerun. Values are medians.

| Workload / metric | v0.144.0 | v0.145.0 | Change |
| --- | ---: | ---: | ---: |
| Pink-noise analysis, wall | 1.50 s | 0.93 s | -38.00% |
| Pink-noise analysis, user CPU | 1.66 s | 1.10 s | -33.73% |
| Pink-noise normalize + verify, wall | 4.71 s | 3.22 s | -31.63% |
| Pink-noise normalize + verify, user CPU | 4.34 s | 2.66 s | -38.71% |
| Rising two-tone analysis, wall | 1.50 s | 1.53 s | +2.00% |
| Rising two-tone analysis, user CPU | 1.65 s | 1.66 s | +0.61% |
| Rising two-tone normalize + verify, wall | 4.67 s | 4.79 s | +2.57% |
| Rising two-tone normalize + verify, user CPU | 4.31 s | 4.38 s | +1.62% |

The analysis JSON and normalized WAVE files matched v0.144.0 byte for byte for
both fixtures. Median peak RSS changed by less than 0.25%. The small dense-case
cost is retained and documented because common peak distributions save roughly
one third of end-to-end CPU without changing BS.1770 results or output bytes.

### v0.146.0: bounded parallel FLAC frames

FLAC fixed-size frames are independent once their ordinal and immutable stream
format are known. The writer now coalesces short decoder packets into a bounded
batch, encodes each 4096-sample frame and its bitstream in the existing global
Rayon pool, and writes the completed bytes in ordinal order. Input MD5,
STREAMINFO extrema, total samples, frame numbering, dither state, verification,
and error observation remain serial and deterministic.

The batch retains at most 256 Ki signed sample values (1 MiB) and exposes at
most eight active tasks, regardless of the global `--jobs` value. Worker-local
frame and byte sinks are reused. One-worker runs bypass coalescing and keep the
previous serial path. This avoids oversubscribing album/batch work with a
second codec-owned pool: a writer created inside existing file-level Rayon work
also stays serial. Storage remains bounded by configuration rather than
programme duration.

Alternating five-pair comparisons used deterministic 600-second, 48 kHz,
stereo PCM16 sources, native-host fat-LTO builds without PGO, `--verify`, and
`--jobs 32`. The WAVE case exercises long native decoder chunks; the FLAC case
exercises small-packet coalescing. Times include analysis, gain, encoding,
lossless verification, metadata finalization, and atomic replacement. Values
are medians.

| Workload / metric | v0.145.0 | v0.146.0 | Change |
| --- | ---: | ---: | ---: |
| WAVE to FLAC verify, wall | 7.25 s | 5.76 s | -20.55% |
| WAVE to FLAC verify, user CPU | 7.30 s | 7.80 s | +6.85% |
| WAVE to FLAC verify, total CPU | 7.96 s | 8.88 s | +11.56% |
| WAVE to FLAC verify, peak RSS | 83,620 KiB | 94,572 KiB | +13.10% |
| FLAC to FLAC verify, wall | 7.70 s | 6.02 s | -21.82% |
| FLAC to FLAC verify, user CPU | 6.70 s | 7.16 s | +6.87% |
| FLAC to FLAC verify, total CPU | 7.61 s | 8.49 s | +11.56% |
| FLAC to FLAC verify, peak RSS | 79,060 KiB | 84,884 KiB | +7.37% |
| One-worker WAVE control, total CPU | 7.22 s | 7.19 s | -0.42% |
| One-worker WAVE control, peak RSS | 83,440 KiB | 83,700 KiB | +0.31% |

The latency improvement intentionally spends about 12% more aggregate CPU on
the measured multicore host; users prioritizing energy or concurrent unrelated
work can select a smaller `--jobs` value or `--jobs 1`. WAVE-input,
FLAC-input, 16-bit dithered, and 24-bit outputs matched v0.145.0 byte for byte.
Tests also cover uneven caller chunks, a partial final frame, coalescing at the
memory boundary, and serial/parallel equality.

### v0.147.0: byte-exact SIMD FLAC sample staging

Before frame encoding, the FLAC writer converted planar f32 chunks into its
frame-major signed-integer queue with a scalar frame/channel loop. Undithered
16-bit and 24-bit output now uses runtime-dispatched AVX2: mono quantizes eight
frames per vector, stereo quantizes two vectors and interleaves their i32 lanes,
and three-to-eight-channel layouts quantize one frame's channel group at a time.
Scalar tails retain the same operation order. Dithered output remains on the
established scalar path so its per-channel TPDF RNG sequence cannot change.
Non-AVX2 targets retain the complete scalar implementation.

Tests compare the SIMD and scalar i32 queues for both depths and 1/2/3/5/8
channels across random f32 bit patterns, NaN, infinities, out-of-range values,
signed zero, subnormals, and quantizer boundaries. Separate tests cover caller
prefix preservation, unequal chunks, dither state across chunks, partial FLAC
tails, and serial/parallel encoded-file equality.

Alternating five-pair comparisons used the exact final v0.146.0 commit and the
v0.147.0 candidate, each built separately with native-host fat LTO and no PGO.
The deterministic input is 600 seconds of stereo 48 kHz f32 WAVE. Every run
includes LUFS analysis, gain, FLAC encoding, exact lossless verification,
metadata finalization, and atomic replacement. Values are medians.

| Workload / metric | v0.146.0 | v0.147.0 | Change |
| --- | ---: | ---: | ---: |
| 24-bit, `--jobs 32`, wall | 5.75 s | 5.57 s | -3.13% |
| 24-bit, `--jobs 32`, total CPU | 8.82 s | 8.71 s | -1.25% |
| 24-bit, `--jobs 32`, peak RSS | 94,104 KiB | 94,088 KiB | -0.02% |
| 16-bit, `--jobs 32`, wall | 5.33 s | 5.26 s | -1.31% |
| 16-bit, `--jobs 32`, total CPU | 8.25 s | 8.15 s | -1.21% |
| 16-bit, `--jobs 32`, peak RSS | 40,312 KiB | 40,312 KiB | unchanged |
| 24-bit, `--jobs 1`, wall | 6.79 s | 6.65 s | -2.06% |
| 24-bit, `--jobs 1`, total CPU | 6.92 s | 6.93 s | +0.14% |
| 24-bit, `--jobs 1`, peak RSS | 83,724 KiB | 83,724 KiB | unchanged |

All measured 16-bit and 24-bit FLAC files matched v0.146.0 byte for byte and
all verification runs passed. The one-worker CPU difference is noise-level;
the retained implementation improves end-to-end latency without material CPU
or memory cost.

### v0.148.0: pipelined FLAC source context

Parallel FLAC batches previously waited for every independent frame and its
bitstream to finish before updating the serial source `Context` used for the
mandatory STREAMINFO MD5 and total-sample count. The writer now executes those
two independent stages with `rayon::join`: one branch uses the existing bounded
frame tasks, while the other visits the same interleaved samples in source
order and calls `Context::fill_interleaved` at the same 4096-frame boundaries.
Frame bytes are still checked and written in ordinal order after both branches
finish. No sample copy, extra thread pool, persistent queue, or new allocation
is introduced.

The pipeline is active only in the bounded multi-frame path. `--jobs 1`, short
tails, and writers created inside existing file-level Rayon work retain the
established serial implementation. The design follows the same overlap used by
flacenc's own parallel source-context worker while keeping Forge's single global
concurrency budget.

Alternating five-pair comparisons used the exact final v0.147.0 commit and the
v0.148.0 candidate, separately built with native-host fat LTO and no PGO. Both
deterministic inputs contain 600 seconds of stereo 48 kHz audio; the WAVE case
uses long native chunks and the FLAC case exercises short-packet coalescing.
Every run used `--bits 24 --verify --jobs 32` and includes analysis, gain,
encoding, exact lossless verification, metadata finalization, and atomic
replacement. Values are medians.

| Workload / metric | v0.147.0 | v0.148.0 | Change |
| --- | ---: | ---: | ---: |
| WAVE to FLAC verify, wall | 5.65 s | 5.29 s | -6.37% |
| WAVE to FLAC verify, total CPU | 8.77 s | 8.86 s | +1.03% |
| WAVE to FLAC verify, peak RSS | 94,092 KiB | 95,300 KiB | +1.28% |
| FLAC to FLAC verify, wall | 5.85 s | 5.56 s | -4.96% |
| FLAC to FLAC verify, total CPU | 8.36 s | 8.63 s | +3.23% |
| FLAC to FLAC verify, peak RSS | 86,064 KiB | 86,792 KiB | +0.85% |

All ten candidate runs passed output verification, and both completed FLAC
files matched v0.147.0 byte for byte. The measured latency reduction is retained
with the disclosed small aggregate-CPU cost; lower `--jobs` values remain
available when concurrent unrelated work or energy is more important than
single-file completion time.

A follow-up experiment packed the retained i32 queue into one reusable PCM-byte
buffer and called the source `Context` once per FLAC block instead of once per
sample. It reduced total CPU by 1.6% for 24-bit output and 2.2% for 16-bit
output, but end-to-end wall time changed by only -1.0% and +0.6%, respectively,
while peak RSS rose by up to 1.9%. An AVX2 byte-shuffle variant reduced the
24-bit wall-time difference to -0.2% rather than making it material. Both
variants produced byte-identical files, but the extra buffer and code were
rejected because they did not improve both integer depths.

### v0.149.0: SIMD block true-peak pruning

The exact v0.145.0 true-peak shortcut still advanced the circular FIR history
and checked the conservative coefficient bound for every sample after its
16-sample proof became active. The analyzer now first reduces a complete
decoder channel to its absolute maximum and NaN flag in one runtime-dispatched
AVX2 pass. If that maximum satisfies the same triangle-inequality proof, every
interpolation window in the chunk is safe: the meter retains its peak and
reconstructs only the final 16-sample history needed by the next chunk.

Stereo channels make that decision independently. If neither block is safe,
the established paired interpolation loop remains unchanged; if only one is
safe, the other retains its exact scalar meter updates. NaN blocks, dense
material, timeline measurement, limiter detection, and sample rates that do
not require interpolation retain their previous paths. Tests compare block
skips against every sample update before and after future transients, cover
NaN/infinity/subnormal history, and retain the existing chunk-boundary and
exact-interpolation equivalence suites.

CPU-pinned alternating comparisons used 600-second, 48 kHz stereo PCM16 WAVE
fixtures and native-host fat-LTO builds without PGO. The quiet-tail fixture has
an early near-full-scale event followed by low-level two-tone material, where
the retained maximum makes complete chunks provably safe. The rising dense
fixture continually challenges that maximum. Analysis uses 15 pairs and full
WAVE normalization with exact lossless `--verify` uses seven pairs. Values are
medians; total CPU is user plus system time.

| Workload / metric | v0.148.0 | v0.149.0 | Change |
| --- | ---: | ---: | ---: |
| Quiet-tail analysis, wall | 0.65 s | 0.52 s | -20.00% |
| Quiet-tail analysis, total CPU | 0.66 s | 0.54 s | -18.18% |
| Quiet-tail normalize + verify, wall | 2.32 s | 1.92 s | -17.24% |
| Quiet-tail normalize + verify, total CPU | 2.36 s | 1.98 s | -16.10% |
| Rising dense analysis, wall | 1.36 s | 1.33 s | -2.21% |
| Rising dense analysis, total CPU | 1.40 s | 1.37 s | -2.14% |
| Rising dense normalize + verify, wall | 4.37 s | 4.37 s | unchanged |
| Rising dense normalize + verify, total CPU | 4.47 s | 4.46 s | -0.22% |

Analysis peak RSS was unchanged at 13,056 KiB for both fixtures. Normalize and
verify peak RSS changed by less than 0.05%. Candidate analysis JSON and
normalized WAVE SHA-256 hashes matched v0.148.0 for both fixtures. A separate
15-pair deterministic sawtooth control improved by 0.74%, which is treated as
noise rather than a broad workload claim; the retained gain is specifically
for chunks made safe by an already-established peak.

### v0.150.0: bounded zero-copy lossy render pipeline

MP3 and Opus encoding can now run on one worker from Forge's existing global
Rayon pool while the caller decodes and applies the next normalization chunk.
The decoder or output-domain PCM spool transfers its channel allocations to the
writer by ownership and receives a recycled allocation in return. Pipeline
depth is fixed at two chunks: storage is bounded by channel count and decoder
chunk geometry rather than programme duration, and no PCM payload is copied.

The writer is constructed inside its worker, so LAME's native handle never
crosses threads. Setup failures are reported before DSP begins; an encoder
failure stops the producer and retains earlier-chunk error precedence; finish
and codec metadata remain ordered. `--jobs 1`, nested file-level work, and
formats without a measured gain remain synchronous. A look-ahead limiter or an
uncached caller-owned resampler output uses the established bounded-copy path.
Normal plan-aware resampling is eligible for zero-copy replay because its
already-required output-domain spool owns recyclable channel buffers.

CPU-pinned alternating comparisons used deterministic 600-second, 48 kHz
stereo PCM16 WAVE fixtures and native-host fat-LTO builds without PGO. MP3
quiet-tail uses five pairs; the dense MP3, Opus, and one-worker control use
seven. The shared host produced visible scheduler-preemption outliers, so
values are medians and execution order alternates on every pair. Total CPU is
user plus system time.

| Workload / metric | v0.149.0 | v0.150.0 | Change |
| --- | ---: | ---: | ---: |
| Quiet-tail MP3, wall | 21.35 s | 20.24 s | -5.20% |
| Quiet-tail MP3, total CPU | 22.07 s | 20.65 s | -6.43% |
| Quiet-tail MP3, peak RSS | 22,888 KiB | 29,296 KiB | +28.00% |
| Rising-dense MP3, wall | 16.77 s | 15.16 s | -9.60% |
| Rising-dense MP3, total CPU | 16.74 s | 15.97 s | -4.60% |
| Rising-dense MP3, peak RSS | 22,544 KiB | 29,024 KiB | +28.74% |
| Quiet-tail Opus, wall | 5.16 s | 4.57 s | -11.43% |
| Quiet-tail Opus, total CPU | 5.46 s | 5.45 s | -0.18% |
| Quiet-tail Opus, peak RSS | 26,752 KiB | 29,308 KiB | +9.55% |
| Opus `--jobs 1`, wall | 4.28 s | 4.29 s | +0.23% |
| Opus `--jobs 1`, total CPU | 4.44 s | 4.45 s | +0.23% |
| Opus `--jobs 1`, peak RSS | 26,948 KiB | 26,948 KiB | unchanged |
| MP3 input to MP3, wall | 17.53 s | 16.28 s | -7.13% |
| MP3 input to MP3, total CPU | 17.01 s | 17.99 s | +5.76% |
| MP3 input to MP3, peak RSS | 22,480 KiB | 22,600 KiB | +0.53% |

The compressed-input case uses five Latin-square-ordered triples and the MP3
produced from the quiet-tail fixture. It exercises Symphonia packet decode,
gain/ceiling DSP, and LAME encode in the same command. Its wall-time improvement
comes with a disclosed aggregate-CPU cost; file-level parallel workloads can
retain the established path by nesting work in the shared Rayon pool, while
latency-sensitive single-file work receives the overlap.

Every measured MP3 and Opus file matched v0.149.0 byte for byte. Dedicated
tests also compare synchronous and pipelined MP3 bytes, writer chunk
boundaries, multi-delivery WAVE/FLAC output, render statistics, and exact
lossless verification. The additional live buffer is fixed at one decoder
chunk beyond the serial path; it buys lower single-file latency without a
programme-length queue or extra thread pool.

Two broader pipeline variants were measured and rejected. A full
decode-to-DSP-to-encode three-stage candidate preserved output bytes and
statistics, but moved the compressed-input median from 16.28 s to 16.49 s
(+1.29%) and total CPU from 17.99 s to 19.39 s (+7.78%): synchronization on
every small codec packet outweighed the added overlap. An eight-packet bounded
handoff reduced system CPU by about 72%, but two order-reversed probes regressed
wall time from 15.82/15.77 s to 17.04/17.42 s. Returning allocations only after
a complete batch removed the fine-grained producer/encoder overlap. Both
experiments were byte-identical and were removed because elapsed time, not a
kernel or synchronization counter, is the release criterion.

A follow-up replaced the standard bounded channels with Crossbeam 0.5.16 to
test whether a more lightly locked queue could preserve per-packet overlap
while lowering coordination cost. Two order-reversed compressed-input probes
were byte-identical, but the aggregate wall midpoint changed by only -0.15%,
total CPU rose 1.34%, and system CPU rose about 9.8%. The generic MPMC channel
was removed. The cache-optimized SPSC result in
[FastForward](https://doi.org/10.1145/1345206.1345215) still motivates a
specialized ring experiment, but only with a separately reviewed memory-order
proof and the same end-to-end acceptance gate.

Render-statistics scans were screened as another memory-traffic candidate. An
explicit AVX2 implementation counted threshold exceedances in eight-sample
groups and combined the post-gain full-scale and ceiling counters into one
pass. Across 15 alternating WAVE normalize-and-verify pairs, its paired wall
median regressed 2.16% despite 1.02% lower total CPU; only three pairs were
faster. A scalar-only follow-up removed the extra SIMD dispatch and retained
only the two-threshold pass fusion, but regressed paired wall and CPU medians by
1.07% and 1.04%, respectively, with six of 15 wall-time wins. Both outputs were
byte-identical and verified exactly. The prototypes were removed: LLVM already
optimizes the established counts sufficiently, and the counters are not a
material end-to-end bottleneck on this workload.

### v0.151.0: byte-exact AArch64 NEON WAVE quantization

Undithered PCM16 and PCM24 WAVE output now uses Advanced SIMD (NEON) on
little-endian AArch64. PCM16 mono and stereo process eight frames per iteration;
the stereo path interleaves narrowed lanes with NEON zip operations. PCM24 mono
and stereo process four frames per iteration and pack the low three bytes of
each signed code directly into the output buffer. Wider layouts quantize four
channels at a time for both depths. Scalar tails preserve partial frames, and
dithered output retains the established scalar RNG order.

The vector quantizer deliberately matches the scalar contract: NaN becomes
zero, infinities and out-of-range samples clamp to the signed endpoints, the
power-of-two f32 scale is exact, halfway values round away from zero, and the
positive endpoint saturates one code below the negative magnitude. Equivalence
tests exercise random f32 bit patterns and boundary values at 1, 2, 3, 5, 6, 7,
8, 9, 10, 11, 12, and 16 channels. Other architectures and big-endian AArch64
retain the scalar or existing AVX2 paths.

The final comparison ran natively on an Apple Silicon GitHub runner with Rust
1.97.0. The exact v0.150.0 commit and candidate were built separately in release
mode. Kernel values are pooled medians of 18 measurements per binary over
16,777,216 frames; normalization values are pooled medians of ten measurements
per binary over deterministic 300-second, 48 kHz fixtures. Each experiment ran
in both orders. The kernel checksum matched the scalar build in every case.

| Workload / median wall | v0.150.0 scalar | v0.151.0 NEON | Change |
| --- | ---: | ---: | ---: |
| PCM16 mono kernel | 72.38 ms | 3.97 ms | -94.51% |
| PCM16 stereo kernel | 71.42 ms | 4.04 ms | -94.34% |
| PCM16 7.1 kernel | 76.35 ms | 14.87 ms | -80.53% |
| PCM24 mono kernel | 89.14 ms | 9.73 ms | -89.08% |
| PCM24 stereo kernel | 95.48 ms | 9.26 ms | -90.30% |
| PCM24 7.1 kernel | 87.88 ms | 20.54 ms | -76.62% |
| Stereo WAVE normalization | 0.897 s | 0.850 s | -5.21% |
| 7.1 WAVE normalization | 2.745 s | 2.410 s | -12.22% |

A WAVE-to-FLAC verification control moved from 3.041 to 2.943 seconds
(-3.21%), but no retained candidate path changes FLAC sample staging, so that
variation is not claimed as an optimization. An earlier prototype did apply
the NEON staging kernel to FLAC; its exact-output run regressed wall time by
0.72% and total CPU by about 1.1%. That path was removed before release. This
keeps the change restricted to the measured WAVE win instead of extrapolating a
fast microkernel to a codec whose parallel encoder has different bottlenecks.

### v0.152.0 candidate: byte-exact AArch64 NEON peak reductions

Sample-peak reduction and the combined peak/NaN observation now use Advanced
SIMD on AArch64. The loops reduce four independent vectors per iteration and
mask NaNs before the maximum operation, preserving the established scalar
result and the True Peak pruning rule that a block containing NaN is never
skipped. Scalar tails retain exact behavior for every slice length.

The candidate was measured against the exact v0.151.0 commit on an Apple
Silicon GitHub runner with Rust 1.97.0. Primitive values are pooled medians of
18 measurements per binary over 16,777,216 samples. End-to-end values are
pooled medians of ten measurements per binary over deterministic 300-second,
48 kHz fixtures. Both orders were run. Primitive checksums, analysis JSON, and
normal and verification WAVE bytes all matched exactly.

| Workload / median wall | v0.151.0 | candidate | Change |
| --- | ---: | ---: | ---: |
| Absolute-peak kernel | 23.30 ms | 2.68 ms | -88.52% |
| Peak plus NaN kernel | 34.13 ms | 2.66 ms | -92.21% |
| Stereo WAVE analysis | 0.627 s | 0.603 s | -3.80% |
| Stereo quiet-tail analysis | 0.308 s | 0.263 s | -14.78% |
| Stereo WAVE normalization | 0.687 s | 0.663 s | -3.46% |
| Stereo WAVE normalization with verification | 1.963 s | 1.820 s | -7.27% |
| 7.1 WAVE normalization | 1.678 s | 1.580 s | -5.84% |

The corresponding total-CPU changes were -4.64%, -14.45%, -3.24%, -6.32%,
and -4.62% for the five end-to-end rows. Earlier prototypes also added explicit
NEON gain, combined gain/clip, and hard-clip loops. The combined path was
neutral to regressive in the first order-reversed run, while 7.1 wall time
regressed 0.58%, so all three were removed before the final measurement. This
keeps the candidate limited to the reductions with both a large isolated win
and consistent end-to-end benefit.

## Final integration gate

Before each release, run the full feature matrix, official conformance jobs
available in the checkout, end-to-end CPU/CUDA and scalar/SIMD equivalence
checks, benchmark regression gates, package reproducibility checks, and the
protected-branch release audit.

Every phase adds a benchmark case or compatible baseline gate before release.
