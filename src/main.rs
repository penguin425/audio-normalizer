//! Forge: a SIMD-accelerated EBU R128 / ITU-R BS.1770-5 loudness normalizer.

use clap::{Arg, ArgAction, CommandFactory};
use forge_normalizer::adm::{self, ReferenceRendererOptions};
use forge_normalizer::analysis::Analysis;
use forge_normalizer::analysis_cache::{
    AnalysisCache, AnalysisCachePolicy, CacheDisposition, Cached,
};
use forge_normalizer::batch::{BatchAssetSpec, BatchJob, BatchProgressEvent};
use forge_normalizer::catalogue::{Catalogue, CatalogueAsset, CatalogueRecord};
use forge_normalizer::cli;
use forge_normalizer::codec_qc;
use forge_normalizer::dsp::limiter::LimiterConfig;
use forge_normalizer::dsp::resample::ResampleQuality;
use forge_normalizer::normalization_diff::{
    self, NormalizationDifferenceAsset, NormalizationDifferenceReport,
};
use forge_normalizer::normalize::{
    self, DialogueSource, DialogueStandard, Mode, OutputFormat, Plan,
};
use forge_normalizer::preset::Preset;
use forge_normalizer::qc::{self, QcOptions};
use forge_normalizer::report::{
    self, AnalysisReport, CodecMetadata, ComplianceProfile, TimelineReport,
};
use forge_normalizer::watch::{WatchCandidate, WatchFolder};
use forge_normalizer::wav::{named_channel_layout, ChannelRole, PcmKind, WavContainer};
use rayon::ThreadPoolBuilder;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;
use tempfile::{Builder, NamedTempFile, TempDir};

#[derive(Debug, Default)]
struct BatchOptions {
    job_state: Option<PathBuf>,
    progress: Option<PathBuf>,
}

#[derive(Debug, Default, Clone)]
struct CacheOptions {
    directory: Option<PathBuf>,
    read_only: bool,
    max_mib: Option<u64>,
}

#[derive(Debug, Default)]
struct WatchOptions {
    enabled: bool,
    state: Option<PathBuf>,
    stable_seconds: Option<u64>,
    poll_seconds: Option<u64>,
    once: bool,
    retry_failed: bool,
}

#[derive(Debug, Default)]
struct CatalogueOptions {
    database: Option<PathBuf>,
    report: Option<PathBuf>,
}

impl CacheOptions {
    fn open(&self) -> Result<Option<AnalysisCache>, String> {
        let Some(directory) = &self.directory else {
            return Ok(None);
        };
        let max_mib = self.max_mib.unwrap_or(1024);
        let max_bytes = max_mib
            .checked_mul(1024 * 1024)
            .ok_or_else(|| "--analysis-cache-max-mib is too large".to_string())?;
        AnalysisCache::new(
            directory,
            AnalysisCachePolicy {
                read_only: self.read_only,
                max_bytes,
            },
        )
        .map(Some)
    }
}

fn main() -> ExitCode {
    let matches = cli::Cli::command()
        .arg(
            Arg::new("job_state")
                .long("job-state")
                .value_name("PATH")
                .value_parser(clap::value_parser!(PathBuf))
                .help(
                    "Atomically checkpoint a multi-file normalization job and resume an identical invocation",
                )
                .conflicts_with_all([
                    "analyze_only",
                    "album",
                    "dry_run",
                    "gain_only",
                    "write_tags",
                    "difference_report",
                ]),
        )
        .arg(
            Arg::new("progress")
                .long("progress")
                .value_name("PATH")
                .value_parser(clap::value_parser!(PathBuf))
                .help("Write versioned normalization lifecycle events as NDJSON (`-` for stdout)")
                .conflicts_with_all([
                    "analyze_only",
                    "album",
                    "dry_run",
                    "gain_only",
                    "write_tags",
                ]),
        )
        .arg(
            Arg::new("analysis_cache")
                .long("analysis-cache")
                .value_name("DIR")
                .value_parser(clap::value_parser!(PathBuf))
                .help("Reuse content-addressed, versioned loudness analyses from DIR"),
        )
        .arg(
            Arg::new("analysis_cache_read_only")
                .long("analysis-cache-read-only")
                .action(ArgAction::SetTrue)
                .requires("analysis_cache")
                .help("Read cache hits but never create, repair, or evict entries"),
        )
        .arg(
            Arg::new("analysis_cache_max_mib")
                .long("analysis-cache-max-mib")
                .value_name("MIB")
                .value_parser(clap::value_parser!(u64).range(1..))
                .requires("analysis_cache")
                .help("Bound recognized cache entries (default: 1024 MiB)"),
        )
        .arg(
            Arg::new("watch")
                .long("watch")
                .action(ArgAction::SetTrue)
                .help("Continuously normalize stable files discovered below the input directory")
                .conflicts_with_all([
                    "analyze_only",
                    "album",
                    "dry_run",
                    "gain_only",
                    "write_tags",
                    "start_seconds",
                    "duration_seconds",
                    "timeline",
                    "compliance",
                    "dialogue_ranges",
                    "auto_dialogue",
                    "codec_qc",
                    "downmix_qc",
                    "manifest",
                    "ebu_qc",
                    "difference_report",
                    "job_state",
                    "progress",
                ]),
        )
        .arg(
            Arg::new("watch_state")
                .long("watch-state")
                .value_name("PATH")
                .value_parser(clap::value_parser!(PathBuf))
                .requires("watch")
                .help("Atomically persist stable-file observations and processing results"),
        )
        .arg(
            Arg::new("watch_stable_seconds")
                .long("watch-stable-seconds")
                .value_name("SECONDS")
                .value_parser(clap::value_parser!(u64).range(1..))
                .requires("watch")
                .help("Require unchanged size and modification time for this interval (default: 5)"),
        )
        .arg(
            Arg::new("watch_poll_seconds")
                .long("watch-poll-seconds")
                .value_name("SECONDS")
                .value_parser(clap::value_parser!(u64).range(1..))
                .requires("watch")
                .help("Delay between directory scans (default: 2)"),
        )
        .arg(
            Arg::new("watch_once")
                .long("watch-once")
                .action(ArgAction::SetTrue)
                .requires("watch")
                .help("Scan once, process files already stable in durable state, and exit"),
        )
        .arg(
            Arg::new("watch_retry_failed")
                .long("watch-retry-failed")
                .action(ArgAction::SetTrue)
                .requires("watch")
                .help("Requeue unchanged failed entries once at startup"),
        )
        .arg(
            Arg::new("catalogue")
                .long("catalogue")
                .value_name("PATH")
                .value_parser(clap::value_parser!(PathBuf))
                .conflicts_with_all(["dry_run", "gain_only", "write_tags", "watch"])
                .help(
                    "Record content hashes, measurements, profile, tool version, and provenance in SQLite",
                ),
        )
        .arg(
            Arg::new("catalogue_report")
                .long("catalogue-report")
                .value_name("PATH")
                .value_parser(clap::value_parser!(PathBuf))
                .requires("catalogue")
                .help("Atomically export records committed by this invocation as JSON"),
        )
        .arg(
            Arg::new("anomaly_audit")
                .long("anomaly-audit")
                .value_name("PATH")
                .value_parser(clap::value_parser!(PathBuf))
                .action(ArgAction::Append)
                .requires("analyze_only")
                .requires("manifest")
                .conflicts_with("watch")
                .help(
                    "Attach one validated forge-anomaly-provider audit per analyzed input in input order",
                ),
        )
        .get_matches();
    let batch_options = BatchOptions {
        job_state: matches.get_one::<PathBuf>("job_state").cloned(),
        progress: matches.get_one::<PathBuf>("progress").cloned(),
    };
    let cache_options = CacheOptions {
        directory: matches.get_one::<PathBuf>("analysis_cache").cloned(),
        read_only: matches.get_flag("analysis_cache_read_only"),
        max_mib: matches.get_one::<u64>("analysis_cache_max_mib").copied(),
    };
    let watch_options = WatchOptions {
        enabled: matches.get_flag("watch"),
        state: matches.get_one::<PathBuf>("watch_state").cloned(),
        stable_seconds: matches.get_one::<u64>("watch_stable_seconds").copied(),
        poll_seconds: matches.get_one::<u64>("watch_poll_seconds").copied(),
        once: matches.get_flag("watch_once"),
        retry_failed: matches.get_flag("watch_retry_failed"),
    };
    let catalogue_options = CatalogueOptions {
        database: matches.get_one::<PathBuf>("catalogue").cloned(),
        report: matches.get_one::<PathBuf>("catalogue_report").cloned(),
    };
    let cli = match cli::Cli::from_matches_with_config(&matches) {
        Ok(cli) => cli,
        Err(error) => {
            eprintln!("forge: error: {error}");
            return ExitCode::from(2);
        }
    };
    let anomaly_audits = matches
        .get_many::<PathBuf>("anomaly_audit")
        .map(|values| values.cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    if let Err(e) = run(
        cli,
        batch_options,
        cache_options,
        watch_options,
        catalogue_options,
        anomaly_audits,
    ) {
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

fn run(
    mut cli: cli::Cli,
    batch_options: BatchOptions,
    cache_options: CacheOptions,
    watch_options: WatchOptions,
    catalogue_options: CatalogueOptions,
    anomaly_audits: Vec<PathBuf>,
) -> Result<(), String> {
    if watch_options.enabled {
        return run_watch(cli, cache_options, watch_options, anomaly_audits);
    }
    let pipeline = PipelineFiles::prepare(&mut cli, &batch_options)?;
    run_paths(
        cli,
        pipeline.stdin_requested(),
        &batch_options,
        &cache_options,
        &catalogue_options,
        &anomaly_audits,
    )?;
    pipeline.emit_stdout()
}

fn run_watch(
    mut cli: cli::Cli,
    cache_options: CacheOptions,
    options: WatchOptions,
    anomaly_audits: Vec<PathBuf>,
) -> Result<(), String> {
    if !anomaly_audits.is_empty() {
        return Err("--anomaly-audit cannot be used with --watch".into());
    }
    if cli.inputs.len() != 1 || !cli.inputs[0].is_dir() {
        return Err("--watch requires exactly one input directory".into());
    }
    let output_root = cli
        .output
        .clone()
        .ok_or_else(|| "--watch requires --output DIR".to_string())?;
    if output_root.exists() && !output_root.is_dir() {
        return Err(format!(
            "--watch output is not a directory: {}",
            output_root.display()
        ));
    }
    std::fs::create_dir_all(&output_root)
        .map_err(|error| format!("create {}: {error}", output_root.display()))?;
    let output_root = std::fs::canonicalize(&output_root)
        .map_err(|error| format!("canonicalize {}: {error}", output_root.display()))?;
    let state = options
        .state
        .ok_or_else(|| "--watch requires --watch-state PATH".to_string())?;
    let stable_seconds = options.stable_seconds.unwrap_or(5);
    let poll_seconds = options.poll_seconds.unwrap_or(2);
    if let Some(jobs) = cli.jobs.take() {
        ThreadPoolBuilder::new()
            .num_threads(jobs)
            .build_global()
            .map_err(|error| format!("thread pool: {error}"))?;
    }
    let operation = watch_operation_descriptor(&cli);
    let mut watch = WatchFolder::open(
        state,
        &cli.inputs[0],
        &output_root,
        cli.recursive,
        Duration::from_secs(stable_seconds),
        operation,
    )?;
    if options.retry_failed {
        let retried = watch.retry_failed()?;
        if retried != 0 {
            eprintln!("watch: requeued {retried} failed file(s)");
        }
    }
    loop {
        let candidates = watch.scan()?;
        let mut failures = Vec::new();
        for candidate in candidates {
            if let Err(error) =
                process_watch_candidate(&cli, &cache_options, &mut watch, &candidate, &output_root)
            {
                watch.mark_failed(&candidate.id, &error)?;
                eprintln!("watch failed: {}: {error}", candidate.input.display());
                failures.push(error);
            }
        }
        if options.once {
            return if failures.is_empty() {
                Ok(())
            } else {
                Err(format!("{} watched file(s) failed", failures.len()))
            };
        }
        std::thread::sleep(Duration::from_secs(poll_seconds));
    }
}

fn process_watch_candidate(
    template: &cli::Cli,
    cache_options: &CacheOptions,
    watch: &mut WatchFolder,
    candidate: &WatchCandidate,
    output_root: &Path,
) -> Result<(), String> {
    let format = template
        .format
        .as_deref()
        .map(parse_format)
        .unwrap_or_else(|| default_format_for_input(&candidate.input));
    let stem = candidate
        .relative
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("out");
    let parent = candidate.relative.parent().unwrap_or_else(|| Path::new(""));
    let output = output_root
        .join(parent)
        .join(format!("{stem}_normalized.{}", fmt_ext(format)));
    if let Some(directory) = output.parent() {
        std::fs::create_dir_all(directory)
            .map_err(|error| format!("create {}: {error}", directory.display()))?;
    }
    let output = watch.mark_processing(&candidate.id, &output)?;
    let mut cli = template.clone();
    cli.inputs = vec![candidate.input.clone()];
    cli.output = Some(output);
    cli.recursive = false;
    cli.overwrite = true;
    let result = run_paths(
        cli,
        false,
        &BatchOptions::default(),
        cache_options,
        &CatalogueOptions::default(),
        &[],
    );
    match result {
        Ok(()) => watch.mark_completed(&candidate.id),
        Err(error) => Err(error),
    }
}

fn watch_operation_descriptor(cli: &cli::Cli) -> serde_json::Value {
    serde_json::json!({
        "schema": "forge-watch-operation-v1",
        "generator": format!("forge-normalizer/{}", env!("CARGO_PKG_VERSION")),
        "preset": cli.preset,
        "mode": cli.mode,
        "target_lufs": cli.target_lufs,
        "target_peak_dbfs": cli.target_peak_db,
        "target_rms_dbfs": cli.target_rms_db,
        "ceiling_dbtp": cli.ceiling_db,
        "max_gain_db": cli.max_gain_db,
        "format": cli.format,
        "sample_rate_hz": cli.sample_rate_hz,
        "resample_quality": cli.resample_quality,
        "bitrate_kbps": cli.bitrate,
        "encoder_quality": cli.quality,
        "channel_layout": cli.channel_layout,
        "dual_mono": cli.dual_mono,
        "verify": cli.verify,
        "verify_tolerance": cli.verify_tolerance,
        "verify_retries": cli.verify_retries,
        "limiter": cli.limiter,
        "limiter_lookahead_ms": cli.limiter_lookahead,
        "limiter_release_ms": cli.limiter_release,
        "dither": cli.dither,
        "bits": cli.bits,
        "wav_container": cli.wav_container,
        "bwf": cli.bwf,
    })
}

fn run_paths(
    mut cli: cli::Cli,
    stdin_requested: bool,
    batch_options: &BatchOptions,
    cache_options: &CacheOptions,
    catalogue_options: &CatalogueOptions,
    anomaly_audit_paths: &[PathBuf],
) -> Result<(), String> {
    if let Some(j) = cli.jobs {
        ThreadPoolBuilder::new()
            .num_threads(j)
            .build_global()
            .map_err(|e| format!("thread pool: {e}"))?;
    }

    let (expanded, relative_paths) = expand_inputs(&cli.inputs, cli.recursive)?;
    cli.inputs = expanded;

    let anomaly_audits = if anomaly_audit_paths.is_empty() {
        Vec::new()
    } else {
        if cli.manifest.is_none() {
            return Err("--anomaly-audit requires --manifest".into());
        }
        if stdin_requested || cli.inputs.iter().any(|input| input == Path::new("-")) {
            return Err("--anomaly-audit cannot be used with stdin".into());
        }
        if anomaly_audit_paths.len() != cli.inputs.len() {
            return Err(format!(
                "--anomaly-audit was supplied {} time(s), but {} analyzed input(s) were found; provide one audit per input in input order",
                anomaly_audit_paths.len(),
                cli.inputs.len()
            ));
        }
        anomaly_audit_paths
            .iter()
            .map(|path| forge_normalizer::anomaly_provider::load_audit(path))
            .map(|result| result.map(Some))
            .collect::<Result<Vec<_>, _>>()?
    };

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
        output_sample_rate: cli.sample_rate_hz,
        resample_quality: ResampleQuality::parse(&cli.resample_quality),
    };
    let analysis_cache = cache_options.open()?;
    if let Some(preset) = preset {
        eprintln!(
            "preset {}: {:.1} LUFS, {:.1} dBTP ({})",
            preset.name, preset.target_lufs, preset.ceiling_db, preset.description
        );
        if let Some(source) = preset.provenance {
            let source_date = source
                .source_date
                .map_or(String::new(), |date| format!(", source dated {date}"));
            eprintln!(
                "profile evidence: {}; source {} (checked {}{})",
                source.evidence.as_str(),
                source.source_url,
                source.checked_on,
                source_date
            );
            eprintln!("profile caveat: {}", source.caveat);
        }
    }

    if cli.album && plan.mode != Mode::Lufs {
        return Err("--album is only valid with --mode lufs".into());
    }
    if !cli.verify_tolerance.is_finite() || cli.verify_tolerance < 0.0 {
        return Err("--verify-tolerance must be a finite non-negative number".into());
    }
    if !cli.codec_qc_tolerance.is_finite() || cli.codec_qc_tolerance < 0.0 {
        return Err("--codec-qc-tolerance must be a finite non-negative number".into());
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
        return write_loudness_tags(
            &cli,
            channel_roles_override.as_deref(),
            analysis_cache.as_ref(),
        );
    }

    let (outputs, formats) = resolve_outputs_and_formats(&cli, &relative_paths)?;
    validate_catalogue_paths(&cli, catalogue_options, &outputs, stdin_requested)?;
    let mut catalogue = catalogue_options
        .database
        .as_ref()
        .map(Catalogue::open)
        .transpose()?;
    let mut catalogue_records = Vec::new();
    let catalogue_source_hashes = if catalogue.is_some() {
        cli.inputs
            .iter()
            .map(|input| {
                normalization_diff::inspect_file(input)
                    .map(|evidence| (input.clone(), evidence.sha256))
            })
            .collect::<Result<HashMap<_, _>, _>>()?
    } else {
        HashMap::new()
    };
    if let Some(path) = &cli.difference_report {
        if path == Path::new("-") {
            return Err("--difference-report requires a file path; stdout is not supported".into());
        }
        if outputs.iter().any(|output| output == path) {
            return Err("--difference-report must not overwrite an audio output".into());
        }
        if cli.inputs.iter().any(|input| input == path) {
            return Err("--difference-report must not overwrite an input".into());
        }
        if path.exists() && !cli.overwrite {
            return Err(format!(
                "{} already exists (use --overwrite to replace it)",
                path.display()
            ));
        }
    }
    let mut difference_inputs = if cli.difference_report.is_some() {
        cli.inputs
            .iter()
            .map(|input| normalization_diff::inspect_file(input))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    if stdin_requested {
        if let Some(input) = difference_inputs.first_mut() {
            input.path = "-".into();
        }
    }
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
    if cli.sample_rate_hz.is_some_and(|rate| rate != 48_000)
        && formats.contains(&OutputFormat::Opus)
    {
        return Err("Ogg Opus output supports only --sample-rate 48000".into());
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
        if cli.codec_reference.is_some() && cli.inputs.len() != 1 {
            return Err("--codec-reference requires exactly one input".into());
        }
        if stdin_requested && cli.codec_qc {
            return Err("--codec-qc cannot be used with stdin".into());
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
        if stdin_requested && cli.adm_profile.is_some() {
            return Err("--adm-profile cannot be used with stdin".into());
        }
        if cli.adm_render && cli.inputs.len() != 1 {
            return Err("--adm-render currently requires exactly one input".into());
        }
        if cli.adm_profile_report.is_some() && cli.inputs.len() != 1 {
            return Err("--adm-profile-report requires exactly one input".into());
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
        let ebu_qc_options = cli.ebu_qc.then_some(QcOptions {
            silence_threshold_dbfs: cli.silence_threshold_dbfs,
            silence_minimum_seconds: cli.silence_duration_seconds,
            clipping_minimum_samples: cli.clipping_minimum_samples,
            tone_frequency_hz: cli.tone_frequency_hz,
            tone_threshold_dbfs: cli.tone_threshold_dbfs,
            tone_minimum_seconds: cli.tone_duration_seconds,
            expected_duration_seconds: cli.expected_duration_seconds,
            duration_tolerance_seconds: cli.duration_tolerance_seconds,
            expected_channel_count: cli.expected_channel_count,
            dropout_threshold_dbfs: cli.dropout_threshold_dbfs,
            dropout_minimum_seconds: cli.dropout_minimum_seconds,
            dropout_maximum_seconds: cli.dropout_maximum_seconds,
            phase_correlation_threshold: cli.phase_correlation_threshold,
            phase_window_seconds: cli.phase_window_seconds,
            click_threshold: cli.click_threshold,
            minimum_average_level_dbfs: cli.minimum_average_level_dbfs,
            hum_threshold_dbfs: cli.hum_threshold_dbfs,
            hum_minimum_seconds: cli.hum_minimum_seconds,
            noise_threshold_dbfs: cli.noise_threshold_dbfs,
            noise_gate_dbfs: cli.noise_gate_dbfs,
            noise_minimum_seconds: cli.noise_minimum_seconds,
            noise_low_hz: cli.noise_low_hz,
            noise_high_hz: cli.noise_high_hz,
            crosstalk_coherence_threshold: cli.crosstalk_coherence_threshold,
            crosstalk_level_delta_db: cli.crosstalk_level_delta_db,
            crosstalk_minimum_seconds: cli.crosstalk_minimum_seconds,
            panning_imbalance_db: cli.panning_imbalance_db,
            panning_minimum_seconds: cli.panning_minimum_seconds,
            lfe_cutoff_hz: cli.lfe_cutoff_hz,
            lfe_out_of_band_ratio: cli.lfe_out_of_band_ratio,
            expect_mono: cli.expect_mono,
            mono_difference_threshold: cli.mono_difference_threshold,
            dc_offset_threshold_dbfs: cli.dc_offset_threshold_dbfs,
            interchannel_delay_samples: cli.interchannel_delay_samples,
            stuck_sample_seconds: cli.stuck_sample_seconds,
            discontinuity_threshold: cli.discontinuity_threshold,
        });
        if let Some(options) = &ebu_qc_options {
            options.validate()?;
        }
        let mut reports = Vec::with_capacity(cli.inputs.len());
        let mut timeline_reports = Vec::new();
        let mut dialogue_detection_output = None;
        let mut adm_profile_audit_output = None;
        let mut qc_failed = false;
        for input in &cli.inputs {
            let timed = analyze_range_cached(
                analysis_cache.as_ref(),
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
            let automatic_codec_qc = cli
                .codec_qc
                .then(|| {
                    codec_qc::probe_and_evaluate(
                        input,
                        cli.codec_prober
                            .as_deref()
                            .unwrap_or_else(|| Path::new("ffprobe")),
                        &an,
                        dialogue.as_ref(),
                        cli.codec_reference.as_deref(),
                        cli.codec_qc_tolerance,
                    )
                })
                .transpose()?;
            if codec_qc.as_ref().is_some_and(|result| {
                result.dialnorm_pass == Some(false) || result.encoded_loudness_pass == Some(false)
            }) {
                qc_failed = true;
            }
            if automatic_codec_qc.as_ref().is_some_and(|result| {
                result.dialnorm_pass == Some(false) || result.roundtrip_pass == Some(false)
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
            let adm_profile = cli
                .adm_profile
                .as_ref()
                .map(|_| {
                    adm::validate_production_profile(
                        input,
                        adm::ProductionProfileMode::parse(&cli.adm_profile_mode),
                    )
                })
                .transpose()?;
            if let Some(audit) = adm_profile.clone() {
                adm_profile_audit_output = Some(audit);
            }
            let ebu_qc = ebu_qc_options
                .as_ref()
                .map(|options| qc::analyze_file(input, &an, options))
                .transpose()?;
            if adm_qc.as_ref().is_some_and(|result| !result.passed) {
                qc_failed = true;
            }
            if adm_profile.as_ref().is_some_and(|result| !result.passed) {
                qc_failed = true;
            }
            if ebu_qc
                .as_ref()
                .is_some_and(|results| results.iter().any(|result| !result.passed))
            {
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
                if let Some(results) = &ebu_qc {
                    report.ebu_qc_results_json = Some(
                        serde_json::to_string(results).expect("EBU QC results are serializable"),
                    );
                    report.ebu_qc_passed = Some(results.iter().all(|result| result.passed));
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
                    report.codec_qc_tolerance_lu = Some(codec.metadata.tolerance_lu.unwrap_or(1.0));
                }
                if let Some(results) = &ebu_qc {
                    eprintln!("  EBU QC baseband:");
                    for result in results {
                        eprintln!(
                            "    {} v{} {}: {} event(s) [{}]",
                            result.ebu_qc_id,
                            result.version,
                            result.name,
                            result.events.len(),
                            if result.passed { "PASS" } else { "FAIL" },
                        );
                        for event in &result.events {
                            eprintln!(
                                "      ch {} {:.3}..{:.3} s{}",
                                event.channel,
                                event.start_seconds,
                                event.end_seconds,
                                event
                                    .measured
                                    .zip(event.unit.as_deref())
                                    .map(|(value, unit)| format!(" ({value:.3} {unit})"))
                                    .unwrap_or_default(),
                            );
                        }
                    }
                }
                if let Some(codec) = &automatic_codec_qc {
                    report.codec = Some(codec.probe.codec.clone());
                    report.codec_dialnorm_lkfs = codec.probe.dialnorm_lkfs;
                    report.codec_downmix_mode = codec.probe.downmix_mode.clone();
                    report.codec_loudness_basis = Some(codec.loudness_basis);
                    report.codec_dialnorm_deviation_lu = codec.dialnorm_deviation_lu;
                    report.codec_dialnorm_pass = codec.dialnorm_pass;
                    report.codec_probe_tool = Some(codec.probe.tool.clone());
                    report.codec_probe_schema = Some(codec_qc::PROBE_SCHEMA);
                    report.codec_profile = codec.probe.profile.clone();
                    report.codec_container = codec.probe.container.clone();
                    report.codec_sample_rate_hz = codec.probe.sample_rate_hz;
                    report.codec_channels = codec.probe.channels;
                    report.codec_channel_layout = codec.probe.channel_layout.clone();
                    report.codec_bitrate_bps = codec.probe.bitrate_bps;
                    report.codec_drc_profile = codec.probe.drc_profile.clone();
                    report.codec_reference_path = codec
                        .reference_path
                        .as_ref()
                        .map(|path| path.to_string_lossy().into_owned());
                    report.codec_loudness_drift_lu = codec.loudness_drift_lu;
                    report.codec_true_peak_drift_db = codec.true_peak_drift_db;
                    report.codec_duration_drift_seconds = codec.duration_drift_seconds;
                    report.codec_roundtrip_pass = codec.roundtrip_pass;
                    report.codec_qc_tolerance_lu = Some(cli.codec_qc_tolerance);
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
                if let Some(audit) = &adm_profile {
                    report.adm_model_standard = Some(audit.adm_standard);
                    report.adm_model_version = Some(audit.adm_version);
                    report.adm_production_profile_standard = Some(audit.standard);
                    report.adm_production_profile_version = Some(audit.profile_version);
                    report.adm_production_profile_level = Some(audit.profile_level);
                    report.adm_production_profile_mode = Some(audit.mode);
                    report.adm_production_profile_validator = Some(audit.validator);
                    report.adm_production_profile_rules_json = Some(
                        serde_json::to_string(&audit.rules)
                            .expect("ADM profile rules are serializable"),
                    );
                    report.adm_production_profile_passed = Some(audit.passed);
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
                    report.dialogue_detection_frames_json = Some(
                        serde_json::to_string(&detection.frames)
                            .expect("dialogue detection frames are serializable"),
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
                if let Some(codec) = &automatic_codec_qc {
                    eprintln!(
                        "  codec QC: {}{}{}{} [{}]\n    prober: {} ({})\n    dialnorm deviation: {:?} LU [{}]\n    reference drift: loudness {:?} LU, true peak {:?} dB, duration {:?} s [{}]",
                        codec.probe.codec,
                        codec
                            .probe
                            .profile
                            .as_deref()
                            .map(|value| format!(" profile={value}"))
                            .unwrap_or_default(),
                        codec
                            .probe
                            .container
                            .as_deref()
                            .map(|value| format!(" container={value}"))
                            .unwrap_or_default(),
                        codec
                            .probe
                            .bitrate_bps
                            .map(|value| format!(" bitrate={value}"))
                            .unwrap_or_default(),
                        if codec.dialnorm_pass != Some(false)
                            && codec.roundtrip_pass != Some(false)
                        {
                            "PASS"
                        } else {
                            "FAIL"
                        },
                        codec.probe.tool,
                        codec_qc::PROBE_SCHEMA,
                        codec.dialnorm_deviation_lu,
                        qc_status(codec.dialnorm_pass),
                        codec.loudness_drift_lu,
                        codec.true_peak_drift_db,
                        codec.duration_drift_seconds,
                        qc_status(codec.roundtrip_pass),
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
                if let Some(audit) = &adm_profile {
                    eprintln!(
                        "  ADM {} {} level {} {:?} [{}]\n    validator: {}",
                        audit.standard,
                        audit.profile_version,
                        audit.profile_level,
                        audit.mode,
                        if audit.passed { "PASS" } else { "FAIL" },
                        audit.validator,
                    );
                    for rule in &audit.rules {
                        eprintln!(
                            "    {} {}: {} [{}]",
                            rule.rule_id,
                            rule.path,
                            rule.observed,
                            if rule.passed { "PASS" } else { "FAIL" },
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
            record_catalogue_asset(
                catalogue.as_mut(),
                &mut catalogue_records,
                CatalogueAsset {
                    source: input,
                    expected_source_sha256: catalogue_source_hashes
                        .get(input)
                        .map_or("", String::as_str),
                    output: None,
                    measurement: &an,
                    operation: "analysis",
                    profile: &catalogue_profile(&cli, &plan),
                    provenance: catalogue_provenance(&cli, &plan, "analysis"),
                },
            )?;
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
                report::write_manifest_with_anomaly_audits(
                    stdout.lock(),
                    &reports,
                    &anomaly_audits,
                )?;
                println!();
            } else {
                let file = File::create(path)
                    .map_err(|error| format!("create {}: {error}", path.display()))?;
                report::write_manifest_with_anomaly_audits(file, &reports, &anomaly_audits)?;
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
        if let Some(path) = &cli.adm_profile_report {
            let audit = adm_profile_audit_output
                .as_ref()
                .expect("ADM profile validation always produces an audit");
            let file = File::create(path)
                .map_err(|error| format!("create {}: {error}", path.display()))?;
            serde_json::to_writer_pretty(file, audit)
                .map_err(|error| format!("write ADM profile report: {error}"))?;
        }
        write_catalogue_report(
            catalogue.as_ref(),
            catalogue_options.report.as_deref(),
            std::mem::take(&mut catalogue_records),
        )?;
        if qc_failed {
            return Err("one or more inputs failed the requested compliance/QC checks".into());
        }
        return Ok(());
    }

    if !cli.gain_only {
        if batch_options.job_state.is_some() && cli.inputs.len() < 2 {
            return Err("--job-state requires at least two expanded input files".into());
        }
        if stdin_requested
            && (batch_options.job_state.is_some() || batch_options.progress.is_some())
        {
            return Err("--job-state and --progress cannot be used with stdin".into());
        }
        validate_control_paths(&cli, batch_options, &outputs)?;
        if cli.album {
            validate_outputs(&cli.inputs, &outputs, cli.overwrite)?;
        }
    }
    let mut difference_assets = Vec::new();

    if cli.album {
        let cached_analyses = analysis_cache
            .as_ref()
            .map(|cache| {
                cli.inputs
                    .iter()
                    .map(|path| {
                        analyze_for_plan_cached(
                            Some(cache),
                            path,
                            channel_roles_override.as_deref(),
                            &plan,
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        if cli.dry_run {
            let analyses = if let Some(analyses) = cached_analyses {
                analyses
            } else {
                cli.inputs
                    .iter()
                    .map(|path| {
                        normalize::analyze_file_for_plan(
                            path,
                            channel_roles_override.as_deref(),
                            &plan,
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()?
            };
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
            let corrected = if let Some(analyses) = cached_analyses.as_deref() {
                normalize::normalize_album_preanalyzed_corrected_with_roles(
                    &cli.inputs,
                    &outputs,
                    &plan,
                    &formats,
                    cli.verify_tolerance,
                    cli.verify_retries as usize,
                    channel_roles_override.as_deref(),
                    analyses,
                )?
            } else {
                normalize::normalize_album_corrected_with_roles(
                    &cli.inputs,
                    &outputs,
                    &plan,
                    &formats,
                    cli.verify_tolerance,
                    cli.verify_retries as usize,
                    channel_roles_override.as_deref(),
                )?
            };
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
            for ((input, output), source) in cli.inputs.iter().zip(&outputs).zip(&corrected.sources)
            {
                record_catalogue_asset(
                    catalogue.as_mut(),
                    &mut catalogue_records,
                    CatalogueAsset {
                        source: input,
                        expected_source_sha256: catalogue_source_hashes
                            .get(input)
                            .map_or("", String::as_str),
                        output: Some(output),
                        measurement: source,
                        operation: "normalization",
                        profile: &catalogue_profile(&cli, &plan),
                        provenance: catalogue_provenance(&cli, &plan, "normalization"),
                    },
                )?;
            }
            if let Some(path) = &cli.difference_report {
                for index in 0..cli.inputs.len() {
                    difference_assets.push(normalization_diff::build_asset(
                        &difference_inputs[index],
                        &outputs[index],
                        formats[index],
                        &plan,
                        normalization_diff::AssetMeasurements {
                            source: &corrected.sources[index],
                            output: &corrected.verifications[index].output,
                            gain: corrected.gain,
                            render: &corrected.renders[index],
                        },
                    )?);
                }
                write_difference_report(path, difference_assets)?;
            }
            write_catalogue_report(
                catalogue.as_ref(),
                catalogue_options.report.as_deref(),
                std::mem::take(&mut catalogue_records),
            )?;
            return Ok(());
        }
        let results = if cli.difference_report.is_some() {
            if let Some(analyses) = cached_analyses.as_deref() {
                normalize::normalize_album_preanalyzed_audited_with_roles(
                    &cli.inputs,
                    &outputs,
                    &plan,
                    &formats,
                    channel_roles_override.as_deref(),
                    analyses,
                )?
            } else {
                normalize::normalize_album_audited_with_roles(
                    &cli.inputs,
                    &outputs,
                    &plan,
                    &formats,
                    channel_roles_override.as_deref(),
                )?
            }
            .into_iter()
            .map(|(analysis, gain, render)| (analysis, gain, Some(render)))
            .collect::<Vec<_>>()
        } else {
            if let Some(analyses) = cached_analyses.as_deref() {
                normalize::normalize_album_preanalyzed_with_roles(
                    &cli.inputs,
                    &outputs,
                    &plan,
                    &formats,
                    channel_roles_override.as_deref(),
                    analyses,
                )?
            } else {
                normalize::normalize_album_with_roles(
                    &cli.inputs,
                    &outputs,
                    &plan,
                    &formats,
                    channel_roles_override.as_deref(),
                )?
            }
            .into_iter()
            .map(|(analysis, gain)| (analysis, gain, None))
            .collect()
        };
        let analyses: Vec<_> = results.iter().map(|(a, _, _)| a.clone()).collect();
        let album_l = normalize::album_lufs(&analyses);
        let gain = results.first().map(|r| r.1).unwrap_or(1.0);
        for (i, (an, g, _)) in results.iter().enumerate() {
            print_analysis(&cli.inputs[i], an, Some(*g));
        }
        eprintln!(
            "album: {:.2} LUFS  shared gain {:+.2} dB",
            album_l,
            20.0 * (gain as f64).log10()
        );
        for (index, (source, _, _)) in results.iter().enumerate() {
            record_catalogue_asset(
                catalogue.as_mut(),
                &mut catalogue_records,
                CatalogueAsset {
                    source: &cli.inputs[index],
                    expected_source_sha256: catalogue_source_hashes
                        .get(&cli.inputs[index])
                        .map_or("", String::as_str),
                    output: Some(&outputs[index]),
                    measurement: source,
                    operation: "normalization",
                    profile: &catalogue_profile(&cli, &plan),
                    provenance: catalogue_provenance(&cli, &plan, "normalization"),
                },
            )?;
        }
        if let Some(path) = &cli.difference_report {
            for (index, (source, asset_gain, render)) in results.iter().enumerate() {
                let output_analysis = normalize::analyze_file_with_roles(
                    &outputs[index],
                    channel_roles_override.as_deref(),
                )?;
                difference_assets.push(normalization_diff::build_asset(
                    &difference_inputs[index],
                    &outputs[index],
                    formats[index],
                    &plan,
                    normalization_diff::AssetMeasurements {
                        source,
                        output: &output_analysis,
                        gain: *asset_gain,
                        render: render
                            .as_ref()
                            .expect("difference reports capture render statistics"),
                    },
                )?);
            }
            write_difference_report(path, difference_assets)?;
        }
        write_catalogue_report(
            catalogue.as_ref(),
            catalogue_options.report.as_deref(),
            std::mem::take(&mut catalogue_records),
        )?;
        return Ok(());
    }

    let operation = batch_operation_descriptor(&cli, &plan, &formats);
    let batch_assets = cli
        .inputs
        .iter()
        .zip(&outputs)
        .map(|(input, output)| BatchAssetSpec::new(input, output))
        .collect::<Vec<_>>();
    let mut batch_job = batch_options
        .job_state
        .as_ref()
        .map(|path| BatchJob::open(path, &batch_assets, &operation, cli.overwrite))
        .transpose()?;
    let pending_inputs = cli
        .inputs
        .iter()
        .zip(&outputs)
        .enumerate()
        .filter(|(index, _)| {
            !batch_job
                .as_ref()
                .is_some_and(|job| job.is_completed(*index))
        })
        .map(|(_, pair)| pair)
        .collect::<Vec<_>>();
    validate_outputs(
        &pending_inputs
            .iter()
            .map(|(input, _)| (*input).clone())
            .collect::<Vec<_>>(),
        &pending_inputs
            .iter()
            .map(|(_, output)| (*output).clone())
            .collect::<Vec<_>>(),
        cli.overwrite,
    )?;
    let mut progress = batch_options
        .progress
        .as_deref()
        .map(ProgressWriter::open)
        .transpose()?;
    let initial_completed = batch_job.as_ref().map_or(0, BatchJob::completed_count);
    if let Some(writer) = &mut progress {
        writer.emit(
            "job_started",
            initial_completed,
            cli.inputs.len(),
            None,
            None,
        )?;
    }

    for (index, ((input, output), fmt)) in cli
        .inputs
        .iter()
        .zip(outputs.iter())
        .zip(formats.iter())
        .enumerate()
    {
        if batch_job
            .as_ref()
            .is_some_and(|job| job.is_completed(index))
        {
            if let Some(writer) = &mut progress {
                writer.emit(
                    "asset_skipped",
                    batch_job
                        .as_ref()
                        .expect("checked batch job")
                        .completed_count(),
                    cli.inputs.len(),
                    Some((index, input, output)),
                    None,
                )?;
            }
            continue;
        }
        if let Some(writer) = &mut progress {
            writer.emit(
                "asset_started",
                batch_job.as_ref().map_or(index, BatchJob::completed_count),
                cli.inputs.len(),
                Some((index, input, output)),
                None,
            )?;
        }
        let mut catalogue_measurement = None;
        let result = (|| -> Result<(), String> {
            let cached_analysis = analysis_cache
                .as_ref()
                .map(|cache| {
                    analyze_for_plan_cached(
                        Some(cache),
                        input,
                        channel_roles_override.as_deref(),
                        &plan,
                    )
                })
                .transpose()?;
            if cli.gain_only || cli.dry_run {
                let an = if let Some(analysis) = cached_analysis {
                    analysis
                } else {
                    normalize::analyze_file_for_plan(
                        input,
                        channel_roles_override.as_deref(),
                        &plan,
                    )?
                };
                let gain = normalize::compute_gain(&an, &plan);
                print_analysis(input, &an, Some(gain));
                catalogue_measurement = Some(an);
                if cli.dry_run {
                    eprintln!("  would write {}", output.display());
                }
            } else {
                prepare_output_directories(std::slice::from_ref(output))?;
                if cli.verify {
                    let corrected = if let Some(analysis) = cached_analysis.as_ref() {
                        normalize::normalize_one_preanalyzed_corrected_with_roles(
                            input,
                            output,
                            &plan,
                            *fmt,
                            cli.verify_tolerance,
                            cli.verify_retries as usize,
                            channel_roles_override.as_deref(),
                            analysis,
                        )?
                    } else {
                        normalize::normalize_one_corrected_with_roles(
                            input,
                            output,
                            &plan,
                            *fmt,
                            cli.verify_tolerance,
                            cli.verify_retries as usize,
                            channel_roles_override.as_deref(),
                        )?
                    };
                    print_analysis(input, &corrected.source, Some(corrected.gain));
                    catalogue_measurement = Some(corrected.source.clone());
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
                    if cli.difference_report.is_some() {
                        difference_assets.push(normalization_diff::build_asset(
                            &difference_inputs[index],
                            output,
                            *fmt,
                            &plan,
                            normalization_diff::AssetMeasurements {
                                source: &corrected.source,
                                output: &corrected.verification.output,
                                gain: corrected.gain,
                                render: &corrected.render,
                            },
                        )?);
                    }
                } else {
                    if cli.difference_report.is_some() {
                        let (an, gain, render) = if let Some(analysis) = cached_analysis.as_ref() {
                            normalize::normalize_one_preanalyzed_audited_with_roles(
                                input,
                                output,
                                &plan,
                                *fmt,
                                channel_roles_override.as_deref(),
                                analysis,
                            )?
                        } else {
                            normalize::normalize_one_audited_with_roles(
                                input,
                                output,
                                &plan,
                                *fmt,
                                channel_roles_override.as_deref(),
                            )?
                        };
                        print_analysis(input, &an, Some(gain));
                        catalogue_measurement = Some(an.clone());
                        let output_analysis = normalize::analyze_file_with_roles(
                            output,
                            channel_roles_override.as_deref(),
                        )?;
                        difference_assets.push(normalization_diff::build_asset(
                            &difference_inputs[index],
                            output,
                            *fmt,
                            &plan,
                            normalization_diff::AssetMeasurements {
                                source: &an,
                                output: &output_analysis,
                                gain,
                                render: &render,
                            },
                        )?);
                    } else {
                        let (an, gain) = if let Some(analysis) = cached_analysis.as_ref() {
                            normalize::normalize_one_preanalyzed_with_roles(
                                input,
                                output,
                                &plan,
                                *fmt,
                                channel_roles_override.as_deref(),
                                analysis,
                            )?
                        } else {
                            normalize::normalize_one_with_roles(
                                input,
                                output,
                                &plan,
                                *fmt,
                                channel_roles_override.as_deref(),
                            )?
                        };
                        print_analysis(input, &an, Some(gain));
                        catalogue_measurement = Some(an);
                    }
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            if let Some(writer) = &mut progress {
                writer.emit(
                    "asset_failed",
                    batch_job.as_ref().map_or(index, BatchJob::completed_count),
                    cli.inputs.len(),
                    Some((index, input, output)),
                    Some(&error),
                )?;
            }
            return Err(error);
        }
        if let Some(measurement) = catalogue_measurement.as_ref() {
            record_catalogue_asset(
                catalogue.as_mut(),
                &mut catalogue_records,
                CatalogueAsset {
                    source: input,
                    expected_source_sha256: catalogue_source_hashes
                        .get(input)
                        .map_or("", String::as_str),
                    output: Some(output),
                    measurement,
                    operation: "normalization",
                    profile: &catalogue_profile(&cli, &plan),
                    provenance: catalogue_provenance(&cli, &plan, "normalization"),
                },
            )?;
        }
        if let Some(job) = &mut batch_job {
            job.mark_completed(index)?;
        }
        if let Some(writer) = &mut progress {
            writer.emit(
                "asset_completed",
                batch_job
                    .as_ref()
                    .map_or(index + 1, BatchJob::completed_count),
                cli.inputs.len(),
                Some((index, input, output)),
                None,
            )?;
        }
    }
    if let Some(writer) = &mut progress {
        writer.emit(
            "job_completed",
            batch_job
                .as_ref()
                .map_or(cli.inputs.len(), BatchJob::completed_count),
            cli.inputs.len(),
            None,
            None,
        )?;
    }
    if let Some(path) = &cli.difference_report {
        write_difference_report(path, difference_assets)?;
    }
    write_catalogue_report(
        catalogue.as_ref(),
        catalogue_options.report.as_deref(),
        catalogue_records,
    )?;
    Ok(())
}

fn write_difference_report(
    path: &Path,
    assets: Vec<NormalizationDifferenceAsset>,
) -> Result<(), String> {
    normalization_diff::write_report(path, &NormalizationDifferenceReport::new(assets))
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

    fn prepare(cli: &mut cli::Cli, batch_options: &BatchOptions) -> Result<Self, String> {
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
            if batch_options.progress.as_deref() == Some(Path::new("-")) {
                return Err("binary output and --progress cannot both use stdout".into());
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

struct ProgressWriter {
    output: Box<dyn Write>,
    sequence: u64,
}

impl ProgressWriter {
    fn open(path: &Path) -> Result<Self, String> {
        let output: Box<dyn Write> = if path == Path::new("-") {
            Box::new(io::stdout())
        } else {
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("create {}: {error}", parent.display()))?;
            }
            Box::new(
                File::create(path)
                    .map_err(|error| format!("create {}: {error}", path.display()))?,
            )
        };
        Ok(Self {
            output,
            sequence: 0,
        })
    }

    fn emit(
        &mut self,
        event: &'static str,
        completed: usize,
        total: usize,
        asset: Option<(usize, &Path, &Path)>,
        error: Option<&str>,
    ) -> Result<(), String> {
        let mut record = BatchProgressEvent::new(self.sequence, event, completed, total);
        if let Some((index, input, output)) = asset {
            record.index = Some(index);
            record.input = Some(input.to_string_lossy().into_owned());
            record.output = Some(output.to_string_lossy().into_owned());
        }
        record.error = error.map(str::to_owned);
        serde_json::to_writer(&mut self.output, &record)
            .map_err(|error| format!("write progress event: {error}"))?;
        self.output
            .write_all(b"\n")
            .and_then(|_| self.output.flush())
            .map_err(|error| format!("flush progress event: {error}"))?;
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| "progress event sequence overflow".to_string())?;
        Ok(())
    }
}

fn batch_operation_descriptor(
    cli: &cli::Cli,
    plan: &Plan,
    formats: &[OutputFormat],
) -> serde_json::Value {
    serde_json::json!({
        "schema": "forge-normalization-operation-v1",
        "mode": match plan.mode {
            Mode::Lufs => "lufs",
            Mode::Peak => "peak",
            Mode::Rms => "rms",
        },
        "target_lufs": plan.target_lufs,
        "target_peak_dbfs": plan.target_peak_db,
        "target_rms_dbfs": plan.target_rms_db,
        "ceiling_dbtp": plan.ceiling_db,
        "max_gain_db": plan.max_gain_db,
        "dither": plan.dither,
        "output_bits": cli.bits,
        "bitrate_kbps": plan.mp3_bitrate,
        "encoder_quality": plan.mp3_quality,
        "limiter": plan.limiter.as_ref().map(|limiter| serde_json::json!({
            "lookahead_ms": limiter.lookahead_ms,
            "release_ms": limiter.release_ms,
        })),
        "wav_container": cli.wav_container,
        "bwf": plan.bwf,
        "output_sample_rate_hz": plan.output_sample_rate,
        "resample_quality": cli.resample_quality,
        "verify": cli.verify,
        "verify_tolerance": cli.verify_tolerance,
        "verify_retries": cli.verify_retries,
        "channel_layout": cli.channel_layout,
        "dual_mono": cli.dual_mono,
        "formats": formats.iter().map(|format| fmt_ext(*format)).collect::<Vec<_>>(),
    })
}

fn validate_control_paths(
    cli: &cli::Cli,
    batch_options: &BatchOptions,
    outputs: &[PathBuf],
) -> Result<(), String> {
    let mut controls = Vec::new();
    if let Some(path) = &batch_options.job_state {
        controls.push(("--job-state", comparison_path(path)?));
    }
    if let Some(path) = &batch_options.progress {
        if path != Path::new("-") {
            controls.push(("--progress", comparison_path(path)?));
        }
    }
    for (label, control) in &controls {
        for path in cli.inputs.iter().chain(outputs) {
            if comparison_path(path)? != *control {
                continue;
            }
            return Err(format!(
                "{label} must not overwrite an audio input or output: {}",
                control.display()
            ));
        }
    }
    if controls.len() == 2 && controls[0].1 == controls[1].1 {
        return Err("--job-state and --progress require different paths".into());
    }
    Ok(())
}

fn validate_catalogue_paths(
    cli: &cli::Cli,
    options: &CatalogueOptions,
    outputs: &[PathBuf],
    stdin_requested: bool,
) -> Result<(), String> {
    let Some(database) = &options.database else {
        return Ok(());
    };
    if stdin_requested || cli.output.as_deref() == Some(Path::new("-")) {
        return Err("--catalogue does not support stdin or binary stdout".into());
    }
    let database = comparison_path(database)?;
    for path in cli.inputs.iter().chain(outputs) {
        if comparison_path(path)? == database {
            return Err(format!(
                "--catalogue must not overwrite an audio input or output: {}",
                database.display()
            ));
        }
    }
    if let Some(report) = &options.report {
        if report == Path::new("-") {
            return Err("--catalogue-report requires a file path".into());
        }
        if report.exists() && !cli.overwrite {
            return Err(format!(
                "{} already exists (use --overwrite to replace it)",
                report.display()
            ));
        }
        let report = comparison_path(report)?;
        if report == database {
            return Err("--catalogue and --catalogue-report require different paths".into());
        }
        for path in cli.inputs.iter().chain(outputs) {
            if comparison_path(path)? == report {
                return Err(format!(
                    "--catalogue-report must not overwrite an audio input or output: {}",
                    report.display()
                ));
            }
        }
    }
    Ok(())
}

fn record_catalogue_asset(
    catalogue: Option<&mut Catalogue>,
    records: &mut Vec<CatalogueRecord>,
    asset: CatalogueAsset<'_>,
) -> Result<(), String> {
    let Some(catalogue) = catalogue else {
        return Ok(());
    };
    records.push(catalogue.record(asset)?);
    Ok(())
}

fn write_catalogue_report(
    catalogue: Option<&Catalogue>,
    report: Option<&Path>,
    records: Vec<CatalogueRecord>,
) -> Result<(), String> {
    match (catalogue, report) {
        (Some(catalogue), Some(report)) => catalogue.write_report(report, records),
        _ => Ok(()),
    }
}

fn catalogue_profile(cli: &cli::Cli, plan: &Plan) -> String {
    if let Some(preset) = &cli.preset {
        return format!("preset:{preset}");
    }
    if let Some(compliance) = &cli.compliance {
        return format!("compliance:{compliance}");
    }
    match plan.mode {
        Mode::Lufs => format!(
            "custom:lufs:{:.3}LUFS:{:.3}dBTP",
            plan.target_lufs, plan.ceiling_db
        ),
        Mode::Peak => format!("custom:peak:{:.3}dBFS", plan.target_peak_db),
        Mode::Rms => format!("custom:rms:{:.3}dBFS", plan.target_rms_db),
    }
}

fn catalogue_provenance(cli: &cli::Cli, plan: &Plan, operation: &str) -> serde_json::Value {
    serde_json::json!({
        "schema": "forge-catalogue-provenance-v1",
        "generator": format!("forge-normalizer/{}", env!("CARGO_PKG_VERSION")),
        "operation": operation,
        "preset": cli.preset,
        "compliance": cli.compliance,
        "mode": cli.mode,
        "target_lufs": plan.target_lufs,
        "target_peak_dbfs": plan.target_peak_db,
        "target_rms_dbfs": plan.target_rms_db,
        "ceiling_dbtp": plan.ceiling_db,
        "max_gain_db": plan.max_gain_db,
        "album": cli.album,
        "verify": cli.verify,
        "verify_tolerance": cli.verify_tolerance,
        "verify_retries": cli.verify_retries,
        "channel_layout": cli.channel_layout,
        "dual_mono": cli.dual_mono,
        "source_start_seconds": cli.start_seconds.unwrap_or(0.0),
        "source_duration_seconds": cli.duration_seconds,
    })
}

fn comparison_path(path: &Path) -> Result<PathBuf, String> {
    if path.exists() {
        std::fs::canonicalize(path)
            .map_err(|error| format!("canonicalize {}: {error}", path.display()))
    } else {
        std::path::absolute(path).map_err(|error| format!("resolve {}: {error}", path.display()))
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
            (Some(minimum), Some(maximum)) => format!(
                "{}{minimum:.2}, {maximum:.2}{}",
                if rule.minimum_inclusive == Some(false) {
                    "("
                } else {
                    "["
                },
                if rule.maximum_inclusive == Some(false) {
                    ")"
                } else {
                    "]"
                }
            ),
            (Some(minimum), None) => format!(
                "{} {minimum:.2}",
                if rule.minimum_inclusive == Some(false) {
                    ">"
                } else {
                    ">="
                }
            ),
            (None, Some(maximum)) => format!(
                "{} {maximum:.2}",
                if rule.maximum_inclusive == Some(false) {
                    "<"
                } else {
                    "<="
                }
            ),
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

fn analyze_range_cached(
    cache: Option<&AnalysisCache>,
    input: &Path,
    channel_roles: Option<&[ChannelRole]>,
    start_seconds: f64,
    duration_seconds: Option<f64>,
    timeline_interval_ms: Option<f64>,
) -> Result<normalize::TimedAnalysis, String> {
    if let Some(cache) = cache {
        return cache
            .analyze_range(
                input,
                channel_roles,
                start_seconds,
                duration_seconds,
                timeline_interval_ms,
            )
            .map(|cached| observe_cache(input, cached));
    }
    normalize::analyze_file_range_with_roles(
        input,
        channel_roles,
        start_seconds,
        duration_seconds,
        timeline_interval_ms,
    )
}

fn analyze_for_plan_cached(
    cache: Option<&AnalysisCache>,
    input: &Path,
    channel_roles: Option<&[ChannelRole]>,
    plan: &Plan,
) -> Result<Analysis, String> {
    if let Some(cache) = cache {
        return cache
            .analyze_for_plan(input, channel_roles, plan)
            .map(|cached| observe_cache(input, cached));
    }
    normalize::analyze_file_for_plan(input, channel_roles, plan)
}

fn analyze_file_cached(
    cache: Option<&AnalysisCache>,
    input: &Path,
    channel_roles: Option<&[ChannelRole]>,
) -> Result<Analysis, String> {
    if let Some(cache) = cache {
        return cache
            .analyze_file(input, channel_roles)
            .map(|cached| observe_cache(input, cached));
    }
    normalize::analyze_file_with_roles(input, channel_roles)
}

fn observe_cache<T>(input: &Path, cached: Cached<T>) -> T {
    let action = match cached.disposition {
        CacheDisposition::Hit => "hit",
        CacheDisposition::Stored => "miss; stored",
        CacheDisposition::Repaired => "invalid; repaired",
        CacheDisposition::ReadOnlyMiss => "miss; read-only",
        CacheDisposition::ReadOnlyInvalid => "invalid; read-only",
        CacheDisposition::TooLarge => "miss; entry too large to store",
    };
    eprintln!("analysis cache {action}: {}", input.display());
    if let Some(warning) = cached.warning {
        eprintln!("analysis cache warning: {warning}");
    }
    cached.value
}

fn write_loudness_tags(
    cli: &cli::Cli,
    channel_roles: Option<&[forge_normalizer::wav::ChannelRole]>,
    cache: Option<&AnalysisCache>,
) -> Result<(), String> {
    let analyses: Vec<_> = cli
        .inputs
        .iter()
        .map(|path| analyze_file_cached(cache, path, channel_roles))
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
        let scheme = forge_normalizer::metadata::loudness_metadata_scheme(input)?;
        if cli.dry_run {
            eprintln!("  would write {} metadata", scheme.label());
        } else {
            let written = forge_normalizer::metadata::write_loudness_metadata(
                input,
                analysis.lufs,
                analysis.true_peak,
                album,
            )?;
            eprintln!("  wrote and verified {} metadata", written.label());
        }
        if cli.sound_check {
            let sound_check = forge_normalizer::metadata::SoundCheck::from_r128(
                analysis.lufs,
                analysis.sample_peak,
            )?;
            if cli.dry_run {
                eprintln!(
                    "  would write Apple Sound Check compatibility metadata \
                     (non-normative iTunNORM mapping)"
                );
            } else {
                let round_trip =
                    forge_normalizer::metadata::write_sound_check(input, &sound_check)?;
                eprintln!(
                    "  wrote and verified Sound Check metadata: engineering gain {:+.2} dB, \
                     sample peak {:.8}",
                    round_trip.engineering_gain_db(),
                    round_trip.engineering_sample_peak()
                );
            }
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
        Some(
            "wav"
                | "wave"
                | "bwf"
                | "bw64"
                | "rf64"
                | "dsf"
                | "dff"
                | "mp3"
                | "flac"
                | "aac"
                | "m4a"
                | "mp4"
                | "ogg"
                | "opus",
        )
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
        "alac" => OutputFormat::Alac,
        "vorbis" => OutputFormat::Vorbis,
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
        OutputFormat::Alac => "m4a",
        OutputFormat::Vorbis => "ogg",
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
        Some("oga") | Some("ogg") => Some(OutputFormat::Vorbis),
        Some("wav") | Some("wave") | Some("bwf") | Some("bw64") | Some("rf64") => {
            Some(OutputFormat::Wav)
        }
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
        Some("ogg") | Some("oga") => {
            #[cfg(feature = "ffmpeg-encoding")]
            {
                OutputFormat::Vorbis
            }
            #[cfg(not(feature = "ffmpeg-encoding"))]
            {
                OutputFormat::Wav
            }
        }
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
