use clap::Parser;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "forge-sadm-qc",
    version,
    about = "Audit an ordered ITU-R BS.2125-1 S-ADM XML frame flow"
)]
struct Cli {
    #[arg(required = true)]
    frames: Vec<PathBuf>,
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
            eprintln!("forge-sadm-qc: error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<bool, String> {
    let audit = forge_normalizer::sadm::audit(&cli.frames)?;
    let mut bytes = if cli.compact {
        serde_json::to_vec(&audit)
    } else {
        serde_json::to_vec_pretty(&audit)
    }
    .map_err(|error| format!("serialize S-ADM audit: {error}"))?;
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
        "forge-sadm-qc: {} ({} frames)",
        if audit.passed { "PASS" } else { "FAIL" },
        audit.frame_count
    );
    Ok(audit.passed)
}
