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
const EBU_QC_SCHEMA_V1: &str =
    "https://penguin425.github.io/audio-normalizer/schema/ebu-qc-results-v1";
const DELIVERY_MANIFEST_V1_SCHEMA: &str =
    include_str!("../schema/delivery-manifest-v1.schema.json");
const DELIVERY_MANIFEST_V2_SCHEMA: &str =
    include_str!("../schema/delivery-manifest-v2.schema.json");
const DELIVERY_MANIFEST_V3_SCHEMA: &str =
    include_str!("../schema/delivery-manifest-v3.schema.json");
pub const MAX_REPORT_BYTES: usize = 64 * 1024 * 1024;
const MAX_ASSETS: usize = 100_000;
const MAX_QC_RESULTS_PER_ASSET: usize = 10_000;
const MAX_COMPLIANCE_RULES_PER_ASSET: usize = 1_000;

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
