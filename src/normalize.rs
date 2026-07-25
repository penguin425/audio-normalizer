//! Normalization pipeline: analyze -> compute gain -> apply -> write.
//!
//! Three loudness strategies share one engine:
//!   * `Lufs`  - EBU R128 integrated loudness (the broadcast/streaming standard).
//!   * `Peak`  - classic sample-peak normalization.
//!   * `Rms`   - RMS-level normalization.
//! All strategies are constrained by a true-peak ceiling: the linear gain is
//! reduced (never increased beyond what's needed) so the gained signal's
//! *inter-sample* true peak does not exceed the ceiling, which is how
//! professional loudness normalizers avoid clipping without a dynamic limiter.

use crate::decoder;
use crate::dsp::{lufs, simd, truepeak};
use crate::metadata;
#[cfg(feature = "mp3-encoding")]
use crate::mp3enc;
use crate::wav::{AudioBuffer, PcmKind, WavWriter};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Lufs,
    Peak,
    Rms,
}

/// Output container format. The DSP/gain stage is format-agnostic; this only
/// selects the muxer used when writing the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Wav,
    Mp3,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub mode: Mode,
    pub target_lufs: f64,
    pub target_peak_db: f64,
    pub target_rms_db: f64,
    /// True-peak ceiling in dBFS. Gain is reduced so the output never exceeds it.
    pub ceiling_db: f64,
    /// Optional safety cap on the applied gain (dB).
    pub max_gain_db: Option<f64>,
    /// Apply TPDF dither when writing integer PCM (WAV only).
    pub dither: bool,
    /// WAV output sample format; otherwise keep the input's format (WAV only).
    pub output_kind: Option<PcmKind>,
    /// MP3 CBR bitrate in kbps (MP3 only).
    pub mp3_bitrate: i32,
    /// MP3 encoder quality 0..=9, 0 = best/slowest (MP3 only).
    pub mp3_quality: i32,
}

/// Loudness/peak analysis of a single file.
#[derive(Debug, Clone)]
pub struct Analysis {
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: usize,
    pub kind: PcmKind,
    pub lufs: f64,
    pub max_momentary_lufs: f64,
    pub max_short_term_lufs: f64,
    pub loudness_range_lu: f64,
    pub rms_db: f64,
    pub sample_peak: f32, // 0..1
    pub true_peak: f32,   // 0..1
    /// Complete 400 ms block energies used to recompute album gating.
    #[doc(hidden)]
    pub loudness_blocks: Vec<f64>,
}

impl Analysis {
    pub fn duration_secs(&self) -> f64 {
        self.frames as f64 / self.sample_rate as f64
    }
    pub fn sample_peak_db(&self) -> f64 {
        to_db(self.sample_peak as f64)
    }
    pub fn true_peak_db(&self) -> f64 {
        to_db(self.true_peak as f64)
    }
}

#[inline]
fn to_db(x: f64) -> f64 {
    if x > 0.0 {
        20.0 * x.log10()
    } else {
        f64::NEG_INFINITY
    }
}

/// Analyze an already-decoded buffer.
pub fn analyze(buf: &AudioBuffer) -> Analysis {
    let ebu = lufs::measure_ebu(buf);
    let (rms_db, sample_peak) = lufs::measure_rms_peak(buf);
    let true_peak = truepeak::measure_true_peak(buf);
    Analysis {
        sample_rate: buf.sample_rate,
        channels: buf.channels,
        frames: buf.frames,
        kind: buf.source_kind,
        lufs: ebu.integrated_lufs,
        max_momentary_lufs: ebu.max_momentary_lufs,
        max_short_term_lufs: ebu.max_short_term_lufs,
        loudness_range_lu: ebu.loudness_range_lu,
        rms_db,
        sample_peak,
        true_peak,
        loudness_blocks: ebu.gating_blocks,
    }
}

/// Linear gain that maps `an` onto the plan's target, after ceiling protection.
pub fn compute_gain(an: &Analysis, plan: &Plan) -> f32 {
    let gain_db = match plan.mode {
        Mode::Lufs => plan.target_lufs - an.lufs,
        Mode::Peak => plan.target_peak_db - an.sample_peak_db(),
        Mode::Rms => plan.target_rms_db - an.rms_db,
    };
    clamp_gain(10.0_f64.powf(gain_db / 20.0), an.true_peak as f64, plan)
}

fn clamp_gain(mut lin: f64, true_peak: f64, plan: &Plan) -> f32 {
    let ceil_lin = 10.0_f64.powf(plan.ceiling_db / 20.0);
    if true_peak > 0.0 {
        let max_for_ceil = ceil_lin / true_peak;
        if lin > max_for_ceil {
            lin = max_for_ceil;
        }
    }
    if let Some(maxg) = plan.max_gain_db {
        let max_lin = 10.0_f64.powf(maxg / 20.0);
        if lin > max_lin {
            lin = max_lin;
        }
    }
    lin as f32
}

/// Apply `gain` to every channel, then a safety brick-wall clip to the ceiling.
pub fn apply_gain_and_protect(buf: &mut AudioBuffer, gain: f32, plan: &Plan) {
    let ceil_lin = 10.0_f64.powf(plan.ceiling_db / 20.0) as f32;
    for ch in buf.data.iter_mut() {
        simd::apply_gain(ch, gain);
        simd::hard_clip(ch, ceil_lin);
    }
}

pub fn load<P: AsRef<Path>>(path: P) -> Result<AudioBuffer, String> {
    decoder::decode(path.as_ref())
}

pub fn write<P: AsRef<Path>>(
    buf: &AudioBuffer,
    path: P,
    plan: &Plan,
    format: OutputFormat,
) -> Result<(), String> {
    let p = path.as_ref();
    match format {
        OutputFormat::Wav => {
            let kind = plan.output_kind.unwrap_or(buf.source_kind);
            WavWriter::write(p, buf, kind, plan.dither)
                .map_err(|e| format!("write {}: {e}", p.display()))
        }
        OutputFormat::Mp3 => {
            #[cfg(feature = "mp3-encoding")]
            {
                mp3enc::write_mp3(p, buf, plan.mp3_bitrate, plan.mp3_quality)
            }
            #[cfg(not(feature = "mp3-encoding"))]
            {
                let _ = (buf, plan);
                Err("MP3 output is unavailable; rebuild with `--features mp3-encoding`".into())
            }
        }
    }
}

/// Analyze a file on disk (buffer is dropped after measurement).
pub fn analyze_file<P: AsRef<Path>>(path: P) -> Result<Analysis, String> {
    let buf = load(&path)?;
    Ok(analyze(&buf))
}

/// Normalize a single file in one pass (load, analyze, gain, write).
pub fn normalize_one<P: AsRef<Path>>(
    input: P,
    output: P,
    plan: &Plan,
    format: OutputFormat,
) -> Result<(Analysis, f32), String> {
    let mut buf = load(&input)?;
    let an = analyze(&buf);
    let gain = compute_gain(&an, plan);
    apply_gain_and_protect(&mut buf, gain, plan);
    write(&buf, &output, plan, format)?;
    metadata::copy_metadata(input.as_ref(), output.as_ref())?;
    Ok((an, gain))
}

/// Album loudness from the combined population of all complete gating blocks.
pub fn album_lufs(analyses: &[Analysis]) -> f64 {
    lufs::gated_lufs(
        &analyses
            .iter()
            .flat_map(|an| an.loudness_blocks.iter().copied())
            .collect::<Vec<_>>(),
    )
}

/// Album-mode gain: a single shared gain from the album loudness, constrained
/// by the worst (largest) true peak across all files so nothing exceeds the ceiling.
pub fn album_gain(analyses: &[Analysis], plan: &Plan) -> f32 {
    let album_l = album_lufs(analyses);
    let gain_db = plan.target_lufs - album_l;
    let worst_tp = analyses.iter().map(|a| a.true_peak).fold(0.0f32, f32::max);
    clamp_gain(10.0_f64.powf(gain_db / 20.0), worst_tp as f64, plan)
}

/// Album mode: measure every file, compute one shared gain, then apply it to
/// each file. Two passes keep peak memory bounded to one file at a time.
/// `formats[i]` selects the output container for file `i`.
pub fn normalize_album(
    inputs: &[PathBuf],
    outputs: &[PathBuf],
    plan: &Plan,
    formats: &[OutputFormat],
) -> Result<Vec<(Analysis, f32)>, String> {
    let analyses: Vec<Analysis> = inputs.iter().map(analyze_file).collect::<Result<_, _>>()?;
    let gain = album_gain(&analyses, plan);
    let mut results = Vec::with_capacity(inputs.len());
    for (i, (input, output)) in inputs.iter().zip(outputs.iter()).enumerate() {
        let mut buf = load(input)?;
        apply_gain_and_protect(&mut buf, gain, plan);
        let fmt = formats.get(i).copied().unwrap_or(OutputFormat::Wav);
        write(&buf, output, plan, fmt)?;
        metadata::copy_metadata(input, output)?;
        results.push((analyses[i].clone(), gain));
    }
    Ok(results)
}
