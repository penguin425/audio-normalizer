#![no_main]

use forge_normalizer::adm::{validate_production_profile, ProductionProfileMode};
use libfuzzer_sys::fuzz_target;
use std::io::Write;

fuzz_target!(|xml: &[u8]| {
    let Ok(size) = u32::try_from(xml.len()) else {
        return;
    };
    let padding = (size & 1) as usize;
    let riff_size = 4_u32
        .saturating_add(8)
        .saturating_add(size)
        .saturating_add(padding as u32);
    let mut wave = Vec::with_capacity(riff_size as usize + 8);
    wave.extend_from_slice(b"RIFF");
    wave.extend_from_slice(&riff_size.to_le_bytes());
    wave.extend_from_slice(b"WAVEaxml");
    wave.extend_from_slice(&size.to_le_bytes());
    wave.extend_from_slice(xml);
    wave.resize(wave.len() + padding, 0);

    if let Ok(mut file) = tempfile::NamedTempFile::new() {
        if file.write_all(&wave).is_ok() {
            let _ = validate_production_profile(file.path(), ProductionProfileMode::Read);
            let _ = validate_production_profile(file.path(), ProductionProfileMode::Write);
        }
    }
});
