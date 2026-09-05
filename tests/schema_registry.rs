use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn load_json(path: &Path) -> Value {
    let bytes = fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("parse {} as JSON: {error}", path.display()))
}

fn validation_errors(schema: &Value, instance: &Value) -> Vec<String> {
    let validator = jsonschema::validator_for(schema).expect("compile JSON Schema draft");
    validator
        .iter_errors(instance)
        .map(|error| error.to_string())
        .collect()
}

#[test]
fn registry_and_every_managed_schema_compile_offline() {
    let root = repo_root();
    let registry_path = root.join("schema/schema-registry-v1.json");
    let registry = load_json(&registry_path);
    let registry_schema = load_json(&root.join("schema/schema-registry-v1.schema.json"));

    let errors = validation_errors(&registry_schema, &registry);
    assert!(errors.is_empty(), "registry schema violations: {errors:#?}");

    let entries = registry["entries"]
        .as_array()
        .expect("registry entries array");
    for entry in entries {
        if entry["document_kind"] != "json-schema" {
            continue;
        }
        let relative = entry["path"].as_str().expect("registered path");
        let schema = load_json(&root.join(relative));
        jsonschema::meta::validate(&schema)
            .unwrap_or_else(|error| panic!("meta-validate {relative}: {error}"));
        jsonschema::validator_for(&schema)
            .unwrap_or_else(|error| panic!("compile {relative} offline: {error}"));

        for sample in entry["samples"].as_array().into_iter().flatten() {
            let sample_reference = sample.as_str().expect("sample repository reference");
            let sample_path = sample_reference
                .strip_prefix("repo:")
                .expect("sample must use repo: prefix");
            let instance = load_json(&root.join(sample_path));
            let errors = validation_errors(&schema, &instance);
            assert!(
                errors.is_empty(),
                "{sample_path} violates {relative}: {errors:#?}"
            );
        }
    }
}
