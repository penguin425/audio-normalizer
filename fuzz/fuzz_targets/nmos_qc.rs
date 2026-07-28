#![no_main]

use libfuzzer_sys::fuzz_target;
use std::fs;

fuzz_target!(|data: &[u8]| {
    if data.len() > 256 * 1024 {
        return;
    }
    let Ok(directory) = tempfile::tempdir() else {
        return;
    };
    let path = directory.path().join("snapshot.json");
    if fs::write(&path, data).is_ok() {
        let _ = forge_normalizer::nmos_qc::audit(&path);
    }
});
