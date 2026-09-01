use clap::{Parser, ValueEnum};
use forge_normalizer::atsc_a85_service;
use forge_normalizer::dash_observe::{self, DashObservationOptions};
use forge_normalizer::dash_qc::{self, DashProfile};
use forge_normalizer::hls_qc::{self, HlsProfile};
use serde::Serialize;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Clone, Copy, ValueEnum)]
enum Profile {
    Rfc8216,
    AppleHls,
    LlHls,
    Iso23009,
    DashIfIop,
    DashLive,
    AtscA85Service,
}

#[derive(Parser)]
#[command(
    name = "forge-streaming-qc",
    version,
    about = "Audit HLS/DASH packages or an ATSC A/85 streaming-service asset plan"
)]
struct Cli {
    input: PathBuf,
    #[arg(long, value_enum)]
    profile: Option<Profile>,
    #[arg(short, long)]
    output: Option<PathBuf>,
    #[arg(long)]
    compact: bool,
    /// Compare this DASH MPD with the preceding full MPD snapshot.
    #[arg(long, value_name = "MPD", conflicts_with = "mpd_patch")]
    previous_mpd: Option<PathBuf>,
    /// Apply and audit an MPD Patch against the input DASH MPD.
    #[arg(long, value_name = "MPP", conflicts_with = "previous_mpd")]
    mpd_patch: Option<PathBuf>,
    /// Perform bounded one-shot HTTP observation of DASH clocks and origin resources.
    #[arg(long, requires = "allow_origin")]
    observe_remote: bool,
    /// Exact HTTP(S) origin authorized for remote observation; repeat as needed.
    #[arg(long, value_name = "ORIGIN", requires = "observe_remote")]
    allow_origin: Vec<String>,
    /// Whole-request timeout for each remote HTTP transaction.
    #[arg(
        long,
        default_value_t = 5_000,
        value_name = "MILLISECONDS",
        requires = "observe_remote"
    )]
    observation_timeout_ms: u64,
    /// Maximum response body retained from a remote request.
    #[arg(
        long,
        default_value_t = 65_536,
        value_name = "BYTES",
        requires = "observe_remote"
    )]
    observation_max_bytes: u64,
    /// Maximum allowlisted redirects followed per observation target.
    #[arg(
        long,
        default_value_t = 2,
        value_name = "COUNT",
        requires = "observe_remote"
    )]
    observation_max_redirects: u32,
    /// Maximum total HTTP transactions, including redirects.
    #[arg(
        long,
        default_value_t = 32,
        value_name = "COUNT",
        requires = "observe_remote"
    )]
    observation_max_requests: usize,
    /// Maximum permitted absolute clock offset.
    #[arg(
        long,
        default_value_t = 5_000,
        value_name = "MILLISECONDS",
        requires = "observe_remote"
    )]
    observation_max_clock_offset_ms: u64,
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
    let profile = cli.profile.unwrap_or_else(|| {
        if cli
            .input
            .extension()
            .is_some_and(|value| value.eq_ignore_ascii_case("mpd"))
        {
            Profile::Iso23009
        } else {
            Profile::Rfc8216
        }
    });
    let (mut bytes, passed, warning_count) = match profile {
        Profile::AtscA85Service => {
            if cli.previous_mpd.is_some() || cli.mpd_patch.is_some() || cli.observe_remote {
                return Err(
                    "--previous-mpd, --mpd-patch, and --observe-remote are only valid for DASH profiles".into(),
                );
            }
            let audit = atsc_a85_service::audit(&cli.input)?;
            let passed = audit.passed;
            let warning_count = audit.warning_count;
            (encode(&audit, cli.compact)?, passed, warning_count)
        }
        Profile::Rfc8216 | Profile::AppleHls | Profile::LlHls => {
            if cli.previous_mpd.is_some() || cli.mpd_patch.is_some() || cli.observe_remote {
                return Err(
                    "--previous-mpd, --mpd-patch, and --observe-remote are only valid for DASH profiles".into(),
                );
            }
            let profile = match profile {
                Profile::Rfc8216 => HlsProfile::Rfc8216,
                Profile::AppleHls => HlsProfile::AppleHls,
                Profile::LlHls => HlsProfile::LlHls,
                Profile::Iso23009
                | Profile::DashIfIop
                | Profile::DashLive
                | Profile::AtscA85Service => unreachable!(),
            };
            let audit = hls_qc::audit(&cli.input, profile)?;
            let passed = audit.passed;
            let warning_count = audit.warning_count;
            (encode(&audit, cli.compact)?, passed, warning_count)
        }
        Profile::Iso23009 | Profile::DashIfIop | Profile::DashLive => {
            let profile = match profile {
                Profile::Iso23009 => DashProfile::Iso23009,
                Profile::DashIfIop => DashProfile::DashIfIop,
                Profile::DashLive => DashProfile::DashLive,
                Profile::Rfc8216 | Profile::AppleHls | Profile::LlHls | Profile::AtscA85Service => {
                    unreachable!()
                }
            };
            let mut audit = if let Some(patch) = &cli.mpd_patch {
                dash_qc::audit_with_patch(&cli.input, patch, profile)?
            } else if let Some(previous) = &cli.previous_mpd {
                dash_qc::audit_with_previous(&cli.input, previous, profile)?
            } else {
                dash_qc::audit(&cli.input, profile)?
            };
            if cli.observe_remote {
                let targets = if let Some(patch) = &cli.mpd_patch {
                    dash_qc::observation_targets_with_patch(&cli.input, patch)?
                } else {
                    dash_qc::observation_targets(&cli.input)?
                };
                let report = dash_observe::observe(
                    &targets,
                    &DashObservationOptions {
                        allowed_origins: cli.allow_origin.clone(),
                        timeout_milliseconds: cli.observation_timeout_ms,
                        max_body_bytes: cli.observation_max_bytes,
                        max_redirects: cli.observation_max_redirects,
                        max_requests: cli.observation_max_requests,
                        maximum_clock_offset_milliseconds: cli.observation_max_clock_offset_ms,
                    },
                )?;
                dash_qc::attach_observation_report(&mut audit, report)?;
            }
            let passed = audit.passed;
            let warning_count = audit.warning_count;
            (encode(&audit, cli.compact)?, passed, warning_count)
        }
    };
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
        if passed { "PASS" } else { "FAIL" },
        warning_count
    );
    Ok(passed)
}

fn encode(value: &impl Serialize, compact: bool) -> Result<Vec<u8>, String> {
    if compact {
        serde_json::to_vec(value)
    } else {
        serde_json::to_vec_pretty(value)
    }
    .map_err(|error| format!("serialize audit: {error}"))
}
