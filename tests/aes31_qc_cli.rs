use serde_json::Value;
use std::fs;
use std::process::Command;

const ADL: &str = r#"<ADL>
<VERSION>
(ADL_ID) "06,64,43,52,01,01,01,04,01,02,03,04,"
(ADL_UID) 12345678-1234-4234-8234-123456789abc
(VER_ADL_VERSION) 01.02
(VER_CREATOR) "Interchange fixture"
(VER_CRTR) 01.00
</VERSION>
<PROJECT>
(PROJ_TITLE) "CLI fixture"
(PROJ_ORIGINATOR) "Forge"
(PROJ_CREATE_DATE) 2026-07-29T12:00:00Z
(PROJ_NOTES) ""
(PROJ_CLIENT_DATA) ""
</PROJECT>
<SEQUENCE>
(SEQ_SAMPLE_RATE) S48000
(SEQ_FRAME_RATE) 25
(SEQ_ADL_LEVEL) 1
(SEQ_CLEAN) TRUE
(SEQ_DEST_START) 00:00:00:00/0000
</SEQUENCE>
<TRACKLIST>
(Track) 1 "Mono"
</TRACKLIST>
<SOURCE_INDEX>
(Index) 1
(F) "URL:file://localhost/audio/source.wav" _ _ _ "Source" N
</SOURCE_INDEX>
<EVENT_LIST>
(Entry) 1
(Cut) I 1 1 1 00:00:00:00/0000 00:00:00:00/0000 00:00:01:00/0000 _
</EVENT_LIST>
</ADL>
"#;

#[test]
fn cli_audits_real_world_record_layout_and_schema_contract() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("project.adl");
    let report = directory.path().join("report.json");
    fs::write(&input, ADL).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-aes31-qc"))
        .arg(&input)
        .arg("--compact")
        .arg("--output")
        .arg(&report)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:#?}");
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("PASS"));
    let value: Value = serde_json::from_slice(&fs::read(report).unwrap()).unwrap();
    assert_eq!(value["schema"], forge_normalizer::aes31_qc::AES31_QC_SCHEMA);
    assert_eq!(
        value["properties"]["method"],
        "forge-aes31-3-edml-structural-v1"
    );
    assert_eq!(value["properties"]["source_count"], 1);
    assert_eq!(value["properties"]["event_count"], 1);
}

#[test]
fn cli_uses_exit_one_for_qc_failure_and_emits_json() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("broken.adl");
    fs::write(&input, ADL.replace("(Cut) I 1", "(Cut) I 99")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-aes31-qc"))
        .arg(&input)
        .arg("--compact")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["passed"], false);
    assert!(value["findings"].as_array().unwrap().iter().any(|finding| {
        finding["rule_id"] == "FORGE-AES31-EVENT-SOURCE-REFERENCE" && finding["passed"] == false
    }));
}

#[test]
fn cli_uses_exit_two_for_resource_limit_error() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("oversized.adl");
    fs::write(&input, vec![b' '; 16 * 1024 * 1024 + 1]).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_forge-aes31-qc"))
        .arg(&input)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("safety limit"));
}
