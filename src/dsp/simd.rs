//! SIMD-accelerated floating-point DSP primitives.
//!
//! Every public entry point performs **runtime** feature detection and falls
//! back to portable scalar code, so the produced binary runs on any CPU while
//! still exploiting AVX2 + FMA on capable x86-64 hosts. With `-C
//! target-cpu=native` the scalar fallbacks are themselves auto-vectorized by
//! LLVM (SSE2 baseline on x86-64), so even the "slow" path is fast.

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
    abs_max_scalar(buf)
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
