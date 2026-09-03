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
//!   * A shared rolling-window pass derives overlapping blocks without
//!     cancellation-prone lifetime prefix subtraction.
//!   * Energy reductions use fixed-order compensated sums; the separate fast
//!     RMS path retains its established SIMD primitive.

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
use crate::dsp::pcm::{self, PlanarChunkMessages};
use crate::dsp::simd;
use crate::dsp::sum::CompensatedSum;
use crate::dsp::truepeak::{oversample_factor, TruePeakMeter, TAPS_PER_PHASE};
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
const LOUDNESS_CLOCK_TICKS_PER_SECOND: u128 = 10;
// `10^((-70 + 0.691) / 10)` committed as IEEE-754 bits so the gate does not
// depend on the platform `pow` implementation.
const ABSOLUTE_GATE_MEAN_SQUARE: f64 = f64::from_bits(0x3e7f_791e_c6e1_d5b7);
/// Reference reports are canonicalized to one nanodecibel. This is far below
/// any published meter tolerance while removing platform-libm last-bit drift.
pub(crate) const REFERENCE_DB_QUANTUM: f64 = 1e-9;

fn canonical_reference_db(value: f64) -> f64 {
    if !value.is_finite() {
        return value;
    }
    let rounded = (value / REFERENCE_DB_QUANTUM).round() * REFERENCE_DB_QUANTUM;
    if rounded == 0.0 {
        0.0
    } else {
        rounded
    }
}

fn canonicalize_reference_measurements(measured: &mut StreamingMeasurements) {
    measured.ebu.integrated_lufs = canonical_reference_db(measured.ebu.integrated_lufs);
    measured.ebu.max_momentary_lufs = canonical_reference_db(measured.ebu.max_momentary_lufs);
    measured.ebu.max_short_term_lufs = canonical_reference_db(measured.ebu.max_short_term_lufs);
    measured.ebu.loudness_range_lu = canonical_reference_db(measured.ebu.loudness_range_lu);
    measured.rms_db = canonical_reference_db(measured.rms_db);
    for point in &mut measured.timeline {
        point.momentary_lufs = point.momentary_lufs.map(canonical_reference_db);
        point.short_term_lufs = point.short_term_lufs.map(canonical_reference_db);
        point.sample_peak_dbfs = canonical_reference_db(point.sample_peak_dbfs);
        point.true_peak_dbtp = canonical_reference_db(point.true_peak_dbtp);
    }
}

/// Absolute 100 ms-grid clock used by every gated and short-term measurement.
///
/// BS.1770 defines 400 ms gating blocks with 75% overlap and requires block
/// durations to be rounded to the nearest sample.  Advancing a truncated
/// `sample_rate / 10` hop accumulates phase error at rates such as 11_025 Hz.
/// This clock instead derives every block start from its absolute grid index.
#[derive(Debug, Clone)]
struct RationalBlockClock {
    sample_rate: u32,
    window_frames: usize,
    next_block_index: u64,
    next_block_frame: usize,
}

impl RationalBlockClock {
    fn new(sample_rate: u32, window_tenths: u64) -> Self {
        let window_frames = rounded_tenth_frames(sample_rate, window_tenths)
            .expect("a u32 sample rate and a bounded loudness window fit in usize")
            .max(1);
        Self {
            sample_rate,
            window_frames,
            next_block_index: 0,
            next_block_frame: window_frames,
        }
    }

    fn window_frames(&self) -> usize {
        self.window_frames
    }

    fn next_block_frame(&self) -> usize {
        self.next_block_frame
    }

    fn advance(&mut self) -> Result<(), String> {
        let next_index = self
            .next_block_index
            .checked_add(1)
            .ok_or_else(|| "loudness block clock index overflow".to_string())?;
        let mut start = rounded_tenth_frames(self.sample_rate, next_index)?;
        // Below 10 Hz several 100 ms boundaries round to the same sample.
        // Retain the historical minimum one-sample hop while keeping all
        // practical audio rates on the exact absolute clock.
        if self.sample_rate < 10 {
            start = start.max(
                usize::try_from(next_index)
                    .map_err(|_| "loudness block clock exceeds the frame range".to_string())?,
            );
        }
        let next_frame = start
            .checked_add(self.window_frames)
            .ok_or_else(|| "loudness block clock frame overflow".to_string())?;
        if next_frame <= self.next_block_frame {
            return Err("loudness block clock did not advance".into());
        }
        self.next_block_index = next_index;
        self.next_block_frame = next_frame;
        Ok(())
    }
}

pub(crate) fn rounded_tenth_frames(sample_rate: u32, tenths: u64) -> Result<usize, String> {
    let numerator = u128::from(sample_rate)
        .checked_mul(u128::from(tenths))
        .ok_or_else(|| "loudness block clock multiplication overflow".to_string())?;
    let rounded = numerator
        .checked_add(LOUDNESS_CLOCK_TICKS_PER_SECOND / 2)
        .ok_or_else(|| "loudness block clock rounding overflow".to_string())?
        / LOUDNESS_CLOCK_TICKS_PER_SECOND;
    usize::try_from(rounded).map_err(|_| "loudness block clock exceeds the frame range".to_string())
}

fn should_parallelize_stereo_true_peak(
    sample_rate: u32,
    channel_count: usize,
    chunk_frames: usize,
    has_timeline: bool,
    worker_threads: usize,
) -> bool {
    !has_timeline
        && channel_count == 2
        && oversample_factor(sample_rate) > 4
        && chunk_frames >= MIN_PARALLEL_TRUE_PEAK_FRAMES
        && worker_threads > 1
}

/// EBU Tech 3342 requires at least 1.5 seconds of post-signal silence before
/// determining the final LRA of a finite file. Round up at odd sample rates so
/// the appended duration is never shorter than the specified minimum.
fn lra_tail_frames(sample_rate: u32) -> usize {
    (sample_rate as usize).saturating_mul(3).div_ceil(2)
}

fn lra_tail_block_reserve(sample_rate: u32) -> usize {
    let tail_ticks =
        (lra_tail_frames(sample_rate) as u128).saturating_mul(LOUDNESS_CLOCK_TICKS_PER_SECOND);
    let rate = u128::from(sample_rate.max(1));
    usize::try_from(tail_ticks.div_ceil(rate)).unwrap_or(usize::MAX)
}

fn record_program_short_term_block(
    blocks: &mut Vec<f64>,
    clock: &mut RationalBlockClock,
    sum: f64,
) -> Result<(), String> {
    // `StreamingAnalyzer::finish` cannot report a capacity error, so reserve
    // the maximum number of 100 ms-grid blocks that its mandatory 1.5 s LRA
    // tail can add. Exceeding the public bound then fails during `process`
    // instead of silently omitting the tail at end of file.
    let program_limit =
        MAX_LOUDNESS_BLOCKS.saturating_sub(lra_tail_block_reserve(clock.sample_rate));
    if blocks.len() >= program_limit {
        return Err(format!(
            "loudness analysis exceeds the {MAX_LOUDNESS_BLOCKS}-short-term-block limit including the finite-file tail"
        ));
    }
    blocks.push(sum / clock.window_frames() as f64);
    clock.advance()?;
    Ok(())
}

fn validate_planar_chunk<T>(
    planar: &[Vec<T>],
    expected_channels: usize,
    consumed_frames: usize,
    invalid_sample: impl Fn(&T) -> bool,
    invalid_label: &str,
) -> Result<usize, String> {
    pcm::validate_planar_chunk(
        planar,
        expected_channels,
        consumed_frames,
        invalid_sample,
        invalid_label,
        PlanarChunkMessages {
            channel_count: "stream channel count changed",
            channel_length: "stream channel length mismatch",
            frame_overflow: "stream frame count overflow",
        },
    )
}

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
    cursor: usize,
    momentary_limit: usize,
    short_term_limit: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AnalysisIngress {
    Unset,
    FastF32,
    ScalarTyped,
}

impl LoudnessWindows {
    fn new(momentary_limit: usize, short_term_limit: usize) -> Self {
        debug_assert_ne!(momentary_limit, 0);
        debug_assert!(short_term_limit >= momentary_limit);
        Self {
            values: Vec::with_capacity(short_term_limit),
            cursor: 0,
            momentary_limit,
            short_term_limit,
        }
    }

    #[inline(always)]
    fn push(
        &mut self,
        momentary_sum: &mut CompensatedSum,
        short_term_sum: &mut CompensatedSum,
        value: f64,
    ) {
        let len = self.values.len();
        if len < self.short_term_limit {
            let momentary_expired =
                (len >= self.momentary_limit).then(|| self.values[len - self.momentary_limit]);
            self.values.push(value);
            momentary_sum.add(value);
            if let Some(expired) = momentary_expired {
                momentary_sum.subtract(expired);
            }
            short_term_sum.add(value);
            return;
        }

        let short_term_expired = self.values[self.cursor];
        let momentary_offset = self.short_term_limit - self.momentary_limit;
        let momentary_cursor = if self.cursor + momentary_offset >= self.short_term_limit {
            self.cursor + momentary_offset - self.short_term_limit
        } else {
            self.cursor + momentary_offset
        };
        let momentary_expired = self.values[momentary_cursor];
        self.values[self.cursor] = value;
        momentary_sum.add(value);
        momentary_sum.subtract(momentary_expired);
        short_term_sum.add(value);
        short_term_sum.subtract(short_term_expired);
        self.cursor += 1;
        if self.cursor == self.short_term_limit {
            self.cursor = 0;
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
    momentary_sum: CompensatedSum,
    short_term_sum: CompensatedSum,
    momentary_clock: RationalBlockClock,
    short_term_clock: RationalBlockClock,
    gating_blocks: Vec<f64>,
    short_term_blocks: Vec<f64>,
    // Division by each fixed window length is monotonic. Retain the maximum
    // sums in the hot loop and convert them to mean square once in `finish`.
    max_momentary_sum: f64,
    max_short_term_sum: f64,
    frames: usize,
    raw_sum_squares: CompensatedSum,
    weighted_sum_squares: CompensatedSum,
    sample_peak: f32,
    timeline_interval_frames: Option<usize>,
    timeline: Vec<LoudnessTimelinePoint>,
    timeline_start_frame: usize,
    interval_sample_peak: f32,
    interval_true_peak: f32,
    ingress: AnalysisIngress,
    reference_engine: bool,
    // A finite true-peak measurement advances the FIR with 15 zero samples at
    // EOF. Retain only the history needed to attribute that response to the
    // final timeline interval; ordinary non-timeline analysis pays no cost.
    timeline_true_peak_tail: Vec<Vec<f32>>,
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
        let momentary_clock = RationalBlockClock::new(sample_rate, 4);
        let short_term_clock = RationalBlockClock::new(sample_rate, 30);
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
                .map(|_| TruePeakMeter::for_finite_sample_rate(sample_rate))
                .collect(),
            #[cfg(all(
                feature = "cuda-truepeak",
                any(target_os = "linux", target_os = "windows")
            ))]
            cuda_true_peak,
            windows: LoudnessWindows::new(
                momentary_clock.window_frames(),
                short_term_clock.window_frames(),
            ),
            momentary_sum: CompensatedSum::new(),
            short_term_sum: CompensatedSum::new(),
            momentary_clock,
            short_term_clock,
            gating_blocks: Vec::new(),
            short_term_blocks: Vec::new(),
            max_momentary_sum: 0.0,
            max_short_term_sum: 0.0,
            frames: 0,
            raw_sum_squares: CompensatedSum::new(),
            weighted_sum_squares: CompensatedSum::new(),
            sample_peak: 0.0,
            timeline_interval_frames: interval_frames,
            timeline: Vec::new(),
            timeline_start_frame: 0,
            interval_sample_peak: 0.0,
            interval_true_peak: 0.0,
            ingress: AnalysisIngress::Unset,
            reference_engine: false,
            timeline_true_peak_tail: interval_frames
                .map(|_| {
                    (0..channels)
                        .map(|_| Vec::with_capacity(TAPS_PER_PHASE - 1))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    pub fn process(&mut self, planar: &[Vec<f32>]) -> Result<(), String> {
        // Validate the complete chunk before advancing any recursive filter,
        // window, peak, CUDA, or timeline state. Scan each planar channel
        // contiguously, then choose the lexicographically first (frame,
        // channel) location so the diagnostic is backend-independent.
        let chunk_frames = validate_planar_chunk(
            planar,
            self.roles.len(),
            self.frames,
            |sample| !sample.is_finite(),
            "non-finite sample",
        )?;
        if chunk_frames != 0 {
            if self.ingress == AnalysisIngress::ScalarTyped {
                return Err("cannot mix fast f32 and scalar typed PCM chunks".into());
            }
            self.ingress = AnalysisIngress::FastF32;
        }
        let momentary_window = self.momentary_clock.window_frames();
        let short_term_window = self.short_term_clock.window_frames();
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
            );
            self.finish_cuda_true_peak(planar);
            return result;
        }
        // Measured 2x/4x interpolation is faster in the fused stereo SIMD loop:
        // Rayon task overhead and a separate PCM pass cost more than they save.
        // Ratios above 4x retain the benchmark-gated long-chunk split because
        // their additional interpolation work can amortize those costs.
        if should_parallelize_stereo_true_peak(
            self.sample_rate,
            planar.len(),
            chunk_frames,
            self.timeline_interval_frames.is_some(),
            rayon::current_num_threads(),
        ) {
            let mut true_peak_meters = std::mem::take(&mut self.true_peak_meters);
            let ((), result) = rayon::join(
                || process_true_peak_channel_group(&mut true_peak_meters, planar),
                || {
                    self.process_without_true_peak(
                        planar,
                        chunk_frames,
                        momentary_window,
                        short_term_window,
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
            let (skip_meter0, block_sample_peak0) =
                meter0.try_skip_peak_only_block_with_sample_peak(&planar[0]);
            let (skip_meter1, block_sample_peak1) =
                meter1.try_skip_peak_only_block_with_sample_peak(&planar[1]);
            self.sample_peak = self
                .sample_peak
                .max(block_sample_peak0)
                .max(block_sample_peak1);
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
                let mut weighted = CompensatedSum::new();
                weighted.add(weight0 * filtered0 * filtered0);
                weighted.add(weight1 * filtered1 * filtered1);
                let weighted = weighted.total();
                let raw0 = sample0 as f64;
                self.raw_sum_squares.add(raw0 * raw0);
                let raw1 = sample1 as f64;
                self.raw_sum_squares.add(raw1 * raw1);
                self.weighted_sum_squares.add(weighted);
                self.windows
                    .push(&mut self.momentary_sum, &mut self.short_term_sum, weighted);
                self.frames += 1;
                if self.windows.momentary_len() == momentary_window {
                    self.max_momentary_sum = self.max_momentary_sum.max(self.momentary_sum.total());
                }
                if self.windows.short_term_len() == short_term_window {
                    self.max_short_term_sum =
                        self.max_short_term_sum.max(self.short_term_sum.total());
                }
                if self.frames == self.momentary_clock.next_block_frame() {
                    if self.gating_blocks.len() == MAX_LOUDNESS_BLOCKS {
                        return Err(format!(
                            "loudness analysis exceeds the {MAX_LOUDNESS_BLOCKS}-gating-block limit"
                        ));
                    }
                    self.gating_blocks
                        .push(self.momentary_sum.total() / momentary_window as f64);
                    self.momentary_clock.advance()?;
                }
                if self.frames == self.short_term_clock.next_block_frame() {
                    record_program_short_term_block(
                        &mut self.short_term_blocks,
                        &mut self.short_term_clock,
                        self.short_term_sum.total(),
                    )?;
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
                self.weighted_sum_squares.add(weighted);
                self.windows
                    .push(&mut self.momentary_sum, &mut self.short_term_sum, weighted);
                self.frames += 1;
                if self.windows.momentary_len() == momentary_window {
                    self.max_momentary_sum = self.max_momentary_sum.max(self.momentary_sum.total());
                }
                if self.windows.short_term_len() == short_term_window {
                    self.max_short_term_sum =
                        self.max_short_term_sum.max(self.short_term_sum.total());
                }
                if self.frames == self.momentary_clock.next_block_frame() {
                    if self.gating_blocks.len() == MAX_LOUDNESS_BLOCKS {
                        return Err(format!(
                            "loudness analysis exceeds the {MAX_LOUDNESS_BLOCKS}-gating-block limit"
                        ));
                    }
                    self.gating_blocks
                        .push(self.momentary_sum.total() / momentary_window as f64);
                    self.momentary_clock.advance()?;
                }
                if self.frames == self.short_term_clock.next_block_frame() {
                    record_program_short_term_block(
                        &mut self.short_term_blocks,
                        &mut self.short_term_clock,
                        self.short_term_sum.total(),
                    )?;
                }
            }
            return Ok(());
        }
        for frame in 0..chunk_frames {
            let mut weighted = CompensatedSum::new();
            for ((index, channel), filter) in planar.iter().enumerate().zip(self.filters.iter_mut())
            {
                let sample = channel[frame];
                let reconstructed_peak = self.true_peak_meters[index].process_sample(sample);
                self.interval_true_peak = self.interval_true_peak.max(reconstructed_peak);
                self.interval_sample_peak = self.interval_sample_peak.max(sample.abs());
                let filtered = filter.process(sample) as f64;
                weighted.add(channel_weight(self.roles[index]) * filtered * filtered);
                let raw = sample as f64;
                self.raw_sum_squares.add(raw * raw);
                self.sample_peak = self.sample_peak.max(sample.abs());
            }
            let weighted = weighted.total();
            self.weighted_sum_squares.add(weighted);
            self.windows
                .push(&mut self.momentary_sum, &mut self.short_term_sum, weighted);
            self.frames += 1;
            if self.windows.momentary_len() == momentary_window {
                self.max_momentary_sum = self.max_momentary_sum.max(self.momentary_sum.total());
            }
            if self.windows.short_term_len() == short_term_window {
                self.max_short_term_sum = self.max_short_term_sum.max(self.short_term_sum.total());
            }
            if self.frames == self.momentary_clock.next_block_frame() {
                if self.gating_blocks.len() == MAX_LOUDNESS_BLOCKS {
                    return Err(format!(
                        "loudness analysis exceeds the {MAX_LOUDNESS_BLOCKS}-gating-block limit"
                    ));
                }
                self.gating_blocks
                    .push(self.momentary_sum.total() / momentary_window as f64);
                self.momentary_clock.advance()?;
            }
            if self.frames == self.short_term_clock.next_block_frame() {
                record_program_short_term_block(
                    &mut self.short_term_blocks,
                    &mut self.short_term_clock,
                    self.short_term_sum.total(),
                )?;
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
        self.remember_timeline_true_peak_tail(planar);
        Ok(())
    }

    /// Process unsigned 8-bit PCM, normalized around code 128.
    pub fn process_u8(&mut self, planar: &[Vec<u8>]) -> Result<(), String> {
        let frames = validate_planar_chunk(
            planar,
            self.roles.len(),
            self.frames,
            |_| false,
            "invalid unsigned 8-bit sample",
        )?;
        self.process_scalar_typed(planar, frames, |sample| (f64::from(sample) - 128.0) / 128.0)
    }

    /// Process signed 16-bit PCM without first rounding it through `f32`.
    pub fn process_i16(&mut self, planar: &[Vec<i16>]) -> Result<(), String> {
        let frames = validate_planar_chunk(
            planar,
            self.roles.len(),
            self.frames,
            |_| false,
            "invalid signed 16-bit sample",
        )?;
        self.process_scalar_typed(planar, frames, |sample| f64::from(sample) / 32_768.0)
    }

    /// Process signed 24-bit PCM carried in `i32` values.
    pub fn process_s24(&mut self, planar: &[Vec<i32>]) -> Result<(), String> {
        let frames = validate_planar_chunk(
            planar,
            self.roles.len(),
            self.frames,
            |sample| !(-8_388_608..=8_388_607).contains(sample),
            "sample outside the signed 24-bit range",
        )?;
        self.process_scalar_typed(planar, frames, |sample| f64::from(sample) / 8_388_608.0)
    }

    /// Process signed 32-bit PCM without first rounding it through `f32`.
    pub fn process_i32(&mut self, planar: &[Vec<i32>]) -> Result<(), String> {
        let frames = validate_planar_chunk(
            planar,
            self.roles.len(),
            self.frames,
            |_| false,
            "invalid signed 32-bit sample",
        )?;
        self.process_scalar_typed(planar, frames, |sample| f64::from(sample) / 2_147_483_648.0)
    }

    /// Process normalized `f64` PCM through the scalar high-precision lane.
    pub fn process_f64(&mut self, planar: &[Vec<f64>]) -> Result<(), String> {
        let frames = validate_planar_chunk(
            planar,
            self.roles.len(),
            self.frames,
            |sample| !sample.is_finite(),
            "non-finite sample",
        )?;
        validate_planar_chunk(
            planar,
            self.roles.len(),
            self.frames,
            |sample| sample.abs() > f64::from(f32::MAX),
            "sample outside the finite true-peak domain",
        )?;
        self.process_scalar_typed(planar, frames, |sample| sample)
    }

    fn process_scalar_typed<T: Copy>(
        &mut self,
        planar: &[Vec<T>],
        chunk_frames: usize,
        normalize: impl Fn(T) -> f64 + Copy,
    ) -> Result<(), String> {
        if chunk_frames != 0 {
            if self.ingress == AnalysisIngress::FastF32 {
                return Err("cannot mix fast f32 and scalar typed PCM chunks".into());
            }
            self.ingress = AnalysisIngress::ScalarTyped;
        }
        let momentary_window = self.momentary_clock.window_frames();
        let short_term_window = self.short_term_clock.window_frames();
        for frame in 0..chunk_frames {
            let mut weighted = CompensatedSum::new();
            for (((samples, meter), filter), role) in planar
                .iter()
                .zip(&mut self.true_peak_meters)
                .zip(&mut self.filters)
                .zip(self.roles.iter().copied())
            {
                let sample = normalize(samples[frame]);
                let sample_f32 = sample as f32;
                let reconstructed_peak = meter.process_sample(sample_f32);
                self.interval_true_peak = self.interval_true_peak.max(reconstructed_peak);
                self.interval_sample_peak = self.interval_sample_peak.max(sample_f32.abs());
                self.sample_peak = self.sample_peak.max(sample_f32.abs());
                let filtered = filter.process_f64(sample);
                weighted.add(channel_weight(role) * filtered * filtered);
                self.raw_sum_squares.add(sample * sample);
            }
            let weighted = weighted.total();
            self.weighted_sum_squares.add(weighted);
            self.windows
                .push(&mut self.momentary_sum, &mut self.short_term_sum, weighted);
            self.frames += 1;
            if self.windows.momentary_len() == momentary_window {
                self.max_momentary_sum = self.max_momentary_sum.max(self.momentary_sum.total());
            }
            if self.windows.short_term_len() == short_term_window {
                self.max_short_term_sum = self.max_short_term_sum.max(self.short_term_sum.total());
            }
            if self.frames == self.momentary_clock.next_block_frame() {
                if self.gating_blocks.len() == MAX_LOUDNESS_BLOCKS {
                    return Err(format!(
                        "loudness analysis exceeds the {MAX_LOUDNESS_BLOCKS}-gating-block limit"
                    ));
                }
                self.gating_blocks
                    .push(self.momentary_sum.total() / momentary_window as f64);
                self.momentary_clock.advance()?;
            }
            if self.frames == self.short_term_clock.next_block_frame() {
                record_program_short_term_block(
                    &mut self.short_term_blocks,
                    &mut self.short_term_clock,
                    self.short_term_sum.total(),
                )?;
            }
            if self
                .timeline_interval_frames
                .is_some_and(|interval| self.frames.is_multiple_of(interval))
            {
                if self.timeline.len() >= MAX_LOUDNESS_TIMELINE_POINTS - 1 {
                    return Err(format!(
                        "loudness timeline exceeds the {MAX_LOUDNESS_TIMELINE_POINTS}-point limit"
                    ));
                }
                self.record_timeline_point(momentary_window, short_term_window);
            }
        }
        self.remember_typed_timeline_true_peak_tail(planar, normalize);
        Ok(())
    }

    fn remember_typed_timeline_true_peak_tail<T: Copy>(
        &mut self,
        planar: &[Vec<T>],
        normalize: impl Fn(T) -> f64,
    ) {
        if self.timeline_interval_frames.is_none() {
            return;
        }
        for (tail, samples) in self.timeline_true_peak_tail.iter_mut().zip(planar) {
            let retained = TAPS_PER_PHASE - 1;
            let start = samples.len().saturating_sub(retained);
            let incoming = samples[start..]
                .iter()
                .copied()
                .map(|sample| normalize(sample) as f32)
                .collect::<Vec<_>>();
            let expired = tail
                .len()
                .saturating_add(incoming.len())
                .saturating_sub(retained);
            tail.drain(..expired);
            tail.extend(incoming);
        }
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
                match CudaTruePeakWorker::new_finite(self.sample_rate, planar.len(), frames) {
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
                let mut weighted = CompensatedSum::new();
                weighted.add(weight0 * filtered0 * filtered0);
                weighted.add(weight1 * filtered1 * filtered1);
                let weighted = weighted.total();
                let raw0 = sample0 as f64;
                self.raw_sum_squares.add(raw0 * raw0);
                self.sample_peak = self.sample_peak.max(sample0.abs());
                let raw1 = sample1 as f64;
                self.raw_sum_squares.add(raw1 * raw1);
                self.sample_peak = self.sample_peak.max(sample1.abs());
                self.weighted_sum_squares.add(weighted);
                self.windows
                    .push(&mut self.momentary_sum, &mut self.short_term_sum, weighted);
                self.frames += 1;
                if self.windows.momentary_len() == momentary_window {
                    self.max_momentary_sum = self.max_momentary_sum.max(self.momentary_sum.total());
                }
                if self.windows.short_term_len() == short_term_window {
                    self.max_short_term_sum =
                        self.max_short_term_sum.max(self.short_term_sum.total());
                }
                if self.frames == self.momentary_clock.next_block_frame() {
                    if self.gating_blocks.len() == MAX_LOUDNESS_BLOCKS {
                        return Err(format!(
                            "loudness analysis exceeds the {MAX_LOUDNESS_BLOCKS}-gating-block limit"
                        ));
                    }
                    self.gating_blocks
                        .push(self.momentary_sum.total() / momentary_window as f64);
                    self.momentary_clock.advance()?;
                }
                if self.frames == self.short_term_clock.next_block_frame() {
                    record_program_short_term_block(
                        &mut self.short_term_blocks,
                        &mut self.short_term_clock,
                        self.short_term_sum.total(),
                    )?;
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
            self.push_weighted_frame(weighted, momentary_window, short_term_window)?;
        }
        Ok(())
    }

    #[inline(always)]
    fn push_weighted_frame(
        &mut self,
        weighted: f64,
        momentary_window: usize,
        short_term_window: usize,
    ) -> Result<(), String> {
        self.weighted_sum_squares.add(weighted);
        self.windows
            .push(&mut self.momentary_sum, &mut self.short_term_sum, weighted);
        self.frames += 1;
        if self.windows.momentary_len() == momentary_window {
            self.max_momentary_sum = self.max_momentary_sum.max(self.momentary_sum.total());
        }
        if self.windows.short_term_len() == short_term_window {
            self.max_short_term_sum = self.max_short_term_sum.max(self.short_term_sum.total());
        }
        if self.frames == self.momentary_clock.next_block_frame() {
            if self.gating_blocks.len() == MAX_LOUDNESS_BLOCKS {
                return Err(format!(
                    "loudness analysis exceeds the {MAX_LOUDNESS_BLOCKS}-gating-block limit"
                ));
            }
            self.gating_blocks
                .push(self.momentary_sum.total() / momentary_window as f64);
            self.momentary_clock.advance()?;
        }
        if self.frames == self.short_term_clock.next_block_frame() {
            record_program_short_term_block(
                &mut self.short_term_blocks,
                &mut self.short_term_clock,
                self.short_term_sum.total(),
            )?;
        }
        Ok(())
    }

    /// Advance only the K-weighting and the 3 s short-term window through the
    /// post-signal silence required by EBU Tech 3342. Programme duration,
    /// integrated-loudness blocks, RMS/peaks, maxima, and timeline state are
    /// deliberately not advanced.
    fn append_finite_lra_tail(&mut self) {
        if self.frames == 0 {
            return;
        }
        let tail_frames = lra_tail_frames(self.sample_rate);
        let short_term_window = self.short_term_clock.window_frames();
        let mut lra_frame = self.frames;
        for _ in 0..tail_frames {
            let weighted = self.process_kweighted_silence_frame();
            self.windows
                .push(&mut self.momentary_sum, &mut self.short_term_sum, weighted);
            lra_frame = lra_frame.saturating_add(1);
            if lra_frame == self.short_term_clock.next_block_frame() {
                assert!(
                    self.short_term_blocks.len() < MAX_LOUDNESS_BLOCKS,
                    "finite-file LRA tail capacity must be reserved during processing"
                );
                self.short_term_blocks
                    .push(self.short_term_sum.total() / short_term_window as f64);
                self.short_term_clock
                    .advance()
                    .expect("the bounded LRA tail cannot overflow its rational clock");
            }
        }
    }

    #[inline(always)]
    fn process_kweighted_silence_frame(&mut self) -> f64 {
        if self.ingress != AnalysisIngress::ScalarTyped
            && self.timeline_interval_frames.is_none()
            && self.roles.len() == 2
        {
            #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
            {
                let filtered = self
                    .kweight_pair
                    .as_mut()
                    .expect("non-timeline stereo analyzer owns paired K-weighting state")
                    .process([0.0, 0.0]);
                let mut weighted = CompensatedSum::new();
                weighted.add(
                    channel_weight(self.roles[0]) * f64::from(filtered[0]) * f64::from(filtered[0]),
                );
                weighted.add(
                    channel_weight(self.roles[1]) * f64::from(filtered[1]) * f64::from(filtered[1]),
                );
                return weighted.total();
            }
            #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
            {
                let filtered0 = f64::from(self.filters[0].process(0.0));
                let filtered1 = f64::from(self.filters[1].process(0.0));
                let mut weighted = CompensatedSum::new();
                weighted.add(channel_weight(self.roles[0]) * filtered0 * filtered0);
                weighted.add(channel_weight(self.roles[1]) * filtered1 * filtered1);
                return weighted.total();
            }
        }

        #[cfg(target_arch = "x86_64")]
        if self.ingress != AnalysisIngress::ScalarTyped
            && self.timeline_interval_frames.is_none()
            && self.roles.len() >= 4
        {
            if let Some(quads) = self.kweight_quads.as_mut() {
                let mut weighted = CompensatedSum::new();
                let mut channel = 0;
                for quad in quads {
                    let filtered = quad.process([0.0; 4]);
                    for (lane, value) in filtered.into_iter().enumerate() {
                        let value = f64::from(value);
                        weighted.add(channel_weight(self.roles[channel + lane]) * value * value);
                    }
                    channel += 4;
                }
                for index in channel..self.filters.len() {
                    let value = f64::from(self.filters[index].process(0.0));
                    weighted.add(channel_weight(self.roles[index]) * value * value);
                }
                return weighted.total();
            }
        }

        self.filters
            .iter_mut()
            .zip(self.roles.iter().copied())
            .map(|(filter, role)| {
                let value = f64::from(filter.process(0.0));
                channel_weight(role) * value * value
            })
            .collect::<CompensatedSum>()
            .total()
    }

    /// Finish a complete programme measurement, including the EBU Tech 3342
    /// finite-file silence required for Loudness Range.
    pub fn finish(self) -> StreamingMeasurements {
        self.finish_impl(true)
    }

    /// Finish a selected region when its LRA is not consumed by the caller.
    /// Integrated loudness, energy, duration, RMS, peaks, gating blocks, and
    /// timeline semantics are identical to [`Self::finish`].
    pub(crate) fn finish_without_lra_tail(self) -> StreamingMeasurements {
        self.finish_impl(false)
    }

    fn finish_impl(mut self, append_lra_tail: bool) -> StreamingMeasurements {
        self.merge_finite_true_peak_tail_into_timeline();
        if self.timeline_interval_frames.is_some() && self.timeline_start_frame < self.frames {
            let momentary_window = self.momentary_clock.window_frames();
            let short_term_window = self.short_term_clock.window_frames();
            self.record_timeline_point(momentary_window, short_term_window);
        }
        let channels = self.roles.len();
        let total_samples = self.frames * channels;
        let rms = if total_samples == 0 {
            0.0
        } else {
            (self.raw_sum_squares.total() / total_samples as f64).sqrt()
        };
        #[cfg(all(
            feature = "cuda-truepeak",
            any(target_os = "linux", target_os = "windows")
        ))]
        let true_peak =
            match std::mem::replace(&mut self.cuda_true_peak, CudaTruePeakState::Disabled) {
                CudaTruePeakState::Active(worker) => worker.finish_peak(),
                CudaTruePeakState::Disabled | CudaTruePeakState::Pending => {
                    std::mem::take(&mut self.true_peak_meters)
                        .into_iter()
                        .map(TruePeakMeter::finish_peak)
                        .fold(0.0, f32::max)
                }
            };
        #[cfg(not(all(
            feature = "cuda-truepeak",
            any(target_os = "linux", target_os = "windows")
        )))]
        let true_peak = std::mem::take(&mut self.true_peak_meters)
            .into_iter()
            .map(TruePeakMeter::finish_peak)
            .fold(0.0, f32::max);
        if append_lra_tail {
            self.append_finite_lra_tail();
        }
        let mut ebu = measurements_from_blocks(self.gating_blocks, &self.short_term_blocks);
        let momentary_window = self.momentary_clock.window_frames();
        let short_term_window = self.short_term_clock.window_frames();
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
                self.weighted_sum_squares.total() / self.frames as f64
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

    fn remember_timeline_true_peak_tail(&mut self, planar: &[Vec<f32>]) {
        for (tail, samples) in self.timeline_true_peak_tail.iter_mut().zip(planar) {
            let retained = TAPS_PER_PHASE - 1;
            if samples.len() >= retained {
                tail.clear();
                tail.extend_from_slice(&samples[samples.len() - retained..]);
            } else {
                let expired = tail
                    .len()
                    .saturating_add(samples.len())
                    .saturating_sub(retained);
                tail.drain(..expired);
                tail.extend_from_slice(samples);
            }
        }
    }

    fn finite_true_peak_tail(&self) -> f32 {
        self.timeline_true_peak_tail
            .iter()
            .map(|samples| {
                if self.reference_engine {
                    TruePeakMeter::finite_reference_tail_peak_from_recent_samples(
                        self.sample_rate,
                        samples,
                    )
                    .expect("reference coefficients were validated by the constructor")
                } else {
                    TruePeakMeter::finite_tail_peak_from_recent_samples(self.sample_rate, samples)
                }
            })
            .fold(0.0, f32::max)
    }

    fn merge_finite_true_peak_tail_into_timeline(&mut self) {
        if self.frames == 0 || self.timeline_interval_frames.is_none() {
            return;
        }
        let tail_peak = self.finite_true_peak_tail();
        if self.timeline_start_frame < self.frames {
            self.interval_true_peak = self.interval_true_peak.max(tail_peak);
        } else if let Some(last) = self.timeline.last_mut() {
            // An exact interval boundary was recorded in `process`; EOF still
            // belongs to that final interval even though no partial point is
            // waiting to be emitted.
            last.true_peak_dbtp = last.true_peak_dbtp.max(amplitude_db(tail_peak));
        }
    }

    fn record_timeline_point(&mut self, momentary_window: usize, short_term_window: usize) {
        self.timeline.push(LoudnessTimelinePoint {
            start_seconds: self.timeline_start_frame as f64 / self.sample_rate as f64,
            end_seconds: self.frames as f64 / self.sample_rate as f64,
            momentary_lufs: complete_window_loudness(
                self.momentary_sum.total(),
                self.windows.momentary_len(),
                momentary_window,
            ),
            short_term_lufs: complete_window_loudness(
                self.short_term_sum.total(),
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

/// Scalar, CPU-only analyzer with committed filter/interpolation coefficient
/// bits and a fixed frame/channel/tap reduction order.
pub struct ReferenceStreamingAnalyzer {
    inner: StreamingAnalyzer,
}

impl ReferenceStreamingAnalyzer {
    pub fn new(sample_rate: u32, roles: Vec<ChannelRole>) -> Result<Self, String> {
        Self::with_timeline_interval(sample_rate, roles, None)
    }

    pub fn with_timeline_interval(
        sample_rate: u32,
        roles: Vec<ChannelRole>,
        interval_frames: Option<usize>,
    ) -> Result<Self, String> {
        let mut inner =
            StreamingAnalyzer::with_timeline_interval(sample_rate, roles, interval_frames);
        inner.filters = (0..inner.roles.len())
            .map(|_| KWeight::for_reference_sample_rate(sample_rate))
            .collect::<Result<Vec<_>, _>>()?;
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        {
            inner.kweight_pair = None;
        }
        #[cfg(target_arch = "x86_64")]
        {
            inner.kweight_quads = None;
        }
        inner.true_peak_meters = (0..inner.roles.len())
            .map(|_| TruePeakMeter::for_finite_reference_sample_rate(sample_rate))
            .collect::<Result<Vec<_>, _>>()?;
        #[cfg(all(
            feature = "cuda-truepeak",
            any(target_os = "linux", target_os = "windows")
        ))]
        {
            inner.cuda_true_peak = CudaTruePeakState::Disabled;
        }
        inner.reference_engine = true;
        Ok(Self { inner })
    }

    /// Validate and process one planar `f32` chunk using only scalar ordered
    /// operations. Rejected chunks leave all analyzer state unchanged.
    pub fn process(&mut self, planar: &[Vec<f32>]) -> Result<(), String> {
        let frames = validate_planar_chunk(
            planar,
            self.inner.roles.len(),
            self.inner.frames,
            |sample| !sample.is_finite(),
            "non-finite sample",
        )?;
        self.inner.process_scalar_typed(planar, frames, f64::from)
    }

    pub fn finish(self) -> StreamingMeasurements {
        let mut measured = self.inner.finish();
        canonicalize_reference_measurements(&mut measured);
        measured
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
    raw_sum_squares: &mut CompensatedSum,
    sample_peak: &mut f32,
) -> f64 {
    debug_assert_eq!(filters.len(), roles.len());
    debug_assert_eq!(filters.len(), planar.len());
    let mut weighted = CompensatedSum::new();
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
                weighted.add(channel_weight(roles[channel + lane]) * lane_filtered * lane_filtered);
                let raw = input[lane] as f64;
                raw_sum_squares.add(raw * raw);
                *sample_peak = sample_peak.max(input[lane].abs());
            }
            channel += 4;
        }
        for index in channel..filters.len() {
            let sample = planar[index][frame];
            let filtered = filters[index].process(sample) as f64;
            weighted.add(channel_weight(roles[index]) * filtered * filtered);
            let raw = sample as f64;
            raw_sum_squares.add(raw * raw);
            *sample_peak = sample_peak.max(sample.abs());
        }
        return weighted.total();
    }
    for ((index, channel), filter) in planar.iter().enumerate().zip(filters.iter_mut()) {
        let sample = channel[frame];
        let filtered = filter.process(sample) as f64;
        weighted.add(channel_weight(roles[index]) * filtered * filtered);
        let raw = sample as f64;
        raw_sum_squares.add(raw * raw);
        *sample_peak = sample_peak.max(sample.abs());
    }
    weighted.total()
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
    let mut momentary_clock = RationalBlockClock::new(buf.sample_rate, 4);
    let mut short_term_clock = RationalBlockClock::new(buf.sample_rate, 30);
    let momentary_window = momentary_clock.window_frames();
    let short_term_window = short_term_clock.window_frames();
    if buf.frames < momentary_window {
        return EbuMeasurements {
            integrated_lufs: f64::NEG_INFINITY,
            max_momentary_lufs: f64::NEG_INFINITY,
            max_short_term_lufs: f64::NEG_INFINITY,
            loudness_range_lu: 0.0,
            gating_blocks: Vec::new(),
        };
    }

    let tail_frames = lra_tail_frames(buf.sample_rate);
    let lra_frames = buf.frames.saturating_add(tail_frames);

    // K-weight channels in parallel, but combine their frame energies in the
    // declared channel order. Rolling compensated sums avoid subtracting two
    // large lifetime prefix sums on long or high-dynamic-range programmes.
    // The extra 1.5 s of filter output is visible only to the LRA population;
    // all other file measurements remain bounded by `buf.frames`.
    let channel_energies: Vec<Vec<f64>> = buf
        .data
        .par_iter()
        .map(|ch| {
            let mut kw = KWeight::for_sample_rate(buf.sample_rate);
            let mut energies = Vec::with_capacity(ch.len().saturating_add(tail_frames));
            for &sample in ch {
                let v = f64::from(kw.process(sample));
                energies.push(v * v);
            }
            for _ in 0..tail_frames {
                let v = f64::from(kw.process(0.0));
                energies.push(v * v);
            }
            energies
        })
        .collect();

    let weights: Vec<f64> = (0..buf.channels as usize)
        .map(|index| channel_weight(buf.channel_role(index)))
        .collect();

    let mut windows = LoudnessWindows::new(momentary_window, short_term_window);
    let mut momentary_sum = CompensatedSum::new();
    let mut short_term_sum = CompensatedSum::new();
    let mut gating_blocks = Vec::new();
    let mut short_term_blocks = Vec::new();
    let mut max_momentary_sum = 0.0_f64;
    let mut max_short_term_sum = 0.0_f64;
    for frame in 0..lra_frames {
        let mut weighted = CompensatedSum::new();
        for channel in 0..channel_energies.len() {
            weighted.add(weights[channel] * channel_energies[channel][frame]);
        }
        windows.push(&mut momentary_sum, &mut short_term_sum, weighted.total());
        let frame_end = frame + 1;
        if frame_end <= buf.frames {
            if windows.momentary_len() == momentary_window {
                max_momentary_sum = max_momentary_sum.max(momentary_sum.total());
            }
            if windows.short_term_len() == short_term_window {
                max_short_term_sum = max_short_term_sum.max(short_term_sum.total());
            }
            if frame_end == momentary_clock.next_block_frame() {
                gating_blocks.push(momentary_sum.total() / momentary_window as f64);
                momentary_clock
                    .advance()
                    .expect("an in-memory measurement cannot overflow its rational clock");
            }
        }
        if frame_end == short_term_clock.next_block_frame() {
            short_term_blocks.push(short_term_sum.total() / short_term_window as f64);
            short_term_clock
                .advance()
                .expect("an in-memory measurement cannot overflow its rational clock");
        }
    }

    let mut measurements = measurements_from_blocks(gating_blocks, &short_term_blocks);
    measurements.max_momentary_lufs =
        maximum_loudness(&[max_momentary_sum / momentary_window as f64]);
    measurements.max_short_term_lufs =
        maximum_loudness(&[max_short_term_sum / short_term_window as f64]);
    measurements
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

/// Apply the BS.1770 absolute and relative gates to a population of blocks.
pub fn gated_lufs(block_ms: &[f64]) -> f64 {
    gated_lufs_iter(block_ms.iter().copied())
}

/// Apply the BS.1770 gates without materializing a combined block population.
///
/// The iterator must be cloneable because the relative threshold is derived
/// from the absolute-gated population before the final gate can be applied.
pub(crate) fn gated_lufs_iter<I>(block_ms: I) -> f64
where
    I: Clone + Iterator<Item = f64>,
{
    let abs_gate_ms = ABSOLUTE_GATE_MEAN_SQUARE;
    let mut absolute_sum = CompensatedSum::new();
    let mut absolute_count = 0_usize;
    for value in block_ms.clone().filter(|&value| value > abs_gate_ms) {
        absolute_sum.add(value);
        absolute_count += 1;
    }
    if absolute_count == 0 {
        return f64::NEG_INFINITY;
    }
    let mean_ms = absolute_sum.total() / absolute_count as f64;
    let rel_gate_ms = mean_ms / 10.0; // -10 dB in the linear domain
    let gate = abs_gate_ms.max(rel_gate_ms);
    let mut final_sum = CompensatedSum::new();
    let mut final_count = 0_usize;
    for value in block_ms.filter(|&value| value > gate) {
        final_sum.add(value);
        final_count += 1;
    }
    let used = if final_count == 0 {
        mean_ms
    } else {
        final_sum.total() / final_count as f64
    };
    -0.691 + 10.0 * used.log10()
}

/// Minimum and maximum BS.1770 integrated loudness over every contiguous
/// fixed-size population of complete 400 ms / 100 ms-hop gating blocks.
///
/// The sliding population is maintained in two Fenwick trees, allowing the
/// absolute- and relative-gated sums for every window to be queried in
/// `O(log n)` instead of re-gating every block in every overlapping window.
pub fn rolling_gated_loudness_extrema(
    block_ms: &[f64],
    blocks_per_window: usize,
) -> Option<(f64, f64)> {
    if blocks_per_window == 0 || block_ms.len() < blocks_per_window {
        return None;
    }
    let normalized = block_ms
        .iter()
        .map(|value| {
            if value.is_finite() && *value > 0.0 {
                *value
            } else {
                0.0
            }
        })
        .collect::<Vec<_>>();
    let mut coordinates = normalized.clone();
    coordinates.sort_by(f64::total_cmp);
    coordinates.dedup_by(|left, right| left.to_bits() == right.to_bits());
    let mut counts = Fenwick::new(coordinates.len());
    let mut sums = Fenwick::new(coordinates.len());
    let coordinate = |value: f64| {
        coordinates
            .binary_search_by(|candidate| candidate.total_cmp(&value))
            .expect("normalized gating block has a coordinate")
    };
    for value in normalized.iter().take(blocks_per_window).copied() {
        let index = coordinate(value);
        counts.add(index, 1.0);
        sums.add(index, value);
    }

    let mut minimum = f64::INFINITY;
    let mut maximum = f64::NEG_INFINITY;
    for start in 0..=normalized.len() - blocks_per_window {
        let loudness = gated_lufs_from_fenwick(&coordinates, &counts, &sums);
        minimum = minimum.min(loudness);
        maximum = maximum.max(loudness);
        if start + blocks_per_window < normalized.len() {
            let removed = normalized[start];
            let added = normalized[start + blocks_per_window];
            let removed_index = coordinate(removed);
            counts.add(removed_index, -1.0);
            sums.add(removed_index, -removed);
            let added_index = coordinate(added);
            counts.add(added_index, 1.0);
            sums.add(added_index, added);
        }
    }
    Some((minimum, maximum))
}

fn gated_lufs_from_fenwick(coordinates: &[f64], counts: &Fenwick, sums: &Fenwick) -> f64 {
    let absolute_gate = ABSOLUTE_GATE_MEAN_SQUARE;
    let absolute_index = coordinates.partition_point(|value| *value <= absolute_gate);
    let absolute_count = counts.suffix(absolute_index);
    if absolute_count < 0.5 {
        return f64::NEG_INFINITY;
    }
    let absolute_sum = sums.suffix(absolute_index);
    let absolute_mean = absolute_sum / absolute_count;
    let relative_gate = absolute_mean / 10.0;
    let gate = absolute_gate.max(relative_gate);
    let final_index = coordinates.partition_point(|value| *value <= gate);
    let final_count = counts.suffix(final_index);
    let used = if final_count < 0.5 {
        absolute_mean
    } else {
        sums.suffix(final_index) / final_count
    };
    -0.691 + 10.0 * used.log10()
}

struct Fenwick {
    tree: Vec<CompensatedSum>,
}

impl Fenwick {
    fn new(length: usize) -> Self {
        Self {
            tree: vec![CompensatedSum::new(); length + 1],
        }
    }

    fn add(&mut self, index: usize, value: f64) {
        let mut cursor = index + 1;
        while cursor < self.tree.len() {
            self.tree[cursor].add(value);
            cursor += cursor & cursor.wrapping_neg();
        }
    }

    fn prefix(&self, end: usize) -> f64 {
        let mut sum = CompensatedSum::new();
        let mut cursor = end;
        while cursor > 0 {
            sum.merge(self.tree[cursor]);
            cursor &= cursor - 1;
        }
        sum.total()
    }

    fn suffix(&self, start: usize) -> f64 {
        self.prefix(self.tree.len() - 1) - self.prefix(start)
    }
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
    let abs_gate_ms = ABSOLUTE_GATE_MEAN_SQUARE;
    let absolute: Vec<f64> = short_term_ms
        .iter()
        .copied()
        .filter(|value| *value >= abs_gate_ms)
        .collect();
    if absolute.is_empty() {
        return 0.0;
    }

    let absolute_mean =
        absolute.iter().copied().collect::<CompensatedSum>().total() / absolute.len() as f64;
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
    rank_percentile(&gated, 95) - rank_percentile(&gated, 10)
}

fn mean_square_to_lufs(value: f64) -> f64 {
    -0.691 + 10.0 * value.log10()
}

fn rank_percentile(sorted: &[f64], percent: usize) -> f64 {
    debug_assert!(!sorted.is_empty());
    debug_assert!(percent <= 100);
    // EBU Tech 3342's published MATLAB example selects (one-based)
    // round((n - 1) * p / 100 + 1). For positive values MATLAB rounds a half
    // upward, so adding 50 before integer division gives the exact zero-based
    // rank without introducing a floating-point tie ambiguity.
    let rank = ((sorted.len() - 1) * percent + 50) / 100;
    sorted[rank]
}

/// RMS level (dBFS) and sample peak (0..1) across all channels, computed in
/// parallel with SIMD primitives.
pub fn measure_rms_peak(buf: &AudioBuffer) -> (f64, f32) {
    let channels = buf
        .data
        .par_iter()
        .map(|ch| (simd::sum_squares_f64(ch), simd::abs_max(ch)))
        .collect::<Vec<_>>();
    let mut sumsq = CompensatedSum::new();
    let mut peak = 0.0_f32;
    for (channel_sum, channel_peak) in channels {
        sumsq.add(channel_sum);
        peak = peak.max(channel_peak);
    }
    let total = (buf.frames as f64) * (buf.channels as f64);
    let rms = if total > 0.0 {
        (sumsq.total() / total).sqrt()
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
    fn stereo_true_peak_parallel_policy_uses_measured_oversampling_boundary() {
        let eligible = |sample_rate| {
            should_parallelize_stereo_true_peak(
                sample_rate,
                2,
                MIN_PARALLEL_TRUE_PEAK_FRAMES,
                false,
                2,
            )
        };
        assert!(
            eligible(44_100),
            "5x interpolation should use the split pass"
        );
        assert!(
            eligible(32_000),
            "6x interpolation should use the split pass"
        );
        assert!(!eligible(48_000), "4x interpolation should stay fused");
        assert!(!eligible(64_000), "3x interpolation should stay fused");

        assert!(!should_parallelize_stereo_true_peak(
            44_100,
            2,
            MIN_PARALLEL_TRUE_PEAK_FRAMES,
            true,
            2,
        ));
        assert!(!should_parallelize_stereo_true_peak(
            44_100,
            2,
            MIN_PARALLEL_TRUE_PEAK_FRAMES - 1,
            false,
            2,
        ));
        assert!(should_parallelize_stereo_true_peak(
            44_100,
            2,
            MIN_PARALLEL_TRUE_PEAK_FRAMES,
            false,
            2,
        ));
        assert!(!should_parallelize_stereo_true_peak(
            44_100,
            2,
            MIN_PARALLEL_TRUE_PEAK_FRAMES,
            false,
            1,
        ));
        assert!(!should_parallelize_stereo_true_peak(
            44_100,
            1,
            MIN_PARALLEL_TRUE_PEAK_FRAMES,
            false,
            2,
        ));
    }

    #[test]
    fn rolling_gated_extrema_match_naive_overlapping_windows() {
        let blocks = [
            0.0, 0.000_001, 0.004, 0.005, 0.006, 0.000_02, 0.008, 0.01, 0.003, 0.02, 0.000_003,
            0.007, 0.009, 0.011, 0.012,
        ];
        let window = 7;
        let expected = blocks.windows(window).map(gated_lufs).fold(
            (f64::INFINITY, f64::NEG_INFINITY),
            |(minimum, maximum), value| (minimum.min(value), maximum.max(value)),
        );
        let actual = rolling_gated_loudness_extrema(&blocks, window).unwrap();
        assert!(
            (actual.0 - expected.0).abs() < 1e-12,
            "{actual:?} {expected:?}"
        );
        assert!(
            (actual.1 - expected.1).abs() < 1e-12,
            "{actual:?} {expected:?}"
        );
    }

    #[test]
    fn rolling_gated_extrema_require_a_complete_window() {
        assert_eq!(rolling_gated_loudness_extrema(&[0.1], 0), None);
        assert_eq!(rolling_gated_loudness_extrema(&[0.1], 2), None);
        let silence = rolling_gated_loudness_extrema(&[0.0; 4], 4).unwrap();
        assert!(silence.0.is_infinite() && silence.0.is_sign_negative());
        assert!(silence.1.is_infinite() && silence.1.is_sign_negative());
    }

    #[test]
    fn integrated_gate_excludes_the_absolute_threshold_and_includes_only_values_above_it() {
        let gate = ABSOLUTE_GATE_MEAN_SQUARE;
        let below = f64::from_bits(gate.to_bits() - 1);
        let above = f64::from_bits(gate.to_bits() + 1);

        for value in [below, gate] {
            let batch = gated_lufs(&[value]);
            assert!(batch.is_infinite() && batch.is_sign_negative());
            let rolling = rolling_gated_loudness_extrema(&[value], 1).unwrap();
            assert!(rolling.0.is_infinite() && rolling.0.is_sign_negative());
            assert!(rolling.1.is_infinite() && rolling.1.is_sign_negative());
        }

        let expected = mean_square_to_lufs(above);
        assert_eq!(gated_lufs(&[above]), expected);
        let rolling = rolling_gated_loudness_extrema(&[above], 1).unwrap();
        assert!((rolling.0 - expected).abs() < 1.0e-12);
        assert!((rolling.1 - expected).abs() < 1.0e-12);
    }

    #[test]
    fn integrated_gate_excludes_a_block_exactly_on_the_relative_threshold() {
        // Binary powers and integer multiples make this relation exact:
        // mean([threshold, 19 * threshold]) / 10 == threshold.
        let threshold = 2.0_f64.powi(-20);
        let loud = 19.0 * threshold;
        assert_eq!((threshold + loud) / 2.0 / 10.0, threshold);

        let expected = mean_square_to_lufs(loud);
        assert_eq!(gated_lufs(&[threshold, loud]), expected);
        let rolling = rolling_gated_loudness_extrema(&[threshold, loud], 2).unwrap();
        assert!((rolling.0 - expected).abs() < 1.0e-12);
        assert!((rolling.1 - expected).abs() < 1.0e-12);
    }

    #[test]
    fn compensated_loudness_windows_match_naive_window_sums() {
        for (momentary_limit, short_term_limit) in [(1, 1), (1, 2), (2, 7), (7, 31), (31, 127)] {
            let mut candidate = LoudnessWindows::new(momentary_limit, short_term_limit);
            let mut candidate_momentary_sum = CompensatedSum::new();
            let mut candidate_short_term_sum = CompensatedSum::new();
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
                assert!(
                    (candidate_momentary_sum.total() - reference_momentary_sum).abs() < 1.0e-12
                );
                assert!(
                    (candidate_short_term_sum.total() - reference_short_term_sum).abs() < 1.0e-12
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
    fn loudness_range_uses_tenth_and_ninety_fifth_rank_percentiles() {
        let blocks: Vec<f64> = (0..=100)
            .map(|step| {
                let lufs = -30.0 + step as f64 / 10.0;
                10.0_f64.powf((lufs + 0.691) / 10.0)
            })
            .collect();
        let range = loudness_range(&blocks);
        assert!((range - 8.5).abs() < 0.01, "LRA = {range}");

        // Tech 3342 selects an observed rank; it does not linearly interpolate
        // between adjacent loudness levels. With four values, the 10th and
        // 95th percentile ranks are the first and fourth values respectively.
        let sparse: Vec<f64> = [-30.0, -29.0, -28.0, -27.0]
            .into_iter()
            .map(|lufs| 10.0_f64.powf((lufs + 0.691) / 10.0))
            .collect();
        assert!((loudness_range(&sparse) - 3.0).abs() < 1.0e-12);

        // MATLAB rounds positive half-way ranks upward.
        assert_eq!(rank_percentile(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0], 10), 1.0);
        assert_eq!(rank_percentile(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0], 95), 5.0);
    }

    #[test]
    fn loudness_range_includes_blocks_exactly_on_both_gates() {
        let absolute_gate = ABSOLUTE_GATE_MEAN_SQUARE;
        let one_lu_above = absolute_gate * 10.0_f64.powf(0.1);
        assert!((loudness_range(&[absolute_gate, one_lu_above]) - 1.0).abs() < 1.0e-12);

        // mean([threshold, 199 * threshold]) / 100 == threshold, so the
        // quieter block lies exactly on the Tech 3342 -20 LU relative gate.
        let threshold = 2.0_f64.powi(-18);
        let loud = 199.0 * threshold;
        assert_eq!((threshold + loud) / 2.0 / 100.0, threshold);
        let expected = 10.0 * 199.0_f64.log10();
        assert!((loudness_range(&[threshold, loud]) - expected).abs() < 1.0e-12);
    }

    #[test]
    fn finite_lra_tail_advances_only_the_lra_state() {
        let sample_rate = 8_000;
        let samples = (0..2 * sample_rate as usize)
            .map(|frame| {
                let amplitude = if frame < sample_rate as usize {
                    0.08
                } else {
                    0.4
                };
                ((frame as f64 * 0.37).sin() * amplitude) as f32
            })
            .collect::<Vec<_>>();
        let mut analyzer = StreamingAnalyzer::with_timeline_interval(
            sample_rate,
            vec![ChannelRole::Main],
            Some(800),
        );
        for chunk in samples.chunks(137) {
            analyzer.process(&[chunk.to_vec()]).unwrap();
        }

        let frames = analyzer.frames;
        let raw_sum_squares = analyzer.raw_sum_squares;
        let weighted_sum_squares = analyzer.weighted_sum_squares;
        let sample_peak = analyzer.sample_peak;
        let true_peak = analyzer.true_peak_meters[0].peak();
        let gating_blocks = analyzer.gating_blocks.clone();
        let max_momentary_sum = analyzer.max_momentary_sum;
        let max_short_term_sum = analyzer.max_short_term_sum;
        let timeline = analyzer
            .timeline
            .iter()
            .map(|point| (point.start_seconds, point.end_seconds))
            .collect::<Vec<_>>();
        assert!(analyzer.short_term_blocks.is_empty());

        analyzer.append_finite_lra_tail();

        assert_eq!(analyzer.short_term_blocks.len(), 6);
        let tail_lra = loudness_range(&analyzer.short_term_blocks);
        assert!(
            tail_lra > 0.01,
            "LRA {tail_lra}, blocks {:?}",
            analyzer.short_term_blocks
        );
        assert_eq!(analyzer.frames, frames);
        assert_eq!(analyzer.raw_sum_squares, raw_sum_squares);
        assert_eq!(analyzer.weighted_sum_squares, weighted_sum_squares);
        assert_eq!(analyzer.sample_peak.to_bits(), sample_peak.to_bits());
        assert_eq!(
            analyzer.true_peak_meters[0].peak().to_bits(),
            true_peak.to_bits()
        );
        assert_eq!(analyzer.gating_blocks, gating_blocks);
        assert_eq!(
            analyzer.max_momentary_sum.to_bits(),
            max_momentary_sum.to_bits()
        );
        assert_eq!(
            analyzer.max_short_term_sum.to_bits(),
            max_short_term_sum.to_bits()
        );
        assert_eq!(
            analyzer
                .timeline
                .iter()
                .map(|point| (point.start_seconds, point.end_seconds))
                .collect::<Vec<_>>(),
            timeline
        );
    }

    #[test]
    fn dialogue_finish_skips_lra_tail_and_preserves_consumed_measurements() {
        let sample_rate = 8_000;
        let samples = (0..2 * sample_rate as usize)
            .map(|frame| {
                let amplitude = if frame < sample_rate as usize {
                    0.08
                } else {
                    0.4
                };
                ((frame as f64 * 0.37).sin() * amplitude) as f32
            })
            .collect::<Vec<_>>();
        let analyze = || {
            let mut analyzer = StreamingAnalyzer::new(sample_rate, vec![ChannelRole::Main]);
            for chunk in samples.chunks(137) {
                analyzer.process(&[chunk.to_vec()]).unwrap();
            }
            analyzer
        };

        let regular = analyze().finish();
        let dialogue = analyze().finish_without_lra_tail();

        assert!(regular.ebu.loudness_range_lu > 0.01);
        assert_eq!(dialogue.ebu.loudness_range_lu, 0.0);
        assert_eq!(dialogue.frames, regular.frames);
        assert_eq!(
            dialogue.weighted_mean_square.to_bits(),
            regular.weighted_mean_square.to_bits()
        );
        assert_eq!(dialogue.ebu.gating_blocks, regular.ebu.gating_blocks);
        assert_eq!(dialogue.rms_db.to_bits(), regular.rms_db.to_bits());
        assert_eq!(
            dialogue.sample_peak.to_bits(),
            regular.sample_peak.to_bits()
        );
        assert_eq!(dialogue.true_peak.to_bits(), regular.true_peak.to_bits());
    }

    #[test]
    fn finite_lra_tail_rounds_up_at_odd_sample_rates_and_handles_short_input() {
        let sample_rate = 11_025;
        let tail = lra_tail_frames(sample_rate);
        let short_term_window = sample_rate as usize * 3;
        assert_eq!(tail, 16_538);

        let measure_block_count = |frames| {
            let mut analyzer = StreamingAnalyzer::new(sample_rate, vec![ChannelRole::Main]);
            for chunk in vec![0.1; frames].chunks(997) {
                analyzer.process(&[chunk.to_vec()]).unwrap();
            }
            analyzer.append_finite_lra_tail();
            analyzer.short_term_blocks.len()
        };
        assert_eq!(measure_block_count(0), 0);
        assert_eq!(measure_block_count(short_term_window - tail - 1), 0);
        assert_eq!(measure_block_count(short_term_window - tail), 1);
    }

    #[test]
    fn finite_lra_tail_capacity_is_reserved_before_finish() {
        let sample_rate = 1_000;
        let reserve = lra_tail_block_reserve(sample_rate);
        assert_eq!(reserve, 15);
        let program_limit = MAX_LOUDNESS_BLOCKS - reserve;
        let mut blocks = vec![0.0; program_limit];
        let mut clock = RationalBlockClock::new(sample_rate, 30);
        let error = record_program_short_term_block(&mut blocks, &mut clock, 0.0).unwrap_err();
        assert!(error.contains("including the finite-file tail"));
        assert_eq!(blocks.len(), program_limit);

        blocks.pop();
        record_program_short_term_block(&mut blocks, &mut clock, 0.0).unwrap();
        assert_eq!(blocks.len(), program_limit);
    }

    #[test]
    fn rational_block_clock_matches_odd_rate_integer_oracles() {
        let cases = [
            (11_025, [4_410, 5_513, 6_615, 7_718, 8_820, 9_923]),
            (22_050, [8_820, 11_025, 13_230, 15_435, 17_640, 19_845]),
            (44_101, [17_640, 22_050, 26_460, 30_870, 35_280, 39_691]),
        ];

        for (sample_rate, expected_ends) in cases {
            let mut clock = RationalBlockClock::new(sample_rate, 4);
            let mut actual_ends = Vec::new();
            for _ in expected_ends {
                actual_ends.push(clock.next_block_frame());
                clock.advance().unwrap();
            }
            assert_eq!(actual_ends, expected_ends, "sample rate {sample_rate}");
        }
    }

    #[test]
    fn rational_block_clock_has_no_one_hour_phase_drift() {
        for (sample_rate, expected_end) in [
            (11_025, 39_694_410),
            (22_050, 79_388_820),
            (44_101, 158_781_240),
        ] {
            let mut clock = RationalBlockClock::new(sample_rate, 4);
            for _ in 0..36_000 {
                clock.advance().unwrap();
            }
            assert_eq!(
                clock.next_block_frame(),
                expected_end,
                "sample rate {sample_rate}"
            );
        }
    }

    #[test]
    fn odd_rate_batch_and_streaming_use_only_complete_oracle_blocks() {
        let sample_rate = 11_025;
        let final_complete_end = 9_923;
        let count_complete = |frames| {
            let mut clock = RationalBlockClock::new(sample_rate, 4);
            let mut count = 0;
            while clock.next_block_frame() <= frames {
                count += 1;
                clock.advance().unwrap();
            }
            count
        };
        assert_eq!(count_complete(final_complete_end), 6);
        assert_eq!(count_complete(final_complete_end - 1), 5);

        let complete = measure_ebu(&mono(vec![0.1; final_complete_end], sample_rate));
        let incomplete = measure_ebu(&mono(vec![0.1; final_complete_end - 1], sample_rate));
        assert_eq!(complete.gating_blocks.len(), 6);
        assert_eq!(incomplete.gating_blocks.len(), 5);

        let mut analyzer = StreamingAnalyzer::new(sample_rate, vec![ChannelRole::Main]);
        analyzer.process(&[vec![0.1; final_complete_end]]).unwrap();
        assert_eq!(analyzer.gating_blocks.len(), 6);
    }

    #[test]
    fn odd_rate_streaming_lra_matches_whole_file_across_chunking() {
        let sample_rate = 44_103;
        let frames = sample_rate as usize * 4 + 137;
        let left = (0..frames)
            .map(|frame| {
                let amplitude = if frame < frames / 2 { 0.05 } else { 0.35 };
                ((frame as f64 * 0.071).sin() * amplitude) as f32
            })
            .collect::<Vec<_>>();
        let right = (0..frames)
            .map(|frame| {
                let amplitude = if frame < frames / 3 { 0.3 } else { 0.09 };
                ((frame as f64 * 0.113).cos() * amplitude) as f32
            })
            .collect::<Vec<_>>();
        let roles = vec![ChannelRole::Main, ChannelRole::Surround];
        let buffer = AudioBuffer {
            sample_rate,
            channels: 2,
            frames,
            data: vec![left.clone(), right.clone()],
            channel_roles: roles.clone(),
            source_kind: PcmKind::F32,
        };
        let whole = measure_ebu(&buffer);

        for chunk_frames in [97, 137, 16_384] {
            let mut analyzer = StreamingAnalyzer::new(sample_rate, roles.clone());
            for start in (0..frames).step_by(chunk_frames) {
                let end = (start + chunk_frames).min(frames);
                analyzer
                    .process(&[left[start..end].to_vec(), right[start..end].to_vec()])
                    .unwrap();
            }
            let streamed = analyzer.finish().ebu;
            assert!(
                (streamed.integrated_lufs - whole.integrated_lufs).abs() < 1.0e-6,
                "chunk={chunk_frames}: streamed={}, whole={}",
                streamed.integrated_lufs,
                whole.integrated_lufs
            );
            assert!(
                (streamed.loudness_range_lu - whole.loudness_range_lu).abs() < 1.0e-6,
                "chunk={chunk_frames}: streamed={}, whole={}",
                streamed.loudness_range_lu,
                whole.loudness_range_lu
            );
        }
    }

    #[test]
    fn streaming_finite_true_peak_drains_the_fir_tail() {
        let mut samples = vec![0.0; 16];
        samples.extend([-1.0, -1.0]);
        let buffer = mono(samples.clone(), 48_000);
        let expected = crate::dsp::truepeak::measure_true_peak(&buffer);

        let mut analyzer = StreamingAnalyzer::new(48_000, vec![ChannelRole::Main]);
        for chunk in samples.chunks(3) {
            analyzer.process(&[chunk.to_vec()]).unwrap();
        }
        let measured = analyzer.finish();
        assert!(expected > 1.2, "finite FIR tail peak was only {expected}");
        assert_eq!(measured.true_peak.to_bits(), expected.to_bits());
    }

    #[test]
    fn finite_true_peak_tail_is_attributed_to_the_last_timeline_interval() {
        let mut samples = vec![0.0; 16];
        samples.extend([-1.0, -1.0]);
        let expected = crate::dsp::truepeak::measure_true_peak(&mono(samples.clone(), 48_000));
        assert!(expected > 1.2, "finite FIR tail peak was only {expected}");

        // Cover both ways a final timeline point can arise: it may already
        // have been emitted on an exact interval boundary, or `finish` may
        // need to emit a partial interval.
        for interval_frames in [samples.len(), samples.len() + 7] {
            let mut analyzer = StreamingAnalyzer::with_timeline_interval(
                48_000,
                vec![ChannelRole::Main],
                Some(interval_frames),
            );
            for chunk in samples.chunks(3) {
                analyzer.process(&[chunk.to_vec()]).unwrap();
            }
            let measured = analyzer.finish();

            assert_eq!(measured.timeline.len(), 1, "interval {interval_frames}");
            assert_eq!(
                measured.timeline[0].true_peak_dbtp.to_bits(),
                amplitude_db(expected).to_bits(),
                "interval {interval_frames}"
            );
            assert_eq!(measured.true_peak.to_bits(), expected.to_bits());
        }
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
    fn streaming_analyzer_rejects_non_finite_chunks_before_mutating_state() {
        let roles = vec![ChannelRole::Main, ChannelRole::Main];
        let prefix = vec![vec![0.1; 137], vec![-0.2; 137]];
        let suffix = vec![vec![0.3; 211], vec![-0.4; 211]];
        let rejected = vec![
            vec![0.5, 0.5, f32::INFINITY, 0.5],
            vec![0.25, f32::NAN, 0.25, f32::NEG_INFINITY],
        ];

        let mut candidate = StreamingAnalyzer::new(48_000, roles.clone());
        candidate.process(&prefix).unwrap();
        let error = candidate.process(&rejected).unwrap_err();
        assert_eq!(error, "non-finite sample at frame 138, channel 1");
        candidate.process(&suffix).unwrap();

        let mut reference = StreamingAnalyzer::new(48_000, roles);
        reference.process(&prefix).unwrap();
        reference.process(&suffix).unwrap();

        let candidate = candidate.finish();
        let reference = reference.finish();
        assert_eq!(candidate.frames, reference.frames);
        assert_eq!(
            candidate.weighted_mean_square.to_bits(),
            reference.weighted_mean_square.to_bits()
        );
        assert_eq!(candidate.rms_db.to_bits(), reference.rms_db.to_bits());
        assert_eq!(
            candidate.sample_peak.to_bits(),
            reference.sample_peak.to_bits()
        );
        assert_eq!(candidate.true_peak.to_bits(), reference.true_peak.to_bits());
        assert_eq!(
            candidate.ebu.integrated_lufs.to_bits(),
            reference.ebu.integrated_lufs.to_bits()
        );
        assert_eq!(candidate.ebu.gating_blocks, reference.ebu.gating_blocks);

        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut analyzer = StreamingAnalyzer::new(48_000, vec![ChannelRole::Main]);
            let error = analyzer.process(&[vec![0.0, value]]).unwrap_err();
            assert_eq!(error, "non-finite sample at frame 1, channel 0");
            assert_eq!(analyzer.finish().frames, 0);
        }
    }

    #[test]
    fn typed_chunks_reject_values_atomically() {
        let roles = vec![ChannelRole::Main];
        let prefix = vec![vec![1_024_i16; 137]];
        let suffix = vec![vec![-2_048_i16; 211]];

        let mut candidate = StreamingAnalyzer::new(48_000, roles.clone());
        candidate.process_i16(&prefix).unwrap();
        let error = candidate
            .process_f64(&[vec![0.25, f64::NAN, 0.5]])
            .unwrap_err();
        assert_eq!(error, "non-finite sample at frame 138, channel 0");
        let error = candidate.process_s24(&[vec![0, 8_388_608, 0]]).unwrap_err();
        assert_eq!(
            error,
            "sample outside the signed 24-bit range at frame 138, channel 0"
        );
        candidate.process_i16(&suffix).unwrap();

        let mut reference = StreamingAnalyzer::new(48_000, roles);
        reference.process_i16(&prefix).unwrap();
        reference.process_i16(&suffix).unwrap();
        let candidate = candidate.finish();
        let reference = reference.finish();
        assert_eq!(candidate.frames, reference.frames);
        assert_eq!(
            candidate.weighted_mean_square.to_bits(),
            reference.weighted_mean_square.to_bits()
        );
        assert_eq!(candidate.rms_db.to_bits(), reference.rms_db.to_bits());
        assert_eq!(
            candidate.sample_peak.to_bits(),
            reference.sample_peak.to_bits()
        );
        assert_eq!(candidate.true_peak.to_bits(), reference.true_peak.to_bits());
        assert_eq!(candidate.ebu.gating_blocks, reference.ebu.gating_blocks);
    }

    #[test]
    fn scalar_i32_ingress_preserves_adjacent_source_codes_in_energy() {
        let base = 1_073_741_824_i32;
        assert_eq!((base as f32).to_bits(), ((base + 1) as f32).to_bits());
        let frames = 8_000;
        let measure = |sample| {
            let mut analyzer = StreamingAnalyzer::new(8_000, vec![ChannelRole::Main]);
            analyzer.process_i32(&[vec![sample; frames]]).unwrap();
            analyzer.finish().weighted_mean_square
        };
        assert_ne!(measure(base).to_bits(), measure(base + 1).to_bits());
    }

    #[test]
    fn every_typed_ingress_accepts_a_complete_finite_chunk() {
        let frames = |analyzer: StreamingAnalyzer| analyzer.finish().frames;

        let mut u8_analyzer = StreamingAnalyzer::new(8_000, vec![ChannelRole::Main]);
        u8_analyzer.process_u8(&[vec![128; 3]]).unwrap();
        assert_eq!(frames(u8_analyzer), 3);

        let mut i16_analyzer = StreamingAnalyzer::new(8_000, vec![ChannelRole::Main]);
        i16_analyzer.process_i16(&[vec![0; 3]]).unwrap();
        assert_eq!(frames(i16_analyzer), 3);

        let mut s24_analyzer = StreamingAnalyzer::new(8_000, vec![ChannelRole::Main]);
        s24_analyzer.process_s24(&[vec![0; 3]]).unwrap();
        assert_eq!(frames(s24_analyzer), 3);

        let mut i32_analyzer = StreamingAnalyzer::new(8_000, vec![ChannelRole::Main]);
        i32_analyzer.process_i32(&[vec![0; 3]]).unwrap();
        assert_eq!(frames(i32_analyzer), 3);

        let mut f64_analyzer = StreamingAnalyzer::new(8_000, vec![ChannelRole::Main]);
        f64_analyzer.process_f64(&[vec![0.0; 3]]).unwrap();
        assert_eq!(frames(f64_analyzer), 3);
    }

    #[test]
    fn reference_analysis_is_chunk_invariant_and_canonicalized() {
        let frames = 240_137;
        let mut state = 0x6a09_e667_f3bc_c909_u64;
        let mut left = Vec::with_capacity(frames);
        let mut right = Vec::with_capacity(frames);
        for frame in 0..frames {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let code = ((state >> 40) as i32) - (1 << 23);
            left.push(code as f32 / (1 << 24) as f32);
            let alternate = if frame.is_multiple_of(997) {
                -code
            } else {
                code / 3
            };
            right.push(alternate as f32 / (1 << 24) as f32);
        }
        let roles = vec![ChannelRole::Main, ChannelRole::Surround];
        let mut whole =
            ReferenceStreamingAnalyzer::with_timeline_interval(48_000, roles.clone(), Some(4_800))
                .unwrap();
        whole.process(&[left.clone(), right.clone()]).unwrap();
        let whole = whole.finish();

        let mut chunked =
            ReferenceStreamingAnalyzer::with_timeline_interval(48_000, roles, Some(4_800)).unwrap();
        let mut start = 0;
        while start < frames {
            let end = (start + 1_009).min(frames);
            chunked
                .process(&[left[start..end].to_vec(), right[start..end].to_vec()])
                .unwrap();
            start = end;
        }
        let chunked = chunked.finish();
        assert_eq!(whole.frames, chunked.frames);
        assert_eq!(whole.ebu.integrated_lufs, chunked.ebu.integrated_lufs);
        assert_eq!(whole.ebu.max_momentary_lufs, chunked.ebu.max_momentary_lufs);
        assert_eq!(
            whole.ebu.max_short_term_lufs,
            chunked.ebu.max_short_term_lufs
        );
        assert_eq!(whole.ebu.loudness_range_lu, chunked.ebu.loudness_range_lu);
        assert_eq!(whole.rms_db, chunked.rms_db);
        assert_eq!(whole.sample_peak.to_bits(), chunked.sample_peak.to_bits());
        assert_eq!(whole.true_peak.to_bits(), chunked.true_peak.to_bits());
        assert_eq!(whole.timeline.len(), chunked.timeline.len());
        assert_eq!(
            whole.ebu.integrated_lufs / REFERENCE_DB_QUANTUM,
            (whole.ebu.integrated_lufs / REFERENCE_DB_QUANTUM).round()
        );
        assert_eq!(whole.ebu.integrated_lufs.to_bits(), 0xc01c_1f6b_5b87_7eff);
        assert_eq!(
            whole.ebu.max_momentary_lufs.to_bits(),
            0xc01b_df25_a4bb_71ed
        );
        assert_eq!(
            whole.ebu.max_short_term_lufs.to_bits(),
            0xc01c_17d4_2d7a_72ca
        );
        assert_eq!(whole.ebu.loudness_range_lu.to_bits(), 0x3fff_a300_142a_8cfb);
        assert_eq!(whole.rms_db.to_bits(), 0xc02a_b5d2_f9d1_568f);
        assert_eq!(whole.sample_peak.to_bits(), 0x3eff_ffc0);
        assert_eq!(whole.true_peak.to_bits(), 0x3f4e_f7d8);
    }

    #[test]
    fn reference_analysis_rejects_uncommitted_sample_rates() {
        let error = ReferenceStreamingAnalyzer::new(32_000, vec![ChannelRole::Main])
            .err()
            .expect("32 kHz has no committed reference vector");
        assert!(error.contains("supports sample rates"));
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
                    let mut analyzer = StreamingAnalyzer::new(44_100, roles.clone());
                    analyzer.process(&planar).unwrap();
                    analyzer.finish()
                })
        };

        // One worker forces the fused loop; two workers meet the benchmarked
        // 5x long-chunk policy and split True Peak from the loudness pass.
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
