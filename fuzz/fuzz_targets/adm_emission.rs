#![no_main]

use forge_normalizer::adm::emission::{self, Level, Options};
use libfuzzer_sys::fuzz_target;
use std::io::Write;

const MAX_FUZZ_BYTES: usize = 2_048;

fn append_chunk(wave: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
    wave.extend_from_slice(id);
    wave.extend_from_slice(&(body.len() as u32).to_le_bytes());
    wave.extend_from_slice(body);
    if !body.len().is_multiple_of(2) {
        wave.push(0);
    }
}

fn build_wave(data: &[u8]) -> Vec<u8> {
    let control = data.first().copied().unwrap_or_default();
    let fuzz = data.get(1..).unwrap_or_default();
    let fuzz = &fuzz[..fuzz.len().min(MAX_FUZZ_BYTES)];
    let bw64 = control & 1 != 0;
    let fuzz_axml = control & 2 != 0;

    let mut wave = if bw64 {
        b"BW64\xff\xff\xff\xffWAVE".to_vec()
    } else {
        b"RIFF\0\0\0\0WAVE".to_vec()
    };
    if bw64 {
        append_chunk(&mut wave, b"ds64", &[0; 28]);
    }
    append_chunk(
        &mut wave,
        b"fmt ",
        &[1, 0, 1, 0, 0x80, 0xbb, 0, 0, 0, 0x77, 1, 0, 2, 0, 16, 0],
    );
    let baseline_axml = b"<audioFormatExtended version=\"ITU-R_BS.2076-3\"/>";
    let baseline_chna = [0_u8; 4];
    append_chunk(
        &mut wave,
        b"axml",
        if fuzz_axml { fuzz } else { baseline_axml },
    );
    append_chunk(
        &mut wave,
        b"chna",
        if fuzz_axml { &baseline_chna } else { fuzz },
    );
    append_chunk(&mut wave, b"data", &[0, 0]);

    let riff_size = (wave.len() as u64).saturating_sub(8);
    if bw64 {
        wave[20..28].copy_from_slice(&riff_size.to_le_bytes());
        wave[28..36].copy_from_slice(&2_u64.to_le_bytes());
        wave[36..44].copy_from_slice(&1_u64.to_le_bytes());
    } else {
        wave[4..8].copy_from_slice(&(riff_size as u32).to_le_bytes());
    }
    wave
}

fuzz_target!(|data: &[u8]| {
    let wave = build_wave(data);
    if let Ok(mut file) = tempfile::NamedTempFile::new() {
        if file.write_all(&wave).is_ok() {
            let mut options = Options::new(file.path(), Level::Level1);
            options.max_axml_bytes = 4_096;
            options.max_chna_bytes = 4_096;
            options.max_xml_nodes = 128;
            options.max_xml_depth = 16;
            options.max_attributes_per_element = 32;
            options.max_xml_text_bytes = 4_096;
            options.max_report_items = 12;
            options.max_evidence_items = 4;
            let _ = emission::validate(&options);
        }
    }
});
