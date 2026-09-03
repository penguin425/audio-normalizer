use serde_json::{json, Value};
use std::fs;
use std::process::Command;

const V2: &str = "https://penguin425.github.io/audio-normalizer/schema/delivery-manifest-v2";
const V3: &str = "https://penguin425.github.io/audio-normalizer/schema/delivery-manifest-v3";
const V4: &str = "https://penguin425.github.io/audio-normalizer/schema/delivery-manifest-v4";

fn legacy_manifest(path: &std::path::Path) {
    let rules = serde_json::to_string(&json!([
        {
            "metric": "integrated_lufs",
            "measured": -20.0,
            "minimum": -23.2,
            "maximum": -22.8,
            "minimum_inclusive": true,
            "maximum_inclusive": true,
            "passed": false
        },
        {
            "metric": "true_peak_dbtp",
            "measured": -1.5,
            "minimum": null,
            "maximum": -1.0,
            "minimum_inclusive": null,
            "maximum_inclusive": true,
            "passed": true
        }
    ]))
    .unwrap();
    fs::write(
        path,
        serde_json::to_vec_pretty(&json!({
            "schema": V2,
            "generator": "forge-normalizer/0.80.0",
            "asset_count": 1,
            "passed_count": 0,
            "failed_count": 1,
            "assets": [{
                "path": "programme.wav",
                "duration_seconds": 60.0,
                "source_start_seconds": 0.0,
                "sample_rate_hz": 48_000,
                "channels": 2,
                "sample_format": "s24",
                "integrated_lufs": -20.0,
                "max_momentary_lufs": -18.0,
                "max_short_term_lufs": -19.0,
                "loudness_range_lu": 4.0,
                "loudness_range_stable": true,
                "loudness_range_stable_after_seconds": 60.0,
                "rms_dbfs": -21.0,
                "sample_peak_dbfs": -2.0,
                "true_peak_dbtp": -1.5,
                "peak_to_loudness_ratio_lu": 18.5,
                "compliance_profile": "ebu-r128",
                "compliance_standard": "EBU R 128",
                "compliance_standard_version": "5.0 (2023)",
                "compliance_rules_json": rules,
                "ebu_qc_results_json": "[]"
            }]
        }))
        .unwrap(),
    )
    .unwrap();
}

#[test]
fn migrate_check_and_atomic_in_place_are_idempotent() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("delivery.json");
    legacy_manifest(&input);

    let check = Command::new(env!("CARGO_BIN_EXE_forge-report"))
        .args(["migrate"])
        .arg(&input)
        .arg("--check")
        .output()
        .unwrap();
    assert_eq!(
        check.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    assert!(String::from_utf8_lossy(&check.stderr).contains("migration required"));

    let migrated = Command::new(env!("CARGO_BIN_EXE_forge-report"))
        .args(["migrate"])
        .arg(&input)
        .arg("--in-place")
        .output()
        .unwrap();
    assert!(migrated.status.success());
    let value: Value = serde_json::from_slice(&fs::read(&input).unwrap()).unwrap();
    assert_eq!(value["schema"], V4);
    assert_eq!(
        value["assets"][0]["qc"]["schema"],
        forge_normalizer::qc::QC_SCHEMA
    );
    assert_eq!(value["assets"][0]["qc"]["results"], json!([]));

    let current = Command::new(env!("CARGO_BIN_EXE_forge-report"))
        .args(["migrate"])
        .arg(&input)
        .arg("--check")
        .output()
        .unwrap();
    assert!(current.status.success());
    assert!(String::from_utf8_lossy(&current.stderr).contains("current"));
}

#[test]
fn migrate_refuses_existing_output_without_overwrite() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("delivery.json");
    let output = directory.path().join("migrated.json");
    legacy_manifest(&input);
    fs::write(&output, b"keep").unwrap();

    let refused = Command::new(env!("CARGO_BIN_EXE_forge-report"))
        .args(["migrate"])
        .arg(&input)
        .args(["--output"])
        .arg(&output)
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(2));
    assert_eq!(fs::read(&output).unwrap(), b"keep");
}

#[test]
fn explain_emits_text_and_versioned_json() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("delivery.json");
    legacy_manifest(&input);

    let text = Command::new(env!("CARGO_BIN_EXE_forge-report"))
        .args(["explain"])
        .arg(&input)
        .output()
        .unwrap();
    assert!(text.status.success());
    let stdout = String::from_utf8_lossy(&text.stdout);
    assert!(stdout.contains("FORGE-COMPLIANCE-INTEGRATED-LUFS"));
    assert!(stdout.contains("source: ebu-r128; EBU R 128 5.0 (2023)"));
    assert!(stdout.contains(r#""metric":"integrated_lufs""#));
    assert!(stdout.contains(r#""measured":-20.0"#));
    assert!(stdout.contains("remediation: Adjust programme gain"));
    assert!(!stdout.contains("FORGE-COMPLIANCE-TRUE-PEAK-DBTP"));

    let json_output = Command::new(env!("CARGO_BIN_EXE_forge-report"))
        .args(["explain"])
        .arg(&input)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(json_output.status.success());
    let report: Value = serde_json::from_slice(&json_output.stdout).unwrap();
    assert_eq!(
        report["schema"],
        "https://penguin425.github.io/audio-normalizer/schema/rule-explanations-v2"
    );
    assert_eq!(report["failed_rule_count"], 1);
    assert_eq!(report["explanations"][0]["category"], "compliance");
    assert_eq!(
        report["explanations"][0]["source"]["url"],
        "https://tech.ebu.ch/publications/r128"
    );
    assert_eq!(report["explanations"][0]["observation"]["measured"], -20.0);

    let v1_output = Command::new(env!("CARGO_BIN_EXE_forge-report"))
        .args(["explain"])
        .arg(&input)
        .args(["--scope", "compliance", "--format", "json"])
        .output()
        .unwrap();
    assert!(v1_output.status.success());
    let v1: Value = serde_json::from_slice(&v1_output.stdout).unwrap();
    assert_eq!(
        v1["schema"],
        "https://penguin425.github.io/audio-normalizer/schema/rule-explanations-v1"
    );
    assert_eq!(v1["failed_rule_count"], 1);
}

#[test]
fn explain_covers_every_manifest_qc_category_without_duplicates() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("all-qc.json");
    let compliance = json!([{
        "metric": "integrated_lufs",
        "measured": -18.0,
        "minimum": -23.2,
        "maximum": -22.8,
        "minimum_inclusive": true,
        "maximum_inclusive": true,
        "passed": false
    }]);
    let adm_rules = json!([
        {
            "rule_id": "TECH3393-2025-PROFILE-IDENTIFIER",
            "path": "/audioFormatExtended/profileList/profile",
            "requirement": "one profile shall contain EBU Tech 3393",
            "observed": "not present",
            "passed": false
        },
        {
            "rule_id": "TECH3393-XML",
            "path": "/",
            "requirement": "well-formed XML",
            "observed": "well-formed",
            "passed": true
        }
    ]);
    fs::write(
        &input,
        serde_json::to_vec_pretty(&json!([{
            "path": "delivery.wav",
            "sample_rate_hz": 48_000,
            "compliance_profile": "ebu-r128",
            "compliance_standard": "EBU R 128",
            "compliance_standard_version": "5.0 (2023)",
            "compliance_rules_json": compliance,
            "qc": {
                "schema": forge_normalizer::qc::QC_SCHEMA,
                "results": [
                    {
                        "rule_id": "0010B",
                        "ebu_qc_id": "0010B",
                        "version": "1.0",
                        "name": "Audio Clipping",
                        "layer": "baseband",
                        "passed": false,
                        "calculated": true,
                        "source_url": "https://qc.ebu.io/items/0010B/",
                        "method": "detect three consecutive full-scale samples",
                        "events_truncated": false,
                        "events": [{
                            "channel": 1,
                            "start_seconds": 1.0,
                            "end_seconds": 1.001,
                            "measured": 1.0,
                            "unit": "linear"
                        }]
                    },
                    {
                        "rule_id": "0084B",
                        "ebu_qc_id": "0084B",
                        "version": "1.0",
                        "name": "Silence",
                        "layer": "baseband",
                        "passed": true,
                        "calculated": true,
                        "source_url": "https://qc.ebu.io/items/0084B/",
                        "method": "silence detector",
                        "events": []
                    }
                ]
            },
            "container_qc": {
                "schema": forge_normalizer::container_qc::CONTAINER_QC_SCHEMA,
                "generator": "forge-normalizer/0.92.0",
                "path": "delivery.wav",
                "format": "wave",
                "passed": false,
                "layers": [
                    {
                        "layer": "wrapper",
                        "passed": false,
                        "checks": [{
                            "rule_id": "FORGE-WAVE-RIFF-SIZE",
                            "passed": false,
                            "message": "RIFF size does not match file size",
                            "observed": {"declared": 10, "actual": 20}
                        }]
                    },
                    {
                        "layer": "bitstream",
                        "passed": false,
                        "checks": [{
                            "rule_id": "FORGE-AAC-ADTS-BOUNDS",
                            "passed": false,
                            "message": "ADTS frame exceeds payload",
                            "observed": {"offset": 44}
                        }]
                    },
                    {"layer": "x-check", "passed": true, "checks": []}
                ],
                "properties": {}
            },
            "adm_axml_present": false,
            "adm_chna_present": true,
            "adm_qc_passed": false,
            "adm_presentations_json": [{
                "id": "APR_1001",
                "name": "Main",
                "channels": [1, 2],
                "integrated_lufs": -23.0,
                "true_peak_dbtp": -2.0,
                "render_method": "channel-map",
                "referenced_by_axml": false
            }],
            "adm_render_renderer": "itu-reference-renderer",
            "adm_render_standard": "ITU-R BS.2127-1",
            "adm_render_profile": "ITU-R BS.2168",
            "adm_render_profile_level": 1,
            "adm_render_layout": "stereo",
            "adm_render_validation_passed": false,
            "adm_render_output_path": "rendered.wav",
            "adm_production_profile_standard": "EBU Tech 3393",
            "adm_production_profile_version": "1.0",
            "adm_production_profile_rules_json": adm_rules,
            "codec": "eac3",
            "codec_loudness_basis": "programme",
            "codec_dialnorm_lkfs": -24.0,
            "codec_dialnorm_deviation_lu": 2.0,
            "codec_dialnorm_pass": false,
            "codec_encoded_loudness_pass": true,
            "codec_qc_tolerance_lu": 0.5,
            "codec_reference_path": "reference.wav",
            "codec_loudness_drift_lu": 1.2,
            "codec_true_peak_drift_db": 0.1,
            "codec_duration_drift_seconds": 0.01,
            "codec_roundtrip_pass": false
        }]))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_forge-report"))
        .args(["explain"])
        .arg(&input)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["failed_rule_count"], 11);
    let explanations = report["explanations"].as_array().unwrap();
    let ids = explanations
        .iter()
        .map(|item| item["rule_id"].as_str().unwrap())
        .collect::<Vec<_>>();
    for expected in [
        "FORGE-COMPLIANCE-INTEGRATED-LUFS",
        "0010B",
        "FORGE-WAVE-RIFF-SIZE",
        "FORGE-AAC-ADTS-BOUNDS",
        "TECH3393-2025-PROFILE-IDENTIFIER",
        "BS2076-3-AXML-REQUIRED",
        "FORGE-ADM-PRESENTATION-REFERENCE",
        "FORGE-ADM-REFERENCE-RENDER-VALIDATION",
        "FORGE-CODEC-DIALNORM",
        "FORGE-CODEC-ROUNDTRIP-LOUDNESS",
        "FORGE-CODEC-ROUNDTRIP-DURATION",
    ] {
        assert!(ids.contains(&expected), "missing {expected}: {ids:#?}");
    }
    assert_eq!(ids.iter().filter(|id| **id == "0010B").count(), 1);
    let categories = explanations
        .iter()
        .map(|item| item["category"].as_str().unwrap())
        .collect::<std::collections::HashSet<_>>();
    for category in [
        "compliance",
        "ebu_qc",
        "container",
        "codec",
        "adm",
        "adm_profile",
    ] {
        assert!(categories.contains(category));
    }
    let clipping = explanations
        .iter()
        .find(|item| item["rule_id"] == "0010B")
        .unwrap();
    assert_eq!(clipping["observation"]["events"][0]["channel"], 1);
    assert_eq!(clipping["source"]["url"], "https://qc.ebu.io/items/0010B/");
    assert!(clipping["remediation"]
        .as_str()
        .unwrap()
        .contains("clipped"));
    let render = explanations
        .iter()
        .find(|item| item["rule_id"] == "FORGE-ADM-REFERENCE-RENDER-VALIDATION")
        .unwrap();
    assert_eq!(
        render["source"]["standard"],
        forge_normalizer::adm::RENDERER_STANDARD
    );
    assert_eq!(render["observation"]["renderer"], "itu-reference-renderer");
}

#[test]
fn explain_accepts_legacy_ebu_qc_ids_without_rule_ids() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("legacy-qc.json");
    fs::write(
        &input,
        serde_json::to_vec(&json!([{
            "path": "legacy.wav",
            "qc": {
                "schema": "https://penguin425.github.io/audio-normalizer/schema/ebu-qc-results-v1",
                "results": [{
                    "ebu_qc_id": "0010B",
                    "version": "1.0",
                    "name": "Audio Clipping",
                    "layer": "baseband",
                    "passed": false,
                    "calculated": true,
                    "method": "detect three consecutive full-scale samples",
                    "events": [{
                        "channel": 1,
                        "start_seconds": 1.0,
                        "end_seconds": 1.001
                    }]
                }]
            }
        }]))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_forge-report"))
        .args(["explain"])
        .arg(&input)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["failed_rule_count"], 1);
    assert_eq!(report["explanations"][0]["rule_id"], "0010B");
    assert_eq!(report["explanations"][0]["category"], "ebu_qc");
    assert_eq!(
        report["explanations"][0]["source"]["url"],
        "https://qc.ebu.io/items/0010B/"
    );
}

#[test]
fn explain_accepts_standalone_container_and_adm_profile_reports() {
    let directory = tempfile::tempdir().unwrap();
    let container = directory.path().join("container.json");
    fs::write(
        &container,
        serde_json::to_vec(&json!({
            "schema": forge_normalizer::container_qc::CONTAINER_QC_SCHEMA,
            "generator": "forge-normalizer/0.92.0",
            "path": "broken.mkv",
            "format": "matroska",
            "passed": false,
            "layers": [
                {"layer": "wrapper", "passed": false, "checks": [{
                    "rule_id": "FORGE-MATROSKA-CRC32",
                    "passed": false,
                    "message": "CRC-32 mismatch",
                    "observed": {"offset": 128}
                }]},
                {"layer": "bitstream", "passed": true, "checks": []},
                {"layer": "x-check", "passed": true, "checks": []}
            ],
            "properties": {}
        }))
        .unwrap(),
    )
    .unwrap();
    let container_output = Command::new(env!("CARGO_BIN_EXE_forge-report"))
        .args(["explain"])
        .arg(&container)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(container_output.status.success());
    let report: Value = serde_json::from_slice(&container_output.stdout).unwrap();
    assert_eq!(report["asset_count"], 1);
    assert_eq!(report["explanations"][0]["category"], "container");
    assert_eq!(report["explanations"][0]["source"]["standard"], "RFC 9559");

    let adm = directory.path().join("adm.json");
    fs::write(
        &adm,
        serde_json::to_vec(&json!({
            "path": "production.wav",
            "standard": "EBU Tech 3393",
            "adm_standard": "ITU-R BS.2076-3",
            "adm_version": "ITU-R_BS.2076-3",
            "profile_name": "EBU Production Profile",
            "profile_version": "1.0",
            "profile_level": "1",
            "mode": "read",
            "validator": forge_normalizer::adm::PRODUCTION_VALIDATOR,
            "passed": false,
            "rules": [{
                "rule_id": "BS2076-3-ID-SYNTAX",
                "path": "/audioFormatExtended",
                "requirement": "ADM IDs shall use valid syntax",
                "observed": "invalid APR_bad",
                "passed": false
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    let adm_output = Command::new(env!("CARGO_BIN_EXE_forge-report"))
        .args(["explain"])
        .arg(&adm)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(adm_output.status.success());
    let report: Value = serde_json::from_slice(&adm_output.stdout).unwrap();
    assert_eq!(report["explanations"][0]["asset"], "production.wav");
    assert_eq!(report["explanations"][0]["category"], "adm_profile");
    assert_eq!(
        report["explanations"][0]["observation"]["observed"],
        "invalid APR_bad"
    );
}

#[test]
fn explain_isolates_failed_presentation_reference_and_compliance_metrics() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("presentations.json");
    fs::write(
        &input,
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "validator": forge_normalizer::presentation_qc::VALIDATOR,
            "codec": "iamf",
            "codec_standard": "AOMedia IAMF v1.1 / Open Audio Renderer v1.0.0",
            "renderer": {"name": "oar", "version": "1.0.0"},
            "source_spec": "spec.json",
            "reference_tolerance_lu_db": 0.1,
            "presentation_count": 1,
            "passed": false,
            "presentations": [{
                "id": "main",
                "rendered_path": "main.wav",
                "reference_path": "reference.wav",
                "integrated_lufs": -21.0,
                "true_peak_dbtp": -0.5,
                "duration_seconds": 60.001,
                "channels": 2,
                "reference_loudness_drift_lu": 0.2,
                "reference_true_peak_drift_db": 0.05,
                "reference_duration_drift_seconds": 0.001,
                "reference_duration_tolerance_seconds": 0.000020833333333333333,
                "reference_passed": false,
                "compliance": {
                    "profile": "ebu-r128",
                    "passed": false,
                    "rules": [{
                        "metric": "true_peak_dbtp",
                        "measured": -0.5,
                        "minimum": null,
                        "maximum": -1.0,
                        "minimum_inclusive": null,
                        "maximum_inclusive": true,
                        "passed": false
                    }]
                },
                "passed": false
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-report"))
        .args(["explain"])
        .arg(&input)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["asset_count"], 1);
    assert_eq!(report["failed_rule_count"], 3);
    let ids = report["explanations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["rule_id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(ids.contains(&"FORGE-PRESENTATION-REFERENCE-LOUDNESS"));
    assert!(ids.contains(&"FORGE-PRESENTATION-REFERENCE-DURATION"));
    assert!(ids.contains(&"FORGE-COMPLIANCE-TRUE-PEAK-DBTP"));
    assert!(!ids.contains(&"FORGE-PRESENTATION-REFERENCE-TRUE-PEAK"));
    assert!(report["explanations"]
        .as_array()
        .unwrap()
        .iter()
        .all(|item| item["category"] == "presentation"));
    let compliance = report["explanations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["rule_id"] == "FORGE-COMPLIANCE-TRUE-PEAK-DBTP")
        .unwrap();
    assert_eq!(compliance["source"]["standard"], "EBU R 128");
    assert_eq!(compliance["source"]["standard_version"], "5.0 (2023)");
}

#[test]
fn migrate_rejects_inconsistent_counts() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("bad.json");
    fs::write(
        &input,
        serde_json::to_vec(&json!({
            "schema": V2,
            "generator": "forge-normalizer/0.80.0",
            "asset_count": 2,
            "passed_count": 1,
            "failed_count": 1,
            "assets": [{"path": "one.wav"}]
        }))
        .unwrap(),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-report"))
        .args(["migrate"])
        .arg(&input)
        .arg("--check")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("asset_count"));
}

#[test]
fn explain_reports_model_qc_as_non_normative_with_stable_rule_id() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("model-manifest.json");
    let audit = json!({
        "schema": forge_normalizer::anomaly_provider::AUDIT_SCHEMA,
        "schema_version": 1,
        "adapter": forge_normalizer::anomaly_provider::ADAPTER,
        "source_path": "programme.wav",
        "source_sha256": "b".repeat(64),
        "provider": "reviewed-detector",
        "provider_version": "2.0",
        "model": "audio-quality",
        "model_version": "2026-08",
        "model_sha256": "a".repeat(64),
        "source_duration_seconds": 10.0,
        "sample_rate_hz": 48_000,
        "confidence_threshold": 0.6,
        "severity_threshold": 0.5,
        "input_event_count": 1,
        "selected_event_count": 1,
        "selected_event_duration_seconds": 0.1,
        "selected_by_kind": {"pop": 1},
        "passed": false,
        "events": [{
            "index": 1,
            "kind": "pop",
            "start_seconds": 1.0,
            "end_seconds": 1.1,
            "confidence": 0.9,
            "severity": 0.8,
            "channel": 1,
            "related_channel": null,
            "evidence_label": "impulse-spectrum",
            "selected": true
        }]
    });
    fs::write(
        &input,
        serde_json::to_vec(&json!({
            "schema": V3,
            "generator": "forge-normalizer/0.112.0",
            "asset_count": 1,
            "passed_count": 1,
            "failed_count": 0,
            "assets": [{
                "path": "programme.wav",
                "model_qc": {
                    "schema": forge_normalizer::report_tools::MODEL_QC_SCHEMA,
                    "layer": "model-qc",
                    "classification": "non-normative-model-evidence",
                    "passed": false,
                    "audit": audit
                }
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_forge-report"))
        .args(["explain"])
        .arg(&input)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["failed_rule_count"], 1);
    assert_eq!(report["explanations"][0]["category"], "model_qc");
    assert_eq!(
        report["explanations"][0]["rule_id"],
        "FORGE-MODEL-ANOMALY-POP"
    );
    assert_eq!(
        report["explanations"][0]["source"]["standard"],
        "non-normative model evidence"
    );
    assert!(report["explanations"][0]["requirement"]
        .as_str()
        .unwrap()
        .contains("does not change EBU/ITU compliance"));
}
