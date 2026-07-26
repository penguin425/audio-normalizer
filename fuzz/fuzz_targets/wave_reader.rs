#![no_main]

use forge_normalizer::wav::WavReader;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = WavReader::read_bytes(data);

    let mut wave = match data.first().copied().unwrap_or_default() % 3 {
        0 => b"RIFF\0\0\0\0WAVE".to_vec(),
        1 => b"RF64\xff\xff\xff\xffWAVE".to_vec(),
        _ => b"BW64\xff\xff\xff\xffWAVE".to_vec(),
    };
    wave.extend_from_slice(data.get(1..).unwrap_or_default());
    let _ = WavReader::read_bytes(&wave);
});
