//! Auditable immersive downmix simulation and clip-risk reporting.
//!
//! This module operates on decoded channel-based PCM. It deliberately does
//! not claim to render IAMF, MPEG-H, AC-4, or proprietary object metadata; an
//! external renderer must be audited separately with `forge-presentation-qc`.

use crate::downmix::{self, Layout, MatrixChannel, Profile};
use crate::normalize;
use crate::wav::AudioBuffer;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: u32 = 1;
pub const VALIDATOR: &str = "forge-immersive-downmix-qc-1";
pub const DEFAULT_MAX_INPUT_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_MAX_DECODED_SAMPLES: u64 = 24 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownmixQcSpec {
    pub schema_version: u32,
    pub source: PathBuf,
    pub input_layout: String,
    pub profiles: Vec<Profile>,
    #[serde(default = "default_true_peak_ceiling")]
    pub true_peak_ceiling_dbtp: f64,
    #[serde(default)]
    pub max_clipped_samples: u64,
    #[serde(default)]
    pub max_loudness_delta_lu: Option<f64>,
    #[serde(default)]
    pub max_true_peak_delta_db: Option<f64>,
    #[serde(default = "default_max_input_bytes")]
    pub max_input_bytes: u64,
    #[serde(default = "default_max_decoded_samples")]
    pub max_decoded_samples: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DownmixQcReport {
    pub schema_version: u32,
    pub validator: &'static str,
    pub source: String,
    pub input_layout: &'static str,
    pub true_peak_ceiling_dbtp: f64,
    pub max_clipped_samples: u64,
    pub max_loudness_delta_lu: Option<f64>,
    pub max_true_peak_delta_db: Option<f64>,
    pub source_measurement: Measurement,
    pub profiles: Vec<ProfileResult>,
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
pub struct ProfileResult {
    pub profile: Profile,
    pub target_layout: &'static str,
    pub method: &'static str,
    pub mapping: Vec<MatrixChannel>,
    pub measurement: Measurement,
    pub loudness_delta_lu: Option<f64>,
    pub true_peak_delta_db: Option<f64>,
    pub sample_peak_delta_db: Option<f64>,
    pub clipped_samples: u64,
    pub maximum_sample: f32,
    pub headroom_to_ceiling_db: Option<f64>,
    pub clip_risk: ClipRisk,
    pub loudness_delta_passed: bool,
    pub true_peak_delta_passed: bool,
    pub clip_risk_passed: bool,
    pub passed: bool,
}

#[derive(Debug, Clone, Copy)]
struct QcLimits {
    true_peak_ceiling_dbtp: f64,
    max_clipped_samples: u64,
    max_loudness_delta_lu: Option<f64>,
    max_true_peak_delta_db: Option<f64>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ClipRisk {
    None,
    TruePeakCeiling,
    SampleClipping,
}

pub fn evaluate_file(path: &Path) -> Result<DownmixQcReport, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("read downmix QC spec {}: {error}", path.display()))?;
    let spec: DownmixQcSpec = match path.extension().and_then(|value| value.to_str()) {
        Some("json") => serde_json::from_str(&text)
            .map_err(|error| format!("parse downmix QC JSON: {error}"))?,
        Some("toml") => {
            toml::from_str(&text).map_err(|error| format!("parse downmix QC TOML: {error}"))?
        }
        _ => return Err("downmix QC spec must use .json or .toml".into()),
    };
    evaluate(path, spec)
}

pub fn evaluate(path: &Path, spec: DownmixQcSpec) -> Result<DownmixQcReport, String> {
    validate_spec(&spec)?;
    let input_layout = Layout::parse(&spec.input_layout)
        .ok_or_else(|| format!("unsupported input layout {}", spec.input_layout))?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let source_path = resolve(base, &spec.source);
    let bytes = fs::metadata(&source_path)
        .map_err(|error| format!("stat downmix source {}: {error}", source_path.display()))?
        .len();
    if bytes > spec.max_input_bytes {
        return Err(format!(
            "downmix source {} is {bytes} bytes, above max_input_bytes {}",
            source_path.display(),
            spec.max_input_bytes
        ));
    }
    let (mut source, layout_provenance) =
        crate::decoder::decode_limited_with_layout(&source_path, spec.max_decoded_samples)
            .map_err(|error| format!("decode downmix source {}: {error}", source_path.display()))?;
    if source.channels as usize != input_layout.channels() {
        return Err(format!(
            "input layout {} requires {} channels, decoded {}",
            input_layout.as_str(),
            input_layout.channels(),
            source.channels
        ));
    }
    // The spec is the auditable layout authority when a legacy WAVE file has
    // an absent or ambiguous channel mask. Keep decoder provenance until this
    // explicit assignment has been checked against the decoded channel count.
    let explicit_roles = input_layout.roles();
    source.channel_roles = normalize::resolve_decoded_channel_roles(
        &source_path,
        source.channels,
        &source.channel_roles,
        layout_provenance,
        Some(&explicit_roles),
    )?;
    let source_analysis = normalize::analyze(&source);
    let source_measurement = measurement(input_layout.as_str(), &source_analysis);
    let limits = QcLimits {
        true_peak_ceiling_dbtp: spec.true_peak_ceiling_dbtp,
        max_clipped_samples: spec.max_clipped_samples,
        max_loudness_delta_lu: spec.max_loudness_delta_lu,
        max_true_peak_delta_db: spec.max_true_peak_delta_db,
    };
    let mut profiles = Vec::with_capacity(spec.profiles.len());
    for profile in spec.profiles {
        let rendered = downmix::render(&source, input_layout, profile)
            .map_err(|error| format!("render {} downmix: {error}", profile.as_str()))?;
        let analysis = normalize::analyze(&rendered.buffer);
        profiles.push(profile_result(
            profile,
            &rendered.buffer,
            &analysis,
            rendered.mapping,
            rendered.method,
            &source_analysis,
            limits,
        ));
    }
    Ok(DownmixQcReport {
        schema_version: SCHEMA_VERSION,
        validator: VALIDATOR,
        source: source_path.to_string_lossy().into_owned(),
        input_layout: input_layout.as_str(),
        true_peak_ceiling_dbtp: spec.true_peak_ceiling_dbtp,
        max_clipped_samples: spec.max_clipped_samples,
        max_loudness_delta_lu: spec.max_loudness_delta_lu,
        max_true_peak_delta_db: spec.max_true_peak_delta_db,
        source_measurement,
        passed: profiles.iter().all(|profile| profile.passed),
        profiles,
    })
}

fn profile_result(
    profile: Profile,
    buffer: &AudioBuffer,
    analysis: &crate::analysis::Analysis,
    mapping: Vec<MatrixChannel>,
    method: &'static str,
    source: &crate::analysis::Analysis,
    limits: QcLimits,
) -> ProfileResult {
    let (clipped_samples, maximum_sample) = clipping(buffer);
    let loudness_delta_lu = delta(analysis.lufs, source.lufs);
    let true_peak_delta_db = delta(analysis.true_peak_db(), source.true_peak_db());
    let sample_peak_delta_db = delta(analysis.sample_peak_db(), source.sample_peak_db());
    let headroom_to_ceiling_db = analysis
        .true_peak_db()
        .is_finite()
        .then(|| limits.true_peak_ceiling_dbtp - analysis.true_peak_db());
    let clip_risk = if clipped_samples > 0 {
        ClipRisk::SampleClipping
    } else if analysis.true_peak_db().is_finite()
        && analysis.true_peak_db() > limits.true_peak_ceiling_dbtp
    {
        ClipRisk::TruePeakCeiling
    } else {
        ClipRisk::None
    };
    let loudness_delta_passed = limit_passed(loudness_delta_lu, limits.max_loudness_delta_lu);
    let true_peak_delta_passed = limit_passed(true_peak_delta_db, limits.max_true_peak_delta_db);
    let clip_risk_passed = clipped_samples <= limits.max_clipped_samples
        && (!analysis.true_peak_db().is_finite()
            || analysis.true_peak_db() <= limits.true_peak_ceiling_dbtp);
    ProfileResult {
        profile,
        target_layout: profile.target_layout().as_str(),
        method,
        mapping,
        measurement: measurement(profile.target_layout().as_str(), analysis),
        loudness_delta_lu,
        true_peak_delta_db,
        sample_peak_delta_db,
        clipped_samples,
        maximum_sample,
        headroom_to_ceiling_db,
        clip_risk,
        loudness_delta_passed,
        true_peak_delta_passed,
        clip_risk_passed,
        passed: loudness_delta_passed && true_peak_delta_passed && clip_risk_passed,
    }
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
    let mut count = 0u64;
    let mut maximum = 0.0f32;
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

fn delta(measured: f64, source: f64) -> Option<f64> {
    (measured.is_finite() && source.is_finite()).then_some(measured - source)
}

fn limit_passed(value: Option<f64>, limit: Option<f64>) -> bool {
    match limit {
        Some(limit) => value.is_some_and(|value| value.abs() <= limit),
        None => true,
    }
}

fn validate_spec(spec: &DownmixQcSpec) -> Result<(), String> {
    if spec.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported downmix QC schema {}; expected {SCHEMA_VERSION}",
            spec.schema_version
        ));
    }
    if spec.source.as_os_str().is_empty() {
        return Err("downmix source is required".into());
    }
    if Layout::parse(&spec.input_layout).is_none() {
        return Err(format!("unsupported input layout {}", spec.input_layout));
    }
    if spec.profiles.is_empty() {
        return Err("at least one downmix profile is required".into());
    }
    let mut profiles = HashSet::new();
    for profile in &spec.profiles {
        if !profiles.insert(*profile) {
            return Err(format!("duplicate downmix profile {}", profile.as_str()));
        }
    }
    if !spec.true_peak_ceiling_dbtp.is_finite()
        || !(-120.0..=6.0).contains(&spec.true_peak_ceiling_dbtp)
    {
        return Err("true_peak_ceiling_dbtp must be finite and between -120 and 6 dBTP".into());
    }
    for (name, value) in [
        ("max_loudness_delta_lu", spec.max_loudness_delta_lu),
        ("max_true_peak_delta_db", spec.max_true_peak_delta_db),
    ] {
        if value.is_some_and(|value| !value.is_finite() || value < 0.0) {
            return Err(format!("{name} must be finite and non-negative"));
        }
    }
    if spec.max_input_bytes == 0 || spec.max_decoded_samples == 0 {
        return Err("downmix input and decoded-sample limits must be greater than zero".into());
    }
    Ok(())
}

fn resolve(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::{ChannelRole, PcmKind, WavWriter};

    #[test]
    fn rejects_upmixing_to_seven_one_four() {
        let source = AudioBuffer {
            sample_rate: 48_000,
            channels: 6,
            frames: 1,
            data: vec![vec![0.0]; 6],
            channel_roles: Layout::FiveOne.roles(),
            source_kind: PcmKind::F32,
        };
        assert!(downmix::render(&source, Layout::FiveOne, Profile::SevenOneFour).is_err());
    }

    #[test]
    fn reports_deltas_and_sample_clip_risk() {
        let work = tempfile::tempdir().unwrap();
        let source_path = work.path().join("master.wav");
        let frames = 48_000;
        let source = AudioBuffer {
            sample_rate: 48_000,
            channels: 6,
            frames,
            data: vec![vec![0.9; frames]; 6],
            channel_roles: vec![ChannelRole::Main; 6],
            source_kind: PcmKind::F32,
        };
        WavWriter::write(&source_path, &source, PcmKind::F32, false).unwrap();
        let legacy_error =
            crate::decoder::decode_limited(&source_path, DEFAULT_MAX_DECODED_SAMPLES).unwrap_err();
        assert!(legacy_error.contains("ambiguous channel layout"));
        let spec_path = work.path().join("downmix.json");
        fs::write(
            &spec_path,
            r#"{
                "schema_version": 1,
                "source": "master.wav",
                "input_layout": "5.1",
                "profiles": ["stereo"],
                "true_peak_ceiling_dbtp": 0.0
            }"#,
        )
        .unwrap();
        let report = evaluate_file(&spec_path).unwrap();
        let result = &report.profiles[0];
        assert!(result.loudness_delta_lu.is_some());
        assert!(result.true_peak_delta_db.is_some());
        assert_eq!(result.clip_risk, ClipRisk::SampleClipping);
        assert!(!report.passed);
    }
}
