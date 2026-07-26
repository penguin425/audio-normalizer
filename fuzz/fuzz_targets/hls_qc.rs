#![no_main]

use forge_normalizer::hls_qc::{self, HlsProfile};
use libfuzzer_sys::fuzz_target;
use std::io::Write;

fuzz_target!(|data: &[u8]| {
    if let Ok(mut file) = tempfile::NamedTempFile::new() {
        if file.write_all(data).is_ok() {
            let _ = hls_qc::audit(file.path(), HlsProfile::Rfc8216);
            let _ = hls_qc::audit(file.path(), HlsProfile::AppleHls);
        }
    }
});
