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

const MAX_PHASES: usize = 4;
const TAPS_PER_PHASE: usize = 16;
const HISTORY_SAMPLES: usize = TAPS_PER_PHASE * 2;
type PhaseTable = [[f64; MAX_PHASES]; TAPS_PER_PHASE];

fn phase_table(factor: usize) -> &'static PhaseTable {
    static X2: OnceLock<PhaseTable> = OnceLock::new();
    static X4: OnceLock<PhaseTable> = OnceLock::new();
    match factor {
        2 => X2.get_or_init(|| build_phase_table(2)),
        4 => X4.get_or_init(|| build_phase_table(4)),
        _ => unreachable!("true-peak interpolation factor must be 2 or 4"),
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
            #[cfg(target_arch = "x86_64")]
            use_avx2_fma: is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma"),
            peak: 0.0,
        }
    }

    pub fn process(&mut self, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }
        for &sample in samples {
            self.process_sample(sample);
        }
    }

    pub(crate) fn process_sample(&mut self, sample: f32) -> f32 {
        let mut frame_peak = sample.abs();
        if self.factor > 1 {
            let sample = sample as f64;
            if self.initialized {
                self.cursor = self.cursor.wrapping_sub(1) & (TAPS_PER_PHASE - 1);
                self.history[self.cursor] = sample;
                self.history[self.cursor + TAPS_PER_PHASE] = sample;
            } else {
                self.history.fill(sample);
                self.initialized = true;
            }
            let history: &[f64; TAPS_PER_PHASE] = self.history
                [self.cursor..self.cursor + TAPS_PER_PHASE]
                .try_into()
                .expect("true-peak history window has a fixed length");
            let table = phase_table(self.factor);
            #[cfg(target_arch = "x86_64")]
            let interpolated = if self.use_avx2_fma {
                // SAFETY: the constructor performed runtime AVX2/FMA detection.
                unsafe { interpolate_avx2_fma(history, table) }
            } else {
                interpolate_scalar(history, table, self.factor)
            };
            #[cfg(target_arch = "aarch64")]
            let interpolated = {
                // SAFETY: Advanced SIMD is part of the AArch64 architecture.
                unsafe { interpolate_neon(history, table) }
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

    pub const fn peak(&self) -> f32 {
        self.peak
    }
}

#[inline]
fn oversample_factor(sample_rate: u32) -> usize {
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
