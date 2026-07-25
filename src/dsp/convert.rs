//! Container PCM <-> normalized f32 conversion.
//!
//! Decode produces planar f32 in approximately [-1.0, 1.0); encode consumes
//! planar f32 and writes interleaved container bytes, clamping to full scale
//! and (for integer kinds) optionally applying triangular (TPDF) dither to
//! eliminate quantization distortion.

use crate::wav::PcmKind;
use rayon::prelude::*;

/// Decode an interleaved PCM byte buffer into planar f32 channels.
///
/// Channels are decoded independently and in parallel (one rayon task per
/// channel), which scales well on multi-core hosts and keeps each task's output
/// buffer contiguous for the hardware prefetcher.
pub fn decode_planar(bytes: &[u8], kind: PcmKind, channels: usize) -> Vec<Vec<f32>> {
    assert!(channels >= 1);
    let bpp = kind.bytes_per_sample();
    let frame_bytes = bpp * channels;
    let frames = bytes.len() / frame_bytes;

    (0..channels)
        .into_par_iter()
        .map(|c| decode_channel(bytes, kind, channels, c, frames))
        .collect()
}

#[inline]
fn decode_channel(
    bytes: &[u8],
    kind: PcmKind,
    channels: usize,
    c: usize,
    frames: usize,
) -> Vec<f32> {
    let bpp = kind.bytes_per_sample();
    let mut out = Vec::with_capacity(frames);
    let mut o = c * bpp; // byte offset of the first sample for this channel
    let stride = channels * bpp;
    match kind {
        PcmKind::U8 => {
            for _ in 0..frames {
                out.push((bytes[o] as i32 - 128) as f32 / 128.0);
                o += stride;
            }
        }
        PcmKind::S16 => {
            for _ in 0..frames {
                let lo = bytes[o] as i32;
                let hi = bytes[o + 1] as i32;
                let v = ((hi << 8) | (lo & 0xff)) as i16 as f32;
                out.push(v / 32768.0);
                o += stride;
            }
        }
        PcmKind::S24 => {
            for _ in 0..frames {
                let b0 = bytes[o] as i32;
                let b1 = bytes[o + 1] as i32;
                let b2 = bytes[o + 2] as i32;
                let mut v = (b2 << 16) | (b1 << 8) | b0;
                if (v & 0x800000) != 0 {
                    v |= 0xFF00_0000u32 as i32; // sign-extend 24 -> 32 bits
                }
                out.push(v as f32 / 8_388_608.0);
                o += stride;
            }
        }
        PcmKind::S32 => {
            for _ in 0..frames {
                let v = i32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
                out.push(v as f32 / 2_147_483_648.0);
                o += stride;
            }
        }
        PcmKind::F32 => {
            for _ in 0..frames {
                out.push(f32::from_le_bytes([
                    bytes[o],
                    bytes[o + 1],
                    bytes[o + 2],
                    bytes[o + 3],
                ]));
                o += stride;
            }
        }
        PcmKind::F64 => {
            for _ in 0..frames {
                let mut a = [0u8; 8];
                a.copy_from_slice(&bytes[o..o + 8]);
                out.push(f64::from_le_bytes(a) as f32);
                o += stride;
            }
        }
    }
    out
}

/// Encode planar f32 into an interleaved byte buffer of `kind`.
///
/// Samples are clamped to `[-1.0, 1.0]`. For integer kinds, when `dither` is
/// set, triangular (TPDF) dither of one LSB peak-to-peak is added before
/// quantization, which is the audibly-cleanest way to reduce word length.
pub fn encode_interleaved(planar: &[Vec<f32>], kind: PcmKind, dither: bool) -> Vec<u8> {
    let channels = planar.len();
    assert!(channels >= 1);
    let frames = planar[0].len();
    for ch in planar {
        assert_eq!(ch.len(), frames, "channel length mismatch");
    }
    let bpp = kind.bytes_per_sample();
    let mut out = vec![0u8; frames * channels * bpp];

    // One independent dither RNG per channel (reproducible, no locking).
    let mut rngs: Vec<u64> = (0..channels)
        .map(|i| 0x9E3779B97F4A7C15u64.wrapping_mul(i as u64 + 1))
        .collect();

    let mut idx = 0usize;
    for (f, _) in planar[0].iter().enumerate() {
        for (ch, rng) in planar.iter().zip(rngs.iter_mut()) {
            let s = ch[f];
            let b = encode_sample(s, kind, dither, rng);
            out[idx..idx + bpp].copy_from_slice(&b[..bpp]);
            idx += bpp;
        }
    }
    out
}

#[inline]
fn encode_sample(s: f32, kind: PcmKind, dither: bool, rng: &mut u64) -> [u8; 8] {
    let mut buf = [0u8; 8];
    match kind {
        PcmKind::U8 => {
            let d = if dither { tpdf(rng) } else { 0.0 };
            let v = (clamp(s) as f64 * 128.0 + 128.0 + d)
                .round()
                .clamp(0.0, 255.0) as u8;
            buf[0] = v;
        }
        PcmKind::S16 => {
            let d = if dither { tpdf(rng) } else { 0.0 };
            let v = (clamp(s) as f64 * 32768.0 + d)
                .round()
                .clamp(-32768.0, 32767.0) as i16;
            buf[0..2].copy_from_slice(&v.to_le_bytes());
        }
        PcmKind::S24 => {
            let d = if dither { tpdf(rng) } else { 0.0 };
            let v = (clamp(s) as f64 * 8_388_608.0 + d)
                .round()
                .clamp(-8_388_608.0, 8_388_607.0) as i32;
            let u = v as u32;
            buf[0] = u as u8;
            buf[1] = (u >> 8) as u8;
            buf[2] = (u >> 16) as u8;
        }
        PcmKind::S32 => {
            let d = if dither { tpdf(rng) } else { 0.0 };
            let v = (clamp(s) as f64 * 2_147_483_648.0 + d)
                .round()
                .clamp(-2_147_483_648.0, 2_147_483_647.0) as i32;
            buf[0..4].copy_from_slice(&v.to_le_bytes());
        }
        PcmKind::F32 => {
            buf[0..4].copy_from_slice(&clamp(s).to_le_bytes());
        }
        PcmKind::F64 => {
            buf[0..8].copy_from_slice(&(clamp(s) as f64).to_le_bytes());
        }
    }
    buf
}

#[inline]
fn clamp(s: f32) -> f32 {
    if s.is_nan() {
        0.0
    } else {
        s.clamp(-1.0, 1.0)
    }
}

/// Triangular PDF dither in quantizer LSB units (range approximately [-1, 1]).
#[inline]
fn tpdf(rng: &mut u64) -> f64 {
    next_uniform(rng) - next_uniform(rng)
}

/// xorshift64* uniform in [0, 1).
#[inline]
fn next_uniform(rng: &mut u64) -> f64 {
    let mut x = *rng;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *rng = x;
    let r = (x.wrapping_mul(0x2545_F491_4F6C_DD1D)) >> 11; // 53 mantissa bits
    r as f64 * (1.0 / 9_007_199_254_740_992.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dither_changes_quantized_silence_for_every_integer_kind() {
        for kind in [PcmKind::U8, PcmKind::S16, PcmKind::S24, PcmKind::S32] {
            let samples = vec![vec![0.0; 256]];
            let plain = encode_interleaved(&samples, kind, false);
            let dithered = encode_interleaved(&samples, kind, true);
            assert_ne!(dithered, plain, "dither was inert for {kind:?}");
        }
    }
}
