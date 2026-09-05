use clap::Parser;
use forge_normalizer::dts_adapter::{self, AdapterOptions};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "forge-dts-qc",
    version,
    about = "Audit raw DTS core/HD framing and every decoded presentation"
)]
struct Cli {
    /// Raw DTS core or DTS-HD elementary stream.
    input: PathBuf,
    /// Executable implementing Forge's DTS adapter protocol v1.
    #[arg(long)]
    adapter: PathBuf,
    /// Atomic JSON evidence report.
    #[arg(short, long)]
    output: PathBuf,
    /// Whole adapter-process timeout.
    #[arg(long, default_value_t = 300)]
    timeout_seconds: u64,
    /// Maximum decoded interleaved PCM samples accepted per presentation.
    #[arg(long, default_value_t = 50_000_000)]
    max_decoded_samples: u64,
    /// Optional decoded true-peak ceiling.
    #[arg(long)]
    max_true_peak_dbtp: Option<f64>,
    /// Replace an existing report.
    #[arg(long)]
    overwrite: bool,
    /// Emit compact rather than pretty JSON.
    #[arg(long)]
    compact: bool,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!("forge-dts-qc: error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<bool, String> {
    let report = dts_adapter::run_v2(&AdapterOptions {
        input: cli.input,
        adapter: cli.adapter,
        timeout_seconds: cli.timeout_seconds,
        max_decoded_samples_per_presentation: cli.max_decoded_samples,
        max_true_peak_dbtp: cli.max_true_peak_dbtp,
    })?;
    let passed = report.passed;
    let count = report.presentation_count;
    dts_adapter::write_report_v2(&cli.output, &report, cli.compact, cli.overwrite)?;
    eprintln!(
        "forge-dts-qc: {} ({count} presentations)",
        if passed { "PASS" } else { "FAIL" }
    );
    Ok(passed)
}
