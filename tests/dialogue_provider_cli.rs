use std::process::Command;

#[test]
fn external_provider_cli_emits_forge_dialogue_ranges_and_private_audit() {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("provider.json");
    let ranges = work.path().join("ranges.json");
    let audit = work.path().join("audit.json");
    std::fs::write(
        &input,
        format!(
            r#"{{
              "schema_version": 1,
              "kind": "asr",
              "provider": "reviewed-asr",
              "provider_version": "1.0",
              "model": "ja-speech",
              "model_version": "2026-01",
              "model_sha256": "{}",
              "source_duration_seconds": 5.0,
              "language": "ja",
              "segments": [
                {{"start_seconds": 1.0, "end_seconds": 2.0, "confidence": 0.95, "transcript": "secret"}},
                {{"start_seconds": 3.0, "end_seconds": 4.0, "confidence": 0.10}}
              ]
            }}"#,
            "a".repeat(64)
        ),
    )
    .unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_forge-dialogue-provider"))
        .args([
            "--ranges-output",
            ranges.to_str().unwrap(),
            "--output",
            audit.to_str().unwrap(),
            input.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success());
    let loaded = forge_normalizer::normalize::load_dialogue_ranges(&ranges).unwrap();
    assert_eq!(loaded.len(), 1);
    let evidence = std::fs::read_to_string(audit).unwrap();
    assert!(evidence.contains("\"transcript_data_present\": true"));
    assert!(!evidence.contains("secret"));
}
