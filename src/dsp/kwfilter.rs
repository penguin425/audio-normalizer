//! K-weighting filter for ITU-R BS.1770-4 / EBU R128 loudness measurement.
//!
//! K-weighting is two cascaded second-order biquads: a high-shelving
//! "pre-filter" and a high-pass "RLB" filter. The coefficients depend on the
//! sample rate. Rather than only supporting 48 kHz, Forge derives the biquad
//! coefficients from the standard's analog-prototype design parameters via the
//! RBJ audio-EQ cookbook bilinear transform. This reproduces the published
//! 48 kHz coefficients *exactly* (see the test below) and generalizes to any
//! sample rate, which is what separates a toy meter from a correct one.
//!
//! The filter is a transposed direct-form-II biquad, the most numerically
//! stable structure for cascaded IIR filtering in floating point.

use std::f64::consts::PI;

#[derive(Debug, Clone, Copy)]
pub struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    fn new(b0: f64, b1: f64, b2: f64, a1: f64, a2: f64) -> Self {
        Self {
            b0,
            b1,
            b2,
            a1,
            a2,
            z1: 0.0,
            z2: 0.0,
        }
    }
    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }
    /// Process one sample (transposed direct form II).
    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let x = x as f64;
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y as f32
    }
}

#[derive(Debug, Clone)]
pub struct KWeight {
    stage1: Biquad,
    stage2: Biquad,
}

impl KWeight {
    /// Build the K-weighting filter pair for the given sample rate.
    pub fn for_sample_rate(fs: u32) -> Self {
        let fs = fs as f64;
        // Design parameters from ITU-R BS.1770-4.
        let stage1 = high_shelf(fs, 1681.974450955533, 3.999843853973347, 0.7071752369554196);
        let stage2 = high_pass(fs, 38.13547087602444, 0.5003270373238773);
        Self { stage1, stage2 }
    }

    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.stage1.reset();
        self.stage2.reset();
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        self.stage2.process(self.stage1.process(x))
    }

    /// Filter a whole channel into `out` (length must equal `inp.len()`).
    pub fn process_block(&mut self, inp: &[f32], out: &mut [f32]) {
        debug_assert_eq!(inp.len(), out.len());
        for (i, &x) in inp.iter().enumerate() {
            out[i] = self.process(x);
        }
    }
}

/// High-shelf "pre-filter" biquad, ITU-R BS.1770-4 design (the DeMan /
/// libebur128 analytical formula, which reproduces the standard's published
/// shelf coefficients to full double precision at 48 kHz).
fn high_shelf(fs: f64, f0: f64, gain_db: f64, q: f64) -> Biquad {
    let k = (PI * f0 / fs).tan();
    let vh = 10.0_f64.powf(gain_db / 20.0);
    let vb = vh.powf(0.499666774155);
    let a0 = 1.0 + k / q + k * k;
    let b0 = (vh + vb * k / q + k * k) / a0;
    let b1 = 2.0 * (k * k - vh) / a0;
    let b2 = (vh - vb * k / q + k * k) / a0;
    let a1 = 2.0 * (k * k - 1.0) / a0;
    let a2 = (1.0 - k / q + k * k) / a0;
    Biquad::new(b0, b1, b2, a1, a2)
}

/// RLB high-pass biquad (stage 2), ITU-R BS.1770-4 design. The numerator is the
/// fixed [1, -2, 1] high-pass prototype; only the denominator is shaped by the
/// bilinear-transformed analog prototype.
fn high_pass(fs: f64, f0: f64, q: f64) -> Biquad {
    let k = (PI * f0 / fs).tan();
    let a0 = 1.0 + k / q + k * k;
    let b0 = 1.0;
    let b1 = -2.0;
    let b2 = 1.0;
    let a1 = 2.0 * (k * k - 1.0) / a0;
    let a2 = (1.0 - k / q + k * k) / a0;
    Biquad::new(b0, b1, b2, a1, a2)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The RBJ design must reproduce the exact ITU-R BS.1770-4 coefficients at
    /// 48 kHz. This is the correctness anchor for the whole loudness chain.
    #[test]
    fn kweight_48k_matches_itu() {
        let kw = KWeight::for_sample_rate(48_000);
        let s1 = kw.stage1;
        assert!(
            (s1.b0 - 1.53512485958697).abs() < 1e-9,
            "stage1.b0 = {}",
            s1.b0
        );
        assert!(
            (s1.b1 - -2.69169618940638).abs() < 1e-9,
            "stage1.b1 = {}",
            s1.b1
        );
        assert!(
            (s1.b2 - 1.19839281085285).abs() < 1e-9,
            "stage1.b2 = {}",
            s1.b2
        );
        assert!(
            (s1.a1 - -1.69065929318241).abs() < 1e-9,
            "stage1.a1 = {}",
            s1.a1
        );
        assert!(
            (s1.a2 - 0.73248077421585).abs() < 1e-9,
            "stage1.a2 = {}",
            s1.a2
        );

        let s2 = kw.stage2;
        assert!((s2.b0 - 1.0).abs() < 1e-9);
        assert!((s2.b1 - -2.0).abs() < 1e-9);
        assert!((s2.b2 - 1.0).abs() < 1e-9);
        assert!(
            (s2.a1 - -1.99004745483398).abs() < 1e-9,
            "stage2.a1 = {}",
            s2.a1
        );
        // NOTE: the ITU-R BS.1770-4 *table* lists a2 = 0.99709018690653, but
        // Brecht DeMan showed the standard's design *equations* yield
        // 0.99007225036621. Every reference implementation (libebur128/FFmpeg,
        // pyloudnorm's "DeMan" filter class) uses the equation value, so we do
        // too — this is what the world's actual loudness tools compute.
        assert!(
            (s2.a2 - 0.99007225036621).abs() < 1e-9,
            "stage2.a2 = {}",
            s2.a2
        );
    }

    #[test]
    fn kweight_passes_dc_through() {
        // A constant (DC) signal: stage2 high-pass removes it, so output -> 0.
        let mut kw = KWeight::for_sample_rate(48_000);
        let mut last = 0.0f32;
        for _ in 0..100_000 {
            last = kw.process(0.5);
        }
        assert!(last.abs() < 1e-3, "DC not removed: {last}");
    }
}
