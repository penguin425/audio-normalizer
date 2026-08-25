//! Gated integrated loudness per ITU-R BS.1770-5 / EBU R128.
//!
//! Implements the full two-stage gating scheme:
//!   1. 400 ms blocks with 75% overlap (100 ms hop), absolute gate at -70 LUFS.
//!   2. Relative gate at -10 dB below the absolute-gated mean loudness.
//! The relative gate is applied in the *linear* mean-square domain (it is just
//! the absolute-gated mean square divided by 10), which avoids repeated
//! log/exp conversions and is numerically exact.
//!
//! Performance notes:
//!   * K-weighting runs once per channel in parallel (rayon).
//!   * Per-channel prefix sums of squared K-weighted samples make every block's
//!     energy an O(1) difference — no redundant work despite 75% overlap.
//!   * Squared-sample summation uses the SIMD `sum_squares_f64` primitive.

#[cfg(all(
    feature = "cuda-truepeak",
    any(target_os = "linux", target_os = "windows")
))]
use crate::dsp::cuda_truepeak::CudaTruePeakWorker;
use crate::dsp::kwfilter::KWeight;
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
use crate::dsp::kwfilter::KWeightPair;
#[cfg(target_arch = "x86_64")]
use crate::dsp::kwfilter::KWeightQuad;
use crate::dsp::simd;
use crate::dsp::truepeak::TruePeakMeter;
use crate::wav::{AudioBuffer, ChannelRole};
use rayon::prelude::*;
use std::sync::atomic::{AtomicU8, Ordering};
#[cfg(all(
    feature = "cuda-truepeak",
    any(target_os = "linux", target_os = "windows")
))]
use std::sync::Mutex;

/// Maximum retained 400 ms gating or short-term blocks per analysis.
pub const MAX_LOUDNESS_BLOCKS: usize = 1_000_000;
/// Maximum retained points in an explicitly requested loudness timeline.
pub const MAX_LOUDNESS_TIMELINE_POINTS: usize = 1_000_000;
/// Avoid scheduling channel-pair tasks for short decoder packets where task
/// coordination costs more than the true-peak interpolation work.
const MIN_PARALLEL_TRUE_PEAK_FRAMES: usize = 16_384;

const TRUE_PEAK_BACKEND_CPU: u8 = 0;
#[cfg(all(
    feature = "cuda-truepeak",
    any(target_os = "linux", target_os = "windows")
))]
const TRUE_PEAK_BACKEND_CUDA: u8 = 1;
static TRUE_PEAK_BACKEND: AtomicU8 = AtomicU8::new(TRUE_PEAK_BACKEND_CPU);
#[cfg(all(
    feature = "cuda-truepeak",
    any(target_os = "linux", target_os = "windows")
))]
static CUDA_RUNTIME_FALLBACK: Mutex<Option<String>> = Mutex::new(None);

/// Process-wide backend preference captured when a streaming analyzer is
/// constructed. CPU remains the default; CUDA must be requested explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruePeakBackend {
    Cpu,
    Cuda,
}

/// Select the backend used by subsequently constructed analyzers.
///
/// CUDA is probed immediately. On an unavailable driver, unsupported platform,
/// or binary built without `cuda-truepeak`, the preference is reset to CPU and
/// the reason is returned. Runtime CUDA failures also recover through CPU meter
/// state without changing loudness analysis results.
pub fn configure_true_peak_backend(backend: TruePeakBackend) -> Result<String, String> {
    clear_cuda_runtime_fallback();
    match backend {
        TruePeakBackend::Cpu => {
            TRUE_PEAK_BACKEND.store(TRUE_PEAK_BACKEND_CPU, Ordering::Release);
            Ok("CPU".into())
        }
        TruePeakBackend::Cuda => configure_cuda_true_peak_backend(),
    }
}

/// First CUDA error that caused an analyzer to recover through the CPU after a
/// successful initial runtime probe. Expected contention for the one bounded
/// worker is not an error and is not reported here.
pub fn cuda_runtime_fallback_reason() -> Option<String> {
    cuda_runtime_fallback_reason_impl()
}

#[cfg(all(
    feature = "cuda-truepeak",
    any(target_os = "linux", target_os = "windows")
))]
fn cuda_runtime_fallback_reason_impl() -> Option<String> {
    CUDA_RUNTIME_FALLBACK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

#[cfg(not(all(
    feature = "cuda-truepeak",
    any(target_os = "linux", target_os = "windows")
)))]
fn cuda_runtime_fallback_reason_impl() -> Option<String> {
    None
}

#[cfg(all(
    feature = "cuda-truepeak",
    any(target_os = "linux", target_os = "windows")
))]
fn clear_cuda_runtime_fallback() {
    *CUDA_RUNTIME_FALLBACK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

#[cfg(not(all(
    feature = "cuda-truepeak",
    any(target_os = "linux", target_os = "windows")
)))]
fn clear_cuda_runtime_fallback() {}

#[cfg(all(
    feature = "cuda-truepeak",
    any(target_os = "linux", target_os = "windows")
))]
fn record_cuda_runtime_fallback(error: String) {
    if !error.contains("bounded CUDA true-peak worker is already in use") {
        let mut fallback = CUDA_RUNTIME_FALLBACK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if fallback.is_none() {
            *fallback = Some(error);
        }
    }
}

#[cfg(all(
    feature = "cuda-truepeak",
    any(target_os = "linux", target_os = "windows")
))]
fn configure_cuda_true_peak_backend() -> Result<String, String> {
    match crate::dsp::cuda_truepeak::probe() {
        Ok(device) => {
            TRUE_PEAK_BACKEND.store(TRUE_PEAK_BACKEND_CUDA, Ordering::Release);
            Ok(device)
        }
        Err(error) => {
            TRUE_PEAK_BACKEND.store(TRUE_PEAK_BACKEND_CPU, Ordering::Release);
            Err(error)
        }
    }
}

#[cfg(not(all(
    feature = "cuda-truepeak",
    any(target_os = "linux", target_os = "windows")
)))]
fn configure_cuda_true_peak_backend() -> Result<String, String> {
    TRUE_PEAK_BACKEND.store(TRUE_PEAK_BACKEND_CPU, Ordering::Release);
    Err("this binary was built without CUDA true-peak support".into())
}

#[cfg(all(
    feature = "cuda-truepeak",
    any(target_os = "linux", target_os = "windows")
))]
enum CudaTruePeakState {
    Disabled,
    Pending,
    Active(Box<CudaTruePeakWorker>),
}

#[derive(Debug, Clone)]
pub struct EbuMeasurements {
    pub integrated_lufs: f64,
    pub max_momentary_lufs: f64,
    pub max_short_term_lufs: f64,
    pub loudness_range_lu: f64,
    pub gating_blocks: Vec<f64>,
}

pub struct StreamingMeasurements {
    pub ebu: EbuMeasurements,
    pub frames: usize,
    /// Mean square after K-weighting and BS.1770 channel weighting, without
    /// absolute or relative loudness gating.
    pub weighted_mean_square: f64,
    pub rms_db: f64,
    pub sample_peak: f32,
    pub true_peak: f32,
    pub timeline: Vec<LoudnessTimelinePoint>,
}

#[derive(Debug, Clone)]
pub struct LoudnessTimelinePoint {
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub momentary_lufs: Option<f64>,
    pub short_term_lufs: Option<f64>,
    pub sample_peak_dbfs: f64,
    pub true_peak_dbtp: f64,
}

struct LoudnessWindows {
    values: Vec<f64>,
    momentary_cursor: usize,
    short_term_cursor: usize,
    momentary_limit: usize,
    short_term_limit: usize,
}

impl LoudnessWindows {
    fn new(momentary_limit: usize, short_term_limit: usize) -> Self {
        debug_assert_ne!(momentary_limit, 0);
        debug_assert!(momentary_limit <= short_term_limit);
        Self {
            values: Vec::with_capacity(short_term_limit),
            momentary_cursor: 0,
            short_term_cursor: 0,
            momentary_limit,
            short_term_limit,
        }
    }

    #[inline(always)]
    fn push(&mut self, momentary_sum: &mut f64, short_term_sum: &mut f64, value: f64) {
        let length = self.values.len();
        if length == self.short_term_limit {
            // Once the three-second window is full, every subsequent frame
            // takes this branch. Load both expired values before replacing the
            // oldest slot because both cursors coincide when the limits are
            // equal. The add/subtract order remains bit-identical to the two
            // original independent windows.
            let expired_momentary = self.values[self.momentary_cursor];
            let expired_short_term = self.values[self.short_term_cursor];
            *momentary_sum += value;
            *momentary_sum -= expired_momentary;
            *short_term_sum += value;
            *short_term_sum -= expired_short_term;
            self.values[self.short_term_cursor] = value;
            self.momentary_cursor += 1;
            if self.momentary_cursor == self.short_term_limit {
                self.momentary_cursor = 0;
            }
            self.short_term_cursor += 1;
            if self.short_term_cursor == self.short_term_limit {
                self.short_term_cursor = 0;
            }
            return;
        }

        // Initial fill is bounded to the first three seconds. Retain the
        // previous two-window arithmetic order exactly: add the new value,
        // then subtract the expired momentary value when that window is full.
        *momentary_sum += value;
        if length >= self.momentary_limit {
            *momentary_sum -= self.values[self.momentary_cursor];
            self.momentary_cursor += 1;
        }
        *short_term_sum += value;
        self.values.push(value);
        if self.momentary_cursor == self.short_term_limit {
            self.momentary_cursor = 0;
        }
    }

    fn momentary_len(&self) -> usize {
        self.values.len().min(self.momentary_limit)
    }

    fn short_term_len(&self) -> usize {
        self.values.len()
    }
}

pub struct StreamingAnalyzer {
    sample_rate: u32,
    roles: Vec<ChannelRole>,
    filters: Vec<KWeight>,
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    kweight_pair: Option<KWeightPair>,
    #[cfg(target_arch = "x86_64")]
    kweight_quads: Option<Vec<KWeightQuad>>,
    true_peak_meters: Vec<TruePeakMeter>,
    #[cfg(all(
        feature = "cuda-truepeak",
        any(target_os = "linux", target_os = "windows")
    ))]
    cuda_true_peak: CudaTruePeakState,
    windows: LoudnessWindows,
    momentary_sum: f64,
    short_term_sum: f64,
    next_momentary_block_frame: usize,
    next_short_term_block_frame: usize,
    gating_blocks: Vec<f64>,
    short_term_blocks: Vec<f64>,
    // Division by each fixed window length is monotonic. Retain the maximum
    // sums in the hot loop and convert them to mean square once in `finish`.
    max_momentary_sum: f64,
    max_short_term_sum: f64,
    frames: usize,
    raw_sum_squares: f64,
    weighted_sum_squares: f64,
    sample_peak: f32,
    timeline_interval_frames: Option<usize>,
    timeline: Vec<LoudnessTimelinePoint>,
    timeline_start_frame: usize,
    interval_sample_peak: f32,
    interval_true_peak: f32,
}

impl StreamingAnalyzer {
    pub fn new(sample_rate: u32, roles: Vec<ChannelRole>) -> Self {
        Self::with_timeline_interval(sample_rate, roles, None)
    }

    pub fn with_timeline_interval(
        sample_rate: u32,
        roles: Vec<ChannelRole>,
        interval_frames: Option<usize>,
    ) -> Self {
        let channels = roles.len();
        let next_momentary_block_frame = ((sample_rate as usize * 4) / 10).max(1);
        let next_short_term_block_frame = (sample_rate as usize * 3).max(1);
        #[cfg(all(
            feature = "cuda-truepeak",
            any(target_os = "linux", target_os = "windows")
        ))]
        let cuda_true_peak = if interval_frames.is_none()
            && TRUE_PEAK_BACKEND.load(Ordering::Acquire) == TRUE_PEAK_BACKEND_CUDA
        {
            CudaTruePeakState::Pending
        } else {
            CudaTruePeakState::Disabled
        };
        #[cfg(target_arch = "x86_64")]
        let kweight_quads = if interval_frames.is_none() && channels >= 4 {
            KWeightQuad::for_sample_rate(sample_rate).map(|first| {
                let mut quads = Vec::with_capacity(channels / 4);
                quads.push(first);
                for _ in 1..channels / 4 {
                    quads.push(
                        KWeightQuad::for_sample_rate(sample_rate)
                            .expect("AVX2 availability cannot change within one process"),
                    );
                }
                quads
            })
        } else {
            None
        };
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        let kweight_pair = (interval_frames.is_none() && channels == 2)
            .then(|| KWeightPair::for_sample_rate(sample_rate));
        Self {
            sample_rate,
            roles,
            filters: (0..channels)
                .map(|_| KWeight::for_sample_rate(sample_rate))
                .collect(),
            #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
            kweight_pair,
            #[cfg(target_arch = "x86_64")]
            kweight_quads,
            true_peak_meters: (0..channels)
                .map(|_| TruePeakMeter::for_sample_rate(sample_rate))
                .collect(),
            #[cfg(all(
                feature = "cuda-truepeak",
                any(target_os = "linux", target_os = "windows")
            ))]
            cuda_true_peak,
            windows: LoudnessWindows::new(next_momentary_block_frame, next_short_term_block_frame),
            momentary_sum: 0.0,
            short_term_sum: 0.0,
            next_momentary_block_frame,
            next_short_term_block_frame,
            gating_blocks: Vec::new(),
            short_term_blocks: Vec::new(),
            max_momentary_sum: 0.0,
            max_short_term_sum: 0.0,
            frames: 0,
            raw_sum_squares: 0.0,
            weighted_sum_squares: 0.0,
            sample_peak: 0.0,
            timeline_interval_frames: interval_frames,
            timeline: Vec::new(),
            timeline_start_frame: 0,
            interval_sample_peak: 0.0,
            interval_true_peak: 0.0,
        }
    }

    pub fn process(&mut self, planar: &[Vec<f32>]) -> Result<(), String> {
        if planar.len() != self.roles.len() {
            return Err("stream channel count changed".into());
        }
        let chunk_frames = planar.first().map_or(0, Vec::len);
        if planar.iter().any(|channel| channel.len() != chunk_frames) {
            return Err("stream channel length mismatch".into());
        }
        let momentary_window = ((self.sample_rate as usize * 4) / 10).max(1);
        let short_term_window = (self.sample_rate as usize * 3).max(1);
        let hop = (self.sample_rate as usize / 10).max(1);
        #[cfg(all(
            feature = "cuda-truepeak",
            any(target_os = "linux", target_os = "windows")
        ))]
        if self.begin_cuda_true_peak(planar, chunk_frames) {
            // Transfers and the CUDA kernel are already queued. Preserve the
            // exact CPU K-weighting/reduction order while that independent work
            // runs, then synchronize the tiny per-channel peak result.
            let result = self.process_without_true_peak(
                planar,
                chunk_frames,
                momentary_window,
                short_term_window,
                hop,
            );
            self.finish_cuda_true_peak(planar);
            return result;
        }
        // True-peak interpolation and loudness/RMS accumulation have no
        // shared mutable state. For a long stereo chunk, let the global pool
        // advance both exact peak meters while this worker retains the
        // established K-weighting, window, and gating order. Short decoder
        // packets keep the fused loop so task coordination cannot dominate.
        // At 192 kHz and above True Peak is the sample peak already collected
        // by the loudness pass, so a second task would only duplicate work.
        if self.timeline_interval_frames.is_none()
            && planar.len() == 2
            && self.sample_rate < 192_000
            && chunk_frames >= MIN_PARALLEL_TRUE_PEAK_FRAMES
            && rayon::current_num_threads() > 1
        {
            let mut true_peak_meters = std::mem::take(&mut self.true_peak_meters);
            let ((), result) = rayon::join(
                || process_true_peak_channel_group(&mut true_peak_meters, planar),
                || {
                    self.process_without_true_peak(
                        planar,
                        chunk_frames,
                        momentary_window,
                        short_term_window,
                        hop,
                    )
                },
            );
            self.true_peak_meters = true_peak_meters;
            return result;
        }
        // Stereo is the dominant file-delivery layout. Keeping both filters in
        // persistent SIMD lanes and both true-peak meters borrowed outside the
        // frame loop removes the dynamic channel iterator and per-frame state
        // packing.
        // The arithmetic and window-update order intentionally matches the
        // generic path below; no temporary PCM or energy buffer is introduced.
        if self.timeline_interval_frames.is_none() && planar.len() == 2 {
            let weight0 = channel_weight(self.roles[0]);
            let weight1 = channel_weight(self.roles[1]);
            #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
            let kweight_pair = self
                .kweight_pair
                .as_mut()
                .expect("non-timeline stereo analyzer owns paired K-weighting state");
            #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
            let (filter0, filter1) = self.filters.split_at_mut(1);
            #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
            let filter0 = &mut filter0[0];
            #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
            let filter1 = &mut filter1[0];
            let (meter0, meter1) = self.true_peak_meters.split_at_mut(1);
            let meter0 = &mut meter0[0];
            let meter1 = &mut meter1[0];
            let skip_meter0 = meter0.try_skip_peak_only_block(&planar[0]);
            let skip_meter1 = meter1.try_skip_peak_only_block(&planar[1]);
            #[allow(
                clippy::needless_range_loop,
                reason = "the measured hot path uses one frame index across two fixed channels"
            )]
            for frame in 0..chunk_frames {
                let sample0 = planar[0][frame];
                let sample1 = planar[1][frame];
                match (skip_meter0, skip_meter1) {
                    (false, false) => {
                        TruePeakMeter::process_stereo_peak_only_sample(
                            meter0, meter1, sample0, sample1,
                        );
                    }
                    (false, true) => {
                        meter0.process_peak_only_sample(sample0);
                    }
                    (true, false) => {
                        meter1.process_peak_only_sample(sample1);
                    }
                    (true, true) => {}
                }
                #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
                let filtered = kweight_pair.process([sample0, sample1]);
                #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
                let filtered0 = filtered[0] as f64;
                #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
                let filtered1 = filtered[1] as f64;
                #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
                let filtered0 = filter0.process(sample0) as f64;
                #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
                let filtered1 = filter1.process(sample1) as f64;
                let mut weighted = 0.0;
                weighted += weight0 * filtered0 * filtered0;
                weighted += weight1 * filtered1 * filtered1;
                let raw0 = sample0 as f64;
                self.raw_sum_squares += raw0 * raw0;
                self.sample_peak = self.sample_peak.max(sample0.abs());
                let raw1 = sample1 as f64;
                self.raw_sum_squares += raw1 * raw1;
                self.sample_peak = self.sample_peak.max(sample1.abs());
                self.weighted_sum_squares += weighted;
                self.windows
                    .push(&mut self.momentary_sum, &mut self.short_term_sum, weighted);
                self.frames += 1;
                if self.windows.momentary_len() == momentary_window {
                    self.max_momentary_sum = self.max_momentary_sum.max(self.momentary_sum);
                }
                if self.windows.short_term_len() == short_term_window {
                    self.max_short_term_sum = self.max_short_term_sum.max(self.short_term_sum);
                }
                if self.frames == self.next_momentary_block_frame {
                    if self.gating_blocks.len() == MAX_LOUDNESS_BLOCKS {
                        return Err(format!(
                            "loudness analysis exceeds the {MAX_LOUDNESS_BLOCKS}-gating-block limit"
                        ));
                    }
                    self.gating_blocks
                        .push(self.momentary_sum / momentary_window as f64);
                    self.next_momentary_block_frame =
                        self.next_momentary_block_frame.saturating_add(hop);
                }
                if self.frames == self.next_short_term_block_frame {
                    if self.short_term_blocks.len() == MAX_LOUDNESS_BLOCKS {
                        return Err(format!(
                            "loudness analysis exceeds the {MAX_LOUDNESS_BLOCKS}-short-term-block limit"
                        ));
                    }
                    self.short_term_blocks
                        .push(self.short_term_sum / short_term_window as f64);
                    self.next_short_term_block_frame =
                        self.next_short_term_block_frame.saturating_add(hop);
                }
            }
            return Ok(());
        }
        // Multichannel delivery has enough independent true-peak states to
        // amortize a separate, channel-contiguous pass. Advancing adjacent
        // meters in pairs shares their immutable interpolation coefficients;
        // K-weighted energy keeps the established frame/channel reduction
        // order below, so every reported value remains bit-identical.
        if self.timeline_interval_frames.is_none() && planar.len() >= 4 {
            if chunk_frames >= MIN_PARALLEL_TRUE_PEAK_FRAMES && rayon::current_num_threads() > 1 {
                self.true_peak_meters
                    .par_chunks_mut(2)
                    .zip(planar.par_chunks(2))
                    .for_each(|(meters, channels)| {
                        process_true_peak_channel_group(meters, channels);
                    });
            } else {
                self.true_peak_meters
                    .chunks_mut(2)
                    .zip(planar.chunks(2))
                    .for_each(|(meters, channels)| {
                        process_true_peak_channel_group(meters, channels);
                    });
            }
            for frame in 0..chunk_frames {
                let weighted = process_kweighted_frame_multichannel(
                    &mut self.filters,
                    #[cfg(target_arch = "x86_64")]
                    &mut self.kweight_quads,
                    &self.roles,
                    planar,
                    frame,
                    &mut self.raw_sum_squares,
                    &mut self.sample_peak,
                );
                self.weighted_sum_squares += weighted;
                self.windows
                    .push(&mut self.momentary_sum, &mut self.short_term_sum, weighted);
                self.frames += 1;
                if self.windows.momentary_len() == momentary_window {
                    self.max_momentary_sum = self.max_momentary_sum.max(self.momentary_sum);
                }
                if self.windows.short_term_len() == short_term_window {
                    self.max_short_term_sum = self.max_short_term_sum.max(self.short_term_sum);
                }
                if self.frames == self.next_momentary_block_frame {
                    if self.gating_blocks.len() == MAX_LOUDNESS_BLOCKS {
                        return Err(format!(
                            "loudness analysis exceeds the {MAX_LOUDNESS_BLOCKS}-gating-block limit"
                        ));
                    }
                    self.gating_blocks
                        .push(self.momentary_sum / momentary_window as f64);
                    self.next_momentary_block_frame =
                        self.next_momentary_block_frame.saturating_add(hop);
                }
                if self.frames == self.next_short_term_block_frame {
                    if self.short_term_blocks.len() == MAX_LOUDNESS_BLOCKS {
                        return Err(format!(
                            "loudness analysis exceeds the {MAX_LOUDNESS_BLOCKS}-short-term-block limit"
                        ));
                    }
                    self.short_term_blocks
                        .push(self.short_term_sum / short_term_window as f64);
                    self.next_short_term_block_frame =
                        self.next_short_term_block_frame.saturating_add(hop);
                }
            }
            return Ok(());
        }
        for frame in 0..chunk_frames {
            let mut weighted = 0.0;
            for ((index, channel), filter) in planar.iter().enumerate().zip(self.filters.iter_mut())
            {
                let sample = channel[frame];
                let reconstructed_peak = self.true_peak_meters[index].process_sample(sample);
                self.interval_true_peak = self.interval_true_peak.max(reconstructed_peak);
                self.interval_sample_peak = self.interval_sample_peak.max(sample.abs());
                let filtered = filter.process(sample) as f64;
                weighted += channel_weight(self.roles[index]) * filtered * filtered;
                let raw = sample as f64;
                self.raw_sum_squares += raw * raw;
                self.sample_peak = self.sample_peak.max(sample.abs());
            }
            self.weighted_sum_squares += weighted;
            self.windows
                .push(&mut self.momentary_sum, &mut self.short_term_sum, weighted);
            self.frames += 1;
            if self.windows.momentary_len() == momentary_window {
                self.max_momentary_sum = self.max_momentary_sum.max(self.momentary_sum);
            }
            if self.windows.short_term_len() == short_term_window {
                self.max_short_term_sum = self.max_short_term_sum.max(self.short_term_sum);
            }
            if self.frames == self.next_momentary_block_frame {
                if self.gating_blocks.len() == MAX_LOUDNESS_BLOCKS {
                    return Err(format!(
                        "loudness analysis exceeds the {MAX_LOUDNESS_BLOCKS}-gating-block limit"
                    ));
                }
                self.gating_blocks
                    .push(self.momentary_sum / momentary_window as f64);
                self.next_momentary_block_frame =
                    self.next_momentary_block_frame.saturating_add(hop);
            }
            if self.frames == self.next_short_term_block_frame {
                if self.short_term_blocks.len() == MAX_LOUDNESS_BLOCKS {
                    return Err(format!(
                        "loudness analysis exceeds the {MAX_LOUDNESS_BLOCKS}-short-term-block limit"
                    ));
                }
                self.short_term_blocks
                    .push(self.short_term_sum / short_term_window as f64);
                self.next_short_term_block_frame =
                    self.next_short_term_block_frame.saturating_add(hop);
            }
            if self
                .timeline_interval_frames
                .is_some_and(|interval| self.frames.is_multiple_of(interval))
            {
                // Keep one slot for a final partial interval in `finish`.
                if self.timeline.len() >= MAX_LOUDNESS_TIMELINE_POINTS - 1 {
                    return Err(format!(
                        "loudness timeline exceeds the {MAX_LOUDNESS_TIMELINE_POINTS}-point limit"
                    ));
                }
                self.record_timeline_point(momentary_window, short_term_window);
            }
        }
        Ok(())
    }

    #[cfg(all(
        feature = "cuda-truepeak",
        any(target_os = "linux", target_os = "windows")
    ))]
    fn begin_cuda_true_peak(&mut self, planar: &[Vec<f32>], frames: usize) -> bool {
        let state = std::mem::replace(&mut self.cuda_true_peak, CudaTruePeakState::Disabled);
        match state {
            CudaTruePeakState::Disabled => false,
            CudaTruePeakState::Pending => {
                if frames == 0 {
                    self.cuda_true_peak = CudaTruePeakState::Pending;
                    return false;
                }
                if !CudaTruePeakWorker::eligible(self.sample_rate, planar.len(), frames) {
                    return false;
                }
                match CudaTruePeakWorker::new(self.sample_rate, planar.len(), frames) {
                    Ok(mut worker) => match worker.begin_chunk(planar) {
                        Ok(()) => {
                            self.cuda_true_peak = CudaTruePeakState::Active(Box::new(worker));
                            true
                        }
                        Err(error) => {
                            record_cuda_runtime_fallback(error);
                            self.true_peak_meters = worker.into_cpu_meters();
                            false
                        }
                    },
                    Err(error) => {
                        record_cuda_runtime_fallback(error);
                        false
                    }
                }
            }
            CudaTruePeakState::Active(mut worker) => match worker.begin_chunk(planar) {
                Ok(()) => {
                    self.cuda_true_peak = CudaTruePeakState::Active(worker);
                    true
                }
                Err(error) => {
                    record_cuda_runtime_fallback(error);
                    self.true_peak_meters = (*worker).into_cpu_meters();
                    false
                }
            },
        }
    }

    #[cfg(all(
        feature = "cuda-truepeak",
        any(target_os = "linux", target_os = "windows")
    ))]
    fn finish_cuda_true_peak(&mut self, planar: &[Vec<f32>]) {
        let state = std::mem::replace(&mut self.cuda_true_peak, CudaTruePeakState::Disabled);
        let CudaTruePeakState::Active(mut worker) = state else {
            return;
        };
        match worker.finish_chunk(planar) {
            Ok(()) => self.cuda_true_peak = CudaTruePeakState::Active(worker),
            Err(error) => {
                record_cuda_runtime_fallback(error);
                self.true_peak_meters = (*worker).into_cpu_meters();
                process_true_peak_cpu(&mut self.true_peak_meters, planar);
            }
        }
    }

    fn process_without_true_peak(
        &mut self,
        planar: &[Vec<f32>],
        chunk_frames: usize,
        momentary_window: usize,
        short_term_window: usize,
        hop: usize,
    ) -> Result<(), String> {
        if planar.len() == 2 {
            let weight0 = channel_weight(self.roles[0]);
            let weight1 = channel_weight(self.roles[1]);
            #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
            let kweight_pair = self
                .kweight_pair
                .as_mut()
                .expect("non-timeline stereo analyzer owns paired K-weighting state");
            #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
            let (filter0, filter1) = self.filters.split_at_mut(1);
            #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
            let filter0 = &mut filter0[0];
            #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
            let filter1 = &mut filter1[0];
            #[allow(
                clippy::needless_range_loop,
                reason = "the measured hot path uses one frame index across two fixed channels"
            )]
            for frame in 0..chunk_frames {
                let sample0 = planar[0][frame];
                let sample1 = planar[1][frame];
                #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
                let filtered = kweight_pair.process([sample0, sample1]);
                #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
                let filtered0 = filtered[0] as f64;
                #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
                let filtered1 = filtered[1] as f64;
                #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
                let filtered0 = filter0.process(sample0) as f64;
                #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
                let filtered1 = filter1.process(sample1) as f64;
                let mut weighted = 0.0;
                weighted += weight0 * filtered0 * filtered0;
                weighted += weight1 * filtered1 * filtered1;
                let raw0 = sample0 as f64;
                self.raw_sum_squares += raw0 * raw0;
                self.sample_peak = self.sample_peak.max(sample0.abs());
                let raw1 = sample1 as f64;
                self.raw_sum_squares += raw1 * raw1;
                self.sample_peak = self.sample_peak.max(sample1.abs());
                self.weighted_sum_squares += weighted;
                self.windows
                    .push(&mut self.momentary_sum, &mut self.short_term_sum, weighted);
                self.frames += 1;
                if self.windows.momentary_len() == momentary_window {
                    self.max_momentary_sum = self.max_momentary_sum.max(self.momentary_sum);
                }
                if self.windows.short_term_len() == short_term_window {
                    self.max_short_term_sum = self.max_short_term_sum.max(self.short_term_sum);
                }
                if self.frames == self.next_momentary_block_frame {
                    if self.gating_blocks.len() == MAX_LOUDNESS_BLOCKS {
                        return Err(format!(
                            "loudness analysis exceeds the {MAX_LOUDNESS_BLOCKS}-gating-block limit"
                        ));
                    }
                    self.gating_blocks
                        .push(self.momentary_sum / momentary_window as f64);
                    self.next_momentary_block_frame =
                        self.next_momentary_block_frame.saturating_add(hop);
                }
                if self.frames == self.next_short_term_block_frame {
                    if self.short_term_blocks.len() == MAX_LOUDNESS_BLOCKS {
                        return Err(format!(
                            "loudness analysis exceeds the {MAX_LOUDNESS_BLOCKS}-short-term-block limit"
                        ));
                    }
                    self.short_term_blocks
                        .push(self.short_term_sum / short_term_window as f64);
                    self.next_short_term_block_frame =
                        self.next_short_term_block_frame.saturating_add(hop);
                }
            }
            return Ok(());
        }

        for frame in 0..chunk_frames {
            let weighted = process_kweighted_frame_multichannel(
                &mut self.filters,
                #[cfg(target_arch = "x86_64")]
                &mut self.kweight_quads,
                &self.roles,
                planar,
                frame,
                &mut self.raw_sum_squares,
                &mut self.sample_peak,
            );
            self.push_weighted_frame(weighted, momentary_window, short_term_window, hop)?;
        }
        Ok(())
    }

    #[inline(always)]
    fn push_weighted_frame(
        &mut self,
        weighted: f64,
        momentary_window: usize,
        short_term_window: usize,
        hop: usize,
    ) -> Result<(), String> {
        self.weighted_sum_squares += weighted;
        self.windows
            .push(&mut self.momentary_sum, &mut self.short_term_sum, weighted);
        self.frames += 1;
        if self.windows.momentary_len() == momentary_window {
            self.max_momentary_sum = self.max_momentary_sum.max(self.momentary_sum);
        }
        if self.windows.short_term_len() == short_term_window {
            self.max_short_term_sum = self.max_short_term_sum.max(self.short_term_sum);
        }
        if self.frames == self.next_momentary_block_frame {
            if self.gating_blocks.len() == MAX_LOUDNESS_BLOCKS {
                return Err(format!(
                    "loudness analysis exceeds the {MAX_LOUDNESS_BLOCKS}-gating-block limit"
                ));
            }
            self.gating_blocks
                .push(self.momentary_sum / momentary_window as f64);
            self.next_momentary_block_frame = self.next_momentary_block_frame.saturating_add(hop);
        }
        if self.frames == self.next_short_term_block_frame {
            if self.short_term_blocks.len() == MAX_LOUDNESS_BLOCKS {
                return Err(format!(
                    "loudness analysis exceeds the {MAX_LOUDNESS_BLOCKS}-short-term-block limit"
                ));
            }
            self.short_term_blocks
                .push(self.short_term_sum / short_term_window as f64);
            self.next_short_term_block_frame = self.next_short_term_block_frame.saturating_add(hop);
        }
        Ok(())
    }

    pub fn finish(mut self) -> StreamingMeasurements {
        if self.timeline_interval_frames.is_some() && self.timeline_start_frame < self.frames {
            let momentary_window = ((self.sample_rate as usize * 4) / 10).max(1);
            let short_term_window = (self.sample_rate as usize * 3).max(1);
            self.record_timeline_point(momentary_window, short_term_window);
        }
        let channels = self.roles.len();
        let total_samples = self.frames * channels;
        let rms = if total_samples == 0 {
            0.0
        } else {
            (self.raw_sum_squares / total_samples as f64).sqrt()
        };
        let cpu_true_peak = self
            .true_peak_meters
            .iter()
            .map(TruePeakMeter::peak)
            .fold(0.0, f32::max);
        #[cfg(all(
            feature = "cuda-truepeak",
            any(target_os = "linux", target_os = "windows")
        ))]
        let true_peak = match &self.cuda_true_peak {
            CudaTruePeakState::Active(worker) => worker.peak(),
            CudaTruePeakState::Disabled | CudaTruePeakState::Pending => cpu_true_peak,
        };
        #[cfg(not(all(
            feature = "cuda-truepeak",
            any(target_os = "linux", target_os = "windows")
        )))]
        let true_peak = cpu_true_peak;
        let mut ebu = measurements_from_blocks(self.gating_blocks, &self.short_term_blocks);
        let momentary_window = ((self.sample_rate as usize * 4) / 10).max(1);
        let short_term_window = (self.sample_rate as usize * 3).max(1);
        ebu.max_momentary_lufs =
            maximum_loudness(&[self.max_momentary_sum / momentary_window as f64]);
        ebu.max_short_term_lufs =
            maximum_loudness(&[self.max_short_term_sum / short_term_window as f64]);
        StreamingMeasurements {
            ebu,
            frames: self.frames,
            weighted_mean_square: if self.frames == 0 {
                0.0
            } else {
                self.weighted_sum_squares / self.frames as f64
            },
            rms_db: if rms > 0.0 {
                20.0 * rms.log10()
            } else {
                f64::NEG_INFINITY
            },
            sample_peak: self.sample_peak,
            true_peak,
            timeline: self.timeline,
        }
    }

    fn record_timeline_point(&mut self, momentary_window: usize, short_term_window: usize) {
        self.timeline.push(LoudnessTimelinePoint {
            start_seconds: self.timeline_start_frame as f64 / self.sample_rate as f64,
            end_seconds: self.frames as f64 / self.sample_rate as f64,
            momentary_lufs: complete_window_loudness(
                self.momentary_sum,
                self.windows.momentary_len(),
                momentary_window,
            ),
            short_term_lufs: complete_window_loudness(
                self.short_term_sum,
                self.windows.short_term_len(),
                short_term_window,
            ),
            sample_peak_dbfs: amplitude_db(self.interval_sample_peak),
            true_peak_dbtp: amplitude_db(self.interval_true_peak),
        });
        self.timeline_start_frame = self.frames;
        self.interval_sample_peak = 0.0;
        self.interval_true_peak = 0.0;
    }
}

#[inline]
fn process_true_peak_channel_group(meters: &mut [TruePeakMeter], channels: &[Vec<f32>]) {
    debug_assert_eq!(meters.len(), channels.len());
    if meters.len() == 2 {
        let (left_meter, right_meter) = meters.split_at_mut(1);
        let skip_left = left_meter[0].try_skip_peak_only_block(&channels[0]);
        let skip_right = right_meter[0].try_skip_peak_only_block(&channels[1]);
        for (&left_sample, &right_sample) in channels[0].iter().zip(&channels[1]) {
            match (skip_left, skip_right) {
                (false, false) => {
                    TruePeakMeter::process_stereo_peak_only_sample(
                        &mut left_meter[0],
                        &mut right_meter[0],
                        left_sample,
                        right_sample,
                    );
                }
                (false, true) => {
                    left_meter[0].process_peak_only_sample(left_sample);
                }
                (true, false) => {
                    right_meter[0].process_peak_only_sample(right_sample);
                }
                (true, true) => {}
            }
        }
    } else if let (Some(meter), Some(channel)) = (meters.first_mut(), channels.first()) {
        meter.process(channel);
    }
}

/// Advance four channel-contiguous K-weighting states without a planar scratch
/// pass. Filter states are independent, while weighted and raw reductions
/// retain the established left-to-right channel order exactly.
#[inline(always)]
fn process_kweighted_frame_multichannel(
    filters: &mut [KWeight],
    #[cfg(target_arch = "x86_64")] kweight_quads: &mut Option<Vec<KWeightQuad>>,
    roles: &[ChannelRole],
    planar: &[Vec<f32>],
    frame: usize,
    raw_sum_squares: &mut f64,
    sample_peak: &mut f32,
) -> f64 {
    debug_assert_eq!(filters.len(), roles.len());
    debug_assert_eq!(filters.len(), planar.len());
    let mut weighted = 0.0;
    #[cfg(target_arch = "x86_64")]
    if let Some(quads) = kweight_quads.as_mut() {
        let mut channel = 0;
        for quad in quads {
            let input = [
                planar[channel][frame],
                planar[channel + 1][frame],
                planar[channel + 2][frame],
                planar[channel + 3][frame],
            ];
            let filtered = quad.process(input);
            for lane in 0..4 {
                let lane_filtered = filtered[lane] as f64;
                weighted += channel_weight(roles[channel + lane]) * lane_filtered * lane_filtered;
                let raw = input[lane] as f64;
                *raw_sum_squares += raw * raw;
                *sample_peak = sample_peak.max(input[lane].abs());
            }
            channel += 4;
        }
        for index in channel..filters.len() {
            let sample = planar[index][frame];
            let filtered = filters[index].process(sample) as f64;
            weighted += channel_weight(roles[index]) * filtered * filtered;
            let raw = sample as f64;
            *raw_sum_squares += raw * raw;
            *sample_peak = sample_peak.max(sample.abs());
        }
        return weighted;
    }
    for ((index, channel), filter) in planar.iter().enumerate().zip(filters.iter_mut()) {
        let sample = channel[frame];
        let filtered = filter.process(sample) as f64;
        weighted += channel_weight(roles[index]) * filtered * filtered;
        let raw = sample as f64;
        *raw_sum_squares += raw * raw;
        *sample_peak = sample_peak.max(sample.abs());
    }
    weighted
}

#[cfg(all(
    feature = "cuda-truepeak",
    any(target_os = "linux", target_os = "windows")
))]
fn process_true_peak_cpu(meters: &mut [TruePeakMeter], planar: &[Vec<f32>]) {
    let frames = planar.first().map_or(0, Vec::len);
    if meters.len() >= 4
        && frames >= MIN_PARALLEL_TRUE_PEAK_FRAMES
        && rayon::current_num_threads() > 1
    {
        meters.par_chunks_mut(2).zip(planar.par_chunks(2)).for_each(
            |(meter_group, channel_group)| {
                process_true_peak_channel_group(meter_group, channel_group);
            },
        );
    } else {
        meters
            .chunks_mut(2)
            .zip(planar.chunks(2))
            .for_each(|(meter_group, channel_group)| {
                process_true_peak_channel_group(meter_group, channel_group);
            });
    }
}

fn complete_window_loudness(sum: f64, length: usize, required: usize) -> Option<f64> {
    (length == required && sum > 0.0).then(|| mean_square_to_lufs(sum / required as f64))
}

fn amplitude_db(amplitude: f32) -> f64 {
    if amplitude > 0.0 {
        20.0 * (amplitude as f64).log10()
    } else {
        f64::NEG_INFINITY
    }
}

/// Per-channel loudness weight (BS.1770).
pub fn channel_weight(role: ChannelRole) -> f64 {
    match role {
        ChannelRole::Main => 1.0,
        ChannelRole::Surround => 1.41,
        ChannelRole::DualMono => 2.0,
        ChannelRole::Positioned {
            azimuth_degrees,
            elevation_degrees,
        } => {
            let azimuth = azimuth_degrees.unsigned_abs();
            let elevation = elevation_degrees.unsigned_abs();
            if elevation < 30 && (60..=120).contains(&azimuth) {
                1.41
            } else {
                1.0
            }
        }
        ChannelRole::Lfe => 0.0,
    }
}

/// Integrated gated loudness in LUFS, or `-inf` for silence.
pub fn measure_lufs(buf: &AudioBuffer) -> f64 {
    measure_ebu(buf).integrated_lufs
}

/// Weighted mean-square energies for every complete 400 ms gating block.
pub fn measure_blocks(buf: &AudioBuffer) -> Vec<f64> {
    measure_ebu(buf).gating_blocks
}

/// Complete EBU Mode file measurement.
pub fn measure_ebu(buf: &AudioBuffer) -> EbuMeasurements {
    let fs = buf.sample_rate as usize;
    let momentary_window = (0.4 * fs as f64).round() as usize;
    let short_term_window = (3.0 * fs as f64).round() as usize;
    let hop = (0.1 * fs as f64).round() as usize;
    if momentary_window == 0 || hop == 0 || buf.frames < momentary_window {
        return EbuMeasurements {
            integrated_lufs: f64::NEG_INFINITY,
            max_momentary_lufs: f64::NEG_INFINITY,
            max_short_term_lufs: f64::NEG_INFINITY,
            loudness_range_lu: 0.0,
            gating_blocks: Vec::new(),
        };
    }

    // K-weight each channel (parallel) and build prefix sums of squares.
    let prefixes: Vec<Vec<f64>> = buf
        .data
        .par_iter()
        .map(|ch| {
            let mut kw = KWeight::for_sample_rate(buf.sample_rate);
            let mut filt = vec![0.0f32; ch.len()];
            kw.process_block(ch, &mut filt);
            let mut p = Vec::with_capacity(ch.len() + 1);
            p.push(0.0);
            let mut acc = 0.0f64;
            for &x in &filt {
                let v = x as f64;
                acc += v * v;
                p.push(acc);
            }
            p
        })
        .collect();

    let weights: Vec<f64> = (0..buf.channels as usize)
        .map(|index| channel_weight(buf.channel_role(index)))
        .collect();

    let gating_blocks = window_mean_squares(&prefixes, &weights, buf.frames, momentary_window, hop);
    let short_term_blocks =
        window_mean_squares(&prefixes, &weights, buf.frames, short_term_window, hop);

    let mut measurements = measurements_from_blocks(gating_blocks, &short_term_blocks);
    measurements.max_momentary_lufs =
        maximum_window_loudness(&prefixes, &weights, buf.frames, momentary_window);
    measurements.max_short_term_lufs =
        maximum_window_loudness(&prefixes, &weights, buf.frames, short_term_window);
    measurements
}

fn maximum_window_loudness(
    prefixes: &[Vec<f64>],
    weights: &[f64],
    frames: usize,
    window: usize,
) -> f64 {
    if window == 0 || frames < window {
        return f64::NEG_INFINITY;
    }
    let mut maximum_sum = 0.0_f64;
    for start in 0..=frames - window {
        let mut total = 0.0;
        for channel in 0..prefixes.len() {
            if weights[channel] != 0.0 {
                total += weights[channel]
                    * (prefixes[channel][start + window] - prefixes[channel][start]);
            }
        }
        maximum_sum = maximum_sum.max(total);
    }
    maximum_loudness(&[maximum_sum / window as f64])
}

fn measurements_from_blocks(gating_blocks: Vec<f64>, short_term_blocks: &[f64]) -> EbuMeasurements {
    EbuMeasurements {
        integrated_lufs: gated_lufs(&gating_blocks),
        max_momentary_lufs: maximum_loudness(&gating_blocks),
        max_short_term_lufs: maximum_loudness(short_term_blocks),
        loudness_range_lu: loudness_range(short_term_blocks),
        gating_blocks,
    }
}

fn window_mean_squares(
    prefixes: &[Vec<f64>],
    weights: &[f64],
    frames: usize,
    window: usize,
    hop: usize,
) -> Vec<f64> {
    if window == 0 || hop == 0 || frames < window {
        return Vec::new();
    }
    let mut means = Vec::new();
    let mut b = 0usize;
    while b + window <= frames {
        let mut total = 0.0f64;
        for c in 0..prefixes.len() {
            let w = weights[c];
            if w == 0.0 {
                continue;
            }
            let ss = prefixes[c][b + window] - prefixes[c][b];
            total += w * ss;
        }
        means.push(total / window as f64);
        b += hop;
    }
    means
}

/// Apply the BS.1770 absolute and relative gates to a population of blocks.
pub fn gated_lufs(block_ms: &[f64]) -> f64 {
    if block_ms.is_empty() {
        return f64::NEG_INFINITY;
    }

    let abs_gate_ms = 10.0_f64.powf((-70.0 + 0.691) / 10.0);
    let abs_gated: Vec<f64> = block_ms
        .iter()
        .copied()
        .filter(|&m| m >= abs_gate_ms)
        .collect();
    if abs_gated.is_empty() {
        return f64::NEG_INFINITY;
    }
    let mean_ms: f64 = abs_gated.iter().sum::<f64>() / abs_gated.len() as f64;
    let rel_gate_ms = mean_ms / 10.0; // -10 dB in the linear domain
    let gate = abs_gate_ms.max(rel_gate_ms);
    let final_set: Vec<f64> = block_ms.iter().copied().filter(|&m| m >= gate).collect();
    let used = if final_set.is_empty() {
        mean_ms
    } else {
        final_set.iter().sum::<f64>() / final_set.len() as f64
    };
    -0.691 + 10.0 * used.log10()
}

/// Convert an ungated K/channel-weighted mean square to LKFS/LUFS.
pub fn ungated_lufs(mean_square: f64) -> f64 {
    if mean_square > 0.0 {
        mean_square_to_lufs(mean_square)
    } else {
        f64::NEG_INFINITY
    }
}

fn maximum_loudness(blocks: &[f64]) -> f64 {
    blocks
        .iter()
        .copied()
        .filter(|value| *value > 0.0)
        .map(mean_square_to_lufs)
        .fold(f64::NEG_INFINITY, f64::max)
}

/// Loudness Range per EBU Tech 3342.
pub fn loudness_range(short_term_ms: &[f64]) -> f64 {
    let abs_gate_ms = 10.0_f64.powf((-70.0 + 0.691) / 10.0);
    let absolute: Vec<f64> = short_term_ms
        .iter()
        .copied()
        .filter(|value| *value >= abs_gate_ms)
        .collect();
    if absolute.is_empty() {
        return 0.0;
    }

    let absolute_mean = absolute.iter().sum::<f64>() / absolute.len() as f64;
    let relative_gate = absolute_mean / 100.0; // -20 LU
    let mut gated: Vec<f64> = absolute
        .into_iter()
        .filter(|value| *value >= relative_gate)
        .map(mean_square_to_lufs)
        .collect();
    if gated.len() < 2 {
        return 0.0;
    }
    gated.sort_by(f64::total_cmp);
    percentile(&gated, 0.95) - percentile(&gated, 0.10)
}

fn mean_square_to_lufs(value: f64) -> f64 {
    -0.691 + 10.0 * value.log10()
}

fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    let position = fraction * (sorted.len() - 1) as f64;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    let mix = position - lower as f64;
    sorted[lower] * (1.0 - mix) + sorted[upper] * mix
}

/// RMS level (dBFS) and sample peak (0..1) across all channels, computed in
/// parallel with SIMD primitives.
pub fn measure_rms_peak(buf: &AudioBuffer) -> (f64, f32) {
    let (sumsq, peak) = buf
        .data
        .par_iter()
        .map(|ch| (simd::sum_squares_f64(ch), simd::abs_max(ch)))
        .reduce(|| (0.0f64, 0.0f32), |(s, p), (s2, p2)| (s + s2, p.max(p2)));
    let total = (buf.frames as f64) * (buf.channels as f64);
    let rms = if total > 0.0 {
        (sumsq / total).sqrt()
    } else {
        0.0
    };
    let rms_db = if rms > 0.0 {
        20.0 * rms.log10()
    } else {
        f64::NEG_INFINITY
    };
    (rms_db, peak)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::PcmKind;
    use std::collections::VecDeque;

    fn mono(samples: Vec<f32>, sample_rate: u32) -> AudioBuffer {
        AudioBuffer {
            sample_rate,
            channels: 1,
            frames: samples.len(),
            data: vec![samples],
            channel_roles: vec![ChannelRole::Main],
            source_kind: PcmKind::F32,
        }
    }

    #[test]
    fn shared_loudness_windows_match_two_vecdeque_sums_bit_exactly() {
        for (momentary_limit, short_term_limit) in [(1, 1), (1, 7), (2, 7), (7, 31), (31, 127)] {
            let mut candidate = LoudnessWindows::new(momentary_limit, short_term_limit);
            let mut candidate_momentary_sum = 0.0;
            let mut candidate_short_term_sum = 0.0;
            let mut reference_momentary = VecDeque::new();
            let mut reference_short_term = VecDeque::new();
            let mut reference_momentary_sum = 0.0;
            let mut reference_short_term_sum = 0.0;
            for index in 0_usize..4_097 {
                let value =
                    ((index.wrapping_mul(97).wrapping_add(31) % 1_009) as f64 - 504.0) / 1_009.0;
                candidate.push(
                    &mut candidate_momentary_sum,
                    &mut candidate_short_term_sum,
                    value,
                );
                reference_momentary.push_back(value);
                reference_momentary_sum += value;
                if reference_momentary.len() > momentary_limit {
                    reference_momentary_sum -= reference_momentary.pop_front().unwrap();
                }
                reference_short_term.push_back(value);
                reference_short_term_sum += value;
                if reference_short_term.len() > short_term_limit {
                    reference_short_term_sum -= reference_short_term.pop_front().unwrap();
                }
                assert_eq!(candidate.momentary_len(), reference_momentary.len());
                assert_eq!(candidate.short_term_len(), reference_short_term.len());
                assert_eq!(
                    candidate_momentary_sum.to_bits(),
                    reference_momentary_sum.to_bits()
                );
                assert_eq!(
                    candidate_short_term_sum.to_bits(),
                    reference_short_term_sum.to_bits()
                );
            }
        }
    }

    #[test]
    fn deferred_maximum_division_is_bit_exact() {
        for window in [1_usize, 400, 19_200, 144_000, 576_000] {
            let divisor = window as f64;
            let mut state = 0x6a09_e667_f3bc_c909_u64;
            let mut divided_maximum = 0.0_f64;
            let mut sum_maximum = 0.0_f64;
            for index in 0..100_000_u64 {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let mantissa = state >> 11;
                let scale = f64::from(((index % 23) + 1) as u32);
                let sum = mantissa as f64 * scale / (1_u64 << 30) as f64;
                divided_maximum = divided_maximum.max(sum / divisor);
                sum_maximum = sum_maximum.max(sum);
            }
            assert_eq!(
                divided_maximum.to_bits(),
                (sum_maximum / divisor).to_bits(),
                "window {window}"
            );
        }
    }

    #[test]
    fn incomplete_trailing_hop_does_not_create_a_block() {
        let sr = 1_000;
        let mut samples = vec![0.1; 400];
        samples.extend(vec![1.0; 50]);
        let with_tail = mono(samples, sr);
        let complete_only = mono(vec![0.1; 400], sr);

        assert_eq!(measure_blocks(&with_tail).len(), 1);
        assert_eq!(measure_lufs(&with_tail), measure_lufs(&complete_only));
    }

    #[test]
    fn channel_roles_select_bs1770_weights() {
        assert_eq!(channel_weight(ChannelRole::Main), 1.0);
        assert_eq!(channel_weight(ChannelRole::Surround), 1.41);
        assert_eq!(channel_weight(ChannelRole::DualMono), 2.0);
        assert_eq!(
            channel_weight(ChannelRole::positioned(-90, 0)),
            1.41,
            "side channels receive the Annex 3 +1.5 dB weighting"
        );
        assert_eq!(
            channel_weight(ChannelRole::positioned(-135, 0)),
            1.0,
            "rear channels outside ±120 degrees use unity weighting"
        );
        assert_eq!(
            channel_weight(ChannelRole::positioned(-90, 45)),
            1.0,
            "elevated channels use unity weighting"
        );
        assert_eq!(channel_weight(ChannelRole::Lfe), 0.0);
    }

    #[test]
    fn dual_mono_adds_the_two_speaker_pan_law() {
        let samples: Vec<f32> = (0..48_000)
            .map(|index| ((index as f64 * 0.13).sin() * 0.1) as f32)
            .collect();
        let ordinary = mono(samples.clone(), 48_000);
        let mut dual = mono(samples, 48_000);
        dual.channel_roles[0] = ChannelRole::DualMono;
        let difference = measure_lufs(&dual) - measure_lufs(&ordinary);
        assert!((difference - 10.0 * 2.0_f64.log10()).abs() < 1e-9);
    }

    #[test]
    fn loudness_range_uses_tenth_and_ninety_fifth_percentiles() {
        let blocks: Vec<f64> = (0..=100)
            .map(|step| {
                let lufs = -30.0 + step as f64 / 10.0;
                10.0_f64.powf((lufs + 0.691) / 10.0)
            })
            .collect();
        let range = loudness_range(&blocks);
        assert!((range - 8.5).abs() < 0.01, "LRA = {range}");
    }

    #[test]
    fn streaming_measurement_matches_whole_buffer() {
        let samples: Vec<f32> = (0..192_000)
            .map(|index| ((index as f64 * 0.071).sin() * 0.3) as f32)
            .collect();
        let buffer = mono(samples.clone(), 48_000);
        let whole_ebu = measure_ebu(&buffer);
        let (whole_rms, whole_peak) = measure_rms_peak(&buffer);
        let whole_true_peak = crate::dsp::truepeak::measure_true_peak(&buffer);

        let mut streaming = StreamingAnalyzer::new(48_000, vec![ChannelRole::Main]);
        for chunk in samples.chunks(137) {
            streaming.process(&[chunk.to_vec()]).unwrap();
        }
        let streamed = streaming.finish();

        assert!(
            (streamed.ebu.integrated_lufs - whole_ebu.integrated_lufs).abs() < 1e-6,
            "streamed={}, whole={}",
            streamed.ebu.integrated_lufs,
            whole_ebu.integrated_lufs
        );
        assert!((streamed.ebu.max_momentary_lufs - whole_ebu.max_momentary_lufs).abs() < 1e-6);
        assert!((streamed.ebu.max_short_term_lufs - whole_ebu.max_short_term_lufs).abs() < 1e-6);
        assert!((streamed.ebu.loudness_range_lu - whole_ebu.loudness_range_lu).abs() < 1e-6);
        assert!((streamed.rms_db - whole_rms).abs() < 1e-9);
        assert_eq!(streamed.sample_peak, whole_peak);
        assert_eq!(streamed.true_peak, whole_true_peak);
    }

    #[test]
    fn stereo_streaming_measurement_matches_whole_buffer() {
        let left: Vec<f32> = (0..192_137)
            .map(|index| ((index as f64 * 0.071).sin() * 0.3) as f32)
            .collect();
        let right: Vec<f32> = (0..192_137)
            .map(|index| ((index as f64 * 0.113).cos() * 0.2) as f32)
            .collect();
        let roles = vec![ChannelRole::Main, ChannelRole::Surround];
        let buffer = AudioBuffer {
            sample_rate: 48_000,
            channels: 2,
            frames: left.len(),
            data: vec![left.clone(), right.clone()],
            channel_roles: roles.clone(),
            source_kind: PcmKind::F32,
        };
        let whole_ebu = measure_ebu(&buffer);
        let (whole_rms, whole_peak) = measure_rms_peak(&buffer);
        let whole_true_peak = crate::dsp::truepeak::measure_true_peak(&buffer);

        let mut streaming = StreamingAnalyzer::new(48_000, roles);
        for start in (0..left.len()).step_by(137) {
            let end = (start + 137).min(left.len());
            streaming
                .process(&[left[start..end].to_vec(), right[start..end].to_vec()])
                .unwrap();
        }
        let streamed = streaming.finish();

        assert!((streamed.ebu.integrated_lufs - whole_ebu.integrated_lufs).abs() < 1e-6);
        assert!((streamed.ebu.max_momentary_lufs - whole_ebu.max_momentary_lufs).abs() < 1e-6);
        assert!((streamed.ebu.max_short_term_lufs - whole_ebu.max_short_term_lufs).abs() < 1e-6);
        assert!((streamed.ebu.loudness_range_lu - whole_ebu.loudness_range_lu).abs() < 1e-6);
        assert!((streamed.rms_db - whole_rms).abs() < 1e-9);
        assert_eq!(streamed.sample_peak, whole_peak);
        assert_eq!(streamed.true_peak, whole_true_peak);
    }

    #[test]
    fn stereo_parallel_true_peak_matches_fused_path_bit_exactly() {
        let frames = 192_137;
        let planar = vec![
            (0..frames)
                .map(|index| {
                    (((index as f64 * 0.071).sin() * 0.31)
                        + ((index as f64 * 0.000_31).cos() * 0.07)) as f32
                })
                .collect::<Vec<_>>(),
            (0..frames)
                .map(|index| {
                    (((index as f64 * 0.113).cos() * 0.23)
                        - ((index as f64 * 0.000_47).sin() * 0.05)) as f32
                })
                .collect::<Vec<_>>(),
        ];
        let roles = vec![ChannelRole::Main, ChannelRole::Surround];
        let measure = |threads| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap()
                .install(|| {
                    let mut analyzer = StreamingAnalyzer::new(48_000, roles.clone());
                    analyzer.process(&planar).unwrap();
                    analyzer.finish()
                })
        };

        // One worker forces the established fused loop; two workers meet the
        // long-chunk threshold and split True Peak from the loudness pass.
        let fused = measure(1);
        let parallel = measure(2);

        assert_eq!(parallel.ebu.integrated_lufs, fused.ebu.integrated_lufs);
        assert_eq!(
            parallel.ebu.max_momentary_lufs,
            fused.ebu.max_momentary_lufs
        );
        assert_eq!(
            parallel.ebu.max_short_term_lufs,
            fused.ebu.max_short_term_lufs
        );
        assert_eq!(parallel.ebu.loudness_range_lu, fused.ebu.loudness_range_lu);
        assert_eq!(parallel.ebu.gating_blocks, fused.ebu.gating_blocks);
        assert_eq!(parallel.frames, fused.frames);
        assert_eq!(parallel.weighted_mean_square, fused.weighted_mean_square);
        assert_eq!(parallel.rms_db, fused.rms_db);
        assert_eq!(parallel.sample_peak, fused.sample_peak);
        assert_eq!(parallel.true_peak, fused.true_peak);
        assert!(parallel.timeline.is_empty());
        assert!(fused.timeline.is_empty());
    }

    #[test]
    fn multichannel_streaming_measurement_matches_whole_buffer() {
        let frames = 192_137;
        for channels in [7, 8] {
            let data: Vec<Vec<f32>> = (0..channels)
                .map(|channel| {
                    (0..frames)
                        .map(|index| {
                            ((index as f64 * (0.041 + channel as f64 * 0.013)
                                + channel as f64 * 0.17)
                                .sin()
                                * (0.08 + channel as f64 * 0.02)) as f32
                        })
                        .collect()
                })
                .collect();
            let roles = (0..channels)
                .map(|channel| match channel {
                    3 => ChannelRole::Lfe,
                    4 | 5 => ChannelRole::Surround,
                    _ => ChannelRole::Main,
                })
                .collect::<Vec<_>>();
            let buffer = AudioBuffer {
                sample_rate: 48_000,
                channels: channels as u16,
                frames,
                data: data.clone(),
                channel_roles: roles.clone(),
                source_kind: PcmKind::F32,
            };
            let whole_ebu = measure_ebu(&buffer);
            let (whole_rms, whole_peak) = measure_rms_peak(&buffer);
            let whole_true_peak = crate::dsp::truepeak::measure_true_peak(&buffer);

            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(4)
                .build()
                .unwrap();
            for chunk_frames in [137, 20_000] {
                let streamed = pool.install(|| {
                    let mut streaming = StreamingAnalyzer::new(48_000, roles.clone());
                    for start in (0..frames).step_by(chunk_frames) {
                        let end = (start + chunk_frames).min(frames);
                        let chunk = data
                            .iter()
                            .map(|channel| channel[start..end].to_vec())
                            .collect::<Vec<_>>();
                        streaming.process(&chunk).unwrap();
                    }
                    streaming.finish()
                });

                assert!((streamed.ebu.integrated_lufs - whole_ebu.integrated_lufs).abs() < 1e-6);
                assert!(
                    (streamed.ebu.max_momentary_lufs - whole_ebu.max_momentary_lufs).abs() < 1e-6
                );
                assert!(
                    (streamed.ebu.max_short_term_lufs - whole_ebu.max_short_term_lufs).abs() < 1e-6
                );
                assert!(
                    (streamed.ebu.loudness_range_lu - whole_ebu.loudness_range_lu).abs() < 1e-6
                );
                assert!((streamed.rms_db - whole_rms).abs() < 1e-9);
                assert_eq!(streamed.sample_peak, whole_peak);
                assert_eq!(streamed.true_peak, whole_true_peak);
            }
        }
    }

    #[test]
    fn timeline_uses_complete_windows_and_keeps_the_partial_interval() {
        let samples: Vec<f32> = (0..50_400)
            .map(|index| ((index as f64 * 0.13).sin() * 0.2) as f32)
            .collect();
        let mut analyzer =
            StreamingAnalyzer::with_timeline_interval(48_000, vec![ChannelRole::Main], Some(4_800));
        for chunk in samples.chunks(997) {
            analyzer.process(&[chunk.to_vec()]).unwrap();
        }
        let measured = analyzer.finish();
        assert_eq!(measured.timeline.len(), 11);
        assert_eq!(measured.timeline[0].start_seconds, 0.0);
        assert_eq!(measured.timeline[0].end_seconds, 0.1);
        assert!(measured.timeline[2].momentary_lufs.is_none());
        assert!(measured.timeline[3].momentary_lufs.is_some());
        assert!(measured
            .timeline
            .iter()
            .all(|point| point.short_term_lufs.is_none()));
        assert_eq!(measured.timeline[10].start_seconds, 1.0);
        assert_eq!(measured.timeline[10].end_seconds, 1.05);
        assert!(measured.timeline[10].true_peak_dbtp.is_finite());
    }
}
