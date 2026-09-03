use forge_normalizer::wav::WavReader;
use serde_json::Value;
use std::f64::consts::PI;
use std::fs;
use std::process::Command;

#[test]
fn dsf_analysis_normalization_and_manifest_are_read_only_and_auditable() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("programme.dsf");
    let output = directory.path().join("normalized.wav");
    let manifest = directory.path().join("manifest.json");
    fs::write(&input, tone_dsf(0.5)).unwrap();
    let original = fs::read(&input).unwrap();

    let analysis = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args([
            input.to_str().unwrap(),
            "--analyze",
            "--json",
            "--manifest",
            manifest.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        analysis.status.success(),
        "{}",
        String::from_utf8_lossy(&analysis.stderr)
    );
    let reports: Value = serde_json::from_slice(&analysis.stdout).unwrap();
    assert_eq!(reports[0]["sample_rate_hz"], 88_200);
    assert_eq!(reports[0]["channels"], 2);
    assert!(reports[0]["integrated_lufs"].is_number());
    let delivery: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    assert_eq!(delivery["assets"][0]["container_qc"]["format"], "dsf");
    assert_eq!(
        delivery["assets"][0]["container_qc"]["properties"]["conversion_policy"],
        forge_normalizer::dsd::CONVERSION_POLICY
    );
    let manifest_schema: Value =
        serde_json::from_str(include_str!("../schema/delivery-manifest-v4.schema.json")).unwrap();
    let validator = jsonschema::validator_for(&manifest_schema).unwrap();
    let errors: Vec<_> = validator
        .iter_errors(&delivery)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "manifest schema violations: {errors:#?}");
    assert_eq!(fs::read(&input).unwrap(), original);

    let normalized = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args([
            input.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        normalized.status.success(),
        "{}",
        String::from_utf8_lossy(&normalized.stderr)
    );
    let wav = WavReader::probe(&output).unwrap();
    assert_eq!(wav.sample_rate, 88_200);
    assert_eq!(wav.channels, 2);
    assert_eq!(fs::read(&input).unwrap(), original);
}

#[test]
fn dsd_container_qc_cli_reports_policy_and_rejects_bad_geometry() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("programme.dsf");
    fs::write(&input, tone_dsf(0.01)).unwrap();
    let valid = Command::new(env!("CARGO_BIN_EXE_forge-container-qc"))
        .arg(&input)
        .output()
        .unwrap();
    assert!(valid.status.success());
    let audit: Value = serde_json::from_slice(&valid.stdout).unwrap();
    assert_eq!(audit["passed"], true);
    assert!(audit["layers"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|layer| layer["checks"].as_array().unwrap())
        .any(|item| item["rule_id"] == "FORGE-DSD-CONVERSION-POLICY"));
    let container_schema: Value =
        serde_json::from_str(include_str!("../schema/container-qc-v1.schema.json")).unwrap();
    let validator = jsonschema::validator_for(&container_schema).unwrap();
    let errors: Vec<_> = validator
        .iter_errors(&audit)
        .map(|error| error.to_string())
        .collect();
    assert!(
        errors.is_empty(),
        "container schema violations: {errors:#?}"
    );

    let mut bytes = fs::read(&input).unwrap();
    bytes[12..20].copy_from_slice(&1_u64.to_le_bytes());
    fs::write(&input, bytes).unwrap();
    let invalid = Command::new(env!("CARGO_BIN_EXE_forge-container-qc"))
        .arg(&input)
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(1));
    let audit: Value = serde_json::from_slice(&invalid.stdout).unwrap();
    assert_eq!(
        audit["layers"][0]["checks"][0]["rule_id"],
        "FORGE-DSD-STRUCTURE"
    );
}

fn tone_dsf(duration_seconds: f64) -> Vec<u8> {
    let sample_rate = 2_822_400_u32;
    let sample_count = (f64::from(sample_rate) * duration_seconds).round() as usize;
    let mut error = 0.0;
    let mut packed = Vec::with_capacity(sample_count.div_ceil(8));
    let mut byte = 0_u8;
    for index in 0..sample_count {
        let desired = 0.1 * (2.0 * PI * 1_000.0 * index as f64 / f64::from(sample_rate)).sin();
        let output = if error + desired >= 0.0 { 1.0 } else { -1.0 };
        error += desired - output;
        if output > 0.0 {
            byte |= 1 << (index % 8);
        }
        if index % 8 == 7 {
            packed.push(byte);
            byte = 0;
        }
    }
    if !sample_count.is_multiple_of(8) {
        packed.push(byte);
    }
    let block_size = 4096_usize;
    let rounds = packed.len().div_ceil(block_size);
    let mut data = Vec::new();
    for round in 0..rounds {
        let start = round * block_size;
        let end = (start + block_size).min(packed.len());
        for _ in 0..2 {
            data.extend_from_slice(&packed[start..end]);
            data.resize(data.len() + block_size - (end - start), 0);
        }
    }
    let total_size = 92_u64 + data.len() as u64;
    let mut output = Vec::new();
    output.extend_from_slice(b"DSD ");
    output.extend_from_slice(&28_u64.to_le_bytes());
    output.extend_from_slice(&total_size.to_le_bytes());
    output.extend_from_slice(&0_u64.to_le_bytes());
    output.extend_from_slice(b"fmt ");
    output.extend_from_slice(&52_u64.to_le_bytes());
    output.extend_from_slice(&1_u32.to_le_bytes());
    output.extend_from_slice(&0_u32.to_le_bytes());
    output.extend_from_slice(&2_u32.to_le_bytes());
    output.extend_from_slice(&2_u32.to_le_bytes());
    output.extend_from_slice(&sample_rate.to_le_bytes());
    output.extend_from_slice(&1_u32.to_le_bytes());
    output.extend_from_slice(&(sample_count as u64).to_le_bytes());
    output.extend_from_slice(&(block_size as u32).to_le_bytes());
    output.extend_from_slice(&0_u32.to_le_bytes());
    output.extend_from_slice(b"data");
    output.extend_from_slice(&(data.len() as u64 + 12).to_le_bytes());
    output.extend_from_slice(&data);
    output
}
