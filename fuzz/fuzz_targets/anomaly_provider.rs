#![no_main]

use forge_normalizer::anomaly_provider::ProviderInput;
use libfuzzer_sys::fuzz_target;
use std::path::Path;

fuzz_target!(|data: &[u8]| {
    if data.len() > 2 * 1024 * 1024 {
        return;
    }
    let Ok(input) = serde_json::from_slice::<ProviderInput>(data) else {
        return;
    };
    let _ = forge_normalizer::anomaly_provider::audit(Path::new("fuzz-source"), input, 0.6, 0.0);
});
