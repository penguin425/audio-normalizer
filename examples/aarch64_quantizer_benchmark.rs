use forge_normalizer::dsp::convert::encode_interleaved;
use forge_normalizer::wav::PcmKind;
use serde_json::json;
use std::hint::black_box;
use std::time::Instant;

const TOTAL_SAMPLES: usize = 16 * 1024 * 1024;

fn planar_fixture(channels: usize) -> Vec<Vec<f32>> {
    let frames = TOTAL_SAMPLES / channels;
    (0..channels)
        .map(|channel| {
            (0..frames)
                .map(|frame| {
                    let phase = frame
                        .wrapping_mul(15_485_863)
                        .wrapping_add(channel.wrapping_mul(32_452_843));
                    let code = (phase & 0x00ff_ffff) as i32 - 0x0080_0000;
                    code as f32 / 8_388_608.0
                })
                .collect()
        })
        .collect()
}

fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[sorted.len() / 2]
}

fn main() {
    let iterations = std::env::args()
        .nth(1)
        .map(|value| value.parse::<usize>().expect("integer iteration count"))
        .unwrap_or(9);
    assert!(iterations > 0);

    let mut results = Vec::new();
    for (name, channels, kind) in [
        ("s16-mono", 1, PcmKind::S16),
        ("s16-stereo", 2, PcmKind::S16),
        ("s16-7.1", 8, PcmKind::S16),
        ("s24-7.1", 8, PcmKind::S24),
    ] {
        let planar = planar_fixture(channels);
        let warm = encode_interleaved(&planar, kind, false);
        let expected_checksum = checksum(&warm);
        black_box(&warm);

        let mut seconds = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let start = Instant::now();
            let encoded = encode_interleaved(&planar, kind, false);
            let elapsed = start.elapsed().as_secs_f64();
            assert_eq!(checksum(&encoded), expected_checksum);
            black_box(&encoded);
            seconds.push(elapsed);
        }
        results.push(json!({
            "id": name,
            "channels": channels,
            "kind": format!("{kind:?}"),
            "frames": planar[0].len(),
            "samples": TOTAL_SAMPLES,
            "iterations": iterations,
            "checksum": format!("{expected_checksum:016x}"),
            "median_seconds": median(&seconds),
            "seconds": seconds,
        }));
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "architecture": std::env::consts::ARCH,
            "results": results,
        }))
        .expect("serialize benchmark")
    );
}
