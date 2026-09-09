//! Forge: a SIMD-accelerated EBU R128 / ITU-R BS.1770-5 loudness normalizer.

use clap::{Arg, ArgAction, Command};
use forge_normalizer::adm::{self, ReferenceRendererOptions};
use forge_normalizer::analysis::{Analysis, AnalysisEngine};
use forge_normalizer::analysis_cache::{
    AnalysisCache, AnalysisCachePolicy, CacheDisposition, Cached,
};
use forge_normalizer::batch::{
    BatchAssetSpec, BatchFailure, BatchFailurePolicy, BatchFailureReport, BatchJob,
    BatchProgressEvent,
};
use forge_normalizer::bound_analysis::BoundAnalysis;
use forge_normalizer::catalogue::{Catalogue, CatalogueAsset, CatalogueRecordV2};
use forge_normalizer::cli;
use forge_normalizer::codec_qc;
use forge_normalizer::decoder::{
    AudioCodec, AudioTrackSelection, InputDescriptor, InputDescriptorOptions,
};
use forge_normalizer::discovery::discover_audio_files;
use forge_normalizer::dsp::limiter::LimiterConfig;
use forge_normalizer::dsp::resample::ResampleQuality;
use forge_normalizer::ebu_qc_report;
use forge_normalizer::ebu_qc_scenario1;
use forge_normalizer::generation::{
    GenerationPhase, GenerationRecovery, GenerationTransaction, PreparedGenerationOutput,
};
use forge_normalizer::metadata_fidelity::{
    MetadataFidelityReport, MetadataPolicy, MetadataPolicyConfig,
};
use forge_normalizer::metadata_transaction::{
    MetadataResume, MetadataTransaction, MetadataTransactionRequest,
};
use forge_normalizer::normalization_diff::{
    self, NormalizationDifferenceAsset, NormalizationDifferenceReport,
};
use forge_normalizer::normalize::{
    self, DialogueSource, DialogueStandard, Mode, OutputFormat, Plan,
};
use forge_normalizer::output::{
    create_live_file_atomically, stage_file_atomically, write_file_atomically, StagedFileOutput,
};
use forge_normalizer::output_plan::{OutputPlan, PlannedOutput, ProtectedPath};
use forge_normalizer::preset::Preset;
use forge_normalizer::qc::{self, QcOptions};
use forge_normalizer::report::{
    self, AnalysisReport, CodecMetadata, ComplianceProfile, TimelineReport,
};
use forge_normalizer::runtime_fingerprint::{
    normalization_semantic_context, NORMALIZATION_FINGERPRINT_REVISION,
};
use forge_normalizer::stable_input::{StableInput, StableInputOptions};
use forge_normalizer::watch::{WatchCandidate, WatchFolder, WatchProcessingOutput};
use forge_normalizer::wav::{named_channel_layout, ChannelRole, PcmKind, WavContainer};
use rayon::{prelude::*, ThreadPoolBuilder};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;
use tempfile::{Builder, NamedTempFile, TempDir};

const MAX_BATCH_WAVE_ASSETS: usize = 32;
const MAX_BATCH_PROGRESS_PATH_BYTES: usize = 4096;
const MAX_BATCH_FAILURE_ERROR_BYTES: usize = 16 * 1024;

#[derive(Debug, Default)]
struct BatchOptions {
    job_state: Option<PathBuf>,
    progress: Option<PathBuf>,
    keep_going: bool,
    failure_report: Option<PathBuf>,
}

#[derive(Debug, Default, Clone)]
struct CacheOptions {
    directory: Option<PathBuf>,
    read_only: bool,
    warm_cache: bool,
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

#[derive(Debug)]
struct AnalysisInvocationOptions {
    engine: AnalysisEngine,
    audio_track: Option<u32>,
    anomaly_audits: Vec<PathBuf>,
    ebu_qc_xml: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct MetadataInvocationOptions {
    policy: MetadataPolicyConfig,
    explicitly_requested: bool,
    report: Option<PathBuf>,
    job_state: Option<PathBuf>,
}

impl MetadataInvocationOptions {
    fn legacy() -> Self {
        Self {
            policy: MetadataPolicyConfig::legacy_generic(),
            explicitly_requested: false,
            report: None,
            job_state: None,
        }
    }

    fn active_for_normalization(&self) -> bool {
        self.explicitly_requested && self.policy.policy() != MetadataPolicy::LegacyGeneric
    }
}

impl AnalysisInvocationOptions {
    fn engine_only(engine: AnalysisEngine) -> Self {
        Self {
            engine,
            audio_track: None,
            anomaly_audits: Vec::new(),
            ebu_qc_xml: None,
        }
    }
}

impl CacheOptions {
    fn open(&self, force_read_only: bool) -> Result<Option<AnalysisCache>, String> {
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
                read_only: self.read_only || force_read_only,
                max_bytes,
            },
        )
        .map(Some)
    }
}

fn main() -> ExitCode {
    let matches = cli::Cli::command_with_analysis_engine()
        .arg(
            Arg::new("audio_track")
                .long("audio-track")
                .value_name("INDEX")
                .value_parser(clap::value_parser!(u32))
                .help("Select a zero-based audio-track index after content probing"),
        )
        .arg(
            Arg::new("true_peak_backend")
                .long("true-peak-backend")
                .value_name("BACKEND")
                .value_parser(["cpu", "cuda"])
                .default_value("cpu")
                .help(
                    "True-peak analysis backend: cpu, or optional CUDA with automatic CPU fallback",
                ),
        )
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
                    "dry_run",
                    "gain_only",
                    "write_tags",
                ]),
        )
        .arg(
            Arg::new("keep_going")
                .long("keep-going")
                .action(ArgAction::SetTrue)
                .requires("job_state")
                .conflicts_with_all([
                    "analyze_only",
                    "album",
                    "dry_run",
                    "gain_only",
                    "write_tags",
                    "watch",
                ])
                .help(
                    "Finish the bounded independent batch after asset failures; publish no generation until every asset succeeds",
                ),
        )
        .arg(
            Arg::new("failure_report")
                .long("failure-report")
                .value_name("PATH")
                .value_parser(clap::value_parser!(PathBuf))
                .requires("keep_going")
                .help("Write a bounded, versioned JSON summary of --keep-going failures"),
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
            Arg::new("warm_cache")
                .long("warm-cache")
                .action(ArgAction::SetTrue)
                .requires("analysis_cache")
                .requires("dry_run")
                .conflicts_with("analysis_cache_read_only")
                .help("Allow --dry-run to populate and evict entries in the analysis cache"),
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
        .arg(
            Arg::new("ebu_qc_xml")
                .long("ebu-qc-xml")
                .value_name("PATH")
                .value_parser(clap::value_parser!(PathBuf))
                .requires("ebu_qc")
                .conflicts_with("watch")
                .help("Write an EBU QC 2026-04 Scenario 1 XML report for one input"),
        )
        .arg(
            Arg::new("metadata_policy")
                .long("metadata-policy")
                .value_name("POLICY")
                .value_parser(["preserve", "strict", "strip", "legacy-generic"])
                .conflicts_with_all(["analyze_only", "gain_only", "dry_run"])
                .help(
                    "Metadata fidelity policy: preserve, strict, strip, or explicit legacy-generic compatibility",
                ),
        )
        .arg(
            Arg::new("metadata_strip_locator")
                .long("metadata-strip-locator")
                .value_name("LOCATOR")
                .action(ArgAction::Append)
                .requires("metadata_policy")
                .conflicts_with_all(["analyze_only", "gain_only"])
                .help("With --metadata-policy strip, remove only the exact repeated locator"),
        )
        .arg(
            Arg::new("metadata_report")
                .long("metadata-report")
                .value_name("PATH")
                .value_parser(clap::value_parser!(PathBuf))
                .requires("metadata_policy")
                .conflicts_with_all(["analyze_only", "gain_only", "dry_run"])
                .help("Write the versioned field-level metadata fidelity report as JSON"),
        )
        .arg(
            Arg::new("metadata_job_state")
                .long("metadata-job-state")
                .value_name("PATH")
                .value_parser(clap::value_parser!(PathBuf))
                .requires("write_tags")
                .conflicts_with_all(["dry_run", "job_state", "progress", "watch"])
                .help(
                    "Persist and resume one metadata-only transaction without mutating the live file before commit",
                ),
        )
        .subcommand_negates_reqs(true)
        .subcommand(
            Command::new("recovery")
                .about("Inspect or safely reclaim one generation journal")
                .subcommand_required(true)
                .subcommand(
                    Command::new("inspect")
                        .about("Inspect a generation journal without creating or changing files")
                        .arg(
                            Arg::new("state")
                                .value_name("STATE")
                                .value_parser(clap::value_parser!(PathBuf))
                                .required(true),
                        ),
                )
                .subcommand(
                    Command::new("reclaim")
                        .about("Recover/abort and reclaim only journal-proven private files")
                        .arg(
                            Arg::new("state")
                                .value_name("STATE")
                                .value_parser(clap::value_parser!(PathBuf))
                                .required(true),
                        )
                        .arg(
                            Arg::new("yes")
                                .long("yes")
                                .action(ArgAction::SetTrue)
                                .help("Confirm destructive reclamation; omission is a dry run"),
                        ),
                ),
        )
        .get_matches();
    if let Some(("recovery", command)) = matches.subcommand() {
        return match run_recovery_command(command) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("forge: error: {error}");
                ExitCode::from(1)
            }
        };
    }
    let true_peak_backend = matches
        .get_one::<String>("true_peak_backend")
        .map(String::as_str)
        .unwrap_or("cpu");
    let backend = if true_peak_backend == "cuda" {
        forge_normalizer::dsp::lufs::TruePeakBackend::Cuda
    } else {
        forge_normalizer::dsp::lufs::TruePeakBackend::Cpu
    };
    match forge_normalizer::dsp::lufs::configure_true_peak_backend(backend) {
        Ok(device) if backend == forge_normalizer::dsp::lufs::TruePeakBackend::Cuda => {
            eprintln!("true-peak backend: CUDA ({device})");
        }
        Ok(_) => {}
        Err(reason) => {
            eprintln!("true-peak backend: CUDA unavailable ({reason}); using CPU");
        }
    }
    let batch_options = BatchOptions {
        job_state: matches.get_one::<PathBuf>("job_state").cloned(),
        progress: matches.get_one::<PathBuf>("progress").cloned(),
        keep_going: matches.get_flag("keep_going"),
        failure_report: matches.get_one::<PathBuf>("failure_report").cloned(),
    };
    let cache_options = CacheOptions {
        directory: matches.get_one::<PathBuf>("analysis_cache").cloned(),
        read_only: matches.get_flag("analysis_cache_read_only"),
        warm_cache: matches.get_flag("warm_cache"),
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
    let metadata_options = match metadata_invocation_options(&matches) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("forge: error: {error}");
            return ExitCode::from(2);
        }
    };
    let (cli, analysis_engine) =
        match cli::Cli::from_matches_with_config_and_analysis_engine(&matches) {
            Ok(parsed) => parsed,
            Err(error) => {
                eprintln!("forge: error: {error}");
                return ExitCode::from(2);
            }
        };
    let analysis_engine = match analysis_engine.parse::<AnalysisEngine>() {
        Ok(engine) => engine,
        Err(error) => {
            eprintln!("forge: error: {error}");
            return ExitCode::from(2);
        }
    };
    let analysis_from_config = cli.analyze_only
        && matches.value_source("analyze_only") != Some(clap::parser::ValueSource::CommandLine);
    if let Err(error) = validate_effective_mode_conflicts(
        &cli,
        &batch_options,
        &watch_options,
        &metadata_options,
        analysis_from_config,
    ) {
        eprintln!("forge: error: {error}");
        return ExitCode::from(2);
    }
    let anomaly_audits = matches
        .get_many::<PathBuf>("anomaly_audit")
        .map(|values| values.cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let ebu_qc_xml = matches.get_one::<PathBuf>("ebu_qc_xml").cloned();
    let audio_track = matches.get_one::<u32>("audio_track").copied();
    let result = run(
        cli,
        batch_options,
        cache_options,
        watch_options,
        catalogue_options,
        metadata_options,
        AnalysisInvocationOptions {
            engine: analysis_engine,
            audio_track,
            anomaly_audits,
            ebu_qc_xml,
        },
    );
    if backend == forge_normalizer::dsp::lufs::TruePeakBackend::Cuda {
        if let Some(reason) = forge_normalizer::dsp::lufs::cuda_runtime_fallback_reason() {
            eprintln!("true-peak backend: CUDA runtime failed ({reason}); continued on CPU");
        }
    }
    if let Err(e) = result {
        eprintln!("forge: error: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

fn run_recovery_command(matches: &clap::ArgMatches) -> Result<(), String> {
    let (operation, arguments) = matches
        .subcommand()
        .ok_or_else(|| "recovery requires inspect or reclaim".to_string())?;
    let state = arguments
        .get_one::<PathBuf>("state")
        .ok_or_else(|| "recovery state path is required".to_string())?;
    let (status, action, confirmed, lock_state, private_file_count) = match operation {
        "inspect" => (
            GenerationTransaction::inspect(state)?,
            "inspected",
            false,
            "not_probed",
            0,
        ),
        "reclaim" if arguments.get_flag("yes") => {
            let inspection = GenerationTransaction::inspect_reclaim(state)?;
            if inspection.action()
                == forge_normalizer::generation::GenerationReclaimAction::BlockedActive
            {
                return Err("generation is active in another process; reclaim is blocked".into());
            }
            let private_file_count = inspection.private_file_count();
            let status = if inspection.action()
                == forge_normalizer::generation::GenerationReclaimAction::Nothing
            {
                inspection.status().clone()
            } else {
                GenerationTransaction::reclaim(state)?
            };
            (
                status,
                "reclaimed",
                true,
                inspection.lock_state().as_str(),
                private_file_count,
            )
        }
        "reclaim" => {
            let inspection = GenerationTransaction::inspect_reclaim(state)?;
            (
                inspection.status().clone(),
                inspection.action().as_str(),
                false,
                inspection.lock_state().as_str(),
                inspection.private_file_count(),
            )
        }
        _ => return Err(format!("unknown recovery operation: {operation}")),
    };
    let record = serde_json::json!({
        "schema": "https://penguin425.github.io/audio-normalizer/schema/generation-recovery-report-v1",
        "generator": status.generator(),
        "action": action,
        "confirmed": confirmed,
        "state": status.state_path().to_string_lossy(),
        "generation_id": status.generation_id(),
        "semantic_fingerprint": status.semantic_fingerprint(),
        "phase": status.phase().as_str(),
        "member_count": status.member_count(),
        "publication_steps": status.publication_steps(),
        "lock_state": lock_state,
        "private_file_count": private_file_count,
    });
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer_pretty(&mut output, &record)
        .map_err(|error| format!("write recovery report: {error}"))?;
    output
        .write_all(b"\n")
        .and_then(|_| output.flush())
        .map_err(|error| format!("flush recovery report: {error}"))
}

fn metadata_invocation_options(
    matches: &clap::ArgMatches,
) -> Result<MetadataInvocationOptions, String> {
    let Some(policy_name) = matches.get_one::<String>("metadata_policy") else {
        let mut options = MetadataInvocationOptions::legacy();
        options.job_state = matches.get_one::<PathBuf>("metadata_job_state").cloned();
        return Ok(options);
    };
    let strip_locators = matches
        .get_many::<String>("metadata_strip_locator")
        .map(|values| values.cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let policy = match policy_name.as_str() {
        "preserve" => {
            if !strip_locators.is_empty() {
                return Err("--metadata-strip-locator requires --metadata-policy strip".into());
            }
            MetadataPolicyConfig::preserve()
        }
        "strict" => {
            if !strip_locators.is_empty() {
                return Err("--metadata-strip-locator requires --metadata-policy strip".into());
            }
            MetadataPolicyConfig::strict()
        }
        "strip" if strip_locators.is_empty() => MetadataPolicyConfig::strip_all(),
        "strip" => MetadataPolicyConfig::strip_selected(strip_locators)
            .map_err(|error| error.to_string())?,
        "legacy-generic" => {
            if !strip_locators.is_empty() {
                return Err("--metadata-strip-locator requires --metadata-policy strip".into());
            }
            if matches.get_one::<PathBuf>("metadata_report").is_some() {
                return Err(
                    "--metadata-report requires preserve, strict, or strip policy semantics".into(),
                );
            }
            MetadataPolicyConfig::legacy_generic()
        }
        _ => unreachable!("clap validates metadata policy names"),
    };
    Ok(MetadataInvocationOptions {
        policy,
        explicitly_requested: true,
        report: matches.get_one::<PathBuf>("metadata_report").cloned(),
        job_state: matches.get_one::<PathBuf>("metadata_job_state").cloned(),
    })
}

/// Clap validates conflicts among command-line values before the optional TOML
/// configuration is applied.  A config file can turn `--analyze` on after that
/// validation, so repeat the mode-boundary checks against the effective CLI
/// before any paths are prepared or outputs are touched.
fn validate_effective_mode_conflicts(
    cli: &cli::Cli,
    batch_options: &BatchOptions,
    watch_options: &WatchOptions,
    metadata_options: &MetadataInvocationOptions,
    analysis_from_config: bool,
) -> Result<(), String> {
    let conflict = |left: &str, right: &str| -> Result<(), String> {
        Err(format!(
            "the argument '{left}' cannot be used with '{right}'"
        ))
    };
    if batch_options.job_state.is_some() {
        if cli.dry_run {
            return conflict("--job-state", "--dry-run");
        }
        if cli.gain_only {
            return conflict("--job-state", "--gain-only");
        }
        if cli.write_tags {
            return conflict("--job-state", "--write-tags");
        }
        if cli.difference_report.is_some() {
            return conflict("--job-state", "--difference-report");
        }
    }
    if batch_options.progress.is_some() && (cli.dry_run || cli.gain_only || cli.write_tags) {
        return Err("--progress cannot be used with dry-run, gain-only, or tags mode".into());
    }
    if batch_options.keep_going && cli.album {
        return conflict("--keep-going", "--album");
    }
    if watch_options.enabled && cli.difference_report.is_some() {
        return conflict("--watch", "--difference-report");
    }
    if cli.verify && (cli.dry_run || cli.gain_only || cli.write_tags) {
        return Err("--verify cannot be used with dry-run, gain-only, or tags mode".into());
    }

    let reject = |option: &str| -> Result<(), String> {
        Err(format!(
            "the argument '{option}' cannot be used with '--analyze'"
        ))
    };

    if !cli.analyze_only {
        return Ok(());
    }

    // Analysis may be supplied directly or enabled by config.  Config is
    // applied after Clap has parsed the raw arguments, so check the effective
    // mode here and fail before analysis prepares any report, output, or
    // renderer paths.
    if cli.dry_run {
        return reject("--dry-run");
    }

    if let Some(path) = &batch_options.job_state {
        return reject(&format!("--job-state {}", path.display()));
    }
    if let Some(path) = &batch_options.progress {
        return reject(&format!("--progress {}", path.display()));
    }
    if batch_options.keep_going {
        return reject("--keep-going");
    }
    if let Some(path) = &batch_options.failure_report {
        return reject(&format!("--failure-report {}", path.display()));
    }
    if watch_options.enabled {
        return reject("--watch");
    }
    if metadata_options.explicitly_requested {
        return reject("--metadata-policy");
    }
    if cli.preset.is_some() {
        return reject("--preset");
    }
    if cli.sample_rate_hz.is_some() {
        return reject("--sample-rate");
    }
    if cli.write_tags {
        return reject("--write-tags");
    }
    if cli.verify {
        return reject("--verify");
    }
    if cli.difference_report.is_some() {
        return reject("--difference-report");
    }

    // These mode flags historically did not declare a direct Clap conflict
    // with --analyze, but an analysis mode loaded implicitly from config would
    // otherwise silently swallow them because the analysis branch runs first.
    // Keep direct command-line behaviour unchanged while rejecting this new
    // config-induced ambiguity.
    if analysis_from_config {
        if cli.gain_only {
            return reject("--gain-only");
        }
        if cli.album {
            return reject("--album");
        }
    }
    Ok(())
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
    metadata_options: MetadataInvocationOptions,
    analysis_options: AnalysisInvocationOptions,
) -> Result<(), String> {
    if watch_options.enabled {
        if analysis_options.audio_track.is_some() {
            return Err("--audio-track cannot be used with --watch".into());
        }
        return run_watch(
            cli,
            analysis_options.engine,
            cache_options,
            watch_options,
            metadata_options,
            analysis_options.anomaly_audits,
        );
    }
    let pipeline = PipelineFiles::prepare(&mut cli, &batch_options)?;
    run_paths(
        cli,
        pipeline.stdin_requested(),
        &batch_options,
        &cache_options,
        &catalogue_options,
        &metadata_options,
        &analysis_options,
    )?;
    pipeline.emit_stdout()
}

fn run_watch(
    mut cli: cli::Cli,
    analysis_engine: AnalysisEngine,
    cache_options: CacheOptions,
    options: WatchOptions,
    metadata_options: MetadataInvocationOptions,
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
    if metadata_options.report.is_some() {
        return Err("--metadata-report cannot name one path for a watch folder".into());
    }
    let operation = watch_operation_descriptor(&cli, &metadata_options);
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
            if let Err(error) = process_watch_candidate(
                &cli,
                analysis_engine,
                &cache_options,
                &mut watch,
                &candidate,
                &output_root,
                &metadata_options,
            ) {
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
    analysis_engine: AnalysisEngine,
    cache_options: &CacheOptions,
    watch: &mut WatchFolder,
    candidate: &WatchCandidate,
    output_root: &Path,
    metadata_options: &MetadataInvocationOptions,
) -> Result<(), String> {
    let format = template
        .format
        .as_deref()
        .map(parse_format)
        .map_or_else(|| default_format_for_input(&candidate.input, None), Ok)?;
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
    let output = watch.mark_processing_output(&candidate.id, &output)?;
    let mut cli = template.clone();
    cli.inputs = vec![candidate.input.clone()];
    cli.output = Some(output.path().to_owned());
    cli.recursive = false;
    cli.overwrite = output.replace_existing();
    let analysis_options = AnalysisInvocationOptions::engine_only(analysis_engine);
    let result = run_paths_with_watch_output(
        cli,
        false,
        &BatchOptions::default(),
        cache_options,
        &CatalogueOptions::default(),
        metadata_options,
        &analysis_options,
        Some(output),
    );
    match result {
        Ok(()) => watch.mark_completed(&candidate.id),
        Err(error) => Err(error),
    }
}

fn watch_operation_descriptor(
    cli: &cli::Cli,
    metadata_options: &MetadataInvocationOptions,
) -> serde_json::Value {
    let descriptor = serde_json::json!({
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
    });
    metadata_operation_descriptor(descriptor, metadata_options)
}

fn run_paths(
    cli: cli::Cli,
    stdin_requested: bool,
    batch_options: &BatchOptions,
    cache_options: &CacheOptions,
    catalogue_options: &CatalogueOptions,
    metadata_options: &MetadataInvocationOptions,
    analysis_options: &AnalysisInvocationOptions,
) -> Result<(), String> {
    run_paths_with_watch_output(
        cli,
        stdin_requested,
        batch_options,
        cache_options,
        catalogue_options,
        metadata_options,
        analysis_options,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_paths_with_watch_output(
    mut cli: cli::Cli,
    stdin_requested: bool,
    batch_options: &BatchOptions,
    cache_options: &CacheOptions,
    catalogue_options: &CatalogueOptions,
    metadata_options: &MetadataInvocationOptions,
    analysis_options: &AnalysisInvocationOptions,
    watch_output: Option<WatchProcessingOutput>,
) -> Result<(), String> {
    let analysis_engine = analysis_options.engine;
    let audio_track = analysis_options.audio_track;
    let anomaly_audit_paths = &analysis_options.anomaly_audits;
    let ebu_qc_xml = analysis_options.ebu_qc_xml.as_deref();
    if let Some(j) = cli.jobs {
        ThreadPoolBuilder::new()
            .num_threads(j)
            .build_global()
            .map_err(|e| format!("thread pool: {e}"))?;
    }

    let (expanded, relative_paths) = expand_inputs(&cli.inputs, cli.recursive)?;
    cli.inputs = expanded;
    if metadata_options.report.is_some() && cli.inputs.len() != 1 {
        return Err("--metadata-report requires exactly one expanded input".into());
    }
    if metadata_options.job_state.is_some() && cli.inputs.len() != 1 {
        return Err("--metadata-job-state requires exactly one expanded input".into());
    }
    if (metadata_options.active_for_normalization() || metadata_options.job_state.is_some())
        && (stdin_requested || cli.inputs.iter().any(|input| input == Path::new("-")))
    {
        return Err(
            "metadata fidelity and transaction options require a regular file input".into(),
        );
    }
    if cli.album && metadata_options.active_for_normalization() {
        return Err(
            "explicit metadata fidelity policy for album publication is deferred to the generation transaction in v0.189.15"
                .into(),
        );
    }
    if cli.write_tags
        && metadata_options.active_for_normalization()
        && metadata_options.job_state.is_none()
    {
        return Err(
            "explicit metadata fidelity with --write-tags requires --metadata-job-state".into(),
        );
    }
    if cli.write_tags && metadata_options.policy.policy() == MetadataPolicy::Strip {
        return Err(
            "--metadata-policy strip is available for a new normalization output; metadata-only stripping is not losslessly implemented"
                .into(),
        );
    }
    if analysis_engine == AnalysisEngine::Reference && !cli.analyze_only {
        return Err("--analysis-engine reference requires --analyze".into());
    }
    if analysis_engine == AnalysisEngine::Reference
        && (cli.auto_dialogue || cli.dialogue_ranges.is_some() || cli.dialogue_stem.is_some())
    {
        return Err(
            "reference analysis cannot be combined with dialogue analysis in this release".into(),
        );
    }
    if audio_track.is_some()
        && (cli.album
            || cli.write_tags
            || cli.auto_dialogue
            || cli.dialogue_ranges.is_some()
            || cli.dialogue_stem.is_some()
            || cli.downmix_qc
            || cli.codec_metadata.is_some()
            || cli.codec_qc
            || cli.adm_presentations.is_some()
            || cli.adm_render
            || cli.adm_profile.is_some())
    {
        return Err(
            "--audio-track currently supports independent normalization, gain/dry-run, analysis, and EBU QC only"
                .into(),
        );
    }

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
    let output_conflict_policy = if cli.overwrite {
        normalize::OutputConflictPolicy::ReplaceUnchanged
    } else {
        normalize::OutputConflictPolicy::CreateNew
    };
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
    plan.validate()?;
    if cli.write_tags {
        validate_metadata_control_paths(&cli, metadata_options)?;
        let analysis_cache = cache_options.open(cli.dry_run && !cache_options.warm_cache)?;
        return write_loudness_tags(
            &cli,
            channel_roles_override.as_deref(),
            analysis_cache.as_ref(),
            metadata_options,
        );
    }

    let (outputs, formats) = resolve_outputs_and_formats(&cli, &relative_paths, audio_track)?;
    if let Some(watch_output) = watch_output.as_ref() {
        if outputs.len() != 1 || outputs[0] != watch_output.path() {
            return Err("watch output changed between checkpoint and normalization".into());
        }
        if cli.overwrite != watch_output.replace_existing() {
            return Err("watch output conflict policy changed after checkpoint".into());
        }
    }
    if !cli.analyze_only && !cli.gain_only {
        for format in &formats {
            if cli.dry_run {
                plan.validate_format_request(*format)?;
            } else {
                plan.validate_for_format(*format)?;
            }
        }
    }
    validate_control_paths(&cli, batch_options, &outputs)?;
    let _output_plan = build_output_plan(
        &cli,
        batch_options,
        catalogue_options,
        metadata_options,
        anomaly_audit_paths,
        ebu_qc_xml,
        &outputs,
    )?;
    let analysis_cache = cache_options.open(cli.dry_run && !cache_options.warm_cache)?;
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
        if ebu_qc_xml.is_some() && cli.inputs.len() != 1 {
            return Err("--ebu-qc-xml requires exactly one input".into());
        }
        if ebu_qc_xml.is_some_and(|path| path.as_os_str() == "-") {
            return Err("--ebu-qc-xml requires a file path, not stdout".into());
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
        let mut ebu_qc_xml_output = None;
        let mut qc_failed = false;
        for input in &cli.inputs {
            let analysis_timeline_interval_ms = if cli.timeline.is_some() {
                Some(cli.timeline_interval_ms)
            } else if ebu_qc_xml.is_some() {
                Some(40.0)
            } else {
                None
            };
            let (input_descriptor, timed) = analyze_range_cached(
                analysis_cache.as_ref(),
                input,
                channel_roles_override.as_deref(),
                start_seconds,
                cli.duration_seconds,
                analysis_timeline_interval_ms,
                analysis_engine,
                audio_track,
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
                .then(|| {
                    normalize::analyze_stereo_downmix_with_roles(
                        input,
                        channel_roles_override.as_deref(),
                    )
                })
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
                .map(|options| qc::analyze_descriptor(&input_descriptor, &an, options))
                .transpose()?;
            if ebu_qc_xml.is_some() {
                let results = ebu_qc
                    .as_ref()
                    .expect("--ebu-qc-xml requires EBU QC analysis");
                ebu_qc_xml_output = Some((
                    ebu_qc_report::EbuQcReportMetadata::from_file(input, &an)?,
                    an.clone(),
                    ebu_qc_options
                        .as_ref()
                        .expect("--ebu-qc-xml requires EBU QC options")
                        .clone(),
                    results.clone(),
                    timed.timeline.clone(),
                    analysis_timeline_interval_ms.expect("--ebu-qc-xml always captures a timeline"),
                ));
            }
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
                if analysis_engine == AnalysisEngine::Reference {
                    eprintln!("  analysis engine: {}", analysis_engine.id());
                }
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
                Some(&input_descriptor),
                catalogue_descriptor_options(&cli, channel_roles_override.as_deref(), audio_track),
                &plan,
                &catalogue_analysis_renderer(analysis_engine),
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
            report::write_json_with_engine(&mut output, &reports, analysis_engine)?;
            writeln!(output).map_err(|error| format!("write stdout: {error}"))?;
        } else if cli.ndjson {
            let stdout = io::stdout();
            report::write_ndjson_with_engine(stdout.lock(), &reports, analysis_engine)?;
        } else if let Some(path) = &cli.csv {
            if path.as_os_str() == "-" {
                let stdout = io::stdout();
                report::write_csv_with_engine(stdout.lock(), &reports, analysis_engine)?;
            } else {
                write_file_atomically(path, cli.overwrite, |file| {
                    report::write_csv_with_engine(file, &reports, analysis_engine)
                })?;
            }
        }
        if let Some(path) = &cli.timeline {
            write_timeline(path, &timeline_reports, analysis_engine, cli.overwrite)?;
        }
        if let Some(path) = &cli.manifest {
            if path.as_os_str() == "-" {
                let stdout = io::stdout();
                report::write_manifest_with_engine_and_anomaly_audits(
                    stdout.lock(),
                    &reports,
                    &anomaly_audits,
                    analysis_engine,
                )?;
                println!();
            } else {
                write_file_atomically(path, cli.overwrite, |file| {
                    report::write_manifest_with_engine_and_anomaly_audits(
                        file,
                        &reports,
                        &anomaly_audits,
                        analysis_engine,
                    )
                })?;
            }
        }
        if let Some(path) = &cli.dialogue_detection_report {
            let detection = dialogue_detection_output
                .as_ref()
                .expect("auto dialogue always produces a detection result");
            write_file_atomically(path, cli.overwrite, |file| {
                serde_json::to_writer_pretty(file, detection)
                    .map_err(|error| format!("write dialogue detection report: {error}"))
            })?;
        }
        if let Some(path) = &cli.adm_profile_report {
            let audit = adm_profile_audit_output
                .as_ref()
                .expect("ADM profile validation always produces an audit");
            write_file_atomically(path, cli.overwrite, |file| {
                serde_json::to_writer_pretty(file, audit)
                    .map_err(|error| format!("write ADM profile report: {error}"))
            })?;
        }
        if let Some(path) = ebu_qc_xml {
            let (metadata, analysis, options, results, timeline, timeline_interval_ms) =
                ebu_qc_xml_output
                    .as_ref()
                    .expect("EBU QC XML analysis always produces report data");
            write_file_atomically(path, cli.overwrite, |file| {
                ebu_qc_scenario1::write_xml(
                    file,
                    metadata,
                    analysis,
                    options,
                    results,
                    timeline,
                    *timeline_interval_ms,
                )
            })?;
        }
        write_catalogue_report(
            catalogue.as_ref(),
            catalogue_options.report.as_deref(),
            std::mem::take(&mut catalogue_records),
            cli.overwrite,
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
        if cli.album && batch_options.job_state.is_none() {
            validate_outputs(&cli.inputs, &outputs, cli.overwrite)?;
        }
    }
    let mut difference_assets = Vec::new();

    let operation = batch_operation_descriptor(
        &cli,
        &plan,
        &formats,
        metadata_options,
        analysis_engine,
        audio_track,
    );
    let batch_assets = cli
        .inputs
        .iter()
        .zip(&outputs)
        .map(|(input, output)| BatchAssetSpec::new(input, output))
        .collect::<Vec<_>>();
    let failure_policy = if batch_options.keep_going {
        BatchFailurePolicy::KeepGoing
    } else {
        BatchFailurePolicy::FailFast
    };
    let semantic_context = batch_options
        .job_state
        .as_ref()
        .map(|_| normalization_semantic_context(analysis_engine, audio_track, &formats))
        .transpose()?;
    let mut batch_job = batch_options
        .job_state
        .as_ref()
        .map(|path| {
            BatchJob::open_v3(
                path,
                &batch_assets,
                &operation,
                semantic_context
                    .as_ref()
                    .expect("job state requested semantic context"),
                NORMALIZATION_FINGERPRINT_REVISION,
                failure_policy,
                cli.overwrite,
            )
        })
        .transpose()?;
    let generation_journal = batch_options
        .job_state
        .as_deref()
        .map(generation_state_path)
        .transpose()?;
    if let (Some(job), Some(journal)) = (&mut batch_job, generation_journal.as_deref()) {
        let reconciliation = reconcile_batch_generation(job, journal, cli.overwrite)?;
        if let BatchGenerationCheckpointOutcome::CommittedNeedsCheckpoint(error) = reconciliation {
            let progress_result = emit_completed_resume_progress(
                batch_options.progress.as_deref(),
                job,
                &cli.inputs,
                &outputs,
            );
            return Err(committed_checkpoint_failure(error, progress_result));
        }
        if job.is_complete() {
            // Progress describes the durable audio generation. Auxiliary
            // catalogue/report repair below is a separate documented write
            // boundary and must not hide the already-committed terminal event.
            emit_completed_resume_progress(
                batch_options.progress.as_deref(),
                job,
                &cli.inputs,
                &outputs,
            )?;
            // A completed generation is an audio no-op, but auxiliary outputs
            // are invocation-scoped.  Rebuild requested catalogue records and
            // reports (and a requested zero-failure report) before returning so
            // a resumed invocation can repair an artifact deleted after the
            // original commit without touching any published audio.
            if catalogue.is_some() || batch_options.failure_report.is_some() {
                if catalogue.is_some() {
                    let repaired = if let Some(cache) = analysis_cache.as_ref() {
                        analyze_many_for_plan_cached(
                            cache,
                            &cli.inputs,
                            channel_roles_override.as_deref(),
                            &plan,
                            audio_track,
                        )?
                    } else {
                        analyze_many_for_plan_uncached(
                            &cli.inputs,
                            channel_roles_override.as_deref(),
                            &plan,
                            audio_track,
                        )?
                    };
                    for (index, analysis) in repaired.iter().enumerate() {
                        job.verify_input_binding(
                            index,
                            analysis.descriptor.stable_input().binding(),
                        )?;
                        record_catalogue_asset(
                            catalogue.as_mut(),
                            &mut catalogue_records,
                            CatalogueAsset {
                                source: &cli.inputs[index],
                                expected_source_sha256: catalogue_source_hashes
                                    .get(&cli.inputs[index])
                                    .map_or("", String::as_str),
                                output: Some(&outputs[index]),
                                measurement: analysis.analysis.analysis(),
                                operation: "normalization",
                                profile: &catalogue_profile(&cli, &plan),
                                provenance: catalogue_provenance(&cli, &plan, "normalization"),
                            },
                            Some(&analysis.descriptor),
                            catalogue_descriptor_options(
                                &cli,
                                channel_roles_override.as_deref(),
                                audio_track,
                            ),
                            &plan,
                            catalogue_output_renderer(formats[index]),
                        )?;
                    }
                    write_catalogue_report(
                        catalogue.as_ref(),
                        catalogue_options.report.as_deref(),
                        std::mem::take(&mut catalogue_records),
                        cli.overwrite,
                    )?;
                }
                if let Some(path) = batch_options.failure_report.as_deref() {
                    let job_id = job
                        .job_id()
                        .ok_or_else(|| {
                            "completed batch state has no v3 job identity for failure report"
                                .to_string()
                        })?
                        .to_owned();
                    let semantic_fingerprint = job
                        .semantic_fingerprint()
                        .ok_or_else(|| {
                            "completed batch state has no semantic fingerprint for failure report"
                                .to_string()
                        })?
                        .to_owned();
                    let fingerprint_revision = job.fingerprint_revision().ok_or_else(|| {
                        "completed batch state has no fingerprint revision for failure report"
                            .to_string()
                    })?;
                    let mut report = BatchFailureReport::new(
                        &job_id,
                        semantic_fingerprint,
                        fingerprint_revision,
                        failure_policy,
                        job.asset_count(),
                    );
                    report.set_completed_counts(job.asset_count(), 0);
                    write_batch_failure_report(path, &report)?;
                }
            }
            eprintln!(
                "batch generation already committed and verified: {} assets",
                job.asset_count()
            );
            return Ok(());
        }
    }

    if cli.album {
        // A v3 album generation is all-or-nothing.  Only destinations that
        // still need work participate in the preflight check; completed
        // members have already been validated by the generation journal.
        let pending_album = cli
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
            &pending_album
                .iter()
                .map(|(input, _)| (*input).clone())
                .collect::<Vec<_>>(),
            &pending_album
                .iter()
                .map(|(_, output)| (*output).clone())
                .collect::<Vec<_>>(),
            cli.overwrite,
        )?;

        let mut album_progress = batch_options
            .progress
            .as_deref()
            .map(|path| open_batch_progress(path, batch_job.as_ref()))
            .transpose()?;
        if let Some(writer) = &mut album_progress {
            writer.emit("job_started", 0, cli.inputs.len(), None, None)?;
            for (index, (input, output)) in cli.inputs.iter().zip(&outputs).enumerate() {
                writer.emit(
                    "asset_started",
                    0,
                    cli.inputs.len(),
                    Some((index, input, output)),
                    None,
                )?;
            }
        }

        let cached_analyses = if let Some(cache) = analysis_cache.as_ref() {
            analyze_many_for_plan_cached(
                cache,
                &cli.inputs,
                channel_roles_override.as_deref(),
                &plan,
                audio_track,
            )
        } else if batch_job.is_some() {
            // A durable album job must render from the exact immutable inputs
            // whose hashes formed its v3 identity. Capture and bind them even
            // when no analysis cache was requested.
            analyze_many_for_plan_uncached(
                &cli.inputs,
                channel_roles_override.as_deref(),
                &plan,
                audio_track,
            )
        } else {
            Ok(Vec::new())
        };
        let cached_analyses = match cached_analyses {
            Ok(analyses) if !analyses.is_empty() || batch_job.is_some() => Some(analyses),
            Ok(_) => None,
            Err(error) => {
                return Err(finish_generation_failure(
                    &mut album_progress,
                    batch_options.failure_report.as_deref(),
                    batch_job.as_ref(),
                    failure_policy,
                    &cli.inputs,
                    &outputs,
                    (!cli.inputs.is_empty()).then_some(0),
                    error,
                ));
            }
        };
        if let (Some(job), Some(analyses)) = (batch_job.as_ref(), cached_analyses.as_ref()) {
            for (index, analysis) in analyses.iter().enumerate() {
                if let Err(error) =
                    job.verify_input_binding(index, analysis.descriptor.stable_input().binding())
                {
                    return Err(finish_generation_failure(
                        &mut album_progress,
                        batch_options.failure_report.as_deref(),
                        batch_job.as_ref(),
                        failure_policy,
                        &cli.inputs,
                        &outputs,
                        Some(index),
                        error,
                    ));
                }
            }
        }
        if cli.dry_run {
            let analyses = if let Some(analyses) = cached_analyses {
                analyses
                    .into_iter()
                    .map(|cached| cached.analysis.analysis().clone())
                    .collect()
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
        if let Err(error) = prepare_output_directories(&outputs) {
            return Err(finish_generation_failure(
                &mut album_progress,
                batch_options.failure_report.as_deref(),
                batch_job.as_ref(),
                failure_policy,
                &cli.inputs,
                &outputs,
                None,
                error,
            ));
        }
        if cli.verify {
            let staged = if let Some(cached) = cached_analyses.as_deref() {
                let stable_inputs = cached
                    .iter()
                    .map(|cached| cached.input.clone())
                    .collect::<Vec<_>>();
                let bound_analyses = cached
                    .iter()
                    .map(|cached| cached.analysis.clone())
                    .collect::<Vec<_>>();
                normalize::normalize_album_bound_corrected_staged_with_policy(
                    &stable_inputs,
                    &outputs,
                    &plan,
                    &formats,
                    cli.verify_tolerance,
                    cli.verify_retries as usize,
                    &bound_analyses,
                    output_conflict_policy,
                )
                .map_err(|error| error.to_string())
            } else {
                normalize::normalize_album_corrected_staged_with_roles_and_policy(
                    &cli.inputs,
                    &outputs,
                    &plan,
                    &formats,
                    cli.verify_tolerance,
                    cli.verify_retries as usize,
                    channel_roles_override.as_deref(),
                    output_conflict_policy,
                )
            };
            let staged = match staged {
                Ok(staged) => staged,
                Err(error) => {
                    return Err(finish_generation_failure(
                        &mut album_progress,
                        batch_options.failure_report.as_deref(),
                        batch_job.as_ref(),
                        failure_policy,
                        &cli.inputs,
                        &outputs,
                        None,
                        error,
                    ));
                }
            };
            let corrected = if let (Some(job), Some(journal)) =
                (&mut batch_job, generation_journal.as_deref())
            {
                let (prepared, corrected) = match staged.into_generation_parts() {
                    Ok(parts) => parts,
                    Err(error) => {
                        return Err(finish_generation_failure(
                            &mut album_progress,
                            batch_options.failure_report.as_deref(),
                            Some(&*job),
                            failure_policy,
                            &cli.inputs,
                            &outputs,
                            None,
                            error,
                        ));
                    }
                };
                let commit_result = commit_batch_generation(job, journal, prepared);
                finish_batch_generation_commit(
                    commit_result,
                    &mut album_progress,
                    batch_options.failure_report.as_deref(),
                    Some(&*job),
                    failure_policy,
                    &cli.inputs,
                    &outputs,
                )?;
                corrected
            } else {
                match staged.commit() {
                    Ok(corrected) => corrected,
                    Err(error) => {
                        return Err(finish_generation_failure(
                            &mut album_progress,
                            batch_options.failure_report.as_deref(),
                            batch_job.as_ref(),
                            failure_policy,
                            &cli.inputs,
                            &outputs,
                            None,
                            error,
                        ));
                    }
                }
            };
            emit_album_completed(&mut album_progress, &cli.inputs, &outputs)?;
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
                    let error = format!("post-encode verification failed: {}", output.display());
                    emit_album_failed(&mut album_progress, &cli.inputs, &outputs, None, &error)?;
                    return Err(error);
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
                let error = "post-encode album verification failed".to_string();
                emit_album_failed(&mut album_progress, &cli.inputs, &outputs, None, &error)?;
                return Err(error);
            }
            for (index, ((input, output), source)) in cli
                .inputs
                .iter()
                .zip(&outputs)
                .zip(&corrected.sources)
                .enumerate()
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
                    None,
                    catalogue_descriptor_options(
                        &cli,
                        channel_roles_override.as_deref(),
                        audio_track,
                    ),
                    &plan,
                    catalogue_output_renderer(formats[index]),
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
                write_difference_report(path, difference_assets, cli.overwrite)?;
            }
            write_catalogue_report(
                catalogue.as_ref(),
                catalogue_options.report.as_deref(),
                std::mem::take(&mut catalogue_records),
                cli.overwrite,
            )?;
            return Ok(());
        }
        let staged = if cli.difference_report.is_some() {
            if let Some(cached) = cached_analyses.as_deref() {
                let stable_inputs = cached
                    .iter()
                    .map(|cached| cached.input.clone())
                    .collect::<Vec<_>>();
                let bound_analyses = cached
                    .iter()
                    .map(|cached| cached.analysis.clone())
                    .collect::<Vec<_>>();
                normalize::normalize_album_bound_audited_staged_with_policy(
                    &stable_inputs,
                    &outputs,
                    &plan,
                    &formats,
                    &bound_analyses,
                    output_conflict_policy,
                )
                .map_err(|error| error.to_string())
            } else {
                normalize::normalize_album_audited_staged_with_roles_and_policy(
                    &cli.inputs,
                    &outputs,
                    &plan,
                    &formats,
                    channel_roles_override.as_deref(),
                    output_conflict_policy,
                )
            }
        } else {
            if let Some(cached) = cached_analyses.as_deref() {
                let stable_inputs = cached
                    .iter()
                    .map(|cached| cached.input.clone())
                    .collect::<Vec<_>>();
                let bound_analyses = cached
                    .iter()
                    .map(|cached| cached.analysis.clone())
                    .collect::<Vec<_>>();
                normalize::normalize_album_bound_staged_with_policy(
                    &stable_inputs,
                    &outputs,
                    &plan,
                    &formats,
                    &bound_analyses,
                    output_conflict_policy,
                )
                .map_err(|error| error.to_string())
            } else {
                normalize::normalize_album_staged_with_roles_and_policy(
                    &cli.inputs,
                    &outputs,
                    &plan,
                    &formats,
                    channel_roles_override.as_deref(),
                    output_conflict_policy,
                )
            }
        };
        let staged = match staged {
            Ok(staged) => staged,
            Err(error) => {
                return Err(finish_generation_failure(
                    &mut album_progress,
                    batch_options.failure_report.as_deref(),
                    batch_job.as_ref(),
                    failure_policy,
                    &cli.inputs,
                    &outputs,
                    None,
                    error,
                ));
            }
        };
        let results =
            if let (Some(job), Some(journal)) = (&mut batch_job, generation_journal.as_deref()) {
                let (prepared, outcomes) = match staged.into_generation_parts() {
                    Ok(parts) => parts,
                    Err(error) => {
                        return Err(finish_generation_failure(
                            &mut album_progress,
                            batch_options.failure_report.as_deref(),
                            Some(&*job),
                            failure_policy,
                            &cli.inputs,
                            &outputs,
                            None,
                            error,
                        ));
                    }
                };
                let commit_result = commit_batch_generation(job, journal, prepared);
                finish_batch_generation_commit(
                    commit_result,
                    &mut album_progress,
                    batch_options.failure_report.as_deref(),
                    Some(&*job),
                    failure_policy,
                    &cli.inputs,
                    &outputs,
                )?;
                outcomes
            } else {
                match staged.commit() {
                    Ok(results) => results,
                    Err(error) => {
                        return Err(finish_generation_failure(
                            &mut album_progress,
                            batch_options.failure_report.as_deref(),
                            batch_job.as_ref(),
                            failure_policy,
                            &cli.inputs,
                            &outputs,
                            None,
                            error,
                        ));
                    }
                }
            };
        emit_album_completed(&mut album_progress, &cli.inputs, &outputs)?;
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
                None,
                catalogue_descriptor_options(&cli, channel_roles_override.as_deref(), audio_track),
                &plan,
                catalogue_output_renderer(formats[index]),
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
            write_difference_report(path, difference_assets, cli.overwrite)?;
        }
        write_catalogue_report(
            catalogue.as_ref(),
            catalogue_options.report.as_deref(),
            std::mem::take(&mut catalogue_records),
            cli.overwrite,
        )?;
        return Ok(());
    }

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
        // A progress path denotes a per-invocation live stream. Preserve its
        // historical replace-on-open behavior independently of audio output
        // overwrite policy; aliases were rejected by the output plan above.
        .map(|path| open_batch_progress(path, batch_job.as_ref()))
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

    if cli.verify {
        if let Some(job) = batch_job.as_mut() {
            let job_id = job
                .job_id()
                .ok_or_else(|| "generation batch requires a v3 job identity".to_string())?
                .to_owned();
            let semantic_fingerprint = job
                .semantic_fingerprint()
                .ok_or_else(|| "generation batch requires a semantic fingerprint".to_string())?
                .to_owned();
            let fingerprint_revision = job
                .fingerprint_revision()
                .ok_or_else(|| "generation batch requires a fingerprint revision".to_string())?;
            let mut failure_report = BatchFailureReport::new(
                &job_id,
                semantic_fingerprint,
                fingerprint_revision,
                failure_policy,
                cli.inputs.len(),
            );
            let mut prepared_outputs = Vec::with_capacity(cli.inputs.len());
            let mut completed_assets = Vec::with_capacity(cli.inputs.len());
            for (index, ((input, output), format)) in
                cli.inputs.iter().zip(&outputs).zip(&formats).enumerate()
            {
                if let Some(writer) = &mut progress {
                    writer.emit(
                        "asset_started",
                        0,
                        cli.inputs.len(),
                        Some((index, input, output)),
                        None,
                    )?;
                }
                let result = (|| {
                    prepare_output_directories(std::slice::from_ref(output))?;
                    let analyzed = if let Some(cache) = analysis_cache.as_ref() {
                        analyze_for_plan_cached(
                            cache,
                            input,
                            channel_roles_override.as_deref(),
                            &plan,
                            audio_track,
                        )?
                    } else {
                        analyze_for_plan_descriptor(
                            input,
                            channel_roles_override.as_deref(),
                            &plan,
                            audio_track,
                        )?
                    };
                    job.verify_input_binding(index, analyzed.descriptor.stable_input().binding())?;
                    let staged = if metadata_options.active_for_normalization() {
                    normalize::normalize_one_descriptor_bound_corrected_staged_with_metadata_policy(
                        &analyzed.descriptor,
                        output,
                        &plan,
                        *format,
                        cli.verify_tolerance,
                        cli.verify_retries as usize,
                        &analyzed.analysis,
                        &metadata_options.policy,
                        output_conflict_policy,
                    )
                } else {
                    normalize::normalize_one_descriptor_bound_corrected_staged_with_policy(
                        &analyzed.descriptor,
                        output,
                        &plan,
                        *format,
                        cli.verify_tolerance,
                        cli.verify_retries as usize,
                        &analyzed.analysis,
                        output_conflict_policy,
                    )
                }
                .map_err(|error| error.to_string())?;
                    let (prepared, outcome) = staged.into_generation_parts()?;
                    Ok::<_, String>((prepared, outcome, analyzed.descriptor))
                })();
                match result {
                    Ok((prepared, outcome, descriptor)) => {
                        prepared_outputs.push(prepared);
                        completed_assets.push((index, outcome, descriptor));
                    }
                    Err(error) => {
                        if let Some(writer) = &mut progress {
                            writer.emit(
                                "asset_failed",
                                0,
                                cli.inputs.len(),
                                Some((index, input, output)),
                                Some(&error),
                            )?;
                        }
                        failure_report.add_failure(BatchFailure::new(
                            index,
                            bounded_utf8(&input.to_string_lossy(), MAX_BATCH_PROGRESS_PATH_BYTES),
                            bounded_utf8(&output.to_string_lossy(), MAX_BATCH_PROGRESS_PATH_BYTES),
                            error.clone(),
                        ))?;
                        if !batch_options.keep_going {
                            failure_report.set_completed_counts(completed_assets.len(), 0);
                            return Err(finish_batch_report_failure(
                                &mut progress,
                                batch_options.failure_report.as_deref(),
                                &failure_report,
                                cli.inputs.len(),
                                format!(
                                    "asset verification failed; no generation was published: {error}"
                                ),
                            ));
                        }
                    }
                }
            }
            failure_report.set_completed_counts(completed_assets.len(), 0);
            if failure_report.failed != 0 {
                let summary = format!(
                    "{} batch asset(s) failed verification; no generation was published",
                    failure_report.failed
                );
                return Err(finish_batch_report_failure(
                    &mut progress,
                    batch_options.failure_report.as_deref(),
                    &failure_report,
                    cli.inputs.len(),
                    summary,
                ));
            }
            let journal = match generation_journal.as_deref() {
                Some(journal) => journal,
                None => {
                    let error = "generation batch has no journal path".to_string();
                    return Err(finish_generation_failure(
                        &mut progress,
                        batch_options.failure_report.as_deref(),
                        Some(&*job),
                        failure_policy,
                        &cli.inputs,
                        &outputs,
                        None,
                        error,
                    ));
                }
            };
            let commit_result = commit_batch_generation(job, journal, prepared_outputs);
            finish_batch_generation_commit(
                commit_result,
                &mut progress,
                batch_options.failure_report.as_deref(),
                Some(&*job),
                failure_policy,
                &cli.inputs,
                &outputs,
            )?;
            emit_album_completed(&mut progress, &cli.inputs, &outputs)?;
            if let Some(path) = batch_options.failure_report.as_deref() {
                // A success report is only valid after the generation journal
                // and v3 checkpoint have committed successfully.
                write_batch_failure_report(path, &failure_report)?;
            }
            for (index, corrected, descriptor) in &completed_assets {
                let input = &cli.inputs[*index];
                let output = &outputs[*index];
                print_analysis(input, &corrected.source, Some(corrected.gain));
                if !print_verification(input, &corrected.verification, &plan) {
                    return Err(format!(
                        "post-encode verification failed after generation publication: {}",
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
                record_catalogue_asset(
                    catalogue.as_mut(),
                    &mut catalogue_records,
                    CatalogueAsset {
                        source: input,
                        expected_source_sha256: catalogue_source_hashes
                            .get(input)
                            .map_or("", String::as_str),
                        output: Some(output),
                        measurement: &corrected.source,
                        operation: "normalization",
                        profile: &catalogue_profile(&cli, &plan),
                        provenance: catalogue_provenance(&cli, &plan, "normalization"),
                    },
                    Some(descriptor),
                    catalogue_descriptor_options(
                        &cli,
                        channel_roles_override.as_deref(),
                        audio_track,
                    ),
                    &plan,
                    catalogue_output_renderer(formats[*index]),
                )?;
            }
            write_catalogue_report(
                catalogue.as_ref(),
                catalogue_options.report.as_deref(),
                catalogue_records,
                cli.overwrite,
            )?;
            return Ok(());
        }
    }

    let parallel_batch = cli.inputs.len() > 1
        && !cli.gain_only
        && !cli.dry_run
        && !cli.verify
        && cli.difference_report.is_none();
    if parallel_batch {
        if let Some(job) = batch_job.as_mut() {
            let job_id = job
                .job_id()
                .ok_or_else(|| "generation batch requires a v3 job identity".to_string())?
                .to_owned();
            let semantic_fingerprint = job
                .semantic_fingerprint()
                .ok_or_else(|| "generation batch requires a semantic fingerprint".to_string())?
                .to_owned();
            let fingerprint_revision = job
                .fingerprint_revision()
                .ok_or_else(|| "generation batch requires a fingerprint revision".to_string())?;
            let mut failure_report = BatchFailureReport::new(
                &job_id,
                semantic_fingerprint,
                fingerprint_revision,
                failure_policy,
                cli.inputs.len(),
            );
            let wave_width = rayon::current_num_threads().clamp(1, MAX_BATCH_WAVE_ASSETS);
            let mut staged_assets = (0..cli.inputs.len()).map(|_| None).collect::<Vec<_>>();
            let mut index = 0_usize;
            while index < cli.inputs.len() {
                let wave_start = index;
                let wave_end = (wave_start + wave_width).min(cli.inputs.len());
                if let Some(writer) = &mut progress {
                    for (offset, (input, output)) in cli.inputs[wave_start..wave_end]
                        .iter()
                        .zip(&outputs[wave_start..wave_end])
                        .enumerate()
                    {
                        writer.emit(
                            "asset_started",
                            0,
                            cli.inputs.len(),
                            Some((wave_start + offset, input, output)),
                            None,
                        )?;
                    }
                }
                let staged = (wave_start..wave_end)
                    .into_par_iter()
                    .map(|asset_index| {
                        let output = &outputs[asset_index];
                        if let Err(error) = prepare_output_directories(std::slice::from_ref(output))
                        {
                            return (Err(error), None);
                        }
                        let (analyzed, observation) = if let Some(cache) = analysis_cache.as_ref() {
                            match analyze_for_plan_cached_unobserved(
                                cache,
                                &cli.inputs[asset_index],
                                channel_roles_override.as_deref(),
                                &plan,
                                audio_track,
                            ) {
                                Ok((analysis, observation)) => (analysis, Some(observation)),
                                Err(error) => return (Err(error), None),
                            }
                        } else {
                            match analyze_for_plan_descriptor(
                                &cli.inputs[asset_index],
                                channel_roles_override.as_deref(),
                                &plan,
                                audio_track,
                            ) {
                                Ok(analysis) => (analysis, None),
                                Err(error) => return (Err(error), None),
                            }
                        };
                        let staged = if metadata_options.active_for_normalization() {
                            normalize::normalize_one_descriptor_bound_staged_with_metadata_policy(
                                &analyzed.descriptor,
                                output,
                                &plan,
                                formats[asset_index],
                                &analyzed.analysis,
                                &metadata_options.policy,
                                output_conflict_policy,
                            )
                        } else {
                            normalize::normalize_one_descriptor_bound_staged_with_policy(
                                &analyzed.descriptor,
                                output,
                                &plan,
                                formats[asset_index],
                                &analyzed.analysis,
                                output_conflict_policy,
                            )
                        }
                        .map(|staged| (staged, analyzed.descriptor))
                        .map_err(|error| error.to_string());
                        (staged, observation)
                    })
                    .collect::<Vec<_>>();

                for (asset_index, (staged, observation)) in (wave_start..wave_end).zip(staged) {
                    let input = &cli.inputs[asset_index];
                    let output = &outputs[asset_index];
                    if let Some(observation) = observation {
                        observe_cache_parts(input, observation.disposition, observation.warning);
                    }
                    let staged = staged.and_then(|(staged, descriptor)| {
                        job.verify_input_binding(asset_index, descriptor.stable_input().binding())?;
                        Ok((staged, descriptor))
                    });
                    match staged {
                        Ok(staged) => staged_assets[asset_index] = Some(staged),
                        Err(error) => {
                            if let Some(writer) = &mut progress {
                                writer.emit(
                                    "asset_failed",
                                    0,
                                    cli.inputs.len(),
                                    Some((asset_index, input, output)),
                                    Some(&error),
                                )?;
                            }
                            failure_report.add_failure(BatchFailure::new(
                                asset_index,
                                bounded_utf8(
                                    &input.to_string_lossy(),
                                    MAX_BATCH_PROGRESS_PATH_BYTES,
                                ),
                                bounded_utf8(
                                    &output.to_string_lossy(),
                                    MAX_BATCH_PROGRESS_PATH_BYTES,
                                ),
                                error.clone(),
                            ))?;
                            if !batch_options.keep_going {
                                let rendered =
                                    staged_assets.iter().filter(|asset| asset.is_some()).count();
                                failure_report.set_completed_counts(rendered, 0);
                                return Err(finish_batch_report_failure(
                                    &mut progress,
                                    batch_options.failure_report.as_deref(),
                                    &failure_report,
                                    cli.inputs.len(),
                                    format!(
                                        "asset rendering failed; no generation was published: {error}"
                                    ),
                                ));
                            }
                        }
                    }
                }
                index = wave_end;
            }

            let rendered = staged_assets.iter().filter(|asset| asset.is_some()).count();
            failure_report.set_completed_counts(rendered, 0);
            if failure_report.failed != 0 {
                let summary = format!(
                    "{} batch asset(s) failed; no generation was published",
                    failure_report.failed
                );
                return Err(finish_batch_report_failure(
                    &mut progress,
                    batch_options.failure_report.as_deref(),
                    &failure_report,
                    cli.inputs.len(),
                    summary,
                ));
            }

            let mut prepared_outputs = Vec::with_capacity(staged_assets.len());
            let mut completed_assets = Vec::with_capacity(staged_assets.len());
            let preparation = (|| -> Result<(), String> {
                for staged in staged_assets {
                    let (staged, descriptor) = staged.ok_or_else(|| {
                        "generation batch lost a successfully rendered asset".to_string()
                    })?;
                    let (prepared, outcome) = staged.into_generation_parts()?;
                    prepared_outputs.push(prepared);
                    completed_assets.push((outcome, descriptor));
                }
                Ok(())
            })();
            if let Err(error) = preparation {
                return Err(finish_generation_failure(
                    &mut progress,
                    batch_options.failure_report.as_deref(),
                    Some(&*job),
                    failure_policy,
                    &cli.inputs,
                    &outputs,
                    None,
                    error,
                ));
            }
            let journal = match generation_journal.as_deref() {
                Some(journal) => journal,
                None => {
                    let error = "generation batch has no journal path".to_string();
                    return Err(finish_generation_failure(
                        &mut progress,
                        batch_options.failure_report.as_deref(),
                        Some(&*job),
                        failure_policy,
                        &cli.inputs,
                        &outputs,
                        None,
                        error,
                    ));
                }
            };
            let commit_result = commit_batch_generation(job, journal, prepared_outputs);
            finish_batch_generation_commit(
                commit_result,
                &mut progress,
                batch_options.failure_report.as_deref(),
                Some(&*job),
                failure_policy,
                &cli.inputs,
                &outputs,
            )?;
            emit_album_completed(&mut progress, &cli.inputs, &outputs)?;
            if let Some(path) = batch_options.failure_report.as_deref() {
                // Do not publish a success report until the all-or-nothing
                // generation journal has committed.
                write_batch_failure_report(path, &failure_report)?;
            }

            for (asset_index, (outcome, descriptor)) in completed_assets.iter().enumerate() {
                let input = &cli.inputs[asset_index];
                let output = &outputs[asset_index];
                print_analysis(input, &outcome.source, Some(outcome.gain));
                record_catalogue_asset(
                    catalogue.as_mut(),
                    &mut catalogue_records,
                    CatalogueAsset {
                        source: input,
                        expected_source_sha256: catalogue_source_hashes
                            .get(input)
                            .map_or("", String::as_str),
                        output: Some(output),
                        measurement: &outcome.source,
                        operation: "normalization",
                        profile: &catalogue_profile(&cli, &plan),
                        provenance: catalogue_provenance(&cli, &plan, "normalization"),
                    },
                    Some(descriptor),
                    catalogue_descriptor_options(
                        &cli,
                        channel_roles_override.as_deref(),
                        audio_track,
                    ),
                    &plan,
                    catalogue_output_renderer(formats[asset_index]),
                )?;
            }
            write_catalogue_report(
                catalogue.as_ref(),
                catalogue_options.report.as_deref(),
                catalogue_records,
                cli.overwrite,
            )?;
            return Ok(());
        }
        let wave_width = rayon::current_num_threads().clamp(1, MAX_BATCH_WAVE_ASSETS);
        let mut index = 0_usize;
        let mut completed_without_job = 0_usize;
        while index < cli.inputs.len() {
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
                        Some((index, &cli.inputs[index], &outputs[index])),
                        None,
                    )?;
                }
                index += 1;
                continue;
            }

            let wave_start = index;
            let mut wave_end = wave_start;
            while wave_end < cli.inputs.len()
                && wave_end - wave_start < wave_width
                && !batch_job
                    .as_ref()
                    .is_some_and(|job| job.is_completed(wave_end))
            {
                wave_end += 1;
            }
            let completed_before_wave = batch_job
                .as_ref()
                .map_or(completed_without_job, BatchJob::completed_count);
            if let Some(writer) = &mut progress {
                for (offset, (input, output)) in cli.inputs[wave_start..wave_end]
                    .iter()
                    .zip(&outputs[wave_start..wave_end])
                    .enumerate()
                {
                    let asset_index = wave_start + offset;
                    writer.emit(
                        "asset_started",
                        completed_before_wave,
                        cli.inputs.len(),
                        Some((asset_index, input, output)),
                        None,
                    )?;
                }
            }

            let staged = (wave_start..wave_end)
                .into_par_iter()
                .map(|asset_index| {
                    let output = &outputs[asset_index];
                    if let Err(error) = prepare_output_directories(std::slice::from_ref(output)) {
                        return (Err(error), None);
                    }
                    let (analyzed, observation) = if let Some(cache) = analysis_cache.as_ref() {
                        match analyze_for_plan_cached_unobserved(
                            cache,
                            &cli.inputs[asset_index],
                            channel_roles_override.as_deref(),
                            &plan,
                            audio_track,
                        ) {
                            Ok((analysis, observation)) => (analysis, Some(observation)),
                            Err(error) => return (Err(error), None),
                        }
                    } else {
                        match analyze_for_plan_descriptor(
                            &cli.inputs[asset_index],
                            channel_roles_override.as_deref(),
                            &plan,
                            audio_track,
                        ) {
                            Ok(analysis) => (analysis, None),
                            Err(error) => return (Err(error), None),
                        }
                    };
                    let staged = if metadata_options.active_for_normalization() {
                        normalize::normalize_one_descriptor_bound_staged_with_metadata_policy(
                            &analyzed.descriptor,
                            output,
                            &plan,
                            formats[asset_index],
                            &analyzed.analysis,
                            &metadata_options.policy,
                            output_conflict_policy,
                        )
                    } else {
                        normalize::normalize_one_descriptor_bound_staged_with_policy(
                            &analyzed.descriptor,
                            output,
                            &plan,
                            formats[asset_index],
                            &analyzed.analysis,
                            output_conflict_policy,
                        )
                    }
                    .map(|staged| (staged, analyzed.descriptor))
                    .map_err(|error| error.to_string());
                    (staged, observation)
                })
                .collect::<Vec<_>>();

            for (asset_index, (staged, observation)) in (wave_start..wave_end).zip(staged) {
                let input = &cli.inputs[asset_index];
                let output = &outputs[asset_index];
                if let Some(observation) = observation {
                    observe_cache_parts(input, observation.disposition, observation.warning);
                }
                let outcome = match staged.and_then(|(staged, descriptor)| {
                    if let Some(report) = staged.metadata_report() {
                        report
                            .require_publication()
                            .map_err(|error| error.to_string())?;
                    }
                    if let Some(job) = &mut batch_job {
                        job.mark_ready_to_publish(asset_index, staged.staged_path())?;
                    }
                    staged.commit().map(|outcome| (outcome, descriptor))
                }) {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        if let Some(writer) = &mut progress {
                            writer.emit(
                                "asset_failed",
                                batch_job
                                    .as_ref()
                                    .map_or(completed_without_job, BatchJob::completed_count),
                                cli.inputs.len(),
                                Some((asset_index, input, output)),
                                Some(&error),
                            )?;
                            if batch_job.is_none() {
                                writer.emit(
                                    "job_failed",
                                    0,
                                    cli.inputs.len(),
                                    None,
                                    Some(&error),
                                )?;
                            }
                        }
                        return Err(error);
                    }
                };
                let (outcome, descriptor) = outcome;
                print_analysis(input, &outcome.source, Some(outcome.gain));
                record_catalogue_asset(
                    catalogue.as_mut(),
                    &mut catalogue_records,
                    CatalogueAsset {
                        source: input,
                        expected_source_sha256: catalogue_source_hashes
                            .get(input)
                            .map_or("", String::as_str),
                        output: Some(output),
                        measurement: &outcome.source,
                        operation: "normalization",
                        profile: &catalogue_profile(&cli, &plan),
                        provenance: catalogue_provenance(&cli, &plan, "normalization"),
                    },
                    Some(&descriptor),
                    catalogue_descriptor_options(
                        &cli,
                        channel_roles_override.as_deref(),
                        audio_track,
                    ),
                    &plan,
                    catalogue_output_renderer(formats[asset_index]),
                )?;
                if let Some(job) = &mut batch_job {
                    job.mark_completed(asset_index)?;
                } else {
                    completed_without_job += 1;
                }
                if let Some(writer) = &mut progress {
                    writer.emit(
                        "asset_completed",
                        batch_job
                            .as_ref()
                            .map_or(completed_without_job, BatchJob::completed_count),
                        cli.inputs.len(),
                        Some((asset_index, input, output)),
                        None,
                    )?;
                }
            }
            index = wave_end;
        }
        if let Some(writer) = &mut progress {
            writer.emit(
                "job_completed",
                batch_job
                    .as_ref()
                    .map_or(completed_without_job, BatchJob::completed_count),
                cli.inputs.len(),
                None,
                None,
            )?;
        }
        write_catalogue_report(
            catalogue.as_ref(),
            catalogue_options.report.as_deref(),
            catalogue_records,
            cli.overwrite,
        )?;
        return Ok(());
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
            let cached_analysis = if let Some(cache) = analysis_cache.as_ref() {
                Some(analyze_for_plan_cached(
                    cache,
                    input,
                    channel_roles_override.as_deref(),
                    &plan,
                    audio_track,
                )?)
            } else if cli.gain_only
                || cli.dry_run
                || cli.verify
                || cli.difference_report.is_some()
                || metadata_options.active_for_normalization()
            {
                Some(analyze_for_plan_descriptor(
                    input,
                    channel_roles_override.as_deref(),
                    &plan,
                    audio_track,
                )?)
            } else {
                None
            };
            if cli.gain_only || cli.dry_run {
                let an = if let Some(analysis) = cached_analysis {
                    analysis.analysis.analysis().clone()
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
                    let staged = if metadata_options.active_for_normalization() {
                        let analysis = cached_analysis
                            .as_ref()
                            .expect("metadata fidelity captures a bound descriptor analysis");
                        if let Some(watch_output) = watch_output.as_ref() {
                            normalize::normalize_one_descriptor_bound_corrected_staged_with_metadata_policy_and_watch_output(
                                &analysis.descriptor,
                                output,
                                &plan,
                                *fmt,
                                cli.verify_tolerance,
                                cli.verify_retries as usize,
                                &analysis.analysis,
                                &metadata_options.policy,
                                output_conflict_policy,
                                watch_output,
                            )
                        } else {
                            normalize::normalize_one_descriptor_bound_corrected_staged_with_metadata_policy(
                                &analysis.descriptor,
                                output,
                                &plan,
                                *fmt,
                                cli.verify_tolerance,
                                cli.verify_retries as usize,
                                &analysis.analysis,
                                &metadata_options.policy,
                                output_conflict_policy,
                            )
                        }
                        .map_err(|error| error.to_string())?
                    } else if let Some(analysis) = cached_analysis.as_ref() {
                        if let Some(watch_output) = watch_output.as_ref() {
                            normalize::normalize_one_descriptor_bound_corrected_staged_with_watch_output(
                                &analysis.descriptor,
                                output,
                                &plan,
                                *fmt,
                                cli.verify_tolerance,
                                cli.verify_retries as usize,
                                &analysis.analysis,
                                output_conflict_policy,
                                watch_output,
                            )
                        } else {
                            normalize::normalize_one_descriptor_bound_corrected_staged_with_policy(
                                &analysis.descriptor,
                                output,
                                &plan,
                                *fmt,
                                cli.verify_tolerance,
                                cli.verify_retries as usize,
                                &analysis.analysis,
                                output_conflict_policy,
                            )
                        }
                        .map_err(|error| error.to_string())?
                    } else {
                        if let Some(watch_output) = watch_output.as_ref() {
                            normalize::normalize_one_corrected_staged_with_roles_and_watch_output(
                                input,
                                output,
                                &plan,
                                *fmt,
                                cli.verify_tolerance,
                                cli.verify_retries as usize,
                                channel_roles_override.as_deref(),
                                output_conflict_policy,
                                watch_output,
                            )?
                        } else {
                            normalize::normalize_one_corrected_staged_with_roles_and_policy(
                                input,
                                output,
                                &plan,
                                *fmt,
                                cli.verify_tolerance,
                                cli.verify_retries as usize,
                                channel_roles_override.as_deref(),
                                output_conflict_policy,
                            )?
                        }
                    };
                    let metadata_report = staged.metadata_report().cloned();
                    let staged_metadata_report = stage_requested_metadata_fidelity_report(
                        metadata_options,
                        metadata_report.as_ref(),
                        cli.overwrite,
                    )?;
                    if let Some(report) = staged.metadata_report() {
                        report
                            .require_publication()
                            .map_err(|error| error.to_string())?;
                    }
                    if let Some(job) = &mut batch_job {
                        job.mark_ready_to_publish(index, staged.staged_path())?;
                    }
                    let corrected = staged.commit()?;
                    publish_requested_metadata_fidelity_report(
                        metadata_options,
                        staged_metadata_report,
                    )?;
                    verify_requested_metadata_fidelity_report(
                        metadata_options,
                        metadata_report.as_ref(),
                    )?;
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
                        let (an, gain, render) = if metadata_options.active_for_normalization() {
                            let analysis = cached_analysis
                                .as_ref()
                                .expect("metadata fidelity captures a bound descriptor analysis");
                            let staged = normalize::normalize_one_descriptor_bound_audited_staged_with_metadata_policy(
                                &analysis.descriptor,
                                output,
                                &plan,
                                *fmt,
                                &analysis.analysis,
                                &metadata_options.policy,
                                output_conflict_policy,
                            )
                            .map_err(|error| error.to_string())?;
                            let metadata_report = staged.metadata_report().cloned();
                            let staged_metadata_report = stage_requested_metadata_fidelity_report(
                                metadata_options,
                                metadata_report.as_ref(),
                                cli.overwrite,
                            )?;
                            let outcome = staged.commit()?;
                            publish_requested_metadata_fidelity_report(
                                metadata_options,
                                staged_metadata_report,
                            )?;
                            verify_requested_metadata_fidelity_report(
                                metadata_options,
                                metadata_report.as_ref(),
                            )?;
                            (
                                outcome.source,
                                outcome.gain,
                                outcome
                                    .render
                                    .expect("audited metadata render captures statistics"),
                            )
                        } else if let Some(analysis) = cached_analysis.as_ref() {
                            normalize::normalize_one_descriptor_bound_audited_with_policy(
                                &analysis.descriptor,
                                output,
                                &plan,
                                *fmt,
                                &analysis.analysis,
                                output_conflict_policy,
                            )
                            .map_err(|error| error.to_string())?
                        } else {
                            normalize::normalize_one_audited_with_roles_and_policy(
                                input,
                                output,
                                &plan,
                                *fmt,
                                channel_roles_override.as_deref(),
                                output_conflict_policy,
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
                        let (an, gain) = if metadata_options.active_for_normalization() {
                            let analysis = cached_analysis
                                .as_ref()
                                .expect("metadata fidelity captures a bound descriptor analysis");
                            let staged = if let Some(watch_output) = watch_output.as_ref() {
                                normalize::normalize_one_descriptor_bound_staged_with_metadata_policy_and_watch_output(
                                    &analysis.descriptor,
                                    output,
                                    &plan,
                                    *fmt,
                                    &analysis.analysis,
                                    &metadata_options.policy,
                                    output_conflict_policy,
                                    watch_output,
                                )
                            } else {
                                normalize::normalize_one_descriptor_bound_staged_with_metadata_policy(
                                    &analysis.descriptor,
                                    output,
                                    &plan,
                                    *fmt,
                                    &analysis.analysis,
                                    &metadata_options.policy,
                                    output_conflict_policy,
                                )
                            }
                            .map_err(|error| error.to_string())?;
                            let metadata_report = staged.metadata_report().cloned();
                            let staged_metadata_report = stage_requested_metadata_fidelity_report(
                                metadata_options,
                                metadata_report.as_ref(),
                                cli.overwrite,
                            )?;
                            let outcome = staged.commit()?;
                            publish_requested_metadata_fidelity_report(
                                metadata_options,
                                staged_metadata_report,
                            )?;
                            verify_requested_metadata_fidelity_report(
                                metadata_options,
                                metadata_report.as_ref(),
                            )?;
                            (outcome.source, outcome.gain)
                        } else if let Some(analysis) = cached_analysis.as_ref() {
                            let staged = if let Some(watch_output) = watch_output.as_ref() {
                                normalize::normalize_one_descriptor_bound_staged_with_watch_output(
                                    &analysis.descriptor,
                                    output,
                                    &plan,
                                    *fmt,
                                    &analysis.analysis,
                                    output_conflict_policy,
                                    watch_output,
                                )
                            } else {
                                normalize::normalize_one_descriptor_bound_staged_with_policy(
                                    &analysis.descriptor,
                                    output,
                                    &plan,
                                    *fmt,
                                    &analysis.analysis,
                                    output_conflict_policy,
                                )
                            }
                            .map_err(|error| error.to_string())?;
                            let outcome = staged.commit()?;
                            (outcome.source, outcome.gain)
                        } else {
                            let descriptor = input_descriptor_for_path(
                                input,
                                channel_roles_override.as_deref(),
                                audio_track,
                            )?;
                            if let Some(watch_output) = watch_output.as_ref() {
                                let staged =
                                    normalize::normalize_one_descriptor_staged_with_watch_output(
                                        &descriptor,
                                        output,
                                        &plan,
                                        *fmt,
                                        output_conflict_policy,
                                        watch_output,
                                    )
                                    .map_err(|error| error.to_string())?;
                                let outcome = staged.commit()?;
                                (outcome.source, outcome.gain)
                            } else {
                                normalize::normalize_one_descriptor_with_policy(
                                    &descriptor,
                                    output,
                                    &plan,
                                    *fmt,
                                    output_conflict_policy,
                                )
                                .map_err(|error| error.to_string())?
                            }
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
                if batch_job.is_none() {
                    writer.emit("job_failed", 0, cli.inputs.len(), None, Some(&error))?;
                }
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
                None,
                catalogue_descriptor_options(&cli, channel_roles_override.as_deref(), audio_track),
                &plan,
                catalogue_output_renderer(*fmt),
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
        write_difference_report(path, difference_assets, cli.overwrite)?;
    }
    write_catalogue_report(
        catalogue.as_ref(),
        catalogue_options.report.as_deref(),
        catalogue_records,
        cli.overwrite,
    )?;
    Ok(())
}

fn write_difference_report(
    path: &Path,
    assets: Vec<NormalizationDifferenceAsset>,
    overwrite: bool,
) -> Result<(), String> {
    normalization_diff::write_report_with_overwrite(
        path,
        &NormalizationDifferenceReport::new(assets),
        overwrite,
    )
}

fn write_batch_failure_report(path: &Path, report: &BatchFailureReport) -> Result<(), String> {
    let bytes = report.to_bytes()?;
    write_file_atomically(path, true, |file| {
        file.write_all(&bytes)
            .map_err(|error| format!("write batch failure report: {error}"))
    })
}

fn write_generation_failure_report(
    path: Option<&Path>,
    job: Option<&BatchJob>,
    failure_policy: BatchFailurePolicy,
    inputs: &[PathBuf],
    outputs: &[PathBuf],
    error: &str,
) -> Result<(), String> {
    let Some(path) = path else {
        return Ok(());
    };
    let job = job.ok_or_else(|| {
        "batch failure report requires a v3 job identity for generation failure".to_string()
    })?;
    let job_id = job
        .job_id()
        .ok_or_else(|| "generation failure report requires a v3 job identity".to_string())?;
    let semantic_fingerprint = job
        .semantic_fingerprint()
        .ok_or_else(|| "generation failure report requires a semantic fingerprint".to_string())?;
    let fingerprint_revision = job
        .fingerprint_revision()
        .ok_or_else(|| "generation failure report requires a fingerprint revision".to_string())?;
    let mut report = BatchFailureReport::new(
        job_id,
        semantic_fingerprint.to_owned(),
        fingerprint_revision,
        failure_policy,
        inputs.len(),
    );
    for (index, (input, output)) in inputs.iter().zip(outputs).enumerate() {
        report.add_failure(BatchFailure::new(
            index,
            bounded_utf8(&input.to_string_lossy(), MAX_BATCH_PROGRESS_PATH_BYTES),
            bounded_utf8(&output.to_string_lossy(), MAX_BATCH_PROGRESS_PATH_BYTES),
            bounded_utf8(error, MAX_BATCH_FAILURE_ERROR_BYTES),
        ))?;
    }
    report.set_completed_counts(0, 0);
    write_batch_failure_report(path, &report)
}

fn stage_metadata_fidelity_report(
    path: &Path,
    report: &MetadataFidelityReport,
    overwrite: bool,
) -> Result<StagedFileOutput, String> {
    report.validate().map_err(|error| error.to_string())?;
    let encoded = metadata_fidelity_report_bytes(report)?;
    stage_file_atomically(path, overwrite, |file| {
        file.write_all(&encoded)
            .map_err(|error| format!("write metadata fidelity report: {error}"))
    })
}

fn metadata_fidelity_report_bytes(report: &MetadataFidelityReport) -> Result<Vec<u8>, String> {
    report.validate().map_err(|error| error.to_string())?;
    let mut encoded = serde_json::to_vec_pretty(report)
        .map_err(|error| format!("encode metadata fidelity report: {error}"))?;
    encoded.push(b'\n');
    Ok(encoded)
}

/// A committed metadata-only transaction may be resumed after its report was
/// already published but before the process returned success. Recognize only
/// the exact deterministic report bytes through the same bounded, symlink-safe
/// stable-input capture used elsewhere; a different destination still follows
/// the caller's normal overwrite/no-clobber policy.
fn metadata_fidelity_report_is_published(
    options: &MetadataInvocationOptions,
    report: Option<&MetadataFidelityReport>,
) -> Result<bool, String> {
    let (Some(path), Some(report)) = (options.report.as_deref(), report) else {
        return Ok(false);
    };
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(format!(
                "metadata fidelity report must not be a symbolic link: {}",
                path.display()
            ));
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(format!(
                "metadata fidelity report is not a regular file: {}",
                path.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "inspect metadata fidelity report {}: {error}",
                path.display()
            ));
        }
    }
    let expected = metadata_fidelity_report_bytes(report)?;
    let expected_byte_len = u64::try_from(expected.len())
        .map_err(|_| "metadata fidelity report length does not fit u64".to_string())?;
    let maximum_bytes = expected_byte_len.max(1);
    let options = StableInputOptions::new(maximum_bytes).map_err(|error| error.to_string())?;
    let observed = match StableInput::from_path(path, &options) {
        Ok(observed) => observed,
        Err(error)
            if error.kind()
                == forge_normalizer::stable_input::StableInputErrorKind::LimitExceeded =>
        {
            return Ok(false);
        }
        Err(error) => return Err(format!("read metadata fidelity report: {error}")),
    };
    Ok(observed.byte_len() == expected_byte_len
        && observed.binding().sha256_hex()
            == forge_normalizer::metadata_fidelity::sha256_hex(&expected))
}

fn stage_requested_metadata_fidelity_report(
    options: &MetadataInvocationOptions,
    report: Option<&MetadataFidelityReport>,
    overwrite: bool,
) -> Result<Option<StagedFileOutput>, String> {
    match (options.report.as_deref(), report) {
        (None, _) => Ok(None),
        (Some(_), None) => {
            Err("explicit metadata normalization did not produce fidelity evidence".into())
        }
        (Some(path), Some(report)) => {
            // A retry may reach this point after the report was published but
            // before the source transaction checkpoint was durable. Treat
            // the exact deterministic bytes as already published so the
            // no-clobber path remains idempotent. A different existing file
            // is still handed to AtomicOutput, which rejects it unless the
            // caller explicitly selected unchanged-destination replacement.
            if metadata_fidelity_report_is_published(options, Some(report))? {
                return Ok(None);
            }
            stage_metadata_fidelity_report(path, report, overwrite).map(Some)
        }
    }
}

fn publish_requested_metadata_fidelity_report(
    options: &MetadataInvocationOptions,
    output: Option<StagedFileOutput>,
) -> Result<(), String> {
    let Some(output) = output else {
        return Ok(());
    };
    let path = options
        .report
        .as_deref()
        .expect("a staged metadata report has a destination");
    output.commit().map_err(|error| {
        format!(
            "audio publication succeeded, but metadata fidelity report publication failed for {}: {error}",
            path.display()
        )
    })?;
    eprintln!("  metadata fidelity report: {}", path.display());
    Ok(())
}

/// Confirm the report bytes immediately before returning success from a
/// normalization or metadata transaction.  `stage_requested...` deliberately
/// skips an exact existing report for idempotent retries; this final bounded
/// read closes the resulting audio/report gap by refusing to report success if
/// that file was removed or replaced after the initial check.
fn verify_requested_metadata_fidelity_report(
    options: &MetadataInvocationOptions,
    report: Option<&MetadataFidelityReport>,
) -> Result<(), String> {
    let Some(path) = options.report.as_deref() else {
        return Ok(());
    };
    let report = report.ok_or(
        "explicit metadata normalization did not produce fidelity evidence for the requested report",
    )?;
    if metadata_fidelity_report_is_published(options, Some(report))? {
        return Ok(());
    }
    Err(format!(
        "audio publication succeeded, but metadata fidelity report was removed or changed before success: {}",
        path.display()
    ))
}

fn write_timeline(
    path: &Path,
    reports: &[TimelineReport],
    engine: AnalysisEngine,
    overwrite: bool,
) -> Result<(), String> {
    let format = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("ndjson")
        .to_ascii_lowercase();
    let write = |writer: &mut dyn Write| match format.as_str() {
        "json" => report::write_timeline_json_with_engine(writer, reports, engine),
        "csv" => report::write_timeline_csv_with_engine(writer, reports, engine),
        "ndjson" | "jsonl" => report::write_timeline_ndjson_with_engine(writer, reports, engine),
        _ => Err("--timeline path must end in .json, .ndjson, .jsonl, or .csv".into()),
    };
    if path.as_os_str() == "-" {
        let stdout = io::stdout();
        let mut output = stdout.lock();
        write(&mut output)
    } else {
        write_file_atomically(path, overwrite, |file| write(file))
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

fn bounded_utf8(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.to_owned();
    }
    let mut end = maximum;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

struct ProgressWriter {
    output: Box<dyn Write>,
    sequence: u64,
    job_id: Option<String>,
}

impl ProgressWriter {
    fn open(path: &Path, overwrite: bool) -> Result<Self, String> {
        Self::open_internal(path, overwrite, None)
    }

    fn open_generation(
        path: &Path,
        overwrite: bool,
        job_id: impl Into<String>,
    ) -> Result<Self, String> {
        Self::open_internal(path, overwrite, Some(job_id.into()))
    }

    fn open_internal(path: &Path, overwrite: bool, job_id: Option<String>) -> Result<Self, String> {
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
            Box::new(create_live_file_atomically(path, overwrite)?)
        };
        Ok(Self {
            output,
            sequence: 0,
            job_id,
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
        if let Some(job_id) = &self.job_id {
            // The v2 contract bounds paths and failure details in bytes.  Do
            // this at the writer boundary so an arbitrarily long filesystem
            // path or codec error cannot make the terminal failure event
            // invalid (and consequently hide the original operation error).
            let phase = match event {
                "job_started" | "asset_started" => "rendering",
                "asset_completed" | "asset_skipped" | "job_completed" => "committed",
                "asset_failed" | "job_failed" => "failed",
                _ => "unknown",
            };
            let mut record = BatchProgressEvent::new_v2(
                self.sequence,
                event,
                completed,
                total,
                job_id,
                1,
                phase,
            );
            if let Some((index, input, output)) = asset {
                record.index = Some(index);
                record.input = Some(bounded_utf8(
                    &input.to_string_lossy(),
                    MAX_BATCH_PROGRESS_PATH_BYTES,
                ));
                record.output = Some(bounded_utf8(
                    &output.to_string_lossy(),
                    MAX_BATCH_PROGRESS_PATH_BYTES,
                ));
            }
            record.error = error.map(|value| bounded_utf8(value, MAX_BATCH_FAILURE_ERROR_BYTES));
            record.validate()?;
            serde_json::to_writer(&mut self.output, &record)
                .map_err(|error| format!("write progress event: {error}"))?;
        } else {
            let mut record = BatchProgressEvent::new(self.sequence, event, completed, total);
            if let Some((index, input, output)) = asset {
                record.index = Some(index);
                record.input = Some(input.to_string_lossy().into_owned());
                record.output = Some(output.to_string_lossy().into_owned());
            }
            record.error = error.map(str::to_owned);
            serde_json::to_writer(&mut self.output, &record)
                .map_err(|error| format!("write progress event: {error}"))?;
        }
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

fn open_batch_progress(path: &Path, job: Option<&BatchJob>) -> Result<ProgressWriter, String> {
    if let Some(job_id) = job.and_then(BatchJob::job_id) {
        ProgressWriter::open_generation(path, true, job_id)
    } else {
        ProgressWriter::open(path, true)
    }
}

fn emit_album_completed(
    writer: &mut Option<ProgressWriter>,
    inputs: &[PathBuf],
    outputs: &[PathBuf],
) -> Result<(), String> {
    let Some(writer) = writer else {
        return Ok(());
    };
    for (index, (input, output)) in inputs.iter().zip(outputs).enumerate() {
        writer.emit(
            "asset_completed",
            index + 1,
            inputs.len(),
            Some((index, input, output)),
            None,
        )?;
    }
    writer.emit("job_completed", inputs.len(), inputs.len(), None, None)
}

fn emit_album_failed(
    writer: &mut Option<ProgressWriter>,
    inputs: &[PathBuf],
    outputs: &[PathBuf],
    failed_index: Option<usize>,
    error: &str,
) -> Result<(), String> {
    let Some(writer) = writer else {
        return Ok(());
    };
    if let Some(index) = failed_index {
        if let (Some(input), Some(output)) = (inputs.get(index), outputs.get(index)) {
            writer.emit(
                "asset_failed",
                0,
                inputs.len(),
                Some((index, input, output)),
                Some(error),
            )?;
        }
    }
    writer.emit("job_failed", 0, inputs.len(), None, Some(error))
}

fn append_auxiliary_failure(primary: &mut String, label: &str, result: Result<(), String>) {
    if let Err(error) = result {
        use std::fmt::Write as _;
        let _ = write!(primary, "; {label}: {error}");
    }
}

#[allow(clippy::too_many_arguments)]
fn finish_generation_failure(
    writer: &mut Option<ProgressWriter>,
    report_path: Option<&Path>,
    job: Option<&BatchJob>,
    failure_policy: BatchFailurePolicy,
    inputs: &[PathBuf],
    outputs: &[PathBuf],
    failed_index: Option<usize>,
    error: String,
) -> String {
    let progress_result = emit_album_failed(writer, inputs, outputs, failed_index, &error);
    let report_result =
        write_generation_failure_report(report_path, job, failure_policy, inputs, outputs, &error);
    let mut combined = error;
    append_auxiliary_failure(
        &mut combined,
        "emit terminal generation progress",
        progress_result,
    );
    append_auxiliary_failure(
        &mut combined,
        "write generation failure report",
        report_result,
    );
    combined
}

fn finish_batch_report_failure(
    writer: &mut Option<ProgressWriter>,
    report_path: Option<&Path>,
    report: &BatchFailureReport,
    total: usize,
    summary: String,
) -> String {
    let progress_result = if let Some(writer) = writer {
        writer.emit("job_failed", 0, total, None, Some(&summary))
    } else {
        Ok(())
    };
    let report_result = report_path.map_or(Ok(()), |path| write_batch_failure_report(path, report));
    let mut combined = summary;
    append_auxiliary_failure(
        &mut combined,
        "emit terminal batch progress",
        progress_result,
    );
    append_auxiliary_failure(&mut combined, "write batch failure report", report_result);
    combined
}

#[derive(Debug, PartialEq, Eq)]
enum BatchGenerationCheckpointOutcome {
    Persisted,
    CommittedNeedsCheckpoint(String),
}

fn classify_committed_checkpoint_result(
    was_complete: bool,
    is_complete: bool,
    result: Result<(), String>,
) -> Result<BatchGenerationCheckpointOutcome, String> {
    match result {
        Ok(()) => Ok(BatchGenerationCheckpointOutcome::Persisted),
        Err(error) if !was_complete && is_complete => Ok(
            BatchGenerationCheckpointOutcome::CommittedNeedsCheckpoint(error),
        ),
        Err(error) => Err(error),
    }
}

fn checkpoint_committed_generation(
    job: &mut BatchJob,
    status: &forge_normalizer::generation::GenerationStatus,
) -> Result<BatchGenerationCheckpointOutcome, String> {
    // `mark_generation_completed_with_evidence` performs every journal/job and
    // live-output check before mutating the in-memory document. Therefore the
    // false -> true transition on an error precisely identifies a state-save
    // failure after the audio generation was already proved committed.
    let was_complete = job.is_complete();
    let result = job.mark_generation_completed_with_evidence(status);
    classify_committed_checkpoint_result(was_complete, job.is_complete(), result)
}

fn committed_checkpoint_failure(
    checkpoint_error: String,
    progress_result: Result<(), String>,
) -> String {
    let mut combined = format!(
        "audio generation committed, but its batch checkpoint requires retry: {checkpoint_error}"
    );
    append_auxiliary_failure(
        &mut combined,
        "emit committed generation progress",
        progress_result,
    );
    combined
}

#[allow(clippy::too_many_arguments)]
fn finish_batch_generation_commit(
    commit_result: Result<BatchGenerationCheckpointOutcome, String>,
    writer: &mut Option<ProgressWriter>,
    report_path: Option<&Path>,
    job: Option<&BatchJob>,
    failure_policy: BatchFailurePolicy,
    inputs: &[PathBuf],
    outputs: &[PathBuf],
) -> Result<(), String> {
    match commit_result {
        Ok(BatchGenerationCheckpointOutcome::Persisted) => Ok(()),
        Ok(BatchGenerationCheckpointOutcome::CommittedNeedsCheckpoint(error)) => Err(
            committed_checkpoint_failure(error, emit_album_completed(writer, inputs, outputs)),
        ),
        Err(error) => Err(finish_generation_failure(
            writer,
            report_path,
            job,
            failure_policy,
            inputs,
            outputs,
            None,
            error,
        )),
    }
}

fn reconcile_batch_generation(
    job: &mut BatchJob,
    journal: &Path,
    reset_changed_outputs: bool,
) -> Result<BatchGenerationCheckpointOutcome, String> {
    match std::fs::symlink_metadata(journal) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if job.is_complete() {
                if reset_changed_outputs {
                    job.reset_completed_generation_for_rebuild()?;
                    eprintln!(
                        "completed batch generation journal is missing; rebuilding all outputs because --overwrite was supplied: {}",
                        journal.display()
                    );
                    return Ok(BatchGenerationCheckpointOutcome::Persisted);
                }
                return Err(format!(
                    "completed batch state is missing its generation journal: {}",
                    journal.display()
                ));
            }
            return Ok(BatchGenerationCheckpointOutcome::Persisted);
        }
        Err(error) => {
            return Err(format!(
                "inspect batch generation journal {}: {error}",
                journal.display()
            ));
        }
        Ok(_) => {}
    }
    let phase = GenerationTransaction::inspect_phase(journal)?;
    if !job.is_complete() {
        if phase == GenerationPhase::RolledBack {
            return Ok(BatchGenerationCheckpointOutcome::Persisted);
        }
        if reset_changed_outputs
            && phase == GenerationPhase::Committed
            && GenerationTransaction::inspect(journal).is_err()
        {
            // An explicitly authorized --overwrite rebuild may have reset the
            // whole v3 job after a terminal generation's outputs changed. The
            // new fully staged generation will replace this terminal journal.
            return Ok(BatchGenerationCheckpointOutcome::Persisted);
        }
    }
    let job_id = job
        .job_id()
        .ok_or_else(|| "batch generation recovery requires a v3 job identity".to_string())?
        .to_owned();
    let recovery = match GenerationTransaction::resume_with_fingerprint(journal, &job_id) {
        Ok(recovery) => recovery,
        Err(resume_error) if phase == GenerationPhase::Committed => {
            match GenerationTransaction::inspect(journal) {
                Ok(status) => {
                    if status.semantic_fingerprint() != job_id {
                        return Err(format!(
                            "{resume_error}; committed generation identity differs from batch job"
                        ));
                    }
                    // A committed journal can remain valid even when cleanup
                    // of its authenticated private backup reports an error.
                    // Cleanup is recoverable maintenance; checkpoint the
                    // already-committed audio instead of reporting job_failed.
                    eprintln!(
                        "committed generation requires private-backup cleanup: {resume_error}"
                    );
                    GenerationRecovery::Committed(status)
                }
                Err(inspect_error) if reset_changed_outputs && job.is_complete() => {
                    // A same-byte replacement has the expected digest but not
                    // the committed generation's file identity. `open_v3`
                    // cannot observe that distinction from its hash-only
                    // checkpoint, so honor the explicit rebuild authorization
                    // only after the complete batch is reset atomically.
                    job.reset_completed_generation_for_rebuild()?;
                    eprintln!(
                        "committed generation evidence changed; rebuilding all outputs because --overwrite was supplied: {resume_error}; inspection: {inspect_error}"
                    );
                    return Ok(BatchGenerationCheckpointOutcome::Persisted);
                }
                Err(inspect_error) => {
                    return Err(format!(
                        "{resume_error}; committed generation inspection failed: {inspect_error}"
                    ));
                }
            }
        }
        Err(error) => return Err(error),
    };
    let checkpoint = match recovery {
        GenerationRecovery::Ready(transaction) => {
            eprintln!(
                "resuming ready batch generation: {} members",
                transaction.member_count()
            );
            finalize_batch_generation_transaction(job, journal, &job_id, transaction)?
        }
        GenerationRecovery::Committed(status) => {
            if status.semantic_fingerprint() != job_id {
                return Err("committed generation identity differs from batch job".into());
            }
            checkpoint_committed_generation(job, &status)?
        }
        GenerationRecovery::RolledBack(status) => {
            if job.is_complete() {
                return Err(format!(
                    "batch state is complete but generation {} is rolled back",
                    status.generation_id()
                ));
            }
            BatchGenerationCheckpointOutcome::Persisted
        }
    };
    Ok(checkpoint)
}

fn commit_batch_generation(
    job: &mut BatchJob,
    journal: &Path,
    outputs: Vec<PreparedGenerationOutput>,
) -> Result<BatchGenerationCheckpointOutcome, String> {
    let job_id = job
        .job_id()
        .ok_or_else(|| "batch generation requires a v3 job identity".to_string())?
        .to_owned();
    let transaction = match std::fs::symlink_metadata(journal) {
        Ok(_) => GenerationTransaction::prepare_replacing_terminal(journal, &job_id, outputs)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            GenerationTransaction::prepare(journal, &job_id, outputs)?
        }
        Err(error) => {
            return Err(format!(
                "inspect generation journal {}: {error}",
                journal.display()
            ));
        }
    };
    finalize_batch_generation_transaction(job, journal, &job_id, transaction)
}

fn finalize_batch_generation_transaction(
    job: &mut BatchJob,
    journal: &Path,
    job_id: &str,
    transaction: GenerationTransaction,
) -> Result<BatchGenerationCheckpointOutcome, String> {
    let status = reconcile_generation_commit_result(journal, job_id, transaction.commit())?;
    checkpoint_committed_generation(job, &status)
}

fn reconcile_generation_commit_result(
    journal: &Path,
    job_id: &str,
    commit_result: Result<forge_normalizer::generation::GenerationStatus, String>,
) -> Result<forge_normalizer::generation::GenerationStatus, String> {
    match commit_result {
        Ok(status) => Ok(status),
        Err(commit_error) => {
            // Publication can have completed before the final committed-state
            // write or private-backup cleanup reports an error. Reconcile the
            // durable journal before classifying every asset as failed. This
            // also rolls a genuinely partial publication back under the same
            // journal lock and evidence checks.
            match GenerationTransaction::recover_with_fingerprint(journal, job_id) {
                Ok(status) if status.phase() == GenerationPhase::Committed => {
                    eprintln!(
                        "generation commit finalization recovered after an error: {commit_error}"
                    );
                    Ok(status)
                }
                Ok(status) => Err(format!(
                    "{commit_error}; generation recovery reached {}",
                    status.phase()
                )),
                Err(recovery_error) => match GenerationTransaction::inspect(journal) {
                    Ok(status) if status.phase() == GenerationPhase::Committed => {
                        // The committed journal and every live destination are
                        // valid. Cleanup is recoverable maintenance, not a
                        // failed audio generation; preserve the warning while
                        // allowing the v3 checkpoint to reflect reality.
                        eprintln!(
                            "generation is committed but private cleanup still requires recovery: {commit_error}; recovery attempt: {recovery_error}"
                        );
                        Ok(status)
                    }
                    Ok(status) => Err(format!(
                        "{commit_error}; generation recovery failed: {recovery_error}; journal remains {}",
                        status.phase()
                    )),
                    Err(inspect_error) => Err(format!(
                        "{commit_error}; generation recovery failed: {recovery_error}; journal inspection failed: {inspect_error}"
                    )),
                },
            }
        }
    }
}

fn emit_completed_resume_progress(
    path: Option<&Path>,
    job: &BatchJob,
    inputs: &[PathBuf],
    outputs: &[PathBuf],
) -> Result<(), String> {
    let Some(path) = path else {
        return Ok(());
    };
    let job_id = job
        .job_id()
        .ok_or_else(|| "generation progress requires a v3 job identity".to_string())?;
    let mut writer = ProgressWriter::open_generation(path, true, job_id)?;
    writer.emit(
        "job_started",
        job.completed_count(),
        job.asset_count(),
        None,
        None,
    )?;
    for (index, (input, output)) in inputs.iter().zip(outputs).enumerate() {
        writer.emit(
            "asset_skipped",
            job.completed_count(),
            job.asset_count(),
            Some((index, input, output)),
            None,
        )?;
    }
    writer.emit(
        "job_completed",
        job.completed_count(),
        job.asset_count(),
        None,
        None,
    )
}

fn batch_operation_descriptor(
    cli: &cli::Cli,
    plan: &Plan,
    formats: &[OutputFormat],
    metadata_options: &MetadataInvocationOptions,
    analysis_engine: AnalysisEngine,
    audio_track: Option<u32>,
) -> serde_json::Value {
    let descriptor = serde_json::json!({
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
        "album": cli.album,
        "analysis_engine": analysis_engine.id(),
        "audio_track": audio_track,
        "channel_layout": cli.channel_layout,
        "dual_mono": cli.dual_mono,
        "formats": formats
            .iter()
            .map(|format| operation_format_id(*format))
            .collect::<Vec<_>>(),
    });
    metadata_operation_descriptor(descriptor, metadata_options)
}

fn metadata_operation_descriptor(
    mut descriptor: serde_json::Value,
    metadata_options: &MetadataInvocationOptions,
) -> serde_json::Value {
    if !metadata_options.active_for_normalization() {
        // Keep the historical operation byte-for-byte compatible when the
        // explicit fidelity contract is not selected. Existing batch/watch
        // state files compare this object exactly on resume.
        return descriptor;
    }
    let object = descriptor
        .as_object_mut()
        .expect("operation descriptors are JSON objects");
    object.insert(
        "metadata_policy".into(),
        serde_json::to_value(&metadata_options.policy)
            .expect("metadata policy serialization cannot fail"),
    );
    object.insert(
        "metadata_registry_revision".into(),
        forge_normalizer::metadata_fidelity::METADATA_REGISTRY_REVISION.into(),
    );
    object.insert(
        "metadata_timing_revision".into(),
        forge_normalizer::metadata_fidelity::METADATA_TIMING_REVISION.into(),
    );
    object.insert(
        "metadata_fidelity_schema_version".into(),
        forge_normalizer::metadata_fidelity::METADATA_FIDELITY_SCHEMA_VERSION.into(),
    );
    descriptor
}

fn validate_control_paths(
    cli: &cli::Cli,
    batch_options: &BatchOptions,
    outputs: &[PathBuf],
) -> Result<(), String> {
    let mut controls = Vec::new();
    if let Some(path) = &batch_options.job_state {
        controls.push(("--job-state", comparison_path(path)?));
        controls.push((
            "--job-state generation journal",
            comparison_path(&generation_state_path(path)?)?,
        ));
    }
    if let Some(path) = &batch_options.progress {
        if path != Path::new("-") {
            controls.push(("--progress", comparison_path(path)?));
        }
    }
    if let Some(path) = &batch_options.failure_report {
        if path == Path::new("-") {
            return Err("--failure-report requires a file path".into());
        }
        controls.push(("--failure-report", comparison_path(path)?));
    }
    let audio_paths = cli
        .inputs
        .iter()
        .chain(outputs)
        .map(|path| comparison_path(path))
        .collect::<Result<Vec<_>, _>>()?;
    for (label, control) in &controls {
        for path in &audio_paths {
            if path != control {
                continue;
            }
            return Err(format!(
                "{label} must not overwrite an audio input or output: {}",
                control.display()
            ));
        }
    }
    for left in 0..controls.len() {
        for right in left + 1..controls.len() {
            if controls[left].1 == controls[right].1 {
                return Err(format!(
                    "{} and {} require different paths",
                    controls[left].0, controls[right].0
                ));
            }
        }
    }
    Ok(())
}

fn validate_metadata_control_paths(
    cli: &cli::Cli,
    options: &MetadataInvocationOptions,
) -> Result<(), String> {
    let protected = cli
        .inputs
        .iter()
        .enumerate()
        .map(|(index, path)| ProtectedPath::new(format!("metadata input {index}"), path))
        .collect::<Vec<_>>();
    let mut outputs = Vec::new();
    if let Some(path) = &options.job_state {
        outputs.push(PlannedOutput::new("metadata transaction state", path, true));
        outputs.push(PlannedOutput::new(
            "metadata transaction state lock",
            state_lock_path(path)?,
            true,
        ));
    }
    if let Some(path) = &options.report {
        // A committed metadata job may have published this exact report just
        // before the prior process stopped. Let recovery inspect it; the
        // deterministic byte comparison below still rejects a different file
        // unless the caller explicitly requested overwrite.
        outputs.push(PlannedOutput::new(
            "metadata fidelity report",
            path,
            cli.overwrite || options.job_state.is_some(),
        ));
    }
    OutputPlan::new(protected, outputs).map(drop)
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
    let audio_paths = cli
        .inputs
        .iter()
        .chain(outputs)
        .map(|path| comparison_path(path))
        .collect::<Result<Vec<_>, _>>()?;
    let database = comparison_path(database)?;
    for path in &audio_paths {
        if path == &database {
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
        for path in &audio_paths {
            if path == &report {
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
    records: &mut Vec<CatalogueRecordV2>,
    asset: CatalogueAsset<'_>,
    descriptor: Option<&InputDescriptor>,
    descriptor_options: InputDescriptorOptions,
    plan: &Plan,
    renderer: &str,
) -> Result<(), String> {
    let Some(catalogue) = catalogue else {
        return Ok(());
    };
    let record = if let Some(descriptor) = descriptor {
        catalogue.record_bound_v3(asset, descriptor, plan, renderer)?
    } else {
        catalogue.record_bound_path_v3(asset, descriptor_options, plan, renderer)?
    };
    records.push(record);
    Ok(())
}

fn catalogue_descriptor_options(
    cli: &cli::Cli,
    channel_roles: Option<&[ChannelRole]>,
    audio_track: Option<u32>,
) -> InputDescriptorOptions {
    let mut options = InputDescriptorOptions::default()
        .with_time_range(cli.start_seconds.unwrap_or(0.0), cli.duration_seconds);
    if let Some(index) = audio_track {
        options = options.with_track(AudioTrackSelection::Index(index));
    }
    if let Some(roles) = channel_roles {
        options = options.with_channel_roles(roles.to_vec());
    }
    options
}

fn catalogue_analysis_renderer(engine: AnalysisEngine) -> String {
    format!("forge-analysis:{}", engine.id())
}

const fn catalogue_output_renderer(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Wav => "forge-native:wav",
        OutputFormat::Flac => "forge-native:flac",
        OutputFormat::Mp3 => "lame:mp3",
        OutputFormat::Opus => "libopus:ogg",
        OutputFormat::M4a => "ffmpeg:aac:ipod",
        OutputFormat::Alac => "ffmpeg:alac:ipod",
        OutputFormat::Vorbis => "ffmpeg:libvorbis:ogg",
    }
}

fn write_catalogue_report(
    catalogue: Option<&Catalogue>,
    report: Option<&Path>,
    records: Vec<CatalogueRecordV2>,
    overwrite: bool,
) -> Result<(), String> {
    match (catalogue, report) {
        (Some(catalogue), Some(report)) => {
            catalogue.write_report_v3_with_overwrite(report, records, overwrite)
        }
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
    let absolute = std::path::absolute(path)
        .map_err(|error| format!("resolve {}: {error}", path.display()))?;
    match std::fs::canonicalize(&absolute) {
        Ok(resolved) => return Ok(comparison_path_platform_key(resolved)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!("canonicalize {}: {error}", path.display()));
        }
    }
    let components = absolute.components().collect::<Vec<_>>();
    let mut resolved = PathBuf::new();
    let mut missing_at = None;
    for (index, component) in components.iter().enumerate() {
        match component {
            std::path::Component::Prefix(prefix) => resolved.push(prefix.as_os_str()),
            std::path::Component::RootDir => {
                resolved.push(Path::new(std::path::MAIN_SEPARATOR_STR));
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir | std::path::Component::Normal(_) => {
                let candidate = resolved.join(component.as_os_str());
                match std::fs::canonicalize(&candidate) {
                    Ok(canonical) => resolved = canonical,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        missing_at = Some(index);
                        break;
                    }
                    Err(error) => {
                        return Err(format!(
                            "canonicalize path prefix {}: {error}",
                            candidate.display()
                        ));
                    }
                }
            }
        }
    }
    if let Some(index) = missing_at {
        for component in &components[index..] {
            match component {
                std::path::Component::Prefix(prefix) => resolved.push(prefix.as_os_str()),
                std::path::Component::RootDir => {
                    resolved.push(Path::new(std::path::MAIN_SEPARATOR_STR));
                }
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    let _ = resolved.pop();
                }
                std::path::Component::Normal(value) => resolved.push(value),
            }
        }
    }
    Ok(comparison_path_platform_key(resolved))
}

fn comparison_path_platform_key(resolved: PathBuf) -> PathBuf {
    #[cfg(windows)]
    let resolved = {
        // Generation path keys use the same conservative ASCII folding for
        // Windows path aliases. A lossy collision only rejects an ambiguous
        // control path; it never authorizes an overwrite.
        PathBuf::from(resolved.to_string_lossy().to_ascii_lowercase())
    };
    resolved
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

#[allow(clippy::too_many_arguments)]
fn analyze_range_cached(
    cache: Option<&AnalysisCache>,
    input: &Path,
    channel_roles: Option<&[ChannelRole]>,
    start_seconds: f64,
    duration_seconds: Option<f64>,
    timeline_interval_ms: Option<f64>,
    engine: AnalysisEngine,
    audio_track: Option<u32>,
) -> Result<(InputDescriptor, normalize::TimedAnalysis), String> {
    let stable_options = StableInputOptions::new(u64::MAX).map_err(|error| error.to_string())?;
    let stable =
        StableInput::from_path(input, &stable_options).map_err(|error| error.to_string())?;
    let mut descriptor_options =
        InputDescriptorOptions::default().with_time_range(start_seconds, duration_seconds);
    if let Some(index) = audio_track {
        descriptor_options = descriptor_options.with_track(AudioTrackSelection::Index(index));
    }
    if let Some(roles) = channel_roles {
        descriptor_options = descriptor_options.with_channel_roles(roles.to_vec());
    }
    let descriptor = InputDescriptor::probe(stable, descriptor_options)?;
    if let Some(cache) = cache {
        let timed = cache
            .analyze_descriptor_range_with_engine(&descriptor, timeline_interval_ms, engine)
            .map(|cached| observe_cache(input, cached))?;
        return Ok((descriptor, timed));
    }
    let timed = normalize::analyze_input_descriptor_range_with_engine(
        &descriptor,
        timeline_interval_ms,
        engine,
    )?;
    Ok((descriptor, timed))
}

struct CachedPlanAnalysis {
    input: StableInput,
    descriptor: InputDescriptor,
    analysis: BoundAnalysis,
}

struct CacheObservation {
    disposition: CacheDisposition,
    warning: Option<String>,
}

fn analyze_many_for_plan_uncached(
    inputs: &[PathBuf],
    channel_roles: Option<&[ChannelRole]>,
    plan: &Plan,
    audio_track: Option<u32>,
) -> Result<Vec<CachedPlanAnalysis>, String> {
    inputs
        .par_iter()
        .map(|input| analyze_for_plan_descriptor(input, channel_roles, plan, audio_track))
        .collect::<Vec<_>>()
        .into_iter()
        .collect()
}

fn analyze_many_for_plan_cached(
    cache: &AnalysisCache,
    inputs: &[PathBuf],
    channel_roles: Option<&[ChannelRole]>,
    plan: &Plan,
    audio_track: Option<u32>,
) -> Result<Vec<CachedPlanAnalysis>, String> {
    let cached = inputs
        .par_iter()
        .map(|path| {
            let options = StableInputOptions::new(u64::MAX).map_err(|error| error.to_string())?;
            let input =
                StableInput::from_path(path, &options).map_err(|error| error.to_string())?;
            let descriptor = input_descriptor_for_plan(input.clone(), channel_roles, audio_track)?;
            let cached = cache.analyze_descriptor_for_plan(&descriptor, plan)?;
            Ok((input, descriptor, cached))
        })
        .collect::<Vec<_>>();
    let mut analyses = Vec::with_capacity(inputs.len());
    for (input, result) in inputs.iter().zip(cached) {
        match result {
            Ok((stable, descriptor, cached)) => analyses.push(CachedPlanAnalysis {
                input: stable,
                descriptor,
                analysis: observe_cache(input, cached),
            }),
            Err(message) => return Err(message),
        }
    }
    Ok(analyses)
}

fn analyze_for_plan_cached(
    cache: &AnalysisCache,
    input: &Path,
    channel_roles: Option<&[ChannelRole]>,
    plan: &Plan,
    audio_track: Option<u32>,
) -> Result<CachedPlanAnalysis, String> {
    let (analysis, observation) =
        analyze_for_plan_cached_unobserved(cache, input, channel_roles, plan, audio_track)?;
    observe_cache_parts(input, observation.disposition, observation.warning);
    Ok(analysis)
}

fn analyze_for_plan_cached_unobserved(
    cache: &AnalysisCache,
    input: &Path,
    channel_roles: Option<&[ChannelRole]>,
    plan: &Plan,
    audio_track: Option<u32>,
) -> Result<(CachedPlanAnalysis, CacheObservation), String> {
    let options = StableInputOptions::new(u64::MAX).map_err(|error| error.to_string())?;
    let stable = StableInput::from_path(input, &options).map_err(|error| error.to_string())?;
    let descriptor = input_descriptor_for_plan(stable.clone(), channel_roles, audio_track)?;
    let Cached {
        value: analysis,
        disposition,
        warning,
    } = cache.analyze_descriptor_for_plan(&descriptor, plan)?;
    Ok((
        CachedPlanAnalysis {
            input: stable,
            descriptor,
            analysis,
        },
        CacheObservation {
            disposition,
            warning,
        },
    ))
}

fn analyze_for_plan_descriptor(
    input: &Path,
    channel_roles: Option<&[ChannelRole]>,
    plan: &Plan,
    audio_track: Option<u32>,
) -> Result<CachedPlanAnalysis, String> {
    let descriptor = input_descriptor_for_path(input, channel_roles, audio_track)?;
    let stable = descriptor.stable_input().clone();
    let analysis = normalize::analyze_input_descriptor_for_plan(&descriptor, plan)
        .map_err(|error| error.to_string())?;
    Ok(CachedPlanAnalysis {
        input: stable,
        descriptor,
        analysis,
    })
}

fn input_descriptor_for_path(
    input: &Path,
    channel_roles: Option<&[ChannelRole]>,
    audio_track: Option<u32>,
) -> Result<InputDescriptor, String> {
    let options = StableInputOptions::new(u64::MAX).map_err(|error| error.to_string())?;
    let stable = StableInput::from_path(input, &options).map_err(|error| error.to_string())?;
    input_descriptor_for_plan(stable, channel_roles, audio_track)
}

fn input_descriptor_for_plan(
    input: StableInput,
    channel_roles: Option<&[ChannelRole]>,
    audio_track: Option<u32>,
) -> Result<InputDescriptor, String> {
    let mut options = InputDescriptorOptions::default();
    if let Some(roles) = channel_roles {
        options = options.with_channel_roles(roles.to_vec());
    }
    if let Some(index) = audio_track {
        options = options.with_track(AudioTrackSelection::Index(index));
    }
    InputDescriptor::probe(input, options)
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
    observe_cache_parts(input, cached.disposition, cached.warning);
    cached.value
}

fn observe_cache_parts(input: &Path, disposition: CacheDisposition, warning: Option<String>) {
    let action = match disposition {
        CacheDisposition::Hit => "hit",
        CacheDisposition::Stored => "miss; stored",
        CacheDisposition::Repaired => "invalid; repaired",
        CacheDisposition::ReadOnlyMiss => "miss; read-only",
        CacheDisposition::ReadOnlyInvalid => "invalid; read-only",
        CacheDisposition::TooLarge => "miss; entry too large to store",
    };
    eprintln!("analysis cache {action}: {}", input.display());
    if let Some(warning) = warning {
        eprintln!("analysis cache warning: {warning}");
    }
}

fn write_loudness_tags(
    cli: &cli::Cli,
    channel_roles: Option<&[forge_normalizer::wav::ChannelRole]>,
    cache: Option<&AnalysisCache>,
    metadata_options: &MetadataInvocationOptions,
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
    let isobmff_album = cli.album.then(|| {
        (
            normalize::album_lufs(&analyses),
            analyses
                .iter()
                .map(|analysis| analysis.sample_peak)
                .fold(0.0_f32, f32::max),
            analyses
                .iter()
                .map(|analysis| analysis.true_peak)
                .fold(0.0_f32, f32::max),
        )
    });
    if let Some(state) = metadata_options.job_state.as_deref() {
        let input = cli
            .inputs
            .first()
            .ok_or("metadata transaction requires one input")?;
        let analysis = analyses
            .first()
            .ok_or("metadata transaction requires one analysis")?;
        print_analysis(input, analysis, None);
        return write_loudness_tags_transaction(
            cli,
            input,
            analysis,
            album,
            isobmff_album,
            channel_roles,
            state,
            metadata_options,
        );
    }
    for (input, analysis) in cli.inputs.iter().zip(&analyses) {
        print_analysis(input, analysis, None);
        let scheme = forge_normalizer::metadata::loudness_metadata_scheme(input)?;
        let is_isobmff = forge_normalizer::metadata::is_isobmff_file(input)?;
        if cli.dry_run {
            eprintln!("  would write {} metadata", scheme.label());
            if is_isobmff {
                eprintln!(
                    "  would also write ISO-BMFF ludt/tlou{} metadata",
                    if cli.album { "/alou" } else { "" }
                );
            }
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
        if is_isobmff && !cli.dry_run {
            if forge_normalizer::metadata::write_isobmff_loudness_metadata(
                input,
                analysis,
                isobmff_album,
            )? {
                eprintln!(
                    "  wrote and verified ISO-BMFF ludt/tlou{} metadata",
                    if cli.album { "/alou" } else { "" }
                );
            } else {
                eprintln!(
                    "  skipped ISO-BMFF loudness boxes because silence has no finite encoded value"
                );
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_loudness_tags_transaction(
    cli: &cli::Cli,
    input: &Path,
    analysis: &Analysis,
    album: Option<(f64, f32)>,
    isobmff_album: Option<(f64, f32, f32)>,
    channel_roles: Option<&[ChannelRole]>,
    state: &Path,
    metadata_options: &MetadataInvocationOptions,
) -> Result<(), String> {
    let is_isobmff = forge_normalizer::metadata::is_isobmff_file(input)?;
    let operation = serde_json::json!({
        "schema": "forge-metadata-loudness-operation-v1",
        "analysis": analysis_operation_evidence(analysis),
        "album": album.map(|(lufs, peak)| serde_json::json!({
            "lufs_bits": format!("{:016x}", lufs.to_bits()),
            "true_peak_bits": format!("{:08x}", peak.to_bits()),
        })),
        "isobmff_album": isobmff_album.map(|(lufs, sample_peak, true_peak)| serde_json::json!({
            "lufs_bits": format!("{:016x}", lufs.to_bits()),
            "sample_peak_bits": format!("{:08x}", sample_peak.to_bits()),
            "true_peak_bits": format!("{:08x}", true_peak.to_bits()),
        })),
        "sound_check": cli.sound_check,
        "isobmff": is_isobmff,
        "metadata_policy": metadata_options.policy,
        "metadata_registry_revision": forge_normalizer::metadata_fidelity::METADATA_REGISTRY_REVISION,
        "metadata_timing_revision": forge_normalizer::metadata_fidelity::METADATA_TIMING_REVISION,
        "metadata_fidelity_schema_version": forge_normalizer::metadata_fidelity::METADATA_FIDELITY_SCHEMA_VERSION,
    });
    let request = MetadataTransactionRequest::new(input, state, operation)
        .with_policy(metadata_options.policy.policy());
    let resume = MetadataTransaction::resume(request)?;
    let receipt = match resume {
        MetadataResume::Prepared(transaction) => {
            let policy = metadata_options.policy.clone();
            let explicit_fidelity = metadata_options.active_for_normalization();
            let fidelity_report = RefCell::new(None);
            let ready = transaction.stage(
                |stage| {
                    let expected_sound_check = cli
                        .sound_check
                        .then(|| {
                            forge_normalizer::metadata::SoundCheck::from_r128(
                                analysis.lufs,
                                analysis.sample_peak,
                            )
                        })
                        .transpose()?;
                    let (scheme, isobmff_written) = if explicit_fidelity {
                        let result = forge_normalizer::write_loudness_metadata_with_fidelity(
                            stage,
                            analysis,
                            album,
                            isobmff_album,
                            expected_sound_check.as_ref(),
                            &policy,
                        )?;
                        *fidelity_report.borrow_mut() = Some(result.report().clone());
                        (result.scheme(), result.isobmff_written())
                    } else {
                        let scheme = forge_normalizer::metadata::write_loudness_metadata(
                            stage,
                            analysis.lufs,
                            analysis.true_peak,
                            album,
                        )?;
                        if let Some(expected) = expected_sound_check.as_ref() {
                            forge_normalizer::metadata::write_sound_check(stage, expected)?;
                        }
                        let isobmff_written = is_isobmff
                            && forge_normalizer::metadata::write_isobmff_loudness_metadata(
                                stage,
                                analysis,
                                isobmff_album,
                            )?;
                        (scheme, isobmff_written)
                    };
                    let sound_check = expected_sound_check.as_ref().map(|value| {
                        serde_json::json!({
                            "engineering_gain_db_bits": format!(
                                "{:016x}",
                                value.engineering_gain_db().to_bits()
                            ),
                            "engineering_sample_peak_bits": format!(
                                "{:016x}",
                                value.engineering_sample_peak().to_bits()
                            ),
                        })
                    });
                    Ok(serde_json::json!({
                        "loudness_scheme": scheme.label(),
                        "sound_check": sound_check,
                        "isobmff_loudness_written": isobmff_written,
                    }))
                },
                |stage| {
                    let round_trip = normalize::analyze_file_with_roles(stage, channel_roles)?;
                    require_same_audio_analysis(analysis, &round_trip)?;
                    let fidelity = if explicit_fidelity {
                        let report = fidelity_report.borrow_mut().take().ok_or(
                            "metadata transaction is missing its loudness fidelity report",
                        )?;
                        report
                            .require_publication()
                            .map_err(|error| error.to_string())?;
                        Some(report)
                    } else {
                        None
                    };
                    Ok(serde_json::json!({
                        "schema": "forge-metadata-loudness-verification-v1",
                        "audio_round_trip": analysis_operation_evidence(&round_trip),
                        "metadata_fidelity_report": fidelity,
                    }))
                },
            )?;
            let fidelity =
                validated_transaction_fidelity(ready.verification(), analysis, metadata_options)?;
            let staged_report = stage_requested_metadata_fidelity_report(
                metadata_options,
                fidelity.as_ref(),
                cli.overwrite,
            )?;
            let receipt = ready.commit()?;
            publish_requested_metadata_fidelity_report(metadata_options, staged_report)?;
            verify_requested_metadata_fidelity_report(metadata_options, fidelity.as_ref())?;
            receipt
        }
        MetadataResume::Ready(ready) => {
            let fidelity =
                validated_transaction_fidelity(ready.verification(), analysis, metadata_options)?;
            let staged_report = stage_requested_metadata_fidelity_report(
                metadata_options,
                fidelity.as_ref(),
                cli.overwrite,
            )?;
            let receipt = ready.commit()?;
            publish_requested_metadata_fidelity_report(metadata_options, staged_report)?;
            verify_requested_metadata_fidelity_report(metadata_options, fidelity.as_ref())?;
            receipt
        }
        MetadataResume::Committed(receipt) => {
            let fidelity =
                validated_transaction_fidelity(receipt.verification(), analysis, metadata_options)?;
            let report_was_staged = stage_requested_metadata_fidelity_report(
                metadata_options,
                fidelity.as_ref(),
                cli.overwrite,
            )?;
            if report_was_staged.is_some() {
                publish_requested_metadata_fidelity_report(metadata_options, report_was_staged)?;
            } else if let Some(path) = metadata_options.report.as_deref() {
                // `stage_requested_metadata_fidelity_report` returns None
                // for an exact existing report (and only for that case when
                // a report path was requested).
                eprintln!("  metadata fidelity report: {}", path.display());
            }
            verify_requested_metadata_fidelity_report(metadata_options, fidelity.as_ref())?;
            receipt
        }
        _ => return Err("unsupported metadata transaction resume state".into()),
    };
    eprintln!(
        "  metadata transaction committed: {} (output sha256 {})",
        receipt.job_id(),
        receipt.output_sha256()
    );
    Ok(())
}

fn analysis_operation_evidence(analysis: &Analysis) -> serde_json::Value {
    serde_json::json!({
        "sample_rate_hz": analysis.sample_rate,
        "channels": analysis.channels,
        "channel_roles": analysis
            .channel_roles
            .iter()
            .map(|role| format!("{role:?}"))
            .collect::<Vec<_>>(),
        "frames": analysis.frames,
        "pcm_kind": format!("{:?}", analysis.kind),
        "integrated_lufs_bits": format!("{:016x}", analysis.lufs.to_bits()),
        "max_momentary_lufs_bits": format!("{:016x}", analysis.max_momentary_lufs.to_bits()),
        "max_short_term_lufs_bits": format!("{:016x}", analysis.max_short_term_lufs.to_bits()),
        "loudness_range_lu_bits": format!("{:016x}", analysis.loudness_range_lu.to_bits()),
        "rms_db_bits": format!("{:016x}", analysis.rms_db.to_bits()),
        "sample_peak_bits": format!("{:08x}", analysis.sample_peak.to_bits()),
        "true_peak_bits": format!("{:08x}", analysis.true_peak.to_bits()),
        "loudness_block_count": analysis.loudness_blocks.len(),
        "loudness_blocks_sha256": loudness_blocks_sha256(&analysis.loudness_blocks),
    })
}

fn loudness_blocks_sha256(blocks: &[f64]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"forge-loudness-block-bits-v1\0");
    hasher.update((blocks.len() as u64).to_be_bytes());
    for value in blocks {
        hasher.update(value.to_bits().to_be_bytes());
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn require_same_audio_analysis(expected: &Analysis, actual: &Analysis) -> Result<(), String> {
    if analysis_operation_evidence(expected) != analysis_operation_evidence(actual) {
        return Err(
            "metadata-only transaction changed decoded audio or its exact measurement evidence"
                .into(),
        );
    }
    Ok(())
}

fn fidelity_report_from_verification(
    verification: &serde_json::Map<String, serde_json::Value>,
) -> Result<Option<MetadataFidelityReport>, String> {
    let Some(value) = verification
        .get("metadata_fidelity_report")
        .filter(|value| !value.is_null())
    else {
        return Ok(None);
    };
    let report: MetadataFidelityReport = serde_json::from_value(value.clone())
        .map_err(|error| format!("decode metadata fidelity transaction evidence: {error}"))?;
    report.validate().map_err(|error| error.to_string())?;
    Ok(Some(report))
}

fn validated_transaction_fidelity(
    verification: Option<&serde_json::Value>,
    expected_analysis: &Analysis,
    options: &MetadataInvocationOptions,
) -> Result<Option<MetadataFidelityReport>, String> {
    let verification = verification
        .and_then(serde_json::Value::as_object)
        .ok_or("metadata transaction is missing typed verification evidence")?;
    if verification
        .get("schema")
        .and_then(serde_json::Value::as_str)
        != Some("forge-metadata-loudness-verification-v1")
    {
        return Err("metadata transaction verification schema is invalid".into());
    }
    let round_trip = verification
        .get("audio_round_trip")
        .ok_or("metadata transaction is missing audio round-trip evidence")?;
    if round_trip != &analysis_operation_evidence(expected_analysis) {
        return Err(
            "metadata transaction audio round-trip evidence does not match the current decoded analysis"
                .into(),
        );
    }
    let report = fidelity_report_from_verification(verification)?;
    if options.active_for_normalization() {
        let report = report
            .as_ref()
            .ok_or("metadata transaction is missing its fidelity report evidence")?;
        if report.policy() != &options.policy {
            return Err("metadata transaction fidelity policy does not match this request".into());
        }
        if report.evidence().registry_revision()
            != forge_normalizer::metadata_fidelity::METADATA_REGISTRY_REVISION
            || report.evidence().timing_revision()
                != forge_normalizer::metadata_fidelity::METADATA_TIMING_REVISION
        {
            return Err(
                "metadata transaction fidelity revisions do not match this executable".into(),
            );
        }
        report
            .require_publication()
            .map_err(|error| error.to_string())?;
    }
    Ok(report)
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
            let root = std::fs::canonicalize(input)
                .map_err(|error| format!("canonicalize {}: {error}", input.display()))?;
            for path in discover_audio_files(&root, true)? {
                relative.push(path.strip_prefix(&root).map(PathBuf::from).map_err(|_| {
                    format!("discovered input escaped its root: {}", path.display())
                })?);
                expanded.push(path);
            }
        } else {
            return Err(format!("input does not exist: {}", input.display()));
        }
    }
    if expanded.is_empty() {
        return Err("no supported audio files found".into());
    }
    Ok((expanded, relative))
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

fn build_output_plan(
    cli: &cli::Cli,
    batch_options: &BatchOptions,
    catalogue_options: &CatalogueOptions,
    metadata_options: &MetadataInvocationOptions,
    anomaly_audits: &[PathBuf],
    ebu_qc_xml: Option<&Path>,
    audio_outputs: &[PathBuf],
) -> Result<OutputPlan, String> {
    let mut protected = Vec::new();
    for (index, path) in cli.inputs.iter().enumerate() {
        if path != Path::new("-") {
            protected.push(ProtectedPath::new(format!("input {index}"), path));
        }
    }
    for (label, path) in [
        ("configuration", cli.config.as_deref()),
        ("dialogue ranges", cli.dialogue_ranges.as_deref()),
        ("dialogue stem", cli.dialogue_stem.as_deref()),
        ("codec metadata", cli.codec_metadata.as_deref()),
        ("codec reference", cli.codec_reference.as_deref()),
        ("codec prober", cli.codec_prober.as_deref()),
        ("ADM presentation map", cli.adm_presentations.as_deref()),
        ("ADM renderer", cli.adm_renderer.as_deref()),
    ] {
        if let Some(path) = path {
            protected.push(ProtectedPath::new(label, path));
        }
    }
    protected.extend(
        anomaly_audits
            .iter()
            .enumerate()
            .map(|(index, path)| ProtectedPath::new(format!("anomaly audit {index}"), path)),
    );

    let mut outputs = Vec::new();
    if !cli.analyze_only && !cli.gain_only && !cli.write_tags {
        outputs.extend(audio_outputs.iter().enumerate().map(|(index, path)| {
            // A resumable job may legitimately begin with hash-verified
            // completed outputs. BatchJob validates those hashes before the
            // pending subset applies the caller's actual overwrite policy.
            PlannedOutput::new(
                format!("audio output {index}"),
                path,
                cli.overwrite || batch_options.job_state.is_some(),
            )
        }));
    }
    if cli.analyze_only {
        for (label, path) in [
            ("CSV report", cli.csv.as_deref()),
            ("timeline report", cli.timeline.as_deref()),
            ("delivery manifest", cli.manifest.as_deref()),
            (
                "dialogue detection report",
                cli.dialogue_detection_report.as_deref(),
            ),
            ("ADM profile report", cli.adm_profile_report.as_deref()),
            ("EBU QC XML report", ebu_qc_xml),
            ("ADM rendered output", cli.adm_rendered_output.as_deref()),
        ] {
            if let Some(path) = path.filter(|path| *path != Path::new("-")) {
                outputs.push(PlannedOutput::new(label, path, cli.overwrite));
            }
        }
    }
    if let Some(path) = &cli.difference_report {
        outputs.push(PlannedOutput::new(
            "normalization difference report",
            path,
            cli.overwrite,
        ));
    }
    if let Some(path) = &batch_options.job_state {
        outputs.push(PlannedOutput::new("batch state", path, true));
        outputs.push(PlannedOutput::new(
            "batch state lock",
            state_lock_path(path)?,
            true,
        ));
        let generation = generation_state_path(path)?;
        outputs.push(PlannedOutput::new(
            "batch generation journal",
            &generation,
            true,
        ));
        outputs.push(PlannedOutput::new(
            "batch generation lock",
            state_lock_path(&generation)?,
            true,
        ));
    }
    if let Some(path) = batch_options
        .progress
        .as_deref()
        .filter(|path| *path != Path::new("-"))
    {
        outputs.push(PlannedOutput::new("batch progress report", path, true));
    }
    if let Some(path) = &batch_options.failure_report {
        outputs.push(PlannedOutput::new("batch failure report", path, true));
    }
    if let Some(path) = &catalogue_options.database {
        outputs.push(PlannedOutput::new("catalogue database", path, true));
    }
    if let Some(path) = &catalogue_options.report {
        outputs.push(PlannedOutput::new("catalogue report", path, cli.overwrite));
    }
    if let Some(path) = &metadata_options.report {
        outputs.push(PlannedOutput::new(
            "metadata fidelity report",
            path,
            cli.overwrite,
        ));
    }
    OutputPlan::new(protected, outputs)
}

fn state_lock_path(state: &Path) -> Result<PathBuf, String> {
    let name = state.file_name().ok_or_else(|| {
        format!(
            "state path has no final component for locking: {}",
            state.display()
        )
    })?;
    let mut lock_name = name.to_os_string();
    lock_name.push(".lock");
    Ok(state.with_file_name(lock_name))
}

fn generation_state_path(batch_state: &Path) -> Result<PathBuf, String> {
    let name = batch_state.file_name().ok_or_else(|| {
        format!(
            "batch state path has no final component: {}",
            batch_state.display()
        )
    })?;
    let mut generation_name = name.to_os_string();
    generation_name.push(".generation.json");
    Ok(batch_state.with_file_name(generation_name))
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
    audio_track: Option<u32>,
) -> Result<(Vec<PathBuf>, Vec<OutputFormat>), String> {
    let explicit = cli.format.as_deref().map(parse_format);
    let mut outputs = Vec::with_capacity(cli.inputs.len());
    let mut formats = Vec::with_capacity(cli.inputs.len());

    if let Some(out) = &cli.output {
        if out.is_dir() || (!out.exists() && (cli.inputs.len() > 1 || cli.recursive)) {
            for (index, inp) in cli.inputs.iter().enumerate() {
                let fmt = match explicit {
                    Some(format) => format,
                    None => default_format_for_input(inp, audio_track)?,
                };
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
                .map_or_else(|| default_format_for_input(&cli.inputs[0], audio_track), Ok)?;
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
        let fmt = match explicit {
            Some(format) => format,
            None => default_format_for_input(inp, audio_track)?,
        };
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

/// Stable semantic format identifiers used by operation fingerprints.
///
/// These are deliberately distinct from filename/container extensions: ALAC
/// is carried in an M4A container and Vorbis in Ogg, but changing either
/// codec must still change the normalization operation identity.
const fn operation_format_id(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Wav => "wav",
        OutputFormat::Flac => "flac",
        OutputFormat::Mp3 => "mp3",
        OutputFormat::Opus => "opus",
        OutputFormat::M4a => "m4a",
        OutputFormat::Alac => "alac",
        OutputFormat::Vorbis => "vorbis",
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

/// Select a default from the content-probed codec. A lossless input never
/// silently falls through to a lossy encoder merely because of its suffix.
fn default_format_for_input(path: &Path, audio_track: Option<u32>) -> Result<OutputFormat, String> {
    let selection = audio_track.map_or(AudioTrackSelection::Default, AudioTrackSelection::Index);
    // Output planning happens before a resumable batch starts processing its
    // assets. An unreadable asset must therefore not prevent earlier valid
    // assets from reaching their durable checkpoints. WAV is the conservative
    // lossless fallback whenever the selected programme cannot be identified;
    // descriptor construction will still report the original probe error when
    // that asset is processed.
    let identity = match forge_normalizer::decoder::probe_audio_program(path, selection) {
        Ok(identity) => identity,
        Err(_) => return Ok(OutputFormat::Wav),
    };
    let format = match identity.codec {
        AudioCodec::Flac => OutputFormat::Flac,
        AudioCodec::Mp3 => {
            #[cfg(feature = "mp3-encoding")]
            {
                OutputFormat::Mp3
            }
            #[cfg(not(feature = "mp3-encoding"))]
            {
                OutputFormat::Wav
            }
        }
        AudioCodec::Opus => {
            #[cfg(feature = "opus-encoding")]
            {
                OutputFormat::Opus
            }
            #[cfg(not(feature = "opus-encoding"))]
            {
                OutputFormat::Wav
            }
        }
        AudioCodec::Vorbis => {
            #[cfg(feature = "ffmpeg-encoding")]
            {
                OutputFormat::Vorbis
            }
            #[cfg(not(feature = "ffmpeg-encoding"))]
            {
                OutputFormat::Wav
            }
        }
        AudioCodec::Aac => {
            #[cfg(feature = "ffmpeg-encoding")]
            {
                OutputFormat::M4a
            }
            #[cfg(not(feature = "ffmpeg-encoding"))]
            {
                OutputFormat::Wav
            }
        }
        AudioCodec::Alac => {
            #[cfg(feature = "ffmpeg-encoding")]
            {
                OutputFormat::Alac
            }
            #[cfg(not(feature = "ffmpeg-encoding"))]
            {
                OutputFormat::Wav
            }
        }
        AudioCodec::Pcm(_) | AudioCodec::Dsd | AudioCodec::Mp1 | AudioCodec::Mp2 => {
            OutputFormat::Wav
        }
        _ => OutputFormat::Wav,
    };
    Ok(format)
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

#[cfg(test)]
mod tests {
    use super::{
        bounded_utf8, classify_committed_checkpoint_result, BatchGenerationCheckpointOutcome,
    };

    #[test]
    fn bounded_utf8_truncates_at_a_character_boundary() {
        let value = "あ".repeat(2_000);
        let bounded = bounded_utf8(&value, 4_097);
        assert!(bounded.len() <= 4_097);
        assert!(bounded.is_char_boundary(bounded.len()));
        assert_eq!(bounded.chars().count(), 1_365);
    }

    #[test]
    fn committed_checkpoint_errors_are_not_classified_as_generation_failures() {
        assert_eq!(
            classify_committed_checkpoint_result(false, true, Err("save failed".into())).unwrap(),
            BatchGenerationCheckpointOutcome::CommittedNeedsCheckpoint("save failed".into())
        );
        assert_eq!(
            classify_committed_checkpoint_result(false, true, Ok(())).unwrap(),
            BatchGenerationCheckpointOutcome::Persisted
        );
        assert_eq!(
            classify_committed_checkpoint_result(false, false, Err("validation failed".into()))
                .unwrap_err(),
            "validation failed"
        );
        assert_eq!(
            classify_committed_checkpoint_result(true, true, Err("revalidation failed".into()))
                .unwrap_err(),
            "revalidation failed"
        );
    }
}
