//! Auditable loudness QC for externally rendered immersive presentations.

use crate::normalize;
use crate::report::{ComplianceProfile, ComplianceResult};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: u32 = 1;
pub const VALIDATOR: &str = "forge-immersive-presentation-qc-1";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresentationQcSpec {
    pub schema_version: u32,
    pub codec: ImmersiveCodec,
    pub renderer: RendererEvidence,
    #[serde(default = "default_tolerance")]
    pub tolerance: f64,
    pub presentations: Vec<PresentationSpec>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImmersiveCodec {
    Ac4,
    Iamf,
    MpegH,
}

impl ImmersiveCodec {
    fn standard(self) -> &'static str {
        match self {
            Self::Ac4 => "ETSI TS 103 190",
            Self::Iamf => "AOMedia IAMF v1.1 / Open Audio Renderer v1.0.0",
            Self::MpegH => "ISO/IEC 23008-3",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RendererEvidence {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresentationSpec {
    pub id: String,
    pub rendered_path: PathBuf,
    #[serde(default)]
    pub reference_path: Option<PathBuf>,
    #[serde(default)]
    pub compliance: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub accessibility: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PresentationQcReport {
    pub schema_version: u32,
    pub validator: &'static str,
    pub codec: ImmersiveCodec,
    pub codec_standard: &'static str,
    pub renderer: RendererEvidence,
    pub source_spec: String,
    pub presentation_count: usize,
    pub passed: bool,
    pub presentations: Vec<PresentationResult>,
}

#[derive(Debug, Serialize)]
pub struct PresentationResult {
    pub id: String,
    pub language: Option<String>,
    pub accessibility: Option<String>,
    pub rendered_path: String,
    pub reference_path: Option<String>,
    pub integrated_lufs: f64,
    pub true_peak_dbtp: f64,
    pub duration_seconds: f64,
    pub channels: u16,
    pub reference_loudness_drift_lu: Option<f64>,
    pub reference_true_peak_drift_db: Option<f64>,
    pub reference_duration_drift_seconds: Option<f64>,
    pub reference_passed: Option<bool>,
    pub compliance: Option<ComplianceResult>,
    pub passed: bool,
}

pub fn evaluate_file(path: &Path) -> Result<PresentationQcReport, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("read presentation QC spec {}: {error}", path.display()))?;
    let spec: PresentationQcSpec = match path.extension().and_then(|value| value.to_str()) {
        Some("json") => serde_json::from_str(&text)
            .map_err(|error| format!("parse presentation QC JSON: {error}"))?,
        Some("toml") => {
            toml::from_str(&text).map_err(|error| format!("parse presentation QC TOML: {error}"))?
        }
        _ => return Err("presentation QC spec must use .json or .toml".into()),
    };
    evaluate(path, spec)
}

pub fn evaluate(path: &Path, spec: PresentationQcSpec) -> Result<PresentationQcReport, String> {
    validate_spec(&spec)?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let mut presentations = Vec::with_capacity(spec.presentations.len());
    for item in spec.presentations {
        let rendered_path = resolve(base, &item.rendered_path);
        let analysis = normalize::analyze_file(&rendered_path)
            .map_err(|error| format!("presentation {}: {error}", item.id))?;
        let reference_path = item
            .reference_path
            .as_ref()
            .map(|value| resolve(base, value));
        let reference = reference_path
            .as_deref()
            .map(normalize::analyze_file)
            .transpose()
            .map_err(|error| format!("presentation {} reference: {error}", item.id))?;
        let loudness_drift = reference
            .as_ref()
            .map(|value| metric_delta(analysis.lufs, value.lufs));
        let true_peak_drift = reference
            .as_ref()
            .map(|value| metric_delta(analysis.true_peak_db(), value.true_peak_db()));
        let duration_drift = reference
            .as_ref()
            .map(|value| analysis.duration_secs() - value.duration_secs());
        let sample_tolerance = 1.0 / analysis.sample_rate.max(1) as f64;
        let reference_passed = reference.as_ref().map(|_| {
            loudness_drift.is_some_and(|value| value.abs() <= spec.tolerance)
                && true_peak_drift.is_some_and(|value| value.abs() <= spec.tolerance)
                && duration_drift.is_some_and(|value| value.abs() <= sample_tolerance)
        });
        let compliance = item
            .compliance
            .as_deref()
            .map(ComplianceProfile::load)
            .transpose()?
            .map(|profile| profile.evaluate(&analysis))
            .transpose()?;
        let passed = reference_passed != Some(false)
            && compliance.as_ref().is_none_or(|result| result.passed);
        presentations.push(PresentationResult {
            id: item.id,
            language: item.language,
            accessibility: item.accessibility,
            rendered_path: rendered_path.to_string_lossy().into_owned(),
            reference_path: reference_path.map(|value| value.to_string_lossy().into_owned()),
            integrated_lufs: analysis.lufs,
            true_peak_dbtp: analysis.true_peak_db(),
            duration_seconds: analysis.duration_secs(),
            channels: analysis.channels,
            reference_loudness_drift_lu: loudness_drift,
            reference_true_peak_drift_db: true_peak_drift,
            reference_duration_drift_seconds: duration_drift,
            reference_passed,
            compliance,
            passed,
        });
    }
    Ok(PresentationQcReport {
        schema_version: SCHEMA_VERSION,
        validator: VALIDATOR,
        codec: spec.codec,
        codec_standard: spec.codec.standard(),
        renderer: spec.renderer,
        source_spec: path.to_string_lossy().into_owned(),
        presentation_count: presentations.len(),
        passed: presentations.iter().all(|item| item.passed),
        presentations,
    })
}

fn validate_spec(spec: &PresentationQcSpec) -> Result<(), String> {
    if spec.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported presentation QC schema {}; expected {SCHEMA_VERSION}",
            spec.schema_version
        ));
    }
    if spec.renderer.name.trim().is_empty() || spec.renderer.version.trim().is_empty() {
        return Err("renderer name and version are required audit evidence".into());
    }
    if !spec.tolerance.is_finite() || spec.tolerance < 0.0 {
        return Err("presentation QC tolerance must be finite and non-negative".into());
    }
    if spec.presentations.is_empty() {
        return Err("at least one presentation is required".into());
    }
    let mut ids = HashSet::new();
    for item in &spec.presentations {
        if item.id.trim().is_empty() || !ids.insert(item.id.as_str()) {
            return Err("presentation IDs must be non-empty and unique".into());
        }
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

fn default_tolerance() -> f64 {
    1.0
}

fn metric_delta(measured: f64, reference: f64) -> f64 {
    if measured == reference {
        0.0
    } else {
        measured - reference
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::{default_channel_roles, AudioBuffer, PcmKind, WavWriter};

    #[test]
    fn iamf_uses_aomedia_renderer_standard() {
        assert_eq!(
            ImmersiveCodec::Iamf.standard(),
            "AOMedia IAMF v1.1 / Open Audio Renderer v1.0.0"
        );
        let spec: PresentationQcSpec = serde_json::from_str(
            r#"{
                "schema_version": 1,
                "codec": "iamf",
                "renderer": {"name": "oar", "version": "1.0.0"},
                "presentations": [{"id": "stereo", "rendered_path": "stereo.wav"}]
            }"#,
        )
        .unwrap();
        assert!(matches!(spec.codec, ImmersiveCodec::Iamf));
    }

    #[test]
    fn evaluates_every_external_presentation() {
        let work = tempfile::tempdir().unwrap();
        let rendered = work.path().join("english.wav");
        let audio = AudioBuffer {
            sample_rate: 48_000,
            channels: 2,
            frames: 48_000,
            data: vec![vec![0.01; 48_000], vec![0.01; 48_000]],
            channel_roles: default_channel_roles(2),
            source_kind: PcmKind::F32,
        };
        WavWriter::write(&rendered, &audio, PcmKind::F32, false).unwrap();
        let spec = PresentationQcSpec {
            schema_version: 1,
            codec: ImmersiveCodec::MpegH,
            renderer: RendererEvidence {
                name: "reference-renderer".into(),
                version: "1.2.3".into(),
            },
            tolerance: 0.1,
            presentations: vec![PresentationSpec {
                id: "main-en".into(),
                rendered_path: PathBuf::from("english.wav"),
                reference_path: Some(PathBuf::from("english.wav")),
                compliance: None,
                language: Some("en".into()),
                accessibility: None,
            }],
        };
        let report = evaluate(&work.path().join("presentations.json"), spec).unwrap();
        assert!(report.passed);
        assert_eq!(report.codec_standard, "ISO/IEC 23008-3");
        assert_eq!(report.presentations[0].reference_passed, Some(true));
    }
}
