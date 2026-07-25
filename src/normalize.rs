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

use crate::atomic::AtomicOutput;
use crate::decoder;
use crate::dsp::limiter::{LimiterConfig, TruePeakLimiter};
use crate::dsp::{lufs, simd, truepeak};
use crate::flacenc::FlacStreamWriter;
use crate::metadata;
#[cfg(feature = "mp3-encoding")]
use crate::mp3enc;
use crate::wav::{AudioBuffer, ChannelRole, PcmKind, WavContainer, WavStreamWriter, WavWriter};
use serde::{Deserialize, Serialize};
use std::fs;
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
    Flac,
    Mp3,
    Opus,
    M4a,
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
    /// Apply TPDF dither when writing integer PCM (WAV/FLAC).
    pub dither: bool,
    /// PCM output sample format; FLAC maps this to 16 or 24 bits.
    pub output_kind: Option<PcmKind>,
    /// MP3 CBR bitrate in kbps (MP3 only).
    pub mp3_bitrate: i32,
    /// MP3 encoder quality 0..=9, 0 = best/slowest (MP3 only).
    pub mp3_quality: i32,
    /// Optional streaming look-ahead true-peak limiter.
    pub limiter: Option<LimiterConfig>,
    /// RIFF/RF64/BW64 selection for WAV output.
    pub wav_container: WavContainer,
    /// Preserve/create BWF metadata and update its R128 measurement fields.
    pub bwf: bool,
}

/// Loudness/peak analysis of a single file.
#[derive(Debug, Clone)]
pub struct Analysis {
    pub sample_rate: u32,
    pub channels: u16,
    pub channel_roles: Vec<ChannelRole>,
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

#[derive(Debug, Clone)]
pub struct TimedAnalysis {
    pub analysis: Analysis,
    pub timeline: Vec<lufs::LoudnessTimelinePoint>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DialogueRange {
    pub start_seconds: f64,
    pub duration_seconds: f64,
}

#[derive(Debug, Clone)]
pub struct DialogueMeasurement {
    pub lufs: f64,
    pub duration_seconds: f64,
    pub range_count: usize,
    pub standard: &'static str,
    pub method: &'static str,
    pub source: DialogueSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DialogueStandard {
    AtscA85,
    EbuR128S4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DialogueSource {
    Mix,
    Center,
    Stem,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DialogueRangeFile {
    ranges: Vec<DialogueRange>,
}

#[derive(Debug, Clone)]
pub struct Verification {
    pub output: Analysis,
    pub expected_level: f64,
    pub actual_level: f64,
    pub deviation: f64,
    pub level_ok: bool,
    pub true_peak_ok: bool,
}

#[derive(Debug, Clone)]
pub struct CorrectedNormalization {
    pub source: Analysis,
    pub gain: f32,
    pub verification: Verification,
    /// Number of encoding passes, including the initial pass.
    pub attempts: usize,
}

#[derive(Debug, Clone)]
pub struct CorrectedAlbumNormalization {
    pub sources: Vec<Analysis>,
    pub gain: f32,
    pub verifications: Vec<Verification>,
    pub expected_album_lufs: f64,
    pub actual_album_lufs: f64,
    /// Number of complete album encoding passes, including the initial pass.
    pub attempts: usize,
}

impl Verification {
    pub fn passed(&self) -> bool {
        self.level_ok && self.true_peak_ok
    }
}

impl Analysis {
    /// EBU Tech 3341 warns that LRA is not stable during the first minute.
    pub const LRA_STABLE_AFTER_SECONDS: f64 = 60.0;

    pub fn duration_secs(&self) -> f64 {
        self.frames as f64 / self.sample_rate as f64
    }
    pub fn loudness_range_stable(&self) -> bool {
        self.duration_secs() >= Self::LRA_STABLE_AFTER_SECONDS
    }
    pub fn sample_peak_db(&self) -> f64 {
        to_db(self.sample_peak as f64)
    }
    pub fn true_peak_db(&self) -> f64 {
        to_db(self.true_peak as f64)
    }
    /// Peak-to-Loudness Ratio (PLR), expressed in LU.
    pub fn peak_to_loudness_ratio_lu(&self) -> f64 {
        self.true_peak_db() - self.lufs
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
        channel_roles: buf.channel_roles.clone(),
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
    if plan.limiter.is_none() {
        let ceil_lin = 10.0_f64.powf(plan.ceiling_db / 20.0);
        if true_peak > 0.0 {
            let max_for_ceil = ceil_lin / true_peak;
            if lin > max_for_ceil {
                lin = max_for_ceil;
            }
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
    if let Some(config) = plan.limiter {
        apply_gain(&mut buf.data, gain);
        let mut limiter =
            TruePeakLimiter::new(buf.sample_rate, buf.channels, plan.ceiling_db, config)
                .expect("validated limiter configuration");
        let mut output = limiter
            .process(&buf.data)
            .expect("AudioBuffer channel layout is internally consistent");
        let tail = limiter.finish();
        for (channel, tail) in output.iter_mut().zip(tail) {
            channel.extend(tail);
        }
        buf.data = output;
        return;
    }
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
            let bext = plan.bwf.then(metadata::blank_bext);
            WavWriter::write_with_options(
                p,
                buf,
                kind,
                plan.dither,
                plan.wav_container,
                bext.as_deref(),
            )
            .map_err(|e| format!("write {}: {e}", p.display()))
        }
        OutputFormat::Flac => {
            let bits = flac_bits(plan.output_kind.unwrap_or(buf.source_kind))?;
            let mut writer =
                FlacStreamWriter::create(p, buf.sample_rate, buf.channels, bits, plan.dither)?;
            writer.write_chunk(&buf.data)?;
            writer.finish()
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
        OutputFormat::Opus => {
            #[cfg(feature = "opus-encoding")]
            {
                let mut writer = crate::opus::OpusStreamWriter::create(
                    p,
                    buf.sample_rate,
                    buf.frames,
                    buf.channels,
                    &buf.channel_roles,
                    plan.mp3_bitrate,
                    analyze(buf).lufs,
                    None,
                )?;
                writer.write_chunk(&buf.data)?;
                writer.finish()
            }
            #[cfg(not(feature = "opus-encoding"))]
            {
                let _ = (buf, plan);
                Err(
                    "Ogg Opus output is unavailable; rebuild with `--features opus-encoding`"
                        .into(),
                )
            }
        }
        OutputFormat::M4a => {
            #[cfg(feature = "aac-encoding")]
            {
                let mut writer = crate::aac::AacStreamWriter::create(
                    p,
                    buf.sample_rate,
                    buf.channels,
                    plan.mp3_bitrate,
                )?;
                writer.write_chunk(&buf.data)?;
                writer.finish()
            }
            #[cfg(not(feature = "aac-encoding"))]
            {
                let _ = (buf, plan);
                Err("AAC/M4A output is unavailable; rebuild with `--features aac-encoding`".into())
            }
        }
    }
}

/// Analyze a file on disk (buffer is dropped after measurement).
pub fn analyze_file<P: AsRef<Path>>(path: P) -> Result<Analysis, String> {
    analyze_file_with_roles(path, None)
}

/// Analyze a file with an optional explicit channel layout.
pub fn analyze_file_with_roles<P: AsRef<Path>>(
    path: P,
    channel_roles: Option<&[ChannelRole]>,
) -> Result<Analysis, String> {
    Ok(analyze_file_range_with_roles(path, channel_roles, 0.0, None, None)?.analysis)
}

/// Analyze an optional source-time range and optionally capture a loudness
/// timeline at the requested interval.
pub fn analyze_file_range_with_roles<P: AsRef<Path>>(
    path: P,
    channel_roles: Option<&[ChannelRole]>,
    start_seconds: f64,
    duration_seconds: Option<f64>,
    timeline_interval_ms: Option<f64>,
) -> Result<TimedAnalysis, String> {
    if !start_seconds.is_finite() || start_seconds < 0.0 {
        return Err("analysis start must be a finite non-negative number".into());
    }
    if duration_seconds.is_some_and(|value| !value.is_finite() || value <= 0.0) {
        return Err("analysis duration must be a finite positive number".into());
    }
    if timeline_interval_ms.is_some_and(|value| !value.is_finite() || value <= 0.0) {
        return Err("timeline interval must be a finite positive number".into());
    }

    const RANGE_COMPLETE: &str = "__forge_analysis_range_complete__";
    let mut analyzer: Option<lufs::StreamingAnalyzer> = None;
    let mut captured_info = None;
    let mut source_frames = 0usize;
    let decoded = decoder::decode_stream(path.as_ref(), |info, chunk| {
        captured_info.get_or_insert_with(|| info.clone());
        if channel_roles.is_none()
            && info.channels > 6
            && info
                .channel_roles
                .iter()
                .all(|role| matches!(role, ChannelRole::Main))
        {
            return Err(format!(
                "{}: ambiguous {}-channel layout; provide --channel-layout",
                path.as_ref().display(),
                info.channels
            ));
        }
        let roles = if let Some(roles) = channel_roles {
            if roles.len() != info.channels as usize {
                return Err(format!(
                    "channel layout has {} channels but input has {}",
                    roles.len(),
                    info.channels
                ));
            }
            roles.to_vec()
        } else {
            info.channel_roles.clone()
        };
        let range_start = (start_seconds * info.sample_rate as f64).round() as usize;
        let range_end = duration_seconds.map(|duration| {
            range_start.saturating_add((duration * info.sample_rate as f64).round() as usize)
        });
        let chunk_start = source_frames;
        let chunk_end = source_frames + chunk.first().map_or(0, Vec::len);
        source_frames = chunk_end;
        if range_end.is_some_and(|end| chunk_start >= end) {
            return Err(RANGE_COMPLETE.into());
        }
        let selected_start = range_start.saturating_sub(chunk_start);
        let selected_end = range_end
            .map_or(chunk_end, |end| end.min(chunk_end))
            .saturating_sub(chunk_start);
        if selected_start < selected_end {
            let selected = chunk
                .iter()
                .map(|channel| channel[selected_start..selected_end].to_vec())
                .collect::<Vec<_>>();
            let interval_frames = timeline_interval_ms.map(|milliseconds| {
                ((info.sample_rate as f64 * milliseconds / 1_000.0).round() as usize).max(1)
            });
            let meter = analyzer.get_or_insert_with(|| {
                lufs::StreamingAnalyzer::with_timeline_interval(
                    info.sample_rate,
                    roles,
                    interval_frames,
                )
            });
            meter.process(&selected)?;
        }
        if range_end.is_some_and(|end| chunk_end >= end) {
            Err(RANGE_COMPLETE.into())
        } else {
            Ok(())
        }
    });
    let info = match decoded {
        Ok(info) => info,
        Err(error) if error == RANGE_COMPLETE => {
            captured_info.ok_or_else(|| format!("{}: no audio decoded", path.as_ref().display()))?
        }
        Err(error) => return Err(error),
    };
    let measured = analyzer
        .ok_or_else(|| {
            format!(
                "{}: requested analysis range contains no audio",
                path.as_ref().display()
            )
        })?
        .finish();
    let mut timeline = measured.timeline;
    let actual_start_seconds = ((start_seconds * info.sample_rate as f64).round() as usize) as f64
        / info.sample_rate as f64;
    for point in &mut timeline {
        point.start_seconds += actual_start_seconds;
        point.end_seconds += actual_start_seconds;
    }
    Ok(TimedAnalysis {
        analysis: Analysis {
            sample_rate: info.sample_rate,
            channels: info.channels,
            channel_roles: channel_roles
                .map(ToOwned::to_owned)
                .unwrap_or(info.channel_roles),
            frames: measured.frames,
            kind: info.source_kind,
            lufs: measured.ebu.integrated_lufs,
            max_momentary_lufs: measured.ebu.max_momentary_lufs,
            max_short_term_lufs: measured.ebu.max_short_term_lufs,
            loudness_range_lu: measured.ebu.loudness_range_lu,
            rms_db: measured.rms_db,
            sample_peak: measured.sample_peak,
            true_peak: measured.true_peak,
            loudness_blocks: measured.ebu.gating_blocks,
        },
        timeline,
    })
}

/// Load and validate non-overlapping source-time regions used as dialogue
/// anchors. JSON and TOML files use a top-level `ranges` array.
pub fn load_dialogue_ranges(path: &Path) -> Result<Vec<DialogueRange>, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("read dialogue ranges {}: {error}", path.display()))?;
    let file: DialogueRangeFile = match path.extension().and_then(|value| value.to_str()) {
        Some("json") => serde_json::from_str(&text)
            .map_err(|error| format!("parse dialogue ranges {}: {error}", path.display()))?,
        Some("toml") => toml::from_str(&text)
            .map_err(|error| format!("parse dialogue ranges {}: {error}", path.display()))?,
        _ => return Err("dialogue ranges must use a .json or .toml extension".into()),
    };
    validate_dialogue_ranges(&file.ranges)?;
    Ok(file.ranges)
}

pub fn validate_dialogue_ranges(ranges: &[DialogueRange]) -> Result<(), String> {
    if ranges.is_empty() {
        return Err("dialogue ranges must contain at least one range".into());
    }
    let mut previous_end = 0.0;
    for (index, range) in ranges.iter().enumerate() {
        if !range.start_seconds.is_finite() || range.start_seconds < 0.0 {
            return Err(format!(
                "dialogue range {} start must be a finite non-negative number",
                index + 1
            ));
        }
        if !range.duration_seconds.is_finite() || range.duration_seconds <= 0.0 {
            return Err(format!(
                "dialogue range {} duration must be a finite positive number",
                index + 1
            ));
        }
        let range_end = range.start_seconds + range.duration_seconds;
        if !range_end.is_finite() {
            return Err(format!("dialogue range {} end is not finite", index + 1));
        }
        if index > 0 && range.start_seconds < previous_end {
            return Err(format!(
                "dialogue range {} overlaps or is not sorted",
                index + 1
            ));
        }
        previous_end = range_end;
    }
    Ok(())
}

/// Measure explicit dialogue/anchor regions for ATSC A/85:2026-07. Dialogue
/// selection is the gate; the selected K-weighted energy is averaged without
/// the BS.1770-2+ relative-level gate, as required by A/85 Annex M.
pub fn analyze_dialogue_ranges_with_roles<P: AsRef<Path>>(
    path: P,
    channel_roles: Option<&[ChannelRole]>,
    ranges: &[DialogueRange],
) -> Result<DialogueMeasurement, String> {
    analyze_dialogue_ranges_for_standard_with_roles(
        path,
        channel_roles,
        ranges,
        DialogueStandard::AtscA85,
        DialogueSource::Mix,
    )
}

pub fn analyze_dialogue_ranges_for_standard_with_roles<P: AsRef<Path>>(
    path: P,
    channel_roles: Option<&[ChannelRole]>,
    ranges: &[DialogueRange],
    standard: DialogueStandard,
    source: DialogueSource,
) -> Result<DialogueMeasurement, String> {
    validate_dialogue_ranges(ranges)?;
    let mut analyzers = (0..ranges.len()).map(|_| None).collect::<Vec<_>>();
    let mut source_frames = 0usize;
    let info = decoder::decode_stream(path.as_ref(), |info, chunk| {
        if channel_roles.is_none()
            && info.channels > 6
            && info
                .channel_roles
                .iter()
                .all(|role| matches!(role, ChannelRole::Main))
        {
            return Err(format!(
                "{}: ambiguous {}-channel layout; provide --channel-layout",
                path.as_ref().display(),
                info.channels
            ));
        }
        let roles = if let Some(roles) = channel_roles {
            if roles.len() != info.channels as usize {
                return Err(format!(
                    "channel layout has {} channels but input has {}",
                    roles.len(),
                    info.channels
                ));
            }
            roles
        } else {
            &info.channel_roles
        };
        if source == DialogueSource::Center && info.channels < 3 {
            return Err("center dialogue source requires an input with a centre channel".into());
        }
        let chunk_start = source_frames;
        let chunk_end = source_frames + chunk.first().map_or(0, Vec::len);
        source_frames = chunk_end;
        for (range, analyzer) in ranges.iter().zip(&mut analyzers) {
            let range_start = (range.start_seconds * info.sample_rate as f64).round() as usize;
            let range_end =
                range_start.saturating_add(
                    (range.duration_seconds * info.sample_rate as f64).round() as usize,
                );
            let overlap_start = chunk_start.max(range_start);
            let overlap_end = chunk_end.min(range_end);
            if overlap_start < overlap_end {
                let selected_start = overlap_start - chunk_start;
                let selected_end = overlap_end - chunk_start;
                let selected = if source == DialogueSource::Center {
                    vec![chunk[2][selected_start..selected_end].to_vec()]
                } else {
                    chunk
                        .iter()
                        .map(|channel| channel[selected_start..selected_end].to_vec())
                        .collect::<Vec<_>>()
                };
                let selected_roles = if source == DialogueSource::Center {
                    vec![ChannelRole::Main]
                } else {
                    roles.to_vec()
                };
                analyzer
                    .get_or_insert_with(|| {
                        lufs::StreamingAnalyzer::new(info.sample_rate, selected_roles)
                    })
                    .process(&selected)?;
            }
        }
        Ok(())
    })?;
    let mut weighted_energy = 0.0;
    let mut gating_blocks = Vec::new();
    let mut frames = 0usize;
    for (index, analyzer) in analyzers.into_iter().enumerate() {
        let measured = analyzer
            .ok_or_else(|| {
                format!(
                    "{}: dialogue range {} contains no audio",
                    path.as_ref().display(),
                    index + 1
                )
            })?
            .finish();
        weighted_energy += measured.weighted_mean_square * measured.frames as f64;
        gating_blocks.extend(measured.ebu.gating_blocks);
        frames += measured.frames;
    }
    if frames == 0 {
        return Err("dialogue ranges contain no audio".into());
    }
    let (loudness, standard_name, method) = match standard {
        DialogueStandard::AtscA85 => (
            lufs::ungated_lufs(weighted_energy / frames as f64),
            "ATSC A/85:2026-07",
            "BS.1770-1 K-weighting + explicit dialogue gate; no relative-level gate",
        ),
        DialogueStandard::EbuR128S4 => (
            if gating_blocks.is_empty() {
                return Err(
                    "EBU dialogue ranges contain no complete 400 ms loudness blocks".into(),
                );
            } else {
                lufs::gated_lufs(&gating_blocks)
            },
            "EBU R 128 s4",
            "BS.1770-5 K-weighting + explicit dialogue selection + absolute/relative gating",
        ),
    };
    Ok(DialogueMeasurement {
        lufs: loudness,
        duration_seconds: frames as f64 / info.sample_rate as f64,
        range_count: ranges.len(),
        standard: standard_name,
        method,
        source,
    })
}

/// Re-decode an encoded output and compare it with the level predicted from
/// the source analysis and applied gain.
pub fn verify_file<P: AsRef<Path>>(
    output: P,
    source: &Analysis,
    gain: f32,
    plan: &Plan,
    tolerance: f64,
) -> Result<Verification, String> {
    if !tolerance.is_finite() || tolerance < 0.0 {
        return Err("verification tolerance must be a finite non-negative number".into());
    }
    let output = analyze_file(output)?;
    Ok(verify_analysis(&output, source, gain, plan, tolerance))
}

/// Verify an encoded output against a fixed intended level.
///
/// Unlike [`verify_file`], the expected level does not move when a subsequent
/// encoding pass uses a corrected gain.
pub fn verify_file_at_level<P: AsRef<Path>>(
    output: P,
    expected_level: f64,
    plan: &Plan,
    tolerance: f64,
) -> Result<Verification, String> {
    if !tolerance.is_finite() || tolerance < 0.0 {
        return Err("verification tolerance must be a finite non-negative number".into());
    }
    verify_file_at_level_with_roles(output.as_ref(), expected_level, plan, tolerance, None)
}

fn verify_file_at_level_with_roles(
    output: &Path,
    expected_level: f64,
    plan: &Plan,
    tolerance: f64,
    channel_roles: Option<&[ChannelRole]>,
) -> Result<Verification, String> {
    let output = analyze_file_with_roles(output, channel_roles)?;
    Ok(verify_analysis_at_level(
        &output,
        expected_level,
        plan,
        tolerance,
    ))
}

pub fn verify_analysis(
    output: &Analysis,
    source: &Analysis,
    gain: f32,
    plan: &Plan,
    tolerance: f64,
) -> Verification {
    let gain_db = 20.0 * (gain as f64).log10();
    let source_level = analysis_level(source, plan.mode);
    let expected_level = source_level + gain_db;
    verify_analysis_at_level(output, expected_level, plan, tolerance)
}

fn verify_analysis_at_level(
    output: &Analysis,
    expected_level: f64,
    plan: &Plan,
    tolerance: f64,
) -> Verification {
    let actual_level = analysis_level(output, plan.mode);
    let deviation = level_deviation(expected_level, actual_level);
    Verification {
        output: output.clone(),
        expected_level,
        actual_level,
        deviation,
        level_ok: deviation <= tolerance,
        true_peak_ok: output.true_peak_db() <= plan.ceiling_db + tolerance,
    }
}

fn analysis_level(analysis: &Analysis, mode: Mode) -> f64 {
    match mode {
        Mode::Lufs => analysis.lufs,
        Mode::Peak => analysis.sample_peak_db(),
        Mode::Rms => analysis.rms_db,
    }
}

fn level_deviation(expected: f64, actual: f64) -> f64 {
    if expected == actual {
        0.0
    } else if expected.is_finite() && actual.is_finite() {
        (actual - expected).abs()
    } else {
        f64::INFINITY
    }
}

/// Normalize a single file in one pass (load, analyze, gain, write).
pub fn normalize_one<P: AsRef<Path>>(
    input: P,
    output: P,
    plan: &Plan,
    format: OutputFormat,
) -> Result<(Analysis, f32), String> {
    normalize_one_with_roles(input, output, plan, format, None)
}

pub fn normalize_one_with_roles<P: AsRef<Path>>(
    input: P,
    output: P,
    plan: &Plan,
    format: OutputFormat,
    channel_roles: Option<&[ChannelRole]>,
) -> Result<(Analysis, f32), String> {
    let an = analyze_file_with_roles(input.as_ref(), channel_roles)?;
    let gain = compute_gain(&an, plan);
    let staged = AtomicOutput::new(output.as_ref())?;
    normalize_stream(input.as_ref(), staged.path(), &an, gain, plan, format, None)?;
    finalize_metadata(
        input.as_ref(),
        staged.path(),
        format,
        an.lufs + gain_db(gain),
        None,
        plan,
    )?;
    staged.commit()?;
    Ok((an, gain))
}

/// Normalize, re-decode, and automatically compensate for post-encode level
/// drift or a true-peak overshoot. Every correction is rendered again from the
/// original input, so lossy artifacts are never compounded across retries.
pub fn normalize_one_corrected<P: AsRef<Path>>(
    input: P,
    output: P,
    plan: &Plan,
    format: OutputFormat,
    tolerance: f64,
    max_retries: usize,
) -> Result<CorrectedNormalization, String> {
    normalize_one_corrected_with_roles(input, output, plan, format, tolerance, max_retries, None)
}

pub fn normalize_one_corrected_with_roles<P: AsRef<Path>>(
    input: P,
    output: P,
    plan: &Plan,
    format: OutputFormat,
    tolerance: f64,
    max_retries: usize,
    channel_roles: Option<&[ChannelRole]>,
) -> Result<CorrectedNormalization, String> {
    if !tolerance.is_finite() || tolerance < 0.0 {
        return Err("verification tolerance must be a finite non-negative number".into());
    }
    let input = input.as_ref();
    let output = output.as_ref();
    let source = analyze_file_with_roles(input, channel_roles)?;
    let mut gain = compute_gain(&source, plan);
    let expected_level = analysis_level(&source, plan.mode) + gain_db(gain);
    let staged = AtomicOutput::new(output)?;

    for attempt in 0..=max_retries {
        normalize_stream(input, staged.path(), &source, gain, plan, format, None)?;
        let verification = verify_file_at_level_with_roles(
            staged.path(),
            expected_level,
            plan,
            tolerance,
            channel_roles,
        )?;
        if verification.passed() {
            finalize_metadata(
                input,
                staged.path(),
                format,
                verification.output.lufs,
                None,
                plan,
            )?;
            staged.commit()?;
            return Ok(CorrectedNormalization {
                source,
                gain,
                verification,
                attempts: attempt + 1,
            });
        }
        if attempt == max_retries {
            return Err(format!(
                "post-encode verification failed after {} encoding pass(es)",
                attempt + 1
            ));
        }
        gain = corrected_gain(gain, &verification, plan)?;
    }
    unreachable!("the inclusive retry loop always returns")
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
    normalize_album_with_roles(inputs, outputs, plan, formats, None)
}

pub fn normalize_album_with_roles(
    inputs: &[PathBuf],
    outputs: &[PathBuf],
    plan: &Plan,
    formats: &[OutputFormat],
    channel_roles: Option<&[ChannelRole]>,
) -> Result<Vec<(Analysis, f32)>, String> {
    let analyses: Vec<Analysis> = inputs
        .iter()
        .map(|path| analyze_file_with_roles(path, channel_roles))
        .collect::<Result<_, _>>()?;
    let gain = album_gain(&analyses, plan);
    let album_output_lufs = album_lufs(&analyses) + gain_db(gain);
    let staged: Vec<AtomicOutput> = outputs
        .iter()
        .map(|output| AtomicOutput::new(output))
        .collect::<Result<_, _>>()?;
    let mut results = Vec::with_capacity(inputs.len());
    for (i, (input, output)) in inputs.iter().zip(staged.iter()).enumerate() {
        let fmt = formats.get(i).copied().unwrap_or(OutputFormat::Wav);
        normalize_stream(
            input,
            output.path(),
            &analyses[i],
            gain,
            plan,
            fmt,
            Some(album_output_lufs),
        )?;
        finalize_metadata(
            input,
            output.path(),
            fmt,
            analyses[i].lufs + gain_db(gain),
            Some(album_output_lufs),
            plan,
        )?;
        results.push((analyses[i].clone(), gain));
    }
    for output in staged {
        output.commit()?;
    }
    Ok(results)
}

/// Album normalization with a shared gain and iterative post-encode
/// correction. Corrections use the decoded album loudness and the worst
/// decoded true peak while preserving one common gain for every track.
pub fn normalize_album_corrected(
    inputs: &[PathBuf],
    outputs: &[PathBuf],
    plan: &Plan,
    formats: &[OutputFormat],
    tolerance: f64,
    max_retries: usize,
) -> Result<CorrectedAlbumNormalization, String> {
    normalize_album_corrected_with_roles(
        inputs,
        outputs,
        plan,
        formats,
        tolerance,
        max_retries,
        None,
    )
}

pub fn normalize_album_corrected_with_roles(
    inputs: &[PathBuf],
    outputs: &[PathBuf],
    plan: &Plan,
    formats: &[OutputFormat],
    tolerance: f64,
    max_retries: usize,
    channel_roles: Option<&[ChannelRole]>,
) -> Result<CorrectedAlbumNormalization, String> {
    if inputs.is_empty() {
        return Err("cannot correct an empty album".into());
    }
    if inputs.len() != outputs.len() {
        return Err("album input/output count mismatch".into());
    }
    if !tolerance.is_finite() || tolerance < 0.0 {
        return Err("verification tolerance must be a finite non-negative number".into());
    }
    let sources: Vec<Analysis> = inputs
        .iter()
        .map(|path| analyze_file_with_roles(path, channel_roles))
        .collect::<Result<_, _>>()?;
    let mut gain = album_gain(&sources, plan);
    let expected_album_lufs = album_lufs(&sources) + gain_db(gain);
    let expected_track_levels: Vec<f64> = sources
        .iter()
        .map(|source| analysis_level(source, plan.mode) + gain_db(gain))
        .collect();
    let staged: Vec<AtomicOutput> = outputs
        .iter()
        .map(|output| AtomicOutput::new(output))
        .collect::<Result<_, _>>()?;
    let staged_paths: Vec<PathBuf> = staged
        .iter()
        .map(|output| output.path().to_owned())
        .collect();

    for attempt in 0..=max_retries {
        for (index, (input, output)) in inputs.iter().zip(&staged_paths).enumerate() {
            let format = formats.get(index).copied().unwrap_or(OutputFormat::Wav);
            normalize_stream(
                input,
                output,
                &sources[index],
                gain,
                plan,
                format,
                Some(album_lufs(&sources) + gain_db(gain)),
            )?;
        }
        let decoded: Vec<Analysis> = staged_paths
            .iter()
            .map(|path| analyze_file_with_roles(path, channel_roles))
            .collect::<Result<_, _>>()?;
        let actual_album_lufs = album_lufs(&decoded);
        let verifications: Vec<Verification> = decoded
            .iter()
            .zip(&expected_track_levels)
            .map(|(output, expected)| verify_analysis_at_level(output, *expected, plan, tolerance))
            .collect();
        let album_deviation = level_deviation(expected_album_lufs, actual_album_lufs);
        let worst_true_peak = decoded
            .iter()
            .map(Analysis::true_peak_db)
            .fold(f64::NEG_INFINITY, f64::max);
        let album_passed = album_deviation <= tolerance
            && worst_true_peak <= plan.ceiling_db + tolerance
            && verifications.iter().all(Verification::passed);
        if album_passed {
            for (index, (input, output)) in inputs.iter().zip(&staged_paths).enumerate() {
                let format = formats.get(index).copied().unwrap_or(OutputFormat::Wav);
                finalize_metadata(
                    input,
                    output,
                    format,
                    decoded[index].lufs,
                    Some(actual_album_lufs),
                    plan,
                )?;
            }
            for output in staged {
                output.commit()?;
            }
            return Ok(CorrectedAlbumNormalization {
                sources,
                gain,
                verifications,
                expected_album_lufs,
                actual_album_lufs,
                attempts: attempt + 1,
            });
        }
        if attempt == max_retries {
            return Err(format!(
                "post-encode album verification failed after {} encoding pass(es)",
                attempt + 1
            ));
        }
        let album_verification = Verification {
            output: Analysis {
                true_peak: decoded
                    .iter()
                    .map(|analysis| analysis.true_peak)
                    .fold(0.0_f32, f32::max),
                lufs: actual_album_lufs,
                ..decoded
                    .first()
                    .cloned()
                    .ok_or_else(|| "cannot correct an empty album".to_string())?
            },
            expected_level: expected_album_lufs,
            actual_level: actual_album_lufs,
            deviation: album_deviation,
            level_ok: album_deviation <= tolerance,
            true_peak_ok: worst_true_peak <= plan.ceiling_db + tolerance,
        };
        gain = corrected_gain(gain, &album_verification, plan)?;
    }
    unreachable!("the inclusive retry loop always returns")
}

fn gain_db(gain: f32) -> f64 {
    20.0 * (gain as f64).log10()
}

fn finalize_metadata(
    input: &Path,
    output: &Path,
    format: OutputFormat,
    _track_lufs: f64,
    _album_lufs: Option<f64>,
    plan: &Plan,
) -> Result<(), String> {
    metadata::copy_metadata(input, output)?;
    if format == OutputFormat::Wav && plan.bwf {
        let measured = analyze_file(output)?;
        metadata::update_bwf_loudness(output, &measured)?;
    }
    if format == OutputFormat::Opus {
        #[cfg(feature = "opus-encoding")]
        {
            crate::opus::rewrite_r128_tags(output, _track_lufs, _album_lufs)?;
        }
    }
    if format == OutputFormat::M4a {
        let measured = analyze_file(output)?;
        metadata::write_replaygain(output, measured.lufs, measured.true_peak, None)?;
    }
    Ok(())
}

fn corrected_gain(
    current_gain: f32,
    verification: &Verification,
    plan: &Plan,
) -> Result<f32, String> {
    let level_adjustment = if verification.expected_level == verification.actual_level {
        0.0
    } else if verification.expected_level.is_finite() && verification.actual_level.is_finite() {
        verification.expected_level - verification.actual_level
    } else {
        return Err("cannot automatically correct a non-finite output level".into());
    };
    let peak_adjustment = if verification.output.true_peak > 0.0 {
        plan.ceiling_db - verification.output.true_peak_db()
    } else {
        f64::INFINITY
    };
    let adjustment_db = level_adjustment.min(peak_adjustment);
    if !adjustment_db.is_finite() {
        return Err("cannot automatically correct output gain".into());
    }
    let mut corrected = current_gain as f64 * 10.0_f64.powf(adjustment_db / 20.0);
    if let Some(max_gain_db) = plan.max_gain_db {
        corrected = corrected.min(10.0_f64.powf(max_gain_db / 20.0));
    }
    if !corrected.is_finite() || corrected <= 0.0 {
        return Err("automatic correction produced an invalid gain".into());
    }
    Ok(corrected as f32)
}

fn normalize_stream(
    input: &Path,
    output: &Path,
    analysis: &Analysis,
    gain: f32,
    plan: &Plan,
    format: OutputFormat,
    _opus_album_lufs: Option<f64>,
) -> Result<(), String> {
    let ceiling = 10.0_f64.powf(plan.ceiling_db / 20.0) as f32;
    match format {
        OutputFormat::Wav => {
            let kind = plan.output_kind.unwrap_or(analysis.kind);
            let metadata_chunks = if plan.bwf {
                metadata::prepare_broadcast_chunks(input)?
            } else {
                Vec::new()
            };
            let mut writer = WavStreamWriter::create_with_metadata(
                output,
                analysis.sample_rate,
                analysis.channels,
                analysis.frames,
                kind,
                plan.dither,
                plan.wav_container,
                &analysis.channel_roles,
                &metadata_chunks,
            )
            .map_err(|error| format!("write {}: {error}", output.display()))?;
            process_normalized_stream(input, analysis, gain, ceiling, plan, |planar| {
                writer
                    .write_chunk(planar)
                    .map_err(|error| format!("write {}: {error}", output.display()))
            })?;
            writer
                .finish()
                .map_err(|error| format!("write {}: {error}", output.display()))
        }
        OutputFormat::Flac => {
            let bits = flac_bits(plan.output_kind.unwrap_or(analysis.kind))?;
            let mut writer = FlacStreamWriter::create(
                output,
                analysis.sample_rate,
                analysis.channels,
                bits,
                plan.dither,
            )?;
            process_normalized_stream(input, analysis, gain, ceiling, plan, |planar| {
                writer.write_chunk(planar)
            })?;
            writer.finish()
        }
        OutputFormat::Mp3 => {
            #[cfg(feature = "mp3-encoding")]
            {
                let mut writer = mp3enc::Mp3StreamWriter::create(
                    output,
                    analysis.sample_rate,
                    analysis.channels,
                    plan.mp3_bitrate,
                    plan.mp3_quality,
                )?;
                process_normalized_stream(input, analysis, gain, ceiling, plan, |planar| {
                    writer.write_chunk(planar)
                })?;
                writer.finish()
            }
            #[cfg(not(feature = "mp3-encoding"))]
            {
                let _ = (input, output, analysis, gain, plan, ceiling);
                Err("MP3 output is unavailable; rebuild with `--features mp3-encoding`".into())
            }
        }
        OutputFormat::Opus => {
            #[cfg(feature = "opus-encoding")]
            {
                let output_lufs = analysis.lufs + gain_db(gain);
                let mut writer = crate::opus::OpusStreamWriter::create(
                    output,
                    analysis.sample_rate,
                    analysis.frames,
                    analysis.channels,
                    &analysis.channel_roles,
                    plan.mp3_bitrate,
                    output_lufs,
                    _opus_album_lufs,
                )?;
                process_normalized_stream(input, analysis, gain, ceiling, plan, |planar| {
                    writer.write_chunk(planar)
                })?;
                writer.finish()
            }
            #[cfg(not(feature = "opus-encoding"))]
            {
                let _ = (input, output, analysis, gain, plan, ceiling);
                Err(
                    "Ogg Opus output is unavailable; rebuild with `--features opus-encoding`"
                        .into(),
                )
            }
        }
        OutputFormat::M4a => {
            #[cfg(feature = "aac-encoding")]
            {
                let mut writer = crate::aac::AacStreamWriter::create(
                    output,
                    analysis.sample_rate,
                    analysis.channels,
                    plan.mp3_bitrate,
                )?;
                process_normalized_stream(input, analysis, gain, ceiling, plan, |planar| {
                    writer.write_chunk(planar)
                })?;
                writer.finish()
            }
            #[cfg(not(feature = "aac-encoding"))]
            {
                let _ = (input, output, analysis, gain, plan, ceiling);
                Err("AAC/M4A output is unavailable; rebuild with `--features aac-encoding`".into())
            }
        }
    }
}

fn process_normalized_stream(
    input: &Path,
    analysis: &Analysis,
    gain: f32,
    ceiling: f32,
    plan: &Plan,
    mut write: impl FnMut(&[Vec<f32>]) -> Result<(), String>,
) -> Result<(), String> {
    let mut limiter = plan
        .limiter
        .map(|config| {
            TruePeakLimiter::new(
                analysis.sample_rate,
                analysis.channels,
                plan.ceiling_db,
                config,
            )
        })
        .transpose()?;
    decoder::decode_stream(input, |_, planar| {
        if let Some(limiter) = limiter.as_mut() {
            apply_gain(planar, gain);
            let output = limiter.process(planar)?;
            if output.first().is_some_and(|channel| !channel.is_empty()) {
                write(&output)?;
            }
        } else {
            gain_chunk(planar, gain, ceiling);
            write(planar)?;
        }
        Ok(())
    })?;
    if let Some(limiter) = limiter {
        let tail = limiter.finish();
        if tail.first().is_some_and(|channel| !channel.is_empty()) {
            write(&tail)?;
        }
    }
    Ok(())
}

fn flac_bits(kind: PcmKind) -> Result<u16, String> {
    match kind {
        PcmKind::U8 | PcmKind::S16 => Ok(16),
        PcmKind::S24 | PcmKind::S32 | PcmKind::F32 | PcmKind::F64 => Ok(24),
    }
}

fn apply_gain(planar: &mut [Vec<f32>], gain: f32) {
    for channel in planar {
        simd::apply_gain(channel, gain);
    }
}

fn gain_chunk(planar: &mut [Vec<f32>], gain: f32, ceiling: f32) {
    for channel in planar {
        simd::apply_gain(channel, gain);
        simd::hard_clip(channel, ceiling);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::default_channel_roles;

    fn analysis(level: f64, true_peak_db: f64) -> Analysis {
        Analysis {
            sample_rate: 48_000,
            channels: 2,
            channel_roles: crate::wav::default_channel_roles(2),
            frames: 48_000,
            kind: PcmKind::F32,
            lufs: level,
            max_momentary_lufs: level,
            max_short_term_lufs: level,
            loudness_range_lu: 0.0,
            rms_db: level,
            sample_peak: 10.0_f64.powf(true_peak_db / 20.0) as f32,
            true_peak: 10.0_f64.powf(true_peak_db / 20.0) as f32,
            loudness_blocks: Vec::new(),
        }
    }

    fn plan() -> Plan {
        Plan {
            mode: Mode::Lufs,
            target_lufs: -16.0,
            target_peak_db: -1.0,
            target_rms_db: -18.0,
            ceiling_db: -1.0,
            max_gain_db: None,
            dither: false,
            output_kind: None,
            mp3_bitrate: 192,
            mp3_quality: 2,
            limiter: None,
            wav_container: WavContainer::Auto,
            bwf: false,
        }
    }

    #[test]
    fn corrected_gain_compensates_a_quiet_encoded_output() {
        let output = analysis(-16.8, -3.0);
        let verification = verify_analysis_at_level(&output, -16.0, &plan(), 0.1);
        let corrected = corrected_gain(1.0, &verification, &plan()).unwrap();

        assert!((gain_db(corrected) - 0.8).abs() < 1e-5);
    }

    #[test]
    fn corrected_gain_prioritizes_true_peak_ceiling() {
        let output = analysis(-16.4, -0.2);
        let verification = verify_analysis_at_level(&output, -16.0, &plan(), 0.1);
        let corrected = corrected_gain(1.0, &verification, &plan()).unwrap();

        assert!((gain_db(corrected) - (-0.8)).abs() < 1e-5);
    }

    #[test]
    fn dialogue_ranges_reject_empty_unsorted_and_overlapping_regions() {
        assert!(validate_dialogue_ranges(&[]).is_err());
        assert!(validate_dialogue_ranges(&[
            DialogueRange {
                start_seconds: 2.0,
                duration_seconds: 2.0,
            },
            DialogueRange {
                start_seconds: 3.0,
                duration_seconds: 1.0,
            },
        ])
        .is_err());
        assert!(validate_dialogue_ranges(&[DialogueRange {
            start_seconds: 0.0,
            duration_seconds: f64::NAN,
        }])
        .is_err());
    }

    #[test]
    fn lra_stability_requires_sixty_seconds() {
        let mut measured = analysis(-23.0, -1.0);
        measured.frames = 48_000 * 59;
        assert!(!measured.loudness_range_stable());

        measured.frames = 48_000 * 60;
        assert!(measured.loudness_range_stable());
    }

    #[test]
    fn corrected_normalization_reuses_the_original_source() {
        let frames = 48_000 * 4;
        let data = (0..frames)
            .map(|frame| {
                0.1 * (2.0 * std::f32::consts::PI * 1_000.0 * frame as f32 / 48_000.0).sin()
            })
            .collect::<Vec<_>>();
        let buffer = AudioBuffer {
            sample_rate: 48_000,
            channels: 1,
            frames,
            data: vec![data],
            channel_roles: default_channel_roles(1),
            source_kind: PcmKind::F32,
        };
        let input = std::env::temp_dir().join("forge_corrected_original.wav");
        let output = std::env::temp_dir().join("forge_corrected_output.wav");
        WavWriter::write(&input, &buffer, PcmKind::F32, false).unwrap();

        let result =
            normalize_one_corrected(&input, &output, &plan(), OutputFormat::Wav, 0.01, 2).unwrap();

        assert!(result.verification.passed());
        assert_eq!(result.attempts, 1);
        let _ = std::fs::remove_file(input);
        let _ = std::fs::remove_file(output);
    }
}
