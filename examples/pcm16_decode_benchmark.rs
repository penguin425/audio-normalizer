use forge_normalizer::dsp::convert::decode_planar_into;
use forge_normalizer::wav::PcmKind;
use rayon::prelude::*;
use serde_json::json;
use std::time::{Duration, Instant};

const FRAMES: usize = 1 << 24;
const CHUNK_BYTES: usize = 1 << 20;

fn main() {
    let iterations = std::env::args()
        .nth(1)
        .map(|value| {
            value
                .parse::<usize>()
                .expect("iterations must be an integer")
        })
        .unwrap_or(18);
    assert!(iterations > 0);

    let mut input = Vec::with_capacity(FRAMES * 4);
    for frame in 0..FRAMES {
        let left = (frame as u32)
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223) as i16;
        let right = (frame as u32).wrapping_mul(22_695_477).wrapping_add(1) as i16;
        input.extend_from_slice(&left.to_le_bytes());
        input.extend_from_slice(&right.to_le_bytes());
    }

    let mut scalar_seconds = Vec::with_capacity(iterations);
    let mut simd_seconds = Vec::with_capacity(iterations);
    let mut checksum = None;
    for iteration in 0..iterations {
        let (scalar, simd) = if iteration % 2 == 0 {
            (measure_scalar(&input), measure_simd(&input))
        } else {
            let simd = measure_simd(&input);
            let scalar = measure_scalar(&input);
            (scalar, simd)
        };
        assert_eq!(scalar.1, simd.1);
        assert!(checksum.is_none_or(|expected| expected == scalar.1));
        checksum = Some(scalar.1);
        scalar_seconds.push(scalar.0);
        simd_seconds.push(simd.0);
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "frames": FRAMES,
            "iterations": iterations,
            "scalar_seconds": scalar_seconds,
            "simd_seconds": simd_seconds,
            "checksum": format!("{:016x}", checksum.unwrap()),
        }))
        .unwrap()
    );
}

fn measure_scalar(input: &[u8]) -> (f64, u64) {
    let mut planar = vec![Vec::new(), Vec::new()];
    let mut elapsed = Duration::ZERO;
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for chunk in input.chunks_exact(CHUNK_BYTES) {
        let frames = chunk.len() / 4;
        let started = Instant::now();
        planar
            .par_iter_mut()
            .enumerate()
            .for_each(|(channel, output)| {
                output.clear();
                output.reserve(frames);
                let mut offset = channel * 2;
                for _ in 0..frames {
                    let low = chunk[offset] as i32;
                    let high = chunk[offset + 1] as i32;
                    let sample = ((high << 8) | (low & 0xff)) as i16 as f32;
                    output.push(sample / 32_768.0);
                    offset += 4;
                }
            });
        elapsed += started.elapsed();
        hash = checksum(hash, &planar);
    }
    (elapsed.as_secs_f64(), hash)
}

fn measure_simd(input: &[u8]) -> (f64, u64) {
    let mut planar = Vec::new();
    let mut elapsed = Duration::ZERO;
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for chunk in input.chunks_exact(CHUNK_BYTES) {
        let started = Instant::now();
        decode_planar_into(chunk, PcmKind::S16, 2, &mut planar);
        elapsed += started.elapsed();
        hash = checksum(hash, &planar);
    }
    (elapsed.as_secs_f64(), hash)
}

fn checksum(mut hash: u64, planar: &[Vec<f32>]) -> u64 {
    for channel in planar {
        for sample in channel {
            hash ^= sample.to_bits() as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}
