use forge_normalizer::metadata;
use forge_normalizer::wav::{AudioBuffer, PcmKind, WavContainer, WavWriter, WaveChunk};
use serde_json::Value;
use std::fs;
use std::process::Command;

fn write_fixture(path: &std::path::Path) {
    let audio = AudioBuffer {
        sample_rate: 48_000,
        channels: 1,
        frames: 64,
        data: vec![vec![0.1; 64]],
        channel_roles: forge_normalizer::wav::default_channel_roles(1),
        source_kind: PcmKind::F32,
    };
    WavWriter::write_with_metadata(
        path,
        &audio,
        PcmKind::F32,
        false,
        WavContainer::Riff,
        &[
            WaveChunk {
                id: *b"JUNK",
                body: vec![1, 2, 3, 4, 5],
            },
            WaveChunk {
                id: *b"bext",
                body: metadata::blank_bext(),
            },
        ],
    )
    .unwrap();
}

#[test]
fn cli_repairs_bwf_into_atomic_copy_and_validates_report() {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("source.wav");
    let destination = work.path().join("repaired.wav");
    let request = work.path().join("request.json");
    let report_path = work.path().join("report.json");
    write_fixture(&source);
    let before = fs::read(&source).unwrap();
    fs::write(
        &request,
        r#"{
          "schema_version": 1,
          "source": "source.wav",
          "destination": "repaired.wav",
          "ensure_bwf_v2": true,
          "atomic_replace": true,
          "bwf_loudness": {
            "integrated_lufs": -23.0,
            "loudness_range_lu": 8.0,
            "true_peak_dbtp": -1.0,
            "max_momentary_lufs": -10.0,
            "max_short_term_lufs": -14.0
          }
        }"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_forge-metadata-repair"))
        .args([
            "--output",
            report_path.to_str().unwrap(),
            request.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    assert_eq!(before, fs::read(&source).unwrap());
    assert_ne!(before, fs::read(&destination).unwrap());
    assert_eq!(
        metadata::read_wave_chunk(&destination, *b"JUNK").unwrap(),
        Some(vec![1, 2, 3, 4, 5])
    );
    let report: Value = serde_json::from_slice(&fs::read(report_path).unwrap()).unwrap();
    let schema: Value = serde_json::from_str(include_str!(
        "../schema/metadata-repair-report-v1.schema.json"
    ))
    .unwrap();
    assert!(jsonschema::validator_for(&schema)
        .unwrap()
        .is_valid(&report));
    assert_eq!(report["validator"], "forge-metadata-repair-1");
    assert_eq!(report["source_format"], "wave");
    assert_eq!(report["changed"], true);
    assert_eq!(report["unknown_bytes_preserved"], true);
}

#[test]
fn validate_mode_copies_without_mutating_and_rejects_same_path() {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("source.wav");
    let destination = work.path().join("copy.wav");
    let request = work.path().join("request.json");
    write_fixture(&source);
    fs::write(
        &request,
        r#"{
          "schema_version": 1,
          "source": "source.wav",
          "destination": "copy.wav",
          "mode": "validate"
        }"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-metadata-repair"))
        .arg(&request)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    assert_eq!(fs::read(&source).unwrap(), fs::read(&destination).unwrap());
}
