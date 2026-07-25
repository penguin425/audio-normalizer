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

use crate::dsp::kwfilter::KWeight;
use crate::dsp::simd;
use crate::wav::{AudioBuffer, ChannelRole};
use rayon::prelude::*;

#[derive(Debug, Clone)]
pub struct EbuMeasurements {
    pub integrated_lufs: f64,
    pub max_momentary_lufs: f64,
    pub max_short_term_lufs: f64,
    pub loudness_range_lu: f64,
    pub gating_blocks: Vec<f64>,
}

/// Per-channel loudness weight (BS.1770).
pub fn channel_weight(role: ChannelRole) -> f64 {
    match role {
        ChannelRole::Main => 1.0,
        ChannelRole::Surround => 1.41,
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

    EbuMeasurements {
        integrated_lufs: gated_lufs(&gating_blocks),
        max_momentary_lufs: maximum_loudness(&gating_blocks),
        max_short_term_lufs: maximum_loudness(&short_term_blocks),
        loudness_range_lu: loudness_range(&short_term_blocks),
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
        assert_eq!(channel_weight(ChannelRole::Lfe), 0.0);
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
}
