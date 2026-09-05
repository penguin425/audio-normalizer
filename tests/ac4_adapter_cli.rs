#[cfg(unix)]
use forge_normalizer::wav::{default_channel_roles, AudioBuffer, PcmKind, WavWriter};
#[cfg(unix)]
use serde_json::Value;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn exposes_the_bounded_adapter_contract() {
    let output = Command::new(env!("CARGO_BIN_EXE_forge-ac4-qc"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    for option in [
        "--adapter",
        "--output",
        "--timeout-seconds",
        "--max-decoded-samples",
        "--dialnorm-tolerance-lu",
        "--max-true-peak-dbtp",
        "--overwrite",
    ] {
        assert!(stdout.contains(option), "missing {option} in:\n{stdout}");
    }
}

#[cfg(unix)]
#[test]
fn invokes_bounded_adapter_and_emits_schema_valid_evidence() {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("fixture.wav");
    let adapter = work.path().join("licensed-adapter");
    let report = work.path().join("report.json");
    let captured_request = work.path().join("captured-request.json");
    let captured_response = work.path().join("captured-response.json");
    let signal: Vec<f32> = (0..48_000)
        .map(|frame| (std::f32::consts::TAU * 997.0 * frame as f32 / 48_000.0).sin() * 0.1)
        .collect();
    let audio = AudioBuffer {
        sample_rate: 48_000,
        channels: 2,
        frames: 48_000,
        data: vec![signal.clone(), signal],
        channel_roles: default_channel_roles(2),
        source_kind: PcmKind::F32,
    };
    WavWriter::write(&input, &audio, PcmKind::F32, false).unwrap();
    let measured = forge_normalizer::analysis::analyze(&audio).lufs;
    let dialnorm_bits = ((-measured * 4.0).round() as i64).clamp(0, 127);
    let dialnorm = -(dialnorm_bits as f64) / 4.0;
    let script = format!(
        r#"#!/bin/sh
set -eu
request=
response=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --request) request=$2; shift 2 ;;
    --response) response=$2; shift 2 ;;
    *) exit 9 ;;
  esac
done
output=$(sed -n 's/^  "output_directory": "\(.*\)",$/\1/p' "$request")
input_sha=$(sed -n 's/^  "input_sha256": "\([0-9a-f]*\)",$/\1/p' "$request")
cp "$request" '{captured_request}'
cp '{input}' "$output/main.wav"
printf '%s\n' '{{"schema":"https://penguin425.github.io/audio-normalizer/schema/ac4-adapter-response-v1","protocol_version":1,"input_sha256":"'"$input_sha"'","decoder":{{"name":"licensed-reference","version":"2026.1"}},"ac4_part1_standard":"ETSI TS 103 190-1 V1.4.1 (2025-07)","ac4_part2_standard":"ETSI TS 103 190-2 V1.3.1 (2025-07)","presentation_count":1,"presentations":[{{"id":"main-en","presentation_version":1,"rendered_path":"main.wav","output_layout":"stereo","language":"en","loudness":{{"dialnorm_bits":{bits},"dialnorm_lkfs":{dialnorm},"dialnorm_source":"presentation-substream","downmix_correction_db":0.0}}}}]}}' > "$response"
cp "$response" '{captured_response}'
"#,
        input = input.display(),
        captured_request = captured_request.display(),
        captured_response = captured_response.display(),
        bits = dialnorm_bits,
    );
    std::fs::write(&adapter, script).unwrap();
    std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o755)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_forge-ac4-qc"))
        .arg(&input)
        .args(["--adapter", adapter.to_str().unwrap()])
        .args(["--output", report.to_str().unwrap()])
        .args(["--dialnorm-tolerance-lu", "0.2"])
        .output()
        .unwrap();
    let instance: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert!(output.status.success(), "{output:#?}\n{instance:#}");
    assert_eq!(
        instance["schema"],
        "https://penguin425.github.io/audio-normalizer/schema/ac4-adapter-report-v2"
    );
    assert_eq!(instance["passed"], true);
    assert_eq!(instance["presentation_count"], 1);
    assert_eq!(instance["decoder"]["name"], "licensed-reference");
    assert_eq!(
        instance["presentations"][0]["channel_layout"]["origin"],
        "renderer"
    );
    assert_eq!(
        instance["presentations"][0]["channel_layout"]["renderer"]["executable_sha256"],
        instance["adapter_sha256"]
    );
    assert_eq!(
        instance["presentations"][0]["channel_layout"]["renderer"]["settings_sha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    assert_eq!(instance["presentations"][0]["dialnorm_passed"], true);
    assert_eq!(
        instance["presentations"][0]["checks"][0]["rule_id"],
        "FORGE-AC4-DIALNORM-MATCH"
    );
    assert_eq!(
        instance["presentations"][0]["loudness_metadata"]["dialnorm_bits"],
        dialnorm_bits
    );
    assert_eq!(instance["input_sha256"].as_str().unwrap().len(), 64);
    assert_eq!(instance["adapter_sha256"].as_str().unwrap().len(), 64);
    assert_eq!(
        instance["presentations"][0]["rendered_sha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );

    validate_schema(
        &serde_json::from_slice(&std::fs::read(captured_request).unwrap()).unwrap(),
        include_str!("../schema/ac4-adapter-request-v1.schema.json"),
    );
    validate_schema(
        &serde_json::from_slice(&std::fs::read(captured_response).unwrap()).unwrap(),
        include_str!("../schema/ac4-adapter-response-v1.schema.json"),
    );
    validate_schema(
        &instance,
        include_str!("../schema/ac4-adapter-report-v2.schema.json"),
    );

    let legacy =
        forge_normalizer::ac4_adapter::run(&forge_normalizer::ac4_adapter::AdapterOptions {
            input,
            adapter,
            timeout_seconds: 300,
            max_decoded_samples_per_presentation: 50_000_000,
            dialnorm_tolerance_lu: 0.2,
            max_true_peak_dbtp: None,
        })
        .unwrap();
    assert_eq!(legacy.schema, forge_normalizer::ac4_adapter::REPORT_SCHEMA);
    let legacy_value = serde_json::to_value(legacy).unwrap();
    validate_schema(
        &legacy_value,
        include_str!("../schema/ac4-adapter-report-v1.schema.json"),
    );
    let mut expected_legacy = instance;
    expected_legacy["schema"] = Value::String(forge_normalizer::ac4_adapter::REPORT_SCHEMA.into());
    for presentation in expected_legacy["presentations"].as_array_mut().unwrap() {
        presentation
            .as_object_mut()
            .unwrap()
            .remove("channel_layout");
    }
    assert_eq!(legacy_value, expected_legacy);
}

#[cfg(unix)]
fn validate_schema(instance: &Value, schema: &str) {
    let schema: Value = serde_json::from_str(schema).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let errors: Vec<_> = validator
        .iter_errors(instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}
