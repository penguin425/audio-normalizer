use forge_normalizer::wav::{default_channel_roles, AudioBuffer, PcmKind, WavWriter};
use std::process::Command;

#[test]
fn presentation_qc_cli_writes_auditable_multi_presentation_report() {
    let work = tempfile::tempdir().unwrap();
    let render = work.path().join("render.wav");
    let report = work.path().join("report.json");
    let spec = work.path().join("presentations.json");
    let audio = AudioBuffer {
        sample_rate: 48_000,
        channels: 2,
        frames: 48_000,
        data: vec![vec![0.01; 48_000], vec![0.01; 48_000]],
        channel_roles: default_channel_roles(2),
        source_kind: PcmKind::F32,
    };
    WavWriter::write(&render, &audio, PcmKind::F32, false).unwrap();
    std::fs::write(
        &spec,
        r#"{
          "schema_version": 1,
          "codec": "ac4",
          "renderer": {"name": "vendor-reference", "version": "2.0"},
          "presentations": [
            {"id": "main", "rendered_path": "render.wav", "reference_path": "render.wav"},
            {"id": "dialogue-enhanced", "rendered_path": "render.wav", "accessibility": "dialogue-enhanced"}
          ]
        }"#,
    )
    .unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_forge-presentation-qc"))
        .args(["--output", report.to_str().unwrap(), spec.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success());
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(report).unwrap()).unwrap();
    assert_eq!(value["codec_standard"], "ETSI TS 103 190");
    assert_eq!(value["presentation_count"], 2);
    assert_eq!(value["passed"], true);
}
