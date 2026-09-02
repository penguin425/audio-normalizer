use clap::{Parser, ValueEnum};
use forge_normalizer::adm_interactivity_qc::{self, Options, Profile};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliProfile {
    Safety,
    Bs2168EmissionRanges,
}

impl From<CliProfile> for Profile {
    fn from(value: CliProfile) -> Self {
        match value {
            CliProfile::Safety => Self::Safety,
            CliProfile::Bs2168EmissionRanges => Self::Bs2168EmissionRanges,
        }
    }
}

#[derive(Parser)]
#[command(
    name = "forge-adm-interactivity-qc",
    version,
    about = "Audit bounded ADM gain and position personalization metadata"
)]
struct Cli {
    /// ADM BW64 input containing axml and chna chunks.
    input: PathBuf,
    /// Safety-only audit or the BS.2168 emission interactivity subset.
    #[arg(long, value_enum, default_value = "safety")]
    profile: CliProfile,
    /// Atomic JSON evidence report.
    #[arg(short, long)]
    output: PathBuf,
    /// Maximum audioObject elements accepted from the ADM graph.
    #[arg(
        long,
        default_value_t = adm_interactivity_qc::DEFAULT_MAX_OBJECTS
    )]
    max_objects: usize,
    /// Maximum parent and alternative-value interaction configurations.
    #[arg(
        long,
        default_value_t = adm_interactivity_qc::DEFAULT_MAX_CONFIGURATIONS
    )]
    max_configurations: usize,
    /// Maximum axml chunk size accepted in bytes.
    #[arg(
        long,
        default_value_t = adm_interactivity_qc::DEFAULT_MAX_AXML_BYTES
    )]
    max_axml_bytes: usize,
    /// Maximum parsed XML element count.
    #[arg(
        long,
        default_value_t = adm_interactivity_qc::DEFAULT_MAX_XML_NODES
    )]
    max_xml_nodes: usize,
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
            eprintln!("forge-adm-interactivity-qc: error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<bool, String> {
    if cli.output.exists() && !cli.overwrite {
        return Err(format!(
            "refusing to replace existing ADM interactivity report {}; pass --overwrite",
            cli.output.display()
        ));
    }
    let report = adm_interactivity_qc::run(&Options {
        input: cli.input,
        profile: cli.profile.into(),
        max_objects: cli.max_objects,
        max_configurations: cli.max_configurations,
        max_axml_bytes: cli.max_axml_bytes,
        max_xml_nodes: cli.max_xml_nodes,
    })?;
    let passed = report.passed;
    let configurations = report.configuration_count;
    adm_interactivity_qc::write_report(&cli.output, &report, cli.compact, cli.overwrite)?;
    eprintln!(
        "forge-adm-interactivity-qc: {} ({configurations} configurations; metadata only)",
        if passed { "PASS" } else { "FAIL" }
    );
    Ok(passed)
}
