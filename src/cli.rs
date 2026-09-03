//! Command-line interface definition (clap derive).

use clap::parser::ValueSource;
use clap::{CommandFactory, FromArgMatches, Parser};
use serde::Deserialize;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

#[derive(Parser, Debug, Clone)]
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

    /// Container hint for stdin (`-`): wav, dsf, dff, flac, mp3, opus, aac, m4a, mp4, or ogg.
    #[arg(
        long = "input-format",
        value_parser = [
            "wav", "dsf", "dff", "flac", "mp3", "opus", "aac", "m4a", "mp4", "ogg"
        ]
    )]
    pub input_format: Option<String>,

    /// Output file (single input) or existing directory (multiple inputs).
    /// If omitted, writes `<stem>_normalized.wav` next to each input.
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
            "spotify-normal-2026-07-30",
            "apple-music",
            "apple-music-reference-2026-07-30",
            "youtube",
            "youtube-reference-2026-07-30",
            "podcast-stereo",
            "podcast-mono",
            "ebu-r128",
            "atsc-a85",
            "arib-tr-b32"
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

    /// Output container format: `wav`, `flac`, `mp3`, `opus`, `m4a`, `alac`, or `vorbis`. If omitted, inferred from the
    /// `-o` extension, else defaults to the input's format when supported
    /// (FLAC/MP3 are kept) and otherwise wav.
    #[arg(
        long = "format",
        value_parser = ["wav", "flac", "mp3", "opus", "m4a", "alac", "vorbis"]
    )]
    pub format: Option<String>,

    /// Output sample rate in Hz, using delay-compensated windowed-sinc conversion.
    #[arg(
        long = "sample-rate",
        value_name = "HZ",
        value_parser = clap::value_parser!(u32).range(8000..=384000),
        conflicts_with_all = ["analyze_only", "write_tags"]
    )]
    pub sample_rate_hz: Option<u32>,

    /// Sample-rate conversion quality.
    #[arg(
        long = "resample-quality",
        value_parser = ["fast", "balanced", "best"],
        default_value = "balanced",
        requires = "sample_rate_hz"
    )]
    pub resample_quality: String,

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

    /// Measurement engine: fast production path or deterministic reference path.
    #[arg(
        long = "analysis-engine",
        value_parser = ["fast", "reference"],
        default_value = "fast"
    )]
    analysis_engine: String,

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

    /// Automatically extract codec delivery metadata with an ffprobe-compatible
    /// tool and evaluate it against the decoded programme.
    #[arg(
        long = "codec-qc",
        requires = "analyze_only",
        conflicts_with = "codec_metadata"
    )]
    pub codec_qc: bool,

    /// Path to the ffprobe-compatible codec metadata extractor.
    #[arg(long = "codec-prober", value_name = "PATH", requires = "codec_qc")]
    pub codec_prober: Option<PathBuf>,

    /// Unencoded reference audio for decoded codec round-trip comparison.
    #[arg(long = "codec-reference", value_name = "PATH", requires = "codec_qc")]
    pub codec_reference: Option<PathBuf>,

    /// Maximum absolute loudness, true-peak, and dialnorm deviation in LU/dB.
    #[arg(
        long = "codec-qc-tolerance",
        default_value_t = 0.5,
        requires = "codec_qc"
    )]
    pub codec_qc_tolerance: f64,

    /// Measure a WAVE-order stereo downmix as a separate delivery presentation.
    #[arg(long = "downmix-qc", requires = "analyze_only")]
    pub downmix_qc: bool,

    /// Write a versioned JSON delivery manifest containing every asset and container QC report.
    #[arg(long, value_name = "PATH", requires = "analyze_only")]
    pub manifest: Option<PathBuf>,

    /// Run published EBU QC baseband checks, including signal-health checks.
    #[arg(long = "ebu-qc", requires = "analyze_only")]
    pub ebu_qc: bool,

    /// Level at or below which EBU 0078B silence is detected.
    #[arg(
        long = "silence-threshold",
        default_value_t = -60.0,
        requires = "ebu_qc"
    )]
    pub silence_threshold_dbfs: f64,

    /// Minimum EBU 0078B silence duration in seconds.
    #[arg(long = "silence-duration", default_value_t = 1.0, requires = "ebu_qc")]
    pub silence_duration_seconds: f64,

    /// Consecutive full-scale samples required by EBU 0005B.
    #[arg(long = "clipping-samples", default_value_t = 3, requires = "ebu_qc")]
    pub clipping_minimum_samples: usize,

    /// Test-tone frequency detected by EBU 0014B.
    #[arg(long = "tone-frequency", default_value_t = 1000.0, requires = "ebu_qc")]
    pub tone_frequency_hz: f64,

    /// Minimum test-tone level in dBFS.
    #[arg(
        long = "tone-threshold",
        default_value_t = -30.0,
        requires = "ebu_qc"
    )]
    pub tone_threshold_dbfs: f64,

    /// Minimum continuous test-tone duration in seconds.
    #[arg(long = "tone-duration", default_value_t = 0.5, requires = "ebu_qc")]
    pub tone_duration_seconds: f64,

    /// Expected decoded programme duration for EBU 0009F.
    #[arg(
        long = "expected-duration",
        value_name = "SECONDS",
        requires = "ebu_qc"
    )]
    pub expected_duration_seconds: Option<f64>,

    /// Allowed deviation from --expected-duration.
    #[arg(
        long = "duration-tolerance",
        default_value_t = 0.01,
        requires = "expected_duration_seconds"
    )]
    pub duration_tolerance_seconds: f64,

    /// Expected decoded audio channel count for EBU 0004F.
    #[arg(long = "expected-channels", requires = "ebu_qc")]
    pub expected_channel_count: Option<u16>,

    /// Maximum level treated as a short EBU 0008B dropout.
    #[arg(
        long = "dropout-threshold",
        default_value_t = -70.0,
        requires = "ebu_qc"
    )]
    pub dropout_threshold_dbfs: f64,

    /// Minimum interior dropout duration in seconds.
    #[arg(
        long = "dropout-duration",
        default_value_t = 0.002,
        requires = "ebu_qc"
    )]
    pub dropout_minimum_seconds: f64,

    /// Maximum interior dropout duration in seconds.
    #[arg(
        long = "dropout-max-duration",
        default_value_t = 0.1,
        requires = "ebu_qc"
    )]
    pub dropout_maximum_seconds: f64,

    /// Maximum accepted stereo-pair correlation for EBU 0012B detection.
    #[arg(
        long = "phase-correlation-threshold",
        default_value_t = -0.5,
        requires = "ebu_qc"
    )]
    pub phase_correlation_threshold: f64,

    /// EBU 0012B correlation window duration in seconds.
    #[arg(long = "phase-window", default_value_t = 0.5, requires = "ebu_qc")]
    pub phase_window_seconds: f64,

    /// Local full-scale impulse threshold for EBU 0057B clicks.
    #[arg(long = "click-threshold", default_value_t = 0.5, requires = "ebu_qc")]
    pub click_threshold: f64,

    /// Minimum whole-programme RMS level for EBU 0077B.
    #[arg(
        long = "minimum-average-level",
        default_value_t = -50.0,
        requires = "ebu_qc"
    )]
    pub minimum_average_level_dbfs: f64,

    /// Minimum fitted 50/60 Hz harmonic level for EBU 0088B.
    #[arg(
        long = "hum-threshold",
        default_value_t = -50.0,
        requires = "ebu_qc"
    )]
    pub hum_threshold_dbfs: f64,

    /// Minimum continuous hum/buzz duration in seconds.
    #[arg(long = "hum-duration", default_value_t = 1.0, requires = "ebu_qc")]
    pub hum_minimum_seconds: f64,

    /// Minimum band-limited EBU 0086B noise level in dBFS.
    #[arg(long = "noise-threshold", default_value_t = -60.0, requires = "ebu_qc")]
    pub noise_threshold_dbfs: f64,

    /// Maximum programme RMS level at which EBU 0086B noise is evaluated.
    #[arg(long = "noise-gate", default_value_t = -35.0, requires = "ebu_qc")]
    pub noise_gate_dbfs: f64,

    /// Minimum continuous EBU 0086B noise duration in seconds.
    #[arg(long = "noise-duration", default_value_t = 1.0, requires = "ebu_qc")]
    pub noise_minimum_seconds: f64,

    /// Lower edge of the declared EBU 0086B measurement bandwidth.
    #[arg(long = "noise-low-hz", default_value_t = 200.0, requires = "ebu_qc")]
    pub noise_low_hz: f64,

    /// Upper edge of the declared EBU 0086B measurement bandwidth.
    #[arg(
        long = "noise-high-hz",
        default_value_t = 15_000.0,
        requires = "ebu_qc"
    )]
    pub noise_high_hz: f64,

    /// Minimum time-frequency coherence for EBU 0170B cross-talk.
    #[arg(
        long = "crosstalk-coherence",
        default_value_t = 0.95,
        requires = "ebu_qc"
    )]
    pub crosstalk_coherence_threshold: f64,

    /// Minimum source-to-victim level delta for EBU 0170B.
    #[arg(
        long = "crosstalk-level-delta",
        default_value_t = 18.0,
        requires = "ebu_qc"
    )]
    pub crosstalk_level_delta_db: f64,

    /// Minimum continuous EBU 0170B cross-talk duration.
    #[arg(
        long = "crosstalk-duration",
        default_value_t = 1.0,
        requires = "ebu_qc"
    )]
    pub crosstalk_minimum_seconds: f64,

    /// Stereo-pair level imbalance that triggers EBU 0230B.
    #[arg(
        long = "panning-imbalance",
        default_value_t = 18.0,
        requires = "ebu_qc"
    )]
    pub panning_imbalance_db: f64,

    /// Minimum continuous EBU 0230B panning anomaly duration.
    #[arg(long = "panning-duration", default_value_t = 2.0, requires = "ebu_qc")]
    pub panning_minimum_seconds: f64,

    /// Highest expected LFE frequency for EBU 0095B.
    #[arg(long = "lfe-cutoff", default_value_t = 120.0, requires = "ebu_qc")]
    pub lfe_cutoff_hz: f64,

    /// Maximum accepted LFE energy ratio above --lfe-cutoff.
    #[arg(
        long = "lfe-out-of-band-ratio",
        default_value_t = 0.25,
        requires = "ebu_qc"
    )]
    pub lfe_out_of_band_ratio: f64,

    /// Require mono or sample-identical dual-mono presentation for EBU 0124B.
    #[arg(long = "expect-mono", requires = "ebu_qc")]
    pub expect_mono: bool,

    /// Maximum full-scale sample difference accepted for dual mono.
    #[arg(
        long = "mono-difference-threshold",
        default_value_t = 1.0 / 32_768.0,
        requires = "expect_mono"
    )]
    pub mono_difference_threshold: f64,

    /// Maximum accepted absolute DC mean in dBFS.
    #[arg(
        long = "dc-offset-threshold",
        default_value_t = -40.0,
        requires = "ebu_qc"
    )]
    pub dc_offset_threshold_dbfs: f64,

    /// Maximum accepted stereo-pair sample delay.
    #[arg(
        long = "interchannel-delay-samples",
        default_value_t = 1,
        requires = "ebu_qc"
    )]
    pub interchannel_delay_samples: usize,

    /// Minimum duration of an active constant sample run.
    #[arg(
        long = "stuck-sample-duration",
        default_value_t = 0.05,
        requires = "ebu_qc"
    )]
    pub stuck_sample_seconds: f64,

    /// Adjacent-sample full-scale delta treated as a discontinuity.
    #[arg(
        long = "discontinuity-threshold",
        default_value_t = 0.75,
        requires = "ebu_qc"
    )]
    pub discontinuity_threshold: f64,

    /// JSON/TOML ADM presentation-to-channel map for presentation-aware QC.
    #[arg(
        long = "adm-presentations",
        value_name = "PATH",
        requires = "analyze_only"
    )]
    pub adm_presentations: Option<PathBuf>,

    /// Validate ADM against a named production profile.
    #[arg(
        long = "adm-profile",
        value_parser = ["ebu-production"],
        requires = "analyze_only"
    )]
    pub adm_profile: Option<String>,

    /// Apply the EBU Tech 3393 reading or writing requirements.
    #[arg(
        long = "adm-profile-mode",
        value_parser = ["read", "write"],
        default_value = "read",
        requires = "adm_profile"
    )]
    pub adm_profile_mode: String,

    /// Write the complete rule-by-rule ADM production-profile audit as JSON.
    #[arg(
        long = "adm-profile-report",
        value_name = "PATH",
        requires = "adm_profile"
    )]
    pub adm_profile_report: Option<PathBuf>,

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

    /// Measure without changing audio and write container-native loudness metadata.
    ///
    /// Ogg Opus uses RFC 7845 R128_GAIN; other supported inputs use ReplayGain 2.0.
    #[arg(
        long = "write-tags",
        conflicts_with_all = ["analyze_only", "gain_only", "output"]
    )]
    pub write_tags: bool,

    /// Also write Apple Sound Check compatibility metadata and verify its
    /// exact write/read round trip. The iTunNORM mapping is non-normative.
    #[arg(long = "sound-check", requires = "write_tags")]
    pub sound_check: bool,

    /// Verify level/true peak from exact native lossless PCM or a codec re-decode.
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

    /// Write a versioned JSON report of gain envelope, limiting, clipping,
    /// and decoded codec drift for all normalized outputs.
    #[arg(
        long = "difference-report",
        value_name = "PATH",
        conflicts_with_all = ["analyze_only", "gain_only", "write_tags", "dry_run"]
    )]
    pub difference_report: Option<PathBuf>,

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
    engine: Option<String>,
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
    sample_rate_hz: Option<u32>,
    resample_quality: Option<String>,
    bits: Option<String>,
    bitrate_kbps: Option<i32>,
    quality: Option<i32>,
    verify: Option<bool>,
    verify_tolerance: Option<f64>,
    verify_retries: Option<u8>,
    difference_report: Option<PathBuf>,
    wav_container: Option<String>,
    bwf: Option<bool>,
}

impl Cli {
    /// Return the selected loudness-analysis engine name.
    pub fn analysis_engine(&self) -> &str {
        &self.analysis_engine
    }

    pub fn parse_with_config() -> Result<Self, String> {
        let matches = Self::command().get_matches();
        Self::from_matches_with_config(&matches)
    }

    /// Build a CLI value from already parsed matches and apply its optional
    /// TOML configuration.
    ///
    /// Callers may add their own arguments to the command returned by
    /// `Cli::command()` before parsing;
    /// unknown match IDs are ignored while Forge's built-in options retain
    /// their normal explicit-command-line precedence.
    pub fn from_matches_with_config(matches: &clap::ArgMatches) -> Result<Self, String> {
        let mut cli = Self::from_arg_matches(matches)
            .map_err(|error| format!("parse command line: {error}"))?;
        let Some(path) = cli.config.clone() else {
            return Ok(cli);
        };
        let text = fs::read_to_string(&path)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        let config: ForgeConfig =
            toml::from_str(&text).map_err(|error| format!("parse {}: {error}", path.display()))?;
        cli.apply_config(config, matches, &path)?;
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
        set_if_implicit(
            matches,
            "analysis_engine",
            &mut self.analysis_engine,
            analysis.engine,
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
        set_option_if_implicit(
            matches,
            "sample_rate_hz",
            &mut self.sample_rate_hz,
            output.sample_rate_hz,
        );
        set_if_implicit(
            matches,
            "resample_quality",
            &mut self.resample_quality,
            output.resample_quality,
        );
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
        set_option_if_implicit(
            matches,
            "difference_report",
            &mut self.difference_report,
            output
                .difference_report
                .map(|path| resolve_path(config_path, path)),
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
                || self.analysis_engine != "fast"
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
        if self.difference_report.is_some()
            && (self.analyze_only || self.gain_only || self.write_tags || self.dry_run)
        {
            return Err(
                "output.difference_report conflicts with analysis, gain-only, tags, and dry-run"
                    .into(),
            );
        }
        self.validate_config_values()
    }

    fn validate_config_values(&self) -> Result<(), String> {
        validate_choice("normalization.mode", &self.mode, &["lufs", "peak", "rms"])?;
        validate_choice(
            "analysis.engine",
            &self.analysis_engine,
            &["fast", "reference"],
        )?;
        if let Some(preset) = &self.preset {
            validate_choice(
                "normalization.preset",
                preset,
                &[
                    "spotify",
                    "spotify-normal-2026-07-30",
                    "apple-music",
                    "apple-music-reference-2026-07-30",
                    "youtube",
                    "youtube-reference-2026-07-30",
                    "podcast-stereo",
                    "podcast-mono",
                    "ebu-r128",
                    "atsc-a85",
                    "arib-tr-b32",
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
            "output.resample_quality",
            &self.resample_quality,
            &["fast", "balanced", "best"],
        )?;
        if self
            .sample_rate_hz
            .is_some_and(|rate| !(8_000..=384_000).contains(&rate))
        {
            return Err("output.sample_rate_hz must be between 8000 and 384000".into());
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
