#![no_main]

use forge_normalizer::container_qc;
use libfuzzer_sys::fuzz_target;
use std::io::Write;

fn audit_bytes(bytes: &[u8]) {
    if let Ok(mut file) = tempfile::NamedTempFile::new() {
        if file.write_all(bytes).is_ok() {
            let _ = container_qc::audit(file.path());
        }
    }
}

fuzz_target!(|data: &[u8]| {
    audit_bytes(data);

    let mut container = match data.first().copied().unwrap_or_default() % 4 {
        0 => b"RIFF\0\0\0\0WAVE".to_vec(),
        1 => b"RF64\xff\xff\xff\xffWAVE".to_vec(),
        2 => b"BW64\xff\xff\xff\xffWAVE".to_vec(),
        _ => b"OggS".to_vec(),
    };
    container.extend_from_slice(data.get(1..).unwrap_or_default());
    audit_bytes(&container);
});
