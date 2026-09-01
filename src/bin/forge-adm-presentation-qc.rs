use clap::Parser;
use forge_normalizer::adm_presentation_qc::{self, Options};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "forge-adm-presentation-qc",
    version,
    about = "Render and audit every ADM programme and complementary-object presentation"
)]
struct Cli {
    /// ADM BW64 input containing axml and chna chunks.
    input: PathBuf,
    /// EBU ADM Renderer `ear-render` executable.
    #[arg(long, default_value = "ear-render")]
    renderer: PathBuf,
    /// ITU-R BS.2051 output layout passed to `ear-render -s`.
    #[arg(long, default_value = "0+2+0")]
    layout: String,
    /// Atomic JSON evidence report.
    #[arg(short, long)]
    output: PathBuf,
    /// Per-presentation renderer timeout.
    #[arg(long, default_value_t = 300)]
    timeout_seconds: u64,
    /// Maximum total programme/complementary presentations to render.
    #[arg(
        long,
        default_value_t = adm_presentation_qc::DEFAULT_MAX_PRESENTATIONS
    )]
    max_presentations: usize,
    /// Maximum decoded interleaved PCM samples accepted per render.
    #[arg(
        long,
        default_value_t = adm_presentation_qc::DEFAULT_MAX_DECODED_SAMPLES
    )]
    max_decoded_samples: u64,
    /// Allowed absolute drift from declared integrated loudness.
    #[arg(long, default_value_t = 1.0)]
    loudness_tolerance_lu: f64,
    /// Allowed absolute drift from declared maximum true peak.
    #[arg(long, default_value_t = 1.0)]
    true_peak_tolerance_db: f64,
    /// Optional directory in which to retain numbered WAVE renders.
    #[arg(long)]
    retain_renders: Option<PathBuf>,
    /// Replace an existing report or retained render with the same name.
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
            eprintln!("forge-adm-presentation-qc: error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<bool, String> {
    if cli.output.exists() && !cli.overwrite {
        return Err(format!(
            "refusing to replace existing ADM presentation report {}; pass --overwrite",
            cli.output.display()
        ));
    }
    let report = adm_presentation_qc::run(&Options {
        input: cli.input,
        renderer: cli.renderer,
        layout: cli.layout,
        timeout_seconds: cli.timeout_seconds,
        max_presentations: cli.max_presentations,
        max_decoded_samples_per_presentation: cli.max_decoded_samples,
        loudness_tolerance_lu: cli.loudness_tolerance_lu,
        true_peak_tolerance_db: cli.true_peak_tolerance_db,
        retained_renders: cli.retain_renders,
        overwrite: cli.overwrite,
    })?;
    let passed = report.passed;
    let count = report.presentation_count;
    adm_presentation_qc::write_report(&cli.output, &report, cli.compact, cli.overwrite)?;
    eprintln!(
        "forge-adm-presentation-qc: {} ({count} presentations)",
        if passed { "PASS" } else { "FAIL" }
    );
    Ok(passed)
}
