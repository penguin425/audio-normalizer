//! Versioned, auditable adapter for optional external audio-quality anomaly models.
//!
//! Forge deliberately does not ship a neural-network runtime or model weights in
//! the core binary.  A provider can emit this small JSON contract, and Forge
//! validates the provenance, bounds, and review thresholds before the result is
//! used by a downstream workflow.  The adapter is evidence only; it never
//! changes the standards-based loudness or EBU QC results.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

pub const SCHEMA_VERSION: u32 = 1;
pub const INPUT_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/audio-anomaly-provider-v1";
pub const AUDIT_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/anomaly-provider-audit-v1";
pub const ADAPTER: &str = "forge-anomaly-provider-1";

const MAX_EVENTS: usize = 100_000;
const MAX_AUDIT_BYTES: usize = 32 * 1024 * 1024;
const MAX_SOURCE_DURATION_SECONDS: f64 = 7.0 * 24.0 * 60.0 * 60.0;
const MAX_LABEL_LENGTH: usize = 128;

/// Categories currently covered by the provider contract.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "kebab-case")]
pub enum AnomalyKind {
    Noise,
    Pop,
    Dropout,
    LipNoise,
    PhaseCancellation,
    Clipping,
    Other,
}

impl AnomalyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Noise => "noise",
            Self::Pop => "pop",
            Self::Dropout => "dropout",
            Self::LipNoise => "lip-noise",
            Self::PhaseCancellation => "phase-cancellation",
            Self::Clipping => "clipping",
            Self::Other => "other",
        }
    }
}

/// Provider output submitted to Forge.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderInput {
    pub schema_version: u32,
    pub provider: String,
    pub provider_version: String,
    pub model: String,
    pub model_version: String,
    pub model_sha256: String,
    pub source_sha256: String,
    pub source_duration_seconds: f64,
    #[serde(default)]
    pub sample_rate_hz: Option<u32>,
    pub events: Vec<ProviderEvent>,
}

/// One model finding.  Events may overlap when models report different kinds
/// of defects over the same audio span; they are ordered by time, not merged.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEvent {
    pub kind: AnomalyKind,
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub confidence: f64,
    pub severity: f64,
    #[serde(default)]
    pub channel: Option<u16>,
    #[serde(default)]
    pub related_channel: Option<u16>,
    /// A short non-sensitive feature label, never a transcript or raw audio.
    #[serde(default)]
    pub evidence_label: Option<String>,
}

/// Bounded, reviewable audit emitted by `forge-anomaly-provider`.
#[derive(Debug, Serialize)]
pub struct ProviderAudit {
    pub schema: &'static str,
    pub schema_version: u32,
    pub adapter: &'static str,
    pub source_path: String,
    pub source_sha256: String,
    pub provider: String,
    pub provider_version: String,
    pub model: String,
    pub model_version: String,
    pub model_sha256: String,
    pub source_duration_seconds: f64,
    pub sample_rate_hz: Option<u32>,
    pub confidence_threshold: f64,
    pub severity_threshold: f64,
    pub input_event_count: usize,
    pub selected_event_count: usize,
    /// Sum of selected event durations. Overlapping events are intentionally
    /// not unioned and therefore this is an event-duration total.
    pub selected_event_duration_seconds: f64,
    pub selected_by_kind: BTreeMap<String, usize>,
    pub passed: bool,
    pub events: Vec<AuditedEvent>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuditedEvent {
    pub index: usize,
    pub kind: AnomalyKind,
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub confidence: f64,
    pub severity: f64,
    #[serde(default)]
    pub channel: Option<u16>,
    #[serde(default)]
    pub related_channel: Option<u16>,
    #[serde(default)]
    pub evidence_label: Option<String>,
    pub selected: bool,
}

/// A previously generated anomaly-provider audit that is safe to import into
/// a delivery manifest.  This is deliberately separate from [`ProviderAudit`]
/// because the latter contains static string fields for the generator output,
/// while an imported document must be fully owned and independently validated.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAuditDocument {
    pub schema: String,
    pub schema_version: u32,
    pub adapter: String,
    pub source_path: String,
    pub source_sha256: String,
    pub provider: String,
    pub provider_version: String,
    pub model: String,
    pub model_version: String,
    pub model_sha256: String,
    pub source_duration_seconds: f64,
    #[serde(default)]
    pub sample_rate_hz: Option<u32>,
    pub confidence_threshold: f64,
    pub severity_threshold: f64,
    pub input_event_count: usize,
    pub selected_event_count: usize,
    pub selected_event_duration_seconds: f64,
    pub selected_by_kind: BTreeMap<String, usize>,
    pub passed: bool,
    pub events: Vec<AuditedEvent>,
}

/// Read and validate a generated anomaly-provider audit under the same bounds
/// used by the provider contract.  The audit is evidence, not a signature: the
/// caller must still decide how much trust to place in the external model.
pub fn load_audit(path: &Path) -> Result<ProviderAuditDocument, String> {
    let bytes = fs::read(path)
        .map_err(|error| format!("read anomaly audit {}: {error}", path.display()))?;
    if bytes.len() > MAX_AUDIT_BYTES {
        return Err(format!(
            "anomaly audit is {} bytes; maximum is {MAX_AUDIT_BYTES}",
            bytes.len()
        ));
    }
    let document: ProviderAuditDocument = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse anomaly audit {}: {error}", path.display()))?;
    validate_audit(&document)?;
    Ok(document)
}

/// Validate an imported anomaly-provider audit in memory.
pub fn validate_audit(document: &ProviderAuditDocument) -> Result<(), String> {
    if document.schema != AUDIT_SCHEMA {
        return Err(format!(
            "unsupported anomaly audit schema {}; expected {AUDIT_SCHEMA}",
            document.schema
        ));
    }
    if document.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported anomaly audit schema version {}; expected {SCHEMA_VERSION}",
            document.schema_version
        ));
    }
    if document.adapter != ADAPTER {
        return Err(format!(
            "unsupported anomaly audit adapter {}; expected {ADAPTER}",
            document.adapter
        ));
    }
    for (label, value) in [
        ("source_path", document.source_path.as_str()),
        ("provider", document.provider.as_str()),
        ("provider_version", document.provider_version.as_str()),
        ("model", document.model.as_str()),
        ("model_version", document.model_version.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(format!("anomaly audit {label} is required"));
        }
    }
    validate_sha256("source_sha256", &document.source_sha256)?;
    validate_sha256("model_sha256", &document.model_sha256)?;
    if !document.source_duration_seconds.is_finite()
        || document.source_duration_seconds <= 0.0
        || document.source_duration_seconds > MAX_SOURCE_DURATION_SECONDS
    {
        return Err(format!(
            "anomaly audit source_duration_seconds must be greater than 0 and no more than {MAX_SOURCE_DURATION_SECONDS}"
        ));
    }
    if document.sample_rate_hz == Some(0) {
        return Err("anomaly audit sample_rate_hz must be positive when present".into());
    }
    validate_threshold("confidence", document.confidence_threshold)?;
    validate_threshold("severity", document.severity_threshold)?;
    if document.input_event_count > MAX_EVENTS || document.events.len() > MAX_EVENTS {
        return Err(format!(
            "anomaly audit contains more than {MAX_EVENTS} events"
        ));
    }
    if document.input_event_count != document.events.len() {
        return Err(format!(
            "anomaly audit input_event_count is {}, but events contains {} entries",
            document.input_event_count,
            document.events.len()
        ));
    }
    if document.selected_event_count > document.input_event_count {
        return Err("anomaly audit selected_event_count exceeds input_event_count".into());
    }

    let mut selected_count = 0usize;
    let mut selected_duration = 0.0;
    let mut selected_by_kind = BTreeMap::new();
    let mut previous = None;
    for (offset, event) in document.events.iter().enumerate() {
        if event.index != offset + 1 {
            return Err(format!(
                "anomaly audit event {} has index {}; expected {}",
                offset + 1,
                event.index,
                offset + 1
            ));
        }
        if !event.start_seconds.is_finite()
            || !event.end_seconds.is_finite()
            || event.start_seconds < 0.0
            || event.end_seconds <= event.start_seconds
            || event.end_seconds > document.source_duration_seconds
        {
            return Err(format!(
                "anomaly audit event {} has invalid time bounds",
                offset + 1
            ));
        }
        if previous.is_some_and(|(start, end)| {
            event.start_seconds < start || (event.start_seconds == start && event.end_seconds < end)
        }) {
            return Err(format!(
                "anomaly audit event {} is not sorted by start/end time",
                offset + 1
            ));
        }
        previous = Some((event.start_seconds, event.end_seconds));
        for (label, value) in [
            ("confidence", event.confidence),
            ("severity", event.severity),
        ] {
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return Err(format!(
                    "anomaly audit event {} {label} must be between 0 and 1",
                    offset + 1
                ));
            }
        }
        if event.channel == Some(0) || event.related_channel == Some(0) {
            return Err(format!(
                "anomaly audit event {} channel numbers are one-based",
                offset + 1
            ));
        }
        if event.channel.is_some() && event.channel == event.related_channel {
            return Err(format!(
                "anomaly audit event {} channel and related_channel must differ",
                offset + 1
            ));
        }
        if let Some(label) = event.evidence_label.as_deref() {
            if label.is_empty()
                || label.chars().count() > MAX_LABEL_LENGTH
                || label.chars().any(char::is_control)
            {
                return Err(format!(
                    "anomaly audit event {} evidence_label must be 1-{MAX_LABEL_LENGTH} printable characters",
                    offset + 1
                ));
            }
        }
        let expected_selected = event.confidence >= document.confidence_threshold
            && event.severity >= document.severity_threshold;
        if event.selected != expected_selected {
            return Err(format!(
                "anomaly audit event {} selected flag does not match the recorded thresholds",
                offset + 1
            ));
        }
        if event.selected {
            selected_count += 1;
            selected_duration += event.end_seconds - event.start_seconds;
            *selected_by_kind
                .entry(event.kind.as_str().to_owned())
                .or_insert(0) += 1;
        }
    }
    if selected_count != document.selected_event_count {
        return Err(format!(
            "anomaly audit selected_event_count is {}, but {} events are selected",
            document.selected_event_count, selected_count
        ));
    }
    if document.passed != (selected_count == 0) {
        return Err("anomaly audit passed does not match selected_event_count".into());
    }
    if selected_by_kind != document.selected_by_kind {
        return Err("anomaly audit selected_by_kind does not match selected events".into());
    }
    if !document.selected_event_duration_seconds.is_finite()
        || document.selected_event_duration_seconds < 0.0
        || (document.selected_event_duration_seconds - selected_duration).abs()
            > 1e-9_f64.max(selected_duration.abs() * 1e-9)
    {
        return Err("anomaly audit selected event duration does not match events".into());
    }
    Ok(())
}

pub fn load_and_audit(
    path: &Path,
    confidence_threshold: f64,
    severity_threshold: f64,
) -> Result<ProviderAudit, String> {
    validate_threshold("confidence", confidence_threshold)?;
    validate_threshold("severity", severity_threshold)?;
    let bytes = fs::read(path)
        .map_err(|error| format!("read anomaly provider JSON {}: {error}", path.display()))?;
    let input: ProviderInput = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse anomaly provider JSON {}: {error}", path.display()))?;
    audit(path, input, confidence_threshold, severity_threshold)
}

pub fn audit(
    source_path: &Path,
    input: ProviderInput,
    confidence_threshold: f64,
    severity_threshold: f64,
) -> Result<ProviderAudit, String> {
    validate_threshold("confidence", confidence_threshold)?;
    validate_threshold("severity", severity_threshold)?;
    validate_input(&input)?;

    let mut events = Vec::with_capacity(input.events.len());
    let mut selected_event_count = 0;
    let mut selected_event_duration_seconds = 0.0;
    let mut selected_by_kind = BTreeMap::new();
    let input_event_count = input.events.len();
    for (offset, event) in input.events.into_iter().enumerate() {
        let selected =
            event.confidence >= confidence_threshold && event.severity >= severity_threshold;
        if selected {
            selected_event_count += 1;
            selected_event_duration_seconds += event.end_seconds - event.start_seconds;
            *selected_by_kind
                .entry(event.kind.as_str().to_owned())
                .or_insert(0) += 1;
        }
        events.push(AuditedEvent {
            index: offset + 1,
            kind: event.kind,
            start_seconds: event.start_seconds,
            end_seconds: event.end_seconds,
            confidence: event.confidence,
            severity: event.severity,
            channel: event.channel,
            related_channel: event.related_channel,
            evidence_label: event.evidence_label,
            selected,
        });
    }

    Ok(ProviderAudit {
        schema: AUDIT_SCHEMA,
        schema_version: SCHEMA_VERSION,
        adapter: ADAPTER,
        source_path: source_path.to_string_lossy().into_owned(),
        source_sha256: input.source_sha256.to_ascii_lowercase(),
        provider: input.provider,
        provider_version: input.provider_version,
        model: input.model,
        model_version: input.model_version,
        model_sha256: input.model_sha256.to_ascii_lowercase(),
        source_duration_seconds: input.source_duration_seconds,
        sample_rate_hz: input.sample_rate_hz,
        confidence_threshold,
        severity_threshold,
        input_event_count,
        selected_event_count,
        selected_event_duration_seconds,
        selected_by_kind,
        passed: selected_event_count == 0,
        events,
    })
}

fn validate_threshold(label: &str, value: f64) -> Result<(), String> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(format!("anomaly {label} threshold must be between 0 and 1"));
    }
    Ok(())
}

fn validate_input(input: &ProviderInput) -> Result<(), String> {
    if input.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported anomaly-provider schema {}; expected {SCHEMA_VERSION}",
            input.schema_version
        ));
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
    validate_sha256("model_sha256", &input.model_sha256)?;
    validate_sha256("source_sha256", &input.source_sha256)?;
    if !input.source_duration_seconds.is_finite()
        || input.source_duration_seconds <= 0.0
        || input.source_duration_seconds > MAX_SOURCE_DURATION_SECONDS
    {
        return Err(format!(
            "source_duration_seconds must be greater than 0 and no more than {MAX_SOURCE_DURATION_SECONDS}"
        ));
    }
    if input.sample_rate_hz == Some(0) {
        return Err("sample_rate_hz must be positive when present".into());
    }
    if input.events.len() > MAX_EVENTS {
        return Err(format!(
            "anomaly provider returned {} events; maximum is {MAX_EVENTS}",
            input.events.len()
        ));
    }

    let mut previous = None;
    for (index, event) in input.events.iter().enumerate() {
        if !event.start_seconds.is_finite()
            || !event.end_seconds.is_finite()
            || event.start_seconds < 0.0
            || event.end_seconds <= event.start_seconds
            || event.end_seconds > input.source_duration_seconds
        {
            return Err(format!(
                "anomaly event {} has invalid time bounds",
                index + 1
            ));
        }
        if previous.is_some_and(|(start, end)| {
            event.start_seconds < start || (event.start_seconds == start && event.end_seconds < end)
        }) {
            return Err(format!(
                "anomaly event {} is not sorted by start/end time",
                index + 1
            ));
        }
        previous = Some((event.start_seconds, event.end_seconds));
        for (label, value) in [
            ("confidence", event.confidence),
            ("severity", event.severity),
        ] {
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return Err(format!(
                    "anomaly event {} {label} must be between 0 and 1",
                    index + 1
                ));
            }
        }
        if event.channel == Some(0) || event.related_channel == Some(0) {
            return Err(format!(
                "anomaly event {} channel numbers are one-based",
                index + 1
            ));
        }
        if event.channel.is_some() && event.channel == event.related_channel {
            return Err(format!(
                "anomaly event {} channel and related_channel must differ",
                index + 1
            ));
        }
        if let Some(label) = event.evidence_label.as_deref() {
            if label.is_empty()
                || label.chars().count() > MAX_LABEL_LENGTH
                || label.chars().any(char::is_control)
            {
                return Err(format!(
                    "anomaly event {} evidence_label must be 1-{MAX_LABEL_LENGTH} printable characters",
                    index + 1
                ));
            }
        }
    }
    Ok(())
}

fn validate_sha256(label: &str, value: &str) -> Result<(), String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!(
            "{label} must contain exactly 64 hexadecimal digits"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> ProviderInput {
        ProviderInput {
            schema_version: SCHEMA_VERSION,
            provider: "reviewed-detector".into(),
            provider_version: "1.2".into(),
            model: "quality-model".into(),
            model_version: "2026-08".into(),
            model_sha256: "a".repeat(64),
            source_sha256: "b".repeat(64),
            source_duration_seconds: 10.0,
            sample_rate_hz: Some(48_000),
            events: vec![
                ProviderEvent {
                    kind: AnomalyKind::Pop,
                    start_seconds: 1.0,
                    end_seconds: 1.1,
                    confidence: 0.9,
                    severity: 0.8,
                    channel: Some(1),
                    related_channel: None,
                    evidence_label: Some("impulse-spectrum".into()),
                },
                ProviderEvent {
                    kind: AnomalyKind::Noise,
                    start_seconds: 4.0,
                    end_seconds: 5.0,
                    confidence: 0.4,
                    severity: 0.9,
                    channel: None,
                    related_channel: None,
                    evidence_label: None,
                },
            ],
        }
    }

    #[test]
    fn selects_by_both_thresholds_and_preserves_provenance() {
        let audit = audit(Path::new("programme.wav"), fixture(), 0.6, 0.5).unwrap();
        assert_eq!(audit.selected_event_count, 1);
        assert_eq!(audit.selected_by_kind.get("pop"), Some(&1));
        assert!((audit.selected_event_duration_seconds - 0.1).abs() < 1e-9);
        assert!(!audit.passed);
        assert_eq!(audit.source_sha256, "b".repeat(64));
        assert_eq!(audit.model_sha256, "a".repeat(64));
    }

    #[test]
    fn accepts_empty_findings_as_a_pass() {
        let mut input = fixture();
        input.events.clear();
        let audit = audit(Path::new("programme.wav"), input, 0.6, 0.5).unwrap();
        assert!(audit.passed);
        assert!(audit.events.is_empty());
    }

    #[test]
    fn rejects_unsorted_and_out_of_range_events() {
        let mut input = fixture();
        input.events.swap(0, 1);
        assert!(audit(Path::new("programme.wav"), input, 0.6, 0.5).is_err());

        let mut input = fixture();
        input.events[0].end_seconds = 11.0;
        assert!(audit(Path::new("programme.wav"), input, 0.6, 0.5).is_err());
    }

    #[test]
    fn rejects_invalid_hash_and_channel_pair() {
        let mut input = fixture();
        input.source_sha256 = "not-a-hash".into();
        assert!(audit(Path::new("programme.wav"), input, 0.6, 0.5).is_err());

        let mut input = fixture();
        input.events[0].related_channel = Some(1);
        assert!(audit(Path::new("programme.wav"), input, 0.6, 0.5).is_err());
    }

    #[test]
    fn validates_audit_round_trip_and_rejects_tampering() {
        let generated = audit(Path::new("programme.wav"), fixture(), 0.6, 0.5).unwrap();
        let value = serde_json::to_value(&generated).unwrap();
        let document: ProviderAuditDocument = serde_json::from_value(value.clone()).unwrap();
        validate_audit(&document).unwrap();

        let mut tampered = value;
        tampered["events"][0]["selected"] = serde_json::Value::Bool(false);
        let document: ProviderAuditDocument = serde_json::from_value(tampered).unwrap();
        assert!(validate_audit(&document).is_err());
    }
}
