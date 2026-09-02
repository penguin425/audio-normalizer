use forge_normalizer::wav::{
    default_channel_roles, AudioBuffer, PcmKind, WavContainer, WavWriter, WaveChunk,
};
use serde_json::Value;
use std::process::Command;

#[test]
fn exposes_bounded_interactivity_audit_options() {
    let output = Command::new(env!("CARGO_BIN_EXE_forge-adm-interactivity-qc"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    for option in [
        "--profile",
        "--output",
        "--max-objects",
        "--max-configurations",
        "--max-axml-bytes",
        "--max-xml-nodes",
        "--overwrite",
        "--compact",
    ] {
        assert!(stdout.contains(option), "missing {option} in:\n{stdout}");
    }
}

#[test]
fn audits_parent_and_alternative_ranges_with_versioned_evidence() {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("programme.bw64");
    let report = work.path().join("report.json");
    write_adm(
        &input,
        r#"<audioObject audioObjectID="AO_1001" audioObjectName="Dialogue" interact="1">
  <gain gainUnit="dB">0</gain>
  <audioObjectInteraction onOffInteract="0" gainInteract="1" positionInteract="1">
    <gainInteractionRange bound="min" gainUnit="dB">-12</gainInteractionRange>
    <gainInteractionRange bound="max" gainUnit="dB">6</gainInteractionRange>
    <positionInteractionRange coordinate="azimuth" bound="min">-30</positionInteractionRange>
    <positionInteractionRange coordinate="azimuth" bound="max">30</positionInteractionRange>
  </audioObjectInteraction>
  <alternativeValueSet alternativeValueSetID="AVS_1001_0001"><gain gainUnit="dB">3</gain></alternativeValueSet>
</audioObject>"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_forge-adm-interactivity-qc"))
        .arg(&input)
        .args(["--profile", "bs2168-emission-ranges"])
        .args(["--output", report.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let instance: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert_eq!(instance["passed"], true);
    assert_eq!(instance["profile"], "bs2168-emission-ranges");
    assert_eq!(instance["object_count"], 1);
    assert_eq!(instance["configuration_count"], 2);
    assert_eq!(instance["interactive_configuration_count"], 2);
    assert_eq!(instance["continuous_audio_compliance_verified"], false);
    assert_eq!(instance["endpoint_rendering_required"], true);
    assert_eq!(instance["configurations"][1]["interaction_inherited"], true);
    assert_eq!(instance["input_sha256"].as_str().unwrap().len(), 64);
    validate_schema(
        &instance,
        include_str!("../schema/adm-interactivity-report-v1.schema.json"),
    );
}

#[test]
fn reports_implicit_unbounded_gain_as_qc_failure() {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("unbounded.bw64");
    let report = work.path().join("report.json");
    write_adm(
        &input,
        r#"<audioObject audioObjectID="AO_1001" audioObjectName="Unbounded" interact="1"/>"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_forge-adm-interactivity-qc"))
        .arg(&input)
        .args(["--output", report.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let instance: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert_eq!(instance["passed"], false);
    assert_eq!(instance["configurations"][0]["gain_interact"], true);
    assert_eq!(instance["configurations"][0]["gain_maximum"], Value::Null);
    assert!(instance["configurations"][0]["rules"]
        .as_array()
        .unwrap()
        .iter()
        .any(|rule| { rule["rule_id"] == "FORGE-GAIN-RANGE-EXPLICIT" && rule["passed"] == false }));
    validate_schema(
        &instance,
        include_str!("../schema/adm-interactivity-report-v1.schema.json"),
    );
}

#[test]
fn rejects_alternative_expansion_over_the_configured_limit() {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("alternatives.bw64");
    let report = work.path().join("report.json");
    write_adm(
        &input,
        r#"<audioObject audioObjectID="AO_1001" audioObjectName="Alternatives"><alternativeValueSet alternativeValueSetID="AVS_1001_0001"/></audioObject>"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_forge-adm-interactivity-qc"))
        .arg(&input)
        .args(["--output", report.to_str().unwrap()])
        .args(["--max-configurations", "1"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("configuration limit"));
    assert!(!report.exists());
}

#[test]
fn rejects_oversized_axml_before_parsing() {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("bounded.bw64");
    let report = work.path().join("report.json");
    write_adm(
        &input,
        r#"<audioObject audioObjectID="AO_1001" audioObjectName="Bounded"/>"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_forge-adm-interactivity-qc"))
        .arg(&input)
        .args(["--output", report.to_str().unwrap()])
        .args(["--max-axml-bytes", "1"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("exceeding the configured limit 1"));
    assert!(!report.exists());
}

fn write_adm(input: &std::path::Path, object: &str) {
    let audio = AudioBuffer {
        sample_rate: 48_000,
        channels: 1,
        frames: 480,
        data: vec![vec![0.0; 480]],
        channel_roles: default_channel_roles(1),
        source_kind: PcmKind::F32,
    };
    let axml = format!(
        r#"<audioFormatExtended version="ITU-R_BS.2076-3">
  <audioProgramme audioProgrammeID="APR_1001" audioProgrammeName="Main"><audioContentIDRef>ACO_1001</audioContentIDRef></audioProgramme>
  <audioContent audioContentID="ACO_1001" audioContentName="Content"><audioObjectIDRef>AO_1001</audioObjectIDRef></audioContent>
  {object}
  <audioTrackUID UID="ATU_00000001"><audioChannelFormatIDRef>AC_00010001</audioChannelFormatIDRef><audioPackFormatIDRef>AP_00010001</audioPackFormatIDRef></audioTrackUID>
</audioFormatExtended>"#
    );
    let mut chna = Vec::with_capacity(44);
    chna.extend_from_slice(&1_u16.to_le_bytes());
    chna.extend_from_slice(&1_u16.to_le_bytes());
    chna.extend_from_slice(&1_u16.to_le_bytes());
    chna.extend_from_slice(b"ATU_00000001");
    chna.extend_from_slice(&[0; 14]);
    chna.extend_from_slice(&[0; 11]);
    chna.push(0);
    WavWriter::write_with_metadata(
        input,
        &audio,
        PcmKind::F32,
        false,
        WavContainer::Bw64,
        &[
            WaveChunk {
                id: *b"axml",
                body: axml.into_bytes(),
            },
            WaveChunk {
                id: *b"chna",
                body: chna,
            },
        ],
    )
    .unwrap();
}

fn validate_schema(instance: &Value, schema: &str) {
    let schema: Value = serde_json::from_str(schema).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let errors: Vec<_> = validator
        .iter_errors(instance)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}
