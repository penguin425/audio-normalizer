#![no_main]

use libfuzzer_sys::fuzz_target;
use std::fs;

fuzz_target!(|data: &[u8]| {
    if let Ok(directory) = tempfile::tempdir() {
        let assetmap = directory.path().join("ASSETMAP");
        if fs::write(&assetmap, data).is_ok() {
            let _ = forge_normalizer::imf_qc::audit(directory.path());
        }
    }
});
