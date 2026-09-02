use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

fn write_frame(
    directory: &Path,
    name: &str,
    id: &str,
    start: &str,
    extra_attributes: &str,
) -> PathBuf {
    let path = directory.join(name);
    std::fs::write(
        &path,
        format!(
            r#"<frame version="ITU-R_BS.2125-1"><frameHeader><frameFormat frameFormatID="FF_{id}" start="{start}" duration="24000S48000" type="divided" numMetadataChunks="2" countToSameChunk="1"{extra_attributes}/><transportTrackFormat/></frameHeader><audioFormatExtended/></frame>"#
        ),
    )
    .unwrap();
    path
}

#[test]
fn accepts_divided_frames_and_reports_logical_flow_evidence() {
    let work = tempfile::tempdir().unwrap();
    let report = work.path().join("report.json");
    let frames = [
        write_frame(work.path(), "1-1.xml", "00000001_01", "0S48000", ""),
        write_frame(work.path(), "1-2.xml", "00000001_02", "0S48000", ""),
        write_frame(work.path(), "2-1.xml", "00000002_01", "24000S48000", ""),
        write_frame(work.path(), "2-2.xml", "00000002_02", "24000S48000", ""),
    ];
    let output = Command::new(env!("CARGO_BIN_EXE_forge-sadm-qc"))
        .args(&frames)
        .args(["--output", report.to_str().unwrap(), "--compact"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("PASS (4 frames)"));
    let audit: Value = serde_json::from_slice(&std::fs::read(report).unwrap()).unwrap();
    assert_eq!(audit["validator"], "forge-bs2125-1-flow-2");
    assert_eq!(audit["frame_count"], 4);
    assert_eq!(audit["passed"], true);
    assert!(audit["flow_rules"].as_array().unwrap().iter().any(|rule| {
        rule["rule_id"] == "BS2125-FLOW-TYPE"
            && rule["observed"] == "divided-frame"
            && rule["passed"] == true
    }));
    assert!(audit["flow_rules"].as_array().unwrap().iter().any(|rule| {
        rule["rule_id"] == "BS2125-LOGICAL-FRAME-COUNT"
            && rule["observed"] == "4 input frame document(s), 2 logical frame(s)"
    }));
}

#[test]
fn returns_one_and_writes_evidence_for_an_exact_timing_gap() {
    let work = tempfile::tempdir().unwrap();
    let first = work.path().join("first.xml");
    let second = work.path().join("second.xml");
    let report = work.path().join("report.json");
    for (path, id, start, kind) in [
        (&first, "00000001", "00:00:00.000000000", "header"),
        (&second, "00000002", "00:00:00.500000001", "full"),
    ] {
        std::fs::write(
            path,
            format!(
                r#"<frame><frameHeader><frameFormat frameFormatID="FF_{id}" start="{start}" duration="00:00:00.500000000" type="{kind}"/><transportTrackFormat/></frameHeader><audioFormatExtended/></frame>"#
            ),
        )
        .unwrap();
    }
    let output = Command::new(env!("CARGO_BIN_EXE_forge-sadm-qc"))
        .args([first, second])
        .args(["--output", report.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let audit: Value = serde_json::from_slice(&std::fs::read(report).unwrap()).unwrap();
    assert_eq!(audit["passed"], false);
    assert!(audit["flow_rules"].as_array().unwrap().iter().any(|rule| {
        rule["rule_id"] == "BS2125-LOGICAL-FRAME-CONTIGUITY" && rule["passed"] == false
    }));
}

#[test]
fn returns_two_without_a_report_for_an_unreadable_input() {
    let work = tempfile::tempdir().unwrap();
    let missing = work.path().join("missing.xml");
    let report = work.path().join("report.json");
    let output = Command::new(env!("CARGO_BIN_EXE_forge-sadm-qc"))
        .arg(missing)
        .args(["--output", report.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("forge-sadm-qc: error: read"));
    assert!(!report.exists());
}
