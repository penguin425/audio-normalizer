use clap::Parser;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "forge-metadata-repair",
    version,
    about = "Validate and conservatively repair BWF, ADM, or ISO-BMFF loudness metadata into a separate output file"
)]
struct Cli {
    /// JSON or TOML metadata repair request.
    request: PathBuf,
    /// Write the JSON report to this path instead of stdout.
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Emit compact JSON.
    #[arg(long)]
    compact: bool,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!("forge-metadata-repair: error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<bool, String> {
    let plan = forge_normalizer::metadata_repair::prepare_versioned_file(
        &cli.request,
        cli.output.as_deref(),
    )?;
    let execution = plan.execute()?;
    let report = if cli.output.is_some() {
        if cli.compact {
            execution.publish_compact_report()?
        } else {
            execution.publish_report()?
        }
    } else {
        let mut bytes = if cli.compact {
            serde_json::to_vec(execution.report())
        } else {
            serde_json::to_vec_pretty(execution.report())
        }
        .map_err(|error| format!("serialize metadata repair report: {error}"))?;
        bytes.push(b'\n');
        io::stdout()
            .lock()
            .write_all(&bytes)
            .map_err(|error| format!("write stdout: {error}"))?;
        execution.into_report()
    };
    eprintln!(
        "forge-metadata-repair: {} ({}, {} action(s))",
        if report.report().passed {
            "PASS"
        } else {
            "REVIEW REQUIRED"
        },
        report.report().source_format,
        report.report().actions.len()
    );
    Ok(report.report().passed)
}
