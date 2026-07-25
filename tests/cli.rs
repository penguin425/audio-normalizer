use forge_normalizer::wav::{default_channel_roles, AudioBuffer, PcmKind, WavWriter};
use std::path::PathBuf;
use std::process::Command;

fn temp_root() -> PathBuf {
    std::env::temp_dir().join(format!("forge_cli_{}", std::process::id()))
}

#[test]
fn recursive_dry_run_preserves_relative_directories() {
    let root = temp_root();
    let input = root.join("input/disc1");
    let output = root.join("output");
    std::fs::create_dir_all(&input).unwrap();
    let wav = input.join("track.wav");
    let samples = vec![0.0; 48_000];
    let buffer = AudioBuffer {
        sample_rate: 48_000,
        channels: 1,
        frames: samples.len(),
        data: vec![samples],
        channel_roles: default_channel_roles(1),
        source_kind: PcmKind::F32,
    };
    WavWriter::write(&wav, &buffer, PcmKind::F32, false).unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args([
            root.join("input").to_str().unwrap(),
            "--recursive",
            "--dry-run",
            "-o",
            output.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(result.status.success());
    let stderr = String::from_utf8(result.stderr).unwrap();
    assert!(stderr.contains("output/disc1/track_normalized.wav"));
    assert!(!output.exists(), "dry-run created the output directory");

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn preset_is_accepted_but_explicit_target_conflicts() {
    let cli = Cli::try_parse_from(["forge", "track.wav", "--preset", "ebu-r128"]).unwrap();
    assert_eq!(cli.preset.as_deref(), Some("ebu-r128"));
    assert!(
        Cli::try_parse_from(["forge", "track.wav", "--preset", "spotify", "--target=-12"]).is_err()
    );
}

#[test]
fn verification_retries_require_verification_and_are_bounded() {
    assert!(Cli::try_parse_from(["forge", "track.wav", "--verify-retries", "2"]).is_err());
    let cli =
        Cli::try_parse_from(["forge", "track.wav", "--verify", "--verify-retries", "3"]).unwrap();
    assert_eq!(cli.verify_retries, 3);
    assert!(
        Cli::try_parse_from(["forge", "track.wav", "--verify", "--verify-retries", "11"]).is_err()
    );
}

#[test]
fn channel_layout_is_validated_and_exposed() {
    let cli = Cli::try_parse_from(["forge", "track.wav", "--channel-layout", "7.1.4"]).unwrap();
    assert_eq!(cli.channel_layout.as_deref(), Some("7.1.4"));
    assert!(Cli::try_parse_from(["forge", "track.wav", "--channel-layout", "unknown"]).is_err());
    assert!(Cli::try_parse_from([
        "forge",
        "track.wav",
        "--channel-layout",
        "mono",
        "--dual-mono"
    ])
    .is_err());
}
use clap::Parser;
use forge_normalizer::cli::Cli;
