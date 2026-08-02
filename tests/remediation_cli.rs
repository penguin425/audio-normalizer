use forge_normalizer::wav::{AudioBuffer, PcmKind, WavWriter};
use serde_json::Value;
use std::fs;
use std::process::Command;

fn write_fixture(path: &std::path::Path, frames: usize, value: f32) {
    let audio = AudioBuffer {
        sample_rate: 48_000,
        channels: 2,
        frames,
        data: vec![vec![value; frames], vec![value; frames]],
        channel_roles: forge_normalizer::wav::default_channel_roles(2),
        source_kind: PcmKind::F32,
    };
    WavWriter::write(path, &audio, PcmKind::F32, false).unwrap();
}

#[test]
fn cli_emits_schema_valid_plan_without_touching_source() {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("source.wav");
    let request = work.path().join("request.json");
    let report_path = work.path().join("report.json");
    write_fixture(&source, 48_000, 0.05);
    let before = fs::read(&source).unwrap();
    fs::write(
        &request,
        r#"{
          "schema_version": 1,
          "source": "source.wav",
          "target_lufs": -40.0,
          "true_peak_ceiling_dbtp": -1.0
        }"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_forge-remediate"))
        .args([
            "--output",
            report_path.to_str().unwrap(),
            request.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("FEASIBLE"));
    assert_eq!(before, fs::read(&source).unwrap());

    let report: Value = serde_json::from_slice(&fs::read(report_path).unwrap()).unwrap();
    let schema: Value =
        serde_json::from_str(include_str!("../schema/remediation-report-v1.schema.json")).unwrap();
    assert!(jsonschema::validator_for(&schema)
        .unwrap()
        .is_valid(&report));
    assert_eq!(report["validator"], "forge-smart-remediation-1");
    assert_eq!(report["requires_audio_write"], true);
    assert_eq!(report["plan"]["actions"][0]["kind"], "static-gain");
    assert_eq!(report["source_sha256"].as_str().unwrap().len(), 64);
}

#[test]
fn cli_returns_json_failure_when_static_gain_cap_is_exceeded() {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("source.wav");
    let request = work.path().join("request.json");
    write_fixture(&source, 48_000, 0.001);
    fs::write(
        &request,
        r#"{
          "schema_version": 1,
          "source": "source.wav",
          "target_lufs": -1.0,
          "max_static_gain_db": 1.0
        }"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_forge-remediate"))
        .arg(&request)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:#?}");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["feasible"], false);
    assert!(!report["plan"]["infeasibility_reasons"]
        .as_array()
        .unwrap()
        .is_empty());
}
