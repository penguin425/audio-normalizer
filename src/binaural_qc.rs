//! Auditable verification of externally rendered binaural output.
//!
//! Forge does not bundle an HRTF or object renderer.  This module therefore
//! treats the renderer as an explicitly selected external dependency and
//! records its identity (including immutable SHA-256 evidence) in every
//! report.  It measures the source, the renderer output, and an optional
//! trusted reference render; it never attempts to infer a binaural render from
//! channel-based PCM.

use crate::downmix::Layout;
use crate::normalize;
use crate::wav::AudioBuffer;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: u32 = 1;
pub const VALIDATOR: &str = "forge-binaural-qc-1";
pub const DEFAULT_MAX_INPUT_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_MAX_DECODED_SAMPLES: u64 = 24 * 1024 * 1024;
pub const DEFAULT_DURATION_TOLERANCE_SECONDS: f64 = 0.001;
pub const DEFAULT_LOUDNESS_TOLERANCE_LU: f64 = 1.0;
pub const DEFAULT_TRUE_PEAK_TOLERANCE_DB: f64 = 1.0;

/// A renderer identity is required even when no reference file is supplied.
/// The hashes are evidence supplied by the caller and are deliberately not
/// interpreted as a claim that Forge executed the renderer.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RendererEvidence {
    pub name: String,
    pub version: String,
    pub renderer_sha256: String,
    pub model: String,
    pub model_version: String,
    pub model_sha256: String,
    #[serde(default)]
    pub config_sha256: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BinauralQcSpec {
    pub schema_version: u32,
    pub source: PathBuf,
    pub rendered: PathBuf,
    #[serde(default)]
    pub reference: Option<PathBuf>,
    pub input_layout: String,
    pub renderer: RendererEvidence,
    #[serde(default = "default_duration_tolerance")]
    pub max_duration_delta_seconds: f64,
    #[serde(default = "default_loudness_tolerance")]
    pub max_loudness_delta_lu: f64,
    #[serde(default = "default_true_peak_tolerance")]
    pub max_true_peak_delta_db: f64,
    #[serde(default = "default_true_peak_ceiling")]
    pub true_peak_ceiling_dbtp: f64,
    #[serde(default)]
    pub max_clipped_samples: u64,
    #[serde(default = "default_max_input_bytes")]
    pub max_input_bytes: u64,
    #[serde(default = "default_max_decoded_samples")]
    pub max_decoded_samples: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BinauralQcReport {
    pub schema_version: u32,
    pub validator: &'static str,
    pub source: String,
    pub rendered: String,
    pub reference: Option<String>,
    pub input_layout: &'static str,
    pub output_layout: &'static str,
    pub renderer: RendererEvidence,
    pub source_measurement: Measurement,
    pub rendered_measurement: Measurement,
    pub reference_measurement: Option<Measurement>,
    pub source_duration_delta_seconds: f64,
    pub source_duration_passed: bool,
    pub reference_drift: Option<ReferenceDrift>,
    pub true_peak_ceiling_dbtp: f64,
    pub max_clipped_samples: u64,
    pub rendered_clipped_samples: u64,
    pub rendered_maximum_sample: f32,
    pub rendered_clip_risk: ClipRisk,
    pub clip_risk_passed: bool,
    pub passed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Measurement {
    pub layout: &'static str,
    pub channels: u16,
    pub sample_rate_hz: u32,
    pub frames: usize,
    pub duration_seconds: f64,
    pub integrated_lufs: Option<f64>,
    pub loudness_range_lu: Option<f64>,
    pub sample_peak_dbfs: Option<f64>,
    pub true_peak_dbtp: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReferenceDrift {
    pub loudness_delta_lu: Option<f64>,
    pub true_peak_delta_db: Option<f64>,
    pub duration_delta_seconds: f64,
    pub duration_tolerance_seconds: f64,
    pub loudness_passed: bool,
    pub true_peak_passed: bool,
    pub duration_passed: bool,
    pub passed: bool,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ClipRisk {
    None,
    TruePeakCeiling,
    SampleClipping,
}

pub fn evaluate_file(path: &Path) -> Result<BinauralQcReport, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("read binaural QC spec {}: {error}", path.display()))?;
    let spec: BinauralQcSpec = match path.extension().and_then(|value| value.to_str()) {
        Some("json") => serde_json::from_str(&text)
            .map_err(|error| format!("parse binaural QC JSON: {error}"))?,
        Some("toml") => {
            toml::from_str(&text).map_err(|error| format!("parse binaural QC TOML: {error}"))?
        }
        _ => return Err("binaural QC spec must use .json or .toml".into()),
    };
    evaluate(path, spec)
}

pub fn evaluate(path: &Path, spec: BinauralQcSpec) -> Result<BinauralQcReport, String> {
    validate_spec(&spec)?;
    let input_layout = Layout::parse(&spec.input_layout)
        .ok_or_else(|| format!("unsupported input layout {}", spec.input_layout))?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let source_path = resolve(base, &spec.source);
    let rendered_path = resolve(base, &spec.rendered);
    let reference_path = spec.reference.as_ref().map(|value| resolve(base, value));
    for candidate in std::iter::once(&source_path)
        .chain(std::iter::once(&rendered_path))
        .chain(reference_path.iter())
    {
        enforce_input_bytes(candidate, spec.max_input_bytes)?;
    }
    let mut source = crate::decoder::decode_limited(&source_path, spec.max_decoded_samples)
        .map_err(|error| format!("decode binaural source {}: {error}", source_path.display()))?;
    if source.channels as usize != input_layout.channels() {
        return Err(format!(
            "input layout {} requires {} channels, decoded {}",
            input_layout.as_str(),
            input_layout.channels(),
            source.channels
        ));
    }
    source.channel_roles = input_layout.roles();
    let source_analysis = normalize::analyze(&source);

    let mut rendered = decode_audio(&rendered_path, spec.max_decoded_samples, "rendered")?;
    if rendered.channels != 2 {
        return Err(format!(
            "binaural renderer output must be stereo (2 channels), decoded {}",
            rendered.channels
        ));
    }
    rendered.channel_roles = Layout::Stereo.roles();
    let rendered_analysis = normalize::analyze(&rendered);
    let reference = reference_path
        .as_deref()
        .map(|path| decode_audio(path, spec.max_decoded_samples, "reference"))
        .transpose()?;
    let (reference_analysis, reference_measurement) = match reference {
        Some(mut audio) => {
            if audio.channels != 2 {
                return Err(format!(
                    "binaural reference must be stereo (2 channels), decoded {}",
                    audio.channels
                ));
            }
            audio.channel_roles = Layout::Stereo.roles();
            let analysis = normalize::analyze(&audio);
            let measurement = measurement("binaural", &analysis);
            (Some(analysis), Some(measurement))
        }
        None => (None, None),
    };
    let source_duration_delta = rendered_analysis.duration_secs() - source_analysis.duration_secs();
    let source_duration_passed = source_duration_delta.abs() <= spec.max_duration_delta_seconds;
    let (rendered_clipped_samples, rendered_maximum_sample) = clipping(&rendered);
    let rendered_clip_risk = if rendered_clipped_samples > 0 {
        ClipRisk::SampleClipping
    } else if rendered_analysis.true_peak_db().is_finite()
        && rendered_analysis.true_peak_db() > spec.true_peak_ceiling_dbtp
    {
        ClipRisk::TruePeakCeiling
    } else {
        ClipRisk::None
    };
    let clip_risk_passed = rendered_clipped_samples <= spec.max_clipped_samples
        && (!rendered_analysis.true_peak_db().is_finite()
            || rendered_analysis.true_peak_db() <= spec.true_peak_ceiling_dbtp);
    let reference_drift = reference_analysis.map(|reference| {
        let loudness_delta = delta(rendered_analysis.lufs, reference.lufs);
        let true_peak_delta = delta(rendered_analysis.true_peak_db(), reference.true_peak_db());
        let duration_delta = rendered_analysis.duration_secs() - reference.duration_secs();
        let loudness_passed =
            loudness_delta.is_some_and(|value| value.abs() <= spec.max_loudness_delta_lu);
        let true_peak_passed =
            true_peak_delta.is_some_and(|value| value.abs() <= spec.max_true_peak_delta_db);
        let duration_passed = duration_delta.abs() <= spec.max_duration_delta_seconds;
        ReferenceDrift {
            loudness_delta_lu: loudness_delta,
            true_peak_delta_db: true_peak_delta,
            duration_delta_seconds: duration_delta,
            duration_tolerance_seconds: spec.max_duration_delta_seconds,
            loudness_passed,
            true_peak_passed,
            duration_passed,
            passed: loudness_passed && true_peak_passed && duration_passed,
        }
    });
    let passed = source_duration_passed
        && clip_risk_passed
        && reference_drift.as_ref().is_none_or(|value| value.passed);
    Ok(BinauralQcReport {
        schema_version: SCHEMA_VERSION,
        validator: VALIDATOR,
        source: source_path.to_string_lossy().into_owned(),
        rendered: rendered_path.to_string_lossy().into_owned(),
        reference: reference_path.map(|value| value.to_string_lossy().into_owned()),
        input_layout: input_layout.as_str(),
        output_layout: "binaural",
        renderer: spec.renderer,
        source_measurement: measurement(input_layout.as_str(), &source_analysis),
        rendered_measurement: measurement("binaural", &rendered_analysis),
        reference_measurement,
        source_duration_delta_seconds: source_duration_delta,
        source_duration_passed,
        reference_drift,
        true_peak_ceiling_dbtp: spec.true_peak_ceiling_dbtp,
        max_clipped_samples: spec.max_clipped_samples,
        rendered_clipped_samples,
        rendered_maximum_sample,
        rendered_clip_risk,
        clip_risk_passed,
        passed,
    })
}

fn decode_audio(path: &Path, max_decoded_samples: u64, label: &str) -> Result<AudioBuffer, String> {
    crate::decoder::decode_limited(path, max_decoded_samples)
        .map_err(|error| format!("decode binaural {label} {}: {error}", path.display()))
}

fn measurement(layout: &'static str, analysis: &crate::analysis::Analysis) -> Measurement {
    Measurement {
        layout,
        channels: analysis.channels,
        sample_rate_hz: analysis.sample_rate,
        frames: analysis.frames,
        duration_seconds: analysis.duration_secs(),
        integrated_lufs: finite(analysis.lufs),
        loudness_range_lu: finite(analysis.loudness_range_lu),
        sample_peak_dbfs: finite(analysis.sample_peak_db()),
        true_peak_dbtp: finite(analysis.true_peak_db()),
    }
}

fn clipping(buffer: &AudioBuffer) -> (u64, f32) {
    let mut count = 0_u64;
    let mut maximum = 0.0_f32;
    for channel in &buffer.data {
        for sample in channel {
            maximum = maximum.max(sample.abs());
            if sample.abs() > 1.0 {
                count = count.saturating_add(1);
            }
        }
    }
    (count, maximum)
}

fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

fn delta(measured: f64, reference: f64) -> Option<f64> {
    (measured.is_finite() && reference.is_finite()).then_some(measured - reference)
}

fn enforce_input_bytes(path: &Path, max_input_bytes: u64) -> Result<(), String> {
    let bytes = fs::metadata(path)
        .map_err(|error| format!("stat binaural input {}: {error}", path.display()))?
        .len();
    if bytes > max_input_bytes {
        return Err(format!(
            "binaural input {} is {bytes} bytes, above max_input_bytes {}",
            path.display(),
            max_input_bytes
        ));
    }
    Ok(())
}

fn validate_spec(spec: &BinauralQcSpec) -> Result<(), String> {
    if spec.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported binaural QC schema {}; expected {SCHEMA_VERSION}",
            spec.schema_version
        ));
    }
    if spec.source.as_os_str().is_empty() || spec.rendered.as_os_str().is_empty() {
        return Err("binaural source and rendered paths are required".into());
    }
    let input_layout = Layout::parse(&spec.input_layout)
        .ok_or_else(|| format!("unsupported input layout {}", spec.input_layout))?;
    if input_layout == Layout::Mono || input_layout == Layout::Stereo {
        return Err("binaural source must use a multichannel immersive input layout".into());
    }
    let renderer = &spec.renderer;
    if renderer.name.trim().is_empty()
        || renderer.version.trim().is_empty()
        || renderer.model.trim().is_empty()
        || renderer.model_version.trim().is_empty()
    {
        return Err("renderer name/version and model name/version are required".into());
    }
    for (field, value) in [
        ("renderer_sha256", renderer.renderer_sha256.as_str()),
        ("model_sha256", renderer.model_sha256.as_str()),
    ] {
        if !is_sha256(value) {
            return Err(format!(
                "{field} must be exactly 64 lowercase hexadecimal characters"
            ));
        }
    }
    if let Some(value) = renderer.config_sha256.as_deref() {
        if !is_sha256(value) {
            return Err("config_sha256 must be exactly 64 lowercase hexadecimal characters".into());
        }
    }
    for (name, value) in [
        (
            "max_duration_delta_seconds",
            spec.max_duration_delta_seconds,
        ),
        ("max_loudness_delta_lu", spec.max_loudness_delta_lu),
        ("max_true_peak_delta_db", spec.max_true_peak_delta_db),
    ] {
        if !value.is_finite() || value < 0.0 {
            return Err(format!("{name} must be finite and non-negative"));
        }
    }
    if !spec.true_peak_ceiling_dbtp.is_finite()
        || !(-120.0..=6.0).contains(&spec.true_peak_ceiling_dbtp)
    {
        return Err("true_peak_ceiling_dbtp must be finite and between -120 and 6 dBTP".into());
    }
    if spec.max_input_bytes == 0 || spec.max_decoded_samples == 0 {
        return Err("binaural input and decoded-sample limits must be greater than zero".into());
    }
    let mut paths = HashSet::new();
    if !paths.insert(spec.source.to_string_lossy().into_owned())
        || !paths.insert(spec.rendered.to_string_lossy().into_owned())
    {
        return Err("source and rendered paths must be different".into());
    }
    if spec
        .reference
        .as_ref()
        .is_some_and(|path| path.as_os_str().is_empty())
    {
        return Err("reference path must not be empty when supplied".into());
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn resolve(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn default_duration_tolerance() -> f64 {
    DEFAULT_DURATION_TOLERANCE_SECONDS
}

fn default_loudness_tolerance() -> f64 {
    DEFAULT_LOUDNESS_TOLERANCE_LU
}

fn default_true_peak_tolerance() -> f64 {
    DEFAULT_TRUE_PEAK_TOLERANCE_DB
}

fn default_true_peak_ceiling() -> f64 {
    0.0
}

fn default_max_input_bytes() -> u64 {
    DEFAULT_MAX_INPUT_BYTES
}

fn default_max_decoded_samples() -> u64 {
    DEFAULT_MAX_DECODED_SAMPLES
}

/// Compute a digest for a renderer/model artifact when a caller wants to
/// generate the evidence values in a shell script before writing the spec.
pub fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes =
        fs::read(path).map_err(|error| format!("read {} for SHA-256: {error}", path.display()))?;
    Ok(Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::{AudioBuffer, PcmKind, WavWriter};

    fn evidence() -> RendererEvidence {
        RendererEvidence {
            name: "reference-hrtf".into(),
            version: "1.2.3".into(),
            renderer_sha256: "a".repeat(64),
            model: "studio-hrtf".into(),
            model_version: "2026.1".into(),
            model_sha256: "b".repeat(64),
            config_sha256: None,
        }
    }

    #[test]
    fn validates_renderer_hash_evidence() {
        let mut renderer = evidence();
        assert!(validate_spec(&BinauralQcSpec {
            schema_version: 1,
            source: "source.wav".into(),
            rendered: "rendered.wav".into(),
            reference: None,
            input_layout: "7.1.4".into(),
            renderer: renderer.clone(),
            max_duration_delta_seconds: 0.001,
            max_loudness_delta_lu: 1.0,
            max_true_peak_delta_db: 1.0,
            true_peak_ceiling_dbtp: 0.0,
            max_clipped_samples: 0,
            max_input_bytes: 1,
            max_decoded_samples: 1,
        })
        .is_ok());
        renderer.renderer_sha256 = "A".repeat(64);
        assert!(is_sha256("a".repeat(64).as_str()));
        assert!(!is_sha256(&renderer.renderer_sha256));
    }

    #[test]
    fn evaluates_duration_and_reference_drift() {
        let work = tempfile::tempdir().unwrap();
        let source_path = work.path().join("source.wav");
        let rendered_path = work.path().join("rendered.wav");
        let reference_path = work.path().join("reference.wav");
        let source = AudioBuffer {
            sample_rate: 48_000,
            channels: 12,
            frames: 48_000,
            data: vec![vec![0.01; 48_000]; 12],
            channel_roles: Layout::SevenOneFour.roles(),
            source_kind: PcmKind::F32,
        };
        let output = AudioBuffer {
            sample_rate: 48_000,
            channels: 2,
            frames: 48_000,
            data: vec![vec![0.01; 48_000]; 2],
            channel_roles: Layout::Stereo.roles(),
            source_kind: PcmKind::F32,
        };
        WavWriter::write(&source_path, &source, PcmKind::F32, false).unwrap();
        WavWriter::write(&rendered_path, &output, PcmKind::F32, false).unwrap();
        WavWriter::write(&reference_path, &output, PcmKind::F32, false).unwrap();
        let report = evaluate(
            &work.path().join("binaural.json"),
            BinauralQcSpec {
                schema_version: 1,
                source: source_path.file_name().unwrap().into(),
                rendered: rendered_path.file_name().unwrap().into(),
                reference: Some(reference_path.file_name().unwrap().into()),
                input_layout: "7.1.4".into(),
                renderer: evidence(),
                max_duration_delta_seconds: 0.001,
                max_loudness_delta_lu: 1.0,
                max_true_peak_delta_db: 1.0,
                true_peak_ceiling_dbtp: 0.0,
                max_clipped_samples: 0,
                max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
                max_decoded_samples: DEFAULT_MAX_DECODED_SAMPLES,
            },
        )
        .unwrap();
        assert!(report.passed);
        assert_eq!(report.output_layout, "binaural");
        assert!(report.reference_drift.unwrap().passed);
    }
}
