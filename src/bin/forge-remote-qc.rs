use clap::Parser;
use forge_normalizer::remote_range::{self, RemoteRangeOptions};
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "forge-remote-qc",
    version,
    about = "Perform an explicitly allow-listed, bounded HTTP Range media probe"
)]
struct Cli {
    /// HTTPS, s3://bucket/key, or gs://bucket/key object URI.
    uri: String,
    /// Exact origin authorized for the object and every redirect; repeat as needed.
    #[arg(long, value_name = "ORIGIN", required = true)]
    allow_origin: Vec<String>,
    /// Byte range to inspect, written as START-LENGTH.
    #[arg(long, default_value = "0-65536", value_name = "START-LENGTH")]
    range: String,
    /// Whole-request timeout in milliseconds.
    #[arg(long, default_value_t = 5_000, value_name = "MILLISECONDS")]
    timeout_ms: u64,
    /// Maximum bytes returned by one Range request.
    #[arg(long, default_value_t = 1024 * 1024, value_name = "BYTES")]
    max_range_bytes: u64,
    /// Maximum aggregate response bytes for this invocation.
    #[arg(long, default_value_t = 64 * 1024 * 1024, value_name = "BYTES")]
    max_total_bytes: u64,
    /// Maximum object size advertised by Content-Range.
    #[arg(long, default_value_t = 4 * 1024 * 1024 * 1024, value_name = "BYTES")]
    max_object_bytes: u64,
    /// Maximum HTTP transactions, including redirects.
    #[arg(long, default_value_t = 128, value_name = "COUNT")]
    max_requests: usize,
    /// Maximum allow-listed redirects per range.
    #[arg(long, default_value_t = 2, value_name = "COUNT")]
    max_redirects: u32,
    /// Permit plain HTTP only for explicitly allow-listed test/private origins.
    #[arg(long)]
    allow_insecure_http: bool,
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
            eprintln!("forge-remote-qc: error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<bool, String> {
    let (start, length) = parse_range(&cli.range)?;
    let report = remote_range::probe(
        &cli.uri,
        RemoteRangeOptions {
            allowed_origins: cli.allow_origin,
            timeout_milliseconds: cli.timeout_ms,
            max_range_bytes: cli.max_range_bytes,
            max_total_bytes: cli.max_total_bytes,
            max_object_bytes: cli.max_object_bytes,
            max_requests: cli.max_requests,
            max_redirects: cli.max_redirects,
            allow_insecure_http: cli.allow_insecure_http,
        },
        start,
        length,
    )?;
    let passed = report.passed;
    let bytes = encode(&report, cli.compact)?;
    if let Some(path) = cli.output {
        std::fs::write(&path, bytes)
            .map_err(|error| format!("write {}: {error}", path.display()))?;
    } else {
        io::stdout()
            .lock()
            .write_all(&bytes)
            .map_err(|error| format!("write stdout: {error}"))?;
    }
    eprintln!(
        "forge-remote-qc: {} ({} byte(s), {})",
        if passed { "PASS" } else { "FAIL" },
        report.returned_bytes,
        report.detected_format
    );
    Ok(passed)
}

fn parse_range(value: &str) -> Result<(u64, u64), String> {
    let (start, length) = value
        .split_once('-')
        .ok_or_else(|| "range must be START-LENGTH".to_string())?;
    let start = start
        .parse::<u64>()
        .map_err(|_| "range start is not an unsigned integer".to_string())?;
    let length = length
        .parse::<u64>()
        .map_err(|_| "range length is not an unsigned integer".to_string())?;
    if length == 0 {
        return Err("range length must be greater than zero".into());
    }
    Ok((start, length))
}

fn encode(value: &impl Serialize, compact: bool) -> Result<Vec<u8>, String> {
    let mut bytes = if compact {
        serde_json::to_vec(value)
    } else {
        serde_json::to_vec_pretty(value)
    }
    .map_err(|error| format!("serialize remote QC: {error}"))?;
    bytes.push(b'\n');
    Ok(bytes)
}
