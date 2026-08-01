use clap::Parser;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "forge-anomaly-provider",
    version,
    about = "Validate external audio-quality anomaly model output and create an auditable report"
)]
struct Cli {
    input: PathBuf,
    #[arg(long = "confidence-threshold", default_value_t = 0.6)]
    confidence_threshold: f64,
    #[arg(long = "severity-threshold", default_value_t = 0.0)]
    severity_threshold: f64,
    #[arg(short, long)]
    output: Option<PathBuf>,
    #[arg(long)]
    compact: bool,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("forge-anomaly-provider: error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let audit = forge_normalizer::anomaly_provider::load_and_audit(
        &cli.input,
        cli.confidence_threshold,
        cli.severity_threshold,
    )?;
    let mut bytes = if cli.compact {
        serde_json::to_vec(&audit)
    } else {
        serde_json::to_vec_pretty(&audit)
    }
    .map_err(|error| format!("serialize anomaly audit: {error}"))?;
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
        "forge-anomaly-provider: selected {} of {} events ({})",
        audit.selected_event_count,
        audit.input_event_count,
        if audit.passed { "PASS" } else { "FINDINGS" }
    );
    Ok(())
}
