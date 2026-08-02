use clap::Parser;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "forge-binaural-qc",
    version,
    about = "Verify externally rendered binaural output with renderer/model evidence"
)]
struct Cli {
    spec: PathBuf,
    #[arg(short, long)]
    output: Option<PathBuf>,
    #[arg(long)]
    compact: bool,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!("forge-binaural-qc: error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<bool, String> {
    let report = forge_normalizer::binaural_qc::evaluate_file(&cli.spec)?;
    let mut bytes = if cli.compact {
        serde_json::to_vec(&report)
    } else {
        serde_json::to_vec_pretty(&report)
    }
    .map_err(|error| format!("serialize binaural QC: {error}"))?;
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
        "forge-binaural-qc: {} (renderer {})",
        if report.passed { "PASS" } else { "FAIL" },
        report.renderer.name
    );
    Ok(report.passed)
}
