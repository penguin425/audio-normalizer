use forge_normalizer::wav::{AudioBuffer, ChannelRole, PcmKind, WavWriter};
use serde_json::Value;
use std::fs;
use std::process::Command;

#[test]
fn container_qc_cli_returns_pass_and_fail_status() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("programme.wav");
    let audio = AudioBuffer {
        sample_rate: 48_000,
        channels: 1,
        frames: 100,
        data: vec![vec![0.0; 100]],
        channel_roles: vec![ChannelRole::Main],
        source_kind: PcmKind::S16,
    };
    WavWriter::write(&path, &audio, PcmKind::S16, false).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-container-qc"))
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("PASS"));
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(audit["passed"], true);

    let mut bytes = fs::read(&path).unwrap();
    bytes[4..8].copy_from_slice(&1_u32.to_le_bytes());
    fs::write(&path, bytes).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-container-qc"))
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("FAIL"));
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(audit["passed"], false);
}

#[test]
fn container_qc_cli_reports_malformed_isobmff_as_qc_failure() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("programme.m4a");
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&20_u32.to_be_bytes());
    bytes.extend_from_slice(b"ftyp");
    bytes.extend_from_slice(b"M4A ");
    bytes.extend_from_slice(&0_u32.to_be_bytes());
    bytes.extend_from_slice(b"isom");
    bytes.extend_from_slice(&16_u32.to_be_bytes());
    bytes.extend_from_slice(b"moov");
    bytes.extend_from_slice(&32_u32.to_be_bytes());
    bytes.extend_from_slice(b"trak");
    fs::write(&path, bytes).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_forge-container-qc"))
        .arg(&path)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("FAIL"));
    let audit: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(audit["format"], "isobmff");
    assert_eq!(audit["passed"], false);
    assert!(audit["layers"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|layer| layer["checks"].as_array().unwrap())
        .any(|check| check["rule_id"] == "FORGE-ISOBMFF-MOVIE-STRUCTURE"));
}
