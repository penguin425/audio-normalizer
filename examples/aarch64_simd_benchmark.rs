#[allow(dead_code)]
#[path = "../src/dsp/simd.rs"]
mod simd;

use serde_json::json;
use std::hint::black_box;
use std::time::Instant;

const SAMPLES: usize = 1 << 24;

fn main() {
    let iterations = std::env::args()
        .nth(1)
        .map(|value| {
            value
                .parse::<usize>()
                .expect("iterations must be an integer")
        })
        .unwrap_or(9);
    assert!(iterations > 0);

    let source = (0..SAMPLES)
        .map(|index| {
            let code = ((index.wrapping_mul(97) % 20_000) as i32) - 10_000;
            code as f32 / 8_192.0
        })
        .collect::<Vec<_>>();

    let results = [
        measure_mutating("apply-gain", &source, iterations, |samples| {
            simd::apply_gain(samples, 0.812_345_7);
        }),
        measure_mutating("gain-hard-clip", &source, iterations, |samples| {
            simd::apply_gain_and_hard_clip(samples, 0.812_345_7, 0.891_250_9);
        }),
        measure_mutating("hard-clip", &source, iterations, |samples| {
            simd::hard_clip(samples, 0.891_250_9);
        }),
        measure_reduction("abs-max", &source, iterations, |samples| {
            u64::from(simd::abs_max(samples).to_bits())
        }),
        measure_reduction("abs-max-nan", &source, iterations, |samples| {
            let (maximum, has_nan) = simd::abs_max_and_has_nan(samples);
            u64::from(maximum.to_bits()) | (u64::from(has_nan) << 32)
        }),
    ];

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "samples": SAMPLES,
            "iterations": iterations,
            "results": results,
        }))
        .unwrap()
    );
}

fn measure_mutating(
    id: &str,
    source: &[f32],
    iterations: usize,
    operation: impl Fn(&mut [f32]),
) -> serde_json::Value {
    let mut seconds = Vec::with_capacity(iterations);
    let mut checksum = None;
    for _ in 0..iterations {
        let mut working = source.to_vec();
        let started = Instant::now();
        operation(black_box(&mut working));
        seconds.push(started.elapsed().as_secs_f64());
        let current = hash_samples(&working);
        assert!(checksum.is_none_or(|expected| expected == current));
        checksum = Some(current);
    }
    json!({
        "id": id,
        "median_seconds": median(&seconds),
        "seconds": seconds,
        "checksum": format!("{:016x}", checksum.unwrap()),
    })
}

fn measure_reduction(
    id: &str,
    source: &[f32],
    iterations: usize,
    operation: impl Fn(&[f32]) -> u64,
) -> serde_json::Value {
    let mut seconds = Vec::with_capacity(iterations);
    let mut checksum = None;
    for _ in 0..iterations {
        let started = Instant::now();
        let current = operation(black_box(source));
        seconds.push(started.elapsed().as_secs_f64());
        assert!(checksum.is_none_or(|expected| expected == current));
        checksum = Some(current);
    }
    json!({
        "id": id,
        "median_seconds": median(&seconds),
        "seconds": seconds,
        "checksum": format!("{:016x}", checksum.unwrap()),
    })
}

fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[sorted.len() / 2]
}

fn hash_samples(samples: &[f32]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for sample in samples {
        for byte in sample.to_bits().to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x1000_0000_01b3);
        }
    }
    hash
}
