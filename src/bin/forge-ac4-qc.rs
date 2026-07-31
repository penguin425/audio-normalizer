use clap::Parser;
use forge_normalizer::ac4_adapter::{self, AdapterOptions};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "forge-ac4-qc",
    version,
    about = "Audit every AC-4 presentation through a licensed/reference decoder adapter"
)]
struct Cli {
    /// AC-4 elementary stream or container accepted by the selected adapter.
    input: PathBuf,
    /// Executable implementing Forge's AC-4 adapter protocol v1.
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
    /// Allowed measured-loudness difference from AC-4 dialnorm.
    #[arg(long, default_value_t = 1.0)]
    dialnorm_tolerance_lu: f64,
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
            eprintln!("forge-ac4-qc: error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<bool, String> {
    let report = ac4_adapter::run(&AdapterOptions {
        input: cli.input,
        adapter: cli.adapter,
        timeout_seconds: cli.timeout_seconds,
        max_decoded_samples_per_presentation: cli.max_decoded_samples,
        dialnorm_tolerance_lu: cli.dialnorm_tolerance_lu,
        max_true_peak_dbtp: cli.max_true_peak_dbtp,
    })?;
    let passed = report.passed;
    let presentation_count = report.presentation_count;
    ac4_adapter::write_report(&cli.output, &report, cli.compact, cli.overwrite)?;
    eprintln!(
        "forge-ac4-qc: {} ({presentation_count} presentations)",
        if passed { "PASS" } else { "FAIL" }
    );
    Ok(passed)
}
