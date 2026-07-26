//! Forge: a SIMD-accelerated EBU R128 / ITU-R BS.1770-5 loudness normalizer.

use forge_normalizer::adm::{self, ReferenceRendererOptions};
use forge_normalizer::cli;
use forge_normalizer::dsp::limiter::LimiterConfig;
use forge_normalizer::normalize::{
    self, DialogueSource, DialogueStandard, Mode, OutputFormat, Plan,
};
use forge_normalizer::preset::Preset;
use forge_normalizer::report::{
    self, AnalysisReport, CodecMetadata, ComplianceProfile, TimelineReport,
};
use forge_normalizer::wav::{named_channel_layout, ChannelRole, PcmKind, WavContainer};
use rayon::ThreadPoolBuilder;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use tempfile::{Builder, NamedTempFile, TempDir};

fn main() -> ExitCode {
    let cli = match cli::Cli::parse_with_config() {
        Ok(cli) => cli,
        Err(error) => {
            eprintln!("forge: error: {error}");
            return ExitCode::from(2);
        }
    };
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

fn parse_wav_container(value: &str) -> WavContainer {
    match value {
        "riff" => WavContainer::Riff,
        "rf64" => WavContainer::Rf64,
        "bw64" => WavContainer::Bw64,
        _ => WavContainer::Auto,
    }
}

fn run(mut cli: cli::Cli) -> Result<(), String> {
    let pipeline = PipelineFiles::prepare(&mut cli)?;
    run_paths(cli, pipeline.stdin_requested())?;
    pipeline.emit_stdout()
}

fn run_paths(mut cli: cli::Cli, stdin_requested: bool) -> Result<(), String> {
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
    let channel_roles_override = if cli.dual_mono {
        Some(vec![ChannelRole::DualMono])
    } else {
        cli.channel_layout.as_deref().and_then(named_channel_layout)
    };
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
        wav_container: parse_wav_container(&cli.wav_container),
        bwf: cli.bwf,
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
        return write_loudness_tags(&cli, channel_roles_override.as_deref());
    }

    let (outputs, formats) = resolve_outputs_and_formats(&cli, &relative_paths)?;
    if cli.bits.is_some()
        && formats.contains(&OutputFormat::Flac)
        && !matches!(cli.bits.as_deref(), Some("16" | "24"))
    {
        return Err("FLAC output supports only --bits=16 or --bits=24".into());
    }
    if (cli.bwf || cli.wav_container != "auto")
        && formats.iter().any(|format| *format != OutputFormat::Wav)
    {
        return Err("--bwf and --wav-container are valid only for WAV output".into());
    }

    if cli.analyze_only {
        let start_seconds = cli.start_seconds.unwrap_or(0.0);
        if !start_seconds.is_finite() || start_seconds < 0.0 {
            return Err("--start must be a finite non-negative number".into());
        }
        if cli
            .duration_seconds
            .is_some_and(|value| !value.is_finite() || value <= 0.0)
        {
            return Err("--duration must be a finite positive number".into());
        }
        if cli.timeline.is_some()
            && (!cli.timeline_interval_ms.is_finite() || cli.timeline_interval_ms <= 0.0)
        {
            return Err("--timeline-interval must be a finite positive number".into());
        }
        if cli
            .timeline
            .as_ref()
            .is_some_and(|path| path.as_os_str() == "-")
            && (cli.json
                || cli.ndjson
                || cli.csv.as_ref().is_some_and(|path| path == Path::new("-")))
        {
            return Err("analysis report and timeline cannot both use stdout".into());
        }
        if cli
            .manifest
            .as_ref()
            .is_some_and(|path| path.as_os_str() == "-")
            && (cli.json
                || cli.ndjson
                || cli.csv.as_ref().is_some_and(|path| path == Path::new("-"))
                || cli
                    .timeline
                    .as_ref()
                    .is_some_and(|path| path == Path::new("-")))
        {
            return Err("delivery manifest cannot share stdout with another report".into());
        }
        let compliance = cli
            .compliance
            .as_deref()
            .map(ComplianceProfile::load)
            .transpose()?;
        if stdin_requested && (cli.dialogue_ranges.is_some() || cli.auto_dialogue) {
            return Err("dialogue range analysis cannot be used with stdin".into());
        }
        let dialogue_ranges = cli
            .dialogue_ranges
            .as_deref()
            .map(normalize::load_dialogue_ranges)
            .transpose()?;
        let dialogue_standard = match cli.dialogue_standard.as_str() {
            "auto"
                if compliance
                    .as_ref()
                    .is_some_and(|profile| profile.max_loudness_to_dialogue_ratio_lu.is_some()) =>
            {
                DialogueStandard::EbuR128S4
            }
            "auto" | "atsc-a85" => DialogueStandard::AtscA85,
            "ebu-r128-s4" => DialogueStandard::EbuR128S4,
            _ => unreachable!("clap validates dialogue standards"),
        };
        if dialogue_standard == DialogueStandard::AtscA85
            && compliance
                .as_ref()
                .is_some_and(|profile| profile.max_loudness_to_dialogue_ratio_lu.is_some())
        {
            return Err("LDR compliance requires --dialogue-standard ebu-r128-s4 (or auto)".into());
        }
        let dialogue_source = match cli.dialogue_source.as_str() {
            "mix" => DialogueSource::Mix,
            "center" => DialogueSource::Center,
            "stem" => DialogueSource::Stem,
            _ => unreachable!("clap validates dialogue sources"),
        };
        if dialogue_source == DialogueSource::Stem && cli.dialogue_stem.is_none() {
            return Err("--dialogue-source stem requires --dialogue-stem".into());
        }
        if dialogue_source != DialogueSource::Stem && cli.dialogue_stem.is_some() {
            return Err("--dialogue-stem requires --dialogue-source stem".into());
        }
        if compliance
            .as_ref()
            .is_some_and(ComplianceProfile::requires_dialogue)
            && dialogue_ranges.is_none()
            && !cli.auto_dialogue
        {
            return Err(format!(
                "compliance profile {} requires --dialogue-ranges",
                compliance.as_ref().unwrap().name
            ));
        }
        if cli.dialogue_detection_report.is_some() && cli.inputs.len() != 1 {
            return Err("--dialogue-detection-report requires exactly one input".into());
        }
        if cli.codec_metadata.is_some() && cli.inputs.len() != 1 {
            return Err("--codec-metadata currently requires exactly one input".into());
        }
        if stdin_requested && cli.downmix_qc {
            return Err("--downmix-qc cannot be used with stdin".into());
        }
        if stdin_requested && cli.adm_presentations.is_some() {
            return Err("--adm-presentations cannot be used with stdin".into());
        }
        if stdin_requested && cli.adm_render {
            return Err("--adm-render cannot be used with stdin".into());
        }
        if cli.adm_render && cli.inputs.len() != 1 {
            return Err("--adm-render currently requires exactly one input".into());
        }
        let codec_metadata = cli
            .codec_metadata
            .as_deref()
            .map(CodecMetadata::load)
            .transpose()?;
        let adm_map = cli
            .adm_presentations
            .as_deref()
            .map(normalize::load_adm_presentation_map)
            .transpose()?;
        let mut reports = Vec::with_capacity(cli.inputs.len());
        let mut timeline_reports = Vec::new();
        let mut dialogue_detection_output = None;
        let mut qc_failed = false;
        for input in &cli.inputs {
            let timed = normalize::analyze_file_range_with_roles(
                input,
                channel_roles_override.as_deref(),
                start_seconds,
                cli.duration_seconds,
                cli.timeline.as_ref().map(|_| cli.timeline_interval_ms),
            )?;
            let an = timed.analysis;
            let detection = cli
                .auto_dialogue
                .then(|| {
                    normalize::detect_dialogue_ranges(
                        cli.dialogue_stem.as_deref().unwrap_or(input),
                        channel_roles_override.as_deref(),
                        cli.dialogue_confidence,
                    )
                })
                .transpose()?;
            let detected_ranges = detection
                .as_ref()
                .map(normalize::DialogueDetection::measurement_ranges);
            let active_dialogue_ranges = dialogue_ranges.as_deref().or(detected_ranges.as_deref());
            let dialogue = active_dialogue_ranges
                .map(|ranges| {
                    normalize::analyze_dialogue_ranges_for_standard_with_roles(
                        cli.dialogue_stem.as_deref().unwrap_or(input),
                        channel_roles_override.as_deref(),
                        ranges,
                        dialogue_standard,
                        dialogue_source,
                    )
                })
                .transpose()?;
            if let Some(detection) = detection.clone() {
                dialogue_detection_output = Some(detection);
            }
            let compliance_result = compliance
                .as_ref()
                .map(|profile| {
                    profile.evaluate_with_dialogue(&an, dialogue.as_ref().map(|value| value.lufs))
                })
                .transpose()?;
            if compliance_result
                .as_ref()
                .is_some_and(|result| !result.passed)
            {
                qc_failed = true;
            }
            let downmix = cli
                .downmix_qc
                .then(|| normalize::analyze_stereo_downmix(input))
                .transpose()?;
            let codec_qc = codec_metadata
                .as_ref()
                .map(|metadata| metadata.evaluate(&an, dialogue.as_ref()));
            if codec_qc.as_ref().is_some_and(|result| {
                result.dialnorm_pass == Some(false) || result.encoded_loudness_pass == Some(false)
            }) {
                qc_failed = true;
            }
            let adm_qc = adm_map
                .as_ref()
                .map(|map| {
                    normalize::analyze_adm_presentations(
                        input,
                        channel_roles_override.as_deref(),
                        map,
                    )
                })
                .transpose()?;
            let adm_render = cli
                .adm_render
                .then(|| {
                    adm::validate_and_render(
                        input,
                        cli.adm_rendered_output.as_deref(),
                        &ReferenceRendererOptions {
                            command: cli
                                .adm_renderer
                                .clone()
                                .unwrap_or_else(|| PathBuf::from("eat-process")),
                            layout: cli.adm_layout.clone(),
                            profile_level: cli.adm_profile_level,
                            overwrite: cli.overwrite,
                        },
                    )
                })
                .transpose()?;
            if adm_qc.as_ref().is_some_and(|result| !result.passed) {
                qc_failed = true;
            }
            if cli.json || cli.ndjson || cli.csv.is_some() || cli.manifest.is_some() {
                let mut report = AnalysisReport::with_measurements_at(
                    if stdin_requested {
                        Path::new("-")
                    } else {
                        input
                    },
                    &an,
                    dialogue.as_ref(),
                    compliance.as_ref(),
                    (start_seconds * an.sample_rate as f64).round() / an.sample_rate as f64,
                )?;
                if let Some(downmix) = &downmix {
                    report.downmix_integrated_lufs = Some(downmix.analysis.lufs);
                    report.downmix_true_peak_dbtp = Some(downmix.analysis.true_peak_db());
                    report.downmix_method = Some(downmix.method);
                }
                if let Some(codec) = &codec_qc {
                    report.codec = Some(codec.metadata.codec.clone());
                    report.codec_dialnorm_lkfs = codec.metadata.dialnorm_lkfs;
                    report.codec_encoded_loudness_lufs = codec.metadata.encoded_loudness_lufs;
                    report.codec_downmix_mode = codec.metadata.downmix_mode.clone();
                    report.codec_loudness_basis = Some(codec.loudness_basis);
                    report.codec_dialnorm_deviation_lu = codec.dialnorm_deviation_lu;
                    report.codec_dialnorm_pass = codec.dialnorm_pass;
                    report.codec_encoded_loudness_deviation_lu =
                        codec.encoded_loudness_deviation_lu;
                    report.codec_encoded_loudness_pass = codec.encoded_loudness_pass;
                }
                if let Some(adm) = &adm_qc {
                    report.adm_axml_present = Some(adm.axml_present);
                    report.adm_chna_present = Some(adm.chna_present);
                    report.adm_presentations_json = Some(
                        serde_json::to_string(&adm.presentations)
                            .expect("ADM presentation measurements are serializable"),
                    );
                    report.adm_qc_passed = Some(adm.passed);
                }
                if let Some(render) = &adm_render {
                    report.adm_axml_present = Some(true);
                    report.adm_chna_present = Some(true);
                    report.adm_qc_passed = Some(true);
                    report.adm_render_renderer = Some(render.renderer.clone());
                    report.adm_render_standard = Some(render.renderer_standard);
                    report.adm_render_profile = Some(render.profile_standard);
                    report.adm_render_profile_level = Some(render.profile_level);
                    report.adm_render_layout = Some(render.layout.clone());
                    report.adm_render_validation_passed = Some(true);
                    report.adm_render_integrated_lufs = Some(render.analysis.lufs);
                    report.adm_render_true_peak_dbtp = Some(render.analysis.true_peak_db());
                    report.adm_render_channels = Some(render.analysis.channels);
                    report.adm_render_output_path = render
                        .output_path
                        .as_ref()
                        .map(|path| path.to_string_lossy().into_owned());
                }
                if let Some(detection) = &detection {
                    report.dialogue_detector = Some(detection.detector);
                    report.dialogue_detector_version = Some(detection.detector_version);
                    report.dialogue_detection_threshold = Some(detection.threshold);
                    report.dialogue_detection_ranges_json = Some(
                        serde_json::to_string(&detection.ranges)
                            .expect("dialogue detections are serializable"),
                    );
                }
                reports.push(report);
            } else {
                print_analysis(input, &an, None);
                if let Some(dialogue) = &dialogue {
                    eprintln!(
                        "  dialogue: {:.2} LUFS across {} range(s), {:.3} s\n    source: {:?}\n    standard: {}\n    method: {}\n    LDR: {:.2} LU",
                        dialogue.lufs,
                        dialogue.range_count,
                        dialogue.duration_seconds,
                        dialogue.source,
                        dialogue.standard,
                        dialogue.method,
                        an.lufs - dialogue.lufs,
                    );
                }
                if let Some(detection) = &detection {
                    eprintln!(
                        "  dialogue detector: {} {} threshold {:.2}, {} selected range(s)",
                        detection.detector,
                        detection.detector_version,
                        detection.threshold,
                        detection.ranges.len(),
                    );
                    for range in &detection.ranges {
                        eprintln!(
                            "    {:.3}..{:.3} s confidence {:.3}",
                            range.start_seconds,
                            range.start_seconds + range.duration_seconds,
                            range.confidence,
                        );
                    }
                }
                if let Some(profile) = &compliance {
                    print_compliance(profile, &an, dialogue.as_ref())?;
                }
                if let Some(downmix) = &downmix {
                    eprintln!(
                        "  stereo downmix: {:.2} LUFS, {:.2} dBTP\n    method: {}",
                        downmix.analysis.lufs,
                        downmix.analysis.true_peak_db(),
                        downmix.method
                    );
                }
                if let Some(codec) = &codec_qc {
                    eprintln!(
                        "  codec metadata {} ({} basis): dialnorm deviation {:?} LU [{}], encoded loudness deviation {:?} LU [{}]",
                        codec.metadata.codec,
                        codec.loudness_basis,
                        codec.dialnorm_deviation_lu,
                        qc_status(codec.dialnorm_pass),
                        codec.encoded_loudness_deviation_lu,
                        qc_status(codec.encoded_loudness_pass),
                    );
                }
                if let Some(adm) = &adm_qc {
                    eprintln!(
                        "  ADM QC: axml={} chna={} [{}]",
                        adm.axml_present,
                        adm.chna_present,
                        if adm.passed { "PASS" } else { "FAIL" }
                    );
                    for presentation in &adm.presentations {
                        eprintln!(
                            "    {} {}: {:.2} LUFS, {:.2} dBTP, channels {:?}, axml-ref={} ({})",
                            presentation.id,
                            presentation.name,
                            presentation.integrated_lufs,
                            presentation.true_peak_dbtp,
                            presentation.channels,
                            presentation.referenced_by_axml,
                            presentation.render_method,
                        );
                    }
                }
                if let Some(render) = &adm_render {
                    eprintln!(
                        "  ADM reference render: {:.2} LUFS, {:.2} dBTP, {} ch [PASS]\n    renderer: {} ({})\n    validation: {} level {}\n    layout: {}",
                        render.analysis.lufs,
                        render.analysis.true_peak_db(),
                        render.analysis.channels,
                        render.renderer,
                        render.renderer_standard,
                        render.profile_standard,
                        render.profile_level,
                        render.layout,
                    );
                    if let Some(path) = &render.output_path {
                        eprintln!("    output: {}", path.display());
                    }
                }
            }
            if cli.timeline.is_some() {
                timeline_reports.extend(TimelineReport::from_points(
                    if stdin_requested {
                        Path::new("-")
                    } else {
                        input
                    },
                    &timed.timeline,
                    compliance.as_ref(),
                ));
            }
        }
        if cli.json {
            let stdout = io::stdout();
            let mut output = stdout.lock();
            report::write_json(&mut output, &reports)?;
            writeln!(output).map_err(|error| format!("write stdout: {error}"))?;
        } else if cli.ndjson {
            let stdout = io::stdout();
            report::write_ndjson(stdout.lock(), &reports)?;
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
        if let Some(path) = &cli.timeline {
            write_timeline(path, &timeline_reports)?;
        }
        if let Some(path) = &cli.manifest {
            if path.as_os_str() == "-" {
                let stdout = io::stdout();
                report::write_manifest(stdout.lock(), &reports)?;
                println!();
            } else {
                let file = File::create(path)
                    .map_err(|error| format!("create {}: {error}", path.display()))?;
                report::write_manifest(file, &reports)?;
            }
        }
        if let Some(path) = &cli.dialogue_detection_report {
            let detection = dialogue_detection_output
                .as_ref()
                .expect("auto dialogue always produces a detection result");
            let file = File::create(path)
                .map_err(|error| format!("create {}: {error}", path.display()))?;
            serde_json::to_writer_pretty(file, detection)
                .map_err(|error| format!("write dialogue detection report: {error}"))?;
        }
        if qc_failed {
            return Err("one or more inputs failed the requested compliance/QC checks".into());
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
                .map(|path| {
                    normalize::analyze_file_with_roles(path, channel_roles_override.as_deref())
                })
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
            let corrected = normalize::normalize_album_corrected_with_roles(
                &cli.inputs,
                &outputs,
                &plan,
                &formats,
                cli.verify_tolerance,
                cli.verify_retries as usize,
                channel_roles_override.as_deref(),
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
        let results = normalize::normalize_album_with_roles(
            &cli.inputs,
            &outputs,
            &plan,
            &formats,
            channel_roles_override.as_deref(),
        )?;
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
            let an = normalize::analyze_file_with_roles(input, channel_roles_override.as_deref())?;
            let gain = normalize::compute_gain(&an, &plan);
            print_analysis(input, &an, Some(gain));
            if cli.dry_run {
                eprintln!("  would write {}", output.display());
            }
        } else {
            prepare_output_directories(std::slice::from_ref(output))?;
            if cli.verify {
                let corrected = normalize::normalize_one_corrected_with_roles(
                    input,
                    output,
                    &plan,
                    *fmt,
                    cli.verify_tolerance,
                    cli.verify_retries as usize,
                    channel_roles_override.as_deref(),
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
                let (an, gain) = normalize::normalize_one_with_roles(
                    input,
                    output,
                    &plan,
                    *fmt,
                    channel_roles_override.as_deref(),
                )?;
                print_analysis(input, &an, Some(gain));
            }
        }
    }
    Ok(())
}

fn write_timeline(path: &Path, reports: &[TimelineReport]) -> Result<(), String> {
    let format = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("ndjson")
        .to_ascii_lowercase();
    let write = |writer: &mut dyn Write| match format.as_str() {
        "json" => report::write_timeline_json(writer, reports),
        "csv" => report::write_timeline_csv(writer, reports),
        "ndjson" | "jsonl" => report::write_timeline_ndjson(writer, reports),
        _ => Err("--timeline path must end in .json, .ndjson, .jsonl, or .csv".into()),
    };
    if path.as_os_str() == "-" {
        let stdout = io::stdout();
        let mut output = stdout.lock();
        write(&mut output)
    } else {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
        let mut file =
            File::create(path).map_err(|error| format!("create {}: {error}", path.display()))?;
        write(&mut file)
    }
}

struct PipelineFiles {
    _stdin_file: Option<NamedTempFile>,
    _stdout_directory: Option<TempDir>,
    stdout_path: Option<PathBuf>,
}

impl PipelineFiles {
    fn stdin_requested(&self) -> bool {
        self._stdin_file.is_some()
    }

    fn prepare(cli: &mut cli::Cli) -> Result<Self, String> {
        let stdin_requested = cli.inputs.iter().any(|path| path.as_os_str() == "-");
        let stdout_requested = cli
            .output
            .as_ref()
            .is_some_and(|path| path.as_os_str() == "-");
        let mut stdin_file = None;
        let mut stdout_directory = None;
        let mut stdout_path = None;

        if stdout_requested {
            if cli.inputs.len() != 1 {
                return Err("stdout (`-`) supports exactly one input".into());
            }
            if cli.analyze_only || cli.gain_only || cli.dry_run || cli.write_tags || cli.album {
                return Err(
                    "binary stdout cannot be combined with analysis-only, dry-run, tag, or album modes"
                        .into(),
                );
            }
            if cli.format.is_none() {
                return Err("stdout output requires --format".into());
            }
        }

        if stdin_requested {
            if cli.inputs.len() != 1 {
                return Err("stdin (`-`) must be the only input".into());
            }
            if cli.recursive || cli.album || cli.write_tags {
                return Err(
                    "stdin cannot be combined with --recursive, --album, or --write-tags".into(),
                );
            }
            let format = cli
                .input_format
                .as_deref()
                .ok_or_else(|| "stdin requires --input-format".to_string())?;
            if !cli.analyze_only && cli.output.is_none() {
                return Err("stdin normalization requires an explicit --output".into());
            }
            let mut temporary = Builder::new()
                .prefix("forge-stdin-")
                .suffix(&format!(".{format}"))
                .tempfile()
                .map_err(|error| format!("create stdin spool: {error}"))?;
            io::copy(&mut io::stdin().lock(), temporary.as_file_mut())
                .map_err(|error| format!("read stdin: {error}"))?;
            temporary
                .as_file_mut()
                .flush()
                .map_err(|error| format!("flush stdin spool: {error}"))?;
            cli.inputs[0] = temporary.path().to_owned();
            stdin_file = Some(temporary);
        } else if cli.input_format.is_some() {
            return Err("--input-format is valid only when reading stdin (`-`)".into());
        }

        if stdout_requested {
            let format = cli
                .format
                .as_deref()
                .expect("stdout format validated above");
            let directory =
                tempfile::tempdir().map_err(|error| format!("create stdout spool: {error}"))?;
            let path = directory.path().join(format!("output.{format}"));
            cli.output = Some(path.clone());
            stdout_path = Some(path);
            stdout_directory = Some(directory);
        }

        Ok(Self {
            _stdin_file: stdin_file,
            _stdout_directory: stdout_directory,
            stdout_path,
        })
    }

    fn emit_stdout(&self) -> Result<(), String> {
        let Some(path) = &self.stdout_path else {
            return Ok(());
        };
        let mut source = File::open(path).map_err(|error| format!("open stdout spool: {error}"))?;
        let stdout = io::stdout();
        let mut destination = stdout.lock();
        io::copy(&mut source, &mut destination)
            .map_err(|error| format!("write encoded audio to stdout: {error}"))?;
        destination
            .flush()
            .map_err(|error| format!("flush stdout: {error}"))
    }
}

fn print_compliance(
    profile: &ComplianceProfile,
    analysis: &normalize::Analysis,
    dialogue: Option<&normalize::DialogueMeasurement>,
) -> Result<(), String> {
    let result =
        profile.evaluate_with_dialogue(analysis, dialogue.map(|measurement| measurement.lufs))?;
    eprintln!("  compliance {}:", result.profile);
    for rule in &result.rules {
        let bounds = match (rule.minimum, rule.maximum) {
            (Some(minimum), Some(maximum)) => format!("{minimum:.2}..={maximum:.2}"),
            (Some(minimum), None) => format!(">= {minimum:.2}"),
            (None, Some(maximum)) => format!("<= {maximum:.2}"),
            (None, None) => "unbounded".into(),
        };
        eprintln!(
            "    {}: {:.2} ({}) [{}]",
            rule.metric,
            rule.measured,
            bounds,
            if rule.passed { "PASS" } else { "FAIL" }
        );
    }
    eprintln!(
        "    result: {}",
        if result.passed { "PASS" } else { "FAIL" }
    );
    Ok(())
}

fn qc_status(result: Option<bool>) -> &'static str {
    match result {
        Some(true) => "PASS",
        Some(false) => "FAIL",
        None => "N/A",
    }
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

fn write_loudness_tags(
    cli: &cli::Cli,
    channel_roles: Option<&[forge_normalizer::wav::ChannelRole]>,
) -> Result<(), String> {
    let analyses: Vec<_> = cli
        .inputs
        .iter()
        .map(|path| normalize::analyze_file_with_roles(path, channel_roles))
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
        Some("wav" | "wave" | "mp3" | "flac" | "aac" | "m4a" | "mp4" | "ogg" | "opus")
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
        "opus" => OutputFormat::Opus,
        "m4a" => OutputFormat::M4a,
        _ => OutputFormat::Wav,
    }
}

fn fmt_ext(f: OutputFormat) -> &'static str {
    match f {
        OutputFormat::Wav => "wav",
        OutputFormat::Flac => "flac",
        OutputFormat::Mp3 => "mp3",
        OutputFormat::Opus => "opus",
        OutputFormat::M4a => "m4a",
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
        Some("opus") => Some(OutputFormat::Opus),
        Some("m4a") | Some("mp4") => Some(OutputFormat::M4a),
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
        Some("opus") => OutputFormat::Opus,
        Some("m4a") | Some("mp4") => {
            #[cfg(feature = "aac-encoding")]
            {
                OutputFormat::M4a
            }
            #[cfg(not(feature = "aac-encoding"))]
            {
                OutputFormat::Wav
            }
        }
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
        "{:<42} Max M {:>7.2}  Max S {:>7.2}  LRA {:>6.2} LU{}  PLR {:>6.2} LU",
        "",
        an.max_momentary_lufs,
        an.max_short_term_lufs,
        an.loudness_range_lu,
        if an.loudness_range_stable() {
            ""
        } else {
            " (provisional: <60 s)"
        },
        an.peak_to_loudness_ratio_lu(),
    );
}
