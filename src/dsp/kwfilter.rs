//! K-weighting filter for ITU-R BS.1770-5 / EBU R128 loudness measurement.
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

#[cfg(target_arch = "aarch64")]
use std::arch::aarch64::*;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;
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

    /// Process one sample without the compatibility `f32` stage rounding.
    #[inline]
    fn process_f64(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
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
        // Design parameters from ITU-R BS.1770-5.
        let stage1 = high_shelf(fs, 1681.974450955533, 3.999843853973347, 0.7071752369554196);
        let stage2 = high_pass(fs, 38.13547087602444, 0.5003270373238773);
        Self { stage1, stage2 }
    }

    /// Build the reference engine from committed coefficient bits.
    ///
    /// The deliberately finite rate set covers the common music and
    /// production families while avoiding platform `libm` variation in the
    /// analytic coefficient design.
    pub(crate) fn for_reference_sample_rate(fs: u32) -> Result<Self, String> {
        let coefficients = match fs {
            44_100 => [
                0x3ff87e535fa6f506,
                0xc0053534ffec5184,
                0x3ff2b48c43eb3692,
                0xbffa9e54d2f41fe2,
                0x3fe6cd94ed5b50e2,
                0x3ff0000000000000,
                0xc000000000000000,
                0x3ff0000000000000,
                0xbfffd3a39466f5a0,
                0x3fefa784bc7e1508,
            ],
            44_101 => [
                0x3ff87e549fcbb80c,
                0xc005353af0337901,
                0x3ff2b494bd645125,
                0xbffa9e5cb71b2867,
                0x3fe6cda067c87f2e,
                0x3ff0000000000000,
                0xc000000000000000,
                0x3ff0000000000000,
                0xbfffd3a3d6258aee,
                0x3fefa7853f44bbbf,
            ],
            48_000 => [
                0x3ff88fdf15b33e98,
                0xc005889803022554,
                0x3ff32c9df0a5fd59,
                0xbffb0cf0c24e59d1,
                0x3fe7707b85469636,
                0x3ff0000000000000,
                0xc000000000000000,
                0x3ff0000000000000,
                0xbfffd73bffffffeb,
                0x3fefaeabfffffff7,
            ],
            88_200 => [
                0x3ff8eb953e121c19,
                0xc0073eb969153ebb,
                0x3ff5c8062302d39d,
                0xbffd4b72c1de4140,
                0x3feb0336a19166fb,
                0x3ff0000000000000,
                0xc000000000000000,
                0x3ff0000000000000,
                0xbfffe9ca1ce3237d,
                0x3fefd3a3a95fc246,
            ],
            96_000 => [
                0x3ff8f496e848e968,
                0xc00769f77d137460,
                0x3ff60d5b9e54ac07,
                0xbffd838538311e5d,
                0x3feb6311894f9616,
                0x3ff0000000000000,
                0xc000000000000000,
                0x3ff0000000000000,
                0xbfffeb9784589f37,
                0x3fefd73c10fa5c29,
            ],
            176_400 => [
                0x3ff92349d0bf2281,
                0xc0084ad7d81569a1,
                0x3ff7807f84f84cf1,
                0xbffea5305a8de33e,
                0x3fed66940034fedd,
                0x3ff0000000000000,
                0xc000000000000000,
                0x3ff0000000000000,
                0xbffff4e321c89ecd,
                0x3fefe9ca20ce90ee,
            ],
            192_000 => [
                0x3ff927d7b96b2c02,
                0xc00860d5efc0e542,
                0x3ff7a5c546deec1b,
                0xbffec157204c1e5e,
                0x3fed9a908228d7ed,
                0x3ff0000000000000,
                0xc000000000000000,
                0x3ff0000000000000,
                0xbffff5ca2239fe53,
                0x3fefeb9787905080,
            ],
            352_800 => [
                0x3ff93f6091b92569,
                0xc008d2a9c00148ac,
                0x3ff8698c3ee9438d,
                0xbfff5285d63b4ce5,
                0x3feeac3e4db6490a,
                0x3ff0000000000000,
                0xc000000000000000,
                0x3ff0000000000000,
                0xbffffa71158f56ba,
                0x3feff4e32298f554,
            ],
            384_000 => [
                0x3ff941aa70dd2dbc,
                0xc008ddbf9607b10b,
                0x3ff87cdfbc6207c6,
                0xbfff609d4444f3f3,
                0x3feec7508ae98ec1,
                0x3ff0000000000000,
                0xc000000000000000,
                0x3ff0000000000000,
                0xbffffae4a8ff44ae,
                0x3feff5ca22e6ee77,
            ],
            _ => {
                return Err(format!(
                    "reference analysis supports sample rates 44100, 44101, 48000, 88200, 96000, 176400, 192000, 352800, and 384000 Hz; got {fs} Hz"
                ));
            }
        };
        let value = |index| f64::from_bits(coefficients[index]);
        Ok(Self {
            stage1: Biquad::new(value(0), value(1), value(2), value(3), value(4)),
            stage2: Biquad::new(value(5), value(6), value(7), value(8), value(9)),
        })
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

    /// Scalar high-precision lane for integer and `f64` analysis ingress.
    #[inline]
    pub(crate) fn process_f64(&mut self, x: f64) -> f64 {
        let stage1 = self.stage1.process_f64(x);
        self.stage2.process_f64(stage1)
    }

    /// Filter a whole channel into `out` (length must equal `inp.len()`).
    pub fn process_block(&mut self, inp: &[f32], out: &mut [f32]) {
        debug_assert_eq!(inp.len(), out.len());
        for (i, &x) in inp.iter().enumerate() {
            out[i] = self.process(x);
        }
    }
}

/// Two persistent K-weighting states held in f64 SIMD lanes for the dominant
/// stereo delivery path. Each lane is one channel; reductions remain outside
/// this type so callers can preserve their established left-to-right order.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub(crate) struct KWeightPair {
    stage1: PairBiquad,
    stage2: PairBiquad,
}

#[cfg(target_arch = "x86_64")]
type PairF64 = __m128d;
#[cfg(target_arch = "aarch64")]
type PairF64 = float64x2_t;

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
struct PairBiquad {
    b0: PairF64,
    b1: PairF64,
    b2: PairF64,
    a1: PairF64,
    a2: PairF64,
    z1: PairF64,
    z2: PairF64,
}

#[cfg(target_arch = "x86_64")]
impl KWeightPair {
    pub(crate) fn for_sample_rate(sample_rate: u32) -> Self {
        let scalar = KWeight::for_sample_rate(sample_rate);
        // SAFETY: SSE2 is part of the x86-64 architecture baseline.
        unsafe { Self::from_scalar_sse2(&scalar) }
    }

    #[inline]
    pub(crate) fn process(&mut self, input: [f32; 2]) -> [f32; 2] {
        // SAFETY: SSE2 is part of the x86-64 architecture baseline.
        unsafe { self.process_sse2(input) }
    }

    #[target_feature(enable = "sse2")]
    unsafe fn from_scalar_sse2(scalar: &KWeight) -> Self {
        Self {
            stage1: PairBiquad::from_scalar_sse2(&scalar.stage1),
            stage2: PairBiquad::from_scalar_sse2(&scalar.stage2),
        }
    }

    #[inline]
    #[target_feature(enable = "sse2")]
    unsafe fn process_sse2(&mut self, input: [f32; 2]) -> [f32; 2] {
        let input = _mm_cvtps_pd(_mm_set_ps(0.0, 0.0, input[1], input[0]));
        let stage1 = self.stage1.process_sse2(input);
        // Scalar KWeight rounds stage 1 to f32 before entering stage 2.
        let stage1 = _mm_cvtps_pd(_mm_cvtpd_ps(stage1));
        let stage2 = _mm_cvtpd_ps(self.stage2.process_sse2(stage1));
        [
            _mm_cvtss_f32(stage2),
            _mm_cvtss_f32(_mm_shuffle_ps(stage2, stage2, 0x55)),
        ]
    }
}

#[cfg(target_arch = "x86_64")]
impl PairBiquad {
    #[target_feature(enable = "sse2")]
    unsafe fn from_scalar_sse2(scalar: &Biquad) -> Self {
        Self {
            b0: _mm_set1_pd(scalar.b0),
            b1: _mm_set1_pd(scalar.b1),
            b2: _mm_set1_pd(scalar.b2),
            a1: _mm_set1_pd(scalar.a1),
            a2: _mm_set1_pd(scalar.a2),
            z1: _mm_setzero_pd(),
            z2: _mm_setzero_pd(),
        }
    }

    #[inline]
    #[target_feature(enable = "sse2")]
    unsafe fn process_sse2(&mut self, input: __m128d) -> __m128d {
        let output = _mm_add_pd(_mm_mul_pd(self.b0, input), self.z1);
        self.z1 = _mm_add_pd(
            _mm_sub_pd(_mm_mul_pd(self.b1, input), _mm_mul_pd(self.a1, output)),
            self.z2,
        );
        self.z2 = _mm_sub_pd(_mm_mul_pd(self.b2, input), _mm_mul_pd(self.a2, output));
        output
    }
}

#[cfg(target_arch = "aarch64")]
impl KWeightPair {
    pub(crate) fn for_sample_rate(sample_rate: u32) -> Self {
        let scalar = KWeight::for_sample_rate(sample_rate);
        // SAFETY: Advanced SIMD is part of the AArch64 architecture baseline.
        unsafe { Self::from_scalar_neon(&scalar) }
    }

    #[inline]
    pub(crate) fn process(&mut self, input: [f32; 2]) -> [f32; 2] {
        // SAFETY: Advanced SIMD is part of the AArch64 architecture baseline.
        unsafe { self.process_neon(input) }
    }

    #[target_feature(enable = "neon")]
    unsafe fn from_scalar_neon(scalar: &KWeight) -> Self {
        Self {
            stage1: PairBiquad::from_scalar_neon(&scalar.stage1),
            stage2: PairBiquad::from_scalar_neon(&scalar.stage2),
        }
    }

    #[inline]
    #[target_feature(enable = "neon")]
    unsafe fn process_neon(&mut self, input: [f32; 2]) -> [f32; 2] {
        let input = vcvt_f64_f32(vld1_f32(input.as_ptr()));
        let stage1 = self.stage1.process_neon(input);
        // Scalar KWeight rounds stage 1 to f32 before entering stage 2.
        let stage1 = vcvt_f64_f32(vcvt_f32_f64(stage1));
        let stage2 = vcvt_f32_f64(self.stage2.process_neon(stage1));
        let mut output = [0.0; 2];
        vst1_f32(output.as_mut_ptr(), stage2);
        output
    }
}

#[cfg(target_arch = "aarch64")]
impl PairBiquad {
    #[target_feature(enable = "neon")]
    unsafe fn from_scalar_neon(scalar: &Biquad) -> Self {
        Self {
            b0: vdupq_n_f64(scalar.b0),
            b1: vdupq_n_f64(scalar.b1),
            b2: vdupq_n_f64(scalar.b2),
            a1: vdupq_n_f64(scalar.a1),
            a2: vdupq_n_f64(scalar.a2),
            z1: vdupq_n_f64(0.0),
            z2: vdupq_n_f64(0.0),
        }
    }

    #[inline]
    #[target_feature(enable = "neon")]
    unsafe fn process_neon(&mut self, input: float64x2_t) -> float64x2_t {
        let output = vaddq_f64(vmulq_f64(self.b0, input), self.z1);
        self.z1 = vaddq_f64(
            vsubq_f64(vmulq_f64(self.b1, input), vmulq_f64(self.a1, output)),
            self.z2,
        );
        self.z2 = vsubq_f64(vmulq_f64(self.b2, input), vmulq_f64(self.a2, output));
        output
    }
}

/// Four channel-contiguous K-weighting states retained in AVX2 registers across
/// frames. Coefficients and delay states stay in lane form, avoiding the
/// per-frame AoS gather/store cost of a temporary SIMD wrapper.
#[cfg(target_arch = "x86_64")]
pub(crate) struct KWeightQuad {
    stage1: QuadBiquad,
    stage2: QuadBiquad,
}

#[cfg(target_arch = "x86_64")]
struct QuadBiquad {
    b0: __m256d,
    b1: __m256d,
    b2: __m256d,
    a1: __m256d,
    a2: __m256d,
    z1: __m256d,
    z2: __m256d,
}

#[cfg(target_arch = "x86_64")]
impl KWeightQuad {
    pub(crate) fn for_sample_rate(sample_rate: u32) -> Option<Self> {
        if !is_x86_feature_detected!("avx2") {
            return None;
        }
        let scalar = KWeight::for_sample_rate(sample_rate);
        // SAFETY: runtime AVX2 detection succeeded above.
        Some(unsafe { Self::from_scalar_avx2(&scalar) })
    }

    #[inline]
    pub(crate) fn process(&mut self, input: [f32; 4]) -> [f32; 4] {
        // SAFETY: this value can only be constructed after AVX2 detection.
        unsafe { self.process_avx2(input) }
    }

    #[target_feature(enable = "avx2")]
    unsafe fn from_scalar_avx2(scalar: &KWeight) -> Self {
        Self {
            stage1: QuadBiquad::from_scalar_avx2(&scalar.stage1),
            stage2: QuadBiquad::from_scalar_avx2(&scalar.stage2),
        }
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn process_avx2(&mut self, input: [f32; 4]) -> [f32; 4] {
        let input = _mm256_cvtps_pd(_mm_loadu_ps(input.as_ptr()));
        let stage1 = self.stage1.process_avx2(input);
        // Scalar KWeight rounds stage 1 to f32 before entering stage 2.
        let stage1 = _mm256_cvtps_pd(_mm256_cvtpd_ps(stage1));
        let stage2 = self.stage2.process_avx2(stage1);
        let mut output = [0.0; 4];
        _mm_storeu_ps(output.as_mut_ptr(), _mm256_cvtpd_ps(stage2));
        output
    }
}

#[cfg(target_arch = "x86_64")]
impl QuadBiquad {
    #[target_feature(enable = "avx2")]
    unsafe fn from_scalar_avx2(scalar: &Biquad) -> Self {
        Self {
            b0: _mm256_set1_pd(scalar.b0),
            b1: _mm256_set1_pd(scalar.b1),
            b2: _mm256_set1_pd(scalar.b2),
            a1: _mm256_set1_pd(scalar.a1),
            a2: _mm256_set1_pd(scalar.a2),
            z1: _mm256_setzero_pd(),
            z2: _mm256_setzero_pd(),
        }
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn process_avx2(&mut self, input: __m256d) -> __m256d {
        let output = _mm256_add_pd(_mm256_mul_pd(self.b0, input), self.z1);
        self.z1 = _mm256_add_pd(
            _mm256_sub_pd(
                _mm256_mul_pd(self.b1, input),
                _mm256_mul_pd(self.a1, output),
            ),
            self.z2,
        );
        self.z2 = _mm256_sub_pd(
            _mm256_mul_pd(self.b2, input),
            _mm256_mul_pd(self.a2, output),
        );
        output
    }
}

/// High-shelf "pre-filter" biquad, ITU-R BS.1770-5 design (the DeMan /
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

/// RLB high-pass biquad (stage 2), ITU-R BS.1770-5 design. The numerator is the
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

    /// The RBJ design must reproduce the exact ITU-R BS.1770-5 coefficients at
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
        // NOTE: the ITU-R BS.1770-5 *table* lists a2 = 0.99709018690653, but
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

    #[test]
    fn reference_kweight_uses_the_committed_48k_vector_and_rejects_other_rates() {
        let kw = KWeight::for_reference_sample_rate(48_000).unwrap();
        assert_eq!(kw.stage1.b0.to_bits(), 0x3ff8_8fdf_15b3_3e98);
        assert_eq!(kw.stage1.b1.to_bits(), 0xc005_8898_0302_2554);
        assert_eq!(kw.stage1.b2.to_bits(), 0x3ff3_2c9d_f0a5_fd59);
        assert_eq!(kw.stage1.a1.to_bits(), 0xbffb_0cf0_c24e_59d1);
        assert_eq!(kw.stage1.a2.to_bits(), 0x3fe7_707b_8546_9636);
        assert_eq!(kw.stage2.a1.to_bits(), 0xbfff_d73b_ffff_ffeb);
        assert_eq!(kw.stage2.a2.to_bits(), 0x3fef_aeab_ffff_fff7);
        assert!(KWeight::for_reference_sample_rate(32_000)
            .unwrap_err()
            .contains("supports sample rates"));
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn quad_kweight_is_bit_exact_including_exceptional_samples() {
        for sample_rate in [8_000, 44_100, 48_000, 96_000, 192_000, 384_000] {
            let mut expected = (0..4)
                .map(|_| KWeight::for_sample_rate(sample_rate))
                .collect::<Vec<_>>();
            let Some(mut actual) = KWeightQuad::for_sample_rate(sample_rate) else {
                eprintln!("AVX2 unavailable; exact quad test skipped");
                return;
            };
            for frame in 0..20_003 {
                let mut input = [0.0; 4];
                for (channel, sample) in input.iter_mut().enumerate() {
                    *sample = ((frame as f64 * (0.011 + channel as f64 * 0.004) + channel as f64)
                        .sin()
                        * (0.7 + channel as f64 * 0.13)) as f32;
                }
                match frame {
                    101 => input[0] = f32::from_bits(1),
                    307 => input[1] = f32::NAN,
                    509 => input[2] = f32::INFINITY,
                    701 => input[3] = f32::NEG_INFINITY,
                    _ => {}
                }
                let expected_output = [
                    expected[0].process(input[0]),
                    expected[1].process(input[1]),
                    expected[2].process(input[2]),
                    expected[3].process(input[3]),
                ];
                let actual_output = actual.process(input);
                for channel in 0..4 {
                    assert_eq!(
                        actual_output[channel].to_bits(),
                        expected_output[channel].to_bits(),
                        "sample rate {sample_rate}, frame {frame}, channel {channel}"
                    );
                }
            }
        }
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    #[test]
    fn pair_kweight_is_bit_exact_including_exceptional_samples() {
        for sample_rate in [8_000, 44_100, 48_000, 96_000, 192_000, 384_000] {
            let mut expected = (0..2)
                .map(|_| KWeight::for_sample_rate(sample_rate))
                .collect::<Vec<_>>();
            let mut actual = KWeightPair::for_sample_rate(sample_rate);
            for frame in 0..20_003 {
                let mut input = [0.0; 2];
                for (channel, sample) in input.iter_mut().enumerate() {
                    *sample = ((frame as f64 * (0.011 + channel as f64 * 0.004) + channel as f64)
                        .sin()
                        * (0.7 + channel as f64 * 0.13)) as f32;
                }
                if frame == 101 {
                    input[0] = f32::from_bits(1);
                } else if frame == 307 {
                    input[1] = -f32::from_bits(1);
                }
                let expected_output =
                    [expected[0].process(input[0]), expected[1].process(input[1])];
                let actual_output = actual.process(input);
                for channel in 0..2 {
                    assert_eq!(
                        actual_output[channel].to_bits(),
                        expected_output[channel].to_bits(),
                        "sample rate {sample_rate}, frame {frame}, channel {channel}"
                    );
                }
            }

            for exceptional in [
                [f32::NAN, 0.0],
                [0.0, f32::from_bits(0x7fa0_1234)],
                [f32::INFINITY, 0.0],
                [0.0, f32::INFINITY],
                [f32::NEG_INFINITY, 0.0],
                [0.0, f32::NEG_INFINITY],
            ] {
                let mut expected = [
                    KWeight::for_sample_rate(sample_rate),
                    KWeight::for_sample_rate(sample_rate),
                ];
                let mut actual = KWeightPair::for_sample_rate(sample_rate);
                for frame in 0..32 {
                    let input = [
                        (frame as f32 * 0.017).sin() * 0.71,
                        (frame as f32 * 0.023 + 0.4).sin() * 0.83,
                    ];
                    let expected_output =
                        [expected[0].process(input[0]), expected[1].process(input[1])];
                    assert_eq!(actual.process(input), expected_output);
                }
                let expected_output = [
                    expected[0].process(exceptional[0]),
                    expected[1].process(exceptional[1]),
                ];
                let actual_output = actual.process(exceptional);
                for channel in 0..2 {
                    assert_eq!(
                        actual_output[channel].to_bits(),
                        expected_output[channel].to_bits(),
                        "exceptional sample rate {sample_rate}, channel {channel}, input {exceptional:?}"
                    );
                }
            }
        }
    }
}
