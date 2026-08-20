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

## Next implementation order

1. Fuse gain, ceiling enforcement, quantization, and writer hand-off where
   output semantics permit it.
2. Tee lossless verification measurements into encoding and remove avoidable
   post-encode reads without weakening lossy-codec verification.
3. Reuse content-addressed analyses across identical input/plan requests.
4. Train and compare profile-guided builds, then publish architecture-specific
   binaries only where reproducible gains justify the distribution cost.
5. Run a GPU proof of concept only after CPU pass/memory optimizations; retain
   it only if transfer and launch overhead improve realistic long-form and
   multichannel workloads.

Every phase adds a benchmark case or compatible baseline gate before release.
