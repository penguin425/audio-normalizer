use clap::{Parser, ValueEnum};
use forge_normalizer::adm_semantics_qc::{self, Options, PresentationIntent};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliPresentationIntent {
    Auto,
    Fixed,
    Interactive,
}

impl From<CliPresentationIntent> for PresentationIntent {
    fn from(value: CliPresentationIntent) -> Self {
        match value {
            CliPresentationIntent::Auto => Self::Auto,
            CliPresentationIntent::Fixed => Self::Fixed,
            CliPresentationIntent::Interactive => Self::Interactive,
        }
    }
}

#[derive(Parser)]
#[command(
    name = "forge-adm-semantics-qc",
    version,
    about = "Audit bounded ADM dialogue, selection, importance, and tag semantics"
)]
struct Cli {
    /// ADM BW64 input containing axml and chna chunks.
    input: PathBuf,
    /// Infer the layout, or enforce fixed/interactive alternative-selection intent.
    #[arg(long, value_enum, default_value = "auto")]
    presentation_intent: CliPresentationIntent,
    /// Require the lowest-ID fallback selection to equal this audioProgramme ID.
    #[arg(long)]
    expected_default_programme: Option<String>,
    /// Plan an importance threshold for this maximum audioObject metadata count.
    #[arg(long)]
    renderer_object_limit: Option<usize>,
    /// Atomic JSON evidence report.
    #[arg(short, long)]
    output: PathBuf,
    /// Maximum audioProgramme elements accepted.
    #[arg(long, default_value_t = adm_semantics_qc::DEFAULT_MAX_PROGRAMMES)]
    max_programmes: usize,
    /// Maximum audioContent elements accepted.
    #[arg(long, default_value_t = adm_semantics_qc::DEFAULT_MAX_CONTENTS)]
    max_contents: usize,
    /// Maximum audioObject elements accepted.
    #[arg(long, default_value_t = adm_semantics_qc::DEFAULT_MAX_OBJECTS)]
    max_objects: usize,
    /// Maximum expanded audit items accepted in the report.
    #[arg(long, default_value_t = adm_semantics_qc::DEFAULT_MAX_REPORT_ITEMS)]
    max_report_items: usize,
    /// Maximum axml chunk size accepted in bytes.
    #[arg(long, default_value_t = adm_semantics_qc::DEFAULT_MAX_AXML_BYTES)]
    max_axml_bytes: usize,
    /// Maximum parsed XML element count.
    #[arg(long, default_value_t = adm_semantics_qc::DEFAULT_MAX_XML_NODES)]
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
            eprintln!("forge-adm-semantics-qc: error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<bool, String> {
    if cli.output.exists() && !cli.overwrite {
        return Err(format!(
            "refusing to replace existing ADM semantics report {}; pass --overwrite",
            cli.output.display()
        ));
    }
    let report = adm_semantics_qc::run(&Options {
        input: cli.input,
        presentation_intent: cli.presentation_intent.into(),
        expected_default_programme: cli.expected_default_programme,
        renderer_object_limit: cli.renderer_object_limit,
        max_programmes: cli.max_programmes,
        max_contents: cli.max_contents,
        max_objects: cli.max_objects,
        max_report_items: cli.max_report_items,
        max_axml_bytes: cli.max_axml_bytes,
        max_xml_nodes: cli.max_xml_nodes,
    })?;
    let passed = report.passed;
    let programmes = report.counts.programmes;
    let contents = report.counts.contents;
    let objects = report.counts.objects;
    adm_semantics_qc::write_report(&cli.output, &report, cli.compact, cli.overwrite)?;
    eprintln!(
        "forge-adm-semantics-qc: {} ({programmes} programmes, {contents} contents, {objects} objects; metadata only)",
        if passed { "PASS" } else { "FAIL" }
    );
    Ok(passed)
}
