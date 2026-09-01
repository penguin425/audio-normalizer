use forge_normalizer::atsc_a85_service::{REPORT_SCHEMA, REQUEST_SCHEMA};
use forge_normalizer::normalize::{self, DialogueRange};
use forge_normalizer::wav::{default_channel_roles, AudioBuffer, PcmKind, WavWriter};
use serde_json::{json, Value};
use std::f64::consts::TAU;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const DURATION_SECONDS: usize = 3;

fn write_stereo_tone(path: &Path, amplitude: f64) {
    let sample_rate = 48_000_u32;
    let frames = sample_rate as usize * DURATION_SECONDS;
    let left = (0..frames)
        .map(|frame| (amplitude * (TAU * 997.0 * frame as f64 / sample_rate as f64).sin()) as f32)
        .collect::<Vec<_>>();
    let right = left.clone();
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

fn run(request: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_forge-streaming-qc"))
        .arg(request)
        .args(["--profile", "atsc-a85-service"])
        .output()
        .unwrap()
}

fn dialogue_loudness(path: &Path) -> f64 {
    normalize::analyze_dialogue_ranges_with_roles(
        path,
        None,
        &[DialogueRange {
            start_seconds: 0.0,
            duration_seconds: DURATION_SECONDS as f64,
        }],
    )
    .unwrap()
    .lufs
}

#[test]
fn audits_mixed_metadata_and_nonmetadata_service_assets() {
    let directory = tempfile::tempdir().unwrap();
    let long = directory.path().join("long.wav");
    let short = directory.path().join("short.wav");
    let request = directory.path().join("service.json");
    write_stereo_tone(&long, 0.06);
    write_stereo_tone(&short, 0.06);
    let long_loudness = dialogue_loudness(&long);
    let target = normalize::analyze_file(&short).unwrap().lufs;
    assert!((-27.0..=-23.0).contains(&target));
    fs::write(
        &request,
        serde_json::to_vec_pretty(&json!({
            "schema": REQUEST_SCHEMA,
            "service_id": "mixed-service",
            "target_lkfs": target,
            "assets": [
                {
                    "id": "long",
                    "path": "long.wav",
                    "programme_kind": "long_form",
                    "delivery_codec": "ac3",
                    "declaration_source": "traffic-system dialnorm export",
                    "declared_loudness_lkfs": long_loudness,
                    "dialogue_ranges": [{
                        "start_seconds": 0.0,
                        "duration_seconds": DURATION_SECONDS
                    }]
                },
                {
                    "id": "short",
                    "path": "short.wav",
                    "programme_kind": "short_form",
                    "delivery_codec": "aac",
                    "declaration_source": "packager codec report",
                    "accompanies": "long",
                    "inserted": true
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    let request_document: Value = serde_json::from_slice(&fs::read(&request).unwrap()).unwrap();
    let request_schema: Value = serde_json::from_str(include_str!(
        "../schema/atsc-a85-service-request-v1.schema.json"
    ))
    .unwrap();
    let request_validator = jsonschema::validator_for(&request_schema).unwrap();
    let request_errors = request_validator
        .iter_errors(&request_document)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(
        request_errors.is_empty(),
        "request schema violations: {request_errors:#?}"
    );

    let output = run(&request);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema"], REPORT_SCHEMA);
    assert_eq!(report["standard"]["name"], "ATSC A/85:2026-07");
    assert_eq!(report["assets"][0]["loudness_basis"], "dialogue_anchor");
    assert_eq!(report["assets"][1]["loudness_basis"], "full_programme");
    assert_eq!(report["assets"][0]["metadata_mode"], "metadata");
    assert_eq!(report["assets"][1]["metadata_mode"], "non_metadata");
    assert_eq!(report["warning_count"], 0);
    assert_eq!(report["passed"], true);
    assert!(report["request"]["sha256"].as_str().unwrap().len() == 64);
    assert!(report["service_checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(
            |check| check["rule_id"] == "ATSC-A85-L6-L7-MIXED-MODE-CONSISTENCY"
                && check["passed"] == true
        ));
    let report_schema: Value = serde_json::from_str(include_str!(
        "../schema/atsc-a85-service-report-v1.schema.json"
    ))
    .unwrap();
    let report_validator = jsonschema::validator_for(&report_schema).unwrap();
    let report_errors = report_validator
        .iter_errors(&report)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(
        report_errors.is_empty(),
        "report schema violations: {report_errors:#?}"
    );
}

#[test]
fn metadata_mismatch_is_a_qc_failure_not_a_request_error() {
    let directory = tempfile::tempdir().unwrap();
    let long = directory.path().join("long.wav");
    let request = directory.path().join("service.json");
    write_stereo_tone(&long, 0.06);
    let measured = dialogue_loudness(&long);
    fs::write(
        &request,
        serde_json::to_vec_pretty(&json!({
            "schema": REQUEST_SCHEMA,
            "service_id": "metadata-mismatch",
            "assets": [{
                "id": "long",
                "path": "long.wav",
                "programme_kind": "long_form",
                "delivery_codec": "xhe-aac",
                "declaration_source": "packager loudnessInfoSet export",
                "declared_loudness_lkfs": measured + 3.0,
                "dialogue_ranges": [{
                    "start_seconds": 0.0,
                    "duration_seconds": DURATION_SECONDS
                }]
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    let output = run(&request);
    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let check = report["assets"][0]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["rule_id"] == "ATSC-A85-L4-METADATA-MATCH")
        .unwrap();
    assert_eq!(check["passed"], false);
    assert_eq!(report["passed"], false);
}

#[test]
fn detects_short_form_louder_than_its_long_form_programme() {
    let directory = tempfile::tempdir().unwrap();
    let long = directory.path().join("long.wav");
    let short = directory.path().join("short.wav");
    let request = directory.path().join("service.json");
    write_stereo_tone(&long, 0.05);
    write_stereo_tone(&short, 0.07);
    let long_loudness = dialogue_loudness(&long);
    let short_loudness = normalize::analyze_file(&short).unwrap().lufs;
    let target = (long_loudness + short_loudness) / 2.0;
    assert!((-27.0..=-23.0).contains(&target));
    assert!((short_loudness - target).abs() <= 2.0);
    assert!((long_loudness - target).abs() <= 2.0);
    fs::write(
        &request,
        serde_json::to_vec_pretty(&json!({
            "schema": REQUEST_SCHEMA,
            "service_id": "relative-level",
            "target_lkfs": target,
            "assets": [
                {
                    "id": "long",
                    "path": "long.wav",
                    "programme_kind": "long_form",
                    "delivery_codec": "aac",
                    "declaration_source": "packager codec report",
                    "dialogue_ranges": [{
                        "start_seconds": 0.0,
                        "duration_seconds": DURATION_SECONDS
                    }]
                },
                {
                    "id": "short",
                    "path": "short.wav",
                    "programme_kind": "short_form",
                    "delivery_codec": "mp3",
                    "declaration_source": "packager codec report",
                    "accompanies": "long"
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    let output = run(&request);
    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let relationship = report["service_checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["rule_id"] == "ATSC-A85-L5-SHORT-NOT-LOUDER")
        .unwrap();
    assert_eq!(relationship["passed"], false);
}

#[test]
fn rejects_long_form_without_dialogue_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let audio = directory.path().join("long.wav");
    let request = directory.path().join("service.json");
    write_stereo_tone(&audio, 0.06);
    fs::write(
        &request,
        serde_json::to_vec_pretty(&json!({
            "schema": REQUEST_SCHEMA,
            "service_id": "missing-dialogue",
            "assets": [{
                "id": "long",
                "path": "long.wav",
                "programme_kind": "long_form",
                "delivery_codec": "ac3",
                "declaration_source": "traffic-system dialnorm export",
                "declared_loudness_lkfs": -24.0
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    let output = run(&request);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("dialogue ranges"));
}

#[test]
fn checked_in_schema_ids_match_public_contracts() {
    let request: Value = serde_json::from_str(include_str!(
        "../schema/atsc-a85-service-request-v1.schema.json"
    ))
    .unwrap();
    let report: Value = serde_json::from_str(include_str!(
        "../schema/atsc-a85-service-report-v1.schema.json"
    ))
    .unwrap();
    assert_eq!(request["$id"], REQUEST_SCHEMA);
    assert_eq!(report["$id"], REPORT_SCHEMA);
}
