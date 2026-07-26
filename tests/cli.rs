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

#[test]
fn m4a_output_format_is_exposed() {
    let cli = Cli::try_parse_from(["forge", "track.wav", "--format", "m4a"]).unwrap();
    assert_eq!(cli.format.as_deref(), Some("m4a"));
}

#[test]
fn adm_reference_renderer_options_are_validated_and_exposed() {
    let cli = Cli::try_parse_from([
        "forge",
        "programme.wav",
        "--analyze",
        "--adm-render",
        "--adm-renderer",
        "/opt/eat-process",
        "--adm-layout",
        "0+5+0",
        "--adm-profile-level",
        "2",
        "--adm-rendered-output",
        "rendered.wav",
    ])
    .unwrap();
    assert!(cli.adm_render);
    assert_eq!(
        cli.adm_renderer.as_deref(),
        Some(std::path::Path::new("/opt/eat-process"))
    );
    assert_eq!(cli.adm_layout, "0+5+0");
    assert_eq!(cli.adm_profile_level, 2);
    assert_eq!(
        cli.adm_rendered_output.as_deref(),
        Some(std::path::Path::new("rendered.wav"))
    );
    assert!(Cli::try_parse_from([
        "forge",
        "programme.wav",
        "--analyze",
        "--adm-render",
        "--adm-profile-level",
        "3"
    ])
    .is_err());
    assert!(Cli::try_parse_from([
        "forge",
        "programme.wav",
        "--analyze",
        "--adm-render",
        "--adm-presentations",
        "map.json"
    ])
    .is_err());
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

#[test]
fn analysis_range_writes_a_time_resolved_qc_report() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("programme.wav");
    let timeline = directory.path().join("timeline.ndjson");
    std::fs::write(&input, wav_fixture_bytes()).unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args([
            input.to_str().unwrap(),
            "--analyze",
            "--start",
            "0.2",
            "--duration",
            "0.5",
            "--timeline",
            timeline.to_str().unwrap(),
            "--timeline-interval",
            "100",
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let lines = std::fs::read_to_string(timeline).unwrap();
    let points = lines
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(points.len(), 5);
    assert_eq!(points[0]["start_seconds"], 0.2);
    assert_eq!(points[4]["end_seconds"], 0.7);
    assert!(points[0]["momentary_lufs"].is_null());
    assert!(points[3]["momentary_lufs"].is_number());
}

#[test]
fn codec_metadata_and_downmix_are_reported() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("surround.wav");
    let metadata = directory.path().join("delivery.json");
    let frames = 48_000;
    let channel = (0..frames)
        .map(|frame| 0.02 * (std::f32::consts::TAU * 997.0 * frame as f32 / 48_000.0).sin())
        .collect::<Vec<_>>();
    let buffer = AudioBuffer {
        sample_rate: 48_000,
        channels: 6,
        frames,
        data: vec![channel; 6],
        channel_roles: default_channel_roles(6),
        source_kind: PcmKind::F32,
    };
    WavWriter::write(&input, &buffer, PcmKind::F32, false).unwrap();
    std::fs::write(
        &metadata,
        r#"{"codec":"eac3","encoded_loudness_lufs":-24.0,"downmix_mode":"loro","tolerance_lu":100.0}"#,
    )
    .unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args([
            input.to_str().unwrap(),
            "--analyze",
            "--json",
            "--codec-metadata",
            metadata.to_str().unwrap(),
            "--downmix-qc",
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report[0]["codec"], "eac3");
    assert_eq!(report[0]["codec_downmix_mode"], "loro");
    assert_eq!(report[0]["codec_encoded_loudness_pass"], true);
    assert!(report[0]["downmix_integrated_lufs"].is_number());
    assert!(report[0]["downmix_method"]
        .as_str()
        .unwrap()
        .contains("LFE omitted"));
}

#[cfg(unix)]
#[test]
fn automatic_codec_probe_and_reference_roundtrip_are_reported() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("delivery.wav");
    let prober = directory.path().join("fake-ffprobe");
    std::fs::write(&input, wav_fixture_bytes()).unwrap();
    std::fs::write(
        &prober,
        r#"#!/bin/sh
printf '%s\n' '{"streams":[{"codec_name":"eac3","profile":"E-AC-3","sample_rate":"48000","channels":1,"channel_layout":"mono","bit_rate":"192000","side_data_list":[{"dialnorm":24,"downmix_mode":"loro","drc_profile":"film_standard"}]}],"format":{"format_name":"eac3"}}'
"#,
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&prober).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&prober, permissions).unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args([
            input.to_str().unwrap(),
            "--analyze",
            "--json",
            "--codec-qc",
            "--codec-prober",
            prober.to_str().unwrap(),
            "--codec-reference",
            input.to_str().unwrap(),
            "--codec-qc-tolerance",
            "100",
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report[0]["codec"], "eac3");
    assert_eq!(report[0]["codec_profile"], "E-AC-3");
    assert_eq!(report[0]["codec_container"], "eac3");
    assert_eq!(report[0]["codec_bitrate_bps"], 192_000);
    assert_eq!(report[0]["codec_downmix_mode"], "loro");
    assert_eq!(report[0]["codec_drc_profile"], "film_standard");
    assert_eq!(report[0]["codec_probe_schema"], "ffprobe-json-v1");
    assert_eq!(report[0]["codec_loudness_drift_lu"], 0.0);
    assert_eq!(report[0]["codec_true_peak_drift_db"], 0.0);
    assert_eq!(report[0]["codec_duration_drift_seconds"], 0.0);
    assert_eq!(report[0]["codec_roundtrip_pass"], true);
}

#[test]
fn batch_analysis_writes_a_delivery_manifest() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("one.wav");
    let second = directory.path().join("two.wav");
    let manifest = directory.path().join("delivery.json");
    let bytes = wav_fixture_bytes();
    std::fs::write(&first, &bytes).unwrap();
    std::fs::write(&second, &bytes).unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args([
            first.to_str().unwrap(),
            second.to_str().unwrap(),
            "--analyze",
            "--manifest",
            manifest.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(result.status.success());
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
    assert_eq!(value["asset_count"], 2);
    assert_eq!(value["passed_count"], 2);
    assert_eq!(value["assets"][0]["path"], first.to_str().unwrap());
}

#[test]
fn ebu_qc_writes_versioned_baseband_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("programme.wav");
    let manifest = directory.path().join("delivery.json");
    std::fs::write(&input, wav_fixture_bytes()).unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args([
            input.to_str().unwrap(),
            "--analyze",
            "--ebu-qc",
            "--silence-threshold=-200",
            "--tone-threshold=1",
            "--manifest",
            manifest.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
    assert!(value["schema"]
        .as_str()
        .unwrap()
        .ends_with("delivery-manifest-v2"));
    let results = value["assets"][0]["qc"]["results"].as_array().unwrap();
    assert_eq!(results.len(), 6);
    assert_eq!(results[0]["ebu_qc_id"], "0078B");
    assert_eq!(results[4]["ebu_qc_id"], "0010B");
    assert_eq!(results[5]["ebu_qc_id"], "0084B");
}

#[test]
fn automatic_dialogue_writes_detection_audit() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("speech.wav");
    let audit = directory.path().join("dialogue-detection.json");
    std::fs::write(&input, wav_fixture_bytes()).unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args([
            input.to_str().unwrap(),
            "--analyze",
            "--json",
            "--auto-dialogue",
            "--dialogue-detection-report",
            audit.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(
        report[0]["dialogue_detector"],
        "forge-dialogue-deterministic"
    );
    assert_eq!(report[0]["dialogue_detection_threshold"], 0.6);
    assert!(report[0]["dialogue_detection_ranges_json"]
        .as_str()
        .unwrap()
        .contains("confidence"));
    assert!(report[0]["dialogue_detection_frames_json"]
        .as_str()
        .unwrap()
        .contains("speech_band_energy_ratio"));
    let audit: serde_json::Value = serde_json::from_slice(&std::fs::read(audit).unwrap()).unwrap();
    assert_eq!(audit["features"].as_array().unwrap().len(), 8);
    assert!(audit["ranges"][0]["confidence"].is_number());
    assert!(audit["frames"][0]["adaptive_noise_floor_dbfs"].is_number());
    assert!(audit["frames"][0]["selected"].is_boolean());
}

#[test]
fn explicit_dialogue_ranges_drive_long_form_compliance() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("programme.wav");
    let ranges = directory.path().join("dialogue.json");
    std::fs::write(&input, wav_fixture_bytes()).unwrap();
    std::fs::write(
        &ranges,
        r#"{"ranges":[{"start_seconds":0.0,"duration_seconds":1.0}]}"#,
    )
    .unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args([
            input.to_str().unwrap(),
            "--analyze",
            "--json",
            "--dialogue-ranges",
            ranges.to_str().unwrap(),
            "--compliance",
            "atsc-a85-long",
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert!(report[0]["dialogue_lufs"].is_number());
    assert_eq!(report[0]["dialogue_range_count"], 1);
    assert_eq!(
        report[0]["dialogue_measurement_standard"],
        "ATSC A/85:2026-07"
    );
    assert!(report[0]["dialogue_measurement_method"]
        .as_str()
        .unwrap()
        .contains("no relative-level gate"));
    assert_eq!(report[0]["compliance_loudness_basis"], "dialogue");
    assert!(report[0]["compliance_passed"].as_bool().unwrap());
    assert!(report[0]["compliance_rules_json"]
        .as_str()
        .unwrap()
        .contains("dialogue_lufs"));

    let ebu = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args([
            input.to_str().unwrap(),
            "--analyze",
            "--json",
            "--dialogue-ranges",
            ranges.to_str().unwrap(),
            "--dialogue-standard",
            "ebu-r128-s4",
            "--dialogue-source",
            "mix",
        ])
        .output()
        .unwrap();
    assert!(ebu.status.success());
    let ebu_report: serde_json::Value = serde_json::from_slice(&ebu.stdout).unwrap();
    assert_eq!(
        ebu_report[0]["dialogue_measurement_standard"],
        "EBU R 128 s4"
    );
    assert_eq!(ebu_report[0]["dialogue_source"], "mix");
    assert!(ebu_report[0]["loudness_to_dialogue_ratio_lu"].is_number());

    let missing = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args([
            input.to_str().unwrap(),
            "--analyze",
            "--compliance",
            "atsc-a85-long",
        ])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("requires --dialogue-ranges"));
}

#[test]
fn toml_job_config_is_relative_and_cli_options_override_it() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("programme.wav");
    let config = directory.path().join("forge.toml");
    std::fs::write(&input, wav_fixture_bytes()).unwrap();
    std::fs::write(
        &config,
        r#"
            [analysis]
            enabled = true
            start_seconds = 0.1
            duration_seconds = 0.6
            timeline = "configured.ndjson"
            timeline_interval_ms = 100
        "#,
    )
    .unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args([
            input.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
            "--analyze",
            "--duration",
            "0.3",
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let timeline = std::fs::read_to_string(directory.path().join("configured.ndjson")).unwrap();
    assert_eq!(timeline.lines().count(), 3);
}
use clap::Parser;
use forge_normalizer::cli::Cli;

#[test]
fn automatic_codec_qc_options_are_scoped_and_exclusive() {
    assert!(Cli::try_parse_from(["forge", "track.eac3", "--codec-qc"]).is_err());
    let cli = Cli::try_parse_from([
        "forge",
        "track.eac3",
        "--analyze",
        "--codec-qc",
        "--codec-prober",
        "custom-probe",
    ])
    .unwrap();
    assert!(cli.codec_qc);
    assert_eq!(
        cli.codec_prober.as_deref(),
        Some(std::path::Path::new("custom-probe"))
    );
    assert!(Cli::try_parse_from([
        "forge",
        "track.eac3",
        "--analyze",
        "--codec-qc",
        "--codec-metadata",
        "delivery.json"
    ])
    .is_err());
}
