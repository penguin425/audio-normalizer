//! Container PCM <-> normalized f32 conversion.
//!
//! Decode produces planar f32 in approximately [-1.0, 1.0); encode consumes
//! planar f32 and writes interleaved container bytes, clamping to full scale
//! and (for integer kinds) optionally applying triangular (TPDF) dither to
//! eliminate quantization distortion.

use crate::wav::PcmKind;
use rayon::prelude::*;
#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
use std::arch::aarch64::*;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;
use std::mem::MaybeUninit;

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

/// Decode into caller-owned channel buffers so streaming verification can
/// inspect the exact PCM bytes written by a lossless muxer without allocating
/// one new planar buffer per chunk.
pub(crate) fn decode_planar_into(
    bytes: &[u8],
    kind: PcmKind,
    channels: usize,
    output: &mut Vec<Vec<f32>>,
) {
    assert!(channels >= 1);
    let bpp = kind.bytes_per_sample();
    let frame_bytes = bpp * channels;
    assert_eq!(bytes.len() % frame_bytes, 0, "partial PCM frame");
    let frames = bytes.len() / frame_bytes;
    if output.len() != channels {
        output.clear();
        output.resize_with(channels, Vec::new);
    }
    if kind == PcmKind::S16 && channels == 2 && s16_stereo_simd_available() {
        decode_s16_stereo_into(bytes, frames, output);
        return;
    }
    output
        .par_iter_mut()
        .enumerate()
        .for_each(|(channel, decoded)| {
            decode_channel_into(bytes, kind, channels, channel, frames, decoded);
        });
}

#[inline]
fn decode_channel(
    bytes: &[u8],
    kind: PcmKind,
    channels: usize,
    c: usize,
    frames: usize,
) -> Vec<f32> {
    let mut out = Vec::with_capacity(frames);
    decode_channel_into(bytes, kind, channels, c, frames, &mut out);
    out
}

#[inline]
fn decode_channel_into(
    bytes: &[u8],
    kind: PcmKind,
    channels: usize,
    c: usize,
    frames: usize,
    out: &mut Vec<f32>,
) {
    let bpp = kind.bytes_per_sample();
    out.clear();
    out.reserve(frames);
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
}

#[inline]
fn s16_stereo_simd_available() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        is_x86_feature_detected!("avx2")
    }
    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    {
        true
    }
    #[cfg(not(any(
        target_arch = "x86_64",
        all(target_arch = "aarch64", target_endian = "little")
    )))]
    {
        false
    }
}

/// Decode and deinterleave the default stereo PCM16 layout in one pass. The
/// caller-owned vectors expose spare capacity during the write so reuse does
/// not pay for a zero-fill pass before every decoder chunk.
fn decode_s16_stereo_into(bytes: &[u8], frames: usize, output: &mut [Vec<f32>]) {
    debug_assert_eq!(output.len(), 2);
    debug_assert!(bytes.len() >= frames * 4);
    let (left, right) = output.split_at_mut(1);
    let left = &mut left[0];
    let right = &mut right[0];
    left.clear();
    right.clear();
    left.reserve(frames);
    right.reserve(frames);

    {
        let left_spare = &mut left.spare_capacity_mut()[..frames];
        let right_spare = &mut right.spare_capacity_mut()[..frames];

        #[cfg(target_arch = "x86_64")]
        let frame = if is_x86_feature_detected!("avx2") {
            // SAFETY: AVX2 is checked at runtime. Both spare slices have
            // `frames` writable elements and the byte slice contains every
            // complete interleaved frame passed to this helper.
            unsafe { decode_s16_stereo_avx2(bytes, left_spare, right_spare) }
        } else {
            0
        };

        #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
        let frame = {
            // SAFETY: Advanced SIMD is mandatory on AArch64. The slice bounds
            // are established above and WAVE PCM is little-endian.
            unsafe { decode_s16_stereo_neon(bytes, left_spare, right_spare) }
        };

        #[cfg(not(any(
            target_arch = "x86_64",
            all(target_arch = "aarch64", target_endian = "little")
        )))]
        let frame = 0;

        decode_s16_stereo_scalar_from(bytes, left_spare, right_spare, frame);
    }

    // SAFETY: the SIMD prefix and scalar tail above initialize every element
    // in both spare-capacity slices before either length becomes observable.
    unsafe {
        left.set_len(frames);
        right.set_len(frames);
    }
}

fn decode_s16_stereo_scalar_from(
    bytes: &[u8],
    left: &mut [MaybeUninit<f32>],
    right: &mut [MaybeUninit<f32>],
    start_frame: usize,
) {
    debug_assert_eq!(left.len(), right.len());
    for frame in start_frame..left.len() {
        let offset = frame * 4;
        let left_sample = i16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
        let right_sample = i16::from_le_bytes([bytes[offset + 2], bytes[offset + 3]]);
        left[frame].write(left_sample as f32 / 32_768.0);
        right[frame].write(right_sample as f32 / 32_768.0);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn decode_s16_stereo_avx2(
    bytes: &[u8],
    left: &mut [MaybeUninit<f32>],
    right: &mut [MaybeUninit<f32>],
) -> usize {
    debug_assert_eq!(left.len(), right.len());
    let frames = left.len();
    let shuffle = _mm_setr_epi8(0, 1, 4, 5, 8, 9, 12, 13, 2, 3, 6, 7, 10, 11, 14, 15);
    let scale = _mm256_set1_ps(1.0 / 32_768.0);
    let left_output = left.as_mut_ptr().cast::<f32>();
    let right_output = right.as_mut_ptr().cast::<f32>();
    let mut frame = 0;
    while frame + 8 <= frames {
        let source = bytes.as_ptr().add(frame * 4);
        let interleaved = _mm256_loadu_si256(source.cast());
        let first = _mm_shuffle_epi8(_mm256_castsi256_si128(interleaved), shuffle);
        let second = _mm_shuffle_epi8(_mm256_extracti128_si256(interleaved, 1), shuffle);
        let left_i16 = _mm_unpacklo_epi64(first, second);
        let right_i16 = _mm_unpackhi_epi64(first, second);
        let left_f32 = _mm256_mul_ps(_mm256_cvtepi32_ps(_mm256_cvtepi16_epi32(left_i16)), scale);
        let right_f32 = _mm256_mul_ps(_mm256_cvtepi32_ps(_mm256_cvtepi16_epi32(right_i16)), scale);
        _mm256_storeu_ps(left_output.add(frame), left_f32);
        _mm256_storeu_ps(right_output.add(frame), right_f32);
        frame += 8;
    }
    frame
}

#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
#[target_feature(enable = "neon")]
unsafe fn decode_s16_stereo_neon(
    bytes: &[u8],
    left: &mut [MaybeUninit<f32>],
    right: &mut [MaybeUninit<f32>],
) -> usize {
    debug_assert_eq!(left.len(), right.len());
    let frames = left.len();
    let scale = vdupq_n_f32(1.0 / 32_768.0);
    let left_output = left.as_mut_ptr().cast::<f32>();
    let right_output = right.as_mut_ptr().cast::<f32>();
    let mut frame = 0;
    while frame + 8 <= frames {
        let source = bytes.as_ptr().add(frame * 4).cast::<i16>();
        let first = vld1q_s16(source);
        let second = vld1q_s16(source.add(8));
        let left_i16 = vuzp1q_s16(first, second);
        let right_i16 = vuzp2q_s16(first, second);
        let left_low = vmulq_f32(vcvtq_f32_s32(vmovl_s16(vget_low_s16(left_i16))), scale);
        let left_high = vmulq_f32(vcvtq_f32_s32(vmovl_high_s16(left_i16)), scale);
        let right_low = vmulq_f32(vcvtq_f32_s32(vmovl_s16(vget_low_s16(right_i16))), scale);
        let right_high = vmulq_f32(vcvtq_f32_s32(vmovl_high_s16(right_i16)), scale);
        vst1q_f32(left_output.add(frame), left_low);
        vst1q_f32(left_output.add(frame + 4), left_high);
        vst1q_f32(right_output.add(frame), right_low);
        vst1q_f32(right_output.add(frame + 4), right_high);
        frame += 8;
    }
    frame
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

/// Append the interleaved signed integer representation consumed by the FLAC
/// writer. The undithered path uses the same byte-exact AVX2 quantizer as WAVE
/// output while preserving the established f32 multiply/round/clamp result.
pub(crate) fn append_quantized_interleaved_i32(
    planar: &[Vec<f32>],
    bits: usize,
    dither: bool,
    rngs: &mut [u64],
    output: &mut Vec<i32>,
) {
    let channels = planar.len();
    assert!(channels >= 1);
    assert!(matches!(bits, 16 | 24));
    assert_eq!(rngs.len(), channels);
    let frames = planar[0].len();
    for channel in planar {
        assert_eq!(channel.len(), frames, "channel length mismatch");
    }

    let start = output.len();
    output.resize(start + frames * channels, 0);
    let appended = &mut output[start..];

    #[cfg(target_arch = "x86_64")]
    if !dither && is_x86_feature_detected!("avx2") {
        unsafe { append_quantized_interleaved_i32_avx2(planar, bits, rngs, appended) };
        return;
    }

    append_quantized_interleaved_i32_scalar_from(planar, bits, dither, rngs, appended, 0);
}

fn append_quantized_interleaved_i32_scalar_from(
    planar: &[Vec<f32>],
    bits: usize,
    dither: bool,
    rngs: &mut [u64],
    output: &mut [i32],
    start_frame: usize,
) {
    let channels = planar.len();
    let frames = planar[0].len();
    let scale = (1_u32 << (bits - 1)) as f32;
    let minimum = -(1_i32 << (bits - 1));
    let maximum = (1_i32 << (bits - 1)) - 1;
    for frame in start_frame..frames {
        for (channel, (samples, rng)) in planar.iter().zip(rngs.iter_mut()).enumerate() {
            let noise = if dither { tpdf(rng) as f32 } else { 0.0 };
            output[frame * channels + channel] = (samples[frame] * scale + noise)
                .round()
                .clamp(minimum as f32, maximum as f32)
                as i32;
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn append_quantized_interleaved_i32_avx2(
    planar: &[Vec<f32>],
    bits: usize,
    rngs: &mut [u64],
    output: &mut [i32],
) {
    let channels = planar.len();
    let frames = planar[0].len();
    let mut frame = 0;

    if channels == 1 {
        while frame + 8 <= frames {
            let samples = _mm256_loadu_ps(planar[0].as_ptr().add(frame));
            let quantized = quantize_flacx8(samples, bits);
            _mm256_storeu_si256(output.as_mut_ptr().add(frame).cast(), quantized);
            frame += 8;
        }
    } else if channels == 2 {
        while frame + 8 <= frames {
            let left = quantize_flacx8(_mm256_loadu_ps(planar[0].as_ptr().add(frame)), bits);
            let right = quantize_flacx8(_mm256_loadu_ps(planar[1].as_ptr().add(frame)), bits);
            let low_pairs = _mm256_unpacklo_epi32(left, right);
            let high_pairs = _mm256_unpackhi_epi32(left, right);
            let first = _mm256_permute2x128_si256::<0x20>(low_pairs, high_pairs);
            let second = _mm256_permute2x128_si256::<0x31>(low_pairs, high_pairs);
            let destination = output.as_mut_ptr().add(frame * 2);
            _mm256_storeu_si256(destination.cast(), first);
            _mm256_storeu_si256(destination.add(8).cast(), second);
            frame += 8;
        }
    } else {
        for frame in 0..frames {
            let mut channel = 0;
            while channel < channels {
                let count = (channels - channel).min(8);
                let samples = load_planar_frame_x8(planar, channel, frame, count);
                let quantized = quantize_flacx8(samples, bits);
                let destination = output.as_mut_ptr().add(frame * channels + channel);
                if count == 8 {
                    _mm256_storeu_si256(destination.cast(), quantized);
                } else {
                    let mut values = [0_i32; 8];
                    _mm256_storeu_si256(values.as_mut_ptr().cast(), quantized);
                    std::ptr::copy_nonoverlapping(values.as_ptr(), destination, count);
                }
                channel += count;
            }
        }
        return;
    }

    append_quantized_interleaved_i32_scalar_from(planar, bits, false, rngs, output, frame);
}

#[cfg(target_arch = "x86_64")]
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn quantize_flacx8(samples: __m256, bits: usize) -> __m256i {
    match bits {
        16 => quantize_s16x8(samples),
        24 => quantize_s24x8(samples),
        _ => unreachable!(),
    }
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
    if !dither && is_x86_feature_detected!("avx2") {
        if kind == PcmKind::S16 {
            if channels <= 2 {
                unsafe { encode_s16_no_dither_avx2(planar, out) };
            } else {
                unsafe { encode_multichannel_no_dither_avx2(planar, kind, out) };
            }
            return;
        }
        if kind == PcmKind::S24 {
            if channels <= 2 {
                unsafe { encode_s24_no_dither_avx2(planar, out) };
            } else {
                unsafe { encode_multichannel_no_dither_avx2(planar, kind, out) };
            }
            return;
        }
    }

    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    if !dither {
        if kind == PcmKind::S16 {
            if channels <= 2 {
                unsafe { encode_s16_no_dither_neon(planar, out) };
            } else {
                unsafe { encode_multichannel_no_dither_neon(planar, kind, out) };
            }
            return;
        }
        if kind == PcmKind::S24 {
            if channels <= 2 {
                unsafe { encode_s24_no_dither_neon(planar, out) };
            } else {
                unsafe { encode_multichannel_no_dither_neon(planar, kind, out) };
            }
            return;
        }
    }

    encode_interleaved_scalar_from(planar, kind, dither, rngs, out, 0);
}

/// Apply the established gain/clip operation and encode borrowed channel
/// planes in one pass. This is intentionally limited to the undithered formats
/// used by the one-shot in-memory normalization path.
pub(crate) fn encode_normalized_borrowed_interleaved_with_rngs_into(
    planar: &[&[f32]],
    kind: PcmKind,
    gain: f32,
    ceiling: f32,
    rngs: &mut [u64],
    out: &mut Vec<u8>,
) {
    let channels = planar.len();
    assert!(channels >= 1);
    assert_eq!(rngs.len(), channels);
    assert!(matches!(kind, PcmKind::S16 | PcmKind::F32));
    assert!(kind != PcmKind::S16 || channels <= 2);
    let frames = planar[0].len();
    for channel in planar {
        assert_eq!(channel.len(), frames, "channel length mismatch");
    }
    let bpp = kind.bytes_per_sample();
    out.clear();
    out.resize(frames * channels * bpp, 0);

    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx2") {
        match kind {
            PcmKind::S16 => {
                unsafe { encode_normalized_s16_no_dither_avx2(planar, gain, ceiling, out) };
                return;
            }
            PcmKind::F32 => {
                unsafe { encode_normalized_f32_avx2(planar, gain, ceiling, out) };
                return;
            }
            _ => unreachable!(),
        }
    }

    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    match kind {
        PcmKind::S16 => unsafe { encode_normalized_s16_no_dither_neon(planar, gain, ceiling, out) },
        PcmKind::F32 => unsafe { encode_normalized_f32_neon(planar, gain, ceiling, out) },
        _ => unreachable!(),
    }

    #[cfg(not(all(target_arch = "aarch64", target_endian = "little")))]
    encode_normalized_interleaved_scalar_from(planar, kind, gain, ceiling, rngs, out, 0);
}

fn encode_interleaved_scalar_from<P: AsRef<[f32]>>(
    planar: &[P],
    kind: PcmKind,
    dither: bool,
    rngs: &mut [u64],
    out: &mut [u8],
    start_frame: usize,
) {
    let channels = planar.len();
    let bpp = kind.bytes_per_sample();
    let frames = planar[0].as_ref().len();
    let mut idx = start_frame * channels * bpp;
    for f in start_frame..frames {
        for (ch, rng) in planar.iter().zip(rngs.iter_mut()) {
            let s = ch.as_ref()[f];
            let b = encode_sample(s, kind, dither, rng);
            out[idx..idx + bpp].copy_from_slice(&b[..bpp]);
            idx += bpp;
        }
    }
}

fn encode_normalized_interleaved_scalar_from<P: AsRef<[f32]>>(
    planar: &[P],
    kind: PcmKind,
    gain: f32,
    ceiling: f32,
    rngs: &mut [u64],
    out: &mut [u8],
    start_frame: usize,
) {
    let channels = planar.len();
    let bpp = kind.bytes_per_sample();
    let frames = planar[0].as_ref().len();
    let mut idx = start_frame * channels * bpp;
    for frame in start_frame..frames {
        for (channel, rng) in planar.iter().zip(rngs.iter_mut()) {
            let normalized = (channel.as_ref()[frame] * gain).clamp(-ceiling, ceiling);
            let encoded = encode_sample(normalized, kind, false, rng);
            out[idx..idx + bpp].copy_from_slice(&encoded[..bpp]);
            idx += bpp;
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn encode_s16_no_dither_avx2<P: AsRef<[f32]>>(planar: &[P], out: &mut [u8]) {
    let frames = planar[0].as_ref().len();
    let channels = planar.len();
    let mut frame = 0;
    if channels == 1 {
        while frame + 8 <= frames {
            let samples = _mm256_loadu_ps(planar[0].as_ref().as_ptr().add(frame));
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
            let left = quantize_s16x8(_mm256_loadu_ps(planar[0].as_ref().as_ptr().add(frame)));
            let right = quantize_s16x8(_mm256_loadu_ps(planar[1].as_ref().as_ptr().add(frame)));
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
#[target_feature(enable = "avx2")]
unsafe fn encode_s24_no_dither_avx2(planar: &[Vec<f32>], out: &mut [u8]) {
    let frames = planar[0].len();
    let channels = planar.len();
    debug_assert!(matches!(channels, 1 | 2));
    let mut frame = 0;
    if channels == 1 {
        while frame + 8 <= frames {
            let quantized = quantize_s24x8(_mm256_loadu_ps(planar[0].as_ptr().add(frame)));
            let destination = out.as_mut_ptr().add(frame * 3);
            store_packed_s24x4(_mm256_castsi256_si128(quantized), destination);
            store_packed_s24x4(_mm256_extracti128_si256(quantized, 1), destination.add(12));
            frame += 8;
        }
    } else {
        while frame + 8 <= frames {
            let left = quantize_s24x8(_mm256_loadu_ps(planar[0].as_ptr().add(frame)));
            let right = quantize_s24x8(_mm256_loadu_ps(planar[1].as_ptr().add(frame)));
            let low_pairs = _mm256_unpacklo_epi32(left, right);
            let high_pairs = _mm256_unpackhi_epi32(left, right);
            let first = _mm256_permute2x128_si256::<0x20>(low_pairs, high_pairs);
            let second = _mm256_permute2x128_si256::<0x31>(low_pairs, high_pairs);
            let destination = out.as_mut_ptr().add(frame * 6);
            store_packed_s24x4(_mm256_castsi256_si128(first), destination);
            store_packed_s24x4(_mm256_extracti128_si256(first, 1), destination.add(12));
            store_packed_s24x4(_mm256_castsi256_si128(second), destination.add(24));
            store_packed_s24x4(_mm256_extracti128_si256(second, 1), destination.add(36));
            frame += 8;
        }
    }
    let mut rngs = [0_u64; 2];
    encode_interleaved_scalar_from(
        planar,
        PcmKind::S24,
        false,
        &mut rngs[..channels],
        out,
        frame,
    );
}

#[cfg(target_arch = "x86_64")]
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn store_packed_s24x4(samples: __m128i, destination: *mut u8) {
    let packed = _mm_shuffle_epi8(
        samples,
        _mm_setr_epi8(0, 1, 2, 4, 5, 6, 8, 9, 10, 12, 13, 14, -1, -1, -1, -1),
    );
    _mm_storel_epi64(destination.cast(), packed);
    std::ptr::write_unaligned(
        destination.add(8).cast::<u32>(),
        _mm_extract_epi32::<2>(packed) as u32,
    );
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn encode_normalized_s16_no_dither_avx2<P: AsRef<[f32]>>(
    planar: &[P],
    gain: f32,
    ceiling: f32,
    out: &mut [u8],
) {
    let frames = planar[0].as_ref().len();
    let channels = planar.len();
    let gain_vector = _mm256_set1_ps(gain);
    let lower = _mm256_set1_ps(-ceiling);
    let upper = _mm256_set1_ps(ceiling);
    let mut frame = 0;
    if channels == 1 {
        while frame + 8 <= frames {
            let samples = _mm256_loadu_ps(planar[0].as_ref().as_ptr().add(frame));
            let quantized = quantize_normalized_s16x8(samples, gain_vector, lower, upper);
            let packed = _mm_packs_epi32(
                _mm256_castsi256_si128(quantized),
                _mm256_extracti128_si256(quantized, 1),
            );
            _mm_storeu_si128(out.as_mut_ptr().add(frame * 2).cast(), packed);
            frame += 8;
        }
    } else {
        while frame + 8 <= frames {
            let left = quantize_normalized_s16x8(
                _mm256_loadu_ps(planar[0].as_ref().as_ptr().add(frame)),
                gain_vector,
                lower,
                upper,
            );
            let right = quantize_normalized_s16x8(
                _mm256_loadu_ps(planar[1].as_ref().as_ptr().add(frame)),
                gain_vector,
                lower,
                upper,
            );
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
    encode_normalized_interleaved_scalar_from(
        planar,
        PcmKind::S16,
        gain,
        ceiling,
        &mut rngs[..channels],
        out,
        frame,
    );
}

#[cfg(target_arch = "x86_64")]
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn quantize_normalized_s16x8(
    samples: __m256,
    gain: __m256,
    lower: __m256,
    upper: __m256,
) -> __m256i {
    let gained = _mm256_mul_ps(samples, gain);
    // Match `apply_gain_and_hard_clip_avx2`: keeping the gained value in the
    // second operand preserves its NaN for the quantizer's NaN-to-zero rule.
    let protected = _mm256_min_ps(upper, _mm256_max_ps(lower, gained));
    quantize_s16x8(protected)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn encode_normalized_f32_avx2<P: AsRef<[f32]>>(
    planar: &[P],
    gain: f32,
    ceiling: f32,
    out: &mut [u8],
) {
    let frames = planar[0].as_ref().len();
    let channels = planar.len();
    let gain_vector = _mm256_set1_ps(gain);
    let lower = _mm256_set1_ps(-ceiling);
    let upper = _mm256_set1_ps(ceiling);
    let mut frame = 0;
    if channels == 1 {
        while frame + 8 <= frames {
            let samples = _mm256_loadu_ps(planar[0].as_ref().as_ptr().add(frame));
            let normalized = normalize_f32_outputx8(samples, gain_vector, lower, upper);
            _mm256_storeu_ps(out.as_mut_ptr().add(frame * 4).cast(), normalized);
            frame += 8;
        }
    } else {
        while frame + 8 <= frames {
            let left = normalize_f32_outputx8(
                _mm256_loadu_ps(planar[0].as_ref().as_ptr().add(frame)),
                gain_vector,
                lower,
                upper,
            );
            let right = normalize_f32_outputx8(
                _mm256_loadu_ps(planar[1].as_ref().as_ptr().add(frame)),
                gain_vector,
                lower,
                upper,
            );
            let low = _mm256_unpacklo_ps(left, right);
            let high = _mm256_unpackhi_ps(left, right);
            let first = _mm256_permute2f128_ps::<0x20>(low, high);
            let second = _mm256_permute2f128_ps::<0x31>(low, high);
            let destination = out.as_mut_ptr().add(frame * 8).cast::<f32>();
            _mm256_storeu_ps(destination, first);
            _mm256_storeu_ps(destination.add(8), second);
            frame += 8;
        }
    }
    let mut rngs = [0_u64; 2];
    encode_normalized_interleaved_scalar_from(
        planar,
        PcmKind::F32,
        gain,
        ceiling,
        &mut rngs[..channels],
        out,
        frame,
    );
}

#[cfg(target_arch = "x86_64")]
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn normalize_f32_outputx8(
    samples: __m256,
    gain: __m256,
    lower: __m256,
    upper: __m256,
) -> __m256 {
    let gained = _mm256_mul_ps(samples, gain);
    let protected = _mm256_min_ps(upper, _mm256_max_ps(lower, gained));
    let ordered = _mm256_cmp_ps::<{ _CMP_ORD_Q }>(protected, protected);
    let finite_or_infinite = _mm256_and_ps(protected, ordered);
    _mm256_min_ps(
        _mm256_set1_ps(1.0),
        _mm256_max_ps(_mm256_set1_ps(-1.0), finite_or_infinite),
    )
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn encode_multichannel_no_dither_avx2(planar: &[Vec<f32>], kind: PcmKind, out: &mut [u8]) {
    debug_assert!(matches!(kind, PcmKind::S16 | PcmKind::S24));
    debug_assert!(planar.len() >= 3);
    let channels = planar.len();
    let frames = planar[0].len();
    let bytes_per_sample = kind.bytes_per_sample();
    for frame in 0..frames {
        let mut channel = 0;
        while channel + 8 <= channels {
            let samples = load_planar_frame_x8(planar, channel, frame, 8);
            let destination = out
                .as_mut_ptr()
                .add((frame * channels + channel) * bytes_per_sample);
            match kind {
                PcmKind::S16 => store_s16x8(quantize_s16x8(samples), destination, 8),
                PcmKind::S24 => store_s24x8(quantize_s24x8(samples), destination, 8),
                _ => unreachable!(),
            }
            channel += 8;
        }
        let remaining = channels - channel;
        if remaining >= 3 {
            let samples = load_planar_frame_x8(planar, channel, frame, remaining);
            let destination = out
                .as_mut_ptr()
                .add((frame * channels + channel) * bytes_per_sample);
            match kind {
                PcmKind::S16 => store_s16x8(quantize_s16x8(samples), destination, remaining),
                PcmKind::S24 => store_s24x8(quantize_s24x8(samples), destination, remaining),
                _ => unreachable!(),
            }
            channel = channels;
        }
        let mut rng = 0;
        while channel < channels {
            let encoded = encode_sample(planar[channel][frame], kind, false, &mut rng);
            let destination = (frame * channels + channel) * bytes_per_sample;
            out[destination..destination + bytes_per_sample]
                .copy_from_slice(&encoded[..bytes_per_sample]);
            channel += 1;
        }
    }
}

#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
#[target_feature(enable = "neon")]
unsafe fn encode_s16_no_dither_neon<P: AsRef<[f32]>>(planar: &[P], out: &mut [u8]) {
    let frames = planar[0].as_ref().len();
    let channels = planar.len();
    let mut frame = 0;
    if channels == 1 {
        while frame + 8 <= frames {
            let low = quantize_s16x4(vld1q_f32(planar[0].as_ref().as_ptr().add(frame)));
            let high = quantize_s16x4(vld1q_f32(planar[0].as_ref().as_ptr().add(frame + 4)));
            let packed = vcombine_s16(vqmovn_s32(low), vqmovn_s32(high));
            vst1q_s16(out.as_mut_ptr().add(frame * 2).cast(), packed);
            frame += 8;
        }
    } else {
        while frame + 8 <= frames {
            let left_low = quantize_s16x4(vld1q_f32(planar[0].as_ref().as_ptr().add(frame)));
            let left_high = quantize_s16x4(vld1q_f32(planar[0].as_ref().as_ptr().add(frame + 4)));
            let right_low = quantize_s16x4(vld1q_f32(planar[1].as_ref().as_ptr().add(frame)));
            let right_high = quantize_s16x4(vld1q_f32(planar[1].as_ref().as_ptr().add(frame + 4)));
            let left = vcombine_s16(vqmovn_s32(left_low), vqmovn_s32(left_high));
            let right = vcombine_s16(vqmovn_s32(right_low), vqmovn_s32(right_high));
            let destination = out.as_mut_ptr().add(frame * 4).cast();
            vst1q_s16(destination, vzip1q_s16(left, right));
            vst1q_s16(destination.add(8), vzip2q_s16(left, right));
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

#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
#[target_feature(enable = "neon")]
unsafe fn encode_normalized_s16_no_dither_neon<P: AsRef<[f32]>>(
    planar: &[P],
    gain: f32,
    ceiling: f32,
    out: &mut [u8],
) {
    let frames = planar[0].as_ref().len();
    let channels = planar.len();
    let gain_vector = vdupq_n_f32(gain);
    let lower = vdupq_n_f32(-ceiling);
    let upper = vdupq_n_f32(ceiling);
    let mut frame = 0;
    if channels == 1 {
        while frame + 8 <= frames {
            let low = quantize_normalized_s16x4(
                vld1q_f32(planar[0].as_ref().as_ptr().add(frame)),
                gain_vector,
                lower,
                upper,
            );
            let high = quantize_normalized_s16x4(
                vld1q_f32(planar[0].as_ref().as_ptr().add(frame + 4)),
                gain_vector,
                lower,
                upper,
            );
            let packed = vcombine_s16(vqmovn_s32(low), vqmovn_s32(high));
            vst1q_s16(out.as_mut_ptr().add(frame * 2).cast(), packed);
            frame += 8;
        }
    } else {
        while frame + 8 <= frames {
            let left_low = quantize_normalized_s16x4(
                vld1q_f32(planar[0].as_ref().as_ptr().add(frame)),
                gain_vector,
                lower,
                upper,
            );
            let left_high = quantize_normalized_s16x4(
                vld1q_f32(planar[0].as_ref().as_ptr().add(frame + 4)),
                gain_vector,
                lower,
                upper,
            );
            let right_low = quantize_normalized_s16x4(
                vld1q_f32(planar[1].as_ref().as_ptr().add(frame)),
                gain_vector,
                lower,
                upper,
            );
            let right_high = quantize_normalized_s16x4(
                vld1q_f32(planar[1].as_ref().as_ptr().add(frame + 4)),
                gain_vector,
                lower,
                upper,
            );
            let left = vcombine_s16(vqmovn_s32(left_low), vqmovn_s32(left_high));
            let right = vcombine_s16(vqmovn_s32(right_low), vqmovn_s32(right_high));
            let destination = out.as_mut_ptr().add(frame * 4).cast();
            vst1q_s16(destination, vzip1q_s16(left, right));
            vst1q_s16(destination.add(8), vzip2q_s16(left, right));
            frame += 8;
        }
    }
    let mut rngs = [0_u64; 2];
    encode_normalized_interleaved_scalar_from(
        planar,
        PcmKind::S16,
        gain,
        ceiling,
        &mut rngs[..channels],
        out,
        frame,
    );
}

#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
#[inline]
#[target_feature(enable = "neon")]
unsafe fn quantize_normalized_s16x4(
    samples: float32x4_t,
    gain: float32x4_t,
    lower: float32x4_t,
    upper: float32x4_t,
) -> int32x4_t {
    let gained = vmulq_f32(samples, gain);
    let protected = vminq_f32(upper, vmaxq_f32(lower, gained));
    quantize_s16x4(protected)
}

#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
#[target_feature(enable = "neon")]
unsafe fn encode_normalized_f32_neon<P: AsRef<[f32]>>(
    planar: &[P],
    gain: f32,
    ceiling: f32,
    out: &mut [u8],
) {
    let frames = planar[0].as_ref().len();
    let channels = planar.len();
    let gain_vector = vdupq_n_f32(gain);
    let lower = vdupq_n_f32(-ceiling);
    let upper = vdupq_n_f32(ceiling);
    let mut frame = 0;
    if channels == 1 {
        while frame + 4 <= frames {
            let samples = vld1q_f32(planar[0].as_ref().as_ptr().add(frame));
            let normalized = normalize_f32_outputx4(samples, gain_vector, lower, upper);
            vst1q_f32(out.as_mut_ptr().add(frame * 4).cast(), normalized);
            frame += 4;
        }
    } else {
        while frame + 4 <= frames {
            let left = normalize_f32_outputx4(
                vld1q_f32(planar[0].as_ref().as_ptr().add(frame)),
                gain_vector,
                lower,
                upper,
            );
            let right = normalize_f32_outputx4(
                vld1q_f32(planar[1].as_ref().as_ptr().add(frame)),
                gain_vector,
                lower,
                upper,
            );
            let destination = out.as_mut_ptr().add(frame * 8).cast::<f32>();
            vst1q_f32(destination, vzip1q_f32(left, right));
            vst1q_f32(destination.add(4), vzip2q_f32(left, right));
            frame += 4;
        }
    }
    let mut rngs = [0_u64; 2];
    encode_normalized_interleaved_scalar_from(
        planar,
        PcmKind::F32,
        gain,
        ceiling,
        &mut rngs[..channels],
        out,
        frame,
    );
}

#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
#[inline]
#[target_feature(enable = "neon")]
unsafe fn normalize_f32_outputx4(
    samples: float32x4_t,
    gain: float32x4_t,
    lower: float32x4_t,
    upper: float32x4_t,
) -> float32x4_t {
    let gained = vmulq_f32(samples, gain);
    let protected = vminq_f32(upper, vmaxq_f32(lower, gained));
    let ordered = vceqq_f32(protected, protected);
    let finite_or_infinite = vbslq_f32(ordered, protected, vdupq_n_f32(0.0));
    vminq_f32(
        vdupq_n_f32(1.0),
        vmaxq_f32(vdupq_n_f32(-1.0), finite_or_infinite),
    )
}

#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
#[target_feature(enable = "neon")]
unsafe fn encode_s24_no_dither_neon(planar: &[Vec<f32>], out: &mut [u8]) {
    let frames = planar[0].len();
    let channels = planar.len();
    let mut frame = 0;
    if channels == 1 {
        while frame + 4 <= frames {
            let quantized = quantize_s24x4(vld1q_f32(planar[0].as_ptr().add(frame)));
            store_s24x4(quantized, out.as_mut_ptr().add(frame * 3), 4);
            frame += 4;
        }
    } else {
        while frame + 4 <= frames {
            let left = quantize_s24x4(vld1q_f32(planar[0].as_ptr().add(frame)));
            let right = quantize_s24x4(vld1q_f32(planar[1].as_ptr().add(frame)));
            let destination = out.as_mut_ptr().add(frame * 6);
            store_s24x4(vzip1q_s32(left, right), destination, 4);
            store_s24x4(vzip2q_s32(left, right), destination.add(12), 4);
            frame += 4;
        }
    }
    let mut rngs = [0_u64; 2];
    encode_interleaved_scalar_from(
        planar,
        PcmKind::S24,
        false,
        &mut rngs[..channels],
        out,
        frame,
    );
}

#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
#[target_feature(enable = "neon")]
unsafe fn encode_multichannel_no_dither_neon(planar: &[Vec<f32>], kind: PcmKind, out: &mut [u8]) {
    debug_assert!(matches!(kind, PcmKind::S16 | PcmKind::S24));
    debug_assert!(planar.len() >= 3);
    let channels = planar.len();
    let frames = planar[0].len();
    let bytes_per_sample = kind.bytes_per_sample();
    for frame in 0..frames {
        let mut channel = 0;
        while channel + 4 <= channels {
            let samples = load_planar_frame_x4(planar, channel, frame, 4);
            let destination = out
                .as_mut_ptr()
                .add((frame * channels + channel) * bytes_per_sample);
            match kind {
                PcmKind::S16 => store_s16x4(quantize_s16x4(samples), destination, 4),
                PcmKind::S24 => store_s24x4(quantize_s24x4(samples), destination, 4),
                _ => unreachable!(),
            }
            channel += 4;
        }
        let remaining = channels - channel;
        if remaining >= 3 {
            let samples = load_planar_frame_x4(planar, channel, frame, remaining);
            let destination = out
                .as_mut_ptr()
                .add((frame * channels + channel) * bytes_per_sample);
            match kind {
                PcmKind::S16 => store_s16x4(quantize_s16x4(samples), destination, remaining),
                PcmKind::S24 => store_s24x4(quantize_s24x4(samples), destination, remaining),
                _ => unreachable!(),
            }
            channel = channels;
        }
        let mut rng = 0;
        while channel < channels {
            let encoded = encode_sample(planar[channel][frame], kind, false, &mut rng);
            let destination = (frame * channels + channel) * bytes_per_sample;
            out[destination..destination + bytes_per_sample]
                .copy_from_slice(&encoded[..bytes_per_sample]);
            channel += 1;
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn load_planar_frame_x8(
    planar: &[Vec<f32>],
    channel: usize,
    frame: usize,
    count: usize,
) -> __m256 {
    debug_assert!((1..=8).contains(&count));
    if count == 8 {
        return _mm256_set_ps(
            planar[channel + 7][frame],
            planar[channel + 6][frame],
            planar[channel + 5][frame],
            planar[channel + 4][frame],
            planar[channel + 3][frame],
            planar[channel + 2][frame],
            planar[channel + 1][frame],
            planar[channel][frame],
        );
    }
    let mut samples = [0.0; 8];
    for lane in 0..count {
        samples[lane] = planar[channel + lane][frame];
    }
    _mm256_loadu_ps(samples.as_ptr())
}

#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
#[inline]
#[target_feature(enable = "neon")]
unsafe fn load_planar_frame_x4(
    planar: &[Vec<f32>],
    channel: usize,
    frame: usize,
    count: usize,
) -> float32x4_t {
    debug_assert!((1..=4).contains(&count));
    let mut samples = [0.0; 4];
    for lane in 0..count {
        samples[lane] = planar[channel + lane][frame];
    }
    vld1q_f32(samples.as_ptr())
}

#[cfg(target_arch = "x86_64")]
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn store_s16x8(quantized: __m256i, destination: *mut u8, count: usize) {
    let packed = _mm_packs_epi32(
        _mm256_castsi256_si128(quantized),
        _mm256_extracti128_si256(quantized, 1),
    );
    if count == 8 {
        _mm_storeu_si128(destination.cast(), packed);
        return;
    }
    let mut samples = [0_i16; 8];
    _mm_storeu_si128(samples.as_mut_ptr().cast(), packed);
    std::ptr::copy_nonoverlapping(samples.as_ptr().cast::<u8>(), destination, count * 2);
}

#[cfg(target_arch = "x86_64")]
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn store_s24x8(quantized: __m256i, destination: *mut u8, count: usize) {
    let mut samples = [0_i32; 8];
    _mm256_storeu_si256(samples.as_mut_ptr().cast(), quantized);
    for (lane, sample) in samples[..count].iter().copied().enumerate() {
        let bytes = sample.to_le_bytes();
        let output = std::slice::from_raw_parts_mut(destination.add(lane * 3), 3);
        output.copy_from_slice(&bytes[..3]);
    }
}

#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
#[inline]
#[target_feature(enable = "neon")]
unsafe fn store_s16x4(quantized: int32x4_t, destination: *mut u8, count: usize) {
    let packed = vqmovn_s32(quantized);
    if count == 4 {
        vst1_s16(destination.cast(), packed);
        return;
    }
    let mut samples = [0_i16; 4];
    vst1_s16(samples.as_mut_ptr(), packed);
    std::ptr::copy_nonoverlapping(samples.as_ptr().cast::<u8>(), destination, count * 2);
}

#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
#[inline]
#[target_feature(enable = "neon")]
unsafe fn store_s24x4(quantized: int32x4_t, destination: *mut u8, count: usize) {
    let mut samples = [0_i32; 4];
    vst1q_s32(samples.as_mut_ptr(), quantized);
    for (lane, sample) in samples[..count].iter().copied().enumerate() {
        let bytes = sample.to_le_bytes();
        let output = std::slice::from_raw_parts_mut(destination.add(lane * 3), 3);
        output.copy_from_slice(&bytes[..3]);
    }
}

#[cfg(target_arch = "x86_64")]
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn quantize_s16x8(samples: __m256) -> __m256i {
    quantize_signedx8(samples, 32_768.0, -32_768, 32_767)
}

#[cfg(target_arch = "x86_64")]
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn quantize_s24x8(samples: __m256) -> __m256i {
    quantize_signedx8(samples, 8_388_608.0, -8_388_608, 8_388_607)
}

#[cfg(target_arch = "x86_64")]
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn quantize_signedx8(samples: __m256, scale: f32, minimum: i32, maximum: i32) -> __m256i {
    let ordered = _mm256_cmp_ps::<{ _CMP_ORD_Q }>(samples, samples);
    let finite_or_infinite = _mm256_and_ps(samples, ordered);
    let clamped = _mm256_min_ps(
        _mm256_set1_ps(1.0),
        _mm256_max_ps(_mm256_set1_ps(-1.0), finite_or_infinite),
    );
    let scaled = _mm256_mul_ps(clamped, _mm256_set1_ps(scale));
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
        _mm256_set1_epi32(maximum),
        _mm256_max_epi32(_mm256_set1_epi32(minimum), rounded),
    )
}

#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
#[inline]
#[target_feature(enable = "neon")]
unsafe fn quantize_s16x4(samples: float32x4_t) -> int32x4_t {
    quantize_signedx4(samples, 32_768.0, -32_768, 32_767)
}

#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
#[inline]
#[target_feature(enable = "neon")]
unsafe fn quantize_s24x4(samples: float32x4_t) -> int32x4_t {
    quantize_signedx4(samples, 8_388_608.0, -8_388_608, 8_388_607)
}

#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
#[inline]
#[target_feature(enable = "neon")]
unsafe fn quantize_signedx4(
    samples: float32x4_t,
    scale: f32,
    minimum: i32,
    maximum: i32,
) -> int32x4_t {
    let ordered = vceqq_f32(samples, samples);
    let finite_or_infinite = vbslq_f32(ordered, samples, vdupq_n_f32(0.0));
    let clamped = vminq_f32(
        vdupq_n_f32(1.0),
        vmaxq_f32(vdupq_n_f32(-1.0), finite_or_infinite),
    );
    let rounded = vcvtaq_s32_f32(vmulq_n_f32(clamped, scale));
    vminq_s32(
        vdupq_n_s32(maximum),
        vmaxq_s32(vdupq_n_s32(minimum), rounded),
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
    fn fused_borrowed_normalization_matches_owned_encoder_exactly() {
        let channel = vec![
            f32::NAN,
            f32::INFINITY,
            -f32::INFINITY,
            -1.25,
            -1.0,
            -0.0,
            0.0,
            0.5 / 32_768.0,
            -0.5 / 32_768.0,
            1.0,
            1.25,
            f32::from_bits(1),
            0.125,
            -0.375,
            0.875,
            -0.9375,
            0.3,
        ];
        for kind in [PcmKind::S16, PcmKind::F32] {
            for channels in 1..=2 {
                for (gain, ceiling) in [(0.75, 0.9), (1.37, 0.99), (1.0, 1.0)] {
                    let original = channel_variants(&channel, channels);
                    let mut normalized = original.clone();
                    for channel in &mut normalized {
                        crate::dsp::simd::apply_gain_and_hard_clip(channel, gain, ceiling);
                    }
                    let mut expected_rngs = dither_rngs(channels);
                    let mut expected = Vec::new();
                    encode_interleaved_with_rngs_into(
                        &normalized,
                        kind,
                        false,
                        &mut expected_rngs,
                        &mut expected,
                    );

                    let borrowed_storage = original;
                    let borrowed = borrowed_storage
                        .iter()
                        .map(Vec::as_slice)
                        .collect::<Vec<_>>();
                    let mut actual_rngs = dither_rngs(channels);
                    let mut actual = Vec::new();
                    encode_normalized_borrowed_interleaved_with_rngs_into(
                        &borrowed,
                        kind,
                        gain,
                        ceiling,
                        &mut actual_rngs,
                        &mut actual,
                    );

                    assert_eq!(
                        actual, expected,
                        "kind={kind:?}, channels={channels}, gain={gain}, ceiling={ceiling}"
                    );
                    assert_eq!(actual_rngs, expected_rngs);
                }
            }
        }
    }

    #[test]
    fn flac_i32_simd_quantizer_matches_scalar_and_preserves_prefix() {
        let mut state = 0xa409_3822_u32;
        let mut channel = vec![
            f32::NAN,
            f32::INFINITY,
            -f32::INFINITY,
            -1.25,
            -1.0,
            1.0,
            1.25,
            -0.0,
            0.0,
            f32::from_bits(1),
            0.5 / 32_768.0,
            -0.5 / 32_768.0,
            1.5 / 8_388_608.0,
            -1.5 / 8_388_608.0,
        ];
        while channel.len() < 4_099 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            channel.push(f32::from_bits(state));
        }

        for bits in [16, 24] {
            for channels in [1, 2, 3, 5, 8] {
                let planar = channel_variants(&channel, channels);
                let prefix = [i32::MIN, -17, 0, 42, i32::MAX];
                let mut expected = prefix.to_vec();
                let expected_start = expected.len();
                expected.resize(expected_start + channel.len() * channels, 0);
                let mut expected_rngs = dither_rngs(channels);
                append_quantized_interleaved_i32_scalar_from(
                    &planar,
                    bits,
                    false,
                    &mut expected_rngs,
                    &mut expected[expected_start..],
                    0,
                );

                let mut actual = prefix.to_vec();
                let mut actual_rngs = dither_rngs(channels);
                append_quantized_interleaved_i32(
                    &planar,
                    bits,
                    false,
                    &mut actual_rngs,
                    &mut actual,
                );

                assert_eq!(actual, expected, "bits={bits}, channels={channels}");
                assert_eq!(
                    actual_rngs, expected_rngs,
                    "bits={bits}, channels={channels}"
                );
            }
        }
    }

    #[test]
    fn flac_i32_dither_keeps_scalar_rng_sequence_across_chunks() {
        let first = vec![
            (0..257)
                .map(|frame| (frame as f32 * 0.017_31).sin() * 0.9)
                .collect::<Vec<_>>(),
            (0..257)
                .map(|frame| (frame as f32 * 0.011_93).cos() * 0.8)
                .collect::<Vec<_>>(),
        ];
        let second = vec![first[0][91..].to_vec(), first[1][91..].to_vec()];

        for bits in [16, 24] {
            let mut expected = vec![11, 22, 33];
            let mut actual = expected.clone();
            let mut expected_rngs = dither_rngs(2);
            let mut actual_rngs = expected_rngs.clone();
            for chunk in [&first, &second] {
                let start = expected.len();
                expected.resize(start + chunk[0].len() * 2, 0);
                append_quantized_interleaved_i32_scalar_from(
                    chunk,
                    bits,
                    true,
                    &mut expected_rngs,
                    &mut expected[start..],
                    0,
                );
                append_quantized_interleaved_i32(chunk, bits, true, &mut actual_rngs, &mut actual);
            }
            assert_eq!(actual, expected, "bits={bits}");
            assert_eq!(actual_rngs, expected_rngs, "bits={bits}");
        }
    }

    #[test]
    fn s16_simd_encoder_matches_scalar_quantization_for_multichannel_layouts() {
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

        for channels in [1, 2, 3, 5, 6, 7, 8, 9, 10, 11, 12, 16] {
            let planar = channel_variants(&channel, channels);
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

    #[test]
    fn s24_simd_encoder_matches_scalar_quantization_for_multichannel_layouts() {
        let mut state = 0x517c_c1b7_u32;
        let mut channel = vec![
            f32::NAN,
            f32::INFINITY,
            -f32::INFINITY,
            -1.0,
            1.0,
            -0.0,
            0.0,
            f32::from_bits(1),
            0.5 / 8_388_608.0,
            -0.5 / 8_388_608.0,
            1.5 / 8_388_608.0,
            -1.5 / 8_388_608.0,
        ];
        while channel.len() < 50_003 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            channel.push(f32::from_bits(state));
        }
        for lower_code in (-8_388_608..8_388_607).step_by(4_093) {
            let sample = (lower_code as f32 + 0.5) / 8_388_608.0;
            channel.extend([
                f32::from_bits(sample.to_bits() - 1),
                sample,
                f32::from_bits(sample.to_bits() + 1),
            ]);
        }

        for channels in [1, 2, 3, 5, 6, 7, 8, 9, 10, 11, 12, 16] {
            let planar = channel_variants(&channel, channels);
            let mut expected = vec![0; channel.len() * channels * 3];
            let mut expected_rngs = dither_rngs(channels);
            encode_interleaved_scalar_from(
                &planar,
                PcmKind::S24,
                false,
                &mut expected_rngs,
                &mut expected,
                0,
            );

            let mut actual_rngs = dither_rngs(channels);
            let mut actual = Vec::new();
            encode_interleaved_with_rngs_into(
                &planar,
                PcmKind::S24,
                false,
                &mut actual_rngs,
                &mut actual,
            );

            assert_eq!(actual, expected, "channels={channels}");
            assert_eq!(actual_rngs, expected_rngs, "channels={channels}");
        }
    }

    fn channel_variants(channel: &[f32], channels: usize) -> Vec<Vec<f32>> {
        (0..channels)
            .map(|index| {
                let mut variant = channel.to_vec();
                let length = variant.len();
                variant.rotate_left((index * 7_919) % length);
                if index % 2 == 1 {
                    variant.reverse();
                }
                variant
            })
            .collect()
    }

    #[test]
    fn caller_owned_decoder_matches_allocating_path_and_reuses_capacity() {
        let planar = vec![
            vec![
                f32::NAN,
                f32::INFINITY,
                -f32::INFINITY,
                -1.0,
                -0.5,
                -0.0,
                0.0,
                0.25,
                0.5,
                1.0,
            ],
            vec![1.0, 0.5, 0.25, 0.0, -0.0, -0.25, -0.5, -1.0, 0.1, -0.1],
        ];
        for kind in [
            PcmKind::U8,
            PcmKind::S16,
            PcmKind::S24,
            PcmKind::S32,
            PcmKind::F32,
            PcmKind::F64,
        ] {
            let encoded = encode_interleaved(&planar, kind, false);
            let expected = decode_planar(&encoded, kind, 2);
            let mut reused = Vec::new();
            decode_planar_into(&encoded, kind, 2, &mut reused);
            assert_eq!(
                reused
                    .iter()
                    .flatten()
                    .map(|sample| sample.to_bits())
                    .collect::<Vec<_>>(),
                expected
                    .iter()
                    .flatten()
                    .map(|sample| sample.to_bits())
                    .collect::<Vec<_>>(),
                "{kind:?}"
            );
            let capacities = reused.iter().map(Vec::capacity).collect::<Vec<_>>();
            decode_planar_into(&encoded, kind, 2, &mut reused);
            assert_eq!(
                reused.iter().map(Vec::capacity).collect::<Vec<_>>(),
                capacities,
                "{kind:?}"
            );
        }
    }

    #[test]
    fn s16_stereo_streaming_simd_matches_scalar_for_all_codes_and_tails() {
        let mut bytes = Vec::with_capacity((u16::MAX as usize + 4) * 4);
        for code in i16::MIN..=i16::MAX {
            bytes.extend_from_slice(&code.to_le_bytes());
            bytes.extend_from_slice(&code.wrapping_neg().wrapping_sub(1).to_le_bytes());
        }
        for frame in 0_i16..4 {
            bytes.extend_from_slice(&frame.to_le_bytes());
            bytes.extend_from_slice(&(-frame).to_le_bytes());
        }

        for frames in (0..=33).chain([4_099, u16::MAX as usize + 1, u16::MAX as usize + 4]) {
            let input = &bytes[..frames * 4];
            let expected = [
                decode_channel(input, PcmKind::S16, 2, 0, frames),
                decode_channel(input, PcmKind::S16, 2, 1, frames),
            ];
            let actual = decode_planar(input, PcmKind::S16, 2);
            assert_eq!(
                actual.as_slice(),
                expected.as_slice(),
                "allocating path, frames={frames}"
            );

            let mut reused = vec![
                Vec::with_capacity(bytes.len()),
                Vec::with_capacity(bytes.len()),
            ];
            let capacities = reused.iter().map(Vec::capacity).collect::<Vec<_>>();
            decode_planar_into(input, PcmKind::S16, 2, &mut reused);
            assert_eq!(
                reused.as_slice(),
                expected.as_slice(),
                "caller-owned path, frames={frames}"
            );
            assert_eq!(
                reused.iter().map(Vec::capacity).collect::<Vec<_>>(),
                capacities,
                "caller-owned capacity, frames={frames}"
            );
        }
    }
}
