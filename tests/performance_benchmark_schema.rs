use serde_json::{json, Value};

fn base_report() -> Value {
    json!({
        "$schema": "https://penguin425.github.io/audio-normalizer/schema/performance-benchmark-v1",
        "generator": "forge-benchmark/1",
        "generated_unix_ms": 1_750_000_000_000_u64,
        "system": {
            "os": "Linux",
            "os_release": "6.8.0",
            "architecture": "x86_64",
            "cpu_model": "Example CPU",
            "cpu_count": 8,
            "python_version": "3.12.3",
            "forge_version": "forge 0.103.0",
            "ffmpeg_version": "ffmpeg version 7.1"
        },
        "configuration": {
            "duration_seconds": 3600,
            "sample_rate_hz": 48000,
            "pathological_chunks": 100001,
            "timeout_seconds": 7200,
            "cases": ["wav-stereo-normalize"]
        },
        "results": [{
            "id": "wav-stereo-normalize",
            "category": "lossless",
            "input_format": "wav",
            "operation": "normalize",
            "channels": 2,
            "sample_rate_hz": 48000,
            "duration_seconds": 3600.0,
            "input_bytes": 691200044_u64,
            "output_bytes": 691200044_u64,
            "command": ["forge", "<input.wav>", "--overwrite", "-o", "<output.wav>"],
            "exit_code": 0,
            "wall_seconds": 12.5,
            "user_cpu_seconds": 18.0,
            "system_cpu_seconds": 1.0,
            "cpu_percent": 152.0,
            "peak_rss_bytes": 100000000,
            "realtime_factor": 288.0,
            "expected_exit_codes": [0],
            "passed": true,
            "regression": null
        }],
        "error": null,
        "passed": true
    })
}

#[test]
fn performance_benchmark_example_conforms_to_schema() {
    let schema: Value = serde_json::from_str(include_str!(
        "../schema/performance-benchmark-v1.schema.json"
    ))
    .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON Schema");
    let report = base_report();
    let errors: Vec<_> = validator
        .iter_errors(&report)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema violations: {errors:#?}");
}

#[test]
fn performance_benchmark_schema_rejects_duplicate_cases() {
    let schema: Value = serde_json::from_str(include_str!(
        "../schema/performance-benchmark-v1.schema.json"
    ))
    .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON Schema");
    let mut report = base_report();
    report["configuration"]["cases"] = json!(["wav-stereo-normalize", "wav-stereo-normalize"]);
    assert!(!validator.is_valid(&report));
}

#[test]
fn performance_benchmark_error_report_conforms_to_schema() {
    let schema: Value = serde_json::from_str(include_str!(
        "../schema/performance-benchmark-v1.schema.json"
    ))
    .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("valid JSON Schema");
    let mut report = base_report();
    report["results"] = json!([]);
    report["error"] = json!("measured command exceeded its timeout");
    report["passed"] = json!(false);
    assert!(validator.is_valid(&report));
}
