use clap::{Parser, ValueEnum};
use forge_normalizer::compare::{self, CompareOptions};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "forge-compare",
    version,
    about = "Compare Forge delivery manifests as a CI quality gate"
)]
struct Cli {
    baseline: PathBuf,
    candidate: PathBuf,
    #[arg(short, long)]
    output: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "json")]
    format: OutputFormat,
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    loudness_tolerance_lu: Option<f64>,
    #[arg(long)]
    true_peak_tolerance_db: Option<f64>,
    #[arg(long)]
    loudness_range_tolerance_lu: Option<f64>,
    #[arg(long)]
    duration_tolerance_seconds: Option<f64>,
    #[arg(long)]
    allow_missing_metrics: bool,
    #[arg(long)]
    reject_new_assets: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum OutputFormat {
    Json,
    Junit,
    Sarif,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!("forge-compare: error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<bool, String> {
    let baseline = fs::read(&cli.baseline)
        .map_err(|error| format!("read {}: {error}", cli.baseline.display()))?;
    let candidate = fs::read(&cli.candidate)
        .map_err(|error| format!("read {}: {error}", cli.candidate.display()))?;
    let mut options = cli
        .config
        .as_deref()
        .map(read_options)
        .transpose()?
        .unwrap_or_default();
    if let Some(value) = cli.loudness_tolerance_lu {
        options.loudness_tolerance_lu = value;
    }
    if let Some(value) = cli.true_peak_tolerance_db {
        options.true_peak_tolerance_db = value;
    }
    if let Some(value) = cli.loudness_range_tolerance_lu {
        options.loudness_range_tolerance_lu = value;
    }
    if let Some(value) = cli.duration_tolerance_seconds {
        options.duration_tolerance_seconds = value;
    }
    if cli.allow_missing_metrics {
        options.allow_missing_metrics = true;
    }
    if cli.reject_new_assets {
        options.allow_new_assets = false;
    }

    let result = compare::compare_manifests(&baseline, &candidate, &options)?;
    let mut bytes = Vec::new();
    match cli.format {
        OutputFormat::Json => compare::write_json(&mut bytes, &result)?,
        OutputFormat::Junit => compare::write_junit(&mut bytes, &result)?,
        OutputFormat::Sarif => compare::write_sarif(&mut bytes, &result)?,
    }
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
        "forge-compare: {} ({} errors, {} warnings, {} assets)",
        if result.passed { "PASS" } else { "FAIL" },
        result.error_count,
        result.warning_count,
        result.compared_assets
    );
    Ok(result.passed)
}

fn read_options(path: &Path) -> Result<CompareOptions, String> {
    let text =
        fs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    match path.extension().and_then(|value| value.to_str()) {
        Some("json") => serde_json::from_str(&text)
            .map_err(|error| format!("decode {}: {error}", path.display())),
        Some("toml") => {
            toml::from_str(&text).map_err(|error| format!("decode {}: {error}", path.display()))
        }
        _ => Err("comparison config must use a .json or .toml extension".into()),
    }
}
