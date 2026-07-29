use forge_normalizer::wav::{AudioBuffer, ChannelRole, PcmKind, WavWriter};
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;

fn channel(frames: usize, frequency: f64) -> Vec<f32> {
    (0..frames)
        .map(|frame| {
            let time = frame as f64 / 48_000.0;
            (0.2 * (std::f64::consts::TAU * frequency * time).sin() + (frame % 997) as f64 * 1e-6)
                as f32
        })
        .collect()
}

fn write(path: &Path, data: Vec<Vec<f32>>) {
    let audio = AudioBuffer {
        sample_rate: 48_000,
        channels: data.len() as u16,
        frames: data[0].len(),
        channel_roles: vec![ChannelRole::Main; data.len()],
        source_kind: PcmKind::F32,
        data,
    };
    WavWriter::write(path, &audio, PcmKind::F32, false).unwrap();
}

#[test]
fn identical_audio_passes_and_emits_auditable_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let reference = directory.path().join("reference.wav");
    let candidate = directory.path().join("candidate.wav");
    let output_path = directory.path().join("comparison.json");
    let left = channel(16_000, 997.0);
    let right = channel(16_000, 431.0);
    write(&reference, vec![left.clone(), right.clone()]);
    write(&candidate, vec![left, right]);

    let output = Command::new(env!("CARGO_BIN_EXE_forge-audio-compare"))
        .arg(&reference)
        .arg(&candidate)
        .args(["--output"])
        .arg(&output_path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("PASS"));
    let report: Value = serde_json::from_slice(&fs::read(output_path).unwrap()).unwrap();
    assert_eq!(report["passed"], true);
    assert_eq!(report["alignment"]["offset_samples"], 0);
    assert_eq!(report["aggregate"]["mapped_null_depth_db"], 300.0);
    assert_eq!(report["channels"][0]["exact_sample_match_ratio"], 1.0);
    assert_eq!(
        report["method"]["classification"],
        "non-normative deterministic engineering QC; not PEAQ conformance"
    );
}

#[test]
fn delayed_audio_reports_positive_candidate_offset() {
    let directory = tempfile::tempdir().unwrap();
    let reference = directory.path().join("reference.wav");
    let candidate = directory.path().join("candidate.wav");
    let source = channel(24_000, 997.0);
    let mut delayed = vec![0.0; 137];
    delayed.extend_from_slice(&source[..source.len() - 137]);
    write(&reference, vec![source]);
    write(&candidate, vec![delayed]);

    let output = Command::new(env!("CARGO_BIN_EXE_forge-audio-compare"))
        .arg(&reference)
        .arg(&candidate)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["alignment"]["offset_samples"], 137);
    assert!(report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|finding| finding["rule_id"] == "FORGE-AUDIO-COMPARE-OFFSET"));
}

#[test]
fn explicitly_allowed_swap_and_inversion_are_notes() {
    let directory = tempfile::tempdir().unwrap();
    let reference = directory.path().join("reference.wav");
    let candidate = directory.path().join("candidate.wav");
    let left = channel(16_000, 997.0);
    let right = channel(16_000, 431.0);
    write(&reference, vec![left.clone(), right.clone()]);
    write(
        &candidate,
        vec![right.iter().map(|sample| -*sample).collect(), left],
    );

    let output = Command::new(env!("CARGO_BIN_EXE_forge-audio-compare"))
        .arg(&reference)
        .arg(&candidate)
        .args(["--allow-channel-permutation", "--allow-polarity-inversion"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["passed"], true);
    assert_eq!(report["error_count"], 0);
    assert_eq!(report["channels"][0]["candidate_channel"], 2);
    assert_eq!(report["channels"][1]["candidate_channel"], 1);
    assert_eq!(report["channels"][1]["polarity_inverted"], true);
}

#[test]
fn decoded_sample_limit_fails_before_comparison() {
    let directory = tempfile::tempdir().unwrap();
    let reference = directory.path().join("reference.wav");
    let candidate = directory.path().join("candidate.wav");
    let source = channel(1_000, 997.0);
    write(&reference, vec![source.clone()]);
    write(&candidate, vec![source]);

    let output = Command::new(env!("CARGO_BIN_EXE_forge-audio-compare"))
        .arg(&reference)
        .arg(&candidate)
        .args(["--max-decoded-samples", "999"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("exceeds safety limit"));
}
