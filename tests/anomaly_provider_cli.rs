use std::process::Command;

#[test]
fn anomaly_provider_cli_writes_provenance_bound_audit() {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("provider.json");
    let output = work.path().join("audit.json");
    std::fs::write(
        &input,
        format!(
            r#"{{
              "schema_version": 1,
              "provider": "reviewed-detector",
              "provider_version": "2.0",
              "model": "audio-quality",
              "model_version": "2026-08",
              "model_sha256": "{}",
              "source_sha256": "{}",
              "source_duration_seconds": 6.0,
              "sample_rate_hz": 48000,
              "events": [
                {{"kind":"pop","start_seconds":1.0,"end_seconds":1.1,"confidence":0.95,"severity":0.8,"channel":1,"evidence_label":"impulse-spectrum"}},
                {{"kind":"noise","start_seconds":3.0,"end_seconds":4.0,"confidence":0.2,"severity":0.9}}
              ]
            }}"#,
            "a".repeat(64),
            "b".repeat(64)
        ),
    )
    .unwrap();
    let provider_instance: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&input).unwrap()).unwrap();
    let provider_schema: serde_json::Value = serde_json::from_str(include_str!(
        "../schema/audio-anomaly-provider-v1.schema.json"
    ))
    .unwrap();
    let provider_validator = jsonschema::validator_for(&provider_schema).unwrap();
    assert!(provider_validator.is_valid(&provider_instance));
    let status = Command::new(env!("CARGO_BIN_EXE_forge-anomaly-provider"))
        .args([
            "--confidence-threshold",
            "0.6",
            "--severity-threshold",
            "0.5",
            "--output",
            output.to_str().unwrap(),
            input.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success());
    let audit: serde_json::Value = serde_json::from_slice(&std::fs::read(output).unwrap()).unwrap();
    let audit_schema: serde_json::Value = serde_json::from_str(include_str!(
        "../schema/anomaly-provider-audit-v1.schema.json"
    ))
    .unwrap();
    let audit_validator = jsonschema::validator_for(&audit_schema).unwrap();
    assert!(audit_validator.is_valid(&audit));
    assert_eq!(audit["schema_version"], 1);
    assert_eq!(audit["selected_event_count"], 1);
    assert_eq!(audit["selected_by_kind"]["pop"], 1);
    assert_eq!(audit["source_sha256"], "b".repeat(64));
    assert_eq!(audit["model_sha256"], "a".repeat(64));
    assert_eq!(audit["events"][0]["evidence_label"], "impulse-spectrum");
    assert_eq!(audit["events"][0]["selected"], true);
}

#[test]
fn anomaly_provider_cli_rejects_unknown_fields() {
    let work = tempfile::tempdir().unwrap();
    let input = work.path().join("provider.json");
    std::fs::write(
        &input,
        format!(
            r#"{{"schema_version":1,"provider":"p","provider_version":"1","model":"m","model_version":"1","model_sha256":"{}","source_sha256":"{}","source_duration_seconds":1.0,"events":[],"unexpected":true}}"#,
            "a".repeat(64),
            "b".repeat(64)
        ),
    )
    .unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_forge-anomaly-provider"))
        .arg(input)
        .status()
        .unwrap();
    assert!(!status.success());
}
