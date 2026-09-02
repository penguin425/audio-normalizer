use forge_normalizer::wav::{
    default_channel_roles, AudioBuffer, PcmKind, WavContainer, WavWriter, WaveChunk,
};
use serde_json::Value;
use std::process::Command;

#[test]
fn exposes_bounded_semantics_audit_options() {
    let output = Command::new(env!("CARGO_BIN_EXE_forge-adm-semantics-qc"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    for option in [
        "--presentation-intent",
        "--expected-default-programme",
        "--renderer-object-limit",
        "--output",
        "--max-programmes",
        "--max-contents",
        "--max-objects",
        "--max-report-items",
        "--max-axml-bytes",
        "--max-xml-nodes",
        "--overwrite",
        "--compact",
    ] {
        assert!(stdout.contains(option), "missing {option} in:\n{stdout}");
    }
}

#[test]
fn reports_dialogue_selection_importance_and_non_authoritative_tags() {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("programme.bw64");
    let report = work.path().join("report.json");
    write_adm(
        &input,
        r#"<audioFormatExtended version="ITU-R_BS.2076-3">
  <audioProgramme audioProgrammeID="APR_1002" audioProgrammeName="Alternative"><audioContentIDRef>ACO_1002</audioContentIDRef></audioProgramme>
  <audioProgramme audioProgrammeID="APR_1001" audioProgrammeName="Main"><audioContentIDRef>ACO_1001</audioContentIDRef><alternativeValueSetIDRef>AVS_1002_0001</alternativeValueSetIDRef></audioProgramme>
  <audioContent audioContentID="ACO_1001" audioContentName="Description"><audioObjectIDRef>AO_1001</audioObjectIDRef><dialogue dialogueContentKind="4">1</dialogue></audioContent>
  <audioContent audioContentID="ACO_1002" audioContentName="Music and effects"><audioObjectIDRef>AO_1002</audioObjectIDRef><dialogue nonDialogueContentKind="3">0</dialogue></audioContent>
  <audioObject audioObjectID="AO_1001" audioObjectName="Description" importance="10"><audioPackFormatIDRef>AP_00031001</audioPackFormatIDRef><audioTrackUIDRef>ATU_00000001</audioTrackUIDRef></audioObject>
  <audioObject audioObjectID="AO_1002" audioObjectName="Alternative" importance="2"><audioComplementaryObjectIDRef>AO_1003</audioComplementaryObjectIDRef><alternativeValueSet alternativeValueSetID="AVS_1002_0001"/></audioObject>
  <audioObject audioObjectID="AO_1003" audioObjectName="Unranked"/>
  <audioPackFormat audioPackFormatID="AP_00031001" audioPackFormatName="Object" typeLabel="0003" importance="5"><audioChannelFormatIDRef>AC_00031001</audioChannelFormatIDRef></audioPackFormat>
  <audioChannelFormat audioChannelFormatID="AC_00031001" audioChannelFormatName="Object" typeLabel="0003"><audioBlockFormatObjects audioBlockFormatID="AB_00031001_00000001" importance="1"/></audioChannelFormat>
  <audioTrackUID UID="ATU_00000001"><audioChannelFormatIDRef>AC_00031001</audioChannelFormatIDRef><audioPackFormatIDRef>AP_00031001</audioPackFormatIDRef></audioTrackUID>
  <tagList><tagGroup><tag class="genre">documentary</tag><audioProgrammeIDRef>APR_1001</audioProgrammeIDRef></tagGroup></tagList>
</audioFormatExtended>"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_forge-adm-semantics-qc"))
        .arg(&input)
        .args(["--expected-default-programme", "APR_1001"])
        .args(["--renderer-object-limit", "2"])
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
    assert_eq!(instance["normative_passed"], true);
    assert_eq!(instance["requested_policy_passed"], true);
    assert_eq!(instance["default_programme_id"], "APR_1001");
    assert_eq!(
        instance["inferred_presentation_mode"],
        "hybrid-multiple-programmes-and-complementary"
    );
    assert_eq!(
        instance["dialogue_contents"][0]["content_kind"],
        "audio-description-visually-impaired"
    );
    assert_eq!(
        instance["dialogue_contents"][1]["content_kind"],
        "music-and-effects"
    );
    assert_eq!(
        instance["alternative_value_set_references"][0]["passed"],
        true
    );
    assert_eq!(
        instance["importance"]["object_threshold_plan"]["selected_threshold"],
        3
    );
    assert_eq!(
        instance["importance"]["object_threshold_plan"]["discard_candidates"][0],
        "AO_1002"
    );
    assert_eq!(instance["tag_semantics_authoritative"], false);
    assert_eq!(instance["rendered_audio_verified"], false);
    assert_eq!(instance["renderer_capacity_verified"], false);
    assert_eq!(instance["input_sha256"].as_str().unwrap().len(), 64);
    validate_schema(
        &instance,
        include_str!("../schema/adm-semantics-report-v1.schema.json"),
    );
}

#[test]
fn writes_a_report_and_exit_one_for_normative_semantic_failures() {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("invalid.bw64");
    let report = work.path().join("report.json");
    write_adm(
        &input,
        r#"<audioFormatExtended version="ITU-R_BS.2076-3">
  <audioProgramme audioProgrammeID="APR_1001" audioProgrammeName="Main"><audioContentIDRef>ACO_1001</audioContentIDRef></audioProgramme>
  <audioContent audioContentID="ACO_1001" audioContentName="Broken"><audioObjectIDRef>AO_1001</audioObjectIDRef><dialogue dialogueContentKind="7">1</dialogue><alternativeValueSetIDRef>AVS_1001_0001</alternativeValueSetIDRef><alternativeValueSetIDRef>AVS_1001_0002</alternativeValueSetIDRef></audioContent>
  <audioObject audioObjectID="AO_1001" audioObjectName="Broken" importance="11"><alternativeValueSet alternativeValueSetID="AVS_1001_0001"/><alternativeValueSet alternativeValueSetID="AVS_1001_0002"/></audioObject>
  <tagList><tagGroup><tag>orphan</tag></tagGroup></tagList>
</audioFormatExtended>"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_forge-adm-semantics-qc"))
        .arg(&input)
        .args(["--output", report.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let instance: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert_eq!(instance["passed"], false);
    assert_eq!(instance["normative_passed"], false);
    for rule_id in [
        "BS2076-3-DIALOGUE-KIND-VALUE",
        "BS2076-3-AVS-ONE-PER-OBJECT",
        "BS2076-3-IMPORTANCE-RANGE",
        "BS2076-3-TAG-GROUP-CONTENT",
    ] {
        assert!(instance["rules"].as_array().unwrap().iter().any(|rule| {
            rule["rule_id"] == rule_id && rule["enforced"] == true && rule["passed"] == false
        }));
    }
    validate_schema(
        &instance,
        include_str!("../schema/adm-semantics-report-v1.schema.json"),
    );
}

#[test]
fn enforces_explicit_presentation_intent_and_conservative_capacity() {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("policy.bw64");
    let report = work.path().join("report.json");
    write_adm(
        &input,
        r#"<audioFormatExtended version="ITU-R_BS.2076-3">
  <audioProgramme audioProgrammeID="APR_1001" audioProgrammeName="Main"/>
  <audioObject audioObjectID="AO_1001" audioObjectName="Protected" importance="10"/>
  <audioObject audioObjectID="AO_1002" audioObjectName="Unranked"/>
</audioFormatExtended>"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_forge-adm-semantics-qc"))
        .arg(&input)
        .args(["--presentation-intent", "interactive"])
        .args(["--renderer-object-limit", "1"])
        .args(["--output", report.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let instance: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert_eq!(instance["normative_passed"], true);
    assert_eq!(instance["requested_policy_passed"], false);
    assert_eq!(
        instance["importance"]["object_threshold_plan"]["selected_threshold"],
        Value::Null
    );
    assert_eq!(
        instance["importance"]["object_threshold_plan"]["requires_renderer_or_merge"],
        true
    );
    validate_schema(
        &instance,
        include_str!("../schema/adm-semantics-report-v1.schema.json"),
    );
}

#[test]
fn rejects_oversized_axml_before_writing_a_report() {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("bounded.bw64");
    let report = work.path().join("report.json");
    write_adm(
        &input,
        r#"<audioFormatExtended version="ITU-R_BS.2076-3"/>"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_forge-adm-semantics-qc"))
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

fn write_adm(input: &std::path::Path, axml: &str) {
    let audio = AudioBuffer {
        sample_rate: 48_000,
        channels: 1,
        frames: 480,
        data: vec![vec![0.0; 480]],
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
        input,
        &audio,
        PcmKind::F32,
        false,
        WavContainer::Bw64,
        &[
            WaveChunk {
                id: *b"axml",
                body: axml.as_bytes().to_vec(),
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
