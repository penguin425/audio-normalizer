//! Sample-rate-aware true-peak meter (ITU-R BS.1770-5 style).
//!
//! Sample peaks (max of the discrete samples) miss inter-sample peaks that can
//! exceed 0 dBFS after DAC reconstruction. True peak is measured by
//! oversampling each channel with a polyphase FIR and taking the maximum
//! absolute interpolated value. The Kaiser-windowed lowpass is normalized to
//! unity DC gain; this is the same approach the ITU reference takes and gives
//! accurate inter-sample peaks at a fraction of the cost of a full oversampled
//! reconstruction. ITU-R BS.1770-5 permits proportionately less oversampling at
//! higher input rates, so the meter uses 4x below 96 kHz, 2x below 192 kHz, and
//! sample peak at 192 kHz and above.
//!
//! Each normalized 2x/4x coefficient table is computed once via a `OnceLock`.

use crate::wav::AudioBuffer;
use rayon::prelude::*;
use std::f64::consts::PI;
use std::sync::OnceLock;

#[cfg(target_arch = "aarch64")]
use std::arch::aarch64::*;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

pub(super) const MAX_PHASES: usize = 4;
pub(super) const TAPS_PER_PHASE: usize = 16;
const HISTORY_SAMPLES: usize = TAPS_PER_PHASE * 2;
pub(super) type PhaseTable = [[f64; MAX_PHASES]; TAPS_PER_PHASE];

pub(super) fn phase_table(factor: usize) -> &'static PhaseTable {
    static X2: OnceLock<PhaseTable> = OnceLock::new();
    static X4: OnceLock<PhaseTable> = OnceLock::new();
    match factor {
        2 => X2.get_or_init(|| build_phase_table(2)),
        4 => X4.get_or_init(|| build_phase_table(4)),
        _ => unreachable!("true-peak interpolation factor must be 2 or 4"),
    }
}

fn interpolation_bound_scale(factor: usize) -> f64 {
    static X2: OnceLock<f64> = OnceLock::new();
    static X4: OnceLock<f64> = OnceLock::new();
    let build = || {
        let table = phase_table(factor);
        let mut maximum_l1 = 0.0_f64;
        for phase in 0..factor {
            let phase_l1 = table
                .iter()
                .map(|coefficients| coefficients[phase].abs())
                .sum::<f64>();
            maximum_l1 = maximum_l1.max(phase_l1);
        }
        // The coefficients and samples are exactly representable f64 values,
        // but the L1 sum, product, and 16 chained FMAs still round. Inflate the
        // triangle-inequality bound well beyond their worst-case error before
        // using it to skip interpolation.
        maximum_l1 * (1.0 + 64.0 * f64::EPSILON)
    };
    match factor {
        2 => *X2.get_or_init(build),
        4 => *X4.get_or_init(build),
        _ => unreachable!("true-peak interpolation factor must be 2 or 4"),
    }
}

/// Conservative upper bound for the True Peak of a signal whose largest
/// discrete sample magnitude is `sample_peak`.
///
/// This uses the triangle inequality for every FIR phase and includes a wide
/// floating-point safety margin in [`interpolation_bound_scale`]. Callers can
/// therefore use the result to prove that an exact True Peak meter cannot
/// cross a ceiling without running the meter over the signal again.
pub(crate) fn upper_bound_from_sample_peak(sample_rate: u32, sample_peak: f32) -> f64 {
    if !sample_peak.is_finite() || sample_peak < 0.0 {
        return f64::INFINITY;
    }
    let factor = oversample_factor(sample_rate);
    let sample_peak = f64::from(sample_peak);
    if factor > 1 {
        sample_peak * interpolation_bound_scale(factor).max(1.0)
    } else {
        sample_peak
    }
}

fn build_phase_table(factor: usize) -> PhaseTable {
    debug_assert!(matches!(factor, 2 | 4));
    let coefficients = {
        let len = factor * TAPS_PER_PHASE;
        let center = (len - 1) as f64 / 2.0;
        let beta = 8.5;
        let i0b = bessel_i0(beta);
        let fc = 0.5 / factor as f64;
        let mut h = vec![0.0f32; len];
        for (k, slot) in h.iter_mut().enumerate() {
            let r = k as f64 - center;
            let s = if r.abs() < 1e-12 {
                1.0
            } else {
                (2.0 * PI * fc * r).sin() / (PI * r)
            };
            let arg = 1.0 - (r / center) * (r / center);
            let w = if arg <= 0.0 {
                0.0
            } else {
                bessel_i0(beta * arg.sqrt()) / i0b
            };
            *slot = (s * w) as f32;
        }
        // Normalize so the full filter sums to the interpolation factor (=> each
        // polyphase phase has unity DC gain).
        let sum: f64 = h.iter().map(|&x| x as f64).sum();
        let g = factor as f64 / sum;
        for v in h.iter_mut() {
            *v = (*v as f64 * g) as f32;
        }
        h
    };
    let mut table = [[0.0; MAX_PHASES]; TAPS_PER_PHASE];
    for phase in 0..factor {
        for tap in 0..TAPS_PER_PHASE {
            table[tap][phase] = coefficients[phase + factor * tap] as f64;
        }
    }
    table
}

/// Modified Bessel function of the first kind, order 0 (series).
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let half = x / 2.0;
    for m in 1..=50 {
        term *= (half / m as f64) * (half / m as f64);
        sum += term;
        if term < 1e-16 * sum {
            break;
        }
    }
    sum
}

/// Max true-peak (0..1) across all channels.
pub fn measure_true_peak(buf: &AudioBuffer) -> f32 {
    if buf.frames == 0 {
        return 0.0;
    }
    buf.data
        .par_iter()
        .map(|ch| {
            let mut meter = TruePeakMeter::for_sample_rate(buf.sample_rate);
            meter.process(ch);
            meter.peak()
        })
        .reduce(|| 0.0f32, f32::max)
}

/// Stateful true-peak meter that accepts consecutive sample chunks.
pub struct TruePeakMeter {
    history: [f64; HISTORY_SAMPLES],
    cursor: usize,
    initialized: bool,
    factor: usize,
    interpolation_bound_scale: f64,
    pruning_active: bool,
    pruning_probe_max: f32,
    pruning_probe_len: u8,
    pruning_probe_delay: u8,
    #[cfg(target_arch = "x86_64")]
    use_avx2_fma: bool,
    peak: f32,
}

impl Default for TruePeakMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl TruePeakMeter {
    /// Construct a 48 kHz-compatible meter for API compatibility.
    ///
    /// Call [`Self::for_sample_rate`] when the input rate is known.
    pub fn new() -> Self {
        Self::for_sample_rate(48_000)
    }

    /// Construct a meter using the minimum BS.1770 oversampling ratio that
    /// reaches the 192 kHz true-peak measurement domain.
    pub fn for_sample_rate(sample_rate: u32) -> Self {
        let factor = oversample_factor(sample_rate);
        if factor > 1 {
            // Build the shared FIR table before this meter reaches a processing
            // callback. Subsequent `process` calls are allocation-free.
            let _ = phase_table(factor);
        }
        Self {
            history: [0.0; HISTORY_SAMPLES],
            cursor: 0,
            initialized: false,
            factor,
            interpolation_bound_scale: if factor > 1 {
                interpolation_bound_scale(factor)
            } else {
                0.0
            },
            pruning_active: false,
            pruning_probe_max: 0.0,
            pruning_probe_len: 0,
            pruning_probe_delay: 0,
            #[cfg(target_arch = "x86_64")]
            use_avx2_fma: is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma"),
            peak: 0.0,
        }
    }

    /// Rebuild the FIR history needed for the next sample after an optional
    /// accelerator hands a streaming measurement back to the CPU. At most the
    /// preceding 15 samples are required because the next 16-tap window starts
    /// with the new sample. `peak_floor` retains all earlier completed chunks.
    #[cfg(all(
        feature = "cuda-truepeak",
        any(target_os = "linux", target_os = "windows")
    ))]
    pub(super) fn from_recent_samples(sample_rate: u32, recent: &[f32], peak_floor: f32) -> Self {
        debug_assert!(recent.len() < TAPS_PER_PHASE);
        let mut meter = Self::for_sample_rate(sample_rate);
        meter.process(recent);
        // Replaying only the retained suffix seeds the exact 16-tap history for
        // the *next* sample, but its artificial repeated first sample is not a
        // real part of the stream. CUDA already measured the complete prefix,
        // so discard any warm-up peak and restore that authoritative value.
        meter.peak = peak_floor;
        meter
    }

    pub fn process(&mut self, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }
        if self.try_skip_peak_only_block(samples) {
            return;
        }
        for &sample in samples {
            self.process_peak_only_sample(sample);
        }
    }

    /// Advance a complete peak-only block after one conservative SIMD
    /// reduction proves that no FIR phase can exceed the retained maximum.
    /// Only the final 16 samples can affect a future window, so the circular
    /// history can be replaced directly instead of being advanced per sample.
    #[inline]
    pub(crate) fn try_skip_peak_only_block(&mut self, samples: &[f32]) -> bool {
        if samples.is_empty() || self.factor <= 1 || !self.pruning_active {
            return false;
        }
        let (block_maximum, has_nan) = crate::dsp::simd::abs_max_and_has_nan(samples);
        if has_nan {
            return false;
        }
        let peak_floor = self.peak.max(block_maximum);
        if f64::from(block_maximum) * self.interpolation_bound_scale > f64::from(peak_floor) {
            return false;
        }

        self.peak = peak_floor;
        if samples.len() < TAPS_PER_PHASE {
            for &sample in samples {
                self.push_history(sample);
            }
        } else {
            let tail = &samples[samples.len() - TAPS_PER_PHASE..];
            self.cursor = 0;
            for (destination, &sample) in self.history[..TAPS_PER_PHASE]
                .iter_mut()
                .zip(tail.iter().rev())
            {
                *destination = f64::from(sample);
            }
            let (first, second) = self.history.split_at_mut(TAPS_PER_PHASE);
            second.copy_from_slice(first);
            self.initialized = true;
        }
        true
    }

    #[inline(always)]
    fn push_history(&mut self, sample: f32) {
        let sample = sample as f64;
        if self.initialized {
            self.cursor = self.cursor.wrapping_sub(1) & (TAPS_PER_PHASE - 1);
            self.history[self.cursor] = sample;
            self.history[self.cursor + TAPS_PER_PHASE] = sample;
        } else {
            self.history.fill(sample);
            self.initialized = true;
        }
    }

    #[inline(always)]
    fn history_window(&self) -> &[f64; TAPS_PER_PHASE] {
        self.history[self.cursor..self.cursor + TAPS_PER_PHASE]
            .try_into()
            .expect("true-peak history window has a fixed length")
    }

    #[inline(always)]
    fn sample_within_pruning_bound(&self, sample: f32, peak_floor: f32) -> bool {
        f64::from(sample).abs() * self.interpolation_bound_scale <= f64::from(peak_floor)
    }

    #[inline(always)]
    fn invalidate_pruning_probe(&mut self) {
        // Limiter and timeline meters stay exact for their entire lifetime.
        // Keep that hot path to one predictable false branch instead of four
        // stores per sample; only meters switching from peak-only mode carry
        // non-default probe state that must be cleared.
        if self.pruning_active || self.pruning_probe_len != 0 || self.pruning_probe_delay != 0 {
            self.pruning_active = false;
            self.pruning_probe_max = 0.0;
            self.pruning_probe_len = 0;
            self.pruning_probe_delay = 0;
        }
    }

    #[inline(always)]
    fn record_exact_pruning_sample(&mut self, sample: f32) {
        self.pruning_active = false;
        if self.pruning_probe_len == 0 {
            // Exact interpolation remains the fast path for dense material.
            // Probe only once per 256 samples; including the 16-sample proof,
            // after a transient this delays activation by at most about 5.7 ms
            // at 48 kHz while avoiding a max reduction on every dense frame.
            self.pruning_probe_delay = self.pruning_probe_delay.wrapping_add(1);
            if self.pruning_probe_delay != 0 {
                return;
            }
            self.pruning_probe_max = sample.abs();
            self.pruning_probe_len = 1;
            return;
        }
        self.pruning_probe_max = self.pruning_probe_max.max(sample.abs());
        self.pruning_probe_len += 1;
        if self.pruning_probe_len as usize == TAPS_PER_PHASE {
            self.pruning_active = f64::from(self.pruning_probe_max)
                * self.interpolation_bound_scale
                <= f64::from(self.peak);
            self.pruning_probe_max = 0.0;
            self.pruning_probe_len = 0;
            self.pruning_probe_delay = 0;
        }
    }

    pub(crate) fn process_sample(&mut self, sample: f32) -> f32 {
        let mut frame_peak = sample.abs();
        if self.factor > 1 {
            self.push_history(sample);
            // Exact frame-level callers may be interleaved with peak-only
            // callers. Restart the probe so a later peak-only call cannot rely
            // on samples it did not classify.
            self.invalidate_pruning_probe();
            let history = self.history_window();
            let table = phase_table(self.factor);
            #[cfg(target_arch = "x86_64")]
            let interpolated = if self.use_avx2_fma {
                // SAFETY: the constructor performed runtime AVX2/FMA detection.
                unsafe {
                    if self.factor == 2 {
                        interpolate_2x_avx2_fma(history, table)
                    } else {
                        interpolate_avx2_fma(history, table)
                    }
                }
            } else {
                interpolate_scalar(history, table, self.factor)
            };
            #[cfg(target_arch = "aarch64")]
            let interpolated = {
                // SAFETY: Advanced SIMD is part of the AArch64 architecture.
                unsafe {
                    if self.factor == 2 {
                        interpolate_2x_neon(history, table)
                    } else {
                        interpolate_neon(history, table)
                    }
                }
            };
            #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
            let interpolated = interpolate_scalar(history, table, self.factor);
            for value in &interpolated[..self.factor] {
                frame_peak = frame_peak.max(value.abs() as f32);
            }
        }
        self.peak = self.peak.max(frame_peak);
        frame_peak
    }

    /// Advance one sample when only the all-time maximum is needed. Once a
    /// previous peak exceeds a conservative triangle-inequality bound for the
    /// current 16-sample FIR window, interpolation cannot change the result.
    /// The history is still advanced exactly, so later frames are unaffected.
    #[inline]
    pub(crate) fn process_peak_only_sample(&mut self, sample: f32) -> bool {
        let mut frame_peak = sample.abs();
        if self.factor > 1 {
            self.push_history(sample);
            let peak_floor = self.peak.max(frame_peak);
            if self.pruning_active && self.sample_within_pruning_bound(sample, peak_floor) {
                self.peak = peak_floor;
                return true;
            }
            let history = self.history_window();
            let table = phase_table(self.factor);
            #[cfg(target_arch = "x86_64")]
            let interpolated = if self.use_avx2_fma {
                // SAFETY: the constructor performed runtime AVX2/FMA detection.
                unsafe {
                    if self.factor == 2 {
                        interpolate_2x_avx2_fma(history, table)
                    } else {
                        interpolate_avx2_fma(history, table)
                    }
                }
            } else {
                interpolate_scalar(history, table, self.factor)
            };
            #[cfg(target_arch = "aarch64")]
            let interpolated = {
                // SAFETY: Advanced SIMD is part of the AArch64 architecture.
                unsafe {
                    if self.factor == 2 {
                        interpolate_2x_neon(history, table)
                    } else {
                        interpolate_neon(history, table)
                    }
                }
            };
            #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
            let interpolated = interpolate_scalar(history, table, self.factor);
            for value in &interpolated[..self.factor] {
                frame_peak = frame_peak.max(value.abs() as f32);
            }
        }
        self.peak = self.peak.max(frame_peak);
        if self.factor > 1 {
            self.record_exact_pruning_sample(sample);
        }
        false
    }

    /// Process one stereo frame while sharing the immutable phase-coefficient
    /// loads. Each channel keeps its own history, FMA accumulator, maximum
    /// reduction, and meter peak, so the result is bit-identical to two
    /// consecutive [`Self::process_sample`] calls.
    #[inline]
    pub(crate) fn process_stereo_sample(
        left: &mut Self,
        right: &mut Self,
        left_sample: f32,
        right_sample: f32,
    ) -> (f32, f32) {
        if left.factor != right.factor {
            return (
                left.process_sample(left_sample),
                right.process_sample(right_sample),
            );
        }

        let mut left_frame_peak = left_sample.abs();
        let mut right_frame_peak = right_sample.abs();
        if left.factor > 1 {
            left.push_history(left_sample);
            right.push_history(right_sample);
            left.invalidate_pruning_probe();
            right.invalidate_pruning_probe();
            let left_history = left.history_window();
            let right_history = right.history_window();
            let table = phase_table(left.factor);
            #[cfg(target_arch = "x86_64")]
            let (left_interpolated, right_interpolated) = if left.use_avx2_fma {
                // SAFETY: both meters run on this process and the constructor
                // performed runtime AVX2/FMA detection.
                unsafe {
                    if left.factor == 2 {
                        interpolate_stereo_2x_avx2_fma(left_history, right_history, table)
                    } else {
                        interpolate_stereo_avx2_fma(left_history, right_history, table)
                    }
                }
            } else {
                interpolate_stereo_scalar(left_history, right_history, table, left.factor)
            };
            #[cfg(target_arch = "aarch64")]
            let (left_interpolated, right_interpolated) = {
                // SAFETY: Advanced SIMD is part of the AArch64 architecture.
                unsafe {
                    if left.factor == 2 {
                        interpolate_stereo_2x_neon(left_history, right_history, table)
                    } else {
                        interpolate_stereo_neon(left_history, right_history, table)
                    }
                }
            };
            #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
            let (left_interpolated, right_interpolated) =
                interpolate_stereo_scalar(left_history, right_history, table, left.factor);

            for value in &left_interpolated[..left.factor] {
                left_frame_peak = left_frame_peak.max(value.abs() as f32);
            }
            for value in &right_interpolated[..right.factor] {
                right_frame_peak = right_frame_peak.max(value.abs() as f32);
            }
        }
        left.peak = left.peak.max(left_frame_peak);
        right.peak = right.peak.max(right_frame_peak);
        (left_frame_peak, right_frame_peak)
    }

    /// Stereo counterpart of [`Self::process_peak_only_sample`]. Coefficient
    /// loads remain shared when both channels require interpolation; if only
    /// one can exceed its retained peak, only that channel is reconstructed.
    #[inline]
    pub(crate) fn process_stereo_peak_only_sample(
        left: &mut Self,
        right: &mut Self,
        left_sample: f32,
        right_sample: f32,
    ) -> (bool, bool) {
        if left.factor != right.factor {
            return (
                left.process_peak_only_sample(left_sample),
                right.process_peak_only_sample(right_sample),
            );
        }

        let mut left_frame_peak = left_sample.abs();
        let mut right_frame_peak = right_sample.abs();
        if left.factor == 1 {
            left.peak = left.peak.max(left_frame_peak);
            right.peak = right.peak.max(right_frame_peak);
            return (false, false);
        }

        left.push_history(left_sample);
        right.push_history(right_sample);
        let left_floor = left.peak.max(left_frame_peak);
        let right_floor = right.peak.max(right_frame_peak);
        let skip_left =
            left.pruning_active && left.sample_within_pruning_bound(left_sample, left_floor);
        let skip_right =
            right.pruning_active && right.sample_within_pruning_bound(right_sample, right_floor);
        if skip_left && skip_right {
            left.peak = left_floor;
            right.peak = right_floor;
            return (true, true);
        }

        let left_history = left.history_window();
        let right_history = right.history_window();
        let table = phase_table(left.factor);
        if !skip_left && !skip_right {
            #[cfg(target_arch = "x86_64")]
            let (left_interpolated, right_interpolated) = if left.use_avx2_fma {
                // SAFETY: both constructors performed runtime AVX2/FMA detection.
                unsafe {
                    if left.factor == 2 {
                        interpolate_stereo_2x_avx2_fma(left_history, right_history, table)
                    } else {
                        interpolate_stereo_avx2_fma(left_history, right_history, table)
                    }
                }
            } else {
                interpolate_stereo_scalar(left_history, right_history, table, left.factor)
            };
            #[cfg(target_arch = "aarch64")]
            let (left_interpolated, right_interpolated) = {
                // SAFETY: Advanced SIMD is part of the AArch64 architecture.
                unsafe {
                    if left.factor == 2 {
                        interpolate_stereo_2x_neon(left_history, right_history, table)
                    } else {
                        interpolate_stereo_neon(left_history, right_history, table)
                    }
                }
            };
            #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
            let (left_interpolated, right_interpolated) =
                interpolate_stereo_scalar(left_history, right_history, table, left.factor);
            for value in &left_interpolated[..left.factor] {
                left_frame_peak = left_frame_peak.max(value.abs() as f32);
            }
            for value in &right_interpolated[..right.factor] {
                right_frame_peak = right_frame_peak.max(value.abs() as f32);
            }
        } else if !skip_left {
            #[cfg(target_arch = "x86_64")]
            let interpolated = if left.use_avx2_fma {
                // SAFETY: the constructor performed runtime AVX2/FMA detection.
                unsafe {
                    if left.factor == 2 {
                        interpolate_2x_avx2_fma(left_history, table)
                    } else {
                        interpolate_avx2_fma(left_history, table)
                    }
                }
            } else {
                interpolate_scalar(left_history, table, left.factor)
            };
            #[cfg(target_arch = "aarch64")]
            let interpolated = {
                // SAFETY: Advanced SIMD is part of the AArch64 architecture.
                unsafe {
                    if left.factor == 2 {
                        interpolate_2x_neon(left_history, table)
                    } else {
                        interpolate_neon(left_history, table)
                    }
                }
            };
            #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
            let interpolated = interpolate_scalar(left_history, table, left.factor);
            for value in &interpolated[..left.factor] {
                left_frame_peak = left_frame_peak.max(value.abs() as f32);
            }
        } else {
            #[cfg(target_arch = "x86_64")]
            let interpolated = if right.use_avx2_fma {
                // SAFETY: the constructor performed runtime AVX2/FMA detection.
                unsafe {
                    if right.factor == 2 {
                        interpolate_2x_avx2_fma(right_history, table)
                    } else {
                        interpolate_avx2_fma(right_history, table)
                    }
                }
            } else {
                interpolate_scalar(right_history, table, right.factor)
            };
            #[cfg(target_arch = "aarch64")]
            let interpolated = {
                // SAFETY: Advanced SIMD is part of the AArch64 architecture.
                unsafe {
                    if right.factor == 2 {
                        interpolate_2x_neon(right_history, table)
                    } else {
                        interpolate_neon(right_history, table)
                    }
                }
            };
            #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
            let interpolated = interpolate_scalar(right_history, table, right.factor);
            for value in &interpolated[..right.factor] {
                right_frame_peak = right_frame_peak.max(value.abs() as f32);
            }
        }
        left.peak = left.peak.max(left_frame_peak);
        right.peak = right.peak.max(right_frame_peak);
        if !skip_left {
            left.record_exact_pruning_sample(left_sample);
        }
        if !skip_right {
            right.record_exact_pruning_sample(right_sample);
        }
        (skip_left, skip_right)
    }

    pub const fn peak(&self) -> f32 {
        self.peak
    }
}

#[inline]
pub(super) fn oversample_factor(sample_rate: u32) -> usize {
    if sample_rate < 96_000 {
        4
    } else if sample_rate < 192_000 {
        2
    } else {
        1
    }
}

#[inline]
fn interpolate_scalar(
    history: &[f64; TAPS_PER_PHASE],
    table: &PhaseTable,
    factor: usize,
) -> [f64; MAX_PHASES] {
    let mut output = [0.0; MAX_PHASES];
    for tap in 0..TAPS_PER_PHASE {
        for phase in 0..factor {
            output[phase] += table[tap][phase] * history[tap];
        }
    }
    output
}

#[inline]
fn interpolate_stereo_scalar(
    left_history: &[f64; TAPS_PER_PHASE],
    right_history: &[f64; TAPS_PER_PHASE],
    table: &PhaseTable,
    factor: usize,
) -> ([f64; MAX_PHASES], [f64; MAX_PHASES]) {
    let mut left_output = [0.0; MAX_PHASES];
    let mut right_output = [0.0; MAX_PHASES];
    for tap in 0..TAPS_PER_PHASE {
        for phase in 0..factor {
            let coefficient = table[tap][phase];
            left_output[phase] += coefficient * left_history[tap];
            right_output[phase] += coefficient * right_history[tap];
        }
    }
    (left_output, right_output)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn interpolate_2x_avx2_fma(
    history: &[f64; TAPS_PER_PHASE],
    table: &PhaseTable,
) -> [f64; MAX_PHASES] {
    let mut accumulator = _mm_setzero_pd();
    for tap in 0..TAPS_PER_PHASE {
        let sample = _mm_set1_pd(*history.get_unchecked(tap));
        let coefficients = _mm_loadu_pd(table.get_unchecked(tap).as_ptr());
        accumulator = _mm_fmadd_pd(sample, coefficients, accumulator);
    }
    let mut output = [0.0; MAX_PHASES];
    _mm_storeu_pd(output.as_mut_ptr(), accumulator);
    output
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn interpolate_avx2_fma(
    history: &[f64; TAPS_PER_PHASE],
    table: &PhaseTable,
) -> [f64; MAX_PHASES] {
    let mut accumulator = _mm256_setzero_pd();
    for tap in 0..TAPS_PER_PHASE {
        let sample = _mm256_set1_pd(*history.get_unchecked(tap));
        let coefficients = _mm256_loadu_pd(table.get_unchecked(tap).as_ptr());
        accumulator = _mm256_fmadd_pd(sample, coefficients, accumulator);
    }
    let mut output = [0.0; MAX_PHASES];
    _mm256_storeu_pd(output.as_mut_ptr(), accumulator);
    output
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn interpolate_stereo_2x_avx2_fma(
    left_history: &[f64; TAPS_PER_PHASE],
    right_history: &[f64; TAPS_PER_PHASE],
    table: &PhaseTable,
) -> ([f64; MAX_PHASES], [f64; MAX_PHASES]) {
    let mut left_accumulator = _mm_setzero_pd();
    let mut right_accumulator = _mm_setzero_pd();
    for tap in 0..TAPS_PER_PHASE {
        let coefficients = _mm_loadu_pd(table.get_unchecked(tap).as_ptr());
        let left_sample = _mm_set1_pd(*left_history.get_unchecked(tap));
        let right_sample = _mm_set1_pd(*right_history.get_unchecked(tap));
        left_accumulator = _mm_fmadd_pd(left_sample, coefficients, left_accumulator);
        right_accumulator = _mm_fmadd_pd(right_sample, coefficients, right_accumulator);
    }
    let mut left_output = [0.0; MAX_PHASES];
    let mut right_output = [0.0; MAX_PHASES];
    _mm_storeu_pd(left_output.as_mut_ptr(), left_accumulator);
    _mm_storeu_pd(right_output.as_mut_ptr(), right_accumulator);
    (left_output, right_output)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn interpolate_stereo_avx2_fma(
    left_history: &[f64; TAPS_PER_PHASE],
    right_history: &[f64; TAPS_PER_PHASE],
    table: &PhaseTable,
) -> ([f64; MAX_PHASES], [f64; MAX_PHASES]) {
    let mut left_accumulator = _mm256_setzero_pd();
    let mut right_accumulator = _mm256_setzero_pd();
    for tap in 0..TAPS_PER_PHASE {
        let coefficients = _mm256_loadu_pd(table.get_unchecked(tap).as_ptr());
        let left_sample = _mm256_set1_pd(*left_history.get_unchecked(tap));
        let right_sample = _mm256_set1_pd(*right_history.get_unchecked(tap));
        left_accumulator = _mm256_fmadd_pd(left_sample, coefficients, left_accumulator);
        right_accumulator = _mm256_fmadd_pd(right_sample, coefficients, right_accumulator);
    }
    let mut left_output = [0.0; MAX_PHASES];
    let mut right_output = [0.0; MAX_PHASES];
    _mm256_storeu_pd(left_output.as_mut_ptr(), left_accumulator);
    _mm256_storeu_pd(right_output.as_mut_ptr(), right_accumulator);
    (left_output, right_output)
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn interpolate_2x_neon(
    history: &[f64; TAPS_PER_PHASE],
    table: &PhaseTable,
) -> [f64; MAX_PHASES] {
    let mut accumulator = vdupq_n_f64(0.0);
    for tap in 0..TAPS_PER_PHASE {
        let sample = vdupq_n_f64(*history.get_unchecked(tap));
        let coefficients = vld1q_f64(table.get_unchecked(tap).as_ptr());
        accumulator = vfmaq_f64(accumulator, sample, coefficients);
    }
    let mut output = [0.0; MAX_PHASES];
    vst1q_f64(output.as_mut_ptr(), accumulator);
    output
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn interpolate_neon(
    history: &[f64; TAPS_PER_PHASE],
    table: &PhaseTable,
) -> [f64; MAX_PHASES] {
    let mut low = vdupq_n_f64(0.0);
    let mut high = vdupq_n_f64(0.0);
    for tap in 0..TAPS_PER_PHASE {
        let sample = vdupq_n_f64(*history.get_unchecked(tap));
        let coefficients = table.get_unchecked(tap);
        low = vfmaq_f64(low, sample, vld1q_f64(coefficients.as_ptr()));
        high = vfmaq_f64(high, sample, vld1q_f64(coefficients.as_ptr().add(2)));
    }
    let mut output = [0.0; MAX_PHASES];
    vst1q_f64(output.as_mut_ptr(), low);
    vst1q_f64(output.as_mut_ptr().add(2), high);
    output
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn interpolate_stereo_2x_neon(
    left_history: &[f64; TAPS_PER_PHASE],
    right_history: &[f64; TAPS_PER_PHASE],
    table: &PhaseTable,
) -> ([f64; MAX_PHASES], [f64; MAX_PHASES]) {
    let mut left_accumulator = vdupq_n_f64(0.0);
    let mut right_accumulator = vdupq_n_f64(0.0);
    for tap in 0..TAPS_PER_PHASE {
        let coefficients = vld1q_f64(table.get_unchecked(tap).as_ptr());
        let left_sample = vdupq_n_f64(*left_history.get_unchecked(tap));
        let right_sample = vdupq_n_f64(*right_history.get_unchecked(tap));
        left_accumulator = vfmaq_f64(left_accumulator, left_sample, coefficients);
        right_accumulator = vfmaq_f64(right_accumulator, right_sample, coefficients);
    }
    let mut left_output = [0.0; MAX_PHASES];
    let mut right_output = [0.0; MAX_PHASES];
    vst1q_f64(left_output.as_mut_ptr(), left_accumulator);
    vst1q_f64(right_output.as_mut_ptr(), right_accumulator);
    (left_output, right_output)
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn interpolate_stereo_neon(
    left_history: &[f64; TAPS_PER_PHASE],
    right_history: &[f64; TAPS_PER_PHASE],
    table: &PhaseTable,
) -> ([f64; MAX_PHASES], [f64; MAX_PHASES]) {
    let mut left_low = vdupq_n_f64(0.0);
    let mut left_high = vdupq_n_f64(0.0);
    let mut right_low = vdupq_n_f64(0.0);
    let mut right_high = vdupq_n_f64(0.0);
    for tap in 0..TAPS_PER_PHASE {
        let coefficients = table.get_unchecked(tap);
        let low_coefficients = vld1q_f64(coefficients.as_ptr());
        let high_coefficients = vld1q_f64(coefficients.as_ptr().add(2));
        let left_sample = vdupq_n_f64(*left_history.get_unchecked(tap));
        let right_sample = vdupq_n_f64(*right_history.get_unchecked(tap));
        left_low = vfmaq_f64(left_low, left_sample, low_coefficients);
        left_high = vfmaq_f64(left_high, left_sample, high_coefficients);
        right_low = vfmaq_f64(right_low, right_sample, low_coefficients);
        right_high = vfmaq_f64(right_high, right_sample, high_coefficients);
    }
    let mut left_output = [0.0; MAX_PHASES];
    let mut right_output = [0.0; MAX_PHASES];
    vst1q_f64(left_output.as_mut_ptr(), left_low);
    vst1q_f64(left_output.as_mut_ptr().add(2), left_high);
    vst1q_f64(right_output.as_mut_ptr(), right_low);
    vst1q_f64(right_output.as_mut_ptr().add(2), right_high);
    (left_output, right_output)
}

#[cfg(test)]
fn reference_peak(samples: &[f32], factor: usize) -> f32 {
    let table = phase_table(factor);
    let mut history: Option<[f64; TAPS_PER_PHASE]> = None;
    let mut peak = 0.0_f32;
    for &sample in samples {
        let history = history.get_or_insert([sample as f64; TAPS_PER_PHASE]);
        history.copy_within(0..TAPS_PER_PHASE - 1, 1);
        history[0] = sample as f64;
        peak = peak.max(sample.abs());
        let mut values = [0.0; MAX_PHASES];
        for (coefficients, sample) in table.iter().zip(history.iter()) {
            for (value, coefficient) in values[..factor].iter_mut().zip(&coefficients[..factor]) {
                *value += coefficient * sample;
            }
        }
        for value in &values[..factor] {
            peak = peak.max(value.abs() as f32);
        }
    }
    peak
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_first_two_lanes_match(expected: [f64; MAX_PHASES], actual: [f64; MAX_PHASES]) {
        for phase in 0..2 {
            assert_eq!(
                actual[phase].to_bits(),
                expected[phase].to_bits(),
                "phase {phase}: {} != {}",
                actual[phase],
                expected[phase]
            );
        }
        assert_eq!(actual[2].to_bits(), 0.0_f64.to_bits());
        assert_eq!(actual[3].to_bits(), 0.0_f64.to_bits());
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn two_phase_avx2_matches_four_lane_fma_bit_for_bit() {
        if !(std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("fma")) {
            return;
        }
        let table = phase_table(2);
        let finite_left = std::array::from_fn(|index| {
            ((index as f64 * 0.173).sin() * 0.83) + ((index as f64 * 0.071).cos() * 0.11)
        });
        let finite_right = std::array::from_fn(|index| {
            ((index as f64 * 0.257).cos() * 0.67) - ((index as f64 * 0.113).sin() * 0.19)
        });
        let mut exceptional_left = finite_left;
        exceptional_left[3] = f64::NAN;
        exceptional_left[11] = f64::INFINITY;
        let mut exceptional_right = finite_right;
        exceptional_right[5] = f64::NEG_INFINITY;
        exceptional_right[13] = -0.0;

        for (left, right) in [
            (finite_left, finite_right),
            (exceptional_left, exceptional_right),
        ] {
            // SAFETY: this test performed runtime AVX2/FMA detection.
            unsafe {
                assert_first_two_lanes_match(
                    interpolate_avx2_fma(&left, table),
                    interpolate_2x_avx2_fma(&left, table),
                );
                let expected = interpolate_stereo_avx2_fma(&left, &right, table);
                let actual = interpolate_stereo_2x_avx2_fma(&left, &right, table);
                assert_first_two_lanes_match(expected.0, actual.0);
                assert_first_two_lanes_match(expected.1, actual.1);
            }
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn two_phase_neon_matches_four_lane_fma_bit_for_bit() {
        let table = phase_table(2);
        let finite_left = std::array::from_fn(|index| {
            ((index as f64 * 0.173).sin() * 0.83) + ((index as f64 * 0.071).cos() * 0.11)
        });
        let finite_right = std::array::from_fn(|index| {
            ((index as f64 * 0.257).cos() * 0.67) - ((index as f64 * 0.113).sin() * 0.19)
        });
        let mut exceptional_left = finite_left;
        exceptional_left[3] = f64::NAN;
        exceptional_left[11] = f64::INFINITY;
        let mut exceptional_right = finite_right;
        exceptional_right[5] = f64::NEG_INFINITY;
        exceptional_right[13] = -0.0;

        for (left, right) in [
            (finite_left, finite_right),
            (exceptional_left, exceptional_right),
        ] {
            // SAFETY: Advanced SIMD is part of the AArch64 architecture.
            unsafe {
                assert_first_two_lanes_match(
                    interpolate_neon(&left, table),
                    interpolate_2x_neon(&left, table),
                );
                let expected = interpolate_stereo_neon(&left, &right, table);
                let actual = interpolate_stereo_2x_neon(&left, &right, table);
                assert_first_two_lanes_match(expected.0, actual.0);
                assert_first_two_lanes_match(expected.1, actual.1);
            }
        }
    }

    #[test]
    fn polyphase_dc_gain_is_one() {
        let x = vec![0.5f32; 1000];
        let mut meter = TruePeakMeter::new();
        meter.process(&x);
        let tp = meter.peak();
        assert!((tp - 0.5).abs() < 1e-4, "true peak of DC = {tp}");
    }

    #[test]
    fn chunk_boundaries_do_not_change_true_peak() {
        let samples: Vec<f32> = (0..1000)
            .map(|index| ((index as f64 * 0.31).sin() * 0.8) as f32)
            .collect();
        let mut whole = TruePeakMeter::new();
        whole.process(&samples);
        let mut chunked = TruePeakMeter::new();
        for chunk in samples.chunks(37) {
            chunked.process(chunk);
        }
        assert_eq!(whole.peak(), chunked.peak());
    }

    #[test]
    fn peak_only_pruning_matches_exact_interpolation_bit_for_bit() {
        let mut samples = Vec::with_capacity(20_001);
        samples.push(0.99_f32);
        samples.extend((0..20_000).map(|index| {
            let first = (index as f64 * 0.173).sin();
            let second = (index as f64 * 0.071 + 0.4).cos();
            (0.008 * first + 0.003 * second) as f32
        }));

        for sample_rate in [48_000, 96_000, 192_000] {
            let mut exact = TruePeakMeter::for_sample_rate(sample_rate);
            for &sample in &samples {
                exact.process_sample(sample);
            }

            let mut pruned = TruePeakMeter::for_sample_rate(sample_rate);
            for chunk in samples.chunks(37) {
                pruned.process(chunk);
            }
            assert_eq!(
                pruned.peak().to_bits(),
                exact.peak().to_bits(),
                "{sample_rate} Hz"
            );
        }
    }

    #[test]
    fn peak_only_pruning_skips_quiet_windows_after_a_transient() {
        for sample_rate in [48_000, 96_000] {
            let mut exact = TruePeakMeter::for_sample_rate(sample_rate);
            let mut pruned = TruePeakMeter::for_sample_rate(sample_rate);
            let mut skipped = 0_usize;
            let samples = std::iter::once(0.99_f32)
                .chain((0..20_000).map(|index| ((index as f64 * 0.19).sin() * 0.001) as f32));
            for sample in samples {
                exact.process_sample(sample);
                skipped += usize::from(pruned.process_peak_only_sample(sample));
            }
            assert_eq!(pruned.peak().to_bits(), exact.peak().to_bits());
            assert!(
                skipped > 19_000,
                "{sample_rate} Hz skipped only {skipped} windows"
            );
        }
    }

    #[test]
    fn block_pruning_matches_sample_updates_and_preserves_future_history() {
        let quiet: Vec<f32> = (0..32_768)
            .map(|index| ((index as f64 * 0.173).sin() * 0.001) as f32)
            .collect();
        let future: Vec<f32> = (0..4096)
            .map(|index| {
                let first = (index as f64 * 0.371).sin();
                let second = (index as f64 * 0.113 + 0.7).cos();
                (0.73 * first + 0.19 * second) as f32
            })
            .collect();

        for sample_rate in [48_000, 96_000] {
            let mut sample_path = TruePeakMeter::for_sample_rate(sample_rate);
            let mut block_path = TruePeakMeter::for_sample_rate(sample_rate);
            for sample in std::iter::once(0.99_f32).chain(quiet.iter().copied().take(1024)) {
                sample_path.process_peak_only_sample(sample);
                block_path.process_peak_only_sample(sample);
            }
            assert!(
                block_path.pruning_active,
                "{sample_rate} Hz did not arm pruning"
            );
            for &sample in &quiet[1024..] {
                sample_path.process_peak_only_sample(sample);
            }
            assert!(block_path.try_skip_peak_only_block(&quiet[1024..]));
            assert_eq!(
                block_path.peak().to_bits(),
                sample_path.peak().to_bits(),
                "{sample_rate} Hz quiet block"
            );

            for &sample in &future {
                sample_path.process_peak_only_sample(sample);
                block_path.process_peak_only_sample(sample);
            }
            assert_eq!(
                block_path.peak().to_bits(),
                sample_path.peak().to_bits(),
                "{sample_rate} Hz future exact windows"
            );
        }
    }

    #[test]
    fn block_pruning_rejects_nan_and_retains_sample_semantics() {
        let prefix = std::iter::once(0.99_f32)
            .chain((0..1024).map(|index| ((index as f64 * 0.1).sin() * 0.001) as f32));
        let mut sample_path = TruePeakMeter::for_sample_rate(48_000);
        let mut block_path = TruePeakMeter::for_sample_rate(48_000);
        for sample in prefix {
            sample_path.process_peak_only_sample(sample);
            block_path.process_peak_only_sample(sample);
        }
        let block = [0.0001_f32, f32::NAN, -0.0002, 0.0003];
        assert!(!block_path.try_skip_peak_only_block(&block));
        for sample in block {
            sample_path.process_peak_only_sample(sample);
            block_path.process_peak_only_sample(sample);
        }
        assert_eq!(block_path.peak().to_bits(), sample_path.peak().to_bits());
    }

    #[test]
    fn peak_only_pruning_matches_exact_across_dynamic_random_blocks() {
        let mut state = 0x6a09_e667_f3bc_c909_u64;
        let samples: Vec<f32> = (0..100_000)
            .map(|index| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let unit = ((state >> 40) as f64 / (1_u64 << 24) as f64) * 2.0 - 1.0;
                let amplitude = match (index / 4096) % 3 {
                    0 => 0.92,
                    1 => 0.015,
                    _ => 0.31,
                };
                (unit * amplitude) as f32
            })
            .collect();

        for sample_rate in [48_000, 96_000, 192_000] {
            let mut exact = TruePeakMeter::for_sample_rate(sample_rate);
            let mut pruned = TruePeakMeter::for_sample_rate(sample_rate);
            for &sample in &samples {
                exact.process_sample(sample);
                pruned.process_peak_only_sample(sample);
            }
            assert_eq!(
                pruned.peak().to_bits(),
                exact.peak().to_bits(),
                "{sample_rate} Hz"
            );
        }
    }

    #[test]
    fn exact_calls_safely_reset_peak_only_probe_state() {
        let samples: Vec<f32> = std::iter::once(0.99_f32)
            .chain((0..20_000).map(|index| ((index as f64 * 0.127).sin() * 0.002) as f32))
            .collect();
        for sample_rate in [48_000, 96_000, 192_000] {
            let mut exact = TruePeakMeter::for_sample_rate(sample_rate);
            let mut mixed = TruePeakMeter::for_sample_rate(sample_rate);
            let mut skipped = 0_usize;
            for (index, &sample) in samples.iter().enumerate() {
                exact.process_sample(sample);
                if index.is_multiple_of(521) {
                    mixed.process_sample(sample);
                } else {
                    skipped += usize::from(mixed.process_peak_only_sample(sample));
                }
            }
            assert_eq!(mixed.peak().to_bits(), exact.peak().to_bits());
            if sample_rate < 192_000 {
                assert!(skipped > 0, "{sample_rate} Hz never reactivated pruning");
            }
        }
    }

    #[test]
    fn stereo_peak_only_pruning_matches_exact_meters() {
        let left: Vec<f32> = std::iter::once(0.98_f32)
            .chain((0..20_000).map(|index| ((index as f64 * 0.173).sin() * 0.004) as f32))
            .collect();
        let right: Vec<f32> = std::iter::once(-0.97_f32)
            .chain((0..20_000).map(|index| ((index as f64 * 0.071 + 0.4).cos() * 0.006) as f32))
            .collect();
        for sample_rate in [48_000, 96_000, 192_000] {
            let mut exact_left = TruePeakMeter::for_sample_rate(sample_rate);
            let mut exact_right = TruePeakMeter::for_sample_rate(sample_rate);
            let mut pruned_left = TruePeakMeter::for_sample_rate(sample_rate);
            let mut pruned_right = TruePeakMeter::for_sample_rate(sample_rate);
            let mut skipped = [0_usize; 2];
            for (&left_sample, &right_sample) in left.iter().zip(&right) {
                exact_left.process_sample(left_sample);
                exact_right.process_sample(right_sample);
                let result = TruePeakMeter::process_stereo_peak_only_sample(
                    &mut pruned_left,
                    &mut pruned_right,
                    left_sample,
                    right_sample,
                );
                skipped[0] += usize::from(result.0);
                skipped[1] += usize::from(result.1);
            }
            assert_eq!(pruned_left.peak().to_bits(), exact_left.peak().to_bits());
            assert_eq!(pruned_right.peak().to_bits(), exact_right.peak().to_bits());
            if sample_rate < 192_000 {
                assert!(skipped[0] > 19_000, "left {sample_rate} Hz: {skipped:?}");
                assert!(skipped[1] > 19_000, "right {sample_rate} Hz: {skipped:?}");
            }
        }
    }

    #[test]
    fn peak_only_pruning_preserves_exceptional_sample_result() {
        let samples = [
            f32::NAN,
            f32::MIN_POSITIVE / 2.0,
            -f32::MIN_POSITIVE / 4.0,
            0.75,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NAN,
            -0.25,
        ];
        for sample_rate in [48_000, 96_000, 192_000] {
            let mut exact = TruePeakMeter::for_sample_rate(sample_rate);
            let mut pruned = TruePeakMeter::for_sample_rate(sample_rate);
            for sample in samples {
                exact.process_sample(sample);
                pruned.process_peak_only_sample(sample);
            }
            assert_eq!(
                pruned.peak().to_bits(),
                exact.peak().to_bits(),
                "{sample_rate} Hz"
            );
        }
    }

    #[cfg(all(
        feature = "cuda-truepeak",
        any(target_os = "linux", target_os = "windows")
    ))]
    #[test]
    fn cuda_cpu_recovery_discards_synthetic_warmup_peak() {
        let prefix = [
            0.928947738,
            -0.444463895,
            0.531343035,
            0.0869211069,
            -0.0373190996,
            -0.919440805,
            -0.521659074,
            -0.0713924204,
            -0.143955913,
            -0.794127755,
            -0.0113511079,
            0.81902389,
            0.242345226,
            0.243654488,
            0.570452889,
            0.953598437,
        ];
        let recent = [
            -0.73747328,
            -0.613953811,
            -0.960254987,
            0.918875278,
            -0.855194621,
            0.0757231787,
            -0.0394378373,
            0.253150232,
            -0.417878514,
            0.864959347,
            -0.35141978,
            -0.215435538,
            0.255568276,
            -0.602383278,
            0.244405988,
        ];
        let mut complete = TruePeakMeter::for_sample_rate(48_000);
        complete.process(&prefix);
        complete.process(&recent);

        // Starting the retained suffix in isolation repeats its first sample
        // and creates a larger, synthetic transient for this fixture.
        let mut suffix_only = TruePeakMeter::for_sample_rate(48_000);
        suffix_only.process(&recent);
        assert!(suffix_only.peak() > complete.peak());

        let recovered = TruePeakMeter::from_recent_samples(48_000, &recent, complete.peak());
        assert_eq!(recovered.peak().to_bits(), complete.peak().to_bits());
    }

    #[test]
    fn stereo_pair_matches_independent_meters_bit_for_bit() {
        let left: Vec<f32> = (0..10_000)
            .map(|index| ((index as f64 * 0.173).sin() * 0.83) as f32)
            .collect();
        let right: Vec<f32> = (0..10_000)
            .map(|index| ((index as f64 * 0.071 + 0.4).cos() * 0.61) as f32)
            .collect();
        for sample_rate in [48_000, 96_000, 192_000] {
            let mut expected_left = TruePeakMeter::for_sample_rate(sample_rate);
            let mut expected_right = TruePeakMeter::for_sample_rate(sample_rate);
            let mut paired_left = TruePeakMeter::for_sample_rate(sample_rate);
            let mut paired_right = TruePeakMeter::for_sample_rate(sample_rate);
            for (&left_sample, &right_sample) in left.iter().zip(&right) {
                let expected = (
                    expected_left.process_sample(left_sample),
                    expected_right.process_sample(right_sample),
                );
                let actual = TruePeakMeter::process_stereo_sample(
                    &mut paired_left,
                    &mut paired_right,
                    left_sample,
                    right_sample,
                );
                assert_eq!(actual.0.to_bits(), expected.0.to_bits());
                assert_eq!(actual.1.to_bits(), expected.1.to_bits());
            }
            assert_eq!(paired_left.peak().to_bits(), expected_left.peak().to_bits());
            assert_eq!(
                paired_right.peak().to_bits(),
                expected_right.peak().to_bits()
            );
        }
    }

    #[test]
    fn circular_history_matches_shift_register_reference() {
        let samples: Vec<f32> = (0..10_000)
            .map(|index| {
                let first = (index as f64 * 0.173).sin();
                let second = (index as f64 * 0.071 + 0.4).cos();
                (0.61 * first + 0.27 * second) as f32
            })
            .collect();
        for &(sample_rate, factor) in &[(48_000, 4), (96_000, 2)] {
            let mut meter = TruePeakMeter::for_sample_rate(sample_rate);
            meter.process(&samples);
            let expected = reference_peak(&samples, factor);
            assert!(
                (meter.peak() - expected).abs() <= 1.0e-6,
                "{sample_rate} Hz: {} != {expected}",
                meter.peak()
            );
        }
    }

    #[test]
    fn sample_peak_bound_contains_exact_meter_after_f32_gain() {
        let mut state = 0x4d59_5df4_d0f3_3173_u64;
        let mut samples = Vec::with_capacity(32_779);
        for index in 0..samples.capacity() {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let noise = ((state >> 40) as i32 - (1 << 23)) as f32 / (1 << 23) as f32;
            let transient = if index.is_multiple_of(997) {
                if index.is_multiple_of(2) {
                    1.0
                } else {
                    -1.0
                }
            } else {
                0.0
            };
            samples.push((noise * 0.81 + transient * 0.19).clamp(-1.0, 1.0));
        }
        let source_peak = samples
            .iter()
            .fold(0.0_f32, |maximum, sample| maximum.max(sample.abs()));

        for sample_rate in [44_100, 48_000, 96_000, 191_999, 192_000, 384_000] {
            for gain in [f32::MIN_POSITIVE, 0.000_123, 0.37, 1.0, 3.75, 65_536.0] {
                let scaled = samples
                    .iter()
                    .map(|sample| *sample * gain)
                    .collect::<Vec<_>>();
                let mut meter = TruePeakMeter::for_sample_rate(sample_rate);
                meter.process(&scaled);
                let rounded_sample_peak_bound = source_peak * gain;
                let upper = upper_bound_from_sample_peak(sample_rate, rounded_sample_peak_bound);
                assert!(
                    f64::from(meter.peak()) <= upper,
                    "{sample_rate} Hz, gain {gain}: {} > {upper}",
                    meter.peak()
                );
            }
        }

        assert!(upper_bound_from_sample_peak(48_000, f32::NAN).is_infinite());
        assert!(upper_bound_from_sample_peak(48_000, f32::INFINITY).is_infinite());
        assert!(upper_bound_from_sample_peak(48_000, -0.5).is_infinite());
    }

    #[test]
    fn oversampling_ratio_tracks_input_sample_rate() {
        assert_eq!(oversample_factor(44_100), 4);
        assert_eq!(oversample_factor(95_999), 4);
        assert_eq!(oversample_factor(96_000), 2);
        assert_eq!(oversample_factor(191_999), 2);
        assert_eq!(oversample_factor(192_000), 1);
    }

    #[test]
    fn high_rate_meter_uses_sample_peak() {
        let samples = [-0.25, 0.5, -0.75, 0.625];
        let mut meter = TruePeakMeter::for_sample_rate(192_000);
        meter.process(&samples);
        assert_eq!(meter.peak(), 0.75);
    }
}
