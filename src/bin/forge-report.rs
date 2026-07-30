use clap::{Parser, Subcommand, ValueEnum};
use forge_normalizer::report_tools;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use tempfile::Builder;

#[derive(Parser)]
#[command(
    name = "forge-report",
    version,
    about = "Migrate Forge reports and explain failed compliance rules"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Migrate a delivery manifest from v1/v2 to the current v3 schema.
    Migrate {
        input: PathBuf,
        #[arg(short, long, conflicts_with = "in_place")]
        output: Option<PathBuf>,
        #[arg(long, conflicts_with = "output")]
        in_place: bool,
        #[arg(long)]
        check: bool,
        /// Replace an existing output file.
        #[arg(long)]
        overwrite: bool,
    },
    /// Explain each failed compliance rule in analysis JSON or a manifest.
    Explain {
        input: PathBuf,
        #[arg(short, long)]
        output: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "text")]
        format: ExplainFormat,
        /// Replace an existing output file.
        #[arg(long)]
        overwrite: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ExplainFormat {
    Text,
    Json,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("forge-report: error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode, String> {
    match cli.command {
        Command::Migrate {
            input,
            output,
            in_place,
            check,
            overwrite,
        } => migrate(&input, output.as_deref(), in_place, check, overwrite),
        Command::Explain {
            input,
            output,
            format,
            overwrite,
        } => explain(&input, output.as_deref(), format, overwrite),
    }
}

fn migrate(
    input: &Path,
    output: Option<&Path>,
    in_place: bool,
    check: bool,
    overwrite: bool,
) -> Result<ExitCode, String> {
    if !in_place && output.is_none() && !check {
        return Err("migrate requires --output, --in-place, or --check".into());
    }
    let bytes = read_report(input)?;
    let (manifest, summary) = report_tools::migrate_delivery_manifest(&bytes)?;
    if check {
        eprintln!(
            "forge-report: {} is {} ({} assets)",
            input.display(),
            if summary.changed {
                "migration required"
            } else {
                "current"
            },
            summary.asset_count
        );
        return Ok(if summary.changed {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        });
    }
    let mut encoded = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| format!("encode migrated manifest: {error}"))?;
    encoded.push(b'\n');
    let destination = if in_place {
        input
    } else {
        output.expect("validated output")
    };
    write_atomic(destination, &encoded, in_place || overwrite)?;
    eprintln!(
        "forge-report: migrated {} -> {} ({} assets, {} QC envelopes)",
        summary.source_schema,
        summary.target_schema,
        summary.asset_count,
        summary.migrated_qc_envelopes
    );
    Ok(ExitCode::SUCCESS)
}

fn explain(
    input: &Path,
    output: Option<&Path>,
    format: ExplainFormat,
    overwrite: bool,
) -> Result<ExitCode, String> {
    let bytes = read_report(input)?;
    let report = report_tools::explain_failed_rules(&bytes)?;
    let mut encoded = match format {
        ExplainFormat::Json => serde_json::to_vec_pretty(&report)
            .map_err(|error| format!("encode explanations: {error}"))?,
        ExplainFormat::Text => format_text(&report).into_bytes(),
    };
    if !encoded.ends_with(b"\n") {
        encoded.push(b'\n');
    }
    if let Some(path) = output {
        write_atomic(path, &encoded, overwrite)?;
    } else {
        io::stdout()
            .lock()
            .write_all(&encoded)
            .map_err(|error| format!("write stdout: {error}"))?;
    }
    eprintln!(
        "forge-report: explained {} failed rules across {} assets",
        report.failed_rule_count, report.asset_count
    );
    Ok(ExitCode::SUCCESS)
}

fn format_text(report: &report_tools::ExplanationReport) -> String {
    if report.explanations.is_empty() {
        return "No failed compliance rules found.\n".into();
    }
    let mut output = String::new();
    for explanation in &report.explanations {
        output.push_str(&format!(
            "{} [{}]\n  source: {}",
            explanation.asset, explanation.rule_id, explanation.source.profile
        ));
        if let Some(standard) = &explanation.source.standard {
            output.push_str(&format!("; {standard}"));
        }
        if let Some(version) = &explanation.source.standard_version {
            output.push_str(&format!(" {version}"));
        }
        if let Some(url) = explanation.source.url {
            output.push_str(&format!("; {url}"));
        }
        output.push_str(&format!(
            "\n  observation: {} = {:.2} {}",
            explanation.metric, explanation.observation.measured, explanation.observation.unit
        ));
        output.push_str(&format!(
            "\n  requirement: {}\n  remediation: {}\n",
            explanation.requirement, explanation.remediation
        ));
    }
    output
}

fn write_atomic(destination: &Path, bytes: &[u8], replace: bool) -> Result<(), String> {
    if destination.exists() && !replace {
        return Err(format!(
            "{} already exists (use --overwrite to replace it)",
            destination.display()
        ));
    }
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = Builder::new()
        .prefix(".forge-report-")
        .tempfile_in(parent)
        .map_err(|error| {
            format!(
                "create temporary output beside {}: {error}",
                destination.display()
            )
        })?;
    temporary
        .write_all(bytes)
        .map_err(|error| format!("write temporary output: {error}"))?;
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(|error| format!("sync temporary output: {error}"))?;
    temporary
        .persist(destination)
        .map_err(|error| format!("commit {}: {}", destination.display(), error.error))?;
    Ok(())
}

fn read_report(path: &Path) -> Result<Vec<u8>, String> {
    let metadata =
        fs::metadata(path).map_err(|error| format!("inspect {}: {error}", path.display()))?;
    let size = usize::try_from(metadata.len())
        .map_err(|_| format!("{} is too large for this platform", path.display()))?;
    if size > report_tools::MAX_REPORT_BYTES {
        return Err(format!(
            "{} is {size} bytes; limit is {} bytes",
            path.display(),
            report_tools::MAX_REPORT_BYTES
        ));
    }
    fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))
}
