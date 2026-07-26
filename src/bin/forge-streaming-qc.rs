use clap::{Parser, ValueEnum};
use forge_normalizer::hls_qc::{self, HlsProfile};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Clone, Copy, ValueEnum)]
enum Profile {
    Rfc8216,
    AppleHls,
}

impl From<Profile> for HlsProfile {
    fn from(value: Profile) -> Self {
        match value {
            Profile::Rfc8216 => Self::Rfc8216,
            Profile::AppleHls => Self::AppleHls,
        }
    }
}

#[derive(Parser)]
#[command(
    name = "forge-streaming-qc",
    version,
    about = "Audit HLS playlists and local CMAF/fMP4 package assets"
)]
struct Cli {
    input: PathBuf,
    #[arg(long, value_enum, default_value = "rfc8216")]
    profile: Profile,
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
            eprintln!("forge-streaming-qc: error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<bool, String> {
    let audit = hls_qc::audit(&cli.input, cli.profile.into())?;
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
        "forge-streaming-qc: {} ({} warning(s))",
        if audit.passed { "PASS" } else { "FAIL" },
        audit.warning_count
    );
    Ok(audit.passed)
}
