use clap::Parser;
use forge_normalizer::multi_delivery;
use forge_normalizer::wav::named_channel_layout;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "forge-multi-delivery",
    version,
    about = "Render and verify several codec/profile deliveries with one conservative shared gain"
)]
struct Cli {
    /// One source audio file.
    input: PathBuf,
    /// Versioned JSON or TOML delivery request.
    #[arg(long)]
    request: PathBuf,
    /// Atomic versioned JSON evidence report.
    #[arg(long)]
    report: PathBuf,
    /// Replace existing outputs and report.
    #[arg(long)]
    overwrite: bool,
    /// Override absent or incorrect input channel-layout metadata.
    #[arg(
        long,
        value_parser = ["mono", "stereo", "5.1", "6.1", "7.1", "5.1.4", "7.1.4"]
    )]
    channel_layout: Option<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let roles = cli.channel_layout.as_deref().and_then(named_channel_layout);
    match multi_delivery::run(
        &cli.input,
        &cli.request,
        &cli.report,
        cli.overwrite,
        roles.as_deref(),
    ) {
        Ok(report) if report.passed => {
            eprintln!(
                "forge-multi-delivery: PASS ({} outputs, {:.2} dB shared gain)",
                report.deliveries.len(),
                report.common.shared_gain_db
            );
            ExitCode::SUCCESS
        }
        Ok(_) => {
            eprintln!("forge-multi-delivery: FAIL");
            ExitCode::from(1)
        }
        Err(error) => {
            eprintln!("forge-multi-delivery: error: {error}");
            ExitCode::from(2)
        }
    }
}
