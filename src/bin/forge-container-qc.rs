use clap::Parser;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "forge-container-qc",
    version,
    about = "Audit WAVE, AIFF/AIFC, CAF, AU, DSF/DSDIFF, WavPack, Monkey's Audio, FLAC, MP3, AAC, MPEG-TS/M2TS, MXF, AAF, Ogg, Matroska/WebM, and ISO-BMFF delivery containers"
)]
struct Cli {
    input: PathBuf,
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
            eprintln!("forge-container-qc: error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<bool, String> {
    let audit = forge_normalizer::container_qc::audit(&cli.input)?;
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
        "forge-container-qc: {} ({})",
        if audit.passed { "PASS" } else { "FAIL" },
        audit.format
    );
    Ok(audit.passed)
}
