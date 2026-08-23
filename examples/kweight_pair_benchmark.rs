#[allow(dead_code)]
#[path = "../src/dsp/kwfilter.rs"]
mod kwfilter;

use kwfilter::{KWeight, KWeightPair};
use serde_json::json;
use std::hint::black_box;
use std::time::Instant;

const FRAMES: usize = 1 << 24;

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

    let input = (0..FRAMES)
        .map(|frame| {
            let phase = frame as f64;
            [
                (phase * 0.011).sin() as f32 * 0.71,
                (phase * 0.017 + 0.3).sin() as f32 * 0.83,
            ]
        })
        .collect::<Vec<_>>();

    let mut scalar_seconds = Vec::with_capacity(iterations);
    let mut paired_seconds = Vec::with_capacity(iterations);
    let mut checksum = None;
    for iteration in 0..iterations {
        let (scalar, paired) = if iteration % 2 == 0 {
            (measure_scalar(&input), measure_paired(&input))
        } else {
            let paired = measure_paired(&input);
            let scalar = measure_scalar(&input);
            (scalar, paired)
        };
        assert_eq!(scalar.1, paired.1);
        assert!(checksum.is_none_or(|expected| expected == scalar.1));
        checksum = Some(scalar.1);
        scalar_seconds.push(scalar.0);
        paired_seconds.push(paired.0);
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "frames": FRAMES,
            "iterations": iterations,
            "scalar_seconds": scalar_seconds,
            "paired_seconds": paired_seconds,
            "checksum": format!("{:016x}", checksum.unwrap()),
        }))
        .unwrap()
    );
}

fn measure_scalar(input: &[[f32; 2]]) -> (f64, u64) {
    let mut left = KWeight::for_sample_rate(48_000);
    let mut right = KWeight::for_sample_rate(48_000);
    let mut sum = 0.0_f64;
    let started = Instant::now();
    for &sample in black_box(input) {
        sum += left.process(sample[0]) as f64;
        sum += right.process(sample[1]) as f64;
    }
    (started.elapsed().as_secs_f64(), black_box(sum.to_bits()))
}

fn measure_paired(input: &[[f32; 2]]) -> (f64, u64) {
    let mut filter = KWeightPair::for_sample_rate(48_000);
    let mut sum = 0.0_f64;
    let started = Instant::now();
    for &sample in black_box(input) {
        let output = filter.process(sample);
        sum += output[0] as f64;
        sum += output[1] as f64;
    }
    (started.elapsed().as_secs_f64(), black_box(sum.to_bits()))
}
