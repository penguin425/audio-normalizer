use serde_json::{json, Value};
use std::fs;
use std::process::Command;

const V2: &str = "https://penguin425.github.io/audio-normalizer/schema/delivery-manifest-v2";
const V3: &str = "https://penguin425.github.io/audio-normalizer/schema/delivery-manifest-v3";

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
    assert_eq!(value["schema"], V3);
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
    assert!(stdout.contains("observation: integrated_lufs = -20.00 LUFS"));
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
        "https://penguin425.github.io/audio-normalizer/schema/rule-explanations-v1"
    );
    assert_eq!(report["failed_rule_count"], 1);
    assert_eq!(
        report["explanations"][0]["source"]["url"],
        "https://tech.ebu.ch/publications/r128"
    );
    assert_eq!(report["explanations"][0]["observation"]["measured"], -20.0);
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
