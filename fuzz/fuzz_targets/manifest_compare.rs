#![no_main]

use forge_normalizer::compare::{compare_manifests, CompareOptions};
use libfuzzer_sys::fuzz_target;
use serde_json::json;

fn manifest(data: &[u8], candidate: bool) -> Vec<u8> {
    let level = data.first().copied().unwrap_or_default() as f64;
    serde_json::to_vec(&json!({
        "schema": "https://penguin425.github.io/audio-normalizer/schema/delivery-manifest-v2",
        "assets": [{
            "path": String::from_utf8_lossy(data),
            "integrated_lufs": -level,
            "true_peak_dbtp": -(level / 10.0),
            "sample_rate_hz": if candidate { 44_100 } else { 48_000 },
            "compliance_passed": data.len() % 2 == 0
        }]
    }))
    .expect("JSON values are serializable")
}

fuzz_target!(|data: &[u8]| {
    let options = CompareOptions::default();
    let _ = compare_manifests(data, data, &options);
    let baseline = manifest(data, false);
    let candidate = manifest(data, true);
    let _ = compare_manifests(&baseline, &candidate, &options);
});
