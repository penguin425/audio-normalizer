#![no_main]

use forge_normalizer::dash_qc::{self, DashProfile};
use libfuzzer_sys::fuzz_target;
use std::io::Write;

fuzz_target!(|data: &[u8]| {
    if let Ok(mut file) = tempfile::NamedTempFile::new() {
        if file.write_all(data).is_ok() {
            let _ = dash_qc::audit(file.path(), DashProfile::Iso23009);
            let _ = dash_qc::audit(file.path(), DashProfile::DashIfIop);
        }
    }
});
