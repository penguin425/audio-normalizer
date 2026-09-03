//! MP3 encoding via LAME (FFI).
//!
//! There is no mature pure-Rust MP3 encoder, so Forge links to LAME — the
//! reference MP3 encoder — through a tiny, hand-written FFI surface (just the
//! handful of C functions we need). `build.rs` locates `libmp3lame`. We feed
//! LAME planar mono or stereo IEEE-f32 samples directly (no interleaving or
//! integer conversion), so the full float precision of the gained signal
//! reaches the encoder.
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
    fn lame_get_brate(gfp: LameT) -> c_int;
    fn lame_get_out_samplerate(gfp: LameT) -> c_int;
    fn lame_close(gfp: LameT) -> c_int;
}

const VBR_OFF: c_int = 0;
const LAME_OKAY: c_int = 0;

fn validate_mp3_configuration(sample_rate: u32, bitrate_kbps: i32) -> Result<(), String> {
    if !matches!(
        sample_rate,
        8_000 | 11_025 | 12_000 | 16_000 | 22_050 | 24_000 | 32_000 | 44_100 | 48_000
    ) {
        return Err(format!(
            "MP3 output sample rate {sample_rate} Hz is unsupported; use 8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, or 48000 Hz"
        ));
    }
    if !(8..=320).contains(&bitrate_kbps) {
        return Err("MP3 bitrate must be between 8 and 320 kbps".into());
    }
    Ok(())
}

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
        validate_mp3_configuration(sample_rate, bitrate_kbps)?;
        let gfp = unsafe { lame_init() };
        if gfp.is_null() {
            return Err("lame_init() returned null".into());
        }
        let (settings_ok, result) = unsafe {
            let settings_ok = lame_set_in_samplerate(gfp, sample_rate as c_int) == LAME_OKAY
                && lame_set_num_channels(gfp, channels as c_int) == LAME_OKAY
                && lame_set_out_samplerate(gfp, sample_rate as c_int) == LAME_OKAY
                && lame_set_brate(gfp, bitrate_kbps) == LAME_OKAY
                && lame_set_VBR(gfp, VBR_OFF) == LAME_OKAY
                && lame_set_quality(gfp, quality.clamp(0, 9)) == LAME_OKAY
                && lame_set_bWriteVbrTag(gfp, 1) == LAME_OKAY;
            (settings_ok, settings_ok.then(|| lame_init_params(gfp)))
        };
        if !settings_ok || result != Some(LAME_OKAY) {
            unsafe {
                lame_close(gfp);
            }
            return Err("configure LAME encoder failed".into());
        }
        let actual_sample_rate = unsafe { lame_get_out_samplerate(gfp) };
        if actual_sample_rate != sample_rate as c_int {
            unsafe {
                lame_close(gfp);
            }
            return Err(format!(
                "LAME selected {actual_sample_rate} Hz instead of requested {sample_rate} Hz"
            ));
        }
        let actual_bitrate = unsafe { lame_get_brate(gfp) };
        if actual_bitrate != bitrate_kbps {
            unsafe {
                lame_close(gfp);
            }
            return Err(format!(
                "LAME selected {actual_bitrate} kbps instead of requested {bitrate_kbps} kbps"
            ));
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
        let required = (1.25 * (frames * self.channels) as f64 + 7200.0) as usize + 16;
        if self.encoded.len() < required {
            self.encoded.resize(required, 0);
        }
        let written = unsafe {
            let right = if self.channels == 1 {
                planar[0].as_ptr()
            } else {
                planar[1].as_ptr()
            };
            lame_encode_buffer_ieee_float(
                self.gfp,
                planar[0].as_ptr(),
                right,
                frames as c_int,
                self.encoded.as_mut_ptr(),
                self.encoded.len() as c_int,
            )
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
    if channels > 2 {
        return Err("MP3 output supports only mono or stereo".into());
    }
    validate_mp3_configuration(buf.sample_rate, bitrate_kbps)?;
    if buf.data.len() != channels {
        return Err("channel count does not match audio planes".into());
    }
    for ch in &buf.data {
        if ch.len() != buf.frames {
            return Err("channel length mismatch".into());
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
        let settings_ok = lame_set_in_samplerate(gfp, buf.sample_rate as c_int) == LAME_OKAY
            && lame_set_num_channels(gfp, channels as c_int) == LAME_OKAY
        // 0 = keep the input sample rate (no resampling).
            && lame_set_out_samplerate(gfp, buf.sample_rate as c_int) == LAME_OKAY
            && lame_set_brate(gfp, bitrate_kbps as c_int) == LAME_OKAY
            && lame_set_VBR(gfp, VBR_OFF) == LAME_OKAY
        // Clamp quality to LAME's 0..=9 range; 0 is best/slowest, 2 is a great default.
            && lame_set_quality(gfp, quality.clamp(0, 9)) == LAME_OKAY
        // Reserve a first-frame Info/LAME tag and backpatch it after flushing.
            && lame_set_bWriteVbrTag(gfp, 1) == LAME_OKAY;

        if !settings_ok || lame_init_params(gfp) != LAME_OKAY {
            lame_close(gfp);
            return Err("configure LAME encoder failed".into());
        }
        let actual_sample_rate = lame_get_out_samplerate(gfp);
        if actual_sample_rate != buf.sample_rate as c_int {
            lame_close(gfp);
            return Err(format!(
                "LAME selected {actual_sample_rate} Hz instead of requested {} Hz",
                buf.sample_rate
            ));
        }
        let actual_bitrate = lame_get_brate(gfp);
        if actual_bitrate != bitrate_kbps {
            lame_close(gfp);
            return Err(format!(
                "LAME selected {actual_bitrate} kbps instead of requested {bitrate_kbps} kbps"
            ));
        }

        let mut pos: usize = 0;
        let total = buf.frames as i32;
        while (pos as i32) < total {
            let n = CHUNK_FRAMES.min(total - pos as i32);
            let left = buf.data[0].as_ptr().add(pos);
            let right = if channels == 1 {
                left
            } else {
                buf.data[1].as_ptr().add(pos)
            };
            let written = lame_encode_buffer_ieee_float(
                gfp,
                left,
                right,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::{default_channel_roles, PcmKind};

    #[test]
    fn encode_rejects_missing_or_extra_planes_before_calling_lame() {
        for (channels, data) in [(2, vec![vec![0.0]]), (1, vec![vec![0.0], vec![0.0]])] {
            let audio = AudioBuffer {
                sample_rate: 48_000,
                channels,
                frames: 1,
                data,
                channel_roles: default_channel_roles(channels),
                source_kind: PcmKind::F32,
            };
            assert_eq!(
                encode_mp3(&audio, 192, 2).unwrap_err(),
                "channel count does not match audio planes"
            );
        }
    }

    #[test]
    fn invalid_configuration_is_rejected_before_output_creation() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("existing.mp3");
        std::fs::write(&destination, b"keep me").unwrap();

        let error = Mp3StreamWriter::create(&destination, 12_345, 2, 192, 2)
            .err()
            .unwrap();
        assert!(error.contains("sample rate"), "{error}");
        assert_eq!(std::fs::read(&destination).unwrap(), b"keep me");

        let error = Mp3StreamWriter::create(&destination, 48_000, 2, 321, 2)
            .err()
            .unwrap();
        assert!(error.contains("bitrate"), "{error}");
        assert_eq!(std::fs::read(&destination).unwrap(), b"keep me");
    }
}
