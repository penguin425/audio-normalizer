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
    gated_lufs(&measure_blocks(buf))
}

/// Weighted mean-square energies for every complete 400 ms gating block.
pub fn measure_blocks(buf: &AudioBuffer) -> Vec<f64> {
    let fs = buf.sample_rate as usize;
    let block = (0.4 * fs as f64).round() as usize;
    let hop = (0.1 * fs as f64).round() as usize;
    if block == 0 || hop == 0 || buf.frames < block {
        return Vec::new();
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

    // Block weighted mean squares.
    let mut block_ms: Vec<f64> = Vec::new();
    let mut b = 0usize;
    while b + block <= buf.frames {
        let mut total = 0.0f64;
        for c in 0..buf.channels as usize {
            let w = weights[c];
            if w == 0.0 {
                continue;
            }
            let ss = prefixes[c][b + block] - prefixes[c][b];
            total += w * ss;
        }
        block_ms.push(total / (block as f64));
        b += hop;
    }
    block_ms
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
}
