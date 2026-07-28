use clap::Parser;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "forge-nmos-qc",
    version,
    about = "Audit a bounded offline AMWA NMOS IS-04/IS-05 snapshot"
)]
struct Cli {
    /// Snapshot bundle JSON or directory of resource collection JSON files
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
            eprintln!("forge-nmos-qc: error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<bool, String> {
    let audit = forge_normalizer::nmos_qc::audit(&cli.input)?;
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
        "forge-nmos-qc: {} ({} warning(s))",
        if passed { "PASS" } else { "FAIL" },
        warning_count
    );
    Ok(passed)
}
