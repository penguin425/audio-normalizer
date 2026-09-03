//! Browser-only, local analysis API for Forge.
//!
//! The shared modules below deliberately point at Forge's native analysis
//! sources. This keeps LUFS, LRA, RMS, sample-peak, and true-peak results
//! identical across Rust, C, Python, and WebAssembly entry points.

#![allow(dead_code, unused_imports)]
#![allow(clippy::doc_lazy_continuation)]

#[path = "../../src/analysis.rs"]
mod analysis;
#[path = "../../src/dsp/mod.rs"]
mod dsp;
#[path = "../../src/wav/mod.rs"]
mod wav;

use analysis::Analysis;
use serde::Serialize;
use wasm_bindgen::prelude::*;
use wav::{
    default_channel_roles, AudioBuffer, ChannelLayoutProvenance, PcmKind, WavReader,
    MAX_DECODE_SAMPLE_RATE_HZ, MIN_DECODE_SAMPLE_RATE_HZ,
};

const MAX_INPUT_BYTES: usize = 128 * 1024 * 1024;
const MAX_DECODED_SAMPLES: usize = 24_000_000;
const MAX_CHANNELS: u16 = 32;
const MAX_IMPLICIT_LAYOUT_CHANNELS: u16 = 2;
const SAMPLE_RATE_ERROR: &str = "sampleRate must be in 8000..=384000";
const WAVE_LAYOUT_ERROR: &str =
    "WAVE channel layout is unknown; use WAVE_FORMAT_EXTENSIBLE with a complete speaker mask";
const INTERLEAVED_LAYOUT_ERROR: &str = "interleaved audio with more than 2 channels requires an explicit layout; use analyzeWav with WAVE_FORMAT_EXTENSIBLE";
const FINITE_SAMPLES_ERROR: &str = "samples must contain only finite values";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowserAnalysis {
    version: &'static str,
    sample_rate: u32,
    channels: u16,
    frames: usize,
    duration_seconds: f64,
    integrated_lufs: Option<f64>,
    max_momentary_lufs: Option<f64>,
    max_short_term_lufs: Option<f64>,
    loudness_range_lu: f64,
    loudness_range_stable: bool,
    rms_dbfs: Option<f64>,
    sample_peak_dbfs: Option<f64>,
    true_peak_dbtp: Option<f64>,
    peak_to_loudness_ratio_lu: Option<f64>,
}

impl From<&Analysis> for BrowserAnalysis {
    fn from(value: &Analysis) -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION"),
            sample_rate: value.sample_rate,
            channels: value.channels,
            frames: value.frames,
            duration_seconds: value.duration_secs(),
            integrated_lufs: finite(value.lufs),
            max_momentary_lufs: finite(value.max_momentary_lufs),
            max_short_term_lufs: finite(value.max_short_term_lufs),
            loudness_range_lu: value.loudness_range_lu,
            loudness_range_stable: value.loudness_range_stable(),
            rms_dbfs: finite(value.rms_db),
            sample_peak_dbfs: finite(value.sample_peak_db()),
            true_peak_dbtp: finite(value.true_peak_db()),
            peak_to_loudness_ratio_lu: finite(value.peak_to_loudness_ratio_lu()),
        }
    }
}

fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

fn serialize(value: &Analysis) -> Result<String, JsValue> {
    serde_json::to_string(&BrowserAnalysis::from(value))
        .map_err(|error| JsValue::from_str(&format!("could not serialize analysis: {error}")))
}

fn validate_sample_rate(sample_rate: u32) -> Result<(), &'static str> {
    if (MIN_DECODE_SAMPLE_RATE_HZ..=MAX_DECODE_SAMPLE_RATE_HZ).contains(&sample_rate) {
        Ok(())
    } else {
        Err(SAMPLE_RATE_ERROR)
    }
}

fn validate_wave_layout(provenance: ChannelLayoutProvenance) -> Result<(), &'static str> {
    if provenance == ChannelLayoutProvenance::KnownSpeakers {
        Ok(())
    } else {
        Err(WAVE_LAYOUT_ERROR)
    }
}

fn validate_interleaved_channels(channels: u16) -> Result<(), &'static str> {
    match channels {
        1 | 2 => Ok(()),
        0 => Err("channels must be in 1..=32"),
        _ => Err(INTERLEAVED_LAYOUT_ERROR),
    }
}

fn validate_buffer_metadata(buffer: &AudioBuffer) -> Result<(), &'static str> {
    validate_sample_rate(buffer.sample_rate)?;
    if buffer.channels == 0 || buffer.channels > MAX_CHANNELS {
        return Err("channels must be in 1..=32");
    }
    let samples = buffer
        .frames
        .checked_mul(buffer.channels as usize)
        .ok_or("decoded sample count overflow")?;
    if samples > MAX_DECODED_SAMPLES {
        return Err("decoded audio exceeds the 24000000-sample browser safety limit");
    }
    Ok(())
}

fn validate_finite_samples<'a>(
    samples: impl IntoIterator<Item = &'a f32>,
) -> Result<(), &'static str> {
    if samples.into_iter().any(|sample| !sample.is_finite()) {
        Err(FINITE_SAMPLES_ERROR)
    } else {
        Ok(())
    }
}

fn validate_buffer(buffer: &AudioBuffer) -> Result<(), &'static str> {
    validate_buffer_metadata(buffer)?;
    validate_finite_samples(buffer.data.iter().flatten())
}

fn decode_wav_input(bytes: &[u8]) -> Result<AudioBuffer, String> {
    if bytes.len() > MAX_INPUT_BYTES {
        return Err("input exceeds the 134217728-byte browser safety limit".into());
    }
    let (buffer, layout_provenance) =
        WavReader::read_bytes_with_layout_and_limits(bytes, MAX_CHANNELS, MAX_DECODED_SAMPLES)
            .map_err(|error| format!("could not decode WAVE input: {error}"))?;
    validate_wave_layout(layout_provenance).map_err(str::to_owned)?;
    validate_buffer(&buffer).map_err(str::to_owned)?;
    Ok(buffer)
}

/// Forge package version.
#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

/// Fixed resource limits enforced before analysis.
#[wasm_bindgen]
pub fn limits_json() -> String {
    format!(
        r#"{{"maxInputBytes":{MAX_INPUT_BYTES},"maxDecodedSamples":{MAX_DECODED_SAMPLES},"maxChannels":{MAX_CHANNELS},"maxImplicitLayoutChannels":{MAX_IMPLICIT_LAYOUT_CHANNELS},"minSampleRate":{MIN_DECODE_SAMPLE_RATE_HZ},"maxSampleRate":{MAX_DECODE_SAMPLE_RATE_HZ},"requiresCompleteWaveLayout":true}}"#
    )
}

/// Decode and analyze an in-memory PCM/IEEE-float WAVE, RF64, or BW64 file.
#[wasm_bindgen]
pub fn analyze_wav_json(bytes: &[u8]) -> Result<String, JsValue> {
    let buffer = decode_wav_input(bytes).map_err(|error| JsValue::from_str(&error))?;
    serialize(&analysis::analyze(&buffer))
}

/// Analyze interleaved Float32 PCM supplied by Web Audio or another decoder.
#[wasm_bindgen]
pub fn analyze_interleaved_json(
    samples: &[f32],
    sample_rate: u32,
    channels: u16,
) -> Result<String, JsValue> {
    validate_sample_rate(sample_rate).map_err(JsValue::from_str)?;
    validate_interleaved_channels(channels).map_err(JsValue::from_str)?;
    if samples.len() > MAX_DECODED_SAMPLES {
        return Err(JsValue::from_str(
            "decoded audio exceeds the 24000000-sample browser safety limit",
        ));
    }
    if !samples.len().is_multiple_of(channels as usize) {
        return Err(JsValue::from_str(
            "interleaved sample count must be divisible by channels",
        ));
    }
    validate_finite_samples(samples).map_err(JsValue::from_str)?;

    let frames = samples.len() / channels as usize;
    let mut planar = (0..channels)
        .map(|_| Vec::with_capacity(frames))
        .collect::<Vec<_>>();
    for frame in samples.chunks_exact(channels as usize) {
        for (channel, sample) in planar.iter_mut().zip(frame) {
            channel.push(*sample);
        }
    }
    let buffer = AudioBuffer {
        sample_rate,
        channels,
        frames,
        data: planar,
        channel_roles: default_channel_roles(channels),
        source_kind: PcmKind::F32,
    };
    validate_buffer_metadata(&buffer).map_err(JsValue::from_str)?;
    serialize(&analysis::analyze(&buffer))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ieee_float_wave(samples: &[f32]) -> Vec<u8> {
        let data_size = u32::try_from(samples.len() * std::mem::size_of::<f32>()).unwrap();
        let mut wave = b"RIFF".to_vec();
        wave.extend_from_slice(&(36 + data_size).to_le_bytes());
        wave.extend_from_slice(b"WAVEfmt ");
        wave.extend_from_slice(&16_u32.to_le_bytes());
        wave.extend_from_slice(&3_u16.to_le_bytes()); // WAVE_FORMAT_IEEE_FLOAT
        wave.extend_from_slice(&1_u16.to_le_bytes());
        wave.extend_from_slice(&48_000_u32.to_le_bytes());
        wave.extend_from_slice(&(48_000_u32 * 4).to_le_bytes());
        wave.extend_from_slice(&4_u16.to_le_bytes());
        wave.extend_from_slice(&32_u16.to_le_bytes());
        wave.extend_from_slice(b"data");
        wave.extend_from_slice(&data_size.to_le_bytes());
        for sample in samples {
            wave.extend_from_slice(&sample.to_le_bytes());
        }
        wave
    }

    #[test]
    fn interleaved_sample_rate_bounds_match_shared_decoder_limits() {
        for sample_rate in [7_999, 384_001] {
            assert_eq!(validate_sample_rate(sample_rate), Err(SAMPLE_RATE_ERROR));
        }
        for sample_rate in [8_000, 384_000] {
            assert_eq!(validate_sample_rate(sample_rate), Ok(()));
        }
    }

    #[test]
    fn raw_interleaved_requires_an_explicit_layout_above_stereo() {
        assert_eq!(validate_interleaved_channels(1), Ok(()));
        assert_eq!(validate_interleaved_channels(2), Ok(()));
        assert_eq!(
            validate_interleaved_channels(3),
            Err(INTERLEAVED_LAYOUT_ERROR)
        );
    }

    #[test]
    fn wave_analysis_requires_known_speakers() {
        assert_eq!(
            validate_wave_layout(ChannelLayoutProvenance::KnownSpeakers),
            Ok(())
        );
        for provenance in [
            ChannelLayoutProvenance::Unknown,
            ChannelLayoutProvenance::SceneBased,
        ] {
            assert_eq!(validate_wave_layout(provenance), Err(WAVE_LAYOUT_ERROR));
        }
    }

    #[test]
    fn wave_analysis_rejects_ieee_float_nan() {
        let error = decode_wav_input(&ieee_float_wave(&[0.0, f32::NAN])).unwrap_err();
        assert_eq!(error, FINITE_SAMPLES_ERROR);
    }

    #[test]
    fn wave_analysis_rejects_ieee_float_infinities() {
        for sample in [f32::INFINITY, f32::NEG_INFINITY] {
            let error = decode_wav_input(&ieee_float_wave(&[0.0, sample])).unwrap_err();
            assert_eq!(error, FINITE_SAMPLES_ERROR);
        }
    }
}
