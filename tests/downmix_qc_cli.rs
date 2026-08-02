use forge_normalizer::downmix::Layout;
use forge_normalizer::wav::{AudioBuffer, PcmKind, WavWriter};
use serde_json::Value;
use std::fs;
use std::process::Command;

fn write_fixture(path: &std::path::Path, layout: Layout, value: f32) {
    let frames = 48_000;
    let audio = AudioBuffer {
        sample_rate: 48_000,
        channels: layout.channels() as u16,
        frames,
        data: vec![vec![value; frames]; layout.channels()],
        channel_roles: layout.roles(),
        source_kind: PcmKind::F32,
    };
    WavWriter::write(path, &audio, PcmKind::F32, false).unwrap();
}

#[test]
fn cli_reports_explicit_immersive_mappings_and_deltas() {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("source-7.1.4.wav");
    let spec = work.path().join("downmix.json");
    let report = work.path().join("downmix-report.json");
    write_fixture(&source, Layout::SevenOneFour, 0.01);
    fs::write(
        &spec,
        r#"{
          "schema_version": 1,
          "source": "source-7.1.4.wav",
          "input_layout": "7.1.4",
          "profiles": ["stereo", "5.1", "7.1.4"]
        }"#,
    )
    .unwrap();
    let request_value: Value = serde_json::from_slice(&fs::read(&spec).unwrap()).unwrap();
    let request_schema: Value =
        serde_json::from_str(include_str!("../schema/downmix-qc-request-v1.schema.json")).unwrap();
    assert!(jsonschema::validator_for(&request_schema)
        .unwrap()
        .is_valid(&request_value));

    let output = Command::new(env!("CARGO_BIN_EXE_forge-downmix-qc"))
        .arg(&spec)
        .args(["--output", report.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("PASS"));
    let value: Value = serde_json::from_slice(&fs::read(report).unwrap()).unwrap();
    let report_schema: Value =
        serde_json::from_str(include_str!("../schema/downmix-qc-report-v1.schema.json")).unwrap();
    assert!(jsonschema::validator_for(&report_schema)
        .unwrap()
        .is_valid(&value));
    assert_eq!(value["validator"], "forge-immersive-downmix-qc-1");
    assert_eq!(value["source_measurement"]["layout"], "7.1.4");
    assert_eq!(value["profiles"][1]["target_layout"], "5.1");
    assert_eq!(
        value["profiles"][1]["mapping"][4]["terms"][1]["input_label"],
        "SL"
    );
    assert_eq!(
        value["profiles"][2]["mapping"][0]["terms"][0]["coefficient"],
        1.0
    );
    assert!(value["profiles"][0]["loudness_delta_lu"].is_number());
    assert_eq!(value["passed"], true);
}

#[test]
fn cli_returns_failure_for_sample_clipping_and_keeps_json_evidence() {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("hot-5.1.wav");
    let spec = work.path().join("hot.json");
    write_fixture(&source, Layout::FiveOne, 0.9);
    fs::write(
        &spec,
        r#"{
          "schema_version": 1,
          "source": "hot-5.1.wav",
          "input_layout": "5.1",
          "profiles": ["stereo"]
        }"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_forge-downmix-qc"))
        .arg(&spec)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["passed"], false);
    assert_eq!(value["profiles"][0]["clip_risk"], "sample-clipping");
    assert!(value["profiles"][0]["clipped_samples"].as_u64().unwrap() > 0);
}
