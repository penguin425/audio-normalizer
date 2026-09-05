#[cfg(unix)]
use forge_normalizer::wav::{default_channel_roles, AudioBuffer, PcmKind, WavWriter};
#[cfg(unix)]
use serde_json::Value;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn exposes_native_mhas_and_bounded_adapter_contract() {
    let output = Command::new(env!("CARGO_BIN_EXE_forge-mpegh-qc"))
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
        "--loudness-tolerance-lu",
        "--max-true-peak-dbtp",
        "--overwrite",
    ] {
        assert!(stdout.contains(option), "missing {option} in:\n{stdout}");
    }
}

#[cfg(unix)]
#[test]
fn parses_mhas_invokes_adapter_and_emits_schema_valid_evidence() {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("fixture.mhas");
    let render = work.path().join("reference.wav");
    let adapter = work.path().join("conforming-adapter");
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
    WavWriter::write(&render, &audio, PcmKind::F32, false).unwrap();
    let measured = forge_normalizer::analysis::analyze(&audio).lufs;
    let mut mhas = Vec::new();
    append_packet(&mut mhas, 6, 0, &[0xA5]);
    append_packet(&mut mhas, 1, 1, &[0x0D, 0]);
    append_packet(&mut mhas, 3, 1, &[0]);
    append_packet(&mut mhas, 13, 1, &[0]);
    append_packet(&mut mhas, 22, 1, &[0]);
    append_packet(&mut mhas, 2, 1, &[0]);
    std::fs::write(&input, mhas).unwrap();
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
cp '{render}' "$output/default.wav"
cp '{render}' "$output/main.wav"
printf '%s\n' '{{"schema":"https://penguin425.github.io/audio-normalizer/schema/mpegh-adapter-response-v1","protocol_version":1,"input_sha256":"'"$input_sha"'","decoder":{{"name":"iso-reference","version":"2025"}},"core_standard":"ISO/IEC 23008-3:2026","reference_software_standard":"ISO/IEC 23008-6:2025","conformance_standard":"ISO/IEC 23008-9:2023","mpegh3da_profile_level_indication":13,"scene":{{"group_count":1,"groups":[{{"id":1,"signal_kind":"objects","allow_on_off":true,"default_on":true,"language":"en","content_kind":"dialogue","allow_gain_interactivity":true,"min_gain_db":-12.0,"max_gain_db":6.0,"allow_position_interactivity":false}}],"switch_group_count":0,"switch_groups":[],"preset_count":1,"presets":[{{"id":7,"name":"English","kind":0,"group_ids":[1]}}]}},"presentation_count":2,"presentations":[{{"id":"default","preset_id":null,"rendered_path":"default.wav","output_layout":"stereo","language":"en","accessibility":null,"loudness":{{"loudness_info_type":0,"mae_group_id":null,"mae_group_preset_id":null,"method_definition":1,"program_loudness_lkfs":{measured},"drc_set_id":0,"downmix_id":0,"measurement_system":"ITU-R BS.1770-5"}}}},{{"id":"main-en","preset_id":7,"rendered_path":"main.wav","output_layout":"stereo","language":"en","accessibility":null,"loudness":{{"loudness_info_type":3,"mae_group_id":null,"mae_group_preset_id":7,"method_definition":1,"program_loudness_lkfs":{measured},"drc_set_id":0,"downmix_id":0,"measurement_system":"ITU-R BS.1770-5"}}}}]}}' > "$response"
cp "$response" '{captured_response}'
"#,
        render = render.display(),
        captured_request = captured_request.display(),
        captured_response = captured_response.display(),
    );
    std::fs::write(&adapter, script).unwrap();
    std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o755)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_forge-mpegh-qc"))
        .arg(&input)
        .args(["--adapter", adapter.to_str().unwrap()])
        .args(["--output", report.to_str().unwrap()])
        .args(["--loudness-tolerance-lu", "0.2"])
        .output()
        .unwrap();
    let instance: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert!(output.status.success(), "{output:#?}\n{instance:#}");
    assert_eq!(
        instance["schema"],
        "https://penguin425.github.io/audio-normalizer/schema/mpegh-adapter-report-v2"
    );
    assert_eq!(instance["passed"], true);
    assert_eq!(instance["presentation_count"], 2);
    assert_eq!(instance["profile_level"]["profile"], "low-complexity");
    assert_eq!(instance["profile_level"]["level"], 3);
    assert_eq!(instance["mhas_inventory"]["packet_count"], 6);
    assert_eq!(instance["mhas_inventory"]["audio_scene_info_count"], 1);
    assert_eq!(instance["scene"]["presets"][0]["id"], 7);
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
    assert_eq!(instance["presentations"][0]["loudness_passed"], true);
    assert_eq!(instance["presentations"][1]["loudness_passed"], true);
    assert_eq!(
        instance["presentations"][0]["checks"][0]["rule_id"],
        "FORGE-MPEGH-PROGRAM-LOUDNESS-MATCH"
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
        include_str!("../schema/mpegh-adapter-request-v1.schema.json"),
    );
    validate_schema(
        &serde_json::from_slice(&std::fs::read(captured_response).unwrap()).unwrap(),
        include_str!("../schema/mpegh-adapter-response-v1.schema.json"),
    );
    validate_schema(
        &instance,
        include_str!("../schema/mpegh-adapter-report-v2.schema.json"),
    );

    let legacy =
        forge_normalizer::mpegh_adapter::run(&forge_normalizer::mpegh_adapter::AdapterOptions {
            input,
            adapter,
            timeout_seconds: 300,
            max_decoded_samples_per_presentation: 50_000_000,
            loudness_tolerance_lu: 0.2,
            max_true_peak_dbtp: None,
        })
        .unwrap();
    assert_eq!(
        legacy.schema,
        forge_normalizer::mpegh_adapter::REPORT_SCHEMA
    );
    let legacy_value = serde_json::to_value(legacy).unwrap();
    validate_schema(
        &legacy_value,
        include_str!("../schema/mpegh-adapter-report-v1.schema.json"),
    );
    let mut expected_legacy = instance;
    expected_legacy["schema"] =
        Value::String(forge_normalizer::mpegh_adapter::REPORT_SCHEMA.into());
    for presentation in expected_legacy["presentations"].as_array_mut().unwrap() {
        presentation
            .as_object_mut()
            .unwrap()
            .remove("channel_layout");
    }
    assert_eq!(legacy_value, expected_legacy);
}

#[cfg(unix)]
fn append_packet(output: &mut Vec<u8>, packet_type: u64, label: u64, payload: &[u8]) {
    let mut bits = Vec::new();
    write_escaped(&mut bits, packet_type, 3, 8, 8);
    write_escaped(&mut bits, label, 2, 8, 32);
    write_escaped(&mut bits, payload.len() as u64, 11, 24, 24);
    assert_eq!(bits.len() % 8, 0);
    for chunk in bits.chunks(8) {
        output.push(chunk.iter().fold(0, |byte, bit| (byte << 1) | bit));
    }
    output.extend_from_slice(payload);
}

#[cfg(unix)]
fn write_escaped(bits: &mut Vec<u8>, value: u64, n1: u8, n2: u8, n3: u8) {
    let max1 = (1_u64 << n1) - 1;
    let first = value.min(max1);
    write_bits(bits, first, n1);
    if first == max1 {
        let rest = value - max1;
        let max2 = (1_u64 << n2) - 1;
        let second = rest.min(max2);
        write_bits(bits, second, n2);
        if second == max2 {
            write_bits(bits, rest - max2, n3);
        }
    }
}

#[cfg(unix)]
fn write_bits(bits: &mut Vec<u8>, value: u64, count: u8) {
    for shift in (0..count).rev() {
        bits.push(((value >> shift) & 1) as u8);
    }
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
