use forge_normalizer::wav::{default_channel_roles, AudioBuffer, PcmKind, WavWriter};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

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
    assert_eq!(
        Cli::try_parse_from(["forge", "track.wav", "--channel-layout", "6.1"])
            .unwrap()
            .channel_layout
            .as_deref(),
        Some("6.1")
    );
    assert!(Cli::try_parse_from([
        "forge",
        "track.wav",
        "--channel-layout",
        "mono",
        "--dual-mono"
    ])
    .is_err());
}

#[test]
fn broadcast_wave_options_are_validated_and_exposed() {
    let cli =
        Cli::try_parse_from(["forge", "track.wav", "--bwf", "--wav-container", "bw64"]).unwrap();
    assert!(cli.bwf);
    assert_eq!(cli.wav_container, "bw64");
    assert!(Cli::try_parse_from(["forge", "track.wav", "--wav-container", "wave64"]).is_err());
}

fn wav_fixture_bytes() -> Vec<u8> {
    let file = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
    let buffer = AudioBuffer {
        sample_rate: 48_000,
        channels: 1,
        frames: 48_000,
        data: vec![(0..48_000)
            .map(|frame| 0.1 * (std::f32::consts::TAU * 440.0 * frame as f32 / 48_000.0).sin())
            .collect()],
        channel_roles: default_channel_roles(1),
        source_kind: PcmKind::F32,
    };
    WavWriter::write(file.path(), &buffer, PcmKind::S16, false).unwrap();
    std::fs::read(file.path()).unwrap()
}

fn run_with_stdin(arguments: &[&str], input: &[u8]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn standard_streams_support_binary_audio_and_ndjson() {
    let input = wav_fixture_bytes();
    let normalized = run_with_stdin(
        &["-", "--input-format", "wav", "-o", "-", "--format", "wav"],
        &input,
    );
    assert!(
        normalized.status.success(),
        "{}",
        String::from_utf8_lossy(&normalized.stderr)
    );
    assert!(normalized.stdout.starts_with(b"RIFF"));
    let decoded = forge_normalizer::wav::WavReader::read_bytes(&normalized.stdout).unwrap();
    assert_eq!((decoded.sample_rate, decoded.channels), (48_000, 1));

    let report = run_with_stdin(
        &["-", "--input-format", "wav", "--analyze", "--ndjson"],
        &input,
    );
    assert!(
        report.status.success(),
        "{}",
        String::from_utf8_lossy(&report.stderr)
    );
    let lines: Vec<_> = report
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .collect();
    assert_eq!(lines.len(), 1);
    let value: serde_json::Value = serde_json::from_slice(lines[0]).unwrap();
    assert_eq!(value["channels"], 1);
    assert_eq!(value["path"], "-");
}
use clap::Parser;
use forge_normalizer::cli::Cli;
