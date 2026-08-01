use clap::Parser;
use forge_normalizer::onnx_provider;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "forge-onnx-provider",
    version,
    about = "Run an explicitly selected ONNX anomaly model and emit provider v1 JSON"
)]
struct Cli {
    /// Versioned model manifest containing provenance, calibration, and limits.
    #[arg(long)]
    manifest: PathBuf,
    /// ONNX model file whose basename and SHA-256 must match the manifest.
    #[arg(long)]
    model: PathBuf,
    /// Bounded feature-frame JSON sidecar.
    #[arg(long)]
    features: PathBuf,
    /// Native ONNX Runtime shared library (.so/.dylib/.dll).
    #[arg(long = "runtime-library")]
    runtime_library: PathBuf,
    /// Write provider JSON to this path instead of stdout.
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Emit compact JSON.
    #[arg(long)]
    compact: bool,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(event_count) => {
            eprintln!("forge-onnx-provider: emitted {event_count} model events");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("forge-onnx-provider: error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<usize, String> {
    let provider = onnx_provider::run(
        &cli.manifest,
        &cli.model,
        &cli.features,
        &cli.runtime_library,
    )?;
    let event_count = provider.events.len();
    let mut bytes = if cli.compact {
        serde_json::to_vec(&provider)
    } else {
        serde_json::to_vec_pretty(&provider)
    }
    .map_err(|error| format!("serialize provider JSON: {error}"))?;
    bytes.push(b'\n');
    if let Some(path) = cli.output {
        fs::write(&path, bytes).map_err(|error| format!("write {}: {error}", path.display()))?;
    } else {
        io::stdout()
            .lock()
            .write_all(&bytes)
            .map_err(|error| format!("write stdout: {error}"))?;
    }
    Ok(event_count)
}
