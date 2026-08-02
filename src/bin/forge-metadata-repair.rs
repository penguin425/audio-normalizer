use clap::Parser;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "forge-metadata-repair",
    version,
    about = "Validate and conservatively repair BWF/ADM metadata into a separate output file"
)]
struct Cli {
    /// JSON or TOML metadata repair request.
    request: PathBuf,
    /// Write the JSON report to this path instead of stdout.
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Emit compact JSON.
    #[arg(long)]
    compact: bool,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!("forge-metadata-repair: error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<bool, String> {
    let report = forge_normalizer::metadata_repair::evaluate_file(&cli.request)?;
    let mut bytes = if cli.compact {
        serde_json::to_vec(&report)
    } else {
        serde_json::to_vec_pretty(&report)
    }
    .map_err(|error| format!("serialize metadata repair report: {error}"))?;
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
        "forge-metadata-repair: {} ({}, {} action(s))",
        if report.passed {
            "PASS"
        } else {
            "REVIEW REQUIRED"
        },
        report.source_format,
        report.actions.len()
    );
    Ok(report.passed)
}
