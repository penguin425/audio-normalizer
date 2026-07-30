//! Versioned report migration and actionable compliance explanations.

use crate::qc::{EBU_QC_CATALOGUE, FORGE_QC_SOURCE, QC_SCHEMA};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

pub const DELIVERY_MANIFEST_V1: &str =
    "https://penguin425.github.io/audio-normalizer/schema/delivery-manifest-v1";
pub const DELIVERY_MANIFEST_V2: &str =
    "https://penguin425.github.io/audio-normalizer/schema/delivery-manifest-v2";
pub const DELIVERY_MANIFEST_V3: &str =
    "https://penguin425.github.io/audio-normalizer/schema/delivery-manifest-v3";
pub const EXPLANATION_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/rule-explanations-v1";
pub const EXPLANATION_SCHEMA_V2: &str =
    "https://penguin425.github.io/audio-normalizer/schema/rule-explanations-v2";
const EBU_QC_SCHEMA_V1: &str =
    "https://penguin425.github.io/audio-normalizer/schema/ebu-qc-results-v1";
const DELIVERY_MANIFEST_V1_SCHEMA: &str =
    include_str!("../schema/delivery-manifest-v1.schema.json");
const DELIVERY_MANIFEST_V2_SCHEMA: &str =
    include_str!("../schema/delivery-manifest-v2.schema.json");
const DELIVERY_MANIFEST_V3_SCHEMA: &str =
    include_str!("../schema/delivery-manifest-v3.schema.json");
const EXPLANATION_V1_SCHEMA: &str = include_str!("../schema/rule-explanations-v1.schema.json");
const EXPLANATION_V2_SCHEMA: &str = include_str!("../schema/rule-explanations-v2.schema.json");
pub const MAX_REPORT_BYTES: usize = 64 * 1024 * 1024;
const MAX_ASSETS: usize = 100_000;
const MAX_QC_RESULTS_PER_ASSET: usize = 10_000;
const MAX_COMPLIANCE_RULES_PER_ASSET: usize = 1_000;
const MAX_FINDINGS_PER_ASSET: usize = 20_000;
const MAX_FINDINGS_PER_REPORT: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationSummary {
    pub source_schema: String,
    pub target_schema: &'static str,
    pub asset_count: usize,
    pub migrated_qc_envelopes: usize,
    pub changed: bool,
}

/// Migrate a Forge delivery manifest to the latest schema without discarding
/// extension evidence carried by individual assets.
pub fn migrate_delivery_manifest(bytes: &[u8]) -> Result<(Value, MigrationSummary), String> {
    ensure_report_size(bytes.len())?;
    let mut manifest: Value =
        serde_json::from_slice(bytes).map_err(|error| format!("decode manifest JSON: {error}"))?;
    let root = manifest
        .as_object_mut()
        .ok_or_else(|| "delivery manifest must be a JSON object".to_string())?;
    let source_schema = root
        .get("schema")
        .and_then(Value::as_str)
        .ok_or_else(|| "delivery manifest requires a string schema".to_string())?
        .to_owned();
    if !matches!(
        source_schema.as_str(),
        DELIVERY_MANIFEST_V1 | DELIVERY_MANIFEST_V2 | DELIVERY_MANIFEST_V3
    ) {
        return Err(format!(
            "unsupported delivery manifest schema {source_schema}"
        ));
    }
    let asset_count = root
        .get("assets")
        .and_then(Value::as_array)
        .ok_or_else(|| "delivery manifest requires an assets array".to_string())?;
    validate_manifest_counts(root, asset_count.len())?;
    let asset_count = asset_count.len();
    ensure_asset_count(asset_count)?;
    let root = manifest
        .as_object_mut()
        .expect("validated delivery manifest object");
    let assets = root
        .get_mut("assets")
        .and_then(Value::as_array_mut)
        .expect("validated assets array");

    let mut migrated_qc_envelopes = 0;
    for (index, asset) in assets.iter_mut().enumerate() {
        let object = asset
            .as_object_mut()
            .ok_or_else(|| format!("asset {} must be a JSON object", index + 1))?;
        if !object.get("path").is_some_and(Value::is_string) {
            return Err(format!("asset {} requires a string path", index + 1));
        }
        if migrate_qc_envelope(object, index)? {
            migrated_qc_envelopes += 1;
        }
    }
    root.insert("schema".into(), Value::String(DELIVERY_MANIFEST_V3.into()));
    let changed = source_schema != DELIVERY_MANIFEST_V3 || migrated_qc_envelopes > 0;
    Ok((
        manifest,
        MigrationSummary {
            source_schema,
            target_schema: DELIVERY_MANIFEST_V3,
            asset_count,
            migrated_qc_envelopes,
            changed,
        },
    ))
}

pub fn delivery_manifest_schema(schema_id: &str) -> Option<&'static str> {
    Some(match schema_id {
        DELIVERY_MANIFEST_V1 => DELIVERY_MANIFEST_V1_SCHEMA,
        DELIVERY_MANIFEST_V2 => DELIVERY_MANIFEST_V2_SCHEMA,
        DELIVERY_MANIFEST_V3 => DELIVERY_MANIFEST_V3_SCHEMA,
        _ => return None,
    })
}

pub fn explanation_schema(schema_id: &str) -> Option<&'static str> {
    Some(match schema_id {
        EXPLANATION_SCHEMA => EXPLANATION_V1_SCHEMA,
        EXPLANATION_SCHEMA_V2 => EXPLANATION_V2_SCHEMA,
        _ => return None,
    })
}

fn ensure_report_size(size: usize) -> Result<(), String> {
    if size > MAX_REPORT_BYTES {
        Err(format!(
            "report is {size} bytes; limit is {MAX_REPORT_BYTES} bytes"
        ))
    } else {
        Ok(())
    }
}

fn ensure_asset_count(count: usize) -> Result<(), String> {
    if count > MAX_ASSETS {
        Err(format!(
            "report contains {count} assets; limit is {MAX_ASSETS}"
        ))
    } else {
        Ok(())
    }
}

fn validate_manifest_counts(root: &Map<String, Value>, asset_count: usize) -> Result<(), String> {
    let declared = required_count(root, "asset_count")?;
    let passed = required_count(root, "passed_count")?;
    let failed = required_count(root, "failed_count")?;
    if declared != asset_count {
        return Err(format!(
            "asset_count is {declared}, but assets contains {asset_count} entries"
        ));
    }
    if passed
        .checked_add(failed)
        .ok_or_else(|| "passed_count + failed_count overflows".to_string())?
        != declared
    {
        return Err("passed_count + failed_count must equal asset_count".into());
    }
    if !root.get("generator").is_some_and(Value::is_string) {
        return Err("delivery manifest requires a string generator".into());
    }
    Ok(())
}

fn required_count(root: &Map<String, Value>, name: &str) -> Result<usize, String> {
    root.get(name)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| format!("delivery manifest requires a non-negative integer {name}"))
}

fn migrate_qc_envelope(asset: &mut Map<String, Value>, asset_index: usize) -> Result<bool, String> {
    if let Some(qc) = asset.get_mut("qc") {
        let envelope = qc
            .as_object_mut()
            .ok_or_else(|| format!("asset {} qc must be an object", asset_index + 1))?;
        let schema = envelope
            .get("schema")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("asset {} qc requires a string schema", asset_index + 1))?
            .to_owned();
        if !matches!(schema.as_str(), EBU_QC_SCHEMA_V1 | QC_SCHEMA) {
            return Err(format!(
                "asset {} uses unsupported QC schema {schema}",
                asset_index + 1
            ));
        }
        let changed = schema != QC_SCHEMA;
        let results = envelope
            .get_mut("results")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| format!("asset {} qc requires a results array", asset_index + 1))?;
        if results.len() > MAX_QC_RESULTS_PER_ASSET {
            return Err(format!(
                "asset {} contains {} QC results; limit is {}",
                asset_index + 1,
                results.len(),
                MAX_QC_RESULTS_PER_ASSET
            ));
        }
        if schema == EBU_QC_SCHEMA_V1 {
            upgrade_legacy_qc_results(results, asset_index)?;
        }
        envelope.insert("schema".into(), Value::String(QC_SCHEMA.into()));
        return Ok(changed);
    }
    let Some(encoded) = asset
        .get("ebu_qc_results_json")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(false);
    };
    let mut results: Value = serde_json::from_str(encoded)
        .map_err(|error| format!("asset {} decode EBU QC results: {error}", asset_index + 1))?;
    let results_array = results.as_array_mut().ok_or_else(|| {
        format!(
            "asset {} EBU QC results must decode to an array",
            asset_index + 1
        )
    })?;
    if results_array.len() > MAX_QC_RESULTS_PER_ASSET {
        return Err(format!(
            "asset {} contains {} QC results; limit is {}",
            asset_index + 1,
            results_array.len(),
            MAX_QC_RESULTS_PER_ASSET
        ));
    }
    upgrade_legacy_qc_results(results_array, asset_index)?;
    asset.insert(
        "qc".into(),
        json!({
            "schema": QC_SCHEMA,
            "results": results,
        }),
    );
    Ok(true)
}

fn upgrade_legacy_qc_results(results: &mut [Value], asset_index: usize) -> Result<(), String> {
    for (result_index, result) in results.iter_mut().enumerate() {
        let object = result.as_object_mut().ok_or_else(|| {
            format!(
                "asset {} QC result {} must be an object",
                asset_index + 1,
                result_index + 1
            )
        })?;
        let id = object
            .get("ebu_qc_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                format!(
                    "asset {} QC result {} requires ebu_qc_id",
                    asset_index + 1,
                    result_index + 1
                )
            })?
            .to_owned();
        object
            .entry("rule_id")
            .or_insert_with(|| Value::String(id.clone()));
        let original_layer = object
            .get("layer")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        if !matches!(original_layer.as_str(), "baseband" | "bitstream") {
            let migrated_layer = if original_layer == "wrapper" || original_layer == "x-check" {
                "bitstream"
            } else {
                "baseband"
            };
            object.insert("layer".into(), Value::String(migrated_layer.into()));
        }
        object.entry("source_url").or_insert_with(|| {
            let source = if id.len() == 5
                && id.as_bytes()[..4].iter().all(u8::is_ascii_digit)
                && id.as_bytes()[4].is_ascii_uppercase()
            {
                format!("{EBU_QC_CATALOGUE}/{id}/")
            } else {
                FORGE_QC_SOURCE.into()
            };
            Value::String(source)
        });
        object.entry("method").or_insert_with(|| {
            Value::String(format!(
                "Migrated legacy QC evidence; original method unavailable (legacy layer: {original_layer})"
            ))
        });
        object
            .entry("events_truncated")
            .or_insert(Value::Bool(false));
        if !object.get("events").is_some_and(Value::is_array) {
            return Err(format!(
                "asset {} QC result {} requires an events array",
                asset_index + 1,
                result_index + 1
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
struct RuleInput {
    metric: String,
    measured: f64,
    minimum: Option<f64>,
    maximum: Option<f64>,
    #[serde(default)]
    minimum_inclusive: Option<bool>,
    #[serde(default)]
    maximum_inclusive: Option<bool>,
    passed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuleSource {
    pub profile: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub standard: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub standard_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuleObservation {
    pub measured: f64,
    pub unit: &'static str,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    pub minimum_inclusive: Option<bool>,
    pub maximum_inclusive: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuleExplanation {
    pub rule_id: String,
    pub asset: String,
    pub metric: String,
    pub source: RuleSource,
    pub observation: RuleObservation,
    pub requirement: String,
    pub remediation: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExplanationReport {
    pub schema: &'static str,
    pub generator: &'static str,
    pub source_schema: Option<String>,
    pub asset_count: usize,
    pub failed_rule_count: usize,
    pub explanations: Vec<RuleExplanation>,
}

/// Explain every failed compliance rule in an analysis JSON report or delivery
/// manifest. The result preserves the measured boundary semantics and adds
/// stable rule IDs, provenance, and metric-specific remediation.
pub fn explain_failed_rules(bytes: &[u8]) -> Result<ExplanationReport, String> {
    ensure_report_size(bytes.len())?;
    let value: Value =
        serde_json::from_slice(bytes).map_err(|error| format!("decode report JSON: {error}"))?;
    let (source_schema, assets) = report_assets(&value)?;
    let asset_count = assets.len();
    ensure_asset_count(asset_count)?;
    let mut explanations = Vec::new();
    for (index, asset) in assets.into_iter().enumerate() {
        let object = asset
            .as_object()
            .ok_or_else(|| format!("asset {} must be a JSON object", index + 1))?;
        let path = object
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("asset {} requires a string path", index + 1))?;
        let profile = object
            .get("compliance_profile")
            .and_then(Value::as_str)
            .unwrap_or("unspecified-profile");
        let standard = optional_string(object, "compliance_standard")?;
        let standard_version = optional_string(object, "compliance_standard_version")?;
        let rules = decode_rules(object, index)?;
        if rules.len() > MAX_COMPLIANCE_RULES_PER_ASSET {
            return Err(format!(
                "asset {} contains {} compliance rules; limit is {}",
                index + 1,
                rules.len(),
                MAX_COMPLIANCE_RULES_PER_ASSET
            ));
        }
        for rule in rules.into_iter().filter(|rule| !rule.passed) {
            explanations.push(explain_rule(
                path,
                profile,
                standard.clone(),
                standard_version.clone(),
                rule,
            ));
        }
    }
    explanations.sort_by(|left, right| {
        left.asset
            .cmp(&right.asset)
            .then_with(|| left.rule_id.cmp(&right.rule_id))
    });
    Ok(ExplanationReport {
        schema: EXPLANATION_SCHEMA,
        generator: concat!("forge-normalizer/", env!("CARGO_PKG_VERSION")),
        source_schema,
        asset_count,
        failed_rule_count: explanations.len(),
        explanations,
    })
}

fn report_assets(value: &Value) -> Result<(Option<String>, Vec<&Value>), String> {
    if let Some(array) = value.as_array() {
        return Ok((None, array.iter().collect()));
    }
    let object = value
        .as_object()
        .ok_or_else(|| "report must be a JSON object or array".to_string())?;
    if let Some(assets) = object.get("assets") {
        let assets = assets
            .as_array()
            .ok_or_else(|| "report assets must be an array".to_string())?;
        return Ok((
            object
                .get("schema")
                .and_then(Value::as_str)
                .map(str::to_owned),
            assets.iter().collect(),
        ));
    }
    Ok((
        object
            .get("schema")
            .and_then(Value::as_str)
            .map(str::to_owned),
        vec![value],
    ))
}

fn optional_string(object: &Map<String, Value>, name: &str) -> Result<Option<String>, String> {
    match object.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("{name} must be a string or null")),
    }
}

fn decode_rules(object: &Map<String, Value>, asset_index: usize) -> Result<Vec<RuleInput>, String> {
    let Some(value) = object.get("compliance_rules_json") else {
        return Ok(Vec::new());
    };
    match value {
        Value::Null => Ok(Vec::new()),
        Value::String(encoded) => serde_json::from_str(encoded).map_err(|error| {
            format!(
                "asset {} decode compliance_rules_json: {error}",
                asset_index + 1
            )
        }),
        Value::Array(_) => serde_json::from_value(value.clone())
            .map_err(|error| format!("asset {} decode compliance rules: {error}", asset_index + 1)),
        _ => Err(format!(
            "asset {} compliance_rules_json must be a string, array, or null",
            asset_index + 1
        )),
    }
}

fn explain_rule(
    asset: &str,
    profile: &str,
    standard: Option<String>,
    standard_version: Option<String>,
    rule: RuleInput,
) -> RuleExplanation {
    let source = RuleSource {
        profile: profile.into(),
        url: source_url(standard.as_deref(), profile),
        standard,
        standard_version,
    };
    let unit = metric_unit(&rule.metric);
    let requirement = format_requirement(&rule, unit);
    RuleExplanation {
        rule_id: format!(
            "FORGE-COMPLIANCE-{}",
            rule.metric.replace('_', "-").to_ascii_uppercase()
        ),
        asset: asset.into(),
        metric: rule.metric.clone(),
        source,
        observation: RuleObservation {
            measured: rule.measured,
            unit,
            minimum: rule.minimum,
            maximum: rule.maximum,
            minimum_inclusive: rule.minimum_inclusive,
            maximum_inclusive: rule.maximum_inclusive,
        },
        requirement,
        remediation: remediation(&rule.metric),
    }
}

fn metric_unit(metric: &str) -> &'static str {
    match metric {
        "integrated_lufs" | "dialogue_lufs" | "max_short_term_lufs" | "max_momentary_lufs" => {
            "LUFS"
        }
        "true_peak_dbtp" => "dBTP",
        "loudness_range_lu" | "peak_to_loudness_ratio_lu" | "loudness_to_dialogue_ratio_lu" => "LU",
        _ => "value",
    }
}

fn format_requirement(rule: &RuleInput, unit: &str) -> String {
    match (rule.minimum, rule.maximum) {
        (Some(minimum), Some(maximum)) => format!(
            "{} {:.2} {unit} and {} {:.2} {unit}",
            if rule.minimum_inclusive.unwrap_or(true) {
                "at least"
            } else {
                "greater than"
            },
            minimum,
            if rule.maximum_inclusive.unwrap_or(true) {
                "at most"
            } else {
                "less than"
            },
            maximum
        ),
        (Some(minimum), None) => format!(
            "{} {:.2} {unit}",
            if rule.minimum_inclusive.unwrap_or(true) {
                "at least"
            } else {
                "greater than"
            },
            minimum
        ),
        (None, Some(maximum)) => format!(
            "{} {:.2} {unit}",
            if rule.maximum_inclusive.unwrap_or(true) {
                "at most"
            } else {
                "less than"
            },
            maximum
        ),
        (None, None) => "a profile-defined condition".into(),
    }
}

fn remediation(metric: &str) -> &'static str {
    match metric {
        "integrated_lufs" | "dialogue_lufs" => {
            "Adjust programme gain toward the permitted interval, then remeasure the final encoded delivery."
        }
        "true_peak_dbtp" => {
            "Lower gain or the true-peak ceiling, or use a verified true-peak limiter, then re-encode and remeasure."
        }
        "max_short_term_lufs" | "max_momentary_lufs" => {
            "Review the flagged loud passages and use mix automation or controlled dynamics processing before remeasurement."
        }
        "loudness_range_lu" => {
            "Review programme dynamics; preserve or reshape the range with mix automation or compression appropriate to the content."
        }
        "peak_to_loudness_ratio_lu" => {
            "Review peak structure and average loudness together; adjust dynamics or peak control without relying on gain alone."
        }
        "loudness_to_dialogue_ratio_lu" => {
            "Rebalance dialogue against music and effects, or revise the reviewed dialogue selection, then remeasure both programme and dialogue."
        }
        _ => "Review the profile requirement and measured evidence, correct the source, and rerun compliance analysis.",
    }
}

fn source_url(standard: Option<&str>, profile: &str) -> Option<&'static str> {
    let name = standard.unwrap_or(profile).to_ascii_lowercase();
    if name.contains("ebu r 128 s1") {
        Some("https://tech.ebu.ch/publications/r128s1")
    } else if name.contains("ebu r 128 s2") {
        Some("https://tech.ebu.ch/publications/r128s2")
    } else if name.contains("ebu r 128 s3") {
        Some("https://tech.ebu.ch/publications/r128s3")
    } else if name.contains("ebu r 128 s4") {
        Some("https://tech.ebu.ch/publications/r128s4")
    } else if name.contains("ebu r 128") {
        Some("https://tech.ebu.ch/publications/r128")
    } else if name.contains("itu-r bs.1770") {
        Some("https://www.itu.int/rec/R-REC-BS.1770")
    } else if name.contains("atsc a/85") || name.contains("atsc-a85") {
        Some("https://www.atsc.org/atsc-documents/a85-techniques-for-establishing-and-maintaining-audio-loudness-for-digital-television/")
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum FindingCategory {
    Compliance,
    EbuQc,
    Container,
    Codec,
    Adm,
    AdmProfile,
    Presentation,
}

impl FindingCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compliance => "compliance",
            Self::EbuQc => "ebu_qc",
            Self::Container => "container",
            Self::Codec => "codec",
            Self::Adm => "adm",
            Self::AdmProfile => "adm_profile",
            Self::Presentation => "presentation",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FindingSource {
    pub profile: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub standard: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub standard_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FindingExplanation {
    pub rule_id: String,
    pub asset: String,
    pub category: FindingCategory,
    pub location: String,
    pub source: FindingSource,
    pub observation: Value,
    pub requirement: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub remediation: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExplanationReportV2 {
    pub schema: &'static str,
    pub generator: &'static str,
    pub source_schema: Option<String>,
    pub asset_count: usize,
    pub failed_rule_count: usize,
    pub explanations: Vec<FindingExplanation>,
}

/// Explain every failed compliance, EBU QC, container/codec, ADM/profile, and
/// externally rendered presentation rule carried by a Forge report.
///
/// The v1 API remains available through [`explain_failed_rules`] for consumers
/// that only accept the original numeric compliance explanation contract.
pub fn explain_failed_findings(bytes: &[u8]) -> Result<ExplanationReportV2, String> {
    ensure_report_size(bytes.len())?;
    let value: Value =
        serde_json::from_slice(bytes).map_err(|error| format!("decode report JSON: {error}"))?;
    let mut explanations = Vec::new();
    let (source_schema, asset_count) = if is_container_report(&value) {
        let object = value
            .as_object()
            .ok_or_else(|| "container QC report must be an object".to_string())?;
        let asset = object
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| "container QC report requires a string path".to_string())?;
        explain_container_audit(object, asset, "", &mut explanations)?;
        (
            object
                .get("schema")
                .and_then(Value::as_str)
                .map(str::to_owned),
            1,
        )
    } else if is_presentation_report(&value) {
        let object = value
            .as_object()
            .ok_or_else(|| "presentation QC report must be an object".to_string())?;
        let count = explain_presentation_report(object, &mut explanations)?;
        (
            Some(format!(
                "{}-v{}",
                object
                    .get("validator")
                    .and_then(Value::as_str)
                    .unwrap_or("forge-presentation-qc"),
                object
                    .get("schema_version")
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
            )),
            count,
        )
    } else if is_adm_profile_report(&value) {
        let object = value
            .as_object()
            .ok_or_else(|| "ADM profile report must be an object".to_string())?;
        let asset = object
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("unknown-adm-asset");
        explain_adm_profile_report(object, asset, "", &mut explanations)?;
        (
            object
                .get("validator")
                .and_then(Value::as_str)
                .map(str::to_owned),
            1,
        )
    } else {
        let (schema, assets) = report_assets(&value)?;
        ensure_asset_count(assets.len())?;
        let has_assets_member = value
            .as_object()
            .is_some_and(|object| object.contains_key("assets"));
        let is_array = value.is_array();
        for (index, asset) in assets.iter().enumerate() {
            let object = asset
                .as_object()
                .ok_or_else(|| format!("asset {} must be a JSON object", index + 1))?;
            let path = object
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("asset {} requires a string path", index + 1))?;
            let location = if has_assets_member {
                format!("/assets/{index}")
            } else if is_array {
                format!("/{index}")
            } else {
                String::new()
            };
            let before = explanations.len();
            explain_analysis_asset(object, path, &location, index, &mut explanations)?;
            if explanations.len() - before > MAX_FINDINGS_PER_ASSET {
                return Err(format!(
                    "asset {} contains more than {MAX_FINDINGS_PER_ASSET} failed rules",
                    index + 1
                ));
            }
        }
        (schema, assets.len())
    };
    explanations.sort_by(|left, right| {
        left.asset
            .cmp(&right.asset)
            .then_with(|| left.category.cmp(&right.category))
            .then_with(|| left.rule_id.cmp(&right.rule_id))
            .then_with(|| left.location.cmp(&right.location))
    });
    Ok(ExplanationReportV2 {
        schema: EXPLANATION_SCHEMA_V2,
        generator: concat!("forge-normalizer/", env!("CARGO_PKG_VERSION")),
        source_schema,
        asset_count,
        failed_rule_count: explanations.len(),
        explanations,
    })
}

fn is_container_report(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        object
            .get("schema")
            .and_then(Value::as_str)
            .is_some_and(|schema| schema == crate::container_qc::CONTAINER_QC_SCHEMA)
            || (object.contains_key("format")
                && object.contains_key("layers")
                && object.contains_key("properties"))
    })
}

fn is_presentation_report(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        object
            .get("validator")
            .and_then(Value::as_str)
            .is_some_and(|validator| validator == crate::presentation_qc::VALIDATOR)
            && object.contains_key("presentations")
    })
}

fn is_adm_profile_report(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        object
            .get("validator")
            .and_then(Value::as_str)
            .is_some_and(|validator| validator == crate::adm::PRODUCTION_VALIDATOR)
            && object.contains_key("rules")
    })
}

fn explain_analysis_asset(
    object: &Map<String, Value>,
    asset: &str,
    location: &str,
    asset_index: usize,
    explanations: &mut Vec<FindingExplanation>,
) -> Result<(), String> {
    explain_asset_compliance(object, asset, location, asset_index, explanations)?;
    explain_asset_ebu_qc(object, asset, location, asset_index, explanations)?;
    if let Some(container) = object.get("container_qc") {
        let container = container
            .as_object()
            .ok_or_else(|| format!("asset {} container_qc must be an object", asset_index + 1))?;
        explain_container_audit(
            container,
            asset,
            &format!("{location}/container_qc"),
            explanations,
        )?;
    }
    explain_asset_adm_profile(object, asset, location, asset_index, explanations)?;
    explain_asset_adm(object, asset, location, asset_index, explanations)?;
    explain_asset_codec(object, asset, location, explanations)?;
    Ok(())
}

fn explain_asset_compliance(
    object: &Map<String, Value>,
    asset: &str,
    location: &str,
    asset_index: usize,
    explanations: &mut Vec<FindingExplanation>,
) -> Result<(), String> {
    let profile = object
        .get("compliance_profile")
        .and_then(Value::as_str)
        .unwrap_or("unspecified-profile");
    let standard = optional_string(object, "compliance_standard")?;
    let standard_version = optional_string(object, "compliance_standard_version")?;
    let rules = decode_rules(object, asset_index)?;
    if rules.len() > MAX_COMPLIANCE_RULES_PER_ASSET {
        return Err(format!(
            "asset {} contains {} compliance rules; limit is {}",
            asset_index + 1,
            rules.len(),
            MAX_COMPLIANCE_RULES_PER_ASSET
        ));
    }
    for (index, rule) in rules
        .into_iter()
        .enumerate()
        .filter(|(_, rule)| !rule.passed)
    {
        push_finding(
            explanations,
            explain_compliance_rule_v2(
                asset,
                profile,
                standard.clone(),
                standard_version.clone(),
                rule,
                format!("{location}/compliance_rules_json/{index}"),
                FindingCategory::Compliance,
            ),
        )?;
    }
    Ok(())
}

fn explain_compliance_rule_v2(
    asset: &str,
    profile: &str,
    standard: Option<String>,
    standard_version: Option<String>,
    rule: RuleInput,
    location: String,
    category: FindingCategory,
) -> FindingExplanation {
    let unit = metric_unit(&rule.metric);
    let requirement = format_requirement(&rule, unit);
    FindingExplanation {
        rule_id: format!(
            "FORGE-COMPLIANCE-{}",
            rule.metric.replace('_', "-").to_ascii_uppercase()
        ),
        asset: asset.into(),
        category,
        location,
        source: FindingSource {
            profile: profile.into(),
            url: source_url(standard.as_deref(), profile).map(str::to_owned),
            standard,
            standard_version,
        },
        observation: json!({
            "metric": rule.metric,
            "measured": rule.measured,
            "unit": unit,
            "minimum": rule.minimum,
            "maximum": rule.maximum,
            "minimum_inclusive": rule.minimum_inclusive,
            "maximum_inclusive": rule.maximum_inclusive,
        }),
        requirement,
        message: None,
        remediation: remediation(&rule.metric).into(),
    }
}

#[derive(Debug, Deserialize)]
struct EbuQcInput {
    #[serde(default)]
    rule_id: Option<String>,
    #[serde(default)]
    ebu_qc_id: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    layer: Option<String>,
    passed: bool,
    #[serde(default)]
    calculated: Option<bool>,
    #[serde(default)]
    source_url: Option<String>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    events_truncated: bool,
    #[serde(default)]
    events: Vec<Value>,
}

fn explain_asset_ebu_qc(
    object: &Map<String, Value>,
    asset: &str,
    location: &str,
    asset_index: usize,
    explanations: &mut Vec<FindingExplanation>,
) -> Result<(), String> {
    let (results, base) = if let Some(qc) = object.get("qc") {
        let qc = qc
            .as_object()
            .ok_or_else(|| format!("asset {} qc must be an object", asset_index + 1))?;
        let schema = qc
            .get("schema")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("asset {} qc requires a string schema", asset_index + 1))?;
        if !matches!(schema, QC_SCHEMA | EBU_QC_SCHEMA_V1) {
            return Err(format!(
                "asset {} uses unsupported QC schema {schema}",
                asset_index + 1
            ));
        }
        let results = qc
            .get("results")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("asset {} qc requires a results array", asset_index + 1))?
            .clone();
        (results, format!("{location}/qc/results"))
    } else {
        let Some(results) =
            decode_embedded_array(object, "ebu_qc_results_json", asset_index, "EBU QC results")?
        else {
            return Ok(());
        };
        (results, format!("{location}/ebu_qc_results_json"))
    };
    if results.len() > MAX_QC_RESULTS_PER_ASSET {
        return Err(format!(
            "asset {} contains {} QC results; limit is {}",
            asset_index + 1,
            results.len(),
            MAX_QC_RESULTS_PER_ASSET
        ));
    }
    for (index, value) in results.into_iter().enumerate() {
        let result: EbuQcInput = serde_json::from_value(value)
            .map_err(|error| format!("asset {} decode EBU QC result: {error}", asset_index + 1))?;
        if result.events.len() > MAX_QC_RESULTS_PER_ASSET {
            return Err(format!(
                "asset {} EBU QC result {} contains too many events",
                asset_index + 1,
                index + 1
            ));
        }
        if result.passed {
            continue;
        }
        let rule_id = result
            .rule_id
            .or_else(|| result.ebu_qc_id.clone())
            .ok_or_else(|| {
                format!(
                    "asset {} EBU QC result {} requires rule_id or ebu_qc_id",
                    asset_index + 1,
                    index + 1
                )
            })?;
        let id = result.ebu_qc_id.clone().unwrap_or_else(|| rule_id.clone());
        let name = result.name.unwrap_or_else(|| rule_id.clone());
        let method = result
            .method
            .unwrap_or_else(|| "the recorded Forge QC method".into());
        let ebu_item = is_ebu_item_id(&id);
        let source = result
            .source_url
            .or_else(|| ebu_item.then(|| format!("{EBU_QC_CATALOGUE}/{id}/")));
        let event_count = result.events.len();
        push_finding(
            explanations,
            FindingExplanation {
                rule_id,
                asset: asset.into(),
                category: FindingCategory::EbuQc,
                location: format!("{base}/{index}"),
                source: FindingSource {
                    profile: if ebu_item {
                        format!("EBU QC Item {id}")
                    } else {
                        "Forge decoded-audio QC".into()
                    },
                    standard: ebu_item.then(|| "EBU QC Items".into()),
                    standard_version: result.version,
                    url: source,
                },
                observation: json!({
                    "layer": result.layer,
                    "calculated": result.calculated,
                    "event_count": event_count,
                    "events_truncated": result.events_truncated,
                    "events": result.events,
                }),
                requirement: format!("No failing {name} events under {method}."),
                message: Some(format!("{name}: {event_count} failing event(s)")),
                remediation: ebu_remediation(&id, &name),
            },
        )?;
    }
    Ok(())
}

fn is_ebu_item_id(id: &str) -> bool {
    id.len() == 5
        && id.as_bytes()[..4].iter().all(u8::is_ascii_digit)
        && id.as_bytes()[4].is_ascii_uppercase()
}

fn ebu_remediation(id: &str, name: &str) -> String {
    let key = format!("{id} {name}").to_ascii_lowercase();
    if key.contains("clipping") || key.contains("true peak") {
        "Reduce gain or repair clipped samples, then re-render and remeasure true peak.".into()
    } else if key.contains("duration") {
        "Correct the edit or declared duration and verify sample-accurate start/end boundaries."
            .into()
    } else if key.contains("silence") || key.contains("dropout") {
        "Inspect the reported time ranges, repair unintended silence/dropouts, and rerun baseband QC."
            .into()
    } else if key.contains("phase") || key.contains("polarity") {
        "Correct channel polarity/phase alignment and verify correlation across the reported ranges."
            .into()
    } else if key.contains("tone") || key.contains("hum") || key.contains("buzz") {
        "Remove or document the detected tonal interference and rerun the same spectral check."
            .into()
    } else if key.contains("channel") || key.contains("mono") || key.contains("panning") {
        "Correct the channel count, assignment, or balance and repeat channel-layout QC.".into()
    } else {
        "Inspect each reported channel/time event, correct the source or delivery configuration, and rerun EBU QC."
            .into()
    }
}

fn explain_container_audit(
    object: &Map<String, Value>,
    fallback_asset: &str,
    location: &str,
    explanations: &mut Vec<FindingExplanation>,
) -> Result<(), String> {
    let before = explanations.len();
    if let Some(schema) = object.get("schema").and_then(Value::as_str) {
        if schema != crate::container_qc::CONTAINER_QC_SCHEMA {
            return Err(format!("unsupported container QC schema {schema}"));
        }
    }
    let asset = object
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or(fallback_asset);
    let format = object
        .get("format")
        .and_then(Value::as_str)
        .ok_or_else(|| "container QC report requires a string format".to_string())?;
    let layers = object
        .get("layers")
        .and_then(Value::as_array)
        .ok_or_else(|| "container QC report requires a layers array".to_string())?;
    if layers.len() > 16 {
        return Err("container QC report contains more than 16 layers".into());
    }
    for (layer_index, layer) in layers.iter().enumerate() {
        let layer = layer
            .as_object()
            .ok_or_else(|| "container QC layer must be an object".to_string())?;
        let layer_name = layer
            .get("layer")
            .and_then(Value::as_str)
            .ok_or_else(|| "container QC layer requires a string layer".to_string())?;
        let checks = layer
            .get("checks")
            .and_then(Value::as_array)
            .ok_or_else(|| "container QC layer requires a checks array".to_string())?;
        if checks.len() > MAX_FINDINGS_PER_ASSET {
            return Err(format!(
                "container QC layer contains more than {MAX_FINDINGS_PER_ASSET} checks"
            ));
        }
        for (check_index, check) in checks.iter().enumerate() {
            let check = check
                .as_object()
                .ok_or_else(|| "container QC check must be an object".to_string())?;
            let passed = check
                .get("passed")
                .and_then(Value::as_bool)
                .ok_or_else(|| "container QC check requires a boolean passed".to_string())?;
            if passed {
                continue;
            }
            let rule_id = check
                .get("rule_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "container QC check requires a string rule_id".to_string())?;
            let message = check
                .get("message")
                .and_then(Value::as_str)
                .ok_or_else(|| "container QC check requires a string message".to_string())?;
            push_finding(
                explanations,
                FindingExplanation {
                    rule_id: rule_id.into(),
                    asset: asset.into(),
                    category: container_category(rule_id),
                    location: format!("{location}/layers/{layer_index}/checks/{check_index}"),
                    source: container_source(rule_id, format),
                    observation: json!({
                        "format": format,
                        "layer": layer_name,
                        "observed": check.get("observed").cloned().unwrap_or(Value::Null),
                    }),
                    requirement: format!(
                        "Satisfy {rule_id} for the {format} {layer_name} evidence."
                    ),
                    message: Some(message.into()),
                    remediation: container_remediation(rule_id, layer_name),
                },
            )?;
            if explanations.len() - before > MAX_FINDINGS_PER_ASSET {
                return Err(format!(
                    "container QC report contains more than {MAX_FINDINGS_PER_ASSET} failed rules"
                ));
            }
        }
    }
    Ok(())
}

fn container_category(rule_id: &str) -> FindingCategory {
    if [
        "FORGE-FLAC",
        "FORGE-MP3",
        "FORGE-AAC",
        "FORGE-AC3",
        "FORGE-EAC3",
        "FORGE-OPUS",
        "FORGE-VORBIS",
        "FORGE-IAMF",
    ]
    .iter()
    .any(|prefix| rule_id.starts_with(prefix))
    {
        FindingCategory::Codec
    } else {
        FindingCategory::Container
    }
}

fn container_source(rule_id: &str, format: &str) -> FindingSource {
    let (standard, url) = if rule_id.starts_with("FORGE-BWF") {
        (
            "EBU Tech 3285",
            Some("https://tech.ebu.ch/publications/tech3285"),
        )
    } else if rule_id.starts_with("FORGE-BS2088") {
        (
            "ITU-R BS.2088-2",
            Some("https://www.itu.int/rec/R-REC-BS.2088"),
        )
    } else if rule_id.starts_with("FORGE-IXML") {
        ("iXML 3.01", Some("https://www.gallery.co.uk/ixml/"))
    } else if rule_id.starts_with("FORGE-FLAC") {
        ("FLAC format", Some("https://xiph.org/flac/format.html"))
    } else if rule_id.starts_with("FORGE-OGG") {
        ("RFC 3533", Some("https://www.rfc-editor.org/rfc/rfc3533"))
    } else if rule_id.starts_with("FORGE-OPUS") {
        (
            "RFC 7845 / RFC 8486",
            Some("https://www.rfc-editor.org/rfc/rfc8486"),
        )
    } else if rule_id.starts_with("FORGE-MATROSKA") {
        ("RFC 9559", Some("https://www.rfc-editor.org/rfc/rfc9559"))
    } else if rule_id.starts_with("FORGE-IAMF") || rule_id.contains("IAMF") {
        (
            "AOMedia IAMF v1.1",
            Some("https://aomediacodec.github.io/iamf/"),
        )
    } else if rule_id.starts_with("FORGE-AC3") || rule_id.starts_with("FORGE-EAC3") {
        (
            "ETSI TS 102 366",
            Some("https://www.etsi.org/deliver/etsi_ts/102300_102399/102366/"),
        )
    } else if rule_id.starts_with("FORGE-MPEGTS") {
        (
            "ITU-T H.222.0",
            Some("https://www.itu.int/rec/T-REC-H.222.0"),
        )
    } else if rule_id.starts_with("FORGE-MXF") {
        ("SMPTE ST 377-1", Some("https://pub.smpte.org/doc/st377-1/"))
    } else if rule_id.starts_with("FORGE-AAF") {
        (
            "AAF specifications",
            Some("https://aafassociation.org/specs/"),
        )
    } else if rule_id.starts_with("FORGE-WAVE") {
        ("RIFF/WAVE", None)
    } else if rule_id.starts_with("FORGE-ISOBMFF") {
        ("ISO/IEC 14496-12", None)
    } else if rule_id.starts_with("FORGE-AAC") {
        ("ISO/IEC 14496-3", None)
    } else if rule_id.starts_with("FORGE-MP3") {
        ("ISO/IEC 11172-3 / 13818-3", None)
    } else {
        ("Forge container QC", Some(FORGE_QC_SOURCE))
    };
    FindingSource {
        profile: format!("{format} delivery"),
        standard: Some(standard.into()),
        standard_version: None,
        url: url.map(str::to_owned),
    }
}

fn container_remediation(rule_id: &str, layer: &str) -> String {
    let key = rule_id.to_ascii_lowercase();
    if key.contains("crc") || key.contains("checksum") || key.contains("hash") {
        "Regenerate or recover the damaged payload/metadata so the published integrity value matches, then re-audit."
            .into()
    } else if key.contains("timing")
        || key.contains("duration")
        || key.contains("timestamp")
        || key.contains("pts")
    {
        "Correct timestamps, sample counts, edit lists, or index timing and remux the delivery before re-audit."
            .into()
    } else if key.contains("channel") || key.contains("layout") || key.contains("assignment") {
        "Correct codec and container channel declarations/mapping so they agree with decoded audio, then re-audit."
            .into()
    } else if key.contains("loudness") || key.contains("dialnorm") || key.contains("replaygain") {
        "Correct the encoded loudness metadata from a verified final decode and repeat the metadata cross-check."
            .into()
    } else if key.contains("bound")
        || key.contains("size")
        || key.contains("structure")
        || key.contains("required")
    {
        format!(
            "Remux or regenerate the file so {layer} sizes, ordering, and required structures are valid."
        )
    } else {
        format!(
            "Correct the reported {layer} evidence, regenerate the delivery if necessary, and rerun the same rule."
        )
    }
}

#[derive(Debug, Deserialize)]
struct AdmRuleInput {
    rule_id: String,
    path: String,
    requirement: String,
    observed: String,
    passed: bool,
}

fn explain_asset_adm_profile(
    object: &Map<String, Value>,
    asset: &str,
    location: &str,
    asset_index: usize,
    explanations: &mut Vec<FindingExplanation>,
) -> Result<(), String> {
    let Some(values) = decode_embedded_array(
        object,
        "adm_production_profile_rules_json",
        asset_index,
        "ADM production profile rules",
    )?
    else {
        return Ok(());
    };
    if values.len() > MAX_FINDINGS_PER_ASSET {
        return Err(format!(
            "asset {} contains too many ADM profile rules",
            asset_index + 1
        ));
    }
    let standard = object
        .get("adm_production_profile_standard")
        .and_then(Value::as_str)
        .unwrap_or(crate::adm::PRODUCTION_PROFILE_STANDARD);
    let version = object
        .get("adm_production_profile_version")
        .and_then(Value::as_str);
    for (index, value) in values.into_iter().enumerate() {
        let rule: AdmRuleInput = serde_json::from_value(value).map_err(|error| {
            format!(
                "asset {} decode ADM production profile rule: {error}",
                asset_index + 1
            )
        })?;
        if rule.passed {
            continue;
        }
        push_adm_profile_finding(
            explanations,
            asset,
            rule,
            standard,
            version,
            format!("{location}/adm_production_profile_rules_json/{index}"),
        )?;
    }
    Ok(())
}

fn explain_adm_profile_report(
    object: &Map<String, Value>,
    asset: &str,
    location: &str,
    explanations: &mut Vec<FindingExplanation>,
) -> Result<(), String> {
    let values = object
        .get("rules")
        .and_then(Value::as_array)
        .ok_or_else(|| "ADM profile report requires a rules array".to_string())?;
    if values.len() > MAX_FINDINGS_PER_ASSET {
        return Err("ADM profile report contains too many rules".into());
    }
    let standard = object
        .get("standard")
        .and_then(Value::as_str)
        .unwrap_or(crate::adm::PRODUCTION_PROFILE_STANDARD);
    let version = object.get("profile_version").and_then(Value::as_str);
    for (index, value) in values.iter().cloned().enumerate() {
        let rule: AdmRuleInput = serde_json::from_value(value)
            .map_err(|error| format!("decode ADM production profile rule: {error}"))?;
        if rule.passed {
            continue;
        }
        push_adm_profile_finding(
            explanations,
            asset,
            rule,
            standard,
            version,
            format!("{location}/rules/{index}"),
        )?;
    }
    Ok(())
}

fn push_adm_profile_finding(
    explanations: &mut Vec<FindingExplanation>,
    asset: &str,
    rule: AdmRuleInput,
    standard: &str,
    version: Option<&str>,
    location: String,
) -> Result<(), String> {
    let (rule_standard, url) = if rule.rule_id.starts_with("BS2076") {
        ("ITU-R BS.2076-3", "https://www.itu.int/rec/R-REC-BS.2076")
    } else {
        (standard, "https://tech.ebu.ch/publications/tech3393")
    };
    let remediation = format!(
        "Correct the ADM evidence at {} to satisfy {}, then rerun the same production-profile validator.",
        rule.path, rule.rule_id
    );
    push_finding(
        explanations,
        FindingExplanation {
            rule_id: rule.rule_id,
            asset: asset.into(),
            category: FindingCategory::AdmProfile,
            location,
            source: FindingSource {
                profile: standard.into(),
                standard: Some(rule_standard.into()),
                standard_version: version.map(str::to_owned),
                url: Some(url.into()),
            },
            observation: json!({"path": rule.path, "observed": rule.observed}),
            requirement: rule.requirement,
            message: None,
            remediation,
        },
    )
}

fn explain_asset_adm(
    object: &Map<String, Value>,
    asset: &str,
    location: &str,
    asset_index: usize,
    explanations: &mut Vec<FindingExplanation>,
) -> Result<(), String> {
    let before = explanations.len();
    let source = FindingSource {
        profile: "ADM delivery".into(),
        standard: Some(
            object
                .get("adm_model_standard")
                .and_then(Value::as_str)
                .unwrap_or(crate::adm::ADM_STANDARD)
                .into(),
        ),
        standard_version: object
            .get("adm_model_version")
            .and_then(Value::as_str)
            .map(str::to_owned),
        url: Some("https://www.itu.int/rec/R-REC-BS.2076".into()),
    };
    for (field, rule_id, requirement, remediation) in [
        (
            "adm_axml_present",
            "BS2076-3-AXML-REQUIRED",
            "ADM delivery shall carry audioFormatExtended metadata in axml.",
            "Add or recover the ADM axml metadata and verify it against the audio tracks.",
        ),
        (
            "adm_chna_present",
            "BS2076-3-CHNA-REQUIRED",
            "Channel-based ADM delivery shall carry a chna track mapping.",
            "Add or repair chna track UID mappings so every PCM track resolves to ADM.",
        ),
    ] {
        if object.get(field).and_then(Value::as_bool) == Some(false) {
            push_finding(
                explanations,
                FindingExplanation {
                    rule_id: rule_id.into(),
                    asset: asset.into(),
                    category: FindingCategory::Adm,
                    location: format!("{location}/{field}"),
                    source: source.clone(),
                    observation: json!({"field": field, "value": false}),
                    requirement: requirement.into(),
                    message: None,
                    remediation: remediation.into(),
                },
            )?;
        }
    }
    if object
        .get("adm_render_validation_passed")
        .and_then(Value::as_bool)
        == Some(false)
    {
        let renderer_standard = object
            .get("adm_render_standard")
            .and_then(Value::as_str)
            .unwrap_or(crate::adm::RENDERER_STANDARD);
        let profile_standard = object
            .get("adm_render_profile")
            .and_then(Value::as_str)
            .unwrap_or(crate::adm::PROFILE_STANDARD);
        push_finding(
            explanations,
            FindingExplanation {
                rule_id: "FORGE-ADM-REFERENCE-RENDER-VALIDATION".into(),
                asset: asset.into(),
                category: FindingCategory::Adm,
                location: format!("{location}/adm_render_validation_passed"),
                source: FindingSource {
                    profile: profile_standard.into(),
                    standard: Some(renderer_standard.into()),
                    standard_version: None,
                    url: Some("https://www.itu.int/rec/R-REC-BS.2127".into()),
                },
                observation: json!({
                    "renderer": object.get("adm_render_renderer"),
                    "profile": profile_standard,
                    "profile_level": object.get("adm_render_profile_level"),
                    "layout": object.get("adm_render_layout"),
                    "output_path": object.get("adm_render_output_path"),
                    "validation_passed": false,
                }),
                requirement:
                    "The configured reference renderer shall validate and render the ADM presentation."
                        .into(),
                message: None,
                remediation:
                    "Correct ADM/profile errors reported by the reference renderer and rerun render validation."
                        .into(),
            },
        )?;
    }
    if let Some(values) = decode_embedded_array(
        object,
        "adm_presentations_json",
        asset_index,
        "ADM presentations",
    )? {
        if values.len() > MAX_FINDINGS_PER_ASSET {
            return Err(format!(
                "asset {} contains too many ADM presentations",
                asset_index + 1
            ));
        }
        for (index, value) in values.iter().enumerate() {
            let presentation = value.as_object().ok_or_else(|| {
                format!(
                    "asset {} ADM presentation must be an object",
                    asset_index + 1
                )
            })?;
            if presentation
                .get("referenced_by_axml")
                .and_then(Value::as_bool)
                == Some(false)
            {
                let id = presentation
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown-presentation");
                push_finding(
                    explanations,
                    FindingExplanation {
                        rule_id: "FORGE-ADM-PRESENTATION-REFERENCE".into(),
                        asset: asset.into(),
                        category: FindingCategory::Adm,
                        location: format!("{location}/adm_presentations_json/{index}"),
                        source: source.clone(),
                        observation: value.clone(),
                        requirement:
                            "Every configured ADM presentation shall be referenced by axml.".into(),
                        message: Some(format!("ADM presentation {id} is not referenced by axml")),
                        remediation:
                            "Correct the audioProgramme/content/object references or the presentation map, then rerun ADM QC."
                                .into(),
                    },
                )?;
            }
        }
    }
    if object.get("adm_qc_passed").and_then(Value::as_bool) == Some(false)
        && explanations.len() == before
    {
        push_finding(
            explanations,
            FindingExplanation {
                rule_id: "FORGE-ADM-QC".into(),
                asset: asset.into(),
                category: FindingCategory::Adm,
                location: format!("{location}/adm_qc_passed"),
                source,
                observation: json!({"adm_qc_passed": false}),
                requirement: "All requested ADM structure and presentation checks shall pass."
                    .into(),
                message: Some("The legacy report records an aggregate ADM failure only".into()),
                remediation:
                    "Rerun ADM QC with current Forge to capture granular rule evidence, then correct the reported metadata."
                        .into(),
            },
        )?;
    }
    Ok(())
}

fn explain_asset_codec(
    object: &Map<String, Value>,
    asset: &str,
    location: &str,
    explanations: &mut Vec<FindingExplanation>,
) -> Result<(), String> {
    let codec = object
        .get("codec")
        .and_then(Value::as_str)
        .unwrap_or("unknown-codec");
    let source = codec_source(codec);
    let tolerance = object.get("codec_qc_tolerance_lu").and_then(Value::as_f64);
    if object.get("codec_dialnorm_pass").and_then(Value::as_bool) == Some(false) {
        push_finding(
            explanations,
            FindingExplanation {
                rule_id: "FORGE-CODEC-DIALNORM".into(),
                asset: asset.into(),
                category: FindingCategory::Codec,
                location: format!("{location}/codec_dialnorm_pass"),
                source: source.clone(),
                observation: json!({
                    "codec": codec,
                    "loudness_basis": object.get("codec_loudness_basis"),
                    "dialnorm_lkfs": object.get("codec_dialnorm_lkfs"),
                    "deviation_lu": object.get("codec_dialnorm_deviation_lu"),
                    "tolerance_lu": tolerance,
                }),
                requirement: tolerance.map_or_else(
                    || "Codec dialnorm deviation shall be within the configured tolerance.".into(),
                    |value| format!("Absolute codec dialnorm deviation shall be at most {value:.3} LU."),
                ),
                message: None,
                remediation:
                    "Set dialnorm from the verified programme/dialogue measurement, re-encode, and re-probe the delivery."
                        .into(),
            },
        )?;
    }
    if object
        .get("codec_encoded_loudness_pass")
        .and_then(Value::as_bool)
        == Some(false)
    {
        push_finding(
            explanations,
            FindingExplanation {
                rule_id: "FORGE-CODEC-ENCODED-LOUDNESS".into(),
                asset: asset.into(),
                category: FindingCategory::Codec,
                location: format!("{location}/codec_encoded_loudness_pass"),
                source: source.clone(),
                observation: json!({
                    "codec": codec,
                    "encoded_loudness_lufs": object.get("codec_encoded_loudness_lufs"),
                    "deviation_lu": object.get("codec_encoded_loudness_deviation_lu"),
                    "tolerance_lu": tolerance,
                }),
                requirement: tolerance.map_or_else(
                    || "Encoded loudness metadata shall match the measured delivery within the configured tolerance.".into(),
                    |value| format!("Absolute encoded-loudness deviation shall be at most {value:.3} LU."),
                ),
                message: None,
                remediation:
                    "Rewrite encoded loudness metadata from the final decoded measurement and verify it after remuxing."
                        .into(),
            },
        )?;
    }
    if object.get("codec_roundtrip_pass").and_then(Value::as_bool) == Some(false) {
        explain_codec_roundtrip(
            object,
            asset,
            location,
            codec,
            tolerance,
            source,
            explanations,
        )?;
    }
    Ok(())
}

fn explain_codec_roundtrip(
    object: &Map<String, Value>,
    asset: &str,
    location: &str,
    codec: &str,
    tolerance: Option<f64>,
    source: FindingSource,
    explanations: &mut Vec<FindingExplanation>,
) -> Result<(), String> {
    let before = explanations.len();
    let metrics = [
        (
            "codec_loudness_drift_lu",
            "FORGE-CODEC-ROUNDTRIP-LOUDNESS",
            tolerance,
            "LU",
            "Adjust the encoder or master gain and remeasure the decoded output loudness.",
        ),
        (
            "codec_true_peak_drift_db",
            "FORGE-CODEC-ROUNDTRIP-TRUE-PEAK",
            tolerance,
            "dB",
            "Lower the pre-encode ceiling or change encoder settings, then remeasure decoded true peak.",
        ),
        (
            "codec_duration_drift_seconds",
            "FORGE-CODEC-ROUNDTRIP-DURATION",
            object
                .get("sample_rate_hz")
                .and_then(Value::as_f64)
                .filter(|rate| *rate > 0.0)
                .map(|rate| 1.0 / rate),
            "s",
            "Correct encoder delay/padding or container timing and verify sample-accurate decoded duration.",
        ),
    ];
    for (field, rule_id, limit, unit, remediation) in metrics {
        let Some(measured) = object.get(field).and_then(Value::as_f64) else {
            continue;
        };
        let Some(limit) = limit else {
            continue;
        };
        if measured.abs() <= limit {
            continue;
        }
        push_finding(
            explanations,
            FindingExplanation {
                rule_id: rule_id.into(),
                asset: asset.into(),
                category: FindingCategory::Codec,
                location: format!("{location}/{field}"),
                source: source.clone(),
                observation: json!({
                    "codec": codec,
                    "metric": field,
                    "measured": measured,
                    "unit": unit,
                    "maximum_absolute": limit,
                    "maximum_inclusive": true,
                    "reference_path": object.get("codec_reference_path"),
                }),
                requirement: format!(
                    "Absolute {} shall be at most {limit:.6} {unit}.",
                    field.trim_start_matches("codec_")
                ),
                message: None,
                remediation: remediation.into(),
            },
        )?;
    }
    if explanations.len() == before {
        push_finding(
            explanations,
            FindingExplanation {
                rule_id: "FORGE-CODEC-ROUNDTRIP".into(),
                asset: asset.into(),
                category: FindingCategory::Codec,
                location: format!("{location}/codec_roundtrip_pass"),
                source,
                observation: json!({
                    "codec": codec,
                    "loudness_drift_lu": object.get("codec_loudness_drift_lu"),
                    "true_peak_drift_db": object.get("codec_true_peak_drift_db"),
                    "duration_drift_seconds": object.get("codec_duration_drift_seconds"),
                    "tolerance_lu_db": tolerance,
                    "sample_rate_hz": object.get("sample_rate_hz"),
                }),
                requirement:
                    "Decoded codec loudness, true peak, and duration shall remain within the recorded tolerances."
                        .into(),
                message: Some(
                    "The report records an aggregate codec round-trip failure without enough boundary evidence to isolate a metric."
                        .into(),
                ),
                remediation:
                    "Rerun codec QC with current Forge to retain tolerances, then correct the failing encoder or timing metric."
                        .into(),
            },
        )?;
    }
    Ok(())
}

fn codec_source(codec: &str) -> FindingSource {
    let key = codec.to_ascii_lowercase();
    let (standard, url) = if key.contains("eac3") || key.contains("ac3") {
        (
            "ETSI TS 102 366",
            Some("https://www.etsi.org/deliver/etsi_ts/102300_102399/102366/"),
        )
    } else if key.contains("opus") {
        (
            "RFC 6716 / RFC 7845",
            Some("https://www.rfc-editor.org/rfc/rfc7845"),
        )
    } else if key.contains("flac") {
        ("FLAC format", Some("https://xiph.org/flac/format.html"))
    } else if key.contains("iamf") {
        (
            "AOMedia IAMF v1.1",
            Some("https://aomediacodec.github.io/iamf/"),
        )
    } else if key.contains("aac") {
        ("ISO/IEC 14496-3", None)
    } else if key.contains("mp3") {
        ("ISO/IEC 11172-3 / 13818-3", None)
    } else {
        ("Codec delivery metadata", None)
    };
    FindingSource {
        profile: codec.into(),
        standard: Some(standard.into()),
        standard_version: None,
        url: url.map(str::to_owned),
    }
}

fn explain_presentation_report(
    object: &Map<String, Value>,
    explanations: &mut Vec<FindingExplanation>,
) -> Result<usize, String> {
    let presentations = object
        .get("presentations")
        .and_then(Value::as_array)
        .ok_or_else(|| "presentation QC report requires a presentations array".to_string())?;
    ensure_asset_count(presentations.len())?;
    if presentations.len() > MAX_FINDINGS_PER_ASSET {
        return Err("presentation QC report contains too many presentations".into());
    }
    let standard = object
        .get("codec_standard")
        .and_then(Value::as_str)
        .unwrap_or("immersive codec presentation");
    let tolerance = object
        .get("reference_tolerance_lu_db")
        .and_then(Value::as_f64);
    let renderer = object.get("renderer").cloned().unwrap_or(Value::Null);
    for (index, presentation) in presentations.iter().enumerate() {
        let presentation = presentation
            .as_object()
            .ok_or_else(|| "presentation entry must be an object".to_string())?;
        let asset = presentation
            .get("rendered_path")
            .and_then(Value::as_str)
            .ok_or_else(|| "presentation entry requires a rendered_path".to_string())?;
        let before = explanations.len();
        explain_presentation_reference(
            presentation,
            asset,
            index,
            standard,
            tolerance,
            &renderer,
            explanations,
        )?;
        explain_presentation_compliance(presentation, asset, index, explanations)?;
        if presentation.get("passed").and_then(Value::as_bool) == Some(false)
            && explanations.len() == before
        {
            push_finding(
                explanations,
                FindingExplanation {
                    rule_id: "FORGE-PRESENTATION-QC".into(),
                    asset: asset.into(),
                    category: FindingCategory::Presentation,
                    location: format!("/presentations/{index}/passed"),
                    source: presentation_source(standard),
                    observation: Value::Object(presentation.clone()),
                    requirement:
                        "The rendered presentation shall pass reference and compliance checks."
                            .into(),
                    message: Some(
                        "The report records an aggregate presentation failure without granular evidence."
                            .into(),
                    ),
                    remediation:
                        "Rerun presentation QC with current Forge, then correct the renderer, metadata, or mix indicated by the granular failures."
                            .into(),
                },
            )?;
        }
        if explanations.len() - before > MAX_FINDINGS_PER_ASSET {
            return Err(format!(
                "presentation {index} contains more than {MAX_FINDINGS_PER_ASSET} failed rules"
            ));
        }
    }
    Ok(presentations.len())
}

fn explain_presentation_reference(
    presentation: &Map<String, Value>,
    asset: &str,
    index: usize,
    standard: &str,
    tolerance: Option<f64>,
    renderer: &Value,
    explanations: &mut Vec<FindingExplanation>,
) -> Result<(), String> {
    if presentation
        .get("reference_passed")
        .and_then(Value::as_bool)
        != Some(false)
    {
        return Ok(());
    }
    let before = explanations.len();
    let duration_tolerance = presentation
        .get("reference_duration_tolerance_seconds")
        .and_then(Value::as_f64);
    for (field, rule_id, limit, unit, remediation) in [
        (
            "reference_loudness_drift_lu",
            "FORGE-PRESENTATION-REFERENCE-LOUDNESS",
            tolerance,
            "LU",
            "Correct the presentation render or mix gain and remeasure against the reference.",
        ),
        (
            "reference_true_peak_drift_db",
            "FORGE-PRESENTATION-REFERENCE-TRUE-PEAK",
            tolerance,
            "dB",
            "Correct renderer/mix peak behaviour and remeasure decoded true peak against the reference.",
        ),
        (
            "reference_duration_drift_seconds",
            "FORGE-PRESENTATION-REFERENCE-DURATION",
            duration_tolerance,
            "s",
            "Correct renderer delay, padding, or edit timing and repeat the sample-accurate comparison.",
        ),
    ] {
        let Some(measured) = presentation.get(field).and_then(Value::as_f64) else {
            continue;
        };
        let Some(limit) = limit else {
            continue;
        };
        if measured.abs() <= limit {
            continue;
        }
        push_finding(
            explanations,
            FindingExplanation {
                rule_id: rule_id.into(),
                asset: asset.into(),
                category: FindingCategory::Presentation,
                location: format!("/presentations/{index}/{field}"),
                source: presentation_source(standard),
                observation: json!({
                    "presentation_id": presentation.get("id"),
                    "reference_path": presentation.get("reference_path"),
                    "renderer": renderer,
                    "metric": field,
                    "measured": measured,
                    "unit": unit,
                    "maximum_absolute": limit,
                    "maximum_inclusive": true,
                }),
                requirement: format!(
                    "Absolute {} shall be at most {limit:.6} {unit}.",
                    field.trim_start_matches("reference_")
                ),
                message: None,
                remediation: remediation.into(),
            },
        )?;
    }
    if explanations.len() == before {
        push_finding(
            explanations,
            FindingExplanation {
                rule_id: "FORGE-PRESENTATION-REFERENCE".into(),
                asset: asset.into(),
                category: FindingCategory::Presentation,
                location: format!("/presentations/{index}/reference_passed"),
                source: presentation_source(standard),
                observation: json!({
                    "presentation_id": presentation.get("id"),
                    "reference_path": presentation.get("reference_path"),
                    "renderer": renderer,
                    "loudness_drift_lu": presentation.get("reference_loudness_drift_lu"),
                    "true_peak_drift_db": presentation.get("reference_true_peak_drift_db"),
                    "duration_drift_seconds": presentation.get("reference_duration_drift_seconds"),
                    "tolerance_lu_db": tolerance,
                    "duration_tolerance_seconds": duration_tolerance,
                }),
                requirement:
                    "Rendered presentation loudness, true peak, and duration shall match the reference within recorded tolerances."
                        .into(),
                message: Some(
                    "The legacy report does not retain enough tolerance evidence to isolate the failed reference metric."
                        .into(),
                ),
                remediation:
                    "Rerun presentation QC with current Forge to retain exact tolerances, then correct the renderer or mix."
                        .into(),
            },
        )?;
    }
    Ok(())
}

fn explain_presentation_compliance(
    presentation: &Map<String, Value>,
    asset: &str,
    index: usize,
    explanations: &mut Vec<FindingExplanation>,
) -> Result<(), String> {
    let Some(compliance) = presentation.get("compliance") else {
        return Ok(());
    };
    let compliance = compliance
        .as_object()
        .ok_or_else(|| "presentation compliance must be an object".to_string())?;
    let profile = compliance
        .get("profile")
        .and_then(Value::as_str)
        .unwrap_or("unspecified-profile");
    let rules = compliance
        .get("rules")
        .and_then(Value::as_array)
        .ok_or_else(|| "presentation compliance requires a rules array".to_string())?;
    if rules.len() > MAX_COMPLIANCE_RULES_PER_ASSET {
        return Err("presentation compliance contains too many rules".into());
    }
    let builtin = crate::report::ComplianceProfile::builtin(profile);
    let source_standard = compliance
        .get("standard")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| builtin.as_ref().and_then(|value| value.standard.clone()));
    let source_version = compliance
        .get("standard_version")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            builtin
                .as_ref()
                .and_then(|value| value.standard_version.clone())
        });
    for (rule_index, value) in rules.iter().cloned().enumerate() {
        let rule: RuleInput = serde_json::from_value(value)
            .map_err(|error| format!("decode presentation compliance rule: {error}"))?;
        if rule.passed {
            continue;
        }
        push_finding(
            explanations,
            explain_compliance_rule_v2(
                asset,
                profile,
                source_standard.clone(),
                source_version.clone(),
                rule,
                format!("/presentations/{index}/compliance/rules/{rule_index}"),
                FindingCategory::Presentation,
            ),
        )?;
    }
    Ok(())
}

fn presentation_source(standard: &str) -> FindingSource {
    let url = if standard.contains("IAMF") {
        Some("https://aomediacodec.github.io/iamf/")
    } else if standard.contains("ETSI TS 102 366") {
        Some("https://www.etsi.org/deliver/etsi_ts/102300_102399/102366/")
    } else {
        None
    };
    FindingSource {
        profile: "externally rendered presentation".into(),
        standard: Some(standard.into()),
        standard_version: None,
        url: url.map(str::to_owned),
    }
}

fn decode_embedded_array(
    object: &Map<String, Value>,
    field: &str,
    asset_index: usize,
    label: &str,
) -> Result<Option<Vec<Value>>, String> {
    let Some(value) = object.get(field) else {
        return Ok(None);
    };
    match value {
        Value::Null => Ok(None),
        Value::String(encoded) if encoded.trim().is_empty() => Ok(None),
        Value::String(encoded) => {
            let decoded: Value = serde_json::from_str(encoded)
                .map_err(|error| format!("asset {} decode {label}: {error}", asset_index + 1))?;
            decoded
                .as_array()
                .cloned()
                .map(Some)
                .ok_or_else(|| format!("asset {} {label} must be an array", asset_index + 1))
        }
        Value::Array(values) => Ok(Some(values.clone())),
        _ => Err(format!(
            "asset {} {field} must be a string, array, or null",
            asset_index + 1
        )),
    }
}

fn push_finding(
    explanations: &mut Vec<FindingExplanation>,
    finding: FindingExplanation,
) -> Result<(), String> {
    if explanations.len() >= MAX_FINDINGS_PER_REPORT {
        return Err(format!(
            "report contains more than {MAX_FINDINGS_PER_REPORT} failed rules"
        ));
    }
    explanations.push(finding);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(schema: &str, asset: Value) -> Vec<u8> {
        let mut complete_asset = json!({
            "path": "programme.wav",
            "duration_seconds": 60.0,
            "source_start_seconds": 0.0,
            "sample_rate_hz": 48_000,
            "channels": 2,
            "sample_format": "s24",
            "integrated_lufs": -23.0,
            "max_momentary_lufs": -21.0,
            "max_short_term_lufs": -22.0,
            "loudness_range_lu": 4.0,
            "loudness_range_stable": true,
            "loudness_range_stable_after_seconds": 60.0,
            "rms_dbfs": -24.0,
            "sample_peak_dbfs": -2.0,
            "true_peak_dbtp": -1.5,
            "peak_to_loudness_ratio_lu": 21.5
        });
        complete_asset
            .as_object_mut()
            .unwrap()
            .extend(asset.as_object().unwrap().clone());
        serde_json::to_vec(&json!({
            "schema": schema,
            "generator": "forge-normalizer/0.90.0",
            "asset_count": 1,
            "passed_count": 0,
            "failed_count": 1,
            "assets": [complete_asset],
        }))
        .unwrap()
    }

    #[test]
    fn migrates_v2_flat_qc_to_v3_envelope() {
        let legacy_result = json!({
            "ebu_qc_id": "0005B",
            "version": "1.0",
            "name": "Clipping",
            "layer": "baseband",
            "passed": true,
            "calculated": true,
            "events": []
        });
        let input = manifest(
            DELIVERY_MANIFEST_V2,
            json!({
                "path": "programme.wav",
                "ebu_qc_results_json": serde_json::to_string(&[legacy_result]).unwrap()
            }),
        );
        let (value, summary) = migrate_delivery_manifest(&input).unwrap();
        assert_eq!(value["schema"], DELIVERY_MANIFEST_V3);
        assert_eq!(value["assets"][0]["qc"]["schema"], QC_SCHEMA);
        assert_eq!(value["assets"][0]["qc"]["results"][0]["rule_id"], "0005B");
        assert_eq!(
            value["assets"][0]["qc"]["results"][0]["source_url"],
            "https://qc.ebu.io/items/0005B/"
        );
        assert!(value["assets"][0]["qc"]["results"][0]["method"]
            .as_str()
            .unwrap()
            .contains("Migrated legacy"));
        assert_eq!(summary.migrated_qc_envelopes, 1);
        assert!(summary.changed);
    }

    #[test]
    fn migrates_v1_measurements_to_v3_without_data_loss() {
        let input = manifest(
            DELIVERY_MANIFEST_V1,
            json!({
                "path": "programme.wav",
                "adm_qc_passed": true
            }),
        );
        let (value, summary) = migrate_delivery_manifest(&input).unwrap();
        assert_eq!(value["schema"], DELIVERY_MANIFEST_V3);
        assert_eq!(value["assets"][0]["adm_qc_passed"], true);
        assert_eq!(summary.source_schema, DELIVERY_MANIFEST_V1);
        assert_eq!(summary.migrated_qc_envelopes, 0);
    }

    #[test]
    fn migration_is_idempotent_and_preserves_extension_evidence() {
        let input = manifest(
            DELIVERY_MANIFEST_V3,
            json!({
                "path": "programme.wav",
                "future_evidence": {"kept": true}
            }),
        );
        let (once, first) = migrate_delivery_manifest(&input).unwrap();
        let (twice, second) =
            migrate_delivery_manifest(&serde_json::to_vec(&once).unwrap()).unwrap();
        assert_eq!(once, twice);
        assert!(!first.changed);
        assert!(!second.changed);
        assert_eq!(twice["assets"][0]["future_evidence"]["kept"], true);
    }

    #[test]
    fn repairs_historical_v3_qc_v1_envelope() {
        let input = manifest(
            DELIVERY_MANIFEST_V3,
            json!({
                "path": "programme.wav",
                "qc": {
                    "schema": EBU_QC_SCHEMA_V1,
                    "results": [{
                        "ebu_qc_id": "0005B",
                        "version": "1.0",
                        "name": "Clipping",
                        "layer": "baseband",
                        "passed": true,
                        "calculated": true,
                        "events": []
                    }]
                }
            }),
        );
        let (value, summary) = migrate_delivery_manifest(&input).unwrap();
        assert_eq!(value["assets"][0]["qc"]["schema"], QC_SCHEMA);
        assert_eq!(value["assets"][0]["qc"]["results"][0]["rule_id"], "0005B");
        assert!(summary.changed);
    }

    #[test]
    fn migration_rejects_unknown_schema_and_inconsistent_counts() {
        let unknown = manifest("https://example.invalid/v9", json!({"path": "x.wav"}));
        assert!(migrate_delivery_manifest(&unknown)
            .unwrap_err()
            .contains("unsupported"));
        let inconsistent = serde_json::to_vec(&json!({
            "schema": DELIVERY_MANIFEST_V2,
            "generator": "forge-normalizer/0.90.0",
            "asset_count": 2,
            "passed_count": 1,
            "failed_count": 1,
            "assets": [{"path": "x.wav"}],
        }))
        .unwrap();
        assert!(migrate_delivery_manifest(&inconsistent)
            .unwrap_err()
            .contains("asset_count"));
    }

    #[test]
    fn report_resource_limits_are_explicit() {
        assert!(ensure_report_size(MAX_REPORT_BYTES + 1)
            .unwrap_err()
            .contains("limit"));
        assert!(ensure_asset_count(MAX_ASSETS + 1)
            .unwrap_err()
            .contains("limit"));
        let oversized_rules = vec![
            json!({
                "metric": "true_peak_dbtp",
                "measured": -0.5,
                "maximum": -1.0,
                "passed": false
            });
            MAX_COMPLIANCE_RULES_PER_ASSET + 1
        ];
        let input = serde_json::to_vec(&json!([{
            "path": "programme.wav",
            "compliance_rules_json": oversized_rules
        }]))
        .unwrap();
        assert!(explain_failed_rules(&input)
            .unwrap_err()
            .contains("compliance rules"));
    }

    #[test]
    fn explains_failed_rules_with_boundary_source_and_remediation() {
        let rules = serde_json::to_string(&json!([
            {
                "metric": "true_peak_dbtp",
                "measured": -0.5,
                "minimum": null,
                "maximum": -1.0,
                "minimum_inclusive": null,
                "maximum_inclusive": true,
                "passed": false
            },
            {
                "metric": "integrated_lufs",
                "measured": -23.0,
                "minimum": -23.2,
                "maximum": -22.8,
                "minimum_inclusive": true,
                "maximum_inclusive": true,
                "passed": true
            }
        ]))
        .unwrap();
        let input = manifest(
            DELIVERY_MANIFEST_V3,
            json!({
                "path": "programme.wav",
                "compliance_profile": "ebu-r128",
                "compliance_standard": "EBU R 128",
                "compliance_standard_version": "5.0 (2023)",
                "compliance_rules_json": rules
            }),
        );
        let report = explain_failed_rules(&input).unwrap();
        assert_eq!(report.failed_rule_count, 1);
        let explanation = &report.explanations[0];
        assert_eq!(explanation.rule_id, "FORGE-COMPLIANCE-TRUE-PEAK-DBTP");
        assert_eq!(explanation.observation.measured, -0.5);
        assert_eq!(explanation.observation.maximum, Some(-1.0));
        assert_eq!(
            explanation.source.url,
            Some("https://tech.ebu.ch/publications/r128")
        );
        assert!(explanation.remediation.contains("true-peak"));
    }
}
