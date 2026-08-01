#![no_main]

use forge_normalizer::onnx_provider::{FeatureFrames, OnnxModelManifest};
use libfuzzer_sys::fuzz_target;

fn reference_manifest() -> OnnxModelManifest {
    serde_json::from_str(
        r#"{
          "schema":"https://penguin425.github.io/audio-normalizer/schema/onnx-anomaly-model-v1",
          "schema_version":1,
          "adapter":"forge-onnx-provider-1",
          "provider":"fuzz",
          "provider_version":"1",
          "model":"fuzz-model",
          "model_version":"1",
          "model_file":"fuzz.onnx",
          "model_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
          "license":{"spdx":"MIT","url":"https://example.test/model","dataset":"fuzz","dataset_url":"https://example.test/dataset"},
          "calibration":{"report_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","report_url":"https://example.test/calibration"},
          "input":{"name":"features","shape":[1,-1,2]},
          "output":{"name":"scores","shape":[1,-1,2]},
          "classes":[{"kind":"pop","confidence_threshold":0.6,"severity_threshold":0.5}],
          "frame_hop_seconds":0.1,
          "limits":{"max_model_bytes":1048576,"max_feature_frames":100,"max_feature_values":1000,"max_events":100,"max_inference_seconds":1.0,"intra_threads":1},
          "fallback":"reject"
        }"#,
    )
    .expect("static fuzz manifest")
}

fuzz_target!(|data: &[u8]| {
    if data.len() > 32 * 1024 * 1024 {
        return;
    }
    if let Ok(manifest) = serde_json::from_slice::<OnnxModelManifest>(data) {
        let _ = forge_normalizer::onnx_provider::validate_manifest(&manifest);
    }
    if let Ok(features) = serde_json::from_slice::<FeatureFrames>(data) {
        let _ =
            forge_normalizer::onnx_provider::validate_features(&features, &reference_manifest());
    }
});
