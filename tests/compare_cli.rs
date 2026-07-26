use serde_json::{json, Value};
use std::fs;
use std::process::Command;

fn write_manifest(path: &std::path::Path, loudness: f64) {
    fs::write(
        path,
        serde_json::to_vec(&json!({
            "schema": "https://penguin425.github.io/audio-normalizer/schema/delivery-manifest-v2",
            "assets": [{
                "path": "programme.wav",
                "integrated_lufs": loudness,
                "true_peak_dbtp": -1.2,
                "sample_rate_hz": 48_000,
                "channels": 2,
                "sample_format": "s24",
                "compliance_passed": true
            }]
        }))
        .unwrap(),
    )
    .unwrap();
}

#[test]
fn compare_cli_returns_gate_status_and_writes_sarif() {
    let directory = tempfile::tempdir().unwrap();
    let baseline = directory.path().join("baseline.json");
    let candidate = directory.path().join("candidate.json");
    let sarif = directory.path().join("result.sarif");
    write_manifest(&baseline, -23.0);
    write_manifest(&candidate, -22.7);

    let output = Command::new(env!("CARGO_BIN_EXE_forge-compare"))
        .arg(&baseline)
        .arg(&candidate)
        .args(["--format", "sarif", "--output"])
        .arg(&sarif)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("FAIL"));
    let report: Value = serde_json::from_slice(&fs::read(sarif).unwrap()).unwrap();
    assert_eq!(report["version"], "2.1.0");
    assert_eq!(
        report["runs"][0]["results"][0]["ruleId"],
        "FORGE-COMPARE-METRIC-DRIFT"
    );

    write_manifest(&candidate, -23.05);
    let output = Command::new(env!("CARGO_BIN_EXE_forge-compare"))
        .arg(&baseline)
        .arg(&candidate)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("PASS"));
}
