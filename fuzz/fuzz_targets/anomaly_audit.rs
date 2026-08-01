#![no_main]

use forge_normalizer::anomaly_provider::ProviderAuditDocument;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() > 32 * 1024 * 1024 {
        return;
    }
    let Ok(document) = serde_json::from_slice::<ProviderAuditDocument>(data) else {
        return;
    };
    let _ = forge_normalizer::anomaly_provider::validate_audit(&document);
});
