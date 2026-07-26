use clap::Parser;
use forge_normalizer::dialogue_provider::DialogueRangeFile;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "forge-dialogue-provider",
    version,
    about = "Validate external ASR/VAD output and create auditable dialogue ranges"
)]
struct Cli {
    input: PathBuf,
    #[arg(long, default_value_t = 0.6)]
    threshold: f64,
    #[arg(short, long)]
    output: Option<PathBuf>,
    #[arg(long = "ranges-output")]
    ranges_output: PathBuf,
    #[arg(long)]
    compact: bool,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("forge-dialogue-provider: error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let audit = forge_normalizer::dialogue_provider::load_and_audit(&cli.input, cli.threshold)?;
    let mut ranges = serde_json::to_vec_pretty(&DialogueRangeFile {
        ranges: &audit.ranges,
    })
    .map_err(|error| format!("serialize dialogue ranges: {error}"))?;
    ranges.push(b'\n');
    fs::write(&cli.ranges_output, ranges)
        .map_err(|error| format!("write {}: {error}", cli.ranges_output.display()))?;

    let mut bytes = if cli.compact {
        serde_json::to_vec(&audit)
    } else {
        serde_json::to_vec_pretty(&audit)
    }
    .map_err(|error| format!("serialize provider audit: {error}"))?;
    bytes.push(b'\n');
    if let Some(path) = &cli.output {
        fs::write(path, bytes).map_err(|error| format!("write {}: {error}", path.display()))?;
    } else {
        io::stdout()
            .lock()
            .write_all(&bytes)
            .map_err(|error| format!("write stdout: {error}"))?;
    }
    eprintln!(
        "forge-dialogue-provider: selected {} of {} segments",
        audit.selected_segment_count, audit.input_segment_count
    );
    Ok(())
}
