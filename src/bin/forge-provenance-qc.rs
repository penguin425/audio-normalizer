use clap::{Parser, ValueEnum};
use forge_normalizer::provenance::{self, ProvenanceOptions, ValidationPolicy};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Policy {
    Integrity,
    Trusted,
}

impl From<Policy> for ValidationPolicy {
    fn from(value: Policy) -> Self {
        match value {
            Policy::Integrity => Self::Integrity,
            Policy::Trusted => Self::Trusted,
        }
    }
}

#[derive(Parser)]
#[command(
    name = "forge-provenance-qc",
    version,
    about = "Validate C2PA Content Credentials with the official c2patool"
)]
struct Cli {
    input: PathBuf,
    #[arg(long, default_value = "c2patool")]
    c2pa_tool: PathBuf,
    #[arg(long)]
    external_manifest: Option<PathBuf>,
    #[arg(long)]
    trust_anchors: Option<String>,
    #[arg(long)]
    allowed_list: Option<String>,
    #[arg(long)]
    trust_config: Option<String>,
    #[arg(long, value_enum, default_value_t = Policy::Integrity)]
    policy: Policy,
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u32).range(1..=600))]
    timeout_seconds: u32,
    #[arg(
        long,
        default_value_t = 16,
        value_parser = clap::value_parser!(u16).range(1..=256)
    )]
    max_report_mib: u16,
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
            eprintln!("forge-provenance-qc: error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<bool, String> {
    let max_report_bytes = usize::from(cli.max_report_mib)
        .checked_mul(1024 * 1024)
        .ok_or_else(|| "report size limit overflow".to_string())?;
    let options = ProvenanceOptions {
        c2pa_tool: cli.c2pa_tool,
        external_manifest: cli.external_manifest,
        trust_anchors: cli.trust_anchors,
        allowed_list: cli.allowed_list,
        trust_config: cli.trust_config,
        policy: cli.policy.into(),
        timeout: Duration::from_secs(u64::from(cli.timeout_seconds)),
        max_report_bytes,
    };
    let audit = provenance::audit(&cli.input, &options)?;
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
        "forge-provenance-qc: {} (integrity={}, trusted={})",
        if audit.passed { "PASS" } else { "FAIL" },
        audit.integrity_valid,
        audit.trusted
    );
    Ok(audit.passed)
}
