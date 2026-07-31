use forge_normalizer::multi_delivery::{load_request, REPORT_SCHEMA, REQUEST_SCHEMA};
use forge_normalizer::wav::{default_channel_roles, AudioBuffer, PcmKind, WavWriter};
use serde_json::{json, Value};
use std::f64::consts::TAU;
use std::fs;
use std::process::Command;

fn write_stereo_tone(path: &std::path::Path) {
    let sample_rate = 48_000;
    let frames = sample_rate as usize * 2;
    let left = (0..frames)
        .map(|frame| (0.12 * (TAU * 997.0 * frame as f64 / sample_rate as f64).sin()) as f32)
        .collect::<Vec<_>>();
    let right = (0..frames)
        .map(|frame| (0.10 * (TAU * 613.0 * frame as f64 / sample_rate as f64).sin()) as f32)
        .collect::<Vec<_>>();
    WavWriter::write(
        path,
        &AudioBuffer {
            sample_rate,
            channels: 2,
            frames,
            data: vec![left, right],
            channel_roles: default_channel_roles(2),
            source_kind: PcmKind::F32,
        },
        PcmKind::F32,
        false,
    )
    .unwrap();
}

fn request() -> Value {
    json!({
        "schema": REQUEST_SCHEMA,
        "verify_tolerance_lu_db": 0.1,
        "verify_retries": 2,
        "deliveries": [
            {
                "id": "streaming",
                "output": "outputs/streaming.wav",
                "format": "wav",
                "preset": "spotify"
            },
            {
                "id": "broadcast",
                "output": "outputs/broadcast.flac",
                "format": "flac",
                "preset": "ebu-r128"
            }
        ]
    })
}

fn run(directory: &std::path::Path, overwrite: bool) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_forge-multi-delivery"));
    command
        .arg(directory.join("source.wav"))
        .arg("--request")
        .arg(directory.join("request.json"))
        .arg("--report")
        .arg(directory.join("report.json"));
    if overwrite {
        command.arg("--overwrite");
    }
    command.output().unwrap()
}

#[test]
fn renders_and_verifies_two_profiles_with_one_shared_gain() {
    let directory = tempfile::tempdir().unwrap();
    write_stereo_tone(&directory.path().join("source.wav"));
    let request_value = request();
    fs::write(
        directory.path().join("request.json"),
        serde_json::to_vec_pretty(&request_value).unwrap(),
    )
    .unwrap();

    let result = run(directory.path(), false);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(directory.path().join("outputs/streaming.wav").is_file());
    assert!(directory.path().join("outputs/broadcast.flac").is_file());

    let report: Value =
        serde_json::from_slice(&fs::read(directory.path().join("report.json")).unwrap()).unwrap();
    assert_eq!(report["schema"], REPORT_SCHEMA);
    assert_eq!(report["method"]["id"], "forge-multi-delivery-v1");
    assert_eq!(report["common"]["target_lufs"], -23.0);
    assert_eq!(report["common"]["ceiling_dbtp"], -1.0);
    assert_eq!(report["deliveries"].as_array().unwrap().len(), 2);
    assert_eq!(report["passed"], true);
    for delivery in report["deliveries"].as_array().unwrap() {
        assert_eq!(delivery["level_passed"], true);
        assert_eq!(delivery["true_peak_passed"], true);
        assert_eq!(delivery["conservative_profile_bounds_passed"], true);
        assert_eq!(delivery["passed"], true);
    }
    let streaming_headroom = report["deliveries"][0]["profile_loudness_headroom_lu"]
        .as_f64()
        .unwrap();
    assert!((streaming_headroom - 9.0).abs() <= 0.1);

    let request_schema: Value = serde_json::from_str(include_str!(
        "../schema/multi-delivery-request-v1.schema.json"
    ))
    .unwrap();
    assert!(jsonschema::validator_for(&request_schema)
        .unwrap()
        .is_valid(&request_value));
    let report_schema: Value = serde_json::from_str(include_str!(
        "../schema/multi-delivery-report-v1.schema.json"
    ))
    .unwrap();
    let validator = jsonschema::validator_for(&report_schema).unwrap();
    let errors = validator
        .iter_errors(&report)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");

    let refused = run(directory.path(), false);
    assert_eq!(refused.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("already exists"));

    let replaced = run(directory.path(), true);
    assert!(
        replaced.status.success(),
        "{}",
        String::from_utf8_lossy(&replaced.stderr)
    );
}

#[test]
fn rejects_lexically_aliased_output_paths_before_writing() {
    let directory = tempfile::tempdir().unwrap();
    write_stereo_tone(&directory.path().join("source.wav"));
    let mut request_value = request();
    request_value["deliveries"][1]["output"] = json!("outputs/../outputs/streaming.wav");
    request_value["deliveries"][1]["format"] = json!("wav");
    fs::write(
        directory.path().join("request.json"),
        serde_json::to_vec_pretty(&request_value).unwrap(),
    )
    .unwrap();

    let result = run(directory.path(), false);

    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("path collision"));
    assert!(!directory.path().join("outputs/streaming.wav").exists());
    assert!(!directory.path().join("report.json").exists());
}

#[test]
fn runtime_rejects_a_mismatched_extension() {
    let directory = tempfile::tempdir().unwrap();
    write_stereo_tone(&directory.path().join("source.wav"));
    let mut request_value = request();
    request_value["deliveries"][0]["output"] = json!("outputs/not-a-wave.flac");
    fs::write(
        directory.path().join("request.json"),
        serde_json::to_vec_pretty(&request_value).unwrap(),
    )
    .unwrap();

    let result = run(directory.path(), false);

    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("extension does not match"));
    assert!(!directory.path().join("outputs/not-a-wave.flac").exists());
}

#[cfg(unix)]
#[test]
fn rejects_outputs_that_alias_through_a_symlinked_parent() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap();
    write_stereo_tone(&directory.path().join("source.wav"));
    fs::create_dir(directory.path().join("real-outputs")).unwrap();
    symlink("real-outputs", directory.path().join("linked-outputs")).unwrap();
    let mut request_value = request();
    request_value["deliveries"][0]["output"] = json!("real-outputs/shared.wav");
    request_value["deliveries"][1]["output"] = json!("linked-outputs/shared.wav");
    request_value["deliveries"][1]["format"] = json!("wav");
    fs::write(
        directory.path().join("request.json"),
        serde_json::to_vec_pretty(&request_value).unwrap(),
    )
    .unwrap();

    let result = run(directory.path(), false);

    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("path collision"));
    assert!(!directory.path().join("real-outputs/shared.wav").exists());
}

#[test]
fn accepts_toml_requests_with_bounded_defaults() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("request.toml");
    fs::write(
        &path,
        format!(
            r#"schema = "{REQUEST_SCHEMA}"

[[deliveries]]
id = "wave"
output = "wave.wav"
format = "wav"
preset = "spotify"

[[deliveries]]
id = "lossless"
output = "lossless.flac"
format = "flac"
preset = "ebu-r128"
"#
        ),
    )
    .unwrap();

    let parsed = load_request(&path).unwrap();

    assert_eq!(parsed.deliveries.len(), 2);
    assert_eq!(parsed.verify_tolerance_lu_db, 0.5);
    assert_eq!(parsed.verify_retries, 2);
    assert_eq!(parsed.mp3_bitrate_kbps, 320);
}

#[test]
fn report_cannot_replace_the_request_even_with_overwrite() {
    let directory = tempfile::tempdir().unwrap();
    write_stereo_tone(&directory.path().join("source.wav"));
    let request_path = directory.path().join("request.json");
    fs::write(
        &request_path,
        serde_json::to_vec_pretty(&request()).unwrap(),
    )
    .unwrap();

    let result = Command::new(env!("CARGO_BIN_EXE_forge-multi-delivery"))
        .arg(directory.path().join("source.wav"))
        .arg("--request")
        .arg(&request_path)
        .arg("--report")
        .arg(&request_path)
        .arg("--overwrite")
        .output()
        .unwrap();

    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("aliases its request"));
    let preserved: Value = serde_json::from_slice(&fs::read(request_path).unwrap()).unwrap();
    assert_eq!(preserved["schema"], REQUEST_SCHEMA);
}

#[cfg(not(feature = "opus-encoding"))]
#[test]
fn unavailable_later_codec_preserves_every_destination() {
    let directory = tempfile::tempdir().unwrap();
    write_stereo_tone(&directory.path().join("source.wav"));
    fs::create_dir(directory.path().join("outputs")).unwrap();
    let preserved_path = directory.path().join("outputs/first.wav");
    fs::write(&preserved_path, b"preserve this destination").unwrap();
    let request_value = json!({
        "schema": REQUEST_SCHEMA,
        "deliveries": [
            {"id": "first", "output": "outputs/first.wav", "format": "wav", "preset": "spotify"},
            {"id": "second", "output": "outputs/second.opus", "format": "opus", "preset": "ebu-r128"}
        ]
    });
    fs::write(
        directory.path().join("request.json"),
        serde_json::to_vec_pretty(&request_value).unwrap(),
    )
    .unwrap();

    let result = run(directory.path(), true);

    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("Opus output is unavailable"));
    assert_eq!(
        fs::read(preserved_path).unwrap(),
        b"preserve this destination"
    );
    assert!(!directory.path().join("outputs/second.opus").exists());
    assert!(!directory.path().join("report.json").exists());
}

#[cfg(feature = "opus-encoding")]
#[test]
fn redecodes_and_verifies_lossy_opus_with_the_shared_gain() {
    let directory = tempfile::tempdir().unwrap();
    write_stereo_tone(&directory.path().join("source.wav"));
    let request_value = json!({
        "schema": REQUEST_SCHEMA,
        "verify_tolerance_lu_db": 0.5,
        "verify_retries": 2,
        "deliveries": [
            {"id": "lossless", "output": "outputs/lossless.wav", "format": "wav", "preset": "spotify"},
            {"id": "lossy", "output": "outputs/lossy.opus", "format": "opus", "preset": "ebu-r128"}
        ]
    });
    fs::write(
        directory.path().join("request.json"),
        serde_json::to_vec_pretty(&request_value).unwrap(),
    )
    .unwrap();

    let result = run(directory.path(), false);

    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: Value =
        serde_json::from_slice(&fs::read(directory.path().join("report.json")).unwrap()).unwrap();
    assert_eq!(report["passed"], true);
    assert_eq!(report["deliveries"][1]["format"], "opus");
    assert_eq!(report["deliveries"][1]["passed"], true);
    assert!(report["common"]["encoding_passes"].as_u64().unwrap() >= 1);
}
