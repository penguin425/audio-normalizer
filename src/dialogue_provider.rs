//! Validated adapter for optional external ASR/VAD dialogue candidates.

use crate::normalize::{self, DialogueRange};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

pub const SCHEMA_VERSION: u32 = 1;
pub const ADAPTER: &str = "forge-dialogue-provider-1";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderInput {
    pub schema_version: u32,
    pub kind: ProviderKind,
    pub provider: String,
    pub provider_version: String,
    pub model: String,
    pub model_version: String,
    pub model_sha256: String,
    pub source_duration_seconds: f64,
    #[serde(default)]
    pub language: Option<String>,
    pub segments: Vec<ProviderSegment>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Asr,
    Vad,
    Hybrid,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSegment {
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub confidence: f64,
    #[serde(default)]
    pub transcript: Option<String>,
    #[serde(default)]
    pub speaker: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ProviderAudit {
    pub schema_version: u32,
    pub adapter: &'static str,
    pub source_path: String,
    pub kind: ProviderKind,
    pub provider: String,
    pub provider_version: String,
    pub model: String,
    pub model_version: String,
    pub model_sha256: String,
    pub language: Option<String>,
    pub source_duration_seconds: f64,
    pub threshold: f64,
    pub input_segment_count: usize,
    pub selected_segment_count: usize,
    pub selected_duration_seconds: f64,
    pub transcript_data_present: bool,
    pub segments: Vec<AuditedSegment>,
    pub ranges: Vec<DialogueRange>,
}

#[derive(Debug, Serialize)]
pub struct AuditedSegment {
    pub index: usize,
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub confidence: f64,
    pub speaker: Option<String>,
    pub transcript_present: bool,
    pub selected: bool,
}

#[derive(Debug, Serialize)]
pub struct DialogueRangeFile<'a> {
    pub ranges: &'a [DialogueRange],
}

pub fn load_and_audit(path: &Path, threshold: f64) -> Result<ProviderAudit, String> {
    if !threshold.is_finite() || !(0.0..=1.0).contains(&threshold) {
        return Err("dialogue-provider threshold must be between 0 and 1".into());
    }
    let bytes = fs::read(path)
        .map_err(|error| format!("read provider JSON {}: {error}", path.display()))?;
    let input: ProviderInput = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse provider JSON {}: {error}", path.display()))?;
    audit(path, input, threshold)
}

pub fn audit(
    source_path: &Path,
    input: ProviderInput,
    threshold: f64,
) -> Result<ProviderAudit, String> {
    validate_input(&input, threshold)?;
    let mut ranges = Vec::new();
    let mut segments = Vec::with_capacity(input.segments.len());
    for (offset, segment) in input.segments.into_iter().enumerate() {
        let selected = segment.confidence >= threshold;
        if selected {
            ranges.push(DialogueRange {
                start_seconds: segment.start_seconds,
                duration_seconds: segment.end_seconds - segment.start_seconds,
            });
        }
        segments.push(AuditedSegment {
            index: offset + 1,
            start_seconds: segment.start_seconds,
            end_seconds: segment.end_seconds,
            confidence: segment.confidence,
            speaker: segment.speaker,
            transcript_present: segment
                .transcript
                .as_ref()
                .is_some_and(|value| !value.is_empty()),
            selected,
        });
    }
    if ranges.is_empty() {
        return Err(format!(
            "dialogue provider selected no segments at threshold {threshold:.2}"
        ));
    }
    normalize::validate_dialogue_ranges(&ranges)?;
    Ok(ProviderAudit {
        schema_version: SCHEMA_VERSION,
        adapter: ADAPTER,
        source_path: source_path.to_string_lossy().into_owned(),
        kind: input.kind,
        provider: input.provider,
        provider_version: input.provider_version,
        model: input.model,
        model_version: input.model_version,
        model_sha256: input.model_sha256.to_ascii_lowercase(),
        language: input.language,
        source_duration_seconds: input.source_duration_seconds,
        threshold,
        input_segment_count: segments.len(),
        selected_segment_count: ranges.len(),
        selected_duration_seconds: ranges.iter().map(|range| range.duration_seconds).sum(),
        transcript_data_present: segments.iter().any(|segment| segment.transcript_present),
        segments,
        ranges,
    })
}

fn validate_input(input: &ProviderInput, threshold: f64) -> Result<(), String> {
    if input.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported dialogue-provider schema {}; expected {SCHEMA_VERSION}",
            input.schema_version
        ));
    }
    if !threshold.is_finite() || !(0.0..=1.0).contains(&threshold) {
        return Err("dialogue-provider threshold must be between 0 and 1".into());
    }
    for (label, value) in [
        ("provider", input.provider.as_str()),
        ("provider_version", input.provider_version.as_str()),
        ("model", input.model.as_str()),
        ("model_version", input.model_version.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(format!("{label} is required"));
        }
    }
    if input.model_sha256.len() != 64
        || !input
            .model_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("model_sha256 must contain exactly 64 hexadecimal digits".into());
    }
    if !input.source_duration_seconds.is_finite() || input.source_duration_seconds <= 0.0 {
        return Err("source_duration_seconds must be finite and positive".into());
    }
    if input.segments.is_empty() {
        return Err("provider output must contain at least one segment".into());
    }
    let mut previous_end = 0.0;
    for (offset, segment) in input.segments.iter().enumerate() {
        if !segment.start_seconds.is_finite()
            || !segment.end_seconds.is_finite()
            || segment.start_seconds < 0.0
            || segment.end_seconds <= segment.start_seconds
            || segment.end_seconds > input.source_duration_seconds
        {
            return Err(format!(
                "provider segment {} has invalid bounds",
                offset + 1
            ));
        }
        if offset > 0 && segment.start_seconds < previous_end {
            return Err(format!(
                "provider segment {} overlaps or is not sorted",
                offset + 1
            ));
        }
        if !segment.confidence.is_finite() || !(0.0..=1.0).contains(&segment.confidence) {
            return Err(format!(
                "provider segment {} confidence must be between 0 and 1",
                offset + 1
            ));
        }
        previous_end = segment.end_seconds;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> ProviderInput {
        ProviderInput {
            schema_version: 1,
            kind: ProviderKind::Hybrid,
            provider: "external-engine".into(),
            provider_version: "2.1".into(),
            model: "speech-model".into(),
            model_version: "2026-01".into(),
            model_sha256: "a".repeat(64),
            source_duration_seconds: 10.0,
            language: Some("ja".into()),
            segments: vec![
                ProviderSegment {
                    start_seconds: 1.0,
                    end_seconds: 2.0,
                    confidence: 0.9,
                    transcript: Some("private text is not copied".into()),
                    speaker: Some("speaker-1".into()),
                },
                ProviderSegment {
                    start_seconds: 3.0,
                    end_seconds: 4.0,
                    confidence: 0.2,
                    transcript: None,
                    speaker: None,
                },
            ],
        }
    }

    #[test]
    fn emits_reviewable_ranges_without_copying_transcripts() {
        let audit = audit(Path::new("provider.json"), fixture(), 0.6).unwrap();
        assert_eq!(audit.selected_segment_count, 1);
        assert_eq!(audit.ranges[0].start_seconds, 1.0);
        assert!(audit.transcript_data_present);
        let json = serde_json::to_string(&audit).unwrap();
        assert!(!json.contains("private text"));
    }

    #[test]
    fn rejects_unversioned_models_and_overlapping_segments() {
        let mut input = fixture();
        input.model_sha256 = "unknown".into();
        assert!(audit(Path::new("provider.json"), input, 0.6).is_err());
        let mut input = fixture();
        input.segments[1].start_seconds = 1.5;
        assert!(audit(Path::new("provider.json"), input, 0.6).is_err());
    }
}
