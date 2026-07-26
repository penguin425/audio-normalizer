//! Command-line interface definition (clap derive).

use clap::parser::ValueSource;
use clap::{CommandFactory, FromArgMatches, Parser};
use serde::Deserialize;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "forge",
    version,
    about = "Forge — a SIMD-accelerated EBU R128 / ITU-R BS.1770-5 loudness normalizer",
    long_about = None
)]
pub struct Cli {
    /// Load repeatable job settings from a TOML file. Explicit CLI options win.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Input WAV file(s). With multiple files and --album, all files are
    /// normalized with one shared gain (album mode).
    #[arg(required = true)]
    pub inputs: Vec<PathBuf>,

    /// Container hint for stdin (`-`): wav, flac, mp3, opus, aac, m4a, mp4, or ogg.
    #[arg(
        long = "input-format",
        value_parser = ["wav", "flac", "mp3", "opus", "aac", "m4a", "mp4", "ogg"]
    )]
    pub input_format: Option<String>,

    /// Output file (single input) or existing directory (multiple inputs).
    /// If omitted, writes <stem>_normalized.wav next to each input.
    #[arg(short = 'o', long = "output")]
    pub output: Option<PathBuf>,

    /// Recursively process supported audio files found in input directories.
    #[arg(long)]
    pub recursive: bool,

    /// Show analysis and planned output paths without writing files.
    #[arg(long)]
    pub dry_run: bool,

    /// Replace output files that already exist.
    #[arg(long)]
    pub overwrite: bool,

    /// Normalization mode: lufs (EBU R128), peak, or rms.
    #[arg(short = 'm', long = "mode", value_parser = ["lufs", "peak", "rms"], default_value = "lufs")]
    pub mode: String,

    /// Apply a named LUFS/true-peak target for a playback or delivery context.
    #[arg(
        long,
        value_parser = [
            "spotify",
            "apple-music",
            "youtube",
            "podcast-stereo",
            "podcast-mono",
            "ebu-r128",
            "atsc-a85"
        ],
        conflicts_with_all = [
            "mode",
            "target_lufs",
            "target_peak_db",
            "target_rms_db",
            "ceiling_db",
            "analyze_only",
            "write_tags"
        ]
    )]
    pub preset: Option<String>,

    /// Target integrated loudness in LUFS (--mode lufs). Spotify ≈ -14, EBU ≈ -23.
    #[arg(long = "target", default_value_t = -16.0)]
    pub target_lufs: f64,

    /// Target sample peak in dBFS (--mode peak).
    #[arg(long = "target-peak", default_value_t = -0.1)]
    pub target_peak_db: f64,

    /// Target RMS in dBFS (--mode rms).
    #[arg(long = "target-rms", default_value_t = -18.0)]
    pub target_rms_db: f64,

    /// True-peak ceiling in dBFS; gain is reduced so output never exceeds it.
    #[arg(long = "ceiling", default_value_t = -1.0)]
    pub ceiling_db: f64,

    /// Maximum applied gain in dB (safety cap on boost).
    #[arg(long = "max-gain")]
    pub max_gain_db: Option<f64>,

    /// Output container format: `wav`, `flac`, `mp3`, `opus`, or `m4a`. If omitted, inferred from the
    /// `-o` extension, else defaults to the input's format when supported
    /// (FLAC/MP3 are kept) and otherwise wav.
    #[arg(long = "format", value_parser = ["wav", "flac", "mp3", "opus", "m4a"])]
    pub format: Option<String>,

    /// Lossy encoder bitrate in kbps, used with MP3, Opus, and AAC output.
    #[arg(long = "bitrate", default_value_t = 192)]
    pub bitrate: i32,

    /// MP3 encoder quality: 0 = best/slowest, 9 = fastest. Default 2.
    #[arg(long = "quality", default_value_t = 2)]
    pub quality: i32,

    /// Album mode: one shared gain for all inputs (requires --mode lufs).
    #[arg(long = "album")]
    pub album: bool,

    /// Override the input channel order when container metadata is absent or
    /// wrong. Orders follow WAVE_FORMAT_EXTENSIBLE.
    #[arg(
        long = "channel-layout",
        value_parser = ["mono", "stereo", "5.1", "6.1", "7.1", "5.1.4", "7.1.4"]
    )]
    pub channel_layout: Option<String>,

    /// Treat a one-channel input as identical signals reproduced by two
    /// speakers, applying the conventional -3.01 dB pan-law correction.
    #[arg(long = "dual-mono", conflicts_with = "channel_layout")]
    pub dual_mono: bool,

    /// Only measure and print stats; do not write files.
    #[arg(long = "analyze")]
    pub analyze_only: bool,

    /// Print analyze results as a JSON array to stdout.
    #[arg(
        long,
        requires = "analyze_only",
        conflicts_with_all = ["csv", "ndjson"]
    )]
    pub json: bool,

    /// Print one compact JSON analysis object per input line to stdout.
    #[arg(
        long,
        requires = "analyze_only",
        conflicts_with_all = ["json", "csv"]
    )]
    pub ndjson: bool,

    /// Write analyze results as CSV to this path, or `-` for stdout.
    #[arg(
        long,
        value_name = "PATH",
        requires = "analyze_only",
        conflicts_with_all = ["json", "ndjson"]
    )]
    pub csv: Option<PathBuf>,

    /// Evaluate analysis against a delivery specification.
    ///
    /// Accepts a built-in name or a custom .json/.toml profile path.
    #[arg(long, requires = "analyze_only")]
    pub compliance: Option<String>,

    /// JSON/TOML file of explicit dialogue or anchor time ranges.
    #[arg(
        long = "dialogue-ranges",
        value_name = "PATH",
        requires = "analyze_only",
        conflicts_with_all = ["start_seconds", "duration_seconds", "auto_dialogue"]
    )]
    pub dialogue_ranges: Option<PathBuf>,

    /// Detect reviewable dialogue candidates with Forge's deterministic detector.
    #[arg(
        long = "auto-dialogue",
        requires = "analyze_only",
        conflicts_with = "dialogue_ranges"
    )]
    pub auto_dialogue: bool,

    /// Minimum automatic-dialogue confidence from 0 to 1.
    #[arg(
        long = "dialogue-confidence",
        default_value_t = 0.6,
        requires = "auto_dialogue"
    )]
    pub dialogue_confidence: f64,

    /// Write the detector features, confidence, and selected ranges as JSON.
    #[arg(
        long = "dialogue-detection-report",
        value_name = "PATH",
        requires = "auto_dialogue"
    )]
    pub dialogue_detection_report: Option<PathBuf>,

    /// Dialogue measurement standard: infer from compliance, ATSC A/85, or EBU R128 s4.
    #[arg(
        long = "dialogue-standard",
        value_parser = ["auto", "atsc-a85", "ebu-r128-s4"],
        default_value = "auto",
        requires = "dialogue_ranges"
    )]
    pub dialogue_standard: String,

    /// Dialogue source: programme mix, centre channel, or a separate stem.
    #[arg(
        long = "dialogue-source",
        value_parser = ["mix", "center", "stem"],
        default_value = "mix",
        requires = "dialogue_ranges"
    )]
    pub dialogue_source: String,

    /// Separate dialogue stem used with `--dialogue-source stem`.
    #[arg(
        long = "dialogue-stem",
        value_name = "PATH",
        requires = "dialogue_ranges"
    )]
    pub dialogue_stem: Option<PathBuf>,

    /// JSON/TOML delivery metadata to compare with measured loudness.
    #[arg(
        long = "codec-metadata",
        value_name = "PATH",
        requires = "analyze_only"
    )]
    pub codec_metadata: Option<PathBuf>,

    /// Measure a WAVE-order stereo downmix as a separate delivery presentation.
    #[arg(long = "downmix-qc", requires = "analyze_only")]
    pub downmix_qc: bool,

    /// Write a versioned JSON delivery manifest containing every asset report.
    #[arg(long, value_name = "PATH", requires = "analyze_only")]
    pub manifest: Option<PathBuf>,

    /// JSON/TOML ADM presentation-to-channel map for presentation-aware QC.
    #[arg(
        long = "adm-presentations",
        value_name = "PATH",
        requires = "analyze_only"
    )]
    pub adm_presentations: Option<PathBuf>,

    /// Validate ADM against BS.2168, render through the EBU BS.2127 reference
    /// implementation, and measure the rendered loudspeaker presentation.
    #[arg(
        long = "adm-render",
        requires = "analyze_only",
        conflicts_with_all = ["adm_presentations", "start_seconds", "duration_seconds"]
    )]
    pub adm_render: bool,

    /// Path to the EBU ADM Toolbox `eat-process` executable.
    #[arg(long = "adm-renderer", value_name = "PATH", requires = "adm_render")]
    pub adm_renderer: Option<PathBuf>,

    /// ITU-R BS.2051 target layout name passed to the reference renderer.
    #[arg(
        long = "adm-layout",
        value_name = "LAYOUT",
        default_value = "4+5+0",
        requires = "adm_render"
    )]
    pub adm_layout: String,

    /// ITU-R BS.2168 emission-profile level used for validation.
    #[arg(
        long = "adm-profile-level",
        default_value_t = 0,
        value_parser = clap::value_parser!(u8).range(0..=2),
        requires = "adm_render"
    )]
    pub adm_profile_level: u8,

    /// Keep the rendered BS.2051 loudspeaker signals at this path.
    #[arg(
        long = "adm-rendered-output",
        value_name = "PATH",
        requires = "adm_render"
    )]
    pub adm_rendered_output: Option<PathBuf>,

    /// Start analysis at this source time in seconds.
    #[arg(long = "start", value_name = "SECONDS", requires = "analyze_only")]
    pub start_seconds: Option<f64>,

    /// Analyze at most this many seconds from --start (or the beginning).
    #[arg(long = "duration", value_name = "SECONDS", requires = "analyze_only")]
    pub duration_seconds: Option<f64>,

    /// Write a time-resolved QC report (.json, .ndjson, or .csv; `-` is NDJSON).
    #[arg(long, value_name = "PATH", requires = "analyze_only")]
    pub timeline: Option<PathBuf>,

    /// Timeline sampling interval in milliseconds.
    #[arg(
        long = "timeline-interval",
        value_name = "MILLISECONDS",
        default_value_t = 100.0,
        requires = "timeline"
    )]
    pub timeline_interval_ms: f64,

    /// Print the gain that would be applied; write nothing.
    #[arg(long = "gain-only")]
    pub gain_only: bool,

    /// Measure without changing audio and write ReplayGain 2.0 metadata to the inputs.
    #[arg(
        long = "write-tags",
        conflicts_with_all = ["analyze_only", "gain_only", "output"]
    )]
    pub write_tags: bool,

    /// Decode and measure each completed output to verify level and true peak.
    #[arg(
        long,
        conflicts_with_all = ["analyze_only", "gain_only", "write_tags", "dry_run"]
    )]
    pub verify: bool,

    /// Maximum verification deviation in LU/dB, including true-peak overshoot.
    #[arg(long, default_value_t = 0.5, requires = "verify")]
    pub verify_tolerance: f64,

    /// Re-encode up to N times with an automatically corrected gain when
    /// post-encode verification misses the intended level or true-peak ceiling.
    #[arg(
        long,
        default_value_t = 0,
        value_parser = clap::value_parser!(u8).range(0..=10),
        requires = "verify"
    )]
    pub verify_retries: u8,

    /// Use a streaming look-ahead true-peak limiter instead of reducing global gain.
    #[arg(long)]
    pub limiter: bool,

    /// Limiter look-ahead in milliseconds.
    #[arg(long, default_value_t = 5.0, requires = "limiter")]
    pub limiter_lookahead: f64,

    /// Limiter release time in milliseconds.
    #[arg(long, default_value_t = 100.0, requires = "limiter")]
    pub limiter_release: f64,

    /// Apply TPDF dither when writing integer PCM.
    #[arg(long = "dither")]
    pub dither: bool,

    /// Output bit depth: 8, 16, 24, 32 (integer) or 32f, 64f (float).
    /// Default: keep the input's format.
    #[arg(long = "bits", value_parser = ["8", "16", "24", "32", "32f", "64f"])]
    pub bits: Option<String>,

    /// WAV container: auto selects RIFF below 4 GiB and RF64 above it.
    #[arg(
        long = "wav-container",
        value_parser = ["auto", "riff", "rf64", "bw64"],
        default_value = "auto"
    )]
    pub wav_container: String,

    /// Write/preserve a Broadcast Wave bext chunk and measured R128 fields.
    #[arg(long)]
    pub bwf: bool,

    /// Number of worker threads (default: all logical cores).
    #[arg(short = 'j', long = "jobs")]
    pub jobs: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ForgeConfig {
    normalization: NormalizationConfig,
    analysis: AnalysisConfig,
    output: OutputConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct NormalizationConfig {
    preset: Option<String>,
    mode: Option<String>,
    target_lufs: Option<f64>,
    target_peak_db: Option<f64>,
    target_rms_db: Option<f64>,
    ceiling_dbtp: Option<f64>,
    max_gain_db: Option<f64>,
    limiter: Option<bool>,
    limiter_lookahead_ms: Option<f64>,
    limiter_release_ms: Option<f64>,
    dither: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AnalysisConfig {
    enabled: Option<bool>,
    compliance: Option<String>,
    dialogue_ranges: Option<PathBuf>,
    start_seconds: Option<f64>,
    duration_seconds: Option<f64>,
    timeline: Option<PathBuf>,
    timeline_interval_ms: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct OutputConfig {
    format: Option<String>,
    bits: Option<String>,
    bitrate_kbps: Option<i32>,
    quality: Option<i32>,
    verify: Option<bool>,
    verify_tolerance: Option<f64>,
    verify_retries: Option<u8>,
    wav_container: Option<String>,
    bwf: Option<bool>,
}

impl Cli {
    pub fn parse_with_config() -> Result<Self, String> {
        let matches = Self::command().get_matches();
        let mut cli = Self::from_arg_matches(&matches)
            .map_err(|error| format!("parse command line: {error}"))?;
        let Some(path) = cli.config.clone() else {
            return Ok(cli);
        };
        let text = fs::read_to_string(&path)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        let config: ForgeConfig =
            toml::from_str(&text).map_err(|error| format!("parse {}: {error}", path.display()))?;
        cli.apply_config(config, &matches, &path)?;
        Ok(cli)
    }

    fn apply_config(
        &mut self,
        config: ForgeConfig,
        matches: &clap::ArgMatches,
        config_path: &Path,
    ) -> Result<(), String> {
        let normalization = config.normalization;
        let explicit_preset = is_explicit(matches, "preset");
        let explicit_target = [
            "mode",
            "target_lufs",
            "target_peak_db",
            "target_rms_db",
            "ceiling_db",
        ]
        .iter()
        .any(|id| is_explicit(matches, id));
        if !explicit_preset && !explicit_target {
            set_option_if_implicit(matches, "preset", &mut self.preset, normalization.preset);
        }
        if self.preset.is_none() {
            set_if_implicit(matches, "mode", &mut self.mode, normalization.mode);
            set_if_implicit(
                matches,
                "target_lufs",
                &mut self.target_lufs,
                normalization.target_lufs,
            );
            set_if_implicit(
                matches,
                "target_peak_db",
                &mut self.target_peak_db,
                normalization.target_peak_db,
            );
            set_if_implicit(
                matches,
                "target_rms_db",
                &mut self.target_rms_db,
                normalization.target_rms_db,
            );
            set_if_implicit(
                matches,
                "ceiling_db",
                &mut self.ceiling_db,
                normalization.ceiling_dbtp,
            );
        }
        set_option_if_implicit(
            matches,
            "max_gain_db",
            &mut self.max_gain_db,
            normalization.max_gain_db,
        );
        set_if_implicit(matches, "limiter", &mut self.limiter, normalization.limiter);
        set_if_implicit(
            matches,
            "limiter_lookahead",
            &mut self.limiter_lookahead,
            normalization.limiter_lookahead_ms,
        );
        set_if_implicit(
            matches,
            "limiter_release",
            &mut self.limiter_release,
            normalization.limiter_release_ms,
        );
        set_if_implicit(matches, "dither", &mut self.dither, normalization.dither);

        let analysis = config.analysis;
        set_if_implicit(
            matches,
            "analyze_only",
            &mut self.analyze_only,
            analysis.enabled,
        );
        let configured_compliance = analysis.compliance.map(|value| {
            if value.ends_with(".json") || value.ends_with(".toml") {
                resolve_path(config_path, PathBuf::from(value))
                    .to_string_lossy()
                    .into_owned()
            } else {
                value
            }
        });
        set_option_if_implicit(
            matches,
            "compliance",
            &mut self.compliance,
            configured_compliance,
        );
        set_option_if_implicit(
            matches,
            "dialogue_ranges",
            &mut self.dialogue_ranges,
            analysis
                .dialogue_ranges
                .map(|path| resolve_path(config_path, path)),
        );
        set_option_if_implicit(
            matches,
            "start_seconds",
            &mut self.start_seconds,
            analysis.start_seconds,
        );
        set_option_if_implicit(
            matches,
            "duration_seconds",
            &mut self.duration_seconds,
            analysis.duration_seconds,
        );
        set_option_if_implicit(
            matches,
            "timeline",
            &mut self.timeline,
            analysis
                .timeline
                .map(|path| resolve_path(config_path, path)),
        );
        set_if_implicit(
            matches,
            "timeline_interval_ms",
            &mut self.timeline_interval_ms,
            analysis.timeline_interval_ms,
        );

        let output = config.output;
        set_option_if_implicit(matches, "format", &mut self.format, output.format);
        set_option_if_implicit(matches, "bits", &mut self.bits, output.bits);
        set_if_implicit(matches, "bitrate", &mut self.bitrate, output.bitrate_kbps);
        set_if_implicit(matches, "quality", &mut self.quality, output.quality);
        set_if_implicit(matches, "verify", &mut self.verify, output.verify);
        set_if_implicit(
            matches,
            "verify_tolerance",
            &mut self.verify_tolerance,
            output.verify_tolerance,
        );
        set_if_implicit(
            matches,
            "verify_retries",
            &mut self.verify_retries,
            output.verify_retries,
        );
        set_if_implicit(
            matches,
            "wav_container",
            &mut self.wav_container,
            output.wav_container,
        );
        set_if_implicit(matches, "bwf", &mut self.bwf, output.bwf);
        if !self.analyze_only
            && (self.compliance.is_some()
                || self.dialogue_ranges.is_some()
                || self.start_seconds.is_some()
                || self.duration_seconds.is_some()
                || self.timeline.is_some())
        {
            return Err(
                "[analysis] compliance/dialogue/range/timeline settings require `enabled = true`"
                    .into(),
            );
        }
        if self.dialogue_ranges.is_some()
            && (self.start_seconds.is_some() || self.duration_seconds.is_some())
        {
            return Err(
                "analysis.dialogue_ranges conflicts with start_seconds/duration_seconds".into(),
            );
        }
        if self.verify_retries > 0 && !self.verify {
            return Err("output.verify_retries requires `verify = true`".into());
        }
        self.validate_config_values()
    }

    fn validate_config_values(&self) -> Result<(), String> {
        validate_choice("normalization.mode", &self.mode, &["lufs", "peak", "rms"])?;
        if let Some(preset) = &self.preset {
            validate_choice(
                "normalization.preset",
                preset,
                &[
                    "spotify",
                    "apple-music",
                    "youtube",
                    "podcast-stereo",
                    "podcast-mono",
                    "ebu-r128",
                    "atsc-a85",
                ],
            )?;
        }
        if let Some(format) = &self.format {
            validate_choice(
                "output.format",
                format,
                &["wav", "flac", "mp3", "opus", "m4a"],
            )?;
        }
        if let Some(bits) = &self.bits {
            validate_choice("output.bits", bits, &["8", "16", "24", "32", "32f", "64f"])?;
        }
        validate_choice(
            "output.wav_container",
            &self.wav_container,
            &["auto", "riff", "rf64", "bw64"],
        )
    }
}

fn is_explicit(matches: &clap::ArgMatches, id: &str) -> bool {
    matches.value_source(id) == Some(ValueSource::CommandLine)
}

fn set_if_implicit<T>(
    matches: &clap::ArgMatches,
    id: &str,
    destination: &mut T,
    configured: Option<T>,
) {
    if !is_explicit(matches, id) {
        if let Some(value) = configured {
            *destination = value;
        }
    }
}

fn set_option_if_implicit<T>(
    matches: &clap::ArgMatches,
    id: &str,
    destination: &mut Option<T>,
    configured: Option<T>,
) {
    if !is_explicit(matches, id) {
        if let Some(value) = configured {
            *destination = Some(value);
        }
    }
}

fn resolve_path(config_path: &Path, value: PathBuf) -> PathBuf {
    if value.is_absolute() || value.as_os_str() == "-" {
        value
    } else {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(value)
    }
}

fn validate_choice(name: &str, value: &str, choices: &[&str]) -> Result<(), String> {
    if choices.contains(&value) {
        Ok(())
    } else {
        Err(format!("{name} must be one of: {}", choices.join(", ")))
    }
}
