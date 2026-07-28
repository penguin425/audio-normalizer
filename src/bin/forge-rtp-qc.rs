use clap::{Parser, ValueEnum};
use forge_normalizer::rtp_qc::{self, RtpAudioProfile};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Clone, Copy, ValueEnum)]
enum Profile {
    Rfc3550,
    Aes67,
    Smpte2110_30,
    Smpte2110_31,
}

impl From<Profile> for RtpAudioProfile {
    fn from(value: Profile) -> Self {
        match value {
            Profile::Rfc3550 => Self::Rfc3550,
            Profile::Aes67 => Self::Aes67,
            Profile::Smpte2110_30 => Self::Smpte2110_30,
            Profile::Smpte2110_31 => Self::Smpte2110_31,
        }
    }
}

#[derive(Parser)]
#[command(
    name = "forge-rtp-qc",
    version,
    about = "Audit an RTP audio SDP and optional offline PCAP/PCAPNG capture"
)]
struct Cli {
    /// Session Description Protocol file
    sdp: PathBuf,
    /// Optional PCAP or PCAPNG containing the described RTP flow
    capture: Option<PathBuf>,
    /// RTP audio interoperability profile
    #[arg(long, value_enum, default_value = "aes67")]
    profile: Profile,
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
            eprintln!("forge-rtp-qc: error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<bool, String> {
    let audit = rtp_qc::audit(&cli.sdp, cli.capture.as_deref(), cli.profile.into())?;
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
        "forge-rtp-qc: {} ({} warning(s))",
        if passed { "PASS" } else { "FAIL" },
        warning_count
    );
    Ok(passed)
}
