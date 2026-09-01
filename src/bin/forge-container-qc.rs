use clap::Parser;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "forge-container-qc",
    version,
    about = "Audit WAVE, AIFF/AIFC, CAF, AU, DSF/DSDIFF, WavPack, Monkey's Audio, FLAC, MP3, AAC, MPEG-TS/M2TS, MXF, AAF, Ogg, Matroska/WebM, and ISO-BMFF delivery containers"
)]
struct Cli {
    input: PathBuf,
    #[arg(short, long)]
    output: Option<PathBuf>,
    #[arg(long)]
    compact: bool,

    /// Decoded, unprocessed PCM render used to reconcile xHE-AAC programme
    /// loudness and peak metadata.
    #[arg(long, value_name = "PATH")]
    decoded_reference: Option<PathBuf>,

    /// Decoded dialogue/anchor render used to reconcile xHE-AAC Anchor
    /// Loudness metadata.
    #[arg(long, value_name = "PATH", requires = "decoded_reference")]
    anchor_reference: Option<PathBuf>,

    /// Maximum absolute decoded-versus-metadata deviation in LU/dB.
    #[arg(
        long,
        default_value_t = 0.5,
        requires = "decoded_reference",
        value_name = "LU_DB"
    )]
    xhe_metadata_tolerance: f64,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!("forge-container-qc: error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<bool, String> {
    let mut audit = forge_normalizer::container_qc::audit(&cli.input)?;
    if let Some(reference) = &cli.decoded_reference {
        let decoded_program = forge_normalizer::normalize::analyze_file(reference)?;
        let decoded_anchor = cli
            .anchor_reference
            .as_deref()
            .map(forge_normalizer::normalize::analyze_file)
            .transpose()?;
        let reconciliation = forge_normalizer::codec_qc::evaluate_xhe_decoded_metadata(
            &audit,
            &decoded_program,
            decoded_anchor.as_ref(),
            cli.xhe_metadata_tolerance,
        )?;
        let passed = reconciliation.passed;
        let observed = serde_json::to_value(&reconciliation)
            .map_err(|error| format!("serialize xHE-AAC reconciliation: {error}"))?;
        let check = forge_normalizer::container_qc::AuditCheck {
            rule_id: "FORGE-ISOBMFF-XHE-AAC-DECODED-PCM-XCHECK",
            passed,
            message: "xHE-AAC Anchor Loudness, peak metadata, and any present programme metadata agree with independently decoded PCM"
                .into(),
            observed: Some(observed.clone()),
        };
        if let Some(layer) = audit
            .layers
            .iter_mut()
            .find(|layer| layer.layer == "x-check")
        {
            layer.checks.push(check);
            layer.passed &= passed;
        } else {
            audit
                .layers
                .push(forge_normalizer::container_qc::AuditLayer {
                    layer: "x-check",
                    passed,
                    checks: vec![check],
                });
        }
        audit.passed &= passed;
        if let Some(properties) = audit.properties.as_object_mut() {
            properties.insert("xhe_decoded_metadata_qc".into(), observed);
        }
    }
    let mut bytes = if cli.compact {
        serde_json::to_vec(&audit)
    } else {
        serde_json::to_vec_pretty(&audit)
    }
    .map_err(|error| format!("serialize audit: {error}"))?;
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
        "forge-container-qc: {} ({})",
        if audit.passed { "PASS" } else { "FAIL" },
        audit.format
    );
    Ok(audit.passed)
}
