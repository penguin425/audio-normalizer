use clap::Parser;
use forge_normalizer::audio_compare::{self, AudioCompareOptions};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "forge-audio-compare",
    version,
    about = "Align and compare decoded candidate audio against a reference"
)]
struct Cli {
    reference: PathBuf,
    candidate: PathBuf,
    #[arg(short, long)]
    output: Option<PathBuf>,
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    alignment_search_ms: Option<u64>,
    #[arg(long)]
    max_offset_samples: Option<u64>,
    #[arg(long)]
    duration_tolerance_samples: Option<u64>,
    #[arg(long)]
    min_alignment_correlation: Option<f64>,
    #[arg(long)]
    min_channel_correlation: Option<f64>,
    #[arg(long)]
    min_null_depth_db: Option<f64>,
    #[arg(long)]
    max_residual_peak_dbfs: Option<f64>,
    #[arg(long)]
    max_spectral_error_db: Option<f64>,
    #[arg(long)]
    max_input_bytes: Option<u64>,
    #[arg(long)]
    max_decoded_samples: Option<u64>,
    #[arg(long)]
    allow_channel_permutation: bool,
    #[arg(long)]
    allow_polarity_inversion: bool,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!("forge-audio-compare: error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<bool, String> {
    let mut options = cli
        .config
        .as_deref()
        .map(read_options)
        .transpose()?
        .unwrap_or_default();
    assign(&mut options.alignment_search_ms, cli.alignment_search_ms);
    assign(&mut options.max_offset_samples, cli.max_offset_samples);
    assign(
        &mut options.duration_tolerance_samples,
        cli.duration_tolerance_samples,
    );
    assign(
        &mut options.min_alignment_correlation,
        cli.min_alignment_correlation,
    );
    assign(
        &mut options.min_channel_correlation,
        cli.min_channel_correlation,
    );
    assign(&mut options.min_null_depth_db, cli.min_null_depth_db);
    assign(
        &mut options.max_residual_peak_dbfs,
        cli.max_residual_peak_dbfs,
    );
    assign(
        &mut options.max_spectral_error_db,
        cli.max_spectral_error_db,
    );
    assign(&mut options.max_input_bytes, cli.max_input_bytes);
    assign(&mut options.max_decoded_samples, cli.max_decoded_samples);
    if cli.allow_channel_permutation {
        options.allow_channel_permutation = true;
    }
    if cli.allow_polarity_inversion {
        options.allow_polarity_inversion = true;
    }

    let result = audio_compare::compare_paths(&cli.reference, &cli.candidate, &options)?;
    let mut bytes = serde_json::to_vec_pretty(&result)
        .map_err(|error| format!("serialize comparison: {error}"))?;
    bytes.push(b'\n');
    if let Some(path) = &cli.output {
        fs::write(path, bytes).map_err(|error| format!("write {}: {error}", path.display()))?;
    } else {
        io::stdout()
            .lock()
            .write_all(&bytes)
            .map_err(|error| format!("write stdout: {error}"))?;
    }
    eprintln!(
        "forge-audio-compare: {} ({} errors, {} aligned frames)",
        if result.passed { "PASS" } else { "FAIL" },
        result.error_count,
        result
            .alignment
            .as_ref()
            .map_or(0, |alignment| alignment.compared_frames)
    );
    Ok(result.passed)
}

fn assign<T>(target: &mut T, value: Option<T>) {
    if let Some(value) = value {
        *target = value;
    }
}

fn read_options(path: &Path) -> Result<AudioCompareOptions, String> {
    let text =
        fs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    match path.extension().and_then(|value| value.to_str()) {
        Some("json") => serde_json::from_str(&text)
            .map_err(|error| format!("decode {}: {error}", path.display())),
        Some("toml") => {
            toml::from_str(&text).map_err(|error| format!("decode {}: {error}", path.display()))
        }
        _ => Err("audio comparison config must use a .json or .toml extension".into()),
    }
}
