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
    name = "forge-st2022-7-qc",
    version,
    about = "Audit two offline RTP legs for SMPTE ST 2022-7 protection"
)]
struct Cli {
    /// SDP describing the primary RTP leg
    primary_sdp: PathBuf,
    /// PCAP or PCAPNG containing the primary RTP leg
    primary_capture: PathBuf,
    /// SDP describing the secondary RTP leg
    secondary_sdp: PathBuf,
    /// PCAP or PCAPNG containing the secondary RTP leg
    secondary_capture: PathBuf,
    /// RTP audio interoperability profile
    #[arg(long, value_enum, default_value = "smpte2110-30")]
    profile: Profile,
    /// Fail when maximum matching-packet arrival skew exceeds this value
    #[arg(long, value_name = "MILLISECONDS")]
    max_skew_ms: Option<f64>,
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
            eprintln!("forge-st2022-7-qc: error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<bool, String> {
    if cli
        .max_skew_ms
        .is_some_and(|value| !value.is_finite() || value < 0.0)
    {
        return Err("--max-skew-ms must be a finite, non-negative number".into());
    }
    let audit = rtp_qc::audit_st2022_7(
        &cli.primary_sdp,
        &cli.primary_capture,
        &cli.secondary_sdp,
        &cli.secondary_capture,
        cli.profile.into(),
        cli.max_skew_ms,
    )?;
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
        "forge-st2022-7-qc: {} ({} warning(s))",
        if passed { "PASS" } else { "FAIL" },
        warning_count
    );
    Ok(passed)
}
