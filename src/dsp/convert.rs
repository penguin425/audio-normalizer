//! Container PCM <-> normalized f32 conversion.
//!
//! Decode produces planar f32 in approximately [-1.0, 1.0); encode consumes
//! planar f32 and writes interleaved container bytes, clamping to full scale
//! and (for integer kinds) optionally applying triangular (TPDF) dither to
//! eliminate quantization distortion.

use crate::wav::PcmKind;
use rayon::prelude::*;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

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
    // One independent dither RNG per channel (reproducible, no locking).
    let mut rngs = dither_rngs(channels);
    encode_interleaved_with_rngs(planar, kind, dither, &mut rngs)
}

pub(crate) fn dither_rngs(channels: usize) -> Vec<u64> {
    (0..channels)
        .map(|i| 0x9E3779B97F4A7C15u64.wrapping_mul(i as u64 + 1))
        .collect()
}

pub(crate) fn encode_interleaved_with_rngs(
    planar: &[Vec<f32>],
    kind: PcmKind,
    dither: bool,
    rngs: &mut [u64],
) -> Vec<u8> {
    let mut out = Vec::new();
    encode_interleaved_with_rngs_into(planar, kind, dither, rngs, &mut out);
    out
}

/// Encode into caller-owned storage so streaming writers can reuse one buffer.
pub(crate) fn encode_interleaved_with_rngs_into(
    planar: &[Vec<f32>],
    kind: PcmKind,
    dither: bool,
    rngs: &mut [u64],
    out: &mut Vec<u8>,
) {
    let channels = planar.len();
    assert!(channels >= 1);
    assert_eq!(rngs.len(), channels);
    let frames = planar[0].len();
    for channel in planar {
        assert_eq!(channel.len(), frames, "channel length mismatch");
    }
    let bpp = kind.bytes_per_sample();
    out.clear();
    out.resize(frames * channels * bpp, 0);

    #[cfg(target_arch = "x86_64")]
    if kind == PcmKind::S16 && !dither && channels <= 2 && is_x86_feature_detected!("avx2") {
        unsafe { encode_s16_no_dither_avx2(planar, out) };
        return;
    }

    encode_interleaved_scalar_from(planar, kind, dither, rngs, out, 0);
}

fn encode_interleaved_scalar_from(
    planar: &[Vec<f32>],
    kind: PcmKind,
    dither: bool,
    rngs: &mut [u64],
    out: &mut [u8],
    start_frame: usize,
) {
    let channels = planar.len();
    let bpp = kind.bytes_per_sample();
    let frames = planar[0].len();
    let mut idx = start_frame * channels * bpp;
    for f in start_frame..frames {
        for (ch, rng) in planar.iter().zip(rngs.iter_mut()) {
            let s = ch[f];
            let b = encode_sample(s, kind, dither, rng);
            out[idx..idx + bpp].copy_from_slice(&b[..bpp]);
            idx += bpp;
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn encode_s16_no_dither_avx2(planar: &[Vec<f32>], out: &mut [u8]) {
    let frames = planar[0].len();
    let channels = planar.len();
    let mut frame = 0;
    if channels == 1 {
        while frame + 8 <= frames {
            let samples = _mm256_loadu_ps(planar[0].as_ptr().add(frame));
            let quantized = quantize_s16x8(samples);
            let packed = _mm_packs_epi32(
                _mm256_castsi256_si128(quantized),
                _mm256_extracti128_si256(quantized, 1),
            );
            _mm_storeu_si128(out.as_mut_ptr().add(frame * 2).cast(), packed);
            frame += 8;
        }
    } else {
        while frame + 8 <= frames {
            let left = quantize_s16x8(_mm256_loadu_ps(planar[0].as_ptr().add(frame)));
            let right = quantize_s16x8(_mm256_loadu_ps(planar[1].as_ptr().add(frame)));
            let left = _mm_packs_epi32(
                _mm256_castsi256_si128(left),
                _mm256_extracti128_si256(left, 1),
            );
            let right = _mm_packs_epi32(
                _mm256_castsi256_si128(right),
                _mm256_extracti128_si256(right, 1),
            );
            let low = _mm_unpacklo_epi16(left, right);
            let high = _mm_unpackhi_epi16(left, right);
            let destination = out.as_mut_ptr().add(frame * 4);
            _mm_storeu_si128(destination.cast(), low);
            _mm_storeu_si128(destination.add(16).cast(), high);
            frame += 8;
        }
    }
    let mut rngs = [0_u64; 2];
    encode_interleaved_scalar_from(
        planar,
        PcmKind::S16,
        false,
        &mut rngs[..channels],
        out,
        frame,
    );
}

#[cfg(target_arch = "x86_64")]
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn quantize_s16x8(samples: __m256) -> __m256i {
    let ordered = _mm256_cmp_ps::<{ _CMP_ORD_Q }>(samples, samples);
    let finite_or_infinite = _mm256_and_ps(samples, ordered);
    let clamped = _mm256_min_ps(
        _mm256_set1_ps(1.0),
        _mm256_max_ps(_mm256_set1_ps(-1.0), finite_or_infinite),
    );
    let scaled = _mm256_mul_ps(clamped, _mm256_set1_ps(32_768.0));
    let truncated = _mm256_cvttps_epi32(scaled);
    let fraction = _mm256_and_ps(
        _mm256_sub_ps(scaled, _mm256_cvtepi32_ps(truncated)),
        _mm256_castsi256_ps(_mm256_set1_epi32(0x7fff_ffff)),
    );
    let crosses_half = _mm256_castps_si256(_mm256_cmp_ps::<{ _CMP_GE_OQ }>(
        fraction,
        _mm256_set1_ps(0.5),
    ));
    let direction = _mm256_or_si256(
        _mm256_srai_epi32::<31>(_mm256_castps_si256(scaled)),
        _mm256_set1_epi32(1),
    );
    let rounded = _mm256_add_epi32(truncated, _mm256_and_si256(crosses_half, direction));
    _mm256_min_epi32(
        _mm256_set1_epi32(32_767),
        _mm256_max_epi32(_mm256_set1_epi32(-32_768), rounded),
    )
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
pub(crate) fn tpdf(rng: &mut u64) -> f64 {
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

    #[test]
    fn s16_simd_encoder_matches_scalar_quantization() {
        let mut state = 0x243f_6a88_u32;
        let mut channel = vec![
            f32::NAN,
            f32::INFINITY,
            -f32::INFINITY,
            -1.0,
            1.0,
            -0.0,
            0.0,
            0.5 / 32_768.0,
            -0.5 / 32_768.0,
            1.5 / 32_768.0,
            -1.5 / 32_768.0,
        ];
        while channel.len() < 4_099 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            channel.push(f32::from_bits(state));
        }
        for lower_code in -32_768..32_768 {
            let sample = (lower_code as f32 + 0.5) / 32_768.0;
            channel.extend([
                f32::from_bits(sample.to_bits() - 1),
                sample,
                f32::from_bits(sample.to_bits() + 1),
            ]);
        }

        for channels in 1..=2 {
            let planar = if channels == 1 {
                vec![channel.clone()]
            } else {
                let mut other = channel.clone();
                other.reverse();
                vec![channel.clone(), other]
            };
            let mut expected = vec![0; channel.len() * channels * 2];
            let mut expected_rngs = dither_rngs(channels);
            encode_interleaved_scalar_from(
                &planar,
                PcmKind::S16,
                false,
                &mut expected_rngs,
                &mut expected,
                0,
            );

            let mut actual_rngs = dither_rngs(channels);
            let mut actual = Vec::new();
            encode_interleaved_with_rngs_into(
                &planar,
                PcmKind::S16,
                false,
                &mut actual_rngs,
                &mut actual,
            );

            assert_eq!(actual, expected, "channels={channels}");
            assert_eq!(actual_rngs, expected_rngs, "channels={channels}");
        }
    }
}
