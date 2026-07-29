#![no_main]

use forge_normalizer::audio_compare::{compare_paths, AudioCompareOptions};
use forge_normalizer::wav::{AudioBuffer, ChannelRole, PcmKind, WavWriter};
use libfuzzer_sys::fuzz_target;

fn samples(bytes: &[u8]) -> Vec<f32> {
    let mut result = bytes
        .chunks_exact(2)
        .take(2_048)
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]) as f32 / 32_768.0)
        .collect::<Vec<_>>();
    if result.is_empty() {
        result.push(0.0);
    }
    result
}

fn audio(data: Vec<f32>) -> AudioBuffer {
    AudioBuffer {
        sample_rate: 48_000,
        channels: 1,
        frames: data.len(),
        data: vec![data],
        channel_roles: vec![ChannelRole::Main],
        source_kind: PcmKind::F32,
    }
}

fuzz_target!(|data: &[u8]| {
    let split = data.len() / 2;
    let reference = audio(samples(&data[..split]));
    let candidate = audio(samples(&data[split..]));
    if let Ok(directory) = tempfile::tempdir() {
        let reference_path = directory.path().join("reference.wav");
        let candidate_path = directory.path().join("candidate.wav");
        if WavWriter::write(&reference_path, &reference, PcmKind::F32, false).is_ok()
            && WavWriter::write(&candidate_path, &candidate, PcmKind::F32, false).is_ok()
        {
            let options = AudioCompareOptions {
                max_input_bytes: 64 * 1024,
                max_decoded_samples: 4_096,
                alignment_search_ms: 10,
                ..AudioCompareOptions::default()
            };
            let _ = compare_paths(&reference_path, &candidate_path, &options);
        }
    }
});
