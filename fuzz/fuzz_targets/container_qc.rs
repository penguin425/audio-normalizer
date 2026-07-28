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

fn append_wave_chunk(wave: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
    wave.extend_from_slice(id);
    wave.extend_from_slice(&(body.len() as u32).to_le_bytes());
    wave.extend_from_slice(body);
    if body.len() % 2 != 0 {
        wave.push(0);
    }
}

fn wave_with_xml(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut wave = b"RIFF\0\0\0\0WAVE".to_vec();
    append_wave_chunk(
        &mut wave,
        b"fmt ",
        &[1, 0, 1, 0, 0x80, 0xbb, 0, 0, 0, 0x77, 1, 0, 2, 0, 16, 0],
    );
    append_wave_chunk(&mut wave, id, body);
    append_wave_chunk(&mut wave, b"data", &[]);
    let riff_size = (wave.len() as u32).saturating_sub(8);
    wave[4..8].copy_from_slice(&riff_size.to_le_bytes());
    wave
}

fuzz_target!(|data: &[u8]| {
    audit_bytes(data);
    let xml_id = match data.first().copied().unwrap_or_default() % 3 {
        0 => b"axml",
        1 => b"bxml",
        _ => b"sxml",
    };
    audit_bytes(&wave_with_xml(
        xml_id,
        data.get(1..).unwrap_or_default(),
    ));

    let mut container = match data.first().copied().unwrap_or_default() % 15 {
        0 => b"RIFF\0\0\0\0WAVE".to_vec(),
        1 => b"RF64\xff\xff\xff\xffWAVE".to_vec(),
        2 => b"BW64\xff\xff\xff\xffWAVE".to_vec(),
        3 => b"OggS".to_vec(),
        4 => b"FORM\0\0\0\0AIFF".to_vec(),
        5 => b"caff\0\x01\0\0".to_vec(),
        6 => b".snd".to_vec(),
        7 => b"fLaC".to_vec(),
        8 => b"\xff\xf1\x4c\x80\x00\xff\xfc".to_vec(),
        9 => b"\x56\xe0\x00".to_vec(),
        10 => b"\x0b\x77\0\0\x14\x40\x2c\x04".to_vec(),
        11 => b"\xf8\x06iamf\0\0".to_vec(),
        12 => b"\x1a\x45\xdf\xa3".to_vec(),
        13 => b"\x47\x40\x00\x10".to_vec(),
        _ => b"\x06\x0e\x2b\x34\x02\x05\x01\x01\x0d\x01\x02\x01\x01\x02\x04\x00".to_vec(),
    };
    container.extend_from_slice(data.get(1..).unwrap_or_default());
    audit_bytes(&container);
});
