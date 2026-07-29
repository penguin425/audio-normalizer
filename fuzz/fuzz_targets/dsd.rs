#![no_main]

use forge_normalizer::{container_qc, dsd};
use libfuzzer_sys::fuzz_target;
use std::io::Write;

fn exercise(bytes: &[u8], extension: &str) {
    if let Ok(mut file) = tempfile::Builder::new().suffix(extension).tempfile() {
        if file.write_all(bytes).is_ok() {
            let _ = dsd::probe(file.path());
            let _ = container_qc::audit(file.path());
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let mut dsf = data.iter().copied().take(64 * 1024).collect::<Vec<_>>();
    dsf.resize(dsf.len().max(4), 0);
    dsf[..4].copy_from_slice(b"DSD ");
    exercise(&dsf, ".dsf");

    let mut dff = data.iter().copied().take(64 * 1024).collect::<Vec<_>>();
    dff.resize(dff.len().max(16), 0);
    dff[..4].copy_from_slice(b"FRM8");
    dff[12..16].copy_from_slice(b"DSD ");
    exercise(&dff, ".dff");
});
