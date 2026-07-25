//! Command-line interface definition (clap derive).

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "forge",
    version,
    about = "Forge — a SIMD-accelerated EBU R128 / ITU-R BS.1770-5 loudness normalizer",
    long_about = None
)]
pub struct Cli {
    /// Input WAV file(s). With multiple files and --album, all files are
    /// normalized with one shared gain (album mode).
    #[arg(required = true)]
    pub inputs: Vec<PathBuf>,

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

    /// Output container format: `wav`, `flac`, or `mp3`. If omitted, inferred from the
    /// `-o` extension, else defaults to the input's format when supported
    /// (FLAC/MP3 are kept) and otherwise wav.
    #[arg(long = "format", value_parser = ["wav", "flac", "mp3"])]
    pub format: Option<String>,

    /// MP3 bitrate in kbps (CBR), used with `--format mp3`.
    #[arg(long = "bitrate", default_value_t = 192)]
    pub bitrate: i32,

    /// MP3 encoder quality: 0 = best/slowest, 9 = fastest. Default 2.
    #[arg(long = "quality", default_value_t = 2)]
    pub quality: i32,

    /// Album mode: one shared gain for all inputs (requires --mode lufs).
    #[arg(long = "album")]
    pub album: bool,

    /// Only measure and print stats; do not write files.
    #[arg(long = "analyze")]
    pub analyze_only: bool,

    /// Print analyze results as a JSON array to stdout.
    #[arg(long, requires = "analyze_only", conflicts_with = "csv")]
    pub json: bool,

    /// Write analyze results as CSV to this path, or `-` for stdout.
    #[arg(long, value_name = "PATH", requires = "analyze_only")]
    pub csv: Option<PathBuf>,

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

    /// Apply TPDF dither when writing integer PCM.
    #[arg(long = "dither")]
    pub dither: bool,

    /// Output bit depth: 8, 16, 24, 32 (integer) or 32f, 64f (float).
    /// Default: keep the input's format.
    #[arg(long = "bits", value_parser = ["8", "16", "24", "32", "32f", "64f"])]
    pub bits: Option<String>,

    /// Number of worker threads (default: all logical cores).
    #[arg(short = 'j', long = "jobs")]
    pub jobs: Option<usize>,
}
