use serde_json::Value;
use std::process::Command;

#[test]
fn json_report_matches_the_versioned_schema() {
    let output = Command::new(env!("CARGO_BIN_EXE_forge-doctor"))
        .arg("--json")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    assert!(output.stderr.is_empty(), "{output:#?}");

    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let schema: Value =
        serde_json::from_str(include_str!("../schema/doctor-report-v1.schema.json")).unwrap();
    assert!(jsonschema::validator_for(&schema)
        .unwrap()
        .is_valid(&report));
    assert_eq!(report["schema_version"], "forge-doctor-v1");
    assert_eq!(
        report["generator"],
        format!("forge-normalizer/{}", env!("CARGO_PKG_VERSION"))
    );
    assert!(report["parallelism"].as_u64().unwrap() >= 1);
    assert!(report["formats"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["format"] == "wav" && item["read"] == true && item["write"] == true));
}

#[test]
fn required_builtin_capabilities_succeed() {
    let output = Command::new(env!("CARGO_BIN_EXE_forge-doctor"))
        .args(["--json", "--require", "read:wav", "--require", "write:flac"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["ok"], true);
    assert_eq!(report["requirements"].as_array().unwrap().len(), 2);
}

#[test]
fn recognized_but_unavailable_capability_exits_one_with_report() {
    let output = Command::new(env!("CARGO_BIN_EXE_forge-doctor"))
        .args(["--json", "--require", "write:dsf"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:#?}");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["ok"], false);
    assert_eq!(report["requirements"][0]["id"], "write:dsf");
    assert_eq!(report["requirements"][0]["available"], false);
}

#[test]
fn unknown_capability_is_a_usage_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_forge-doctor"))
        .args(["--require", "codec:unknown"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:#?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown capability"));
}

#[test]
fn human_report_is_compact_and_actionable() {
    let output = Command::new(env!("CARGO_BIN_EXE_forge-doctor"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Build features:"));
    assert!(stdout.contains("Runtime:"));
    assert!(stdout.contains("Formats:"));
    assert!(stdout.contains("Result: READY"));
}
