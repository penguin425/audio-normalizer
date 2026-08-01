#![cfg(feature = "onnx-provider")]

use forge_normalizer::anomaly_provider::AnomalyKind;
use forge_normalizer::onnx_provider::{
    CalibrationEvidence, FallbackPolicy, FeatureFrames, LicenseEvidence, ModelClass,
    OnnxModelManifest, RuntimeLimits, TensorContract, ADAPTER, FEATURE_SCHEMA, MODEL_SCHEMA,
    SCHEMA_VERSION,
};
use serde_json::Value;

fn manifest() -> OnnxModelManifest {
    OnnxModelManifest {
        schema: MODEL_SCHEMA.into(),
        schema_version: SCHEMA_VERSION,
        adapter: ADAPTER.into(),
        provider: "reference-onnx".into(),
        provider_version: "1.0".into(),
        model: "quality-model".into(),
        model_version: "2026-08".into(),
        model_file: "quality.onnx".into(),
        model_sha256: "a".repeat(64),
        license: LicenseEvidence {
            spdx: "MIT".into(),
            url: "https://example.test/model".into(),
            dataset: "dataset-v1".into(),
            dataset_url: "https://example.test/dataset".into(),
        },
        calibration: CalibrationEvidence {
            report_sha256: "b".repeat(64),
            report_url: "https://example.test/calibration".into(),
        },
        input: TensorContract {
            name: "features".into(),
            shape: vec![1, -1, 2],
        },
        output: TensorContract {
            name: "scores".into(),
            shape: vec![1, -1, 2],
        },
        classes: vec![ModelClass {
            kind: AnomalyKind::Pop,
            confidence_threshold: 0.6,
            severity_threshold: 0.5,
            evidence_label: None,
        }],
        frame_hop_seconds: 0.1,
        limits: RuntimeLimits {
            max_model_bytes: 1024,
            max_feature_frames: 100,
            max_feature_values: 10_000,
            max_events: 100,
            max_inference_seconds: 10.0,
            intra_threads: 1,
        },
        fallback: FallbackPolicy::Reject,
    }
}

#[test]
fn model_manifest_and_feature_frames_match_published_schemas() {
    let model: Value = serde_json::to_value(manifest()).unwrap();
    let features = FeatureFrames {
        schema: FEATURE_SCHEMA.into(),
        schema_version: SCHEMA_VERSION,
        source_path: "programme.wav".into(),
        source_sha256: "c".repeat(64),
        source_duration_seconds: 1.0,
        sample_rate_hz: 48_000,
        frame_hop_seconds: 0.1,
        feature_count: 2,
        frames: vec![vec![0.0, 1.0]],
    };
    let features: Value = serde_json::to_value(features).unwrap();
    for (instance, source) in [
        (
            model,
            include_str!("../schema/onnx-anomaly-model-v1.schema.json"),
        ),
        (
            features,
            include_str!("../schema/onnx-feature-frames-v1.schema.json"),
        ),
    ] {
        let schema: Value = serde_json::from_str(source).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let errors: Vec<_> = validator
            .iter_errors(&instance)
            .map(|error| error.to_string())
            .collect();
        assert!(errors.is_empty(), "schema violations: {errors:#?}");
    }
}
