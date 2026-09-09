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

use crate::atomic::AtomicOutput;
use crate::wav::AudioBuffer;
use std::ffi::{c_char, c_void, CStr};
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
    /// Return LAME's exact runtime version string (for example `3.100`).
    fn get_lame_version() -> *const c_char;
    fn lame_close(gfp: LameT) -> c_int;
}

const VBR_OFF: c_int = 0;
const LAME_OKAY: c_int = 0;
const ENCODE_CHUNK_FRAMES: usize = 8192;

/// Revision of Forge's MP3 writer pipeline represented by runtime evidence.
///
/// Bump this when the writer's LAME configuration or byte/container handling
/// changes in a way that can alter normalized output.
pub const MP3_WRITER_PIPELINE_REVISION: &str = "forge-mp3-writer-v1";

/// Read the exact version string exported by the linked LAME runtime.
///
/// This is intentionally obtained from LAME itself rather than from a build
/// dependency version.  A binary may be linked against a different runtime
/// image on another host, and that distinction belongs in a semantic job
/// fingerprint.
pub fn lame_runtime_version() -> Result<String, String> {
    let version = unsafe { get_lame_version() };
    if version.is_null() {
        return Err("LAME returned a null runtime version string".into());
    }
    unsafe { CStr::from_ptr(version) }
        .to_str()
        .map(str::to_owned)
        .map_err(|_| "LAME runtime version string is not valid UTF-8".into())
}

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

fn validate_mp3_buffer(buf: &AudioBuffer, bitrate_kbps: i32) -> Result<usize, String> {
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
    Ok(channels)
}

struct Mp3Encoder {
    gfp: LameT,
    channels: usize,
    encoded: Vec<u8>,
}

impl Mp3Encoder {
    fn create(
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
        Ok(Self {
            gfp,
            channels: channels as usize,
            encoded: vec![0; 32_768],
        })
    }

    fn encode_planar_chunk(&mut self, left: &[f32], right: &[f32]) -> Result<usize, String> {
        if left.len() != right.len() {
            return Err("MP3 stream channel length mismatch".into());
        }
        let frames = left.len();
        let required = (1.25 * (frames * self.channels) as f64 + 7200.0) as usize + 16;
        if self.encoded.len() < required {
            self.encoded.resize(required, 0);
        }
        let written = unsafe {
            lame_encode_buffer_ieee_float(
                self.gfp,
                left.as_ptr(),
                right.as_ptr(),
                frames as c_int,
                self.encoded.as_mut_ptr(),
                self.encoded.len() as c_int,
            )
        };
        if written < 0 {
            return Err(format!("lame_encode_buffer error code {written}"));
        }
        Ok(written as usize)
    }

    fn flush(&mut self) -> Result<usize, String> {
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
        Ok(written as usize)
    }

    fn lametag_size(&mut self) -> Result<usize, String> {
        let tag_size = unsafe {
            lame_get_lametag_frame(self.gfp, self.encoded.as_mut_ptr(), self.encoded.len())
        };
        if tag_size > self.encoded.len() {
            return Err(format!("LAME tag requires {tag_size} bytes"));
        }
        Ok(tag_size)
    }
}

impl Drop for Mp3Encoder {
    fn drop(&mut self) {
        if !self.gfp.is_null() {
            unsafe {
                lame_close(self.gfp);
            }
            self.gfp = std::ptr::null_mut();
        }
    }
}

/// Incremental MP3 output backed by the shared internal encoder pipeline.
pub struct Mp3StreamWriter {
    encoder: Mp3Encoder,
    output: File,
}

impl Mp3StreamWriter {
    pub fn create(
        path: &Path,
        sample_rate: u32,
        channels: u16,
        bitrate_kbps: i32,
        quality: i32,
    ) -> Result<Self, String> {
        let encoder = Mp3Encoder::create(sample_rate, channels, bitrate_kbps, quality)?;
        let output = File::create(path).map_err(|error| {
            // `encoder` owns the LAME handle and closes it if opening the
            // destination fails.
            format!("create {}: {error}", path.display())
        })?;
        Ok(Self { encoder, output })
    }

    pub fn write_chunk(&mut self, planar: &[Vec<f32>]) -> Result<(), String> {
        let frames = planar.first().map_or(0, Vec::len);
        self.write_chunk_range(planar, 0, frames)
    }

    fn write_chunk_range(
        &mut self,
        planar: &[Vec<f32>],
        start: usize,
        end: usize,
    ) -> Result<(), String> {
        if planar.len() != self.encoder.channels {
            return Err("MP3 stream channel count changed".into());
        }
        let total_frames = planar.first().map_or(0, Vec::len);
        if planar.iter().any(|channel| channel.len() != total_frames) {
            return Err("MP3 stream channel length mismatch".into());
        }
        if start > end || end > total_frames {
            return Err("MP3 stream chunk range is out of bounds".into());
        }
        let left = &planar[0][start..end];
        let right = if self.encoder.channels == 1 {
            left
        } else {
            &planar[1][start..end]
        };
        let written = self.encoder.encode_planar_chunk(left, right)?;
        self.output
            .write_all(&self.encoder.encoded[..written])
            .map_err(|error| format!("write MP3: {error}"))
    }

    pub fn finish(mut self) -> Result<(), String> {
        let written = self.encoder.flush()?;
        self.output
            .write_all(&self.encoder.encoded[..written])
            .map_err(|error| format!("write MP3: {error}"))?;
        let tag_size = self.encoder.lametag_size()?;
        if tag_size > 0 {
            self.output
                .seek(SeekFrom::Start(0))
                .and_then(|_| self.output.write_all(&self.encoder.encoded[..tag_size]))
                .map_err(|error| format!("write MP3 LAME tag: {error}"))?;
        }
        self.output
            .flush()
            .map_err(|error| format!("flush MP3: {error}"))?;
        Ok(())
    }
}

/// Encode a planar [`AudioBuffer`] to MP3 bytes (CBR).
pub fn encode_mp3(buf: &AudioBuffer, bitrate_kbps: i32, quality: i32) -> Result<Vec<u8>, String> {
    let channels = validate_mp3_buffer(buf, bitrate_kbps)?;
    let mut encoder = Mp3Encoder::create(buf.sample_rate, channels as u16, bitrate_kbps, quality)?;
    let mut out: Vec<u8> = Vec::with_capacity(buf.frames / 8);

    let mut pos = 0;
    while pos < buf.frames {
        let end = (pos + ENCODE_CHUNK_FRAMES).min(buf.frames);
        let left = &buf.data[0][pos..end];
        let right = if channels == 1 {
            left
        } else {
            &buf.data[1][pos..end]
        };
        let written = encoder.encode_planar_chunk(left, right)?;
        out.extend_from_slice(&encoder.encoded[..written]);
        pos = end;
    }

    let written = encoder.flush()?;
    out.extend_from_slice(&encoder.encoded[..written]);
    let tag_size = encoder.lametag_size()?;
    if tag_size > out.len() {
        return Err(format!("invalid LAME tag size {tag_size}"));
    }
    if tag_size > 0 {
        out[..tag_size].copy_from_slice(&encoder.encoded[..tag_size]);
    }
    Ok(out)
}

/// Encode `buf` to MP3 and write it to `path`.
///
/// The public one-shot route deliberately uses this stream writer with the
/// same fixed chunking as [`encode_mp3`], so runtime evidence naming the
/// stream writer covers both normalization entry points.
pub fn write_mp3<P: AsRef<Path>>(
    path: P,
    buf: &AudioBuffer,
    bitrate_kbps: i32,
    quality: i32,
) -> Result<(), String> {
    let p = path.as_ref();
    let channels = validate_mp3_buffer(buf, bitrate_kbps)?;
    let staged = AtomicOutput::new(p)?;
    let mut writer = Mp3StreamWriter::create(
        staged.path(),
        buf.sample_rate,
        channels as u16,
        bitrate_kbps,
        quality,
    )?;
    let mut pos = 0;
    while pos < buf.frames {
        let end = (pos + ENCODE_CHUNK_FRAMES).min(buf.frames);
        writer.write_chunk_range(&buf.data, pos, end)?;
        pos = end;
    }
    writer.finish()?;
    staged.commit()
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

    #[test]
    fn one_shot_write_matches_the_shared_stream_pipeline() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("output.mp3");
        let frames = ENCODE_CHUNK_FRAMES * 2 + 137;
        let audio = AudioBuffer {
            sample_rate: 44_100,
            channels: 1,
            frames,
            data: vec![vec![0.0; frames]],
            channel_roles: default_channel_roles(1),
            source_kind: PcmKind::F32,
        };

        let expected = encode_mp3(&audio, 192, 2).unwrap();
        write_mp3(&destination, &audio, 192, 2).unwrap();

        assert_eq!(std::fs::read(destination).unwrap(), expected);
    }
}
