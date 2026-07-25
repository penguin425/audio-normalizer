//! Forge: a SIMD-accelerated EBU R128 / ITU-R BS.1770-5 loudness normalizer.

use clap::Parser;
use forge_normalizer::cli;
use forge_normalizer::dsp::limiter::LimiterConfig;
use forge_normalizer::normalize::{self, Mode, OutputFormat, Plan};
use forge_normalizer::preset::Preset;
use forge_normalizer::report::{self, AnalysisReport};
use forge_normalizer::wav::PcmKind;
use rayon::ThreadPoolBuilder;
use std::fs::File;
use std::io::{self, Write};
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

fn run(mut cli: cli::Cli) -> Result<(), String> {
    if let Some(j) = cli.jobs {
        ThreadPoolBuilder::new()
            .num_threads(j)
            .build_global()
            .map_err(|e| format!("thread pool: {e}"))?;
    }

    let (expanded, relative_paths) = expand_inputs(&cli.inputs, cli.recursive)?;
    cli.inputs = expanded;

    let preset = cli
        .preset
        .as_deref()
        .map(|name| Preset::named(name).expect("clap validates preset names"));
    let plan = Plan {
        mode: if preset.is_some() {
            Mode::Lufs
        } else {
            parse_mode(&cli.mode)
        },
        target_lufs: preset.map_or(cli.target_lufs, |value| value.target_lufs),
        target_peak_db: cli.target_peak_db,
        target_rms_db: cli.target_rms_db,
        ceiling_db: preset.map_or(cli.ceiling_db, |value| value.ceiling_db),
        max_gain_db: cli.max_gain_db,
        dither: cli.dither,
        output_kind: cli.bits.as_deref().map(parse_bits),
        mp3_bitrate: cli.bitrate,
        mp3_quality: cli.quality,
        limiter: cli.limiter.then_some(LimiterConfig {
            lookahead_ms: cli.limiter_lookahead,
            release_ms: cli.limiter_release,
        }),
    };
    if let Some(preset) = preset {
        eprintln!(
            "preset {}: {:.1} LUFS, {:.1} dBTP ({})",
            preset.name, preset.target_lufs, preset.ceiling_db, preset.description
        );
    }

    if cli.album && plan.mode != Mode::Lufs {
        return Err("--album is only valid with --mode lufs".into());
    }
    if !cli.verify_tolerance.is_finite() || cli.verify_tolerance < 0.0 {
        return Err("--verify-tolerance must be a finite non-negative number".into());
    }
    if cli.limiter
        && (!cli.limiter_lookahead.is_finite()
            || cli.limiter_lookahead < 1.0
            || !cli.limiter_release.is_finite()
            || cli.limiter_release <= 0.0)
    {
        return Err(
            "--limiter-lookahead must be >= 1 ms and --limiter-release must be > 0 ms".into(),
        );
    }

    if cli.write_tags {
        return write_loudness_tags(&cli);
    }

    let (outputs, formats) = resolve_outputs_and_formats(&cli, &relative_paths)?;
    if cli.bits.is_some()
        && formats.contains(&OutputFormat::Flac)
        && !matches!(cli.bits.as_deref(), Some("16" | "24"))
    {
        return Err("FLAC output supports only --bits=16 or --bits=24".into());
    }

    if cli.analyze_only {
        let mut reports = Vec::with_capacity(cli.inputs.len());
        for input in &cli.inputs {
            let an = normalize::analyze_file(input)?;
            if cli.json || cli.csv.is_some() {
                reports.push(AnalysisReport::new(input, &an));
            } else {
                print_analysis(input, &an, None);
            }
        }
        if cli.json {
            let stdout = io::stdout();
            let mut output = stdout.lock();
            report::write_json(&mut output, &reports)?;
            writeln!(output).map_err(|error| format!("write stdout: {error}"))?;
        } else if let Some(path) = &cli.csv {
            if path.as_os_str() == "-" {
                let stdout = io::stdout();
                report::write_csv(stdout.lock(), &reports)?;
            } else {
                let file = File::create(path)
                    .map_err(|error| format!("create {}: {error}", path.display()))?;
                report::write_csv(file, &reports)?;
            }
        }
        return Ok(());
    }

    if !cli.gain_only {
        validate_outputs(&cli.inputs, &outputs, cli.overwrite)?;
    }

    if cli.album {
        if cli.dry_run {
            let analyses: Vec<_> = cli
                .inputs
                .iter()
                .map(normalize::analyze_file)
                .collect::<Result<_, _>>()?;
            let gain = normalize::album_gain(&analyses, &plan);
            for ((input, output), analysis) in
                cli.inputs.iter().zip(outputs.iter()).zip(analyses.iter())
            {
                print_analysis(input, analysis, Some(gain));
                eprintln!("  would write {}", output.display());
            }
            return Ok(());
        }
        prepare_output_directories(&outputs)?;
        if cli.verify {
            let corrected = normalize::normalize_album_corrected(
                &cli.inputs,
                &outputs,
                &plan,
                &formats,
                cli.verify_tolerance,
                cli.verify_retries as usize,
            )?;
            for (input, source) in cli.inputs.iter().zip(&corrected.sources) {
                print_analysis(input, source, Some(corrected.gain));
            }
            let source_album = normalize::album_lufs(&corrected.sources);
            eprintln!(
                "album: {:.2} LUFS  shared gain {:+.2} dB",
                source_album,
                20.0 * (corrected.gain as f64).log10()
            );
            for ((input, verification), output) in cli
                .inputs
                .iter()
                .zip(&corrected.verifications)
                .zip(&outputs)
            {
                if !print_verification(input, verification, &plan) {
                    return Err(format!(
                        "post-encode verification failed: {}",
                        output.display()
                    ));
                }
            }
            let album_deviation =
                (corrected.actual_album_lufs - corrected.expected_album_lufs).abs();
            let album_ok = album_deviation <= cli.verify_tolerance;
            eprintln!(
                "album verification: expected {:.2} LUFS, measured {:.2} LUFS, \
                 deviation {:.2} LU [{}]",
                corrected.expected_album_lufs,
                corrected.actual_album_lufs,
                album_deviation,
                if album_ok { "PASS" } else { "FAIL" }
            );
            if corrected.attempts > 1 {
                eprintln!(
                    "album correction: {} re-encode pass(es)",
                    corrected.attempts - 1
                );
            }
            if !album_ok {
                return Err("post-encode album verification failed".into());
            }
            return Ok(());
        }
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
        if cli.gain_only || cli.dry_run {
            let an = normalize::analyze_file(input)?;
            let gain = normalize::compute_gain(&an, &plan);
            print_analysis(input, &an, Some(gain));
            if cli.dry_run {
                eprintln!("  would write {}", output.display());
            }
        } else {
            prepare_output_directories(std::slice::from_ref(output))?;
            if cli.verify {
                let corrected = normalize::normalize_one_corrected(
                    input,
                    output,
                    &plan,
                    *fmt,
                    cli.verify_tolerance,
                    cli.verify_retries as usize,
                )?;
                print_analysis(input, &corrected.source, Some(corrected.gain));
                if !print_verification(input, &corrected.verification, &plan) {
                    return Err(format!(
                        "post-encode verification failed: {}",
                        output.display()
                    ));
                }
                if corrected.attempts > 1 {
                    eprintln!(
                        "{} correction: {} re-encode pass(es)",
                        input.display(),
                        corrected.attempts - 1
                    );
                }
            } else {
                let (an, gain) = normalize::normalize_one(input, output, &plan, *fmt)?;
                print_analysis(input, &an, Some(gain));
            }
        }
    }
    Ok(())
}

fn print_verification(input: &Path, verification: &normalize::Verification, plan: &Plan) -> bool {
    let unit = match plan.mode {
        Mode::Lufs => "LUFS",
        Mode::Peak | Mode::Rms => "dBFS",
    };
    eprintln!(
        "{} verification: expected {:.2} {unit}, measured {:.2} {unit}, deviation \
         {:.2} dB [{}]; true peak {:.2} dBTP [{}]",
        input.display(),
        verification.expected_level,
        verification.actual_level,
        verification.deviation,
        if verification.level_ok {
            "PASS"
        } else {
            "FAIL"
        },
        verification.output.true_peak_db(),
        if verification.true_peak_ok {
            "PASS"
        } else {
            "FAIL"
        }
    );
    verification.passed()
}

fn write_loudness_tags(cli: &cli::Cli) -> Result<(), String> {
    let analyses: Vec<_> = cli
        .inputs
        .iter()
        .map(normalize::analyze_file)
        .collect::<Result<_, _>>()?;
    let album = if cli.album {
        Some((
            normalize::album_lufs(&analyses),
            analyses
                .iter()
                .map(|analysis| analysis.true_peak)
                .fold(0.0_f32, f32::max),
        ))
    } else {
        None
    };
    for (input, analysis) in cli.inputs.iter().zip(&analyses) {
        print_analysis(input, analysis, None);
        if cli.dry_run {
            eprintln!("  would write ReplayGain tags");
        } else {
            forge_normalizer::metadata::write_replaygain(
                input,
                analysis.lufs,
                analysis.true_peak,
                album,
            )?;
            eprintln!("  wrote ReplayGain tags");
        }
    }
    Ok(())
}

fn expand_inputs(
    inputs: &[PathBuf],
    recursive: bool,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>), String> {
    let mut expanded = Vec::new();
    let mut relative = Vec::new();
    for input in inputs {
        if input.is_file() {
            expanded.push(input.clone());
            relative.push(
                input
                    .file_name()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("input")),
            );
        } else if input.is_dir() {
            if !recursive {
                return Err(format!(
                    "{} is a directory; use --recursive",
                    input.display()
                ));
            }
            collect_audio_files(input, input, &mut expanded, &mut relative)?;
        } else {
            return Err(format!("input does not exist: {}", input.display()));
        }
    }
    if expanded.is_empty() {
        return Err("no supported audio files found".into());
    }
    Ok((expanded, relative))
}

fn collect_audio_files(
    root: &Path,
    directory: &Path,
    expanded: &mut Vec<PathBuf>,
    relative: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let mut entries: Vec<_> = std::fs::read_dir(directory)
        .map_err(|error| format!("read {}: {error}", directory.display()))?
        .collect::<Result<_, _>>()
        .map_err(|error| format!("read {}: {error}", directory.display()))?;
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_audio_files(root, &path, expanded, relative)?;
        } else if path.is_file() && is_supported_input(&path) {
            relative.push(
                path.strip_prefix(root)
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| path.clone()),
            );
            expanded.push(path);
        }
    }
    Ok(())
}

fn is_supported_input(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("wav" | "wave" | "mp3" | "flac" | "aac" | "m4a" | "mp4" | "ogg")
    )
}

fn validate_outputs(
    inputs: &[PathBuf],
    outputs: &[PathBuf],
    overwrite: bool,
) -> Result<(), String> {
    for (input, output) in inputs.iter().zip(outputs) {
        if input == output {
            return Err(format!("refusing to overwrite input: {}", input.display()));
        }
        if output.exists() && !overwrite {
            return Err(format!(
                "output already exists: {} (use --overwrite)",
                output.display()
            ));
        }
    }
    Ok(())
}

fn prepare_output_directories(outputs: &[PathBuf]) -> Result<(), String> {
    for output in outputs {
        if let Some(parent) = output
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
    }
    Ok(())
}

fn resolve_outputs_and_formats(
    cli: &cli::Cli,
    relative_paths: &[PathBuf],
) -> Result<(Vec<PathBuf>, Vec<OutputFormat>), String> {
    let explicit = cli.format.as_deref().map(parse_format);
    let mut outputs = Vec::with_capacity(cli.inputs.len());
    let mut formats = Vec::with_capacity(cli.inputs.len());

    if let Some(out) = &cli.output {
        if out.is_dir() || (!out.exists() && (cli.inputs.len() > 1 || cli.recursive)) {
            for (index, inp) in cli.inputs.iter().enumerate() {
                let fmt = explicit.unwrap_or_else(|| default_format_for_input(inp));
                let relative = relative_paths.get(index).unwrap_or(inp);
                let stem = relative
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("out");
                let parent = relative.parent().unwrap_or_else(|| Path::new(""));
                outputs.push(
                    out.join(parent)
                        .join(format!("{stem}_normalized.{}", fmt_ext(fmt))),
                );
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
        "flac" => OutputFormat::Flac,
        "mp3" => OutputFormat::Mp3,
        _ => OutputFormat::Wav,
    }
}

fn fmt_ext(f: OutputFormat) -> &'static str {
    match f {
        OutputFormat::Wav => "wav",
        OutputFormat::Flac => "flac",
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
        Some("flac") => Some(OutputFormat::Flac),
        Some("mp3") => Some(OutputFormat::Mp3),
        Some("wav") | Some("wave") => Some(OutputFormat::Wav),
        _ => None,
    }
}

/// Keep lossless FLAC and MP3 inputs in their original containers; other
/// decoded formats fall back to lossless WAV.
fn default_format_for_input(path: &Path) -> OutputFormat {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("flac") => OutputFormat::Flac,
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
    eprintln!(
        "{:<42} Max M {:>7.2}  Max S {:>7.2}  LRA {:>6.2} LU",
        "", an.max_momentary_lufs, an.max_short_term_lufs, an.loudness_range_lu,
    );
}
