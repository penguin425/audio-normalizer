//! Deterministic delivery-manifest comparison for CI quality gates.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

pub const RESULT_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/manifest-comparison-v1";

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompareOptions {
    pub loudness_tolerance_lu: f64,
    pub true_peak_tolerance_db: f64,
    pub loudness_range_tolerance_lu: f64,
    pub duration_tolerance_seconds: f64,
    pub allow_missing_metrics: bool,
    pub allow_new_assets: bool,
}

impl Default for CompareOptions {
    fn default() -> Self {
        Self {
            loudness_tolerance_lu: 0.1,
            true_peak_tolerance_db: 0.1,
            loudness_range_tolerance_lu: 0.2,
            duration_tolerance_seconds: 0.001,
            allow_missing_metrics: false,
            allow_new_assets: true,
        }
    }
}

impl CompareOptions {
    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("loudness_tolerance_lu", self.loudness_tolerance_lu),
            ("true_peak_tolerance_db", self.true_peak_tolerance_db),
            (
                "loudness_range_tolerance_lu",
                self.loudness_range_tolerance_lu,
            ),
            (
                "duration_tolerance_seconds",
                self.duration_tolerance_seconds,
            ),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(format!("{name} must be finite and non-negative"));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FindingLevel {
    Error,
    Warning,
    Note,
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub level: FindingLevel,
    pub rule_id: String,
    pub asset: String,
    pub metric: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tolerance: Option<f64>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Comparison {
    pub schema: &'static str,
    pub generator: &'static str,
    pub baseline_schema: String,
    pub candidate_schema: String,
    pub passed: bool,
    pub compared_assets: usize,
    pub compared_asset_paths: Vec<String>,
    pub error_count: usize,
    pub warning_count: usize,
    pub findings: Vec<Finding>,
}

#[derive(Deserialize)]
struct Manifest {
    schema: String,
    assets: Vec<Map<String, Value>>,
}

pub fn compare_manifests(
    baseline_bytes: &[u8],
    candidate_bytes: &[u8],
    options: &CompareOptions,
) -> Result<Comparison, String> {
    options.validate()?;
    let baseline: Manifest = serde_json::from_slice(baseline_bytes)
        .map_err(|error| format!("decode baseline manifest: {error}"))?;
    let candidate: Manifest = serde_json::from_slice(candidate_bytes)
        .map_err(|error| format!("decode candidate manifest: {error}"))?;
    if !baseline.schema.contains("delivery-manifest-")
        || !candidate.schema.contains("delivery-manifest-")
    {
        return Err("both inputs must be Forge delivery manifests".into());
    }

    let baseline_assets = index_assets(baseline.assets, "baseline")?;
    let candidate_assets = index_assets(candidate.assets, "candidate")?;
    let mut findings = Vec::new();

    for (path, baseline_asset) in &baseline_assets {
        let Some(candidate_asset) = candidate_assets.get(path) else {
            findings.push(Finding {
                level: FindingLevel::Error,
                rule_id: "FORGE-COMPARE-ASSET-MISSING".into(),
                asset: path.clone(),
                metric: "path".into(),
                baseline: Some(Value::String(path.clone())),
                candidate: None,
                tolerance: None,
                message: "asset present in baseline is missing from candidate".into(),
            });
            continue;
        };
        compare_asset(
            path,
            baseline_asset,
            candidate_asset,
            options,
            &mut findings,
        );
    }

    for path in candidate_assets.keys() {
        if !baseline_assets.contains_key(path) {
            findings.push(Finding {
                level: if options.allow_new_assets {
                    FindingLevel::Note
                } else {
                    FindingLevel::Error
                },
                rule_id: "FORGE-COMPARE-ASSET-NEW".into(),
                asset: path.clone(),
                metric: "path".into(),
                baseline: None,
                candidate: Some(Value::String(path.clone())),
                tolerance: None,
                message: "candidate contains an asset absent from baseline".into(),
            });
        }
    }

    findings.sort_by(|left, right| {
        left.asset
            .cmp(&right.asset)
            .then_with(|| left.rule_id.cmp(&right.rule_id))
            .then_with(|| left.metric.cmp(&right.metric))
    });
    let error_count = findings
        .iter()
        .filter(|finding| finding.level == FindingLevel::Error)
        .count();
    let warning_count = findings
        .iter()
        .filter(|finding| finding.level == FindingLevel::Warning)
        .count();
    let compared_asset_paths = baseline_assets
        .keys()
        .filter(|path| candidate_assets.contains_key(*path))
        .cloned()
        .collect::<Vec<_>>();
    Ok(Comparison {
        schema: RESULT_SCHEMA,
        generator: concat!("forge-normalizer/", env!("CARGO_PKG_VERSION")),
        baseline_schema: baseline.schema,
        candidate_schema: candidate.schema,
        passed: error_count == 0,
        compared_assets: compared_asset_paths.len(),
        compared_asset_paths,
        error_count,
        warning_count,
        findings,
    })
}

fn index_assets(
    assets: Vec<Map<String, Value>>,
    label: &str,
) -> Result<BTreeMap<String, Map<String, Value>>, String> {
    let mut indexed = BTreeMap::new();
    for asset in assets {
        let path = asset
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{label} asset has no string path"))?
            .to_owned();
        if indexed.insert(path.clone(), asset).is_some() {
            return Err(format!(
                "{label} manifest contains duplicate asset path {path}"
            ));
        }
    }
    Ok(indexed)
}

fn compare_asset(
    path: &str,
    baseline: &Map<String, Value>,
    candidate: &Map<String, Value>,
    options: &CompareOptions,
    findings: &mut Vec<Finding>,
) {
    for metric in [
        "sample_rate_hz",
        "channels",
        "sample_format",
        "codec",
        "codec_container",
        "codec_profile",
        "codec_channel_layout",
        "adm_production_profile_standard",
        "adm_production_profile_version",
        "adm_production_profile_level",
    ] {
        compare_exact(path, metric, baseline, candidate, options, findings);
    }

    for metric in [
        "integrated_lufs",
        "dialogue_lufs",
        "max_momentary_lufs",
        "max_short_term_lufs",
        "downmix_integrated_lufs",
        "codec_dialnorm_lkfs",
        "codec_encoded_loudness_lufs",
        "codec_loudness_drift_lu",
        "adm_render_integrated_lufs",
    ] {
        compare_number(
            path,
            metric,
            options.loudness_tolerance_lu,
            baseline,
            candidate,
            options,
            findings,
        );
    }
    for metric in [
        "true_peak_dbtp",
        "sample_peak_dbfs",
        "downmix_true_peak_dbtp",
        "codec_true_peak_drift_db",
        "adm_render_true_peak_dbtp",
    ] {
        compare_number(
            path,
            metric,
            options.true_peak_tolerance_db,
            baseline,
            candidate,
            options,
            findings,
        );
    }
    compare_number(
        path,
        "loudness_range_lu",
        options.loudness_range_tolerance_lu,
        baseline,
        candidate,
        options,
        findings,
    );
    for metric in [
        "duration_seconds",
        "dialogue_duration_seconds",
        "codec_duration_drift_seconds",
    ] {
        compare_number(
            path,
            metric,
            options.duration_tolerance_seconds,
            baseline,
            candidate,
            options,
            findings,
        );
    }

    for metric in [
        "compliance_passed",
        "codec_dialnorm_pass",
        "codec_encoded_loudness_pass",
        "codec_roundtrip_pass",
        "adm_qc_passed",
        "adm_production_profile_passed",
        "adm_render_validation_passed",
        "ebu_qc_passed",
    ] {
        compare_pass(path, metric, baseline, candidate, options, findings);
    }
    compare_nested_pass(
        path,
        "container_qc.passed",
        baseline.get("container_qc"),
        candidate.get("container_qc"),
        options,
        findings,
    );
    compare_qc_results(path, baseline, candidate, findings);
    compare_container_qc_results(path, baseline, candidate, findings);
}

fn compare_nested_pass(
    path: &str,
    metric: &str,
    baseline: Option<&Value>,
    candidate: Option<&Value>,
    options: &CompareOptions,
    findings: &mut Vec<Finding>,
) {
    let before = baseline.and_then(|value| value.get("passed"));
    let after = candidate.and_then(|value| value.get("passed"));
    match (
        before.and_then(Value::as_bool),
        after.and_then(Value::as_bool),
    ) {
        (Some(true), Some(false)) | (None, Some(false)) => findings.push(Finding {
            level: FindingLevel::Error,
            rule_id: "FORGE-COMPARE-NEW-FAILURE".into(),
            asset: path.into(),
            metric: metric.into(),
            baseline: before.cloned(),
            candidate: after.cloned(),
            tolerance: None,
            message: format!("{metric} changed from pass to fail"),
        }),
        (Some(_), None) if !options.allow_missing_metrics => findings.push(missing_finding(
            path,
            metric,
            before.expect("baseline nested pass exists"),
            "QC evidence",
        )),
        _ => {}
    }
}

fn compare_exact(
    path: &str,
    metric: &str,
    baseline: &Map<String, Value>,
    candidate: &Map<String, Value>,
    options: &CompareOptions,
    findings: &mut Vec<Finding>,
) {
    let before = baseline.get(metric).filter(|value| !value.is_null());
    let after = candidate.get(metric).filter(|value| !value.is_null());
    match (before, after) {
        (Some(before), Some(after)) if before != after => findings.push(Finding {
            level: FindingLevel::Error,
            rule_id: "FORGE-COMPARE-FORMAT-CHANGED".into(),
            asset: path.into(),
            metric: metric.into(),
            baseline: Some(before.clone()),
            candidate: Some(after.clone()),
            tolerance: None,
            message: format!("{metric} changed"),
        }),
        (Some(before), None) if !options.allow_missing_metrics => {
            findings.push(missing_finding(path, metric, before, "format field"))
        }
        _ => {}
    }
}

fn compare_number(
    path: &str,
    metric: &str,
    tolerance: f64,
    baseline: &Map<String, Value>,
    candidate: &Map<String, Value>,
    options: &CompareOptions,
    findings: &mut Vec<Finding>,
) {
    let before_value = baseline.get(metric).filter(|value| !value.is_null());
    let after_value = candidate.get(metric).filter(|value| !value.is_null());
    match (
        before_value.and_then(Value::as_f64),
        after_value.and_then(Value::as_f64),
    ) {
        (Some(before), Some(after)) if (after - before).abs() > tolerance => {
            findings.push(Finding {
                level: FindingLevel::Error,
                rule_id: "FORGE-COMPARE-METRIC-DRIFT".into(),
                asset: path.into(),
                metric: metric.into(),
                baseline: before_value.cloned(),
                candidate: after_value.cloned(),
                tolerance: Some(tolerance),
                message: format!(
                    "{metric} drifted by {:.6}, exceeding tolerance {tolerance:.6}",
                    after - before
                ),
            });
        }
        (Some(_), None) if !options.allow_missing_metrics => findings.push(missing_finding(
            path,
            metric,
            before_value.expect("numeric baseline value exists"),
            "numeric metric",
        )),
        _ => {}
    }
}

fn compare_pass(
    path: &str,
    metric: &str,
    baseline: &Map<String, Value>,
    candidate: &Map<String, Value>,
    options: &CompareOptions,
    findings: &mut Vec<Finding>,
) {
    let before = baseline.get(metric).filter(|value| !value.is_null());
    let after = candidate.get(metric).filter(|value| !value.is_null());
    match (
        before.and_then(Value::as_bool),
        after.and_then(Value::as_bool),
    ) {
        (Some(true), Some(false)) | (None, Some(false)) => findings.push(Finding {
            level: FindingLevel::Error,
            rule_id: "FORGE-COMPARE-NEW-FAILURE".into(),
            asset: path.into(),
            metric: metric.into(),
            baseline: before.cloned(),
            candidate: after.cloned(),
            tolerance: None,
            message: format!("{metric} newly fails"),
        }),
        (Some(_), None) if !options.allow_missing_metrics => findings.push(missing_finding(
            path,
            metric,
            before.expect("boolean baseline value exists"),
            "pass/fail evidence",
        )),
        _ => {}
    }
}

fn missing_finding(path: &str, metric: &str, before: &Value, kind: &str) -> Finding {
    Finding {
        level: FindingLevel::Error,
        rule_id: "FORGE-COMPARE-EVIDENCE-MISSING".into(),
        asset: path.into(),
        metric: metric.into(),
        baseline: Some(before.clone()),
        candidate: None,
        tolerance: None,
        message: format!("candidate is missing baseline {kind} {metric}"),
    }
}

fn compare_qc_results(
    path: &str,
    baseline: &Map<String, Value>,
    candidate: &Map<String, Value>,
    findings: &mut Vec<Finding>,
) {
    let before = qc_passes(baseline);
    let after = qc_passes(candidate);
    for (id, before_passed) in before {
        match after.get(&id) {
            Some(false) if before_passed => findings.push(Finding {
                level: FindingLevel::Error,
                rule_id: "FORGE-COMPARE-NEW-QC-FAILURE".into(),
                asset: path.into(),
                metric: format!("qc.{id}"),
                baseline: Some(Value::Bool(true)),
                candidate: Some(Value::Bool(false)),
                tolerance: None,
                message: format!("EBU QC rule {id} newly fails"),
            }),
            None => findings.push(Finding {
                level: FindingLevel::Error,
                rule_id: "FORGE-COMPARE-QC-EVIDENCE-MISSING".into(),
                asset: path.into(),
                metric: format!("qc.{id}"),
                baseline: Some(Value::Bool(before_passed)),
                candidate: None,
                tolerance: None,
                message: format!("candidate is missing EBU QC rule {id}"),
            }),
            _ => {}
        }
    }
}

fn qc_passes(asset: &Map<String, Value>) -> BTreeMap<String, bool> {
    asset
        .get("qc")
        .and_then(|qc| qc.get("results"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|result| {
            Some((
                result.get("ebu_qc_id")?.as_str()?.to_owned(),
                result.get("passed")?.as_bool()?,
            ))
        })
        .collect()
}

fn compare_container_qc_results(
    path: &str,
    baseline: &Map<String, Value>,
    candidate: &Map<String, Value>,
    findings: &mut Vec<Finding>,
) {
    let before = container_qc_passes(baseline);
    let after = container_qc_passes(candidate);
    for (id, before_passed) in before {
        match after.get(&id) {
            Some(false) if before_passed => findings.push(Finding {
                level: FindingLevel::Error,
                rule_id: "FORGE-COMPARE-NEW-CONTAINER-QC-FAILURE".into(),
                asset: path.into(),
                metric: format!("container_qc.{id}"),
                baseline: Some(Value::Bool(true)),
                candidate: Some(Value::Bool(false)),
                tolerance: None,
                message: format!("container QC rule {id} newly fails"),
            }),
            None => findings.push(Finding {
                level: FindingLevel::Error,
                rule_id: "FORGE-COMPARE-CONTAINER-QC-EVIDENCE-MISSING".into(),
                asset: path.into(),
                metric: format!("container_qc.{id}"),
                baseline: Some(Value::Bool(before_passed)),
                candidate: None,
                tolerance: None,
                message: format!("candidate is missing container QC rule {id}"),
            }),
            _ => {}
        }
    }
}

fn container_qc_passes(asset: &Map<String, Value>) -> BTreeMap<String, bool> {
    asset
        .get("container_qc")
        .and_then(|audit| audit.get("layers"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|layer| layer.get("checks").and_then(Value::as_array))
        .flatten()
        .filter_map(|result| {
            Some((
                result.get("rule_id")?.as_str()?.to_owned(),
                result.get("passed")?.as_bool()?,
            ))
        })
        .collect()
}

pub fn write_json<W: Write>(writer: W, comparison: &Comparison) -> Result<(), String> {
    serde_json::to_writer_pretty(writer, comparison)
        .map_err(|error| format!("write comparison JSON: {error}"))
}

pub fn write_sarif<W: Write>(writer: W, comparison: &Comparison) -> Result<(), String> {
    let rules: BTreeSet<_> = comparison
        .findings
        .iter()
        .map(|finding| finding.rule_id.clone())
        .collect();
    let sarif = json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": {"driver": {
                "name": "forge-compare",
                "version": env!("CARGO_PKG_VERSION"),
                "informationUri": "https://github.com/penguin425/audio-normalizer",
                "rules": rules.into_iter().map(|id| json!({
                    "id": id,
                    "shortDescription": {"text": "Forge audio delivery regression"}
                })).collect::<Vec<_>>()
            }},
            "results": comparison.findings.iter().map(|finding| json!({
                "ruleId": finding.rule_id,
                "level": match finding.level {
                    FindingLevel::Error => "error",
                    FindingLevel::Warning => "warning",
                    FindingLevel::Note => "note",
                },
                "message": {"text": finding.message},
                "locations": [{
                    "physicalLocation": {"artifactLocation": {"uri": finding.asset}},
                    "logicalLocations": [{"name": finding.metric}]
                }],
                "properties": {
                    "baseline": finding.baseline,
                    "candidate": finding.candidate,
                    "tolerance": finding.tolerance
                }
            })).collect::<Vec<_>>()
        }]
    });
    serde_json::to_writer_pretty(writer, &sarif)
        .map_err(|error| format!("write comparison SARIF: {error}"))
}

pub fn write_junit<W: Write>(mut writer: W, comparison: &Comparison) -> Result<(), String> {
    let mut by_asset: BTreeMap<&str, Vec<&Finding>> = comparison
        .compared_asset_paths
        .iter()
        .map(|path| (path.as_str(), Vec::new()))
        .collect();
    for finding in &comparison.findings {
        by_asset.entry(&finding.asset).or_default().push(finding);
    }
    let failing_assets = by_asset
        .values()
        .filter(|findings| {
            findings
                .iter()
                .any(|finding| finding.level == FindingLevel::Error)
        })
        .count();
    writeln!(writer, r#"<?xml version="1.0" encoding="UTF-8"?>"#)
        .and_then(|_| {
            writeln!(
                writer,
                r#"<testsuite name="forge-compare" tests="{}" failures="{}">"#,
                by_asset.len(),
                failing_assets
            )
        })
        .map_err(|error| format!("write comparison JUnit: {error}"))?;
    for asset in by_asset.keys().copied() {
        let asset_findings = &by_asset[asset];
        writeln!(
            writer,
            r#"  <testcase classname="forge.delivery" name="{}">"#,
            escape_xml(asset)
        )
        .map_err(|error| format!("write comparison JUnit: {error}"))?;
        for finding in asset_findings
            .iter()
            .filter(|finding| finding.level == FindingLevel::Error)
        {
            writeln!(
                writer,
                r#"    <failure type="{}" message="{}">{}</failure>"#,
                escape_xml(&finding.rule_id),
                escape_xml(&finding.message),
                escape_xml(&format!(
                    "{}: baseline={:?}, candidate={:?}",
                    finding.metric, finding.baseline, finding.candidate
                ))
            )
            .map_err(|error| format!("write comparison JUnit: {error}"))?;
        }
        writeln!(writer, "  </testcase>")
            .map_err(|error| format!("write comparison JUnit: {error}"))?;
    }
    writeln!(writer, "</testsuite>").map_err(|error| format!("write comparison JUnit: {error}"))
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn manifest(asset: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "schema": "https://penguin425.github.io/audio-normalizer/schema/delivery-manifest-v2",
            "assets": [asset]
        }))
        .unwrap()
    }

    #[test]
    fn detects_metric_format_and_pass_regressions_deterministically() {
        let baseline = manifest(json!({
            "path": "programme.wav",
            "integrated_lufs": -23.0,
            "true_peak_dbtp": -1.5,
            "sample_rate_hz": 48000,
            "compliance_passed": true
        }));
        let candidate = manifest(json!({
            "path": "programme.wav",
            "integrated_lufs": -22.7,
            "true_peak_dbtp": -1.5,
            "sample_rate_hz": 44100,
            "compliance_passed": false
        }));
        let result = compare_manifests(&baseline, &candidate, &CompareOptions::default()).unwrap();
        assert!(!result.passed);
        assert_eq!(result.error_count, 3);
        assert_eq!(
            result
                .findings
                .iter()
                .map(|finding| finding.rule_id.as_str())
                .collect::<Vec<_>>(),
            [
                "FORGE-COMPARE-FORMAT-CHANGED",
                "FORGE-COMPARE-METRIC-DRIFT",
                "FORGE-COMPARE-NEW-FAILURE",
            ]
        );
    }

    #[test]
    fn writers_emit_ci_formats() {
        let bytes = manifest(json!({
            "path": "programme.wav",
            "integrated_lufs": -23.0
        }));
        let result = compare_manifests(&bytes, &bytes, &CompareOptions::default()).unwrap();
        let mut sarif = Vec::new();
        write_sarif(&mut sarif, &result).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&sarif).unwrap()["version"],
            "2.1.0"
        );
        let mut junit = Vec::new();
        write_junit(&mut junit, &result).unwrap();
        assert!(String::from_utf8(junit).unwrap().contains("<testsuite"));
    }

    #[test]
    fn detects_container_qc_regressions_and_missing_rules() {
        let baseline = manifest(json!({
            "path": "programme.wav",
            "container_qc": {
                "passed": true,
                "layers": [{"checks": [
                    {"rule_id": "FORGE-WAVE-RIFF-SIZE", "passed": true},
                    {"rule_id": "FORGE-WAVE-FMT-REQUIRED", "passed": true}
                ]}]
            }
        }));
        let candidate = manifest(json!({
            "path": "programme.wav",
            "container_qc": {
                "passed": false,
                "layers": [{"checks": [
                    {"rule_id": "FORGE-WAVE-RIFF-SIZE", "passed": false}
                ]}]
            }
        }));
        let result = compare_manifests(&baseline, &candidate, &CompareOptions::default()).unwrap();
        assert!(!result.passed);
        assert!(result.findings.iter().any(|finding| {
            finding.rule_id == "FORGE-COMPARE-NEW-CONTAINER-QC-FAILURE"
                && finding.metric == "container_qc.FORGE-WAVE-RIFF-SIZE"
        }));
        assert!(result.findings.iter().any(|finding| {
            finding.rule_id == "FORGE-COMPARE-CONTAINER-QC-EVIDENCE-MISSING"
                && finding.metric == "container_qc.FORGE-WAVE-FMT-REQUIRED"
        }));
    }

    proptest! {
        #[test]
        fn arbitrary_manifest_bytes_never_panic(
            baseline in proptest::collection::vec(any::<u8>(), 0..8192),
            candidate in proptest::collection::vec(any::<u8>(), 0..8192),
        ) {
            let _ = compare_manifests(&baseline, &candidate, &CompareOptions::default());
        }
    }
}
