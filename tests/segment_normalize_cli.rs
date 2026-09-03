use forge_normalizer::segment_normalize::{PLAN_SCHEMA, REPORT_SCHEMA, REQUEST_SCHEMA};
use forge_normalizer::wav::{default_channel_roles, AudioBuffer, PcmKind, WavReader, WavWriter};
use serde_json::{json, Value};
use std::f64::consts::TAU;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn write_tone(path: &Path, amplitude: f32, sample_rate: u32) {
    let frames = sample_rate as usize * 2;
    let phase = 0.3;
    let signal = (0..frames)
        .map(|frame| {
            (amplitude as f64 * (TAU * 1_000.0 * frame as f64 / sample_rate as f64 + phase).sin())
                as f32
        })
        .collect::<Vec<_>>();
    WavWriter::write(
        path,
        &AudioBuffer {
            sample_rate,
            channels: 1,
            frames,
            data: vec![signal],
            channel_roles: default_channel_roles(1),
            source_kind: PcmKind::F32,
        },
        PcmKind::F32,
        false,
    )
    .unwrap();
}

fn write_maskless_51_tone(path: &Path, amplitude: f32, sample_rate: u32) {
    let channels = 6_u16;
    let frames = sample_rate as usize * 2;
    let block_align = channels * 2;
    let data_size = u32::from(block_align) * u32::try_from(frames).unwrap();
    let mut bytes = Vec::with_capacity(44 + data_size as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36_u32 + data_size).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&channels.to_le_bytes());
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.extend_from_slice(&(sample_rate * u32::from(block_align)).to_le_bytes());
    bytes.extend_from_slice(&block_align.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_size.to_le_bytes());
    for frame in 0..frames {
        let sample =
            amplitude as f64 * (TAU * 1_000.0 * frame as f64 / sample_rate as f64 + 0.3).sin();
        let quantized = (sample * f64::from(i16::MAX)).round() as i16;
        for _ in 0..channels {
            bytes.extend_from_slice(&quantized.to_le_bytes());
        }
    }
    fs::write(path, bytes).unwrap();
}

fn request() -> Value {
    json!({
        "schema": REQUEST_SCHEMA,
        "target_lufs": -18.0,
        "ceiling_dbtp": -1.0,
        "max_gain_db": 24.0,
        "smoothing_ms": 250.0,
        "verification_tolerance_lu_db": 0.1,
        "duration_tolerance_ms": 1.0,
        "boundary_review_threshold_db": 6.0,
        "max_decoded_samples_per_segment": 250_000,
        "format": "wav",
        "segments": [
            {"id": "quiet", "input": "inputs/quiet.wav", "output": "outputs/quiet.wav"},
            {"id": "loud", "input": "inputs/loud.wav", "output": "outputs/loud.wav"}
        ]
    })
}

fn prepare(directory: &Path) -> Value {
    fs::create_dir(directory.join("inputs")).unwrap();
    write_tone(&directory.join("inputs/quiet.wav"), 0.04, 48_000);
    write_tone(&directory.join("inputs/loud.wav"), 0.16, 48_000);
    let request = request();
    fs::write(
        directory.join("request.json"),
        serde_json::to_vec_pretty(&request).unwrap(),
    )
    .unwrap();
    request
}

fn plan(directory: &Path, overwrite: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_forge-segment-normalize"));
    command
        .arg("plan")
        .arg("--request")
        .arg(directory.join("request.json"))
        .arg("--manifest")
        .arg(directory.join("plan.json"));
    if overwrite {
        command.arg("--overwrite");
    }
    command.output().unwrap()
}

fn plan_with_layout(directory: &Path, layout: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_forge-segment-normalize"))
        .arg("plan")
        .arg("--request")
        .arg(directory.join("request.json"))
        .arg("--manifest")
        .arg(directory.join("plan.json"))
        .arg("--channel-layout")
        .arg(layout)
        .output()
        .unwrap()
}

fn render(directory: &Path, overwrite: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_forge-segment-normalize"));
    command
        .arg("render")
        .arg("--manifest")
        .arg(directory.join("plan.json"))
        .arg("--report")
        .arg(directory.join("report.json"));
    if overwrite {
        command.arg("--overwrite");
    }
    command.output().unwrap()
}

fn assert_schema(instance: &Value, schema: &str) {
    let schema: Value = serde_json::from_str(schema).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let errors = validator
        .iter_errors(instance)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

#[test]
fn two_pass_wav_render_has_a_shared_boundary_and_schema_valid_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let request = prepare(directory.path());
    assert_schema(
        &request,
        include_str!("../schema/segment-normalization-request-v1.schema.json"),
    );

    let planned = plan(directory.path(), false);
    assert!(
        planned.status.success(),
        "{}",
        String::from_utf8_lossy(&planned.stderr)
    );
    let plan_value: Value =
        serde_json::from_slice(&fs::read(directory.path().join("plan.json")).unwrap()).unwrap();
    assert_eq!(plan_value["schema"], PLAN_SCHEMA);
    assert_eq!(
        plan_value["generator"],
        concat!("forge-normalizer/", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(plan_value["method"]["id"], "forge-segment-normalization-v2");
    assert_eq!(
        plan_value["segments"][0]["end_gain_db"],
        plan_value["segments"][1]["start_gain_db"]
    );
    assert_eq!(plan_value["manual_review_recommended"], true);
    assert_schema(
        &plan_value,
        include_str!("../schema/segment-normalization-plan-v2.schema.json"),
    );

    let rendered = render(directory.path(), false);
    assert!(
        rendered.status.success(),
        "{}",
        String::from_utf8_lossy(&rendered.stderr)
    );
    let report: Value =
        serde_json::from_slice(&fs::read(directory.path().join("report.json")).unwrap()).unwrap();
    assert_eq!(report["schema"], REPORT_SCHEMA);
    assert_eq!(report["passed"], true);
    assert_eq!(report["published_segments"], 2);
    assert!(report["segments"]
        .as_array()
        .unwrap()
        .iter()
        .all(|segment| segment["passed"] == true && segment["published"] == true));
    assert_schema(
        &report,
        include_str!("../schema/segment-normalization-report-v2.schema.json"),
    );

    let quiet_input = WavReader::open(directory.path().join("inputs/quiet.wav")).unwrap();
    let loud_input = WavReader::open(directory.path().join("inputs/loud.wav")).unwrap();
    let quiet_output = WavReader::open(directory.path().join("outputs/quiet.wav")).unwrap();
    let loud_output = WavReader::open(directory.path().join("outputs/loud.wav")).unwrap();
    let quiet_ratio =
        quiet_output.data[0][quiet_output.frames - 1] / quiet_input.data[0][quiet_input.frames - 1];
    let loud_ratio = loud_output.data[0][0] / loud_input.data[0][0];
    assert!((quiet_ratio - loud_ratio).abs() < 1.0e-5);

    let refused = render(directory.path(), false);
    assert_eq!(refused.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("already exists"));
    let replaced = render(directory.path(), true);
    assert!(
        replaced.status.success(),
        "{}",
        String::from_utf8_lossy(&replaced.stderr)
    );
}

#[test]
fn maskless_five_one_plan_requires_an_override_and_remains_renderable() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("inputs")).unwrap();
    write_maskless_51_tone(&directory.path().join("inputs/quiet.wav"), 0.04, 48_000);
    write_maskless_51_tone(&directory.path().join("inputs/loud.wav"), 0.16, 48_000);
    let mut request = request();
    request["max_decoded_samples_per_segment"] = json!(1_000_000);
    fs::write(
        directory.path().join("request.json"),
        serde_json::to_vec_pretty(&request).unwrap(),
    )
    .unwrap();

    let rejected = plan(directory.path(), false);
    assert_eq!(rejected.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("ambiguous 6-channel layout"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert!(!directory.path().join("plan.json").exists());

    let planned = plan_with_layout(directory.path(), "5.1");
    assert!(
        planned.status.success(),
        "{}",
        String::from_utf8_lossy(&planned.stderr)
    );
    let rendered = render(directory.path(), false);
    assert!(
        rendered.status.success(),
        "{}",
        String::from_utf8_lossy(&rendered.stderr)
    );
    let report: Value =
        serde_json::from_slice(&fs::read(directory.path().join("report.json")).unwrap()).unwrap();
    assert_eq!(report["passed"], true);
    assert_eq!(report["published_segments"], 2);
}

#[test]
fn pre_layout_provenance_plan_is_rejected_before_publication() {
    let directory = tempfile::tempdir().unwrap();
    prepare(directory.path());
    assert!(plan(directory.path(), false).status.success());

    let plan_path = directory.path().join("plan.json");
    let mut value: Value = serde_json::from_slice(&fs::read(&plan_path).unwrap()).unwrap();
    value["schema"] =
        json!("https://penguin425.github.io/audio-normalizer/schema/segment-normalization-plan-v1");
    value["method"]["id"] = json!("forge-segment-normalization-v1");
    value["method"]["algorithm_revision"] = json!("smoothstep-db-boundary-v1");
    fs::write(&plan_path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

    fs::create_dir(directory.path().join("outputs")).unwrap();
    let quiet_output = directory.path().join("outputs/quiet.wav");
    let loud_output = directory.path().join("outputs/loud.wav");
    fs::write(&quiet_output, b"preserve quiet").unwrap();
    fs::write(&loud_output, b"preserve loud").unwrap();

    let result = render(directory.path(), true);

    assert_eq!(result.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&result.stderr)
            .contains("unsupported segment normalization plan method or schema"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(fs::read(quiet_output).unwrap(), b"preserve quiet");
    assert_eq!(fs::read(loud_output).unwrap(), b"preserve loud");
    assert!(!directory.path().join("report.json").exists());
}

#[test]
fn changed_later_input_is_rejected_before_any_destination_is_replaced() {
    let directory = tempfile::tempdir().unwrap();
    prepare(directory.path());
    assert!(plan(directory.path(), false).status.success());
    fs::create_dir(directory.path().join("outputs")).unwrap();
    let quiet_output = directory.path().join("outputs/quiet.wav");
    let loud_output = directory.path().join("outputs/loud.wav");
    fs::write(&quiet_output, b"preserve quiet").unwrap();
    fs::write(&loud_output, b"preserve loud").unwrap();
    write_tone(&directory.path().join("inputs/loud.wav"), 0.12, 48_000);

    let result = render(directory.path(), true);

    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("does not match the plan binding"));
    assert_eq!(fs::read(quiet_output).unwrap(), b"preserve quiet");
    assert_eq!(fs::read(loud_output).unwrap(), b"preserve loud");
    assert!(!directory.path().join("report.json").exists());
}

#[test]
fn tampered_boundary_plan_is_rejected_before_rendering() {
    let directory = tempfile::tempdir().unwrap();
    prepare(directory.path());
    assert!(plan(directory.path(), false).status.success());
    let plan_path = directory.path().join("plan.json");
    let mut value: Value = serde_json::from_slice(&fs::read(&plan_path).unwrap()).unwrap();
    value["segments"][0]["end_gain_db"] = json!(0.0);
    fs::write(&plan_path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

    let result = render(directory.path(), false);

    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("boundary 0"));
    assert!(!directory.path().join("outputs/quiet.wav").exists());
    assert!(!directory.path().join("outputs/loud.wav").exists());
}

#[test]
fn tampered_output_mapping_is_rejected_against_the_bound_request() {
    let directory = tempfile::tempdir().unwrap();
    prepare(directory.path());
    assert!(plan(directory.path(), false).status.success());
    let plan_path = directory.path().join("plan.json");
    let redirected = directory.path().join("redirected.wav");
    let mut value: Value = serde_json::from_slice(&fs::read(&plan_path).unwrap()).unwrap();
    value["segments"][0]["output_path"] = json!(redirected.to_string_lossy());
    fs::write(&plan_path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

    let result = render(directory.path(), false);

    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("bound request"));
    assert!(!redirected.exists());
    assert!(!directory.path().join("outputs/quiet.wav").exists());
    assert!(!directory.path().join("report.json").exists());
}

#[test]
fn planning_rejects_lexical_output_aliases_and_layout_mismatches() {
    let directory = tempfile::tempdir().unwrap();
    let mut alias_request = prepare(directory.path());
    alias_request["segments"][1]["output"] = json!("outputs/../outputs/quiet.wav");
    fs::write(
        directory.path().join("request.json"),
        serde_json::to_vec_pretty(&alias_request).unwrap(),
    )
    .unwrap();
    let alias = plan(directory.path(), false);
    assert_eq!(alias.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&alias.stderr).contains("path collision"));

    let mut request = request();
    request["segments"][1]["output"] = json!("outputs/loud.wav");
    fs::write(
        directory.path().join("request.json"),
        serde_json::to_vec_pretty(&request).unwrap(),
    )
    .unwrap();
    write_tone(&directory.path().join("inputs/loud.wav"), 0.16, 44_100);
    let layout = plan(directory.path(), false);
    assert_eq!(layout.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&layout.stderr).contains("layout differs"));
    assert!(!directory.path().join("plan.json").exists());
}

#[test]
fn decoded_sample_limit_is_enforced_in_pass_one() {
    let directory = tempfile::tempdir().unwrap();
    let mut request = prepare(directory.path());
    request["max_decoded_samples_per_segment"] = json!(10_000);
    fs::write(
        directory.path().join("request.json"),
        serde_json::to_vec_pretty(&request).unwrap(),
    )
    .unwrap();

    let result = plan(directory.path(), false);

    assert_eq!(result.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("decoded sample count"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!directory.path().join("plan.json").exists());
}

#[test]
fn flac_outputs_are_redecoded_and_verified() {
    let directory = tempfile::tempdir().unwrap();
    let mut request = prepare(directory.path());
    request["format"] = json!("flac");
    request["segments"][0]["output"] = json!("outputs/quiet.flac");
    request["segments"][1]["output"] = json!("outputs/loud.flac");
    fs::write(
        directory.path().join("request.json"),
        serde_json::to_vec_pretty(&request).unwrap(),
    )
    .unwrap();

    assert!(plan(directory.path(), false).status.success());
    let result = render(directory.path(), false);

    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(directory.path().join("outputs/quiet.flac").is_file());
    assert!(directory.path().join("outputs/loud.flac").is_file());
    let report: Value =
        serde_json::from_slice(&fs::read(directory.path().join("report.json")).unwrap()).unwrap();
    assert_eq!(report["passed"], true);
}

#[test]
fn accepts_a_toml_request_with_bounded_defaults() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("inputs")).unwrap();
    write_tone(&directory.path().join("inputs/quiet.wav"), 0.04, 48_000);
    write_tone(&directory.path().join("inputs/loud.wav"), 0.16, 48_000);
    let request_path = directory.path().join("request.toml");
    fs::write(
        &request_path,
        format!(
            r#"schema = "{REQUEST_SCHEMA}"
format = "wav"

[[segments]]
id = "quiet"
input = "inputs/quiet.wav"
output = "outputs/quiet.wav"

[[segments]]
id = "loud"
input = "inputs/loud.wav"
output = "outputs/loud.wav"
"#
        ),
    )
    .unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_forge-segment-normalize"))
        .arg("plan")
        .arg("--request")
        .arg(&request_path)
        .arg("--manifest")
        .arg(directory.path().join("plan.json"))
        .output()
        .unwrap();

    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let plan: Value =
        serde_json::from_slice(&fs::read(directory.path().join("plan.json")).unwrap()).unwrap();
    assert_eq!(plan["settings"]["smoothing_ms"], 500.0);
    assert_eq!(
        plan["settings"]["max_decoded_samples_per_segment"],
        50_000_000
    );
}

#[test]
fn plan_and_report_cannot_replace_the_request() {
    let directory = tempfile::tempdir().unwrap();
    prepare(directory.path());
    let request_path = directory.path().join("request.json");
    let original = fs::read(&request_path).unwrap();
    let plan_alias = Command::new(env!("CARGO_BIN_EXE_forge-segment-normalize"))
        .arg("plan")
        .arg("--request")
        .arg(&request_path)
        .arg("--manifest")
        .arg(&request_path)
        .arg("--overwrite")
        .output()
        .unwrap();
    assert_eq!(plan_alias.status.code(), Some(2));
    assert_eq!(fs::read(&request_path).unwrap(), original);

    assert!(plan(directory.path(), false).status.success());
    let report_alias = Command::new(env!("CARGO_BIN_EXE_forge-segment-normalize"))
        .arg("render")
        .arg("--manifest")
        .arg(directory.path().join("plan.json"))
        .arg("--report")
        .arg(&request_path)
        .arg("--overwrite")
        .output()
        .unwrap();
    assert_eq!(report_alias.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&report_alias.stderr).contains("request file"));
    assert_eq!(fs::read(&request_path).unwrap(), original);
    assert!(!directory.path().join("outputs/quiet.wav").exists());
}

#[cfg(unix)]
#[test]
fn rejects_outputs_that_alias_through_a_symlinked_parent() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap();
    let mut request = prepare(directory.path());
    fs::create_dir(directory.path().join("real-outputs")).unwrap();
    symlink("real-outputs", directory.path().join("linked-outputs")).unwrap();
    request["segments"][0]["output"] = json!("real-outputs/shared.wav");
    request["segments"][1]["output"] = json!("linked-outputs/shared.wav");
    fs::write(
        directory.path().join("request.json"),
        serde_json::to_vec_pretty(&request).unwrap(),
    )
    .unwrap();

    let result = plan(directory.path(), false);

    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("path collision"));
    assert!(!directory.path().join("plan.json").exists());
}

#[cfg(not(feature = "opus-encoding"))]
#[test]
fn unavailable_encoder_preserves_all_existing_destinations() {
    let directory = tempfile::tempdir().unwrap();
    let mut request = prepare(directory.path());
    request["format"] = json!("opus");
    request["segments"][0]["output"] = json!("outputs/quiet.opus");
    request["segments"][1]["output"] = json!("outputs/loud.opus");
    fs::write(
        directory.path().join("request.json"),
        serde_json::to_vec_pretty(&request).unwrap(),
    )
    .unwrap();
    assert!(plan(directory.path(), false).status.success());
    fs::create_dir(directory.path().join("outputs")).unwrap();
    fs::write(
        directory.path().join("outputs/quiet.opus"),
        b"quiet sentinel",
    )
    .unwrap();
    fs::write(directory.path().join("outputs/loud.opus"), b"loud sentinel").unwrap();

    let result = render(directory.path(), true);

    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("Opus output is unavailable"));
    assert_eq!(
        fs::read(directory.path().join("outputs/quiet.opus")).unwrap(),
        b"quiet sentinel"
    );
    assert_eq!(
        fs::read(directory.path().join("outputs/loud.opus")).unwrap(),
        b"loud sentinel"
    );
    assert!(!directory.path().join("report.json").exists());
}

#[cfg(feature = "opus-encoding")]
#[test]
fn lossy_opus_outputs_are_redecoded_and_verified() {
    let directory = tempfile::tempdir().unwrap();
    let mut request = prepare(directory.path());
    request["format"] = json!("opus");
    request["verification_tolerance_lu_db"] = json!(0.5);
    request["duration_tolerance_ms"] = json!(100.0);
    request["segments"][0]["output"] = json!("outputs/quiet.opus");
    request["segments"][1]["output"] = json!("outputs/loud.opus");
    fs::write(
        directory.path().join("request.json"),
        serde_json::to_vec_pretty(&request).unwrap(),
    )
    .unwrap();

    assert!(plan(directory.path(), false).status.success());
    let result = render(directory.path(), false);

    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
