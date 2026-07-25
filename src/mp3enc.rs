//! MP3 encoding via LAME (FFI).
//!
//! There is no mature pure-Rust MP3 encoder, so Forge links to LAME — the
//! reference MP3 encoder — through a tiny, hand-written FFI surface (just the
//! handful of C functions we need). `build.rs` locates `libmp3lame`. We feed
//! LAME interleaved IEEE-f32 samples directly (no integer conversion), so the
//! full float precision of the gained signal reaches the encoder.
//!
//! The encoding is CBR by default (transparent and predictable for loudness
//! work); quality and bitrate are configurable.

use crate::wav::AudioBuffer;
use std::ffi::c_void;
use std::os::raw::{c_float, c_int};
use std::path::Path;

// LAME's opaque handle. The C type is `lame_global_flags*`; we treat it as a
// void pointer to stay independent of the struct layout.
type LameT = *mut c_void;

extern "C" {
    fn lame_init() -> LameT;
    fn lame_set_in_samplerate(gfp: LameT, v: c_int) -> c_int;
    fn lame_set_num_channels(gfp: LameT, v: c_int) -> c_int;
    fn lame_set_out_samplerate(gfp: LameT, v: c_int) -> c_int;
    fn lame_set_brate(gfp: LameT, v: c_int) -> c_int;
    // vbr_mode is a C enum (int-sized); vbr_off = 0 means constant bitrate.
    fn lame_set_VBR(gfp: LameT, v: c_int) -> c_int;
    fn lame_set_quality(gfp: LameT, v: c_int) -> c_int;
    fn lame_set_bWriteVbrTag(gfp: LameT, v: c_int) -> c_int;
    fn lame_init_params(gfp: LameT) -> c_int;
    fn lame_encode_buffer_interleaved_ieee_float(
        gfp: LameT,
        pcm: *const c_float,
        nsamples: c_int,
        mp3buf: *mut u8,
        mp3buf_size: c_int,
    ) -> c_int;
    fn lame_encode_flush(gfp: LameT, mp3buf: *mut u8, mp3buf_size: c_int) -> c_int;
    fn lame_close(gfp: LameT) -> c_int;
}

const VBR_OFF: c_int = 0;
const LAME_OKAY: c_int = 0;

/// Encode a planar [`AudioBuffer`] to MP3 bytes (CBR).
pub fn encode_mp3(buf: &AudioBuffer, bitrate_kbps: i32, quality: i32) -> Result<Vec<u8>, String> {
    let channels = buf.channels as usize;
    if channels == 0 || buf.frames == 0 {
        return Err("no audio to encode".into());
    }
    for ch in &buf.data {
        if ch.len() != buf.frames {
            return Err("channel length mismatch".into());
        }
    }

    // Interleave planar -> flat f32 (LAME's interleaved API needs L,R,L,R,...).
    let mut inter = vec![0.0f32; buf.frames * channels];
    for f in 0..buf.frames {
        for c in 0..channels {
            inter[f * channels + c] = buf.data[c][f];
        }
    }

    let gfp = unsafe { lame_init() };
    if gfp.is_null() {
        return Err("lame_init() returned null".into());
    }

    // LAME recommends 1.25 * samples + 7200 bytes for the output buffer. We
    // encode in chunks to keep peak memory modest; size the per-chunk buffer
    // for the worst case.
    const CHUNK_FRAMES: i32 = 8192;
    let mp3buf_size = (1.25 * (CHUNK_FRAMES as f64 * channels as f64) + 7200.0) as usize + 16;
    let mut mp3buf = vec![0u8; mp3buf_size];
    let mut out: Vec<u8> = Vec::with_capacity(buf.frames / 8);

    unsafe {
        lame_set_in_samplerate(gfp, buf.sample_rate as c_int);
        lame_set_num_channels(gfp, channels as c_int);
        // 0 = keep the input sample rate (no resampling).
        lame_set_out_samplerate(gfp, buf.sample_rate as c_int);
        lame_set_brate(gfp, bitrate_kbps as c_int);
        lame_set_VBR(gfp, VBR_OFF);
        // Clamp quality to LAME's 0..=9 range; 0 is best/slowest, 2 is a great default.
        lame_set_quality(gfp, quality.clamp(0, 9));
        // No Xing/LAME info tag — plain CBR stream.
        lame_set_bWriteVbrTag(gfp, 0);

        if lame_init_params(gfp) != LAME_OKAY {
            lame_close(gfp);
            return Err("lame_init_params() failed".into());
        }

        let mut pos: usize = 0;
        let total = buf.frames as i32;
        while (pos as i32) < total {
            let n = CHUNK_FRAMES.min(total - pos as i32);
            let ptr = inter.as_ptr().add(pos * channels);
            let written = lame_encode_buffer_interleaved_ieee_float(
                gfp,
                ptr,
                n,
                mp3buf.as_mut_ptr(),
                mp3buf.len() as c_int,
            );
            if written < 0 {
                lame_close(gfp);
                return Err(format!("lame_encode_buffer error code {written}"));
            }
            out.extend_from_slice(&mp3buf[..written as usize]);
            pos += n as usize;
        }

        let written = lame_encode_flush(gfp, mp3buf.as_mut_ptr(), mp3buf.len() as c_int);
        if written < 0 {
            lame_close(gfp);
            return Err(format!("lame_encode_flush error code {written}"));
        }
        out.extend_from_slice(&mp3buf[..written as usize]);

        lame_close(gfp);
    }

    Ok(out)
}

/// Encode `buf` to MP3 and write it to `path`.
pub fn write_mp3<P: AsRef<Path>>(
    path: P,
    buf: &AudioBuffer,
    bitrate_kbps: i32,
    quality: i32,
) -> Result<(), String> {
    let p = path.as_ref();
    let bytes = encode_mp3(buf, bitrate_kbps, quality)?;
    std::fs::write(p, &bytes).map_err(|e| format!("write {}: {e}", p.display()))
}
