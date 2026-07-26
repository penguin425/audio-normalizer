//! MP3 encoding via LAME (FFI).
//!
//! There is no mature pure-Rust MP3 encoder, so Forge links to LAME — the
//! reference MP3 encoder — through a tiny, hand-written FFI surface (just the
//! handful of C functions we need). `build.rs` locates `libmp3lame`. We feed
//! LAME planar mono or interleaved stereo IEEE-f32 samples directly (no integer
//! conversion), so the full float precision of the gained signal reaches the
//! encoder.
//!
//! The encoding is CBR by default (transparent and predictable for loudness
//! work); quality and bitrate are configurable.

use crate::wav::AudioBuffer;
use std::ffi::c_void;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
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
    fn lame_encode_buffer_ieee_float(
        gfp: LameT,
        pcm_l: *const c_float,
        pcm_r: *const c_float,
        nsamples: c_int,
        mp3buf: *mut u8,
        mp3buf_size: c_int,
    ) -> c_int;
    fn lame_encode_flush(gfp: LameT, mp3buf: *mut u8, mp3buf_size: c_int) -> c_int;
    fn lame_get_lametag_frame(gfp: LameT, buffer: *mut u8, size: usize) -> usize;
    fn lame_close(gfp: LameT) -> c_int;
}

const VBR_OFF: c_int = 0;
const LAME_OKAY: c_int = 0;

pub struct Mp3StreamWriter {
    gfp: LameT,
    channels: usize,
    output: File,
    encoded: Vec<u8>,
}

impl Mp3StreamWriter {
    pub fn create(
        path: &Path,
        sample_rate: u32,
        channels: u16,
        bitrate_kbps: i32,
        quality: i32,
    ) -> Result<Self, String> {
        if !(1..=2).contains(&channels) {
            return Err("MP3 output supports only mono or stereo".into());
        }
        let gfp = unsafe { lame_init() };
        if gfp.is_null() {
            return Err("lame_init() returned null".into());
        }
        let result = unsafe {
            lame_set_in_samplerate(gfp, sample_rate as c_int);
            lame_set_num_channels(gfp, channels as c_int);
            lame_set_out_samplerate(gfp, sample_rate as c_int);
            lame_set_brate(gfp, bitrate_kbps);
            lame_set_VBR(gfp, VBR_OFF);
            lame_set_quality(gfp, quality.clamp(0, 9));
            lame_set_bWriteVbrTag(gfp, 1);
            lame_init_params(gfp)
        };
        if result != LAME_OKAY {
            unsafe {
                lame_close(gfp);
            }
            return Err("lame_init_params() failed".into());
        }
        let output =
            File::create(path).map_err(|error| format!("create {}: {error}", path.display()))?;
        Ok(Self {
            gfp,
            channels: channels as usize,
            output,
            encoded: vec![0; 32_768],
        })
    }

    pub fn write_chunk(&mut self, planar: &[Vec<f32>]) -> Result<(), String> {
        if planar.len() != self.channels {
            return Err("MP3 stream channel count changed".into());
        }
        let frames = planar.first().map_or(0, Vec::len);
        if planar.iter().any(|channel| channel.len() != frames) {
            return Err("MP3 stream channel length mismatch".into());
        }
        let interleaved = if self.channels == 2 {
            let mut samples = Vec::with_capacity(frames * self.channels);
            for frame in 0..frames {
                for channel in planar {
                    samples.push(channel[frame]);
                }
            }
            samples
        } else {
            Vec::new()
        };
        let required = (1.25 * (frames * self.channels) as f64 + 7200.0) as usize + 16;
        if self.encoded.len() < required {
            self.encoded.resize(required, 0);
        }
        let written = unsafe {
            if self.channels == 1 {
                lame_encode_buffer_ieee_float(
                    self.gfp,
                    planar[0].as_ptr(),
                    planar[0].as_ptr(),
                    frames as c_int,
                    self.encoded.as_mut_ptr(),
                    self.encoded.len() as c_int,
                )
            } else {
                lame_encode_buffer_interleaved_ieee_float(
                    self.gfp,
                    interleaved.as_ptr(),
                    frames as c_int,
                    self.encoded.as_mut_ptr(),
                    self.encoded.len() as c_int,
                )
            }
        };
        if written < 0 {
            return Err(format!("lame_encode_buffer error code {written}"));
        }
        self.output
            .write_all(&self.encoded[..written as usize])
            .map_err(|error| format!("write MP3: {error}"))
    }

    pub fn finish(mut self) -> Result<(), String> {
        let written = unsafe {
            lame_encode_flush(
                self.gfp,
                self.encoded.as_mut_ptr(),
                self.encoded.len() as c_int,
            )
        };
        if written < 0 {
            return Err(format!("lame_encode_flush error code {written}"));
        }
        self.output
            .write_all(&self.encoded[..written as usize])
            .map_err(|error| format!("write MP3: {error}"))?;
        let tag_size = unsafe {
            lame_get_lametag_frame(self.gfp, self.encoded.as_mut_ptr(), self.encoded.len())
        };
        if tag_size > self.encoded.len() {
            return Err(format!("LAME tag requires {tag_size} bytes"));
        }
        if tag_size > 0 {
            self.output
                .seek(SeekFrom::Start(0))
                .and_then(|_| self.output.write_all(&self.encoded[..tag_size]))
                .map_err(|error| format!("write MP3 LAME tag: {error}"))?;
        }
        self.output
            .flush()
            .map_err(|error| format!("flush MP3: {error}"))?;
        unsafe {
            lame_close(self.gfp);
        }
        self.gfp = std::ptr::null_mut();
        Ok(())
    }
}

impl Drop for Mp3StreamWriter {
    fn drop(&mut self) {
        if !self.gfp.is_null() {
            unsafe {
                lame_close(self.gfp);
            }
        }
    }
}

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
        // Reserve a first-frame Info/LAME tag and backpatch it after flushing.
        lame_set_bWriteVbrTag(gfp, 1);

        if lame_init_params(gfp) != LAME_OKAY {
            lame_close(gfp);
            return Err("lame_init_params() failed".into());
        }

        let mut pos: usize = 0;
        let total = buf.frames as i32;
        while (pos as i32) < total {
            let n = CHUNK_FRAMES.min(total - pos as i32);
            let written = if channels == 1 {
                let ptr = buf.data[0].as_ptr().add(pos);
                lame_encode_buffer_ieee_float(
                    gfp,
                    ptr,
                    ptr,
                    n,
                    mp3buf.as_mut_ptr(),
                    mp3buf.len() as c_int,
                )
            } else {
                let ptr = inter.as_ptr().add(pos * channels);
                lame_encode_buffer_interleaved_ieee_float(
                    gfp,
                    ptr,
                    n,
                    mp3buf.as_mut_ptr(),
                    mp3buf.len() as c_int,
                )
            };
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

        let tag_size = lame_get_lametag_frame(gfp, mp3buf.as_mut_ptr(), mp3buf.len());
        if tag_size > mp3buf.len() || tag_size > out.len() {
            lame_close(gfp);
            return Err(format!("invalid LAME tag size {tag_size}"));
        }
        if tag_size > 0 {
            out[..tag_size].copy_from_slice(&mp3buf[..tag_size]);
        }

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
