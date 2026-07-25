//! 4x-oversampled true-peak meter (ITU-R BS.1770-5 style).
//!
//! Sample peaks (max of the discrete samples) miss inter-sample peaks that can
//! exceed 0 dBFS after DAC reconstruction. True peak is measured by 4x
//! oversampling each channel with a polyphase FIR and taking the max absolute
//! interpolated value. The FIR is a Kaiser-windowed lowpass with a cutoff at
//! one quarter of the upsampled Nyquist, normalized to unity DC gain; this is
//! the same approach the ITU reference takes and gives accurate inter-sample
//! peaks at a fraction of the cost of a full oversampled reconstruction.
//!
//! Coefficients are fs-independent (the normalized design is fixed) and are
//! computed once via a `OnceLock`.

use crate::wav::AudioBuffer;
use rayon::prelude::*;
use std::f64::consts::PI;
use std::sync::OnceLock;

const M: usize = 4; // oversample factor
const TAPS_PER_PHASE: usize = 16; // total FIR length = 64

fn coeffs() -> &'static Vec<f32> {
    static C: OnceLock<Vec<f32>> = OnceLock::new();
    C.get_or_init(|| {
        let len = M * TAPS_PER_PHASE;
        let center = (len - 1) as f64 / 2.0;
        let beta = 8.5;
        let i0b = bessel_i0(beta);
        let fc = 0.125; // cutoff in upsampled-Nyquist units
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
        // Normalize so the full filter sums to M (=> each polyphase phase sums to 1).
        let sum: f64 = h.iter().map(|&x| x as f64).sum();
        let g = M as f64 / sum;
        for v in h.iter_mut() {
            *v = (*v as f64 * g) as f32;
        }
        h
    })
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
    let h = coeffs();
    let mut phases = [[0.0f32; TAPS_PER_PHASE]; M];
    for p in 0..M {
        for k in 0..TAPS_PER_PHASE {
            phases[p][k] = h[p + M * k];
        }
    }
    buf.data
        .par_iter()
        .map(|ch| true_peak_channel(ch, &phases))
        .reduce(|| 0.0f32, f32::max)
}

fn true_peak_channel(x: &[f32], phases: &[[f32; TAPS_PER_PHASE]; M]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    // Prime the delay line with the first sample so a steady signal (e.g. DC)
    // interpolates to its own value immediately, with no start-up edge transient
    // that would otherwise read as a false inter-sample peak.
    let mut hist = [x[0] as f64; TAPS_PER_PHASE];
    let mut max_pk = x[0].abs();
    for &s in x {
        // Shift history right by one; hist[0] becomes the newest sample.
        hist.copy_within(0..TAPS_PER_PHASE - 1, 1);
        hist[0] = s as f64;
        for ph in phases {
            let mut acc = 0.0f64;
            for k in 0..TAPS_PER_PHASE {
                acc += ph[k] as f64 * hist[k];
            }
            let a = acc.abs() as f32;
            if a > max_pk {
                max_pk = a;
            }
        }
    }
    max_pk
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polyphase_dc_gain_is_one() {
        // A constant input interpolates to the same constant (unity DC gain).
        let h = coeffs();
        let mut phases = [[0.0f32; TAPS_PER_PHASE]; M];
        for p in 0..M {
            for k in 0..TAPS_PER_PHASE {
                phases[p][k] = h[p + M * k];
            }
        }
        let x = vec![0.5f32; 1000];
        let tp = true_peak_channel(&x, &phases);
        assert!((tp - 0.5).abs() < 1e-4, "true peak of DC = {tp}");
    }
}
