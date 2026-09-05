use forge_normalizer::ebu_qc_validation::{validate_xml, EbuQcValidationProfile};
use forge_normalizer::wav::{
    default_channel_roles, AudioBuffer, PcmKind, WavContainer, WavWriter, WaveChunk,
};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn temp_root() -> PathBuf {
    std::env::temp_dir().join(format!("forge_cli_{}", std::process::id()))
}

fn write_batch_test_wav(path: &std::path::Path, frequency_hz: f64) {
    let sample_rate = 48_000;
    let frames = 24_000;
    let samples = (0..frames)
        .map(|frame| {
            (0.2 * (std::f64::consts::TAU * frequency_hz * frame as f64 / sample_rate as f64).sin())
                as f32
        })
        .collect::<Vec<_>>();
    let buffer = AudioBuffer {
        sample_rate,
        channels: 1,
        frames,
        data: vec![samples],
        channel_roles: default_channel_roles(1),
        source_kind: PcmKind::F32,
    };
    WavWriter::write(path, &buffer, PcmKind::F32, false).unwrap();
}

#[test]
fn sqlite_catalogue_records_normalization_hashes_and_exports_provenance() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("input.wav");
    let output = directory.path().join("output.wav");
    let database = directory.path().join("catalogue.sqlite");
    let report = directory.path().join("catalogue-report.json");
    write_batch_test_wav(&input, 440.0);

    let result = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--catalogue")
        .arg(&database)
        .arg("--catalogue-report")
        .arg(&report)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );

    let connection = rusqlite::Connection::open(&database).unwrap();
    let row: (String, String, String, String, String, String, String) = connection
        .query_row(
            "SELECT operation, source_sha256, output_sha256,
                    measurement_standard, measurement_version,
                    request_sha256, request_json
             FROM catalogue_entries",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(row.0, "normalization");
    assert_eq!(row.1.len(), 64);
    assert_eq!(row.2.len(), 64);
    assert_eq!(row.3, "ITU-R BS.1770-5 / EBU R 128");
    assert_eq!(row.4, "forge-bs1770-5-r4");
    assert_eq!(row.5.len(), 64);
    let request: serde_json::Value = serde_json::from_str(&row.6).unwrap();
    assert_eq!(request["renderer"], "forge-native:wav");
    assert_eq!(request["input_descriptor"]["audio_track_index"], 0);

    let report_value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(report).unwrap()).unwrap();
    let schema: serde_json::Value =
        serde_json::from_str(include_str!("../schema/catalogue-report-v3.schema.json")).unwrap();
    assert!(jsonschema::validator_for(&schema)
        .unwrap()
        .is_valid(&report_value));
    assert_eq!(report_value["records"][0]["source"]["sha256"], row.1);
    assert_eq!(report_value["records"][0]["output"]["sha256"], row.2);
}

#[test]
fn sqlite_catalogue_analysis_is_content_addressed_and_deduplicated() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("input.wav");
    let database = directory.path().join("catalogue.sqlite");
    write_batch_test_wav(&input, 440.0);

    let run = || {
        Command::new(env!("CARGO_BIN_EXE_forge"))
            .arg(&input)
            .arg("--analyze")
            .arg("--catalogue")
            .arg(&database)
            .output()
            .unwrap()
    };
    let first = run();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let second = run();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let ranged = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg(&input)
        .arg("--analyze")
        .arg("--duration")
        .arg("0.25")
        .arg("--catalogue")
        .arg(&database)
        .output()
        .unwrap();
    assert!(
        ranged.status.success(),
        "{}",
        String::from_utf8_lossy(&ranged.stderr)
    );
    let connection = rusqlite::Connection::open(database).unwrap();
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM catalogue_entries", [], |row| {
            row.get(0)
        })
        .unwrap();
    let output_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM catalogue_entries WHERE output_sha256 IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 2);
    assert_eq!(output_count, 0);
}

#[test]
fn sqlite_catalogue_records_every_successful_album_output() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first.wav");
    let second = directory.path().join("second.wav");
    let outputs = directory.path().join("normalized");
    let database = directory.path().join("catalogue.sqlite");
    write_batch_test_wav(&first, 440.0);
    write_batch_test_wav(&second, 880.0);
    std::fs::create_dir(&outputs).unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg(&first)
        .arg(&second)
        .arg("--album")
        .arg("-o")
        .arg(&outputs)
        .arg("--catalogue")
        .arg(&database)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let connection = rusqlite::Connection::open(database).unwrap();
    let evidence: (i64, i64) = connection
        .query_row(
            "SELECT COUNT(*),
                    SUM(CASE WHEN json_extract(provenance_json, '$.album') = 1
                             THEN 1 ELSE 0 END)
             FROM catalogue_entries",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(evidence, (2, 2));
}

#[test]
fn watch_folder_waits_for_stability_then_checkpoints_and_skips_completed_output() {
    let directory = tempfile::tempdir().unwrap();
    let input_directory = directory.path().join("input");
    let output_directory = directory.path().join("output");
    let state = directory.path().join("watch.json");
    std::fs::create_dir(&input_directory).unwrap();
    let input = input_directory.join("tone.wav");
    write_batch_test_wav(&input, 440.0);
    write_batch_test_wav(&input_directory.join("second.wav"), 880.0);

    let run = || {
        Command::new(env!("CARGO_BIN_EXE_forge"))
            .arg(&input_directory)
            .arg("--watch")
            .arg("--watch-once")
            .arg("--watch-state")
            .arg(&state)
            .arg("--watch-stable-seconds")
            .arg("1")
            .arg("-j")
            .arg("1")
            .arg("-o")
            .arg(&output_directory)
            .output()
            .unwrap()
    };
    let first = run();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let output = output_directory.join("tone_normalized.wav");
    assert!(!output.exists());

    std::thread::sleep(std::time::Duration::from_millis(1100));
    let second = run();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(output.is_file());
    assert!(output_directory.join("second_normalized.wav").is_file());
    let output_bytes = std::fs::read(&output).unwrap();
    let state_value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
    let schema: serde_json::Value =
        serde_json::from_str(include_str!("../schema/watch-folder-v1.schema.json")).unwrap();
    assert!(jsonschema::validator_for(&schema)
        .unwrap()
        .is_valid(&state_value));
    assert!(state_value["entries"]
        .as_array()
        .unwrap()
        .iter()
        .all(|entry| entry["status"] == "completed"));

    let third = run();
    assert!(
        third.status.success(),
        "{}",
        String::from_utf8_lossy(&third.stderr)
    );
    assert_eq!(std::fs::read(&output).unwrap(), output_bytes);

    std::fs::write(&output, b"tampered").unwrap();
    let tampered = run();
    assert!(!tampered.status.success());
    assert!(String::from_utf8_lossy(&tampered.stderr).contains("changed since checkpoint"));
}

#[test]
fn watch_folder_validates_required_paths_and_incompatible_modes() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("input");
    let output = directory.path().join("output");
    let state = directory.path().join("watch.json");
    std::fs::create_dir(&input).unwrap();
    let help = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg("--help")
        .output()
        .unwrap();
    let help = String::from_utf8(help.stdout).unwrap();
    for option in [
        "--watch",
        "--watch-state <PATH>",
        "--watch-stable-seconds <SECONDS>",
        "--watch-poll-seconds <SECONDS>",
        "--watch-once",
        "--watch-retry-failed",
    ] {
        assert!(help.contains(option), "missing help option {option}");
    }

    let missing_state = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg(&input)
        .arg("--watch")
        .arg("--watch-once")
        .arg("-o")
        .arg(&output)
        .output()
        .unwrap();
    assert!(!missing_state.status.success());
    assert!(
        String::from_utf8_lossy(&missing_state.stderr).contains("--watch requires --watch-state")
    );

    let analyze = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg(&input)
        .arg("--watch")
        .arg("--watch-state")
        .arg(&state)
        .arg("-o")
        .arg(&output)
        .arg("--analyze")
        .output()
        .unwrap();
    assert!(!analyze.status.success());
    assert!(String::from_utf8_lossy(&analyze.stderr).contains("cannot be used with"));
}

#[test]
fn recursive_watch_preserves_relative_paths_and_ignores_nested_output() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("input");
    let nested = input.join("incoming/album");
    let output = input.join("normalized");
    let state = directory.path().join("watch.json");
    std::fs::create_dir_all(&nested).unwrap();
    write_batch_test_wav(&nested.join("tone.wav"), 440.0);

    let run = || {
        Command::new(env!("CARGO_BIN_EXE_forge"))
            .arg(&input)
            .arg("--watch")
            .arg("--watch-once")
            .arg("--watch-state")
            .arg(&state)
            .arg("--watch-stable-seconds")
            .arg("1")
            .arg("--recursive")
            .arg("-o")
            .arg(&output)
            .output()
            .unwrap()
    };
    assert!(run().status.success());
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let processed = run();
    assert!(
        processed.status.success(),
        "{}",
        String::from_utf8_lossy(&processed.stderr)
    );
    assert!(output.join("incoming/album/tone_normalized.wav").is_file());
    assert!(run().status.success());
    let state_value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
    assert_eq!(state_value["entries"].as_array().unwrap().len(), 1);
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

#[cfg(not(feature = "opus-encoding"))]
#[test]
fn dry_run_validates_an_optional_codec_request_without_requiring_its_encoder() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("tone.wav");
    let output = directory.path().join("would-be.opus");
    write_batch_test_wav(&input, 440.0);

    let result = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg(&input)
        .arg("--dry-run")
        .arg("--format")
        .arg("opus")
        .arg("-o")
        .arg(&output)
        .output()
        .unwrap();

    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!output.exists());
}

#[test]
fn content_addressed_analysis_cache_is_reused_by_real_normalization() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("tone.wav");
    let output = directory.path().join("normalized.wav");
    let cache = directory.path().join("analysis-cache");
    write_batch_test_wav(&input, 440.0);

    let dry_run = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg(&input)
        .arg("--dry-run")
        .arg("--analysis-cache")
        .arg(&cache)
        .arg("--warm-cache")
        .arg("--sample-rate")
        .arg("44100")
        .output()
        .unwrap();
    assert!(
        dry_run.status.success(),
        "{}",
        String::from_utf8_lossy(&dry_run.stderr)
    );
    assert!(String::from_utf8_lossy(&dry_run.stderr).contains("analysis cache miss; stored"));

    let normalize = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--analysis-cache")
        .arg(&cache)
        .arg("--sample-rate")
        .arg("44100")
        .output()
        .unwrap();
    assert!(
        normalize.status.success(),
        "{}",
        String::from_utf8_lossy(&normalize.stderr)
    );
    assert!(String::from_utf8_lossy(&normalize.stderr).contains("analysis cache hit"));
    assert!(output.is_file());

    let prefix = std::fs::read_dir(cache.join("v5"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let content = std::fs::read_dir(prefix)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let entry = std::fs::read_dir(content)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let instance: serde_json::Value =
        serde_json::from_slice(&std::fs::read(entry).unwrap()).unwrap();
    let schema: serde_json::Value =
        serde_json::from_str(include_str!("../schema/analysis-cache-v5.schema.json")).unwrap();
    assert!(jsonschema::validator_for(&schema)
        .unwrap()
        .is_valid(&instance));
}

#[test]
fn dry_run_does_not_populate_analysis_cache_without_warm_cache() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("tone.wav");
    let cache = directory.path().join("analysis-cache");
    write_batch_test_wav(&input, 440.0);

    let dry_run = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg(&input)
        .arg("--dry-run")
        .arg("--analysis-cache")
        .arg(&cache)
        .output()
        .unwrap();
    assert!(
        dry_run.status.success(),
        "{}",
        String::from_utf8_lossy(&dry_run.stderr)
    );
    assert!(String::from_utf8_lossy(&dry_run.stderr).contains("miss; read-only"));
    assert!(!cache.join("v5").exists());

    let tag_dry_run = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg(&input)
        .arg("--write-tags")
        .arg("--dry-run")
        .arg("--analysis-cache")
        .arg(&cache)
        .output()
        .unwrap();
    assert!(
        tag_dry_run.status.success(),
        "{}",
        String::from_utf8_lossy(&tag_dry_run.stderr)
    );
    assert!(String::from_utf8_lossy(&tag_dry_run.stderr).contains("miss; read-only"));
    assert!(!cache.join("v5").exists());
}

#[test]
fn reference_analysis_reports_engine_identity_and_uses_an_isolated_cache_entry() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("tone.wav");
    let cache = directory.path().join("cache");
    write_batch_test_wav(&input, 997.0);

    let reference = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg(&input)
        .args(["--analyze", "--analysis-engine", "reference", "--json"])
        .arg("--analysis-cache")
        .arg(&cache)
        .output()
        .unwrap();
    assert!(
        reference.status.success(),
        "{}",
        String::from_utf8_lossy(&reference.stderr)
    );
    let reports: serde_json::Value = serde_json::from_slice(&reference.stdout).unwrap();
    assert_eq!(
        reports[0]["analysis_engine_id"],
        "forge-reference-bs1770-r1"
    );
    assert_eq!(
        reports[0]["measurement_algorithm_revision"],
        "forge-bs1770-5-r4"
    );
    assert!(reports[0]["integrated_lufs"].is_number());

    let fast = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg(&input)
        .args(["--analyze", "--analysis-engine", "fast", "--json"])
        .arg("--analysis-cache")
        .arg(&cache)
        .output()
        .unwrap();
    assert!(fast.status.success());
    let mut entry_count = 0;
    for prefix in std::fs::read_dir(cache.join("v5")).unwrap() {
        for content in std::fs::read_dir(prefix.unwrap().path()).unwrap() {
            entry_count += std::fs::read_dir(content.unwrap().path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
                .count();
        }
    }
    assert_eq!(entry_count, 2);

    let rejected = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg(&input)
        .args(["--analysis-engine", "reference"])
        .output()
        .unwrap();
    assert_eq!(rejected.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("requires --analyze"));
}

#[test]
fn analysis_engine_config_is_applied_and_an_explicit_cli_value_wins() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("tone.wav");
    let config = directory.path().join("forge.toml");
    write_batch_test_wav(&input, 997.0);
    std::fs::write(
        &config,
        r#"
            [analysis]
            enabled = true
            engine = "reference"
        "#,
    )
    .unwrap();

    let configured = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg(&input)
        .arg("--config")
        .arg(&config)
        .output()
        .unwrap();
    assert!(
        configured.status.success(),
        "{}",
        String::from_utf8_lossy(&configured.stderr)
    );
    assert!(String::from_utf8_lossy(&configured.stderr)
        .contains("analysis engine: forge-reference-bs1770-r1"));

    let overridden = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg(&input)
        .arg("--config")
        .arg(&config)
        .args(["--analysis-engine", "fast", "--analyze", "--json"])
        .output()
        .unwrap();
    assert!(
        overridden.status.success(),
        "{}",
        String::from_utf8_lossy(&overridden.stderr)
    );
    let reports: serde_json::Value = serde_json::from_slice(&overridden.stdout).unwrap();
    assert_eq!(reports[0]["analysis_engine_id"], "forge-fast-bs1770-r4");
}

#[test]
fn read_only_analysis_cache_miss_does_not_create_storage() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("tone.wav");
    let cache = directory.path().join("absent-cache");
    write_batch_test_wav(&input, 440.0);

    let result = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg(&input)
        .arg("--dry-run")
        .arg("--analysis-cache")
        .arg(&cache)
        .arg("--analysis-cache-read-only")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stderr).contains("analysis cache miss; read-only"));
    assert!(!cache.exists());
}

#[test]
fn analysis_cache_options_are_validated_and_exposed() {
    let help = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("--analysis-cache <DIR>"));
    assert!(help.contains("--analysis-cache-read-only"));
    assert!(help.contains("--analysis-cache-max-mib <MIB>"));

    let missing_directory = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg("--analysis-cache-read-only")
        .output()
        .unwrap();
    assert_eq!(missing_directory.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&missing_directory.stderr).contains("--analysis-cache <DIR>"));

    let zero_limit = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg("--analysis-cache")
        .arg("cache")
        .arg("--analysis-cache-max-mib")
        .arg("0")
        .output()
        .unwrap();
    assert_eq!(zero_limit.status.code(), Some(2));

    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("tone.wav");
    let not_a_directory = directory.path().join("cache-file");
    write_batch_test_wav(&input, 440.0);
    std::fs::write(&not_a_directory, b"not a directory").unwrap();
    let invalid_root = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg(&input)
        .arg("--dry-run")
        .arg("--analysis-cache")
        .arg(&not_a_directory)
        .output()
        .unwrap();
    assert_eq!(invalid_root.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&invalid_root.stderr).contains("is not a directory"));
}

#[test]
fn album_verification_consumes_cached_source_analyses() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first.wav");
    let second = directory.path().join("second.wav");
    let output = directory.path().join("normalized");
    let cache = directory.path().join("analysis-cache");
    write_batch_test_wav(&first, 440.0);
    write_batch_test_wav(&second, 880.0);

    let warm = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg(&first)
        .arg(&second)
        .arg("--album")
        .arg("--dry-run")
        .arg("-o")
        .arg(&output)
        .arg("--analysis-cache")
        .arg(&cache)
        .arg("--warm-cache")
        .output()
        .unwrap();
    assert!(
        warm.status.success(),
        "{}",
        String::from_utf8_lossy(&warm.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&warm.stderr)
            .matches("analysis cache miss; stored")
            .count(),
        2
    );

    let verified = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg(&first)
        .arg(&second)
        .arg("--album")
        .arg("--verify")
        .arg("--verify-tolerance")
        .arg("1")
        .arg("-o")
        .arg(&output)
        .arg("--analysis-cache")
        .arg(&cache)
        .output()
        .unwrap();
    assert!(
        verified.status.success(),
        "{}",
        String::from_utf8_lossy(&verified.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&verified.stderr)
            .matches("analysis cache hit")
            .count(),
        2
    );
    assert!(output.join("first_normalized.wav").is_file());
    assert!(output.join("second_normalized.wav").is_file());
}

#[test]
fn resumable_batch_skips_verified_outputs_and_recovers_only_missing_or_changed_assets() {
    let directory = tempfile::tempdir().unwrap();
    let first_input = directory.path().join("first.wav");
    let second_input = directory.path().join("second.wav");
    let output_directory = directory.path().join("normalized");
    let state_path = directory.path().join("job.json");
    let progress_path = directory.path().join("progress.ndjson");
    write_batch_test_wav(&first_input, 440.0);
    write_batch_test_wav(&second_input, 880.0);

    let run = |overwrite: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_forge"));
        command
            .arg(&first_input)
            .arg(&second_input)
            .arg("-o")
            .arg(&output_directory)
            .arg("--job-state")
            .arg(&state_path)
            .arg("--progress")
            .arg(&progress_path)
            .arg("--jobs")
            .arg("2");
        if overwrite {
            command.arg("--overwrite");
        }
        command.output().unwrap()
    };
    let read_events = || {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../schema/batch-progress-v1.schema.json")).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        std::fs::read_to_string(&progress_path)
            .unwrap()
            .lines()
            .map(|line| {
                let event: serde_json::Value = serde_json::from_str(line).unwrap();
                assert!(
                    validator.is_valid(&event),
                    "invalid progress event: {event}"
                );
                event
            })
            .collect::<Vec<_>>()
    };

    let first_run = run(false);
    assert!(
        first_run.status.success(),
        "{}",
        String::from_utf8_lossy(&first_run.stderr)
    );
    let state_schema: serde_json::Value =
        serde_json::from_str(include_str!("../schema/batch-job-v2.schema.json")).unwrap();
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
    assert!(jsonschema::validator_for(&state_schema)
        .unwrap()
        .is_valid(&state));
    assert_eq!(state["asset_count"], 2);
    assert_eq!(state["completed_count"], 2);
    let first_output = PathBuf::from(state["assets"][0]["output"].as_str().unwrap());
    let second_output = PathBuf::from(state["assets"][1]["output"].as_str().unwrap());
    assert!(first_output.is_file());
    assert!(second_output.is_file());
    assert_eq!(
        read_events()
            .iter()
            .map(|event| event["event"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "job_started",
            "asset_started",
            "asset_started",
            "asset_completed",
            "asset_completed",
            "job_completed"
        ]
    );

    let resumed = run(false);
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(
        read_events()
            .iter()
            .map(|event| event["event"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "job_started",
            "asset_skipped",
            "asset_skipped",
            "job_completed"
        ]
    );

    let first_before_recovery = std::fs::read(&first_output).unwrap();
    std::fs::remove_file(&second_output).unwrap();
    let recovered = run(false);
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(std::fs::read(&first_output).unwrap(), first_before_recovery);
    assert!(second_output.is_file());
    assert_eq!(
        read_events()
            .iter()
            .map(|event| event["event"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "job_started",
            "asset_skipped",
            "asset_started",
            "asset_completed",
            "job_completed"
        ]
    );

    std::fs::write(&first_output, b"externally changed").unwrap();
    let rejected = run(false);
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("completed output changed"));

    let rebuilt = run(true);
    assert!(
        rebuilt.status.success(),
        "{}",
        String::from_utf8_lossy(&rebuilt.stderr)
    );
    assert_ne!(std::fs::read(&first_output).unwrap(), b"externally changed");
    assert_eq!(
        read_events()
            .iter()
            .map(|event| event["event"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "job_started",
            "asset_started",
            "asset_completed",
            "asset_skipped",
            "job_completed"
        ]
    );
}

#[test]
fn resumable_batch_checkpoints_before_a_later_asset_fails() {
    let directory = tempfile::tempdir().unwrap();
    let valid_input = directory.path().join("valid.wav");
    let invalid_input = directory.path().join("invalid.wav");
    let later_input = directory.path().join("later.wav");
    let output_directory = directory.path().join("normalized");
    let state_path = directory.path().join("job.json");
    let progress_path = directory.path().join("progress.ndjson");
    write_batch_test_wav(&valid_input, 440.0);
    std::fs::write(&invalid_input, b"not a WAVE file").unwrap();
    write_batch_test_wav(&later_input, 880.0);
    std::fs::create_dir(&output_directory).unwrap();
    let later_output = output_directory.join("later_normalized.wav");
    std::fs::write(&later_output, b"preserve later destination").unwrap();

    let run = || {
        Command::new(env!("CARGO_BIN_EXE_forge"))
            .arg(&valid_input)
            .arg(&invalid_input)
            .arg(&later_input)
            .arg("-o")
            .arg(&output_directory)
            .arg("--overwrite")
            .arg("--job-state")
            .arg(&state_path)
            .arg("--progress")
            .arg(&progress_path)
            .arg("--jobs")
            .arg("3")
            .output()
            .unwrap()
    };
    let event_names = || {
        std::fs::read_to_string(&progress_path)
            .unwrap()
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()["event"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect::<Vec<_>>()
    };

    let first_run = run();
    assert!(!first_run.status.success());
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
    assert_eq!(state["completed_count"], 1);
    assert_eq!(
        event_names(),
        [
            "job_started",
            "asset_started",
            "asset_started",
            "asset_started",
            "asset_completed",
            "asset_failed"
        ]
    );
    assert_eq!(
        std::fs::read(&later_output).unwrap(),
        b"preserve later destination"
    );
    let valid_output = PathBuf::from(state["assets"][0]["output"].as_str().unwrap());
    let valid_output_bytes = std::fs::read(&valid_output).unwrap();

    let resumed = run();
    assert!(!resumed.status.success());
    assert_eq!(std::fs::read(valid_output).unwrap(), valid_output_bytes);
    assert_eq!(
        event_names(),
        [
            "job_started",
            "asset_skipped",
            "asset_started",
            "asset_started",
            "asset_failed"
        ]
    );
    assert_eq!(
        std::fs::read(later_output).unwrap(),
        b"preserve later destination"
    );
}

#[test]
fn parallel_batch_matches_serial_bytes_and_reports_ordered_waves() {
    let directory = tempfile::tempdir().unwrap();
    let inputs = (0..4)
        .map(|index| {
            let path = directory.path().join(format!("track-{index}.wav"));
            write_batch_test_wav(&path, 330.0 + f64::from(index) * 137.0);
            path
        })
        .collect::<Vec<_>>();
    let serial_directory = directory.path().join("serial");
    let parallel_directory = directory.path().join("parallel");
    let progress_path = directory.path().join("parallel.ndjson");

    let run = |jobs: usize, output: &std::path::Path, progress: Option<&std::path::Path>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_forge"));
        command
            .args(&inputs)
            .arg("-o")
            .arg(output)
            .arg("--jobs")
            .arg(jobs.to_string());
        if let Some(progress) = progress {
            command.arg("--progress").arg(progress);
        }
        command.output().unwrap()
    };

    let serial = run(1, &serial_directory, None);
    assert!(
        serial.status.success(),
        "{}",
        String::from_utf8_lossy(&serial.stderr)
    );
    let parallel = run(4, &parallel_directory, Some(&progress_path));
    assert!(
        parallel.status.success(),
        "{}",
        String::from_utf8_lossy(&parallel.stderr)
    );

    for input in &inputs {
        let output_name = format!(
            "{}_normalized.wav",
            input.file_stem().unwrap().to_string_lossy()
        );
        assert_eq!(
            std::fs::read(serial_directory.join(&output_name)).unwrap(),
            std::fs::read(parallel_directory.join(&output_name)).unwrap(),
            "parallel output differed for {output_name}"
        );
    }

    let events = std::fs::read_to_string(&progress_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        events
            .iter()
            .map(|event| event["event"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "job_started",
            "asset_started",
            "asset_started",
            "asset_started",
            "asset_started",
            "asset_completed",
            "asset_completed",
            "asset_completed",
            "asset_completed",
            "job_completed"
        ]
    );
    assert_eq!(
        events[1..5]
            .iter()
            .map(|event| event["index"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [0, 1, 2, 3]
    );
    assert_eq!(
        events[5..9]
            .iter()
            .map(|event| event["index"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [0, 1, 2, 3]
    );
    assert_eq!(
        events[5..9]
            .iter()
            .map(|event| event["completed"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
}

#[test]
fn parallel_cached_batch_matches_serial_bytes_and_reports_hits_in_input_order() {
    let directory = tempfile::tempdir().unwrap();
    let inputs = (0..4)
        .map(|index| {
            let path = directory.path().join(format!("cached-track-{index}.wav"));
            write_batch_test_wav(&path, 250.0 + f64::from(index) * 173.0);
            path
        })
        .collect::<Vec<_>>();
    let cache = directory.path().join("analysis-cache");
    let warm_outputs = directory.path().join("warm-outputs");
    std::fs::create_dir(&warm_outputs).unwrap();
    let warm = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args(&inputs)
        .arg("--dry-run")
        .arg("--analysis-cache")
        .arg(&cache)
        .arg("--warm-cache")
        .arg("-o")
        .arg(&warm_outputs)
        .output()
        .unwrap();
    assert!(
        warm.status.success(),
        "{}",
        String::from_utf8_lossy(&warm.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&warm.stderr)
            .matches("analysis cache miss; stored")
            .count(),
        inputs.len()
    );

    let serial_directory = directory.path().join("cached-serial");
    let parallel_directory = directory.path().join("cached-parallel");
    let progress_path = directory.path().join("cached-parallel.ndjson");
    let run = |jobs: usize, output: &std::path::Path, progress: Option<&std::path::Path>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_forge"));
        command
            .args(&inputs)
            .arg("-o")
            .arg(output)
            .arg("--jobs")
            .arg(jobs.to_string())
            .arg("--analysis-cache")
            .arg(&cache);
        if let Some(progress) = progress {
            command.arg("--progress").arg(progress);
        }
        command.output().unwrap()
    };

    let serial = run(1, &serial_directory, None);
    assert!(
        serial.status.success(),
        "{}",
        String::from_utf8_lossy(&serial.stderr)
    );
    let parallel = run(4, &parallel_directory, Some(&progress_path));
    assert!(
        parallel.status.success(),
        "{}",
        String::from_utf8_lossy(&parallel.stderr)
    );

    let expected_hits = inputs
        .iter()
        .map(|input| format!("analysis cache hit: {}", input.display()))
        .collect::<Vec<_>>();
    for result in [&serial, &parallel] {
        let stderr = String::from_utf8_lossy(&result.stderr);
        let hits = stderr
            .lines()
            .filter(|line| line.starts_with("analysis cache hit:"))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(hits, expected_hits);
    }

    for input in &inputs {
        let output_name = format!(
            "{}_normalized.wav",
            input.file_stem().unwrap().to_string_lossy()
        );
        assert_eq!(
            std::fs::read(serial_directory.join(&output_name)).unwrap(),
            std::fs::read(parallel_directory.join(&output_name)).unwrap(),
            "parallel cached output differed for {output_name}"
        );
    }

    let event_names = std::fs::read_to_string(&progress_path)
        .unwrap()
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["event"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        event_names,
        [
            "job_started",
            "asset_started",
            "asset_started",
            "asset_started",
            "asset_started",
            "asset_completed",
            "asset_completed",
            "asset_completed",
            "asset_completed",
            "job_completed"
        ]
    );
}

#[cfg(unix)]
#[test]
fn progress_path_cannot_alias_an_audio_input_through_a_symlink() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("input.wav");
    let output = directory.path().join("output.wav");
    let progress_alias = directory.path().join("progress.ndjson");
    write_batch_test_wav(&input, 440.0);
    let original = std::fs::read(&input).unwrap();
    std::os::unix::fs::symlink(&input, &progress_alias).unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--progress")
        .arg(&progress_alias)
        .output()
        .unwrap();

    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr)
        .contains("--progress must not overwrite an audio input or output"));
    assert_eq!(std::fs::read(input).unwrap(), original);
}

#[test]
fn progress_can_use_stdout_but_cannot_share_it_with_binary_audio() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("input.wav");
    let output = directory.path().join("output.wav");
    write_batch_test_wav(&input, 440.0);

    let progress = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--progress")
        .arg("-")
        .output()
        .unwrap();
    assert!(
        progress.status.success(),
        "{}",
        String::from_utf8_lossy(&progress.stderr)
    );
    let schema: serde_json::Value =
        serde_json::from_str(include_str!("../schema/batch-progress-v1.schema.json")).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let events = String::from_utf8(progress.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 4);
    assert!(events.iter().all(|event| validator.is_valid(event)));
    assert_eq!(events[0]["event"], "job_started");
    assert_eq!(events[3]["event"], "job_completed");

    let conflict = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg(&input)
        .arg("-o")
        .arg("-")
        .arg("--progress")
        .arg("-")
        .output()
        .unwrap();
    assert!(!conflict.status.success());
    assert!(String::from_utf8_lossy(&conflict.stderr)
        .contains("binary output and --progress cannot both use stdout"));
}

#[test]
fn album_mode_still_refuses_an_existing_output_without_overwrite() {
    let directory = tempfile::tempdir().unwrap();
    let first_input = directory.path().join("first.wav");
    let second_input = directory.path().join("second.wav");
    let output_directory = directory.path().join("normalized");
    let existing_output = output_directory.join("first_normalized.wav");
    write_batch_test_wav(&first_input, 440.0);
    write_batch_test_wav(&second_input, 880.0);
    std::fs::create_dir(&output_directory).unwrap();
    std::fs::write(&existing_output, b"keep this output").unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg("--album")
        .arg(&first_input)
        .arg(&second_input)
        .arg("-o")
        .arg(&output_directory)
        .output()
        .unwrap();

    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("(use --overwrite)"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(std::fs::read(existing_output).unwrap(), b"keep this output");
}

#[test]
fn preset_is_accepted_but_explicit_target_conflicts() {
    let cli = Cli::try_parse_from(["forge", "track.wav", "--preset", "ebu-r128"]).unwrap();
    assert_eq!(cli.preset.as_deref(), Some("ebu-r128"));
    assert!(
        Cli::try_parse_from(["forge", "track.wav", "--preset", "spotify", "--target=-12"]).is_err()
    );
    let spotify_revision = Cli::try_parse_from([
        "forge",
        "track.wav",
        "--preset",
        "spotify-normal-2026-07-30",
    ])
    .unwrap();
    assert_eq!(
        spotify_revision.preset.as_deref(),
        Some("spotify-normal-2026-07-30")
    );
    assert!(Cli::try_parse_from([
        "forge",
        "track.wav",
        "--preset",
        "spotify-normal-2025-01-01"
    ])
    .is_err());
    let arib = Cli::try_parse_from(["forge", "track.wav", "--preset", "arib-tr-b32"]).unwrap();
    assert_eq!(arib.preset.as_deref(), Some("arib-tr-b32"));
}

#[test]
fn sound_check_requires_metadata_write_mode() {
    assert!(Cli::try_parse_from(["forge", "track.m4a", "--sound-check"]).is_err());
    let cli = Cli::try_parse_from(["forge", "track.m4a", "--write-tags", "--sound-check"]).unwrap();
    assert!(cli.write_tags);
    assert!(cli.sound_check);
}

#[test]
fn sound_check_m4a_write_is_read_back_exactly() {
    if !Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("tone.m4a");
    let generated = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:sample_rate=48000:duration=1",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            input.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(generated.success());

    let result = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args([input.to_str().unwrap(), "--write-tags", "--sound-check"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let stderr = String::from_utf8(result.stderr).unwrap();
    assert!(stderr.contains("wrote and verified Sound Check metadata"));

    let value = forge_normalizer::metadata::read_sound_check(&input)
        .unwrap()
        .expect("Sound Check metadata");
    assert_eq!(
        forge_normalizer::metadata::SoundCheck::parse(&value.canonical_value()).unwrap(),
        value
    );
}

#[cfg(feature = "opus-encoding")]
#[test]
fn metadata_only_opus_cli_uses_rfc7845_and_preserves_audio() {
    if !Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("speech.opus");
    let generated = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=700:sample_rate=48000:duration=1",
            "-c:a",
            "libopus",
            input.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(generated.success());
    let before = forge_normalizer::decoder::decode(&input).unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args([input.to_str().unwrap(), "--write-tags"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stderr)
        .contains("wrote and verified RFC 7845 R128_GAIN metadata"));
    let (track, album) = forge_normalizer::opus_tags::read_r128_tags(&input).unwrap();
    assert!(track.is_some());
    assert_eq!(album, None);
    let after = forge_normalizer::decoder::decode(&input).unwrap();
    assert_eq!(before.frames, after.frames);
    assert_eq!(before.data, after.data);
}

#[test]
fn sound_check_metadata_api_round_trips_every_supported_container() {
    if !Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    let cases = [
        ("m4a", "aac", Some("128k")),
        ("mp3", "libmp3lame", Some("128k")),
        ("aiff", "pcm_s16be", None),
        ("aac", "aac", Some("128k")),
    ];
    let expected = forge_normalizer::metadata::SoundCheck::from_r128(-16.0, 0.75).unwrap();
    for (extension, codec, bitrate) in cases {
        let input = directory.path().join(format!("tone.{extension}"));
        let mut command = Command::new("ffmpeg");
        command.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:sample_rate=48000:duration=1",
            "-c:a",
            codec,
        ]);
        if let Some(bitrate) = bitrate {
            command.args(["-b:a", bitrate]);
        }
        let generated = command.arg(&input).status().unwrap();
        assert!(generated.success(), "failed to generate {extension}");

        let written = forge_normalizer::metadata::write_sound_check(&input, &expected).unwrap();
        assert_eq!(written, expected, "{extension} write result");
        assert_eq!(
            forge_normalizer::metadata::read_sound_check(&input).unwrap(),
            Some(expected.clone()),
            "{extension} read result"
        );
    }
}

#[test]
fn platform_preset_reports_version_source_and_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("tone.wav");
    let samples = vec![0.0; 48_000];
    let buffer = AudioBuffer {
        sample_rate: 48_000,
        channels: 1,
        frames: samples.len(),
        data: vec![samples],
        channel_roles: default_channel_roles(1),
        source_kind: PcmKind::F32,
    };
    WavWriter::write(&input, &buffer, PcmKind::F32, false).unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args([
            input.to_str().unwrap(),
            "--preset",
            "apple-music",
            "--dry-run",
        ])
        .output()
        .unwrap();

    assert!(result.status.success());
    let stderr = String::from_utf8(result.stderr).unwrap();
    assert!(stderr.contains("preset apple-music-reference-2026-07-30"));
    assert!(stderr.contains("profile evidence: engineering-reference"));
    assert!(stderr.contains("https://support.apple.com/en-us/109331"));
    assert!(stderr.contains("checked 2026-07-30"));
    assert!(stderr.contains("does not publish these numeric values"));
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
    for format in ["alac", "vorbis"] {
        let cli = Cli::try_parse_from(["forge", "track.wav", "--format", format]).unwrap();
        assert_eq!(cli.format.as_deref(), Some(format));
    }
}

#[test]
fn input_track_selection_and_content_probed_defaults_are_exposed() {
    let help = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("--audio-track <INDEX>"));

    let directory = tempfile::tempdir().unwrap();
    let misleading = directory.path().join("misleading.flac");
    write_batch_test_wav(&misleading, 440.0);
    let normalized = directory.path().join("misleading_normalized.wav");
    let result = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg(&misleading)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(normalized.is_file());
    assert!(!directory.path().join("misleading_normalized.flac").exists());

    let unavailable_track = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg(&misleading)
        .arg("--analyze")
        .arg("--audio-track")
        .arg("1")
        .output()
        .unwrap();
    assert!(!unavailable_track.status.success());
    assert!(
        String::from_utf8_lossy(&unavailable_track.stderr)
            .contains("audio track index 1 is unavailable"),
        "{}",
        String::from_utf8_lossy(&unavailable_track.stderr)
    );
}

#[test]
fn selected_audio_track_drives_the_measured_programme() {
    if !Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("two-audio-tracks.m4a");
    let cache = directory.path().join("analysis-cache");
    let generated = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=2",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:sample_rate=48000:duration=2",
            "-filter_complex",
            "[0:a]volume=0.02[quiet];[1:a]volume=0.4[loud]",
            "-map",
            "[quiet]",
            "-map",
            "[loud]",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            input.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(generated.success());

    let measure = |track: &str| {
        let result = Command::new(env!("CARGO_BIN_EXE_forge"))
            .arg(&input)
            .args(["--analyze", "--json", "--audio-track", track])
            .arg("--analysis-cache")
            .arg(&cache)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        report[0]["integrated_lufs"].as_f64().unwrap()
    };
    let quiet_lufs = measure("0");
    let loud_lufs = measure("1");
    assert!(
        loud_lufs - quiet_lufs > 20.0,
        "track selection was not reflected in loudness: quiet={quiet_lufs}, loud={loud_lufs}"
    );
    let mut entry_count = 0;
    for prefix in std::fs::read_dir(cache.join("v5")).unwrap() {
        for content in std::fs::read_dir(prefix.unwrap().path()).unwrap() {
            entry_count += std::fs::read_dir(content.unwrap().path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
                .count();
        }
    }
    assert_eq!(entry_count, 2, "audio tracks must have distinct cache keys");
}

#[test]
fn sample_rate_conversion_options_are_validated_and_exposed() {
    let cli = Cli::try_parse_from([
        "forge",
        "track.wav",
        "--sample-rate",
        "44100",
        "--resample-quality",
        "best",
    ])
    .unwrap();
    assert_eq!(cli.sample_rate_hz, Some(44_100));
    assert_eq!(cli.resample_quality, "best");
    assert!(Cli::try_parse_from(["forge", "track.wav", "--sample-rate", "1000"]).is_err());
    assert!(Cli::try_parse_from(["forge", "track.wav", "--resample-quality", "best"]).is_err());
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

#[test]
fn ebu_production_profile_writes_a_rule_audit() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("production.bw64");
    let audit = directory.path().join("tech3393.json");
    let buffer = AudioBuffer {
        sample_rate: 48_000,
        channels: 1,
        frames: 48_000,
        data: vec![vec![0.0; 48_000]],
        channel_roles: default_channel_roles(1),
        source_kind: PcmKind::F32,
    };
    let mut chna = Vec::with_capacity(44);
    chna.extend_from_slice(&1_u16.to_le_bytes());
    chna.extend_from_slice(&1_u16.to_le_bytes());
    chna.extend_from_slice(&1_u16.to_le_bytes());
    chna.extend_from_slice(b"ATU_00000001");
    chna.extend_from_slice(&[0; 14]);
    chna.extend_from_slice(&[0; 11]);
    chna.push(0);
    WavWriter::write_with_metadata(
        &input,
        &buffer,
        PcmKind::F32,
        false,
        WavContainer::Bw64,
        &[
            WaveChunk {
                id: *b"axml",
                body: br#"<audioFormatExtended version="ITU-R_BS.2076-3">
  <profileList>
    <profile profileName="EBU Production Profile" profileVersion="1.0" profileLevel="1">EBU Tech 3393</profile>
  </profileList>
  <tagList><tagGroup><tag class="urn:profile:production:Layer">1</tag><audioProgrammeIDRef>APR_1001</audioProgrammeIDRef></tagGroup></tagList>
  <audioProgramme audioProgrammeID="APR_1001" audioProgrammeName="Programme">
    <audioContentIDRef>ACO_1001</audioContentIDRef>
  </audioProgramme>
  <audioContent audioContentID="ACO_1001" audioContentName="Content">
    <audioObjectIDRef>AO_1001</audioObjectIDRef><dialogue>1</dialogue>
  </audioContent>
  <audioObject audioObjectID="AO_1001" audioObjectName="Object" importance="10" interact="0">
    <audioPackFormatIDRef>AP_00010001</audioPackFormatIDRef>
    <audioTrackUIDRef>ATU_00000001</audioTrackUIDRef>
  </audioObject>
  <audioTrackUID UID="ATU_00000001">
    <audioChannelFormatIDRef>AC_00010001</audioChannelFormatIDRef>
    <audioPackFormatIDRef>AP_00010001</audioPackFormatIDRef>
  </audioTrackUID>
</audioFormatExtended>"#.to_vec(),
            },
            WaveChunk {
                id: *b"chna",
                body: chna,
            },
        ],
    )
    .unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args([
            input.to_str().unwrap(),
            "--analyze",
            "--json",
            "--adm-profile",
            "ebu-production",
            "--adm-profile-mode",
            "write",
            "--adm-profile-report",
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
        report[0]["adm_production_profile_standard"],
        "EBU Tech 3393"
    );
    assert_eq!(report[0]["adm_model_standard"], "ITU-R BS.2076-3");
    assert_eq!(report[0]["adm_model_version"], "ITU-R_BS.2076-3");
    assert_eq!(report[0]["adm_production_profile_mode"], "write");
    assert_eq!(report[0]["adm_production_profile_passed"], true);
    let audit: serde_json::Value = serde_json::from_slice(&std::fs::read(audit).unwrap()).unwrap();
    assert_eq!(audit["validator"], "forge-tech3393-2025-bs2076-3-3");
    assert_eq!(audit["adm_standard"], "ITU-R BS.2076-3");
    assert!(audit["rules"].as_array().unwrap().len() >= 16);
}

fn wav_fixture_bytes() -> Vec<u8> {
    wav_fixture_with_frames(48_000)
}

fn wav_fixture_with_frames(frames: usize) -> Vec<u8> {
    let file = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
    let buffer = AudioBuffer {
        sample_rate: 48_000,
        channels: 1,
        frames,
        data: vec![(0..frames)
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
    assert_eq!(value["assets"][0]["container_qc"]["passed"], true);
    assert_eq!(value["assets"][0]["container_qc"]["format"], "wave");
}

#[test]
fn anomaly_audit_is_attached_to_manifest_in_input_order() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("one.wav");
    let provider = directory.path().join("provider.json");
    let audit = directory.path().join("audit.json");
    let manifest = directory.path().join("delivery.json");
    std::fs::write(&input, wav_fixture_bytes()).unwrap();
    std::fs::write(
        &provider,
        format!(
            r#"{{
              "schema_version":1,
              "provider":"reviewed-detector",
              "provider_version":"2.0",
              "model":"audio-quality",
              "model_version":"2026-08",
              "model_sha256":"{}",
              "source_sha256":"{}",
              "source_duration_seconds":1.0,
              "sample_rate_hz":48000,
              "events":[{{"kind":"pop","start_seconds":0.1,"end_seconds":0.2,"confidence":0.95,"severity":0.8,"channel":1}}]
            }}"#,
            "a".repeat(64),
            "b".repeat(64)
        ),
    )
    .unwrap();
    let provider_status = Command::new(env!("CARGO_BIN_EXE_forge-anomaly-provider"))
        .args([
            "--output",
            audit.to_str().unwrap(),
            provider.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(provider_status.success());

    let result = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args([
            input.to_str().unwrap(),
            "--analyze",
            "--manifest",
            manifest.to_str().unwrap(),
            "--anomaly-audit",
            audit.to_str().unwrap(),
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
    assert_eq!(value["passed_count"], 1);
    assert_eq!(value["assets"][0]["model_qc"]["passed"], false);
    assert_eq!(
        value["assets"][0]["model_qc"]["audit"]["selected_by_kind"]["pop"],
        1
    );
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
        .ends_with("delivery-manifest-v4"));
    let results = value["assets"][0]["qc"]["results"].as_array().unwrap();
    assert_eq!(results.len(), 21);
    assert_eq!(results[0]["ebu_qc_id"], "0078B");
    assert_eq!(results[4]["ebu_qc_id"], "0010B");
    assert_eq!(results[5]["ebu_qc_id"], "0084B");
    assert_eq!(results[6]["ebu_qc_id"], "0004F");
    assert_eq!(results[7]["ebu_qc_id"], "0008B");
    assert_eq!(results[8]["ebu_qc_id"], "0012B");
    assert_eq!(results[9]["ebu_qc_id"], "0057B");
    assert_eq!(results[10]["ebu_qc_id"], "0077B");
    assert_eq!(results[11]["ebu_qc_id"], "0088B");
    assert_eq!(results[12]["ebu_qc_id"], "0086B");
    assert_eq!(results[13]["ebu_qc_id"], "0170B");
    assert_eq!(results[14]["ebu_qc_id"], "0230B");
    assert_eq!(results[15]["ebu_qc_id"], "0095B");
    assert_eq!(results[16]["ebu_qc_id"], "0124B");
    assert_eq!(results[17]["ebu_qc_id"], "FORGE-DC-OFFSET");
    assert_eq!(results[18]["ebu_qc_id"], "FORGE-INTERCHANNEL-DELAY");
    assert_eq!(results[19]["ebu_qc_id"], "FORGE-STUCK-SAMPLES");
    assert_eq!(results[20]["ebu_qc_id"], "FORGE-DISCONTINUITY");
    assert!(results
        .iter()
        .all(|result| result["source_url"].is_string()));
    assert!(value["assets"][0]["qc"]["schema"]
        .as_str()
        .unwrap()
        .ends_with("ebu-qc-results-v2"));
}

#[test]
fn ebu_qc_writes_scenario1_2026_04_catalogue_xml() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("programme.wav");
    let report = directory.path().join("reports/ebu-qc.xml");
    std::fs::write(&input, wav_fixture_with_frames(48_000 * 4)).unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args([
            input.to_str().unwrap(),
            "--analyze",
            "--ebu-qc",
            "--silence-threshold=-200",
            "--tone-threshold=1",
            "--ebu-qc-xml",
            report.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let xml = std::fs::read_to_string(&report).unwrap();
    assert!(xml.contains("<Report xmlns=\"tag:qc.ebu.ch,2026-04\">"));
    assert!(xml.contains("<EditRate>48000/1</EditRate>"));
    assert!(xml.contains("<EBUQCID>0010B</EBUQCID>"));
    assert!(xml.contains("<EBUQCID>0084B</EBUQCID>"));
    assert!(xml.contains("<Name>SilenceThresholdLevel</Name>"));
    assert!(xml.contains("<Name>LoudnessMomentaryOverTime</Name>"));
    assert!(!xml.contains("<Name>Event</Name>"));
    assert!(!xml.contains("<Name>CheckResult</Name>"));
    assert!(!xml.contains("FORGE-DC-OFFSET"));
    let summary = validate_xml(xml.as_bytes(), EbuQcValidationProfile::Scenario1).unwrap();
    assert!(summary.check_item_count > 0);
    assert!(summary.report_item_count > 0);

    let validation = Command::new(env!("CARGO_BIN_EXE_forge-report"))
        .args(["ebu-qc-validate", report.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        validation.status.success(),
        "{}",
        String::from_utf8_lossy(&validation.stderr)
    );
    assert!(String::from_utf8_lossy(&validation.stderr).contains("valid EBU QC 2026-04"));

    let invalid_report = directory.path().join("invalid-ebu-qc.xml");
    std::fs::write(
        &invalid_report,
        xml.replacen(
            "<Name>LoudnessMomentaryOverTime</Name>",
            "<Name>CheckResult</Name>",
            1,
        ),
    )
    .unwrap();
    let rejected = Command::new(env!("CARGO_BIN_EXE_forge-report"))
        .args(["ebu-qc-validate", invalid_report.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(rejected.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("prohibits Output/Name=CheckResult"));

    if Command::new("xmllint").arg("--version").output().is_ok() {
        let schema = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("schema/ebu-qc-2026-04/forge-validation.xsd");
        let xsd = Command::new("xmllint")
            .arg("--noout")
            .arg("--schema")
            .arg(schema)
            .arg(&report)
            .output()
            .unwrap();
        assert!(
            xsd.status.success(),
            "{}",
            String::from_utf8_lossy(&xsd.stderr)
        );
    }
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
fn custom_content_profile_reports_lra_and_plr_boundaries() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("programme.wav");
    let profile = directory.path().join("content-profile.toml");
    std::fs::write(&input, wav_fixture_bytes()).unwrap();
    std::fs::write(
        &profile,
        r#"
name = "content-profile"
min_loudness_range_lu = 0.0
max_loudness_range_lu = 100.0
min_peak_to_loudness_ratio_lu = 0.0
max_peak_to_loudness_ratio_lu = 100.0
peak_to_loudness_ratio_max_exclusive = true
"#,
    )
    .unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args([
            input.to_str().unwrap(),
            "--analyze",
            "--json",
            "--compliance",
            profile.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report[0]["compliance_min_peak_to_loudness_ratio_lu"], 0.0);
    assert_eq!(report[0]["compliance_max_peak_to_loudness_ratio_lu"], 100.0);
    assert_eq!(
        report[0]["compliance_peak_to_loudness_ratio_max_exclusive"],
        true
    );
    assert_eq!(report[0]["compliance_peak_to_loudness_ratio_pass"], true);
    let rules: serde_json::Value =
        serde_json::from_str(report[0]["compliance_rules_json"].as_str().unwrap()).unwrap();
    let plr = rules
        .as_array()
        .unwrap()
        .iter()
        .find(|rule| rule["metric"] == "peak_to_loudness_ratio_lu")
        .unwrap();
    assert_eq!(plr["minimum_inclusive"], true);
    assert_eq!(plr["maximum_inclusive"], false);

    let text = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args([
            input.to_str().unwrap(),
            "--analyze",
            "--compliance",
            profile.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(text.status.success());
    let stderr = String::from_utf8_lossy(&text.stderr);
    assert!(stderr.contains("peak_to_loudness_ratio_lu:"));
    assert!(stderr.contains("[0.00, 100.00)"));
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

#[test]
fn normalization_difference_report_is_versioned_bounded_and_schema_valid() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("source.wav");
    let output = directory.path().join("normalized.wav");
    let report_path = directory.path().join("difference.json");
    let sample_rate = 48_000;
    let samples = (0..sample_rate * 2)
        .map(|index| {
            (0.5 * (index as f64 * std::f64::consts::TAU * 997.0 / sample_rate as f64).sin()) as f32
        })
        .collect::<Vec<_>>();
    let audio = AudioBuffer {
        sample_rate,
        channels: 1,
        frames: samples.len(),
        data: vec![samples],
        channel_roles: default_channel_roles(1),
        source_kind: PcmKind::F32,
    };
    WavWriter::write(&input, &audio, PcmKind::F32, false).unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args([
            input.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
            "--target=-3",
            "--ceiling=-6",
            "--limiter",
            "--bits=16",
            "--verify",
            "--difference-report",
            report_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );

    let instance: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&report_path).unwrap()).unwrap();
    let schema: serde_json::Value = serde_json::from_str(include_str!(
        "../schema/normalization-difference-v1.schema.json"
    ))
    .unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let errors = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");

    assert_eq!(
        instance["schema"],
        forge_normalizer::normalization_diff::RESULT_SCHEMA
    );
    assert_eq!(instance["assets"].as_array().unwrap().len(), 1);
    let asset = &instance["assets"][0];
    assert_eq!(
        asset["protection"]["mode"],
        "linked-lookahead-true-peak-limiter"
    );
    assert!(
        asset["protection"]["maximum_limiter_reduction_db"]
            .as_f64()
            .unwrap()
            > 0.0
    );
    assert!(
        asset["clipping"]["pre_protection_ceiling_exceeding_samples"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(
        asset["clipping"]["protected_full_scale_exceeding_samples"],
        0
    );
    assert!(asset["gain_envelope"].as_array().unwrap().len() <= 10_000);
    assert_eq!(asset["input"]["sha256"].as_str().unwrap().len(), 64);
    assert_eq!(asset["output"]["sha256"].as_str().unwrap().len(), 64);
}

#[test]
fn difference_report_rejects_analysis_dry_run_and_stdout() {
    assert!(Cli::try_parse_from([
        "forge",
        "track.wav",
        "--analyze",
        "--difference-report",
        "difference.json"
    ])
    .is_err());
    assert!(Cli::try_parse_from([
        "forge",
        "track.wav",
        "--dry-run",
        "--difference-report",
        "difference.json"
    ])
    .is_err());
}

#[test]
fn album_difference_report_contains_every_output() {
    let directory = tempfile::tempdir().unwrap();
    let output_directory = directory.path().join("normalized");
    std::fs::create_dir(&output_directory).unwrap();
    let report_path = directory.path().join("album-difference.json");
    let sample_rate = 48_000;
    let samples = (0..sample_rate)
        .map(|index| {
            (0.1 * (index as f64 * std::f64::consts::TAU * 440.0 / sample_rate as f64).sin()) as f32
        })
        .collect::<Vec<_>>();
    let audio = AudioBuffer {
        sample_rate,
        channels: 1,
        frames: samples.len(),
        data: vec![samples],
        channel_roles: default_channel_roles(1),
        source_kind: PcmKind::F32,
    };
    let first = directory.path().join("first.wav");
    let second = directory.path().join("second.wav");
    WavWriter::write(&first, &audio, PcmKind::F32, false).unwrap();
    WavWriter::write(&second, &audio, PcmKind::F32, false).unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args([
            first.to_str().unwrap(),
            second.to_str().unwrap(),
            "--album",
            "--output",
            output_directory.to_str().unwrap(),
            "--difference-report",
            report_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(report_path).unwrap()).unwrap();
    assert_eq!(report["assets"].as_array().unwrap().len(), 2);
    assert!(report["assets"]
        .as_array()
        .unwrap()
        .iter()
        .all(|asset| asset["gain_envelope"].as_array().unwrap().len() == 1));
}

#[test]
fn configured_difference_report_path_is_relative_to_config() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("programme.wav");
    let output = directory.path().join("programme-normalized.wav");
    let config = directory.path().join("forge.toml");
    std::fs::write(&input, wav_fixture_bytes()).unwrap();
    std::fs::write(
        &config,
        r#"
            [output]
            difference_report = "reports/difference.json"
        "#,
    )
    .unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args([
            input.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report_path = directory.path().join("reports/difference.json");
    assert!(report_path.is_file());
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(report_path).unwrap()).unwrap();
    assert_eq!(report["assets"].as_array().unwrap().len(), 1);
}

#[test]
fn difference_report_represents_silence_without_non_finite_json() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("silence.wav");
    let output = directory.path().join("normalized.wav");
    let report_path = directory.path().join("difference.json");
    let audio = AudioBuffer {
        sample_rate: 48_000,
        channels: 1,
        frames: 48_000,
        data: vec![vec![0.0; 48_000]],
        channel_roles: default_channel_roles(1),
        source_kind: PcmKind::F32,
    };
    WavWriter::write(&input, &audio, PcmKind::F32, false).unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args([
            input.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
            "--difference-report",
            report_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let instance: serde_json::Value =
        serde_json::from_slice(&std::fs::read(report_path).unwrap()).unwrap();
    assert_eq!(instance["assets"][0]["static_gain_db"], 0.0);
    assert!(instance["assets"][0]["source"]["integrated_lufs"].is_null());
    let schema: serde_json::Value = serde_json::from_str(include_str!(
        "../schema/normalization-difference-v1.schema.json"
    ))
    .unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let errors = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
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
