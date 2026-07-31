use clap::{Parser, Subcommand};
use forge_normalizer::segment_normalize;
use forge_normalizer::wav::named_channel_layout;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "forge-segment-normalize",
    version,
    about = "Plan and render bounded segment-aware catalogue normalization"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Analyze ordered segments and write a content-bound pass-one plan.
    Plan {
        /// Versioned JSON or TOML request.
        #[arg(long)]
        request: PathBuf,
        /// Atomic versioned JSON pass-one plan.
        #[arg(long)]
        manifest: PathBuf,
        /// Replace an existing plan manifest.
        #[arg(long)]
        overwrite: bool,
        /// Override absent or incorrect source channel-layout metadata.
        #[arg(
            long,
            value_parser = ["mono", "stereo", "5.1", "6.1", "7.1", "5.1.4", "7.1.4"]
        )]
        channel_layout: Option<String>,
    },
    /// Verify a pass-one plan, render each segment, and write pass-two evidence.
    Render {
        /// Content-bound pass-one plan.
        #[arg(long)]
        manifest: PathBuf,
        /// Atomic versioned JSON pass-two evidence report.
        #[arg(long)]
        report: PathBuf,
        /// Replace existing segment outputs and report.
        #[arg(long)]
        overwrite: bool,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Plan {
            request,
            manifest,
            overwrite,
            channel_layout,
        } => {
            let roles = channel_layout.as_deref().and_then(named_channel_layout);
            match segment_normalize::create_plan(&request, &manifest, overwrite, roles.as_deref()) {
                Ok(plan) => {
                    eprintln!(
                        "forge-segment-normalize: planned {} segments{}",
                        plan.segments.len(),
                        if plan.manual_review_recommended {
                            " (boundary review recommended)"
                        } else {
                            ""
                        }
                    );
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("forge-segment-normalize: error: {error}");
                    ExitCode::from(2)
                }
            }
        }
        Command::Render {
            manifest,
            report,
            overwrite,
        } => match segment_normalize::render_plan(&manifest, &report, overwrite) {
            Ok(result) if result.passed => {
                eprintln!(
                    "forge-segment-normalize: PASS ({} segments published)",
                    result.published_segments
                );
                ExitCode::SUCCESS
            }
            Ok(result) => {
                eprintln!(
                    "forge-segment-normalize: FAIL ({}/{} segments published)",
                    result.published_segments,
                    result.segments.len()
                );
                ExitCode::from(1)
            }
            Err(error) => {
                eprintln!("forge-segment-normalize: error: {error}");
                ExitCode::from(2)
            }
        },
    }
}
