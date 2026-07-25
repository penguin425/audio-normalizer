//! Forge: a SIMD-accelerated EBU R128 / ITU-R BS.1770-4 loudness normalizer.

use clap::Parser;
use forge_normalizer::cli;
use forge_normalizer::normalize::{self, Mode, OutputFormat, Plan};
use forge_normalizer::wav::PcmKind;
use rayon::ThreadPoolBuilder;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("forge: error: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

fn parse_mode(s: &str) -> Mode {
    match s {
        "peak" => Mode::Peak,
        "rms" => Mode::Rms,
        _ => Mode::Lufs,
    }
}

fn parse_bits(s: &str) -> PcmKind {
    match s {
        "8" => PcmKind::U8,
        "16" => PcmKind::S16,
        "24" => PcmKind::S24,
        "32" => PcmKind::S32,
        "32f" => PcmKind::F32,
        "64f" => PcmKind::F64,
        _ => PcmKind::S16,
    }
}

fn run(cli: cli::Cli) -> Result<(), String> {
    if let Some(j) = cli.jobs {
        ThreadPoolBuilder::new()
            .num_threads(j)
            .build_global()
            .map_err(|e| format!("thread pool: {e}"))?;
    }

    let plan = Plan {
        mode: parse_mode(&cli.mode),
        target_lufs: cli.target_lufs,
        target_peak_db: cli.target_peak_db,
        target_rms_db: cli.target_rms_db,
        ceiling_db: cli.ceiling_db,
        max_gain_db: cli.max_gain_db,
        dither: cli.dither,
        output_kind: cli.bits.as_deref().map(parse_bits),
        mp3_bitrate: cli.bitrate,
        mp3_quality: cli.quality,
    };

    if cli.album && plan.mode != Mode::Lufs {
        return Err("--album is only valid with --mode lufs".into());
    }

    let (outputs, formats) = resolve_outputs_and_formats(&cli)?;

    if cli.analyze_only {
        for input in &cli.inputs {
            let an = normalize::analyze_file(input)?;
            print_analysis(input, &an, None);
        }
        return Ok(());
    }

    if cli.album {
        let results = normalize::normalize_album(&cli.inputs, &outputs, &plan, &formats)?;
        let analyses: Vec<_> = results.iter().map(|(a, _)| a.clone()).collect();
        let album_l = normalize::album_lufs(&analyses);
        let gain = results.first().map(|r| r.1).unwrap_or(1.0);
        for (i, (an, g)) in results.iter().enumerate() {
            print_analysis(&cli.inputs[i], an, Some(*g));
        }
        eprintln!(
            "album: {:.2} LUFS  shared gain {:+.2} dB",
            album_l,
            20.0 * (gain as f64).log10()
        );
        return Ok(());
    }

    for ((input, output), fmt) in cli.inputs.iter().zip(outputs.iter()).zip(formats.iter()) {
        if cli.gain_only {
            let an = normalize::analyze_file(input)?;
            let gain = normalize::compute_gain(&an, &plan);
            print_analysis(input, &an, Some(gain));
        } else {
            let (an, gain) = normalize::normalize_one(input, output, &plan, *fmt)?;
            print_analysis(input, &an, Some(gain));
        }
    }
    Ok(())
}

fn resolve_outputs_and_formats(
    cli: &cli::Cli,
) -> Result<(Vec<PathBuf>, Vec<OutputFormat>), String> {
    let explicit = cli.format.as_deref().map(parse_format);
    let mut outputs = Vec::with_capacity(cli.inputs.len());
    let mut formats = Vec::with_capacity(cli.inputs.len());

    if let Some(out) = &cli.output {
        if out.is_dir() {
            for inp in &cli.inputs {
                let fmt = explicit.unwrap_or_else(|| default_format_for_input(inp));
                let stem = inp.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
                outputs.push(out.join(format!("{stem}_normalized.{}", fmt_ext(fmt))));
                formats.push(fmt);
            }
            return Ok((outputs, formats));
        }
        if cli.inputs.len() == 1 {
            // Single explicit output file: infer its format from the extension.
            let fmt = explicit
                .or_else(|| infer_format(out))
                .unwrap_or_else(|| default_format_for_input(&cli.inputs[0]));
            outputs.push(out.clone());
            formats.push(fmt);
            return Ok((outputs, formats));
        }
        return Err(format!(
            "--output must be an existing directory for multiple inputs: {}",
            out.display()
        ));
    }

    // No -o: write <stem>_normalized.<ext> next to each input. The extension
    // follows the chosen format, which defaults to the input's container when
    // supported (mp3 -> mp3) and otherwise wav.
    for inp in &cli.inputs {
        let fmt = explicit.unwrap_or_else(|| default_format_for_input(inp));
        let stem = inp.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
        let dir = inp.parent().unwrap_or_else(|| Path::new(""));
        outputs.push(dir.join(format!("{stem}_normalized.{}", fmt_ext(fmt))));
        formats.push(fmt);
    }
    Ok((outputs, formats))
}

fn parse_format(s: &str) -> OutputFormat {
    match s {
        "mp3" => OutputFormat::Mp3,
        _ => OutputFormat::Wav,
    }
}

fn fmt_ext(f: OutputFormat) -> &'static str {
    match f {
        OutputFormat::Wav => "wav",
        OutputFormat::Mp3 => "mp3",
    }
}

fn infer_format(path: &Path) -> Option<OutputFormat> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("mp3") => Some(OutputFormat::Mp3),
        Some("wav") | Some("wave") => Some(OutputFormat::Wav),
        _ => None,
    }
}

/// Default output container for an input we won't otherwise transcode: keep MP3
/// as MP3; everything else (wav/flac/aac/ogg) falls back to lossless WAV since
/// Forge only encodes WAV and MP3.
fn default_format_for_input(path: &Path) -> OutputFormat {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("mp3") => OutputFormat::Mp3,
        _ => OutputFormat::Wav,
    }
}

fn fmt_kind(k: PcmKind) -> &'static str {
    match k {
        PcmKind::U8 => "u8",
        PcmKind::S16 => "s16",
        PcmKind::S24 => "s24",
        PcmKind::S32 => "s32",
        PcmKind::F32 => "f32",
        PcmKind::F64 => "f64",
    }
}

fn print_analysis(path: &Path, an: &normalize::Analysis, gain: Option<f32>) {
    let g = gain.map(|g| 20.0 * (g as f64).log10());
    eprintln!(
        "{:<42} {:>7.1}s {:>2}ch {:>6}Hz {:>4} | LUFS {:>7.2}  RMS {:>7.2}  sPeak {:>7.2}  tPeak {:>7.2} | gain {}",
        path.display().to_string(),
        an.duration_secs(),
        an.channels,
        an.sample_rate,
        fmt_kind(an.kind),
        an.lufs,
        an.rms_db,
        an.sample_peak_db(),
        an.true_peak_db(),
        g.map(|x| format!("{x:+.2} dB")).unwrap_or_else(|| "—".to_string())
    );
}
