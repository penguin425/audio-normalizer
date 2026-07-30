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
use wav::{default_channel_roles, AudioBuffer, PcmKind, WavReader};

const MAX_INPUT_BYTES: usize = 128 * 1024 * 1024;
const MAX_DECODED_SAMPLES: usize = 24_000_000;
const MAX_CHANNELS: u16 = 32;
const MAX_SAMPLE_RATE: u32 = 768_000;

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

fn validate_buffer(buffer: &AudioBuffer) -> Result<(), JsValue> {
    if buffer.sample_rate == 0 || buffer.sample_rate > MAX_SAMPLE_RATE {
        return Err(JsValue::from_str("sampleRate must be in 1..=768000"));
    }
    if buffer.channels == 0 || buffer.channels > MAX_CHANNELS {
        return Err(JsValue::from_str("channels must be in 1..=32"));
    }
    let samples = buffer
        .frames
        .checked_mul(buffer.channels as usize)
        .ok_or_else(|| JsValue::from_str("decoded sample count overflow"))?;
    if samples > MAX_DECODED_SAMPLES {
        return Err(JsValue::from_str(
            "decoded audio exceeds the 24000000-sample browser safety limit",
        ));
    }
    Ok(())
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
        r#"{{"maxInputBytes":{MAX_INPUT_BYTES},"maxDecodedSamples":{MAX_DECODED_SAMPLES},"maxChannels":{MAX_CHANNELS},"maxSampleRate":{MAX_SAMPLE_RATE}}}"#
    )
}

/// Decode and analyze an in-memory PCM/IEEE-float WAVE, RF64, or BW64 file.
#[wasm_bindgen]
pub fn analyze_wav_json(bytes: &[u8]) -> Result<String, JsValue> {
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(JsValue::from_str(
            "input exceeds the 134217728-byte browser safety limit",
        ));
    }
    let buffer = WavReader::read_bytes_with_limits(bytes, MAX_CHANNELS, MAX_DECODED_SAMPLES)
        .map_err(|error| JsValue::from_str(&format!("could not decode WAVE input: {error}")))?;
    validate_buffer(&buffer)?;
    serialize(&analysis::analyze(&buffer))
}

/// Analyze interleaved Float32 PCM supplied by Web Audio or another decoder.
#[wasm_bindgen]
pub fn analyze_interleaved_json(
    samples: &[f32],
    sample_rate: u32,
    channels: u16,
) -> Result<String, JsValue> {
    if channels == 0 || channels > MAX_CHANNELS {
        return Err(JsValue::from_str("channels must be in 1..=32"));
    }
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
    if samples.iter().any(|sample| !sample.is_finite()) {
        return Err(JsValue::from_str("samples must contain only finite values"));
    }

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
    validate_buffer(&buffer)?;
    serialize(&analysis::analyze(&buffer))
}
