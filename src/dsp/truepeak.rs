//! Sample-rate-aware true-peak meter (ITU-R BS.1770-5 style).
//!
//! Sample peaks (max of the discrete samples) miss inter-sample peaks that can
//! exceed 0 dBFS after DAC reconstruction. True peak is measured by
//! oversampling each channel with a polyphase FIR and taking the maximum
//! absolute interpolated value. The Kaiser-windowed lowpass is normalized to
//! unity DC gain; this is the same approach the ITU reference takes and gives
//! accurate inter-sample peaks at a fraction of the cost of a full oversampled
//! reconstruction. ITU-R BS.1770-5 permits proportionately less oversampling at
//! higher input rates, so the meter selects the smallest integral factor whose
//! measurement domain reaches at least 192 kHz.
//!
//! Each normalized coefficient table is computed once via a `OnceLock`.

use crate::wav::AudioBuffer;
use rayon::prelude::*;
use std::f64::consts::PI;
use std::sync::OnceLock;

#[cfg(target_arch = "aarch64")]
use std::arch::aarch64::*;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

const TRUE_PEAK_DOMAIN_HZ: u32 = 192_000;
pub(super) const MAX_PHASES: usize = 24;
const SIMD_PHASES: usize = 4;
pub(super) const TAPS_PER_PHASE: usize = 16;
const HISTORY_SAMPLES: usize = TAPS_PER_PHASE * 2;
pub(super) type PhaseTable = [[f64; MAX_PHASES]; TAPS_PER_PHASE];

// Committed f32 coefficient bits used only by the reference engine. Keeping
// the normalized tables in source avoids platform-libm differences during
// FIR design while retaining the established fast engine's generated tables.
const REFERENCE_PHASE_2X: [[u32; 2]; TAPS_PER_PHASE] = [
    [0x80000000, 0xb9693f62],
    [0x3a287d88, 0x3ac0a1fc],
    [0xbb3ff93f, 0xbbae2986],
    [0x3c1369ca, 0x3c6c9fdc],
    [0xbcb64a3c, 0xbd0839e8],
    [0x3d47a36c, 0x3d9153f7],
    [0xbdd65ad5, 0xbe261528],
    [0x3e93fea7, 0x3f6582d5],
    [0x3f6582d5, 0x3e93fea7],
    [0xbe261528, 0xbdd65ad5],
    [0x3d9153f7, 0x3d47a36c],
    [0xbd0839e8, 0xbcb64a3c],
    [0x3c6c9fdc, 0x3c1369ca],
    [0xbbae2986, 0xbb3ff93f],
    [0x3ac0a1fc, 0x3a287d88],
    [0xb9693f62, 0x80000000],
];

const REFERENCE_PHASE_3X: [[u32; 3]; TAPS_PER_PHASE] = [
    [0x80000000, 0xb94ec97e, 0xb96d6afa],
    [0x39e72c3f, 0x3acbb521, 0x3aa779fd],
    [0xbb02b317, 0xbbc3c891, 0xbb8dcf86],
    [0x3bc7caee, 0x3c897b42, 0x3c398052],
    [0xbc762cf5, 0xbd2123be, 0xbcd0b945],
    [0x3d062e28, 0x3dabe36a, 0x3d5c7123],
    [0xbd8e75d0, 0xbe3b77b2, 0xbdff95ad],
    [0x3e3ae71f, 0x3f20567b, 0x3f74032a],
    [0x3f74032a, 0x3f20567b, 0x3e3ae71f],
    [0xbdff95ad, 0xbe3b77b2, 0xbd8e75d0],
    [0x3d5c7123, 0x3dabe36a, 0x3d062e28],
    [0xbcd0b945, 0xbd2123be, 0xbc762cf5],
    [0x3c398052, 0x3c897b42, 0x3bc7caee],
    [0xbb8dcf86, 0xbbc3c891, 0xbb02b317],
    [0x3aa779fd, 0x3acbb521, 0x39e72c3f],
    [0xb96d6afa, 0xb94ec97e, 0x80000000],
];

const REFERENCE_PHASE_4X: [[u32; 4]; TAPS_PER_PHASE] = [
    [0x80000000, 0xb9136527, 0xb9930501, 0xb9564d55],
    [0x39ae4cf1, 0x3aa1eacc, 0x3aeefd95, 0x3a8d559b],
    [0xbac459b3, 0xbba0e68a, 0xbbd6346a, 0xbb683f30],
    [0x3b95ba9b, 0x3c6600eb, 0x3c90a9de, 0x3c1539a3],
    [0xbc3829b0, 0xbd082066, 0xbd25a3cf, 0xbca61b9c],
    [0x3cc85018, 0x3d91781a, 0x3daef748, 0x3d2ead51],
    [0xbd538f23, 0xbe1c13f0, 0xbe41bded, 0xbdcc51cb],
    [0x3e07ad84, 0x3eeaea3c, 0x3f46f1c3, 0x3f7936ed],
    [0x3f7936ed, 0x3f46f1c3, 0x3eeaea3c, 0x3e07ad84],
    [0xbdcc51cb, 0xbe41bded, 0xbe1c13f0, 0xbd538f23],
    [0x3d2ead51, 0x3daef748, 0x3d91781a, 0x3cc85018],
    [0xbca61b9c, 0xbd25a3cf, 0xbd082066, 0xbc3829b0],
    [0x3c1539a3, 0x3c90a9de, 0x3c6600eb, 0x3b95ba9b],
    [0xbb683f30, 0xbbd6346a, 0xbba0e68a, 0xbac459b3],
    [0x3a8d559b, 0x3aeefd95, 0x3aa1eacc, 0x39ae4cf1],
    [0xb9564d55, 0xb9930501, 0xb9136527, 0x80000000],
];

const REFERENCE_PHASE_5X: [[u32; 5]; TAPS_PER_PHASE] = [
    [0x80000000, 0xb8da8441, 0xb9748501, 0xb9a20ce7, 0xb93e2bcc],
    [0x398b7eb5, 0x3a815128, 0x3adc23c7, 0x3aefabdc, 0x3a71a1aa],
    [0xba9cca00, 0xbb833d29, 0xbbcccc1b, 0xbbcec74b, 0xbb431f45],
    [0x3b6ecafd, 0x3c3dacb9, 0x3c8d3049, 0x3c88a21a, 0x3bf82c3c],
    [0xbc12b328, 0xbce1e9e9, 0xbd238d16, 0xbd1a6c8d, 0xbc894460],
    [0x3c9f5b0b, 0x3d71cdd5, 0x3dad2149, 0x3da25859, 0x3d1001d2],
    [0xbd27ca80, 0xbe00aca7, 0xbe3bf0d2, 0xbe35ef44, 0xbda96f13],
    [0x3dd47e5b, 0x3eb68807, 0x3f206275, 0x3f5a7bd0, 0x3f7ba50c],
    [0x3f7ba50c, 0x3f5a7bd0, 0x3f206275, 0x3eb68807, 0x3dd47e5b],
    [0xbda96f13, 0xbe35ef44, 0xbe3bf0d2, 0xbe00aca7, 0xbd27ca80],
    [0x3d1001d2, 0x3da25859, 0x3dad2149, 0x3d71cdd5, 0x3c9f5b0b],
    [0xbc894460, 0xbd1a6c8d, 0xbd238d16, 0xbce1e9e9, 0xbc12b328],
    [0x3bf82c3c, 0x3c88a21a, 0x3c8d3049, 0x3c3dacb9, 0x3b6ecafd],
    [0xbb431f45, 0xbbcec74b, 0xbbcccc1b, 0xbb833d29, 0xba9cca00],
    [0x3a71a1aa, 0x3aefabdc, 0x3adc23c7, 0x3a815128, 0x398b7eb5],
    [0xb93e2bcc, 0xb9a20ce7, 0xb9748501, 0xb8da8441, 0x80000000],
];

pub(super) fn phase_table(factor: usize) -> &'static PhaseTable {
    static TABLES: OnceLock<[OnceLock<PhaseTable>; MAX_PHASES + 1]> = OnceLock::new();
    assert!(
        (2..=MAX_PHASES).contains(&factor),
        "true-peak interpolation factor must be between 2 and {MAX_PHASES}"
    );
    TABLES.get_or_init(|| std::array::from_fn(|_| OnceLock::new()))[factor]
        .get_or_init(|| build_phase_table(factor))
}

pub(super) fn reference_phase_table(factor: usize) -> Result<&'static PhaseTable, String> {
    static TABLE_2X: OnceLock<PhaseTable> = OnceLock::new();
    static TABLE_3X: OnceLock<PhaseTable> = OnceLock::new();
    static TABLE_4X: OnceLock<PhaseTable> = OnceLock::new();
    static TABLE_5X: OnceLock<PhaseTable> = OnceLock::new();
    match factor {
        1 => Err("a 1x true-peak domain does not use an interpolation table".into()),
        2 => Ok(TABLE_2X.get_or_init(|| build_reference_phase_table(&REFERENCE_PHASE_2X))),
        3 => Ok(TABLE_3X.get_or_init(|| build_reference_phase_table(&REFERENCE_PHASE_3X))),
        4 => Ok(TABLE_4X.get_or_init(|| build_reference_phase_table(&REFERENCE_PHASE_4X))),
        5 => Ok(TABLE_5X.get_or_init(|| build_reference_phase_table(&REFERENCE_PHASE_5X))),
        _ => Err(format!(
            "reference true-peak coefficients do not support {factor}x interpolation"
        )),
    }
}

fn build_reference_phase_table<const FACTOR: usize>(
    bits: &[[u32; FACTOR]; TAPS_PER_PHASE],
) -> PhaseTable {
    let mut table = [[0.0; MAX_PHASES]; TAPS_PER_PHASE];
    for tap in 0..TAPS_PER_PHASE {
        for phase in 0..FACTOR {
            table[tap][phase] = f64::from(f32::from_bits(bits[tap][phase]));
        }
    }
    table
}

fn interpolation_bound_scale(factor: usize) -> f64 {
    static SCALES: OnceLock<[OnceLock<f64>; MAX_PHASES + 1]> = OnceLock::new();
    assert!(
        (2..=MAX_PHASES).contains(&factor),
        "true-peak interpolation factor must be between 2 and {MAX_PHASES}"
    );
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
    *SCALES.get_or_init(|| std::array::from_fn(|_| OnceLock::new()))[factor].get_or_init(build)
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
    debug_assert!((2..=MAX_PHASES).contains(&factor));
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
            let mut meter = TruePeakMeter::for_finite_sample_rate(buf.sample_rate);
            meter.process(ch);
            meter.finish_peak()
        })
        .reduce(|| 0.0f32, f32::max)
}

/// Stateful true-peak meter that accepts consecutive sample chunks.
pub struct TruePeakMeter {
    history: [f64; HISTORY_SAMPLES],
    cursor: usize,
    initialized: bool,
    factor: usize,
    table: Option<&'static PhaseTable>,
    interpolation_bound_scale: f64,
    pruning_active: bool,
    pruning_probe_max: f32,
    pruning_probe_len: u8,
    pruning_probe_delay: u8,
    #[cfg(target_arch = "x86_64")]
    use_avx2_fma: bool,
    force_scalar: bool,
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
        Self::with_leading_padding(sample_rate, false)
    }

    /// Construct a meter for a finite signal, treating samples before its
    /// beginning as zero. Consume it with [`Self::finish_peak`] to include the
    /// complete FIR response after the last programme sample.
    ///
    /// The existing constructors retain their streaming edge extension for
    /// compatibility; finite-file analyzers should opt into this constructor.
    pub fn for_finite_sample_rate(sample_rate: u32) -> Self {
        Self::with_leading_padding(sample_rate, true)
    }

    /// Construct a finite meter with committed coefficient bits and scalar
    /// tap/phase ordering for reproducible reference analysis.
    pub(crate) fn for_finite_reference_sample_rate(sample_rate: u32) -> Result<Self, String> {
        let factor = oversample_factor(sample_rate);
        let table = if factor > 1 {
            Some(reference_phase_table(factor)?)
        } else {
            None
        };
        Ok(Self {
            history: [0.0; HISTORY_SAMPLES],
            cursor: 0,
            initialized: true,
            factor,
            table,
            interpolation_bound_scale: 0.0,
            pruning_active: false,
            pruning_probe_max: 0.0,
            pruning_probe_len: 0,
            pruning_probe_delay: 0,
            #[cfg(target_arch = "x86_64")]
            use_avx2_fma: false,
            force_scalar: true,
            peak: 0.0,
        })
    }

    fn with_leading_padding(sample_rate: u32, zero_leading_padding: bool) -> Self {
        let factor = oversample_factor(sample_rate);
        let table = if factor > 1 {
            // Build the shared FIR table before this meter reaches a processing
            // callback. Subsequent `process` calls are allocation-free.
            Some(phase_table(factor))
        } else {
            None
        };
        Self {
            history: [0.0; HISTORY_SAMPLES],
            cursor: 0,
            initialized: zero_leading_padding,
            factor,
            table,
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
            force_scalar: false,
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
        Self::from_recent_samples_with_padding(sample_rate, recent, peak_floor, false)
    }

    /// Rebuild finite-signal history from its retained suffix. This supports
    /// accelerator handoff as well as isolated EOF-response measurement. Zero
    /// padding remains exact for prefixes shorter than the retained 15 samples
    /// and is pushed out before the first finishing zero is evaluated.
    pub(super) fn from_recent_finite_samples(
        sample_rate: u32,
        recent: &[f32],
        peak_floor: f32,
    ) -> Self {
        Self::from_recent_samples_with_padding(sample_rate, recent, peak_floor, true)
    }

    fn from_recent_samples_with_padding(
        sample_rate: u32,
        recent: &[f32],
        peak_floor: f32,
        zero_leading_padding: bool,
    ) -> Self {
        debug_assert!(recent.len() < TAPS_PER_PHASE);
        let mut meter = Self::with_leading_padding(sample_rate, zero_leading_padding);
        meter.process(recent);
        // Replaying only the retained suffix seeds the history needed by the
        // next real sample or first finishing zero. The caller already
        // measured the complete prefix (or wants only its future response),
        // so discard any replay peak and restore the authoritative floor.
        meter.peak = peak_floor;
        meter
    }

    /// Measure only the post-signal FIR response reconstructed from the final
    /// samples of a finite signal. Peaks produced while replaying the retained
    /// suffix are deliberately discarded: its artificial leading-zero edge is
    /// not present in the original stream.
    pub(super) fn finite_tail_peak_from_recent_samples(sample_rate: u32, recent: &[f32]) -> f32 {
        Self::from_recent_finite_samples(sample_rate, recent, 0.0).finish_peak()
    }

    pub(super) fn finite_reference_tail_peak_from_recent_samples(
        sample_rate: u32,
        recent: &[f32],
    ) -> Result<f32, String> {
        let mut meter = Self::for_finite_reference_sample_rate(sample_rate)?;
        meter.process(recent);
        meter.peak = 0.0;
        Ok(meter.finish_peak())
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

    /// Finish a finite measurement and return its maximum True Peak.
    ///
    /// The 16-tap polyphase FIR is advanced with 15 virtual zeros so every
    /// convolution window containing a programme sample is measured. The
    /// method consumes the meter to prevent further samples being appended
    /// after the finite boundary.
    pub fn finish_peak(mut self) -> f32 {
        if self.factor > 1 && self.initialized {
            for _ in 0..TAPS_PER_PHASE - 1 {
                self.process_peak_only_sample(0.0);
            }
        }
        self.peak
    }

    /// Advance a complete peak-only block after one conservative SIMD
    /// reduction proves that no FIR phase can exceed the retained maximum.
    /// Only the final 16 samples can affect a future window, so the circular
    /// history can be replaced directly instead of being advanced per sample.
    #[inline]
    pub(crate) fn try_skip_peak_only_block(&mut self, samples: &[f32]) -> bool {
        if self.force_scalar || samples.is_empty() || self.factor <= 1 || !self.pruning_active {
            return false;
        }
        let (block_maximum, has_nan) = crate::dsp::simd::abs_max_and_has_nan(samples);
        self.try_skip_peak_only_block_reduced(samples, block_maximum, has_nan)
    }

    /// Return the discrete sample peak from the same SIMD reduction used to
    /// decide whether exact FIR interpolation can be skipped. Loudness analysis
    /// can then avoid repeating a scalar sample-peak reduction in its frame loop.
    #[inline]
    pub(crate) fn try_skip_peak_only_block_with_sample_peak(
        &mut self,
        samples: &[f32],
    ) -> (bool, f32) {
        let (block_maximum, has_nan) = crate::dsp::simd::abs_max_and_has_nan(samples);
        (
            self.try_skip_peak_only_block_reduced(samples, block_maximum, has_nan),
            block_maximum,
        )
    }

    #[inline]
    fn try_skip_peak_only_block_reduced(
        &mut self,
        samples: &[f32],
        block_maximum: f32,
        has_nan: bool,
    ) -> bool {
        if self.force_scalar || samples.is_empty() || self.factor <= 1 || !self.pruning_active {
            return false;
        }
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
    fn interpolated_peak(&self) -> f32 {
        let history = self.history_window();
        let table = self.table.expect("oversampling meter has a phase table");
        if self.force_scalar {
            return interpolate_scalar_peak(history, table, self.factor);
        }
        #[cfg(target_arch = "x86_64")]
        if self.use_avx2_fma {
            // SAFETY: the constructor performed runtime AVX2/FMA detection.
            unsafe {
                match self.factor {
                    2 => return maximum_abs(&interpolate_2x_avx2_fma(history, table)[..2]),
                    4 => return maximum_abs(&interpolate_avx2_fma(history, table)),
                    5 => return interpolate_5x_avx2_fma_peak(history, table),
                    _ => return interpolate_tiled_avx2_fma_peak(history, table, self.factor),
                }
            }
        }
        #[cfg(target_arch = "aarch64")]
        // SAFETY: Advanced SIMD is part of the AArch64 architecture.
        return unsafe {
            match self.factor {
                2 => maximum_abs(&interpolate_2x_neon(history, table)[..2]),
                4 => maximum_abs(&interpolate_neon(history, table)),
                5 => interpolate_5x_neon_peak(history, table),
                _ => interpolate_tiled_neon_peak(history, table, self.factor),
            }
        };
        #[cfg(not(target_arch = "aarch64"))]
        interpolate_scalar_peak(history, table, self.factor)
    }

    #[inline(always)]
    fn interpolated_stereo_peaks(left: &Self, right: &Self) -> (f32, f32) {
        let left_history = left.history_window();
        let right_history = right.history_window();
        let table = left.table.expect("oversampling meter has a phase table");
        if left.force_scalar || right.force_scalar {
            return interpolate_stereo_scalar_peaks(
                left_history,
                right_history,
                table,
                left.factor,
            );
        }
        #[cfg(target_arch = "x86_64")]
        if left.use_avx2_fma {
            // SAFETY: both constructors performed runtime AVX2/FMA detection.
            unsafe {
                match left.factor {
                    2 => {
                        let values =
                            interpolate_stereo_2x_avx2_fma(left_history, right_history, table);
                        return (maximum_abs(&values.0[..2]), maximum_abs(&values.1[..2]));
                    }
                    4 => {
                        let values =
                            interpolate_stereo_avx2_fma(left_history, right_history, table);
                        return (maximum_abs(&values.0), maximum_abs(&values.1));
                    }
                    5 => {
                        return interpolate_stereo_5x_avx2_fma_peaks(
                            left_history,
                            right_history,
                            table,
                        );
                    }
                    _ => {
                        return interpolate_stereo_tiled_avx2_fma_peaks(
                            left_history,
                            right_history,
                            table,
                            left.factor,
                        );
                    }
                }
            }
        }
        #[cfg(target_arch = "aarch64")]
        // SAFETY: Advanced SIMD is part of the AArch64 architecture.
        return unsafe {
            match left.factor {
                2 => {
                    let values = interpolate_stereo_2x_neon(left_history, right_history, table);
                    (maximum_abs(&values.0[..2]), maximum_abs(&values.1[..2]))
                }
                4 => {
                    let values = interpolate_stereo_neon(left_history, right_history, table);
                    (maximum_abs(&values.0), maximum_abs(&values.1))
                }
                5 => interpolate_stereo_5x_neon_peaks(left_history, right_history, table),
                _ => interpolate_stereo_tiled_neon_peaks(
                    left_history,
                    right_history,
                    table,
                    left.factor,
                ),
            }
        };
        #[cfg(not(target_arch = "aarch64"))]
        interpolate_stereo_scalar_peaks(left_history, right_history, table, left.factor)
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
        if self.force_scalar {
            return;
        }
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
            frame_peak = frame_peak.max(self.interpolated_peak());
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
            frame_peak = frame_peak.max(self.interpolated_peak());
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
            let interpolated = Self::interpolated_stereo_peaks(left, right);
            left_frame_peak = left_frame_peak.max(interpolated.0);
            right_frame_peak = right_frame_peak.max(interpolated.1);
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

        if !skip_left && !skip_right {
            let interpolated = Self::interpolated_stereo_peaks(left, right);
            left_frame_peak = left_frame_peak.max(interpolated.0);
            right_frame_peak = right_frame_peak.max(interpolated.1);
        } else if !skip_left {
            left_frame_peak = left_frame_peak.max(left.interpolated_peak());
        } else {
            right_frame_peak = right_frame_peak.max(right.interpolated_peak());
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
    if sample_rate == 0 {
        return MAX_PHASES;
    }
    TRUE_PEAK_DOMAIN_HZ
        .div_ceil(sample_rate)
        .clamp(1, MAX_PHASES as u32) as usize
}

#[inline(always)]
fn maximum_abs(values: &[f64]) -> f32 {
    values
        .iter()
        .fold(0.0_f32, |peak, value| peak.max(value.abs() as f32))
}

#[inline]
fn interpolate_scalar_peak(
    history: &[f64; TAPS_PER_PHASE],
    table: &PhaseTable,
    factor: usize,
) -> f32 {
    let mut peak = 0.0_f32;
    for phase_start in (0..factor).step_by(SIMD_PHASES) {
        let lanes = (factor - phase_start).min(SIMD_PHASES);
        let mut values = [0.0_f64; SIMD_PHASES];
        for tap in 0..TAPS_PER_PHASE {
            for lane in 0..lanes {
                values[lane] += table[tap][phase_start + lane] * history[tap];
            }
        }
        peak = peak.max(maximum_abs(&values[..lanes]));
    }
    peak
}

#[inline]
fn interpolate_stereo_scalar_peaks(
    left_history: &[f64; TAPS_PER_PHASE],
    right_history: &[f64; TAPS_PER_PHASE],
    table: &PhaseTable,
    factor: usize,
) -> (f32, f32) {
    let mut left_peak = 0.0_f32;
    let mut right_peak = 0.0_f32;
    for phase_start in (0..factor).step_by(SIMD_PHASES) {
        let lanes = (factor - phase_start).min(SIMD_PHASES);
        let mut left_values = [0.0_f64; SIMD_PHASES];
        let mut right_values = [0.0_f64; SIMD_PHASES];
        for tap in 0..TAPS_PER_PHASE {
            for lane in 0..lanes {
                let coefficient = table[tap][phase_start + lane];
                left_values[lane] += coefficient * left_history[tap];
                right_values[lane] += coefficient * right_history[tap];
            }
        }
        left_peak = left_peak.max(maximum_abs(&left_values[..lanes]));
        right_peak = right_peak.max(maximum_abs(&right_values[..lanes]));
    }
    (left_peak, right_peak)
}

#[cfg(test)]
#[inline]
fn interpolate_scalar(
    history: &[f64; TAPS_PER_PHASE],
    table: &PhaseTable,
    factor: usize,
) -> [f64; MAX_PHASES] {
    let mut output = [0.0; MAX_PHASES];
    // Four-phase tiles keep the common scalar fallback compact while safely
    // covering non-power-of-two ratios up to 24x. Each phase retains the same
    // tap accumulation order as the 2x/4x implementations.
    for phase_start in (0..factor).step_by(SIMD_PHASES) {
        let phase_end = (phase_start + SIMD_PHASES).min(factor);
        for tap in 0..TAPS_PER_PHASE {
            for phase in phase_start..phase_end {
                output[phase] += table[tap][phase] * history[tap];
            }
        }
    }
    output
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn interpolate_2x_avx2_fma(
    history: &[f64; TAPS_PER_PHASE],
    table: &PhaseTable,
) -> [f64; SIMD_PHASES] {
    let mut accumulator = _mm_setzero_pd();
    for tap in 0..TAPS_PER_PHASE {
        let sample = _mm_set1_pd(*history.get_unchecked(tap));
        let coefficients = _mm_loadu_pd(table.get_unchecked(tap).as_ptr());
        accumulator = _mm_fmadd_pd(sample, coefficients, accumulator);
    }
    let mut output = [0.0; SIMD_PHASES];
    _mm_storeu_pd(output.as_mut_ptr(), accumulator);
    output
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn interpolate_avx2_fma(
    history: &[f64; TAPS_PER_PHASE],
    table: &PhaseTable,
) -> [f64; SIMD_PHASES] {
    let mut accumulator = _mm256_setzero_pd();
    for tap in 0..TAPS_PER_PHASE {
        let sample = _mm256_set1_pd(*history.get_unchecked(tap));
        let coefficients = _mm256_loadu_pd(table.get_unchecked(tap).as_ptr());
        accumulator = _mm256_fmadd_pd(sample, coefficients, accumulator);
    }
    let mut output = [0.0; SIMD_PHASES];
    _mm256_storeu_pd(output.as_mut_ptr(), accumulator);
    output
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn interpolate_5x_avx2_fma_peak(history: &[f64; TAPS_PER_PHASE], table: &PhaseTable) -> f32 {
    let mut first_four = _mm256_setzero_pd();
    let mut fifth = 0.0_f64;
    for tap in 0..TAPS_PER_PHASE {
        let sample = *history.get_unchecked(tap);
        let coefficients = table.get_unchecked(tap);
        first_four = _mm256_fmadd_pd(
            _mm256_set1_pd(sample),
            _mm256_loadu_pd(coefficients.as_ptr()),
            first_four,
        );
        fifth = sample.mul_add(*coefficients.get_unchecked(4), fifth);
    }
    let mut values = [0.0; SIMD_PHASES];
    _mm256_storeu_pd(values.as_mut_ptr(), first_four);
    maximum_abs(&values).max(fifth.abs() as f32)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn interpolate_tiled_avx2_fma_peak(
    history: &[f64; TAPS_PER_PHASE],
    table: &PhaseTable,
    factor: usize,
) -> f32 {
    debug_assert!((2..=MAX_PHASES).contains(&factor));
    let mut peak = 0.0_f32;
    for phase_start in (0..factor).step_by(SIMD_PHASES) {
        let mut accumulator = _mm256_setzero_pd();
        for tap in 0..TAPS_PER_PHASE {
            let sample = _mm256_set1_pd(*history.get_unchecked(tap));
            let coefficients = _mm256_loadu_pd(table.get_unchecked(tap).as_ptr().add(phase_start));
            accumulator = _mm256_fmadd_pd(sample, coefficients, accumulator);
        }
        let mut values = [0.0; SIMD_PHASES];
        _mm256_storeu_pd(values.as_mut_ptr(), accumulator);
        let valid_lanes = (factor - phase_start).min(SIMD_PHASES);
        peak = peak.max(maximum_abs(&values[..valid_lanes]));
    }
    peak
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn interpolate_stereo_2x_avx2_fma(
    left_history: &[f64; TAPS_PER_PHASE],
    right_history: &[f64; TAPS_PER_PHASE],
    table: &PhaseTable,
) -> ([f64; SIMD_PHASES], [f64; SIMD_PHASES]) {
    let mut left_accumulator = _mm_setzero_pd();
    let mut right_accumulator = _mm_setzero_pd();
    for tap in 0..TAPS_PER_PHASE {
        let coefficients = _mm_loadu_pd(table.get_unchecked(tap).as_ptr());
        let left_sample = _mm_set1_pd(*left_history.get_unchecked(tap));
        let right_sample = _mm_set1_pd(*right_history.get_unchecked(tap));
        left_accumulator = _mm_fmadd_pd(left_sample, coefficients, left_accumulator);
        right_accumulator = _mm_fmadd_pd(right_sample, coefficients, right_accumulator);
    }
    let mut left_output = [0.0; SIMD_PHASES];
    let mut right_output = [0.0; SIMD_PHASES];
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
) -> ([f64; SIMD_PHASES], [f64; SIMD_PHASES]) {
    let mut left_accumulator = _mm256_setzero_pd();
    let mut right_accumulator = _mm256_setzero_pd();
    for tap in 0..TAPS_PER_PHASE {
        let coefficients = _mm256_loadu_pd(table.get_unchecked(tap).as_ptr());
        let left_sample = _mm256_set1_pd(*left_history.get_unchecked(tap));
        let right_sample = _mm256_set1_pd(*right_history.get_unchecked(tap));
        left_accumulator = _mm256_fmadd_pd(left_sample, coefficients, left_accumulator);
        right_accumulator = _mm256_fmadd_pd(right_sample, coefficients, right_accumulator);
    }
    let mut left_output = [0.0; SIMD_PHASES];
    let mut right_output = [0.0; SIMD_PHASES];
    _mm256_storeu_pd(left_output.as_mut_ptr(), left_accumulator);
    _mm256_storeu_pd(right_output.as_mut_ptr(), right_accumulator);
    (left_output, right_output)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn interpolate_stereo_5x_avx2_fma_peaks(
    left_history: &[f64; TAPS_PER_PHASE],
    right_history: &[f64; TAPS_PER_PHASE],
    table: &PhaseTable,
) -> (f32, f32) {
    let mut left_first_four = _mm256_setzero_pd();
    let mut right_first_four = _mm256_setzero_pd();
    let mut fifth = _mm_setzero_pd();
    for tap in 0..TAPS_PER_PHASE {
        let coefficients = table.get_unchecked(tap);
        let first_four_coefficients = _mm256_loadu_pd(coefficients.as_ptr());
        let left_sample = *left_history.get_unchecked(tap);
        let right_sample = *right_history.get_unchecked(tap);
        left_first_four = _mm256_fmadd_pd(
            _mm256_set1_pd(left_sample),
            first_four_coefficients,
            left_first_four,
        );
        right_first_four = _mm256_fmadd_pd(
            _mm256_set1_pd(right_sample),
            first_four_coefficients,
            right_first_four,
        );
        fifth = _mm_fmadd_pd(
            _mm_set_pd(right_sample, left_sample),
            _mm_set1_pd(*coefficients.get_unchecked(4)),
            fifth,
        );
    }
    let mut left_values = [0.0; SIMD_PHASES];
    let mut right_values = [0.0; SIMD_PHASES];
    let mut fifth_values = [0.0; 2];
    _mm256_storeu_pd(left_values.as_mut_ptr(), left_first_four);
    _mm256_storeu_pd(right_values.as_mut_ptr(), right_first_four);
    _mm_storeu_pd(fifth_values.as_mut_ptr(), fifth);
    (
        maximum_abs(&left_values).max(fifth_values[0].abs() as f32),
        maximum_abs(&right_values).max(fifth_values[1].abs() as f32),
    )
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn interpolate_stereo_tiled_avx2_fma_peaks(
    left_history: &[f64; TAPS_PER_PHASE],
    right_history: &[f64; TAPS_PER_PHASE],
    table: &PhaseTable,
    factor: usize,
) -> (f32, f32) {
    debug_assert!((2..=MAX_PHASES).contains(&factor));
    let mut left_peak = 0.0_f32;
    let mut right_peak = 0.0_f32;
    for phase_start in (0..factor).step_by(SIMD_PHASES) {
        let mut left_accumulator = _mm256_setzero_pd();
        let mut right_accumulator = _mm256_setzero_pd();
        for tap in 0..TAPS_PER_PHASE {
            let coefficients = _mm256_loadu_pd(table.get_unchecked(tap).as_ptr().add(phase_start));
            let left_sample = _mm256_set1_pd(*left_history.get_unchecked(tap));
            let right_sample = _mm256_set1_pd(*right_history.get_unchecked(tap));
            left_accumulator = _mm256_fmadd_pd(left_sample, coefficients, left_accumulator);
            right_accumulator = _mm256_fmadd_pd(right_sample, coefficients, right_accumulator);
        }
        let mut left_values = [0.0; SIMD_PHASES];
        let mut right_values = [0.0; SIMD_PHASES];
        _mm256_storeu_pd(left_values.as_mut_ptr(), left_accumulator);
        _mm256_storeu_pd(right_values.as_mut_ptr(), right_accumulator);
        let valid_lanes = (factor - phase_start).min(SIMD_PHASES);
        left_peak = left_peak.max(maximum_abs(&left_values[..valid_lanes]));
        right_peak = right_peak.max(maximum_abs(&right_values[..valid_lanes]));
    }
    (left_peak, right_peak)
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn interpolate_2x_neon(
    history: &[f64; TAPS_PER_PHASE],
    table: &PhaseTable,
) -> [f64; SIMD_PHASES] {
    let mut accumulator = vdupq_n_f64(0.0);
    for tap in 0..TAPS_PER_PHASE {
        let sample = vdupq_n_f64(*history.get_unchecked(tap));
        let coefficients = vld1q_f64(table.get_unchecked(tap).as_ptr());
        accumulator = vfmaq_f64(accumulator, sample, coefficients);
    }
    let mut output = [0.0; SIMD_PHASES];
    vst1q_f64(output.as_mut_ptr(), accumulator);
    output
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn interpolate_neon(
    history: &[f64; TAPS_PER_PHASE],
    table: &PhaseTable,
) -> [f64; SIMD_PHASES] {
    let mut low = vdupq_n_f64(0.0);
    let mut high = vdupq_n_f64(0.0);
    for tap in 0..TAPS_PER_PHASE {
        let sample = vdupq_n_f64(*history.get_unchecked(tap));
        let coefficients = table.get_unchecked(tap);
        low = vfmaq_f64(low, sample, vld1q_f64(coefficients.as_ptr()));
        high = vfmaq_f64(high, sample, vld1q_f64(coefficients.as_ptr().add(2)));
    }
    let mut output = [0.0; SIMD_PHASES];
    vst1q_f64(output.as_mut_ptr(), low);
    vst1q_f64(output.as_mut_ptr().add(2), high);
    output
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn interpolate_5x_neon_peak(history: &[f64; TAPS_PER_PHASE], table: &PhaseTable) -> f32 {
    let mut low = vdupq_n_f64(0.0);
    let mut high = vdupq_n_f64(0.0);
    let mut fifth = 0.0_f64;
    for tap in 0..TAPS_PER_PHASE {
        let sample_value = *history.get_unchecked(tap);
        let sample = vdupq_n_f64(sample_value);
        let coefficients = table.get_unchecked(tap);
        low = vfmaq_f64(low, sample, vld1q_f64(coefficients.as_ptr()));
        high = vfmaq_f64(high, sample, vld1q_f64(coefficients.as_ptr().add(2)));
        fifth = sample_value.mul_add(*coefficients.get_unchecked(4), fifth);
    }
    let mut values = [0.0; SIMD_PHASES];
    vst1q_f64(values.as_mut_ptr(), low);
    vst1q_f64(values.as_mut_ptr().add(2), high);
    maximum_abs(&values).max(fifth.abs() as f32)
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn interpolate_tiled_neon_peak(
    history: &[f64; TAPS_PER_PHASE],
    table: &PhaseTable,
    factor: usize,
) -> f32 {
    debug_assert!((2..=MAX_PHASES).contains(&factor));
    let mut peak = 0.0_f32;
    for phase_start in (0..factor).step_by(SIMD_PHASES) {
        let mut low = vdupq_n_f64(0.0);
        let mut high = vdupq_n_f64(0.0);
        for tap in 0..TAPS_PER_PHASE {
            let sample = vdupq_n_f64(*history.get_unchecked(tap));
            let coefficients = table.get_unchecked(tap).as_ptr().add(phase_start);
            low = vfmaq_f64(low, sample, vld1q_f64(coefficients));
            high = vfmaq_f64(high, sample, vld1q_f64(coefficients.add(2)));
        }
        let mut values = [0.0; SIMD_PHASES];
        vst1q_f64(values.as_mut_ptr(), low);
        vst1q_f64(values.as_mut_ptr().add(2), high);
        let valid_lanes = (factor - phase_start).min(SIMD_PHASES);
        peak = peak.max(maximum_abs(&values[..valid_lanes]));
    }
    peak
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn interpolate_stereo_2x_neon(
    left_history: &[f64; TAPS_PER_PHASE],
    right_history: &[f64; TAPS_PER_PHASE],
    table: &PhaseTable,
) -> ([f64; SIMD_PHASES], [f64; SIMD_PHASES]) {
    let mut left_accumulator = vdupq_n_f64(0.0);
    let mut right_accumulator = vdupq_n_f64(0.0);
    for tap in 0..TAPS_PER_PHASE {
        let coefficients = vld1q_f64(table.get_unchecked(tap).as_ptr());
        let left_sample = vdupq_n_f64(*left_history.get_unchecked(tap));
        let right_sample = vdupq_n_f64(*right_history.get_unchecked(tap));
        left_accumulator = vfmaq_f64(left_accumulator, left_sample, coefficients);
        right_accumulator = vfmaq_f64(right_accumulator, right_sample, coefficients);
    }
    let mut left_output = [0.0; SIMD_PHASES];
    let mut right_output = [0.0; SIMD_PHASES];
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
) -> ([f64; SIMD_PHASES], [f64; SIMD_PHASES]) {
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
    let mut left_output = [0.0; SIMD_PHASES];
    let mut right_output = [0.0; SIMD_PHASES];
    vst1q_f64(left_output.as_mut_ptr(), left_low);
    vst1q_f64(left_output.as_mut_ptr().add(2), left_high);
    vst1q_f64(right_output.as_mut_ptr(), right_low);
    vst1q_f64(right_output.as_mut_ptr().add(2), right_high);
    (left_output, right_output)
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn interpolate_stereo_5x_neon_peaks(
    left_history: &[f64; TAPS_PER_PHASE],
    right_history: &[f64; TAPS_PER_PHASE],
    table: &PhaseTable,
) -> (f32, f32) {
    let mut left_low = vdupq_n_f64(0.0);
    let mut left_high = vdupq_n_f64(0.0);
    let mut right_low = vdupq_n_f64(0.0);
    let mut right_high = vdupq_n_f64(0.0);
    let mut fifth = vdupq_n_f64(0.0);
    for tap in 0..TAPS_PER_PHASE {
        let coefficients = table.get_unchecked(tap);
        let low_coefficients = vld1q_f64(coefficients.as_ptr());
        let high_coefficients = vld1q_f64(coefficients.as_ptr().add(2));
        let left_sample_value = *left_history.get_unchecked(tap);
        let right_sample_value = *right_history.get_unchecked(tap);
        let left_sample = vdupq_n_f64(left_sample_value);
        let right_sample = vdupq_n_f64(right_sample_value);
        left_low = vfmaq_f64(left_low, left_sample, low_coefficients);
        left_high = vfmaq_f64(left_high, left_sample, high_coefficients);
        right_low = vfmaq_f64(right_low, right_sample, low_coefficients);
        right_high = vfmaq_f64(right_high, right_sample, high_coefficients);
        let tail_samples = vsetq_lane_f64::<1>(right_sample_value, left_sample);
        fifth = vfmaq_f64(
            fifth,
            tail_samples,
            vdupq_n_f64(*coefficients.get_unchecked(4)),
        );
    }
    let mut left_values = [0.0; SIMD_PHASES];
    let mut right_values = [0.0; SIMD_PHASES];
    let mut fifth_values = [0.0; 2];
    vst1q_f64(left_values.as_mut_ptr(), left_low);
    vst1q_f64(left_values.as_mut_ptr().add(2), left_high);
    vst1q_f64(right_values.as_mut_ptr(), right_low);
    vst1q_f64(right_values.as_mut_ptr().add(2), right_high);
    vst1q_f64(fifth_values.as_mut_ptr(), fifth);
    (
        maximum_abs(&left_values).max(fifth_values[0].abs() as f32),
        maximum_abs(&right_values).max(fifth_values[1].abs() as f32),
    )
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn interpolate_stereo_tiled_neon_peaks(
    left_history: &[f64; TAPS_PER_PHASE],
    right_history: &[f64; TAPS_PER_PHASE],
    table: &PhaseTable,
    factor: usize,
) -> (f32, f32) {
    debug_assert!((2..=MAX_PHASES).contains(&factor));
    let mut left_peak = 0.0_f32;
    let mut right_peak = 0.0_f32;
    for phase_start in (0..factor).step_by(SIMD_PHASES) {
        let mut left_low = vdupq_n_f64(0.0);
        let mut left_high = vdupq_n_f64(0.0);
        let mut right_low = vdupq_n_f64(0.0);
        let mut right_high = vdupq_n_f64(0.0);
        for tap in 0..TAPS_PER_PHASE {
            let coefficients = table.get_unchecked(tap).as_ptr().add(phase_start);
            let low_coefficients = vld1q_f64(coefficients);
            let high_coefficients = vld1q_f64(coefficients.add(2));
            let left_sample = vdupq_n_f64(*left_history.get_unchecked(tap));
            let right_sample = vdupq_n_f64(*right_history.get_unchecked(tap));
            left_low = vfmaq_f64(left_low, left_sample, low_coefficients);
            left_high = vfmaq_f64(left_high, left_sample, high_coefficients);
            right_low = vfmaq_f64(right_low, right_sample, low_coefficients);
            right_high = vfmaq_f64(right_high, right_sample, high_coefficients);
        }
        let mut left_values = [0.0; SIMD_PHASES];
        let mut right_values = [0.0; SIMD_PHASES];
        vst1q_f64(left_values.as_mut_ptr(), left_low);
        vst1q_f64(left_values.as_mut_ptr().add(2), left_high);
        vst1q_f64(right_values.as_mut_ptr(), right_low);
        vst1q_f64(right_values.as_mut_ptr().add(2), right_high);
        let valid_lanes = (factor - phase_start).min(SIMD_PHASES);
        left_peak = left_peak.max(maximum_abs(&left_values[..valid_lanes]));
        right_peak = right_peak.max(maximum_abs(&right_values[..valid_lanes]));
    }
    (left_peak, right_peak)
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
fn reference_finite_peak(samples: &[f32], factor: usize) -> f32 {
    if factor == 1 {
        return samples
            .iter()
            .fold(0.0_f32, |maximum, sample| maximum.max(sample.abs()));
    }
    let table = phase_table(factor);
    let mut history = [0.0_f64; TAPS_PER_PHASE];
    let mut peak = 0.0_f32;
    for sample in samples
        .iter()
        .copied()
        .chain(std::iter::repeat_n(0.0, TAPS_PER_PHASE - 1))
    {
        history.copy_within(0..TAPS_PER_PHASE - 1, 1);
        history[0] = f64::from(sample);
        peak = peak.max(sample.abs());
        let values = interpolate_scalar(&history, table, factor);
        for value in &values[..factor] {
            peak = peak.max(value.abs() as f32);
        }
    }
    peak
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::{ChannelRole, PcmKind};

    fn assert_first_two_lanes_match(expected: [f64; SIMD_PHASES], actual: [f64; SIMD_PHASES]) {
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

    fn representative_sample_rate(factor: usize) -> u32 {
        let sample_rate = if factor == 5 {
            // Exercise the most common non-power-of-two ratio directly.
            44_100
        } else {
            TRUE_PEAK_DOMAIN_HZ.div_ceil(factor as u32)
        };
        assert_eq!(oversample_factor(sample_rate), factor);
        sample_rate
    }

    #[test]
    fn reference_phase_tables_are_the_committed_f32_vectors() {
        let vectors: &[(usize, Vec<Vec<u32>>)] = &[
            (
                2,
                REFERENCE_PHASE_2X.iter().map(|row| row.to_vec()).collect(),
            ),
            (
                3,
                REFERENCE_PHASE_3X.iter().map(|row| row.to_vec()).collect(),
            ),
            (
                4,
                REFERENCE_PHASE_4X.iter().map(|row| row.to_vec()).collect(),
            ),
            (
                5,
                REFERENCE_PHASE_5X.iter().map(|row| row.to_vec()).collect(),
            ),
        ];
        for (factor, expected) in vectors {
            let table = reference_phase_table(*factor).unwrap();
            for tap in 0..TAPS_PER_PHASE {
                for phase in 0..*factor {
                    assert_eq!(
                        table[tap][phase].to_bits(),
                        f64::from(f32::from_bits(expected[tap][phase])).to_bits(),
                        "{factor}x tap {tap} phase {phase}"
                    );
                }
                assert!(table[tap][*factor..].iter().all(|value| *value == 0.0));
            }
        }
        assert!(reference_phase_table(6)
            .unwrap_err()
            .contains("do not support"));
    }

    #[test]
    fn finite_reference_meter_is_scalar_and_chunk_invariant() {
        let samples = (0_u64..48_137)
            .map(|index| {
                let code = ((index.wrapping_mul(65_537).wrapping_add(17)) & 0x00ff_ffff) as i32
                    - 0x0080_0000;
                code as f32 / 0x0100_0000 as f32
            })
            .collect::<Vec<_>>();
        let mut whole = TruePeakMeter::for_finite_reference_sample_rate(48_000).unwrap();
        whole.process(&samples);
        let whole = whole.finish_peak();
        let mut chunked = TruePeakMeter::for_finite_reference_sample_rate(48_000).unwrap();
        for chunk in samples.chunks(257) {
            chunked.process(chunk);
        }
        assert_eq!(whole.to_bits(), chunked.finish_peak().to_bits());
    }

    #[cfg(target_arch = "x86_64")]
    unsafe fn architecture_simd_peak(
        history: &[f64; TAPS_PER_PHASE],
        table: &PhaseTable,
        factor: usize,
    ) -> f32 {
        match factor {
            2 => maximum_abs(&interpolate_2x_avx2_fma(history, table)[..2]),
            4 => maximum_abs(&interpolate_avx2_fma(history, table)),
            5 => interpolate_5x_avx2_fma_peak(history, table),
            _ => interpolate_tiled_avx2_fma_peak(history, table, factor),
        }
    }

    #[cfg(target_arch = "x86_64")]
    unsafe fn architecture_stereo_simd_peaks(
        left_history: &[f64; TAPS_PER_PHASE],
        right_history: &[f64; TAPS_PER_PHASE],
        table: &PhaseTable,
        factor: usize,
    ) -> (f32, f32) {
        match factor {
            2 => {
                let values = interpolate_stereo_2x_avx2_fma(left_history, right_history, table);
                (maximum_abs(&values.0[..2]), maximum_abs(&values.1[..2]))
            }
            4 => {
                let values = interpolate_stereo_avx2_fma(left_history, right_history, table);
                (maximum_abs(&values.0), maximum_abs(&values.1))
            }
            5 => interpolate_stereo_5x_avx2_fma_peaks(left_history, right_history, table),
            _ => {
                interpolate_stereo_tiled_avx2_fma_peaks(left_history, right_history, table, factor)
            }
        }
    }

    #[cfg(target_arch = "aarch64")]
    unsafe fn architecture_simd_peak(
        history: &[f64; TAPS_PER_PHASE],
        table: &PhaseTable,
        factor: usize,
    ) -> f32 {
        match factor {
            2 => maximum_abs(&interpolate_2x_neon(history, table)[..2]),
            4 => maximum_abs(&interpolate_neon(history, table)),
            5 => interpolate_5x_neon_peak(history, table),
            _ => interpolate_tiled_neon_peak(history, table, factor),
        }
    }

    #[cfg(target_arch = "aarch64")]
    unsafe fn architecture_stereo_simd_peaks(
        left_history: &[f64; TAPS_PER_PHASE],
        right_history: &[f64; TAPS_PER_PHASE],
        table: &PhaseTable,
        factor: usize,
    ) -> (f32, f32) {
        match factor {
            2 => {
                let values = interpolate_stereo_2x_neon(left_history, right_history, table);
                (maximum_abs(&values.0[..2]), maximum_abs(&values.1[..2]))
            }
            4 => {
                let values = interpolate_stereo_neon(left_history, right_history, table);
                (maximum_abs(&values.0), maximum_abs(&values.1))
            }
            5 => interpolate_stereo_5x_neon_peaks(left_history, right_history, table),
            _ => interpolate_stereo_tiled_neon_peaks(left_history, right_history, table, factor),
        }
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    #[test]
    fn tiled_simd_matches_scalar_oracle_and_ignores_partial_tail_lanes() {
        #[cfg(target_arch = "x86_64")]
        if !(std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("fma")) {
            return;
        }

        for factor in 2..=MAX_PHASES {
            let mut table = *phase_table(factor);
            let tile_end = factor.div_ceil(SIMD_PHASES) * SIMD_PHASES;
            for coefficients in &mut table {
                coefficients[factor..tile_end].fill(f64::INFINITY);
            }
            for iteration in 0..16 {
                let left_history = std::array::from_fn(|tap| {
                    let index = tap + iteration * TAPS_PER_PHASE;
                    (index as f64 * 0.173).sin() * 0.83 + (index as f64 * 0.071 + 0.4).cos() * 0.11
                });
                let right_history = std::array::from_fn(|tap| {
                    let index = tap + iteration * TAPS_PER_PHASE;
                    (index as f64 * 0.257 + 0.2).cos() * 0.67 - (index as f64 * 0.113).sin() * 0.19
                });
                let expected_left =
                    maximum_abs(&interpolate_scalar(&left_history, &table, factor)[..factor]);
                let expected_right =
                    maximum_abs(&interpolate_scalar(&right_history, &table, factor)[..factor]);

                // SAFETY: x86 feature support was checked above; AArch64
                // Advanced SIMD is part of the base architecture.
                let (actual_left, actual_stereo) = unsafe {
                    (
                        architecture_simd_peak(&left_history, &table, factor),
                        architecture_stereo_simd_peaks(
                            &left_history,
                            &right_history,
                            &table,
                            factor,
                        ),
                    )
                };
                assert!(
                    (actual_left - expected_left).abs() <= 1.0e-6,
                    "factor {factor}, iteration {iteration}: mono {actual_left} != {expected_left}"
                );
                assert!(
                    (actual_stereo.0 - expected_left).abs() <= 1.0e-6,
                    "factor {factor}, iteration {iteration}: left {} != {expected_left}",
                    actual_stereo.0
                );
                assert!(
                    (actual_stereo.1 - expected_right).abs() <= 1.0e-6,
                    "factor {factor}, iteration {iteration}: right {} != {expected_right}",
                    actual_stereo.1
                );
            }
        }
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
    fn every_oversampling_factor_preserves_finite_chunk_semantics() {
        let samples: Vec<f32> = (0..1021)
            .map(|index| {
                let first = (index as f64 * 0.173).sin();
                let second = (index as f64 * 0.071 + 0.4).cos();
                (0.71 * first + 0.23 * second) as f32
            })
            .collect();

        for factor in 2..=MAX_PHASES {
            let sample_rate = representative_sample_rate(factor);
            let mut whole = TruePeakMeter::for_finite_sample_rate(sample_rate);
            whole.process(&samples);
            let whole_peak = whole.finish_peak();

            let mut chunked = TruePeakMeter::for_finite_sample_rate(sample_rate);
            for chunk in samples.chunks(37) {
                chunked.process(chunk);
            }
            let chunked_peak = chunked.finish_peak();
            assert_eq!(
                chunked_peak.to_bits(),
                whole_peak.to_bits(),
                "factor {factor}, {sample_rate} Hz"
            );

            let expected = reference_finite_peak(&samples, factor);
            assert!(
                (whole_peak - expected).abs() <= 1.0e-6,
                "factor {factor}, {sample_rate} Hz: {whole_peak} != {expected}"
            );
        }
    }

    #[test]
    fn every_oversampling_factor_preserves_stereo_pair_semantics() {
        let left: Vec<f32> = (0..641)
            .map(|index| ((index as f64 * 0.173).sin() * 0.83) as f32)
            .collect();
        let right: Vec<f32> = (0..641)
            .map(|index| ((index as f64 * 0.071 + 0.4).cos() * 0.61) as f32)
            .collect();

        for factor in 2..=MAX_PHASES {
            let sample_rate = representative_sample_rate(factor);
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
                assert_eq!(actual.0.to_bits(), expected.0.to_bits(), "factor {factor}");
                assert_eq!(actual.1.to_bits(), expected.1.to_bits(), "factor {factor}");
            }
            assert_eq!(
                paired_left.peak().to_bits(),
                expected_left.peak().to_bits(),
                "left factor {factor}"
            );
            assert_eq!(
                paired_right.peak().to_bits(),
                expected_right.peak().to_bits(),
                "right factor {factor}"
            );
        }
    }

    #[test]
    fn every_oversampling_factor_preserves_pruning_and_future_history() {
        let quiet: Vec<f32> = (0..1536)
            .map(|index| ((index as f64 * 0.173).sin() * 0.001) as f32)
            .collect();
        let future: Vec<f32> = (0..257)
            .map(|index| {
                let first = (index as f64 * 0.371).sin();
                let second = (index as f64 * 0.113 + 0.7).cos();
                (0.73 * first + 0.19 * second) as f32
            })
            .collect();

        for factor in 2..=MAX_PHASES {
            let sample_rate = representative_sample_rate(factor);
            let mut exact = TruePeakMeter::for_sample_rate(sample_rate);
            let mut pruned = TruePeakMeter::for_sample_rate(sample_rate);
            let prefix = std::iter::once(0.99_f32).chain(quiet[..1024].iter().copied());
            let mut skipped = 0_usize;
            for sample in prefix {
                exact.process_sample(sample);
                skipped += usize::from(pruned.process_peak_only_sample(sample));
            }
            assert!(skipped > 0, "factor {factor} never armed sample pruning");

            for &sample in &quiet[1024..] {
                exact.process_sample(sample);
            }
            assert!(
                pruned.try_skip_peak_only_block(&quiet[1024..]),
                "factor {factor} did not skip a proven quiet block"
            );
            assert_eq!(
                pruned.peak().to_bits(),
                exact.peak().to_bits(),
                "factor {factor} quiet block"
            );

            for &sample in &future {
                exact.process_sample(sample);
                pruned.process_peak_only_sample(sample);
            }
            assert_eq!(
                pruned.peak().to_bits(),
                exact.peak().to_bits(),
                "factor {factor} future history"
            );
        }
    }

    #[test]
    fn finite_measurement_zero_pads_both_boundaries_and_drains_tail() {
        let samples = [
            0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, -1.0,
            -1.0,
        ];
        let mut unfinished = TruePeakMeter::for_finite_sample_rate(48_000);
        unfinished.process(&samples);
        let prefix_peak = unfinished.peak();

        let mut whole = TruePeakMeter::for_finite_sample_rate(48_000);
        whole.process(&samples);
        let whole_peak = whole.finish_peak();
        let mut chunked = TruePeakMeter::for_finite_sample_rate(48_000);
        for chunk in samples.chunks(3) {
            chunked.process(chunk);
        }
        let chunked_peak = chunked.finish_peak();

        assert!(
            whole_peak > prefix_peak * 1.2,
            "FIR tail {whole_peak} did not exceed unfinished prefix {prefix_peak}"
        );
        assert_eq!(chunked_peak.to_bits(), whole_peak.to_bits());
        let expected = reference_finite_peak(&samples, 4);
        assert!((whole_peak - expected).abs() <= 1.0e-6);

        let buffer = AudioBuffer {
            sample_rate: 48_000,
            channels: 1,
            frames: samples.len(),
            data: vec![samples.to_vec()],
            channel_roles: vec![ChannelRole::Main],
            source_kind: PcmKind::F32,
        };
        assert_eq!(measure_true_peak(&buffer).to_bits(), whole_peak.to_bits());
    }

    #[test]
    fn reconstructed_finite_tail_discards_the_artificial_replay_boundary() {
        // The retained suffix starts with a full-scale sample, but that sample
        // followed a steady full-scale prefix in the real stream. Replaying
        // the suffix as a new finite signal invents a zero-to-one boundary and
        // must not attribute its peak to EOF.
        let mut samples = vec![1.0; 32];
        samples.extend([0.0; TAPS_PER_PHASE - 2]);
        let recent = &samples[samples.len() - (TAPS_PER_PHASE - 1)..];

        let mut full = TruePeakMeter::for_finite_sample_rate(48_000);
        for &sample in &samples {
            full.process_sample(sample);
        }
        let expected_eof_peak = (0..TAPS_PER_PHASE - 1)
            .map(|_| full.process_sample(0.0))
            .fold(0.0, f32::max);

        let mut replay_inclusive = TruePeakMeter::for_finite_sample_rate(48_000);
        replay_inclusive.process(recent);
        let replay_inclusive_peak = replay_inclusive.finish_peak();
        assert!(
            replay_inclusive_peak > expected_eof_peak,
            "fixture did not expose replay boundary: replay={replay_inclusive_peak}, EOF={expected_eof_peak}"
        );

        let reconstructed = TruePeakMeter::finite_tail_peak_from_recent_samples(48_000, recent);
        assert_eq!(reconstructed.to_bits(), expected_eof_peak.to_bits());
    }

    #[test]
    fn finite_measurement_matches_full_convolution_for_all_domain_factors() {
        let samples = [0.81, -0.37, 0.19, -0.93, 0.44, 0.08, -0.61];
        for (sample_rate, factor) in [
            (8_000, 24),
            (11_025, 18),
            (16_000, 12),
            (22_050, 9),
            (32_000, 6),
            (44_100, 5),
            (64_000, 3),
            (96_000, 2),
            (192_000, 1),
            (384_000, 1),
        ] {
            let mut meter = TruePeakMeter::for_finite_sample_rate(sample_rate);
            for chunk in samples.chunks(2) {
                meter.process(chunk);
            }
            let actual = meter.finish_peak();
            let expected = reference_finite_peak(&samples, factor);
            assert!(
                (actual - expected).abs() <= 1.0e-6,
                "{sample_rate} Hz: {actual} != {expected}"
            );
        }
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

        for sample_rate in [
            8_000, 32_000, 44_100, 48_000, 64_000, 88_200, 96_000, 191_999, 192_000, 384_000,
        ] {
            for gain in [f32::MIN_POSITIVE, 0.000_123, 0.37, 1.0, 3.75, 65_536.0] {
                let scaled = samples
                    .iter()
                    .map(|sample| *sample * gain)
                    .collect::<Vec<_>>();
                let mut meter = TruePeakMeter::for_finite_sample_rate(sample_rate);
                meter.process(&scaled);
                let rounded_sample_peak_bound = source_peak * gain;
                let upper = upper_bound_from_sample_peak(sample_rate, rounded_sample_peak_bound);
                let actual = meter.finish_peak();
                assert!(
                    f64::from(actual) <= upper,
                    "{sample_rate} Hz, gain {gain}: {actual} > {upper}"
                );
            }
        }

        assert!(upper_bound_from_sample_peak(48_000, f32::NAN).is_infinite());
        assert!(upper_bound_from_sample_peak(48_000, f32::INFINITY).is_infinite());
        assert!(upper_bound_from_sample_peak(48_000, -0.5).is_infinite());
    }

    #[test]
    fn oversampling_ratio_tracks_input_sample_rate() {
        assert_eq!(oversample_factor(0), 24);
        assert_eq!(oversample_factor(8_000), 24);
        assert_eq!(oversample_factor(11_025), 18);
        assert_eq!(oversample_factor(16_000), 12);
        assert_eq!(oversample_factor(22_050), 9);
        assert_eq!(oversample_factor(32_000), 6);
        assert_eq!(oversample_factor(44_100), 5);
        assert_eq!(oversample_factor(64_000), 3);
        assert_eq!(oversample_factor(95_999), 3);
        assert_eq!(oversample_factor(96_000), 2);
        assert_eq!(oversample_factor(191_999), 2);
        assert_eq!(oversample_factor(192_000), 1);
        assert_eq!(oversample_factor(384_000), 1);
    }

    #[test]
    fn high_rate_meter_uses_sample_peak() {
        let samples = [-0.25, 0.5, -0.75, 0.625];
        let mut meter = TruePeakMeter::for_sample_rate(192_000);
        meter.process(&samples);
        assert_eq!(meter.peak(), 0.75);
    }
}
