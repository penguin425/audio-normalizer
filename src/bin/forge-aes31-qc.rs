use clap::Parser;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "forge-aes31-qc",
    version,
    about = "Audit an AES31-3 EDML Audio Decision List"
)]
struct Cli {
    /// AES31-3 EDML .adl project
    input: PathBuf,
    /// Write the JSON audit to this file instead of stdout
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Emit compact JSON
    #[arg(long)]
    compact: bool,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!("forge-aes31-qc: error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<bool, String> {
    let audit = forge_normalizer::aes31_qc::audit(&cli.input)?;
    let passed = audit.passed;
    let warning_count = audit.warning_count;
    let mut bytes = if cli.compact {
        serde_json::to_vec(&audit)
    } else {
        serde_json::to_vec_pretty(&audit)
    }
    .map_err(|error| format!("serialize audit: {error}"))?;
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
        "forge-aes31-qc: {} ({} warning(s))",
        if passed { "PASS" } else { "FAIL" },
        warning_count
    );
    Ok(passed)
}
