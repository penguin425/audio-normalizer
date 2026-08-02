use clap::Parser;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "forge-remediate",
    version,
    about = "Create a bounded, dry-run loudness remediation plan without writing audio"
)]
struct Cli {
    /// JSON or TOML remediation request.
    request: PathBuf,
    /// Write the JSON plan to this path instead of stdout.
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
            eprintln!("forge-remediate: error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<bool, String> {
    let report = forge_normalizer::remediation::evaluate_file(&cli.request)?;
    let mut bytes = if cli.compact {
        serde_json::to_vec(&report)
    } else {
        serde_json::to_vec_pretty(&report)
    }
    .map_err(|error| format!("serialize remediation plan: {error}"))?;
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
        "forge-remediate: {} ({} action(s))",
        if report.feasible {
            "FEASIBLE"
        } else {
            "REVIEW REQUIRED"
        },
        report.plan.actions.len()
    );
    Ok(report.feasible)
}
