use forge_normalizer::downmix::Layout;
use forge_normalizer::wav::{AudioBuffer, PcmKind, WavWriter};
use serde_json::Value;
use std::fs;
use std::process::Command;

fn write_fixture(path: &std::path::Path, layout: Layout, frames: usize, value: f32) {
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

fn evidence() -> &'static str {
    r#"{
      "name": "reference-hrtf",
      "version": "1.2.3",
      "renderer_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
      "model": "studio-hrtf",
      "model_version": "2026.1",
      "model_sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    }"#
}

#[test]
fn cli_reports_renderer_identity_and_reference_drift() {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("source-7.1.4.wav");
    let rendered = work.path().join("rendered.wav");
    let reference = work.path().join("reference.wav");
    let spec = work.path().join("binaural.json");
    let report_path = work.path().join("report.json");
    write_fixture(&source, Layout::SevenOneFour, 48_000, 0.01);
    write_fixture(&rendered, Layout::Stereo, 48_000, 0.01);
    write_fixture(&reference, Layout::Stereo, 48_000, 0.01);
    fs::write(
        &spec,
        format!(
            r#"{{
              "schema_version": 1,
              "source": "source-7.1.4.wav",
              "rendered": "rendered.wav",
              "reference": "reference.wav",
              "input_layout": "7.1.4",
              "renderer": {}
            }}"#,
            evidence()
        ),
    )
    .unwrap();
    let request: Value = serde_json::from_slice(&fs::read(&spec).unwrap()).unwrap();
    let request_schema: Value =
        serde_json::from_str(include_str!("../schema/binaural-qc-request-v1.schema.json")).unwrap();
    assert!(jsonschema::validator_for(&request_schema)
        .unwrap()
        .is_valid(&request));

    let output = Command::new(env!("CARGO_BIN_EXE_forge-binaural-qc"))
        .args([
            "--output",
            report_path.to_str().unwrap(),
            spec.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("PASS"));
    let report: Value = serde_json::from_slice(&fs::read(report_path).unwrap()).unwrap();
    let report_schema: Value =
        serde_json::from_str(include_str!("../schema/binaural-qc-report-v1.schema.json")).unwrap();
    assert!(jsonschema::validator_for(&report_schema)
        .unwrap()
        .is_valid(&report));
    assert_eq!(report["validator"], "forge-binaural-qc-1");
    assert_eq!(report["output_layout"], "binaural");
    assert_eq!(report["renderer"]["model"], "studio-hrtf");
    assert_eq!(report["reference_drift"]["passed"], true);
}

#[test]
fn cli_fails_for_reference_duration_drift_and_preserves_json() {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("source.wav");
    let rendered = work.path().join("rendered.wav");
    let reference = work.path().join("reference.wav");
    let spec = work.path().join("binaural.json");
    write_fixture(&source, Layout::FiveOne, 48_000, 0.01);
    write_fixture(&rendered, Layout::Stereo, 48_000, 0.01);
    write_fixture(&reference, Layout::Stereo, 48_480, 0.01);
    fs::write(
        &spec,
        format!(
            r#"{{
              "schema_version": 1,
              "source": "source.wav",
              "rendered": "rendered.wav",
              "reference": "reference.wav",
              "input_layout": "5.1",
              "max_duration_delta_seconds": 0.001,
              "renderer": {}
            }}"#,
            evidence()
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-binaural-qc"))
        .arg(&spec)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:#?}");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["passed"], false);
    assert_eq!(report["reference_drift"]["duration_passed"], false);
}
