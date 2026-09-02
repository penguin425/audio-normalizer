use clap::{Parser, ValueEnum};
use forge_normalizer::adm::emission::{self, Level, Options};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliLevel {
    #[value(name = "0")]
    Level0,
    #[value(name = "1")]
    Level1,
    #[value(name = "2")]
    Level2,
}

impl From<CliLevel> for Level {
    fn from(value: CliLevel) -> Self {
        match value {
            CliLevel::Level0 => Self::Level0,
            CliLevel::Level1 => Self::Level1,
            CliLevel::Level2 => Self::Level2,
        }
    }
}

#[derive(Parser)]
#[command(
    name = "forge-adm-emission-qc",
    version,
    about = "Audit AXML-carried ADM against ITU-R BS.2168 sections 2-3",
    long_about = "Audit AXML-carried file-based ADM metadata and its BW64 essence against the selected ITU-R BS.2168 Level 0, 1, or 2 requirements in sections 2-3. BXML carriage is reported as unsupported input. The bounded evidence report is a preflight aid, not a certification."
)]
struct Cli {
    /// ADM BW64 input containing one axml chunk and a chna chunk.
    input: PathBuf,
    /// ITU-R BS.2168 emission profile level to enforce.
    #[arg(long, value_enum)]
    level: CliLevel,
    /// Atomic JSON evidence report.
    #[arg(short, long)]
    output: PathBuf,
    /// Override the maximum accepted axml chunk size in bytes.
    #[arg(long)]
    max_axml_bytes: Option<usize>,
    /// Override the maximum accepted chna chunk size in bytes.
    #[arg(long)]
    max_chna_bytes: Option<usize>,
    /// Override the maximum parsed XML element count.
    #[arg(long)]
    max_xml_nodes: Option<usize>,
    /// Override the maximum XML element nesting depth.
    #[arg(long)]
    max_xml_depth: Option<usize>,
    /// Override the maximum attributes accepted on one XML element.
    #[arg(long)]
    max_attributes_per_element: Option<usize>,
    /// Override the maximum cumulative decoded XML text size in bytes.
    #[arg(long)]
    max_xml_text_bytes: Option<usize>,
    /// Override the maximum expanded evidence items in the report.
    #[arg(long)]
    max_report_items: Option<usize>,
    /// Override the maximum evidence entries retained for one rule.
    #[arg(long)]
    max_evidence_items: Option<usize>,
    /// Replace an existing report.
    #[arg(long)]
    overwrite: bool,
    /// Emit compact rather than pretty JSON.
    #[arg(long)]
    compact: bool,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!("forge-adm-emission-qc: error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<bool, String> {
    if cli.output.exists() && !cli.overwrite {
        return Err(format!(
            "refusing to replace existing ADM emission report {}; pass --overwrite",
            cli.output.display()
        ));
    }

    let level = Level::from(cli.level);
    let mut options = Options::new(cli.input, level);
    if let Some(limit) = cli.max_axml_bytes {
        options.max_axml_bytes = limit;
    }
    if let Some(limit) = cli.max_chna_bytes {
        options.max_chna_bytes = limit;
    }
    if let Some(limit) = cli.max_xml_nodes {
        options.max_xml_nodes = limit;
    }
    if let Some(limit) = cli.max_xml_depth {
        options.max_xml_depth = limit;
    }
    if let Some(limit) = cli.max_attributes_per_element {
        options.max_attributes_per_element = limit;
    }
    if let Some(limit) = cli.max_xml_text_bytes {
        options.max_xml_text_bytes = limit;
    }
    if let Some(limit) = cli.max_report_items {
        options.max_report_items = limit;
    }
    if let Some(limit) = cli.max_evidence_items {
        options.max_evidence_items = limit;
    }

    let report = emission::validate(&options)?;
    let passed = report.passed;
    emission::write_report(&cli.output, &report, cli.compact, cli.overwrite)?;
    eprintln!(
        "forge-adm-emission-qc: {} (ITU-R BS.2168 Level {level}; metadata and essence preflight, not certification)",
        if passed { "PASS" } else { "FAIL" }
    );
    Ok(passed)
}
