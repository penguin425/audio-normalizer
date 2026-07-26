//! Streaming f32le command-line surface for Forge's real-time DSP.

use clap::Parser;
use forge_normalizer::realtime::{
    RealtimeGainConfig, RealtimeGainProcessor, RealtimeMeasurement, RealtimeMeter,
};
use forge_normalizer::wav::default_channel_roles;
use serde::Serialize;
use std::io::{self, Read, Write};
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(
    name = "forge-live",
    version,
    about = "Low-latency f32le loudness meter, gain smoother, and true-peak limiter"
)]
struct Args {
    /// Interleaved stream sample rate.
    #[arg(long, default_value_t = 48_000, value_parser = clap::value_parser!(u32).range(1..))]
    sample_rate: u32,

    /// Interleaved stream channel count in WAVE order.
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u16).range(1..=16))]
    channels: u16,

    /// Target gain applied with attack/release smoothing.
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    gain_db: f64,

    /// True-peak limiter ceiling.
    #[arg(long, default_value_t = -1.0, allow_hyphen_values = true)]
    ceiling_dbtp: f64,

    #[arg(long, default_value_t = 10.0)]
    attack_ms: f64,

    #[arg(long, default_value_t = 100.0)]
    release_ms: f64,

    /// Processing block size in frames.
    #[arg(long, default_value_t = 256, value_parser = clap::value_parser!(u32).range(1..))]
    block_frames: u32,

    /// Emit a measurement NDJSON object to stderr at this interval.
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u64).range(1..))]
    meter_interval_ms: u64,
}

#[derive(Serialize)]
struct LiveReport {
    schema: &'static str,
    sample_rate_hz: u32,
    channels: u16,
    latency_frames: usize,
    current_gain_db: f64,
    max_limiter_reduction_db: f64,
    frames: u64,
    momentary_lufs: Option<f64>,
    short_term_lufs: Option<f64>,
    sample_peak_dbfs: Option<f64>,
    true_peak_dbtp: Option<f64>,
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("forge-live: error: {error}");
            ExitCode::from(1)
        }
    }
}

fn run(args: Args) -> Result<(), String> {
    let channels = usize::from(args.channels);
    let mut processor = RealtimeGainProcessor::new(
        args.sample_rate,
        channels,
        RealtimeGainConfig {
            initial_gain_db: args.gain_db,
            ceiling_dbfs: args.ceiling_dbtp,
            attack_ms: args.attack_ms,
            release_ms: args.release_ms,
        },
    )?;
    let mut meter = RealtimeMeter::new(
        args.sample_rate,
        default_channel_roles(args.channels)
            .into_iter()
            .take(channels)
            .collect(),
    )?;
    let block_frames =
        usize::try_from(args.block_frames).map_err(|_| "block size is too large".to_string())?;
    let block_bytes = block_frames
        .checked_mul(channels)
        .and_then(|samples| samples.checked_mul(4))
        .ok_or_else(|| "block size is too large".to_string())?;
    let mut bytes = vec![0_u8; block_bytes];
    let mut samples = vec![0_f32; block_frames * channels];
    let report_interval_frames =
        (u64::from(args.sample_rate) * args.meter_interval_ms / 1_000).max(1);
    let mut next_report = report_interval_frames;
    let stdin = io::stdin();
    let stdout = io::stdout();
    let stderr = io::stderr();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    let mut diagnostics = stderr.lock();
    let mut filled = 0;

    loop {
        let read = input
            .read(&mut bytes[filled..])
            .map_err(|error| format!("read stdin: {error}"))?;
        if read == 0 && filled == 0 {
            break;
        }
        filled += read;
        if filled < bytes.len() && read != 0 {
            continue;
        }
        if filled % (channels * 4) != 0 {
            if read == 0 {
                return Err("input ended in the middle of an interleaved f32le frame".into());
            }
            continue;
        }
        let sample_count = filled / 4;
        for (sample, encoded) in samples[..sample_count]
            .iter_mut()
            .zip(bytes[..filled].chunks_exact(4))
        {
            *sample = f32::from_le_bytes(encoded.try_into().expect("four-byte PCM sample"));
        }
        let block = &mut samples[..sample_count];
        processor.process_interleaved(block)?;
        meter.process_interleaved(block)?;
        for (sample, encoded) in block.iter().zip(bytes[..filled].chunks_exact_mut(4)) {
            encoded.copy_from_slice(&sample.to_le_bytes());
        }
        output
            .write_all(&bytes[..filled])
            .map_err(|error| format!("write stdout: {error}"))?;
        let measurement = meter.measurement();
        if measurement.frames >= next_report || read == 0 {
            write_report(&mut diagnostics, &args, &processor, measurement)?;
            next_report = measurement.frames.saturating_add(report_interval_frames);
        }
        filled = 0;
        if read == 0 {
            break;
        }
    }
    output
        .flush()
        .map_err(|error| format!("flush stdout: {error}"))
}

fn write_report(
    writer: &mut impl Write,
    args: &Args,
    processor: &RealtimeGainProcessor,
    measurement: RealtimeMeasurement,
) -> Result<(), String> {
    let finite = |value: f64| value.is_finite().then_some(value);
    let report = LiveReport {
        schema: "forge-live-v1",
        sample_rate_hz: args.sample_rate,
        channels: args.channels,
        latency_frames: processor.latency_frames(),
        current_gain_db: processor.current_gain_db(),
        max_limiter_reduction_db: processor.max_reduction_db(),
        frames: measurement.frames,
        momentary_lufs: finite(measurement.momentary_lufs),
        short_term_lufs: finite(measurement.short_term_lufs),
        sample_peak_dbfs: finite(measurement.sample_peak_dbfs),
        true_peak_dbtp: finite(measurement.true_peak_dbtp),
    };
    serde_json::to_writer(&mut *writer, &report)
        .map_err(|error| format!("write meter report: {error}"))?;
    writeln!(writer).map_err(|error| format!("write meter report: {error}"))
}
