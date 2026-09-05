//! SIMD-accelerated floating-point DSP primitives.
//!
//! Every public entry point performs **runtime** feature detection and falls
//! back to portable scalar code, so the produced binary runs on any CPU while
//! still exploiting AVX2 + FMA on capable x86-64 hosts. With `-C
//! target-cpu=native` the scalar fallbacks are themselves auto-vectorized by
//! LLVM (SSE2 baseline on x86-64), so even the "slow" path is fast.

#[cfg(target_arch = "aarch64")]
use std::arch::aarch64::*;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

/// Multiply every element of `buf` by `gain` in place. This is the single most
/// important operation in normalization and is fully vectorized.
#[inline]
pub fn apply_gain(buf: &mut [f32], gain: f32) {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma") {
            unsafe { apply_gain_avx2(buf, gain) };
            return;
        }
    }
    apply_gain_scalar(buf, gain)
}

#[inline]
fn apply_gain_scalar(buf: &mut [f32], gain: f32) {
    for x in buf.iter_mut() {
        *x *= gain;
    }
}

/// Multiply and hard-limit in one pass while preserving the established
/// `f32` operation order and exceptional-value behavior.
#[inline]
pub fn apply_gain_and_hard_clip(buf: &mut [f32], gain: f32, ceil: f32) {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            unsafe { apply_gain_and_hard_clip_avx2(buf, gain, ceil) };
            return;
        }
    }
    apply_gain_and_hard_clip_scalar(buf, gain, ceil)
}

#[inline]
fn apply_gain_and_hard_clip_scalar(buf: &mut [f32], gain: f32, ceil: f32) {
    for sample in buf {
        *sample = (*sample * gain).clamp(-ceil, ceil);
    }
}

/// Exact sum of `x*x` over the slice, accumulated in **f64** for numerical
/// stability across very long files. Used for LUFS energy summation.
#[inline]
pub fn sum_squares_f64(buf: &[f32]) -> f64 {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma") {
            return unsafe { sum_squares_avx2(buf) };
        }
    }
    sum_squares_scalar(buf)
}

#[inline]
fn sum_squares_scalar(buf: &[f32]) -> f64 {
    let mut acc = 0.0f64;
    for &x in buf {
        acc += (x as f64) * (x as f64);
    }
    acc
}

/// Maximum absolute value across the slice (sample-peak detection).
#[inline]
pub fn abs_max(buf: &[f32]) -> f32 {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            return unsafe { abs_max_avx2(buf) };
        }
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        abs_max_neon(buf)
    }
    #[cfg(not(target_arch = "aarch64"))]
    abs_max_scalar(buf)
}

/// Maximum absolute value plus an explicit NaN observation in one pass.
///
/// True-peak block pruning must reject a block containing NaN even though the
/// established sample-peak reduction ignores unordered comparisons. Keeping
/// both observations in one SIMD pass avoids rescanning long audio chunks.
#[inline]
pub(crate) fn abs_max_and_has_nan(buf: &[f32]) -> (f32, bool) {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            return unsafe { abs_max_and_has_nan_avx2(buf) };
        }
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        abs_max_and_has_nan_neon(buf)
    }
    #[cfg(not(target_arch = "aarch64"))]
    abs_max_and_has_nan_scalar(buf)
}

/// Maximum absolute finite value plus an all-samples-finite classification.
///
/// Streaming loudness analysis uses this as a transactional preflight: valid
/// chunks reuse the maximum for true-peak pruning, while exceptional chunks
/// fall back to the scalar validator to retain its exact diagnostic location.
#[inline]
pub(crate) fn abs_max_and_all_finite(buf: &[f32]) -> (f32, bool) {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            return unsafe { abs_max_and_all_finite_avx2(buf) };
        }
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        abs_max_and_all_finite_neon(buf)
    }
    #[cfg(not(target_arch = "aarch64"))]
    abs_max_and_all_finite_scalar(buf)
}

#[inline]
fn abs_max_scalar(buf: &[f32]) -> f32 {
    let mut m = 0.0f32;
    for &x in buf {
        let a = x.abs();
        if a > m {
            m = a;
        }
    }
    m
}

#[inline]
fn abs_max_and_has_nan_scalar(buf: &[f32]) -> (f32, bool) {
    let mut maximum = 0.0_f32;
    let mut has_nan = false;
    for &sample in buf {
        if sample.is_nan() {
            has_nan = true;
        } else {
            maximum = maximum.max(sample.abs());
        }
    }
    (maximum, has_nan)
}

#[inline]
fn abs_max_and_all_finite_scalar(buf: &[f32]) -> (f32, bool) {
    let mut maximum = 0.0_f32;
    let mut all_finite = true;
    for &sample in buf {
        if sample.is_finite() {
            maximum = maximum.max(sample.abs());
        } else {
            all_finite = false;
        }
    }
    (maximum, all_finite)
}

/// Hard-limit (brick-wall clip) every sample to `[-ceil, ceil]`. Used only as a
/// final safety net; the primary loudness math targets a true-peak ceiling via
/// global gain so this should never engage in practice.
#[inline]
pub fn hard_clip(buf: &mut [f32], ceil: f32) {
    for x in buf.iter_mut() {
        *x = x.clamp(-ceil, ceil);
    }
}

// ---------------------------------------------------------------------------
// AVX2 + FMA implementations
// ---------------------------------------------------------------------------

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn apply_gain_avx2(buf: &mut [f32], gain: f32) {
    let n = buf.len();
    let g = _mm256_set1_ps(gain);
    let mut i = 0;
    while i + 8 <= n {
        let x = _mm256_loadu_ps(buf.as_ptr().add(i));
        _mm256_storeu_ps(buf.as_mut_ptr().add(i), _mm256_mul_ps(x, g));
        i += 8;
    }
    for x in buf[i..].iter_mut() {
        *x *= gain;
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn apply_gain_and_hard_clip_avx2(buf: &mut [f32], gain: f32, ceil: f32) {
    let n = buf.len();
    let gain_vector = _mm256_set1_ps(gain);
    let lower = _mm256_set1_ps(-ceil);
    let upper = _mm256_set1_ps(ceil);
    let mut i = 0;
    while i + 8 <= n {
        let samples = _mm256_loadu_ps(buf.as_ptr().add(i));
        let gained = _mm256_mul_ps(samples, gain_vector);
        // Put the gained sample in the second operand. MAXPS/MINPS select that
        // operand for unordered inputs, preserving NaNs for the quantizer's
        // established NaN-to-silence handling.
        let protected = _mm256_min_ps(upper, _mm256_max_ps(lower, gained));
        _mm256_storeu_ps(buf.as_mut_ptr().add(i), protected);
        i += 8;
    }
    apply_gain_and_hard_clip_scalar(&mut buf[i..], gain, ceil);
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn sum_squares_avx2(buf: &[f32]) -> f64 {
    let n = buf.len();
    let mut acc_lo = _mm256_setzero_pd();
    let mut acc_hi = _mm256_setzero_pd();
    let mut i = 0;
    let p = buf.as_ptr();
    while i + 8 <= n {
        let v = _mm256_loadu_ps(p.add(i));
        let lo = _mm256_cvtps_pd(_mm256_extractf128_ps(v, 0));
        let hi = _mm256_cvtps_pd(_mm256_extractf128_ps(v, 1));
        acc_lo = _mm256_fmadd_pd(lo, lo, acc_lo);
        acc_hi = _mm256_fmadd_pd(hi, hi, acc_hi);
        i += 8;
    }
    let total = _mm256_add_pd(acc_lo, acc_hi);
    // scalar tail
    let mut s = 0.0f64;
    for &x in buf[i..].iter() {
        s += (x as f64) * (x as f64);
    }
    // horizontal sum of the four f64 lanes in `total`.
    let hi128 = _mm256_extractf128_pd(total, 1);
    let lo128 = _mm256_castpd256_pd128(total);
    let s2 = _mm_add_pd(lo128, hi128);
    let shuf = _mm_shuffle_pd(s2, s2, 0b01);
    let lane = _mm_add_sd(s2, shuf);
    s + _mm_cvtsd_f64(lane)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn abs_max_avx2(buf: &[f32]) -> f32 {
    let n = buf.len();
    let mut vmax = _mm256_setzero_ps();
    let mut i = 0;
    let p = buf.as_ptr();
    while i + 8 <= n {
        let v = _mm256_loadu_ps(p.add(i));
        vmax = _mm256_max_ps(vmax, _mm256_andnot_ps(_mm256_set1_ps(-0.0), v));
        i += 8;
    }
    // horizontal max across 8 lanes
    let mut m = {
        let hi = _mm256_extractf128_ps(vmax, 1);
        let lo = _mm256_castps256_ps128(vmax);
        let s = _mm_max_ps(lo, hi);
        let shuf = _mm_shuffle_ps(s, s, 0b01_00_11_10);
        let s2 = _mm_max_ps(s, shuf);
        let shuf2 = _mm_shuffle_ps(s2, s2, 0b00_01_10_11); // move hi lane to lo
        _mm_cvtss_f32(_mm_max_ps(s2, shuf2))
    };
    for &x in buf[i..].iter() {
        let a = x.abs();
        if a > m {
            m = a;
        }
    }
    m
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn abs_max_and_has_nan_avx2(buf: &[f32]) -> (f32, bool) {
    let n = buf.len();
    let zero = _mm256_setzero_ps();
    let sign = _mm256_set1_ps(-0.0);
    let mut maximum = zero;
    let mut nan_mask = 0_i32;
    let mut index = 0;
    let pointer = buf.as_ptr();
    while index + 8 <= n {
        let samples = _mm256_loadu_ps(pointer.add(index));
        let unordered = _mm256_cmp_ps(samples, samples, _CMP_UNORD_Q);
        nan_mask |= _mm256_movemask_ps(unordered);
        let absolute = _mm256_andnot_ps(sign, samples);
        let finite_absolute = _mm256_blendv_ps(absolute, zero, unordered);
        maximum = _mm256_max_ps(maximum, finite_absolute);
        index += 8;
    }

    let high = _mm256_extractf128_ps(maximum, 1);
    let low = _mm256_castps256_ps128(maximum);
    let lanes = _mm_max_ps(low, high);
    let shuffled = _mm_shuffle_ps(lanes, lanes, 0b01_00_11_10);
    let pairs = _mm_max_ps(lanes, shuffled);
    let shuffled_pairs = _mm_shuffle_ps(pairs, pairs, 0b00_01_10_11);
    let mut scalar_maximum = _mm_cvtss_f32(_mm_max_ps(pairs, shuffled_pairs));
    let mut has_nan = nan_mask != 0;
    for &sample in &buf[index..] {
        if sample.is_nan() {
            has_nan = true;
        } else {
            scalar_maximum = scalar_maximum.max(sample.abs());
        }
    }
    (scalar_maximum, has_nan)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn abs_max_and_all_finite_avx2(buf: &[f32]) -> (f32, bool) {
    let n = buf.len();
    let zero = _mm256_setzero_ps();
    let sign = _mm256_set1_ps(-0.0);
    let infinity = _mm256_set1_ps(f32::INFINITY);
    let mut maximum = zero;
    let mut finite_lanes = 0xff_i32;
    let mut index = 0;
    let pointer = buf.as_ptr();
    while index + 8 <= n {
        let samples = _mm256_loadu_ps(pointer.add(index));
        let absolute = _mm256_andnot_ps(sign, samples);
        let finite = _mm256_cmp_ps(absolute, infinity, _CMP_LT_OQ);
        finite_lanes &= _mm256_movemask_ps(finite);
        maximum = _mm256_max_ps(maximum, _mm256_and_ps(absolute, finite));
        index += 8;
    }

    let high = _mm256_extractf128_ps(maximum, 1);
    let low = _mm256_castps256_ps128(maximum);
    let lanes = _mm_max_ps(low, high);
    let shuffled = _mm_shuffle_ps(lanes, lanes, 0b01_00_11_10);
    let pairs = _mm_max_ps(lanes, shuffled);
    let shuffled_pairs = _mm_shuffle_ps(pairs, pairs, 0b00_01_10_11);
    let mut scalar_maximum = _mm_cvtss_f32(_mm_max_ps(pairs, shuffled_pairs));
    let mut all_finite = finite_lanes == 0xff;
    for &sample in &buf[index..] {
        if sample.is_finite() {
            scalar_maximum = scalar_maximum.max(sample.abs());
        } else {
            all_finite = false;
        }
    }
    (scalar_maximum, all_finite)
}

// ---------------------------------------------------------------------------
// AArch64 Advanced SIMD implementations
// ---------------------------------------------------------------------------

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn abs_max_neon(buf: &[f32]) -> f32 {
    let zero = vdupq_n_f32(0.0);
    let mut maximum = zero;
    let mut index = 0;
    while index + 16 <= buf.len() {
        for offset in [0, 4, 8, 12] {
            let samples = vld1q_f32(buf.as_ptr().add(index + offset));
            let ordered = vceqq_f32(samples, samples);
            let finite_absolute = vbslq_f32(ordered, vabsq_f32(samples), zero);
            maximum = vmaxq_f32(maximum, finite_absolute);
        }
        index += 16;
    }
    while index + 4 <= buf.len() {
        let samples = vld1q_f32(buf.as_ptr().add(index));
        let ordered = vceqq_f32(samples, samples);
        let finite_absolute = vbslq_f32(ordered, vabsq_f32(samples), zero);
        maximum = vmaxq_f32(maximum, finite_absolute);
        index += 4;
    }
    let mut scalar_maximum = vmaxvq_f32(maximum);
    for &sample in &buf[index..] {
        let absolute = sample.abs();
        if absolute > scalar_maximum {
            scalar_maximum = absolute;
        }
    }
    scalar_maximum
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn abs_max_and_has_nan_neon(buf: &[f32]) -> (f32, bool) {
    let zero = vdupq_n_f32(0.0);
    let mut maximum = zero;
    let mut unordered = vdupq_n_u32(0);
    let mut index = 0;
    while index + 16 <= buf.len() {
        for offset in [0, 4, 8, 12] {
            let samples = vld1q_f32(buf.as_ptr().add(index + offset));
            let ordered = vceqq_f32(samples, samples);
            unordered = vorrq_u32(unordered, vmvnq_u32(ordered));
            let finite_absolute = vbslq_f32(ordered, vabsq_f32(samples), zero);
            maximum = vmaxq_f32(maximum, finite_absolute);
        }
        index += 16;
    }
    while index + 4 <= buf.len() {
        let samples = vld1q_f32(buf.as_ptr().add(index));
        let ordered = vceqq_f32(samples, samples);
        unordered = vorrq_u32(unordered, vmvnq_u32(ordered));
        let finite_absolute = vbslq_f32(ordered, vabsq_f32(samples), zero);
        maximum = vmaxq_f32(maximum, finite_absolute);
        index += 4;
    }
    let mut scalar_maximum = vmaxvq_f32(maximum);
    let mut has_nan = vmaxvq_u32(unordered) != 0;
    for &sample in &buf[index..] {
        if sample.is_nan() {
            has_nan = true;
        } else {
            scalar_maximum = scalar_maximum.max(sample.abs());
        }
    }
    (scalar_maximum, has_nan)
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn abs_max_and_all_finite_neon(buf: &[f32]) -> (f32, bool) {
    let zero = vdupq_n_f32(0.0);
    let infinity = vdupq_n_f32(f32::INFINITY);
    let mut maximum = zero;
    let mut non_finite = vdupq_n_u32(0);
    let mut index = 0;
    while index + 16 <= buf.len() {
        for offset in [0, 4, 8, 12] {
            let absolute = vabsq_f32(vld1q_f32(buf.as_ptr().add(index + offset)));
            let finite = vcltq_f32(absolute, infinity);
            non_finite = vorrq_u32(non_finite, vmvnq_u32(finite));
            maximum = vmaxq_f32(maximum, vbslq_f32(finite, absolute, zero));
        }
        index += 16;
    }
    while index + 4 <= buf.len() {
        let absolute = vabsq_f32(vld1q_f32(buf.as_ptr().add(index)));
        let finite = vcltq_f32(absolute, infinity);
        non_finite = vorrq_u32(non_finite, vmvnq_u32(finite));
        maximum = vmaxq_f32(maximum, vbslq_f32(finite, absolute, zero));
        index += 4;
    }
    let mut scalar_maximum = vmaxvq_f32(maximum);
    let mut all_finite = vmaxvq_u32(non_finite) == 0;
    for &sample in &buf[index..] {
        if sample.is_finite() {
            scalar_maximum = scalar_maximum.max(sample.abs());
        } else {
            all_finite = false;
        }
    }
    (scalar_maximum, all_finite)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combined_gain_and_clip_matches_separate_passes_bit_for_bit() {
        let mut expected = vec![
            -f32::INFINITY,
            -2.0,
            -1.0,
            -0.0,
            0.0,
            0.125,
            0.75,
            1.0,
            2.0,
            f32::INFINITY,
            f32::from_bits(0x7fc0_1234),
            -0.333_333_34,
            0.333_333_34,
            -f32::MIN_POSITIVE,
            f32::MIN_POSITIVE,
            42.0,
            -42.0,
        ];
        let mut actual = expected.clone();
        let gain = 0.812_345_7_f32;
        let ceiling = 0.891_250_9_f32;

        apply_gain(&mut expected, gain);
        hard_clip(&mut expected, ceiling);
        apply_gain_and_hard_clip(&mut actual, gain, ceiling);

        assert_eq!(
            actual
                .iter()
                .map(|sample| sample.to_bits())
                .collect::<Vec<_>>(),
            expected
                .iter()
                .map(|sample| sample.to_bits())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn combined_absolute_maximum_reports_nan_without_losing_finite_peak() {
        let samples = [
            f32::NAN,
            -0.25,
            0.75,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::from_bits(1),
            -1.5,
            f32::NAN,
            0.5,
        ];
        let (maximum, has_nan) = abs_max_and_has_nan(&samples);
        assert!(maximum.is_infinite());
        assert!(has_nan);

        let (maximum, has_nan) = abs_max_and_has_nan(&[-0.25, 0.75, -1.5, 0.5]);
        assert_eq!(maximum.to_bits(), 1.5_f32.to_bits());
        assert!(!has_nan);

        let (maximum, all_finite) = abs_max_and_all_finite(&samples);
        assert_eq!(maximum.to_bits(), 1.5_f32.to_bits());
        assert!(!all_finite);

        let (maximum, all_finite) = abs_max_and_all_finite(&[-0.25, 0.75, -1.5, 0.5]);
        assert_eq!(maximum.to_bits(), 1.5_f32.to_bits());
        assert!(all_finite);
    }

    #[test]
    fn combined_absolute_maximum_matches_scalar_across_vector_lanes() {
        for peak_index in 0..24 {
            let mut samples = (0..24)
                .map(|index| (index as f32 - 12.0) / 16.0)
                .collect::<Vec<_>>();
            let nan_index = (peak_index + 7) % samples.len();
            samples[nan_index] = f32::NAN;
            samples[peak_index] = if peak_index.is_multiple_of(2) {
                3.25
            } else {
                -3.25
            };
            assert_eq!(
                abs_max_and_has_nan(&samples),
                abs_max_and_has_nan_scalar(&samples),
                "peak lane {peak_index}"
            );
            assert_eq!(
                abs_max_and_all_finite(&samples),
                abs_max_and_all_finite_scalar(&samples),
                "finite peak lane {peak_index}"
            );
        }

        let mut state = 0x6a09_e667_f3bc_c909_u64;
        let samples = (0..4099)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                f32::from_bits((state >> 32) as u32)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            abs_max_and_has_nan(&samples),
            abs_max_and_has_nan_scalar(&samples)
        );
        assert_eq!(
            abs_max_and_all_finite(&samples),
            abs_max_and_all_finite_scalar(&samples)
        );
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn neon_peak_primitives_match_scalar() {
        let mut samples = vec![
            -f32::INFINITY,
            -2.0,
            -1.0,
            -0.0,
            0.0,
            f32::from_bits(1),
            -f32::from_bits(1),
            0.125,
            0.75,
            1.0,
            2.0,
            f32::INFINITY,
            f32::from_bits(0x7fc0_1234),
        ];
        let mut state = 0x510e_527f_ade6_82d1_u64;
        for _ in 0..4099 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            samples.push(f32::from_bits((state >> 32) as u32));
        }

        for length in [0, 1, 3, 4, 5, 31, 32, 33, 1027, samples.len()] {
            let input = &samples[..length];
            assert_eq!(
                abs_max(input).to_bits(),
                abs_max_scalar(input).to_bits(),
                "peak len={length}"
            );
            assert_eq!(
                abs_max_and_has_nan(input),
                abs_max_and_has_nan_scalar(input),
                "peak+nan len={length}"
            );
            assert_eq!(
                abs_max_and_all_finite(input),
                abs_max_and_all_finite_scalar(input),
                "peak+finite len={length}"
            );
        }
    }
}
