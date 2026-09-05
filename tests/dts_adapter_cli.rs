#[cfg(unix)]
use forge_normalizer::wav::{default_channel_roles, AudioBuffer, PcmKind, WavWriter};
#[cfg(unix)]
use serde_json::Value;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn exposes_native_and_bounded_adapter_options() {
    let output = Command::new(env!("CARGO_BIN_EXE_forge-dts-qc"))
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
        "--max-true-peak-dbtp",
        "--overwrite",
    ] {
        assert!(stdout.contains(option), "missing {option} in:\n{stdout}");
    }
}

#[cfg(unix)]
#[test]
fn validates_core_and_emits_schema_valid_decoded_evidence() {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("fixture.dts");
    let adapter = work.path().join("reference-adapter");
    let report = work.path().join("report.json");
    let captured_request = work.path().join("request.json");
    let captured_response = work.path().join("response.json");
    std::fs::write(&input, core_frame()).unwrap();

    let signal: Vec<f32> = (0..48_000)
        .map(|frame| (std::f32::consts::TAU * 997.0 * frame as f32 / 48_000.0).sin() * 0.1)
        .collect();
    let rendered = work.path().join("render.wav");
    let audio = AudioBuffer {
        sample_rate: 48_000,
        channels: 2,
        frames: 48_000,
        data: vec![signal.clone(), signal],
        channel_roles: default_channel_roles(2),
        source_kind: PcmKind::F32,
    };
    WavWriter::write(&rendered, &audio, PcmKind::F32, false).unwrap();

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
cp '{rendered}' "$output/main.wav"
printf '%s\n' '{{"schema":"https://penguin425.github.io/audio-normalizer/schema/dts-adapter-response-v1","protocol_version":1,"input_sha256":"'"$input_sha"'","decoder":{{"name":"licensed-reference","version":"2026.1"}},"standard":"ETSI TS 102 114 V1.6.1 (2019-08)","profile":"core","dialog_normalization_policy":"disabled","dynamic_range_control_policy":"disabled","asset_count":1,"assets":[{{"id":"core","extension_substream_index":null,"asset_index":0,"language":"en","channels":2,"maximum_sample_rate_hz":48000,"pcm_resolution_bits":16,"coding_components":["core"],"dialog_normalization_db":-4.0}}],"presentation_count":1,"presentations":[{{"id":"main-en","asset_ids":["core"],"rendered_path":"main.wav","output_layout":"stereo","declared_sample_rate_hz":48000,"declared_channels":2,"language":"en","accessibility":null}}]}}' > "$response"
cp "$response" '{captured_response}'
"#,
        captured_request = captured_request.display(),
        captured_response = captured_response.display(),
        rendered = rendered.display(),
    );
    std::fs::write(&adapter, script).unwrap();
    std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o755)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_forge-dts-qc"))
        .arg(&input)
        .args(["--adapter", adapter.to_str().unwrap()])
        .args(["--output", report.to_str().unwrap()])
        .output()
        .unwrap();
    let instance: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert!(output.status.success(), "{output:#?}\n{instance:#}");
    assert_eq!(
        instance["schema"],
        "https://penguin425.github.io/audio-normalizer/schema/dts-adapter-report-v2"
    );
    assert_eq!(instance["passed"], true);
    assert_eq!(instance["profile"], "core");
    assert_eq!(instance["native_inventory"]["core_frame_count"], 1);
    assert_eq!(instance["native_inventory"]["frame_count"], 1);
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
    assert_eq!(instance["presentations"][0]["sample_rate_passed"], true);
    assert_eq!(instance["presentations"][0]["channels_passed"], true);
    assert_eq!(
        instance["presentations"][0]["checks"][0]["rule_id"],
        "FORGE-DTS-RENDER-SAMPLE-RATE"
    );
    for key in ["input_sha256", "adapter_sha256"] {
        assert_eq!(instance[key].as_str().unwrap().len(), 64);
    }
    assert_eq!(
        instance["presentations"][0]["rendered_sha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );

    validate_schema(
        &serde_json::from_slice(&std::fs::read(captured_request).unwrap()).unwrap(),
        include_str!("../schema/dts-adapter-request-v1.schema.json"),
    );
    validate_schema(
        &serde_json::from_slice(&std::fs::read(captured_response).unwrap()).unwrap(),
        include_str!("../schema/dts-adapter-response-v1.schema.json"),
    );
    validate_schema(
        &instance,
        include_str!("../schema/dts-adapter-report-v2.schema.json"),
    );

    let legacy =
        forge_normalizer::dts_adapter::run(&forge_normalizer::dts_adapter::AdapterOptions {
            input,
            adapter,
            timeout_seconds: 300,
            max_decoded_samples_per_presentation: 50_000_000,
            max_true_peak_dbtp: None,
        })
        .unwrap();
    assert_eq!(legacy.schema, forge_normalizer::dts_adapter::REPORT_SCHEMA);
    let legacy_value = serde_json::to_value(legacy).unwrap();
    validate_schema(
        &legacy_value,
        include_str!("../schema/dts-adapter-report-v1.schema.json"),
    );
    let mut expected_legacy = instance;
    expected_legacy["schema"] = Value::String(forge_normalizer::dts_adapter::REPORT_SCHEMA.into());
    for presentation in expected_legacy["presentations"].as_array_mut().unwrap() {
        presentation
            .as_object_mut()
            .unwrap()
            .remove("channel_layout");
    }
    assert_eq!(legacy_value, expected_legacy);
}

#[cfg(unix)]
#[test]
fn rejects_truncated_native_frame_before_adapter_runs() {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("truncated.dts");
    let adapter = work.path().join("must-not-run");
    let report = work.path().join("report.json");
    let mut frame = core_frame();
    frame.truncate(95);
    std::fs::write(&input, frame).unwrap();
    std::fs::write(&adapter, "#!/bin/sh\nexit 99\n").unwrap();
    std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-dts-qc"))
        .arg(&input)
        .args(["--adapter", adapter.to_str().unwrap()])
        .args(["--output", report.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("declares 96 bytes"));
    assert!(!report.exists());
}

fn core_frame() -> Vec<u8> {
    let mut writer = Bits::default();
    writer.push(0x7FFE_8001, 32);
    writer.push(1, 1); // normal frame
    writer.push(31, 5); // 32 deficit samples
    writer.push(0, 1); // no CRC
    writer.push(7, 7); // 8 PCM blocks
    writer.push(95, 14); // 96-byte frame
    writer.push(2, 6); // stereo
    writer.push(13, 4); // 48 kHz
    writer.push(24, 5); // 1536 kbit/s
    writer.push(0, 1); // reserved
    writer.push(0, 4); // DRC/timestamp/auxiliary/HDCD
    writer.push(0, 3); // extension type
    writer.push(0, 1); // no extension
    writer.push(0, 1); // sync insertion
    writer.push(0, 2); // no LFE
    writer.push(0, 1); // predictor history
    writer.push(0, 1); // multirate interpolator
    writer.push(6, 4); // encoder revision, dialnorm defined
    writer.push(0, 2); // copy history
    writer.push(0, 3); // 16-bit PCM
    writer.push(0, 2); // sum/difference flags
    writer.push(4, 4); // dialog normalization metadata
    let mut bytes = writer.finish();
    bytes.resize(96, 0);
    bytes
}

#[derive(Default)]
struct Bits {
    bytes: Vec<u8>,
    current: u8,
    used: u8,
}

impl Bits {
    fn push(&mut self, value: u64, count: u8) {
        for shift in (0..count).rev() {
            self.current = (self.current << 1) | ((value >> shift) as u8 & 1);
            self.used += 1;
            if self.used == 8 {
                self.bytes.push(self.current);
                self.current = 0;
                self.used = 0;
            }
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.used != 0 {
            self.current <<= 8 - self.used;
            self.bytes.push(self.current);
        }
        self.bytes
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
