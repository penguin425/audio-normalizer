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

pub use crate::analysis::{analyze, Analysis, AnalysisEngine};
use crate::atomic::AtomicOutput;
use crate::bound_analysis::{BoundAnalysis, BoundAnalysisError};
use crate::channel_layout::ChannelLayoutDescriptor;
use crate::decoder::{self, InputDescriptor, InputDescriptorOptions};
use crate::downmix;
use crate::dsp::limiter::{LimiterConfig, LimiterStatistics, TruePeakLimiter};
use crate::dsp::resample::{ResampleQuality, SampleRateConverter};
use crate::dsp::sum::CompensatedSum;
use crate::dsp::{convert, lufs, simd};
use crate::flacenc::FlacStreamWriter;
use crate::metadata;
#[cfg(feature = "mp3-encoding")]
use crate::mp3enc;
use crate::pcm_spool::PcmSpool;
use crate::stable_input::{paths_alias_if_existing, StableInput, StableInputOptions};
use crate::wav::{
    AudioBuffer, ChannelRole, PcmKind, WavContainer, WavStreamWriter, WavWriter,
    MAX_DECODE_SAMPLE_RATE_HZ, MIN_DECODE_SAMPLE_RATE_HZ,
};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError};
use std::sync::Arc;

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
    Alac,
    Vorbis,
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
    /// Output-domain sample rate and converter quality.
    pub output_sample_rate: Option<u32>,
    pub resample_quality: ResampleQuality,
}

impl Plan {
    /// Validate plan-wide numeric invariants without reading an input or
    /// creating an output.
    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("target_lufs", self.target_lufs),
            ("target_peak_db", self.target_peak_db),
            ("target_rms_db", self.target_rms_db),
            ("ceiling_db", self.ceiling_db),
        ] {
            if !value.is_finite() {
                return Err(format!("normalization plan {name} must be finite"));
            }
        }
        validate_representable_linear_db("ceiling_db", self.ceiling_db)?;
        if let Some(max_gain_db) = self.max_gain_db {
            if !max_gain_db.is_finite() {
                return Err("normalization plan max_gain_db must be finite".into());
            }
            validate_representable_linear_db("max_gain_db", max_gain_db)?;
        }
        if let Some(limiter) = self.limiter {
            if !limiter.lookahead_ms.is_finite() || limiter.lookahead_ms < 1.0 {
                return Err(
                    "normalization plan limiter look-ahead must be finite and >= 1 ms".into(),
                );
            }
            if !limiter.release_ms.is_finite() || limiter.release_ms <= 0.0 {
                return Err("normalization plan limiter release must be finite and > 0 ms".into());
            }
            let maximum_lookahead_frames =
                MAX_DECODE_SAMPLE_RATE_HZ as f64 * limiter.lookahead_ms / 1_000.0;
            if !maximum_lookahead_frames.is_finite()
                || maximum_lookahead_frames >= usize::MAX as f64
            {
                return Err(
                    "normalization plan limiter look-ahead exceeds the supported range".into(),
                );
            }
        }
        validate_plan_output_sample_rate(self)
    }

    /// Validate the settings used by one selected output format without
    /// reading an input, starting an encoder, or creating an output.
    pub fn validate_for_format(&self, format: OutputFormat) -> Result<(), String> {
        self.validate_format_request(format)?;
        validate_output_encoder_available(format)
    }

    /// Validate a selected output format's numeric request without requiring
    /// that its optional encoder is present in this build.
    pub fn validate_format_request(&self, format: OutputFormat) -> Result<(), String> {
        self.validate()?;
        validate_plan_format_settings(self, format, self.output_sample_rate, None)
    }
}

/// Measurements captured from the exact float signal passed to an encoder.
///
/// These values intentionally describe the signal before PCM quantization or
/// lossy encoding, allowing a decoded output to be compared with the render
/// that the normalization engine actually intended.
#[derive(Debug, Clone)]
pub struct RenderStatistics {
    pub intended: Analysis,
    pub input_full_scale_exceeding_samples: u64,
    pub post_gain_full_scale_exceeding_samples: u64,
    pub post_gain_ceiling_exceeding_samples: u64,
    pub protected_full_scale_exceeding_samples: u64,
    pub limiter: Option<LimiterStatistics>,
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

#[derive(Debug, Clone, Serialize)]
pub struct DetectedDialogueRange {
    pub start_seconds: f64,
    pub duration_seconds: f64,
    pub confidence: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DialogueDetectionFrame {
    pub start_seconds: f64,
    pub duration_seconds: f64,
    pub rms_dbfs: f64,
    pub adaptive_noise_floor_dbfs: f64,
    pub signal_to_noise_db: f64,
    pub center_or_mid_focus: f64,
    pub zero_crossing_rate: f64,
    pub speech_band_energy_ratio: f64,
    pub amplitude_modulation_db: f64,
    pub periodicity: f64,
    pub confidence: f64,
    pub selected: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DialogueDetection {
    pub detector: &'static str,
    pub detector_version: &'static str,
    pub threshold: f64,
    pub window_seconds: f64,
    pub features: Vec<&'static str>,
    pub frames: Vec<DialogueDetectionFrame>,
    pub ranges: Vec<DetectedDialogueRange>,
}

impl DialogueDetection {
    pub fn measurement_ranges(&self) -> Vec<DialogueRange> {
        self.ranges
            .iter()
            .map(|range| DialogueRange {
                start_seconds: range.start_seconds,
                duration_seconds: range.duration_seconds,
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct DownmixMeasurement {
    pub analysis: Analysis,
    pub method: &'static str,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmPresentationMap {
    pub presentations: Vec<AdmPresentationSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmPresentationSpec {
    pub id: String,
    pub name: String,
    /// One-based input channel numbers.
    pub channels: Vec<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AdmPresentationMeasurement {
    pub id: String,
    pub name: String,
    pub channels: Vec<usize>,
    pub integrated_lufs: f64,
    pub true_peak_dbtp: f64,
    pub render_method: &'static str,
    pub referenced_by_axml: bool,
}

#[derive(Debug, Clone)]
pub struct AdmQcResult {
    pub axml_present: bool,
    pub chna_present: bool,
    pub presentations: Vec<AdmPresentationMeasurement>,
    pub passed: bool,
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
    pub render: RenderStatistics,
    /// Number of encoding passes, including the initial pass.
    pub attempts: usize,
}

/// Measurements produced by one completed normalization render.
///
/// [`StagedNormalization::commit`] returns this value after atomically
/// publishing the staged destination.
#[derive(Debug, Clone)]
pub struct NormalizationOutcome {
    pub source: Analysis,
    pub gain: f32,
    pub render: Option<RenderStatistics>,
}

/// Policy used when a completed render is published to its destination.
///
/// `ReplaceUnchanged` snapshots an existing destination before rendering and
/// replaces it only if its identity and bytes are still unchanged at commit
/// time. `CreateNew` uses an atomic no-clobber publication and therefore also
/// rejects a destination created by another process while rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OutputConflictPolicy {
    /// Publish only when the destination does not exist at commit time.
    CreateNew,
    /// Replace the destination only when it is unchanged since staging began.
    ReplaceUnchanged,
}

impl OutputConflictPolicy {
    fn allows_overwrite(self) -> bool {
        matches!(self, Self::ReplaceUnchanged)
    }
}

/// A fully rendered output that has not replaced its destination yet.
///
/// Dropping this value removes the sibling temporary file and leaves any
/// existing destination untouched. This lets batch callers render bounded
/// waves concurrently, then publish successful outputs in caller order.
pub struct StagedNormalization {
    output: AtomicOutput,
    outcome: NormalizationOutcome,
    protected_inputs: Vec<StableInput>,
}

impl StagedNormalization {
    /// Measurements captured while producing the staged output.
    pub fn outcome(&self) -> &NormalizationOutcome {
        &self.outcome
    }

    /// Path of the complete sibling file awaiting publication.
    ///
    /// This is intended for a durable `ReadyToPublish` checkpoint. The path is
    /// owned by this value and is removed if the value is dropped normally.
    pub fn staged_path(&self) -> &Path {
        self.output.path()
    }

    /// Synchronize and atomically replace the destination with this render.
    pub fn commit(self) -> Result<NormalizationOutcome, String> {
        let Self {
            output,
            outcome,
            protected_inputs,
        } = self;
        verify_stable_inputs(&protected_inputs, "input changed before output publication")?;
        output.commit()?;
        Ok(outcome)
    }
}

/// A verified corrected render that has not yet replaced its destination.
///
/// This exposes the final staged bytes so resumable batch coordinators can
/// durably record a ready-to-publish checkpoint before publication.
pub struct StagedCorrectedNormalization {
    output: AtomicOutput,
    outcome: CorrectedNormalization,
    protected_inputs: Vec<StableInput>,
}

impl StagedCorrectedNormalization {
    /// Corrected measurements captured for the staged render.
    pub fn outcome(&self) -> &CorrectedNormalization {
        &self.outcome
    }

    /// Path of the verified sibling file awaiting publication.
    pub fn staged_path(&self) -> &Path {
        self.output.path()
    }

    /// Revalidate the source and atomically publish the corrected render.
    pub fn commit(self) -> Result<CorrectedNormalization, String> {
        let Self {
            output,
            outcome,
            protected_inputs,
        } = self;
        verify_stable_inputs(
            &protected_inputs,
            "input changed before corrected output publication",
        )?;
        output.commit()?;
        Ok(outcome)
    }
}

#[derive(Debug, Clone)]
pub struct CorrectedAlbumNormalization {
    pub sources: Vec<Analysis>,
    pub gain: f32,
    pub verifications: Vec<Verification>,
    pub renders: Vec<RenderStatistics>,
    pub expected_album_lufs: f64,
    pub actual_album_lufs: f64,
    /// Number of complete album encoding passes, including the initial pass.
    pub attempts: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct CorrectedMultiDeliveryNormalization {
    pub source: Analysis,
    /// One linear gain shared by every encoded delivery.
    pub gain: f32,
    pub verifications: Vec<Verification>,
    pub renders: Vec<RenderStatistics>,
    pub expected_level: f64,
    /// Number of complete multi-output encoding passes, including the initial pass.
    pub attempts: usize,
}

impl Verification {
    pub fn passed(&self) -> bool {
        self.level_ok && self.true_peak_ok
    }
}

/// Render and measure the conventional Lo/Ro stereo presentation from a
/// WAVE_FORMAT_EXTENSIBLE ordered multichannel source. Centre and surround
/// channels use -3.01 dB coefficients and LFE is omitted.
pub fn analyze_stereo_downmix(path: &Path) -> Result<DownmixMeasurement, String> {
    analyze_stereo_downmix_with_roles(path, None)
}

/// Render and measure a stereo downmix with an optional explicit source
/// speaker layout for containers whose channel metadata is absent or
/// ambiguous.
pub fn analyze_stereo_downmix_with_roles(
    path: &Path,
    channel_roles: Option<&[ChannelRole]>,
) -> Result<DownmixMeasurement, String> {
    let (mut source, layout_provenance) = decoder::decode_with_layout(path)?;
    source.channel_roles = resolve_decoded_channel_roles(
        path,
        source.channels,
        &source.channel_roles,
        layout_provenance,
        channel_roles,
    )?;
    if source.channels < 3 {
        return Err("stereo downmix QC requires at least three input channels".into());
    }
    // Keep the legacy manifest option on the same explicit WAVE-order matrix
    // as forge-downmix-qc whenever the file carries one of the supported
    // standard masks. Unusual channel counts/masks retain the historical
    // alternating fallback below for compatibility.
    if let Some(layout) = standard_stereo_downmix_layout(&source) {
        let rendered = downmix::render(&source, layout, downmix::Profile::Stereo)?;
        return Ok(DownmixMeasurement {
            analysis: analyze(&rendered.buffer),
            method: "Lo/Ro: L/R + center/surround at -3.01 dB; LFE omitted; WAVE channel order",
        });
    }
    let mut left = source.data[0].clone();
    let mut right = source.data[1].clone();
    let coefficient = std::f32::consts::FRAC_1_SQRT_2;
    for frame in 0..source.frames {
        let centre = source.data[2][frame] * coefficient;
        left[frame] += centre;
        right[frame] += centre;
    }
    let surround_start = if source.channels >= 6 { 4 } else { 3 };
    for index in surround_start..source.channels as usize {
        if source.channel_roles[index] == ChannelRole::Lfe {
            continue;
        }
        let destination = if (index - surround_start) % 2 == 0 {
            &mut left
        } else {
            &mut right
        };
        for (output, input) in destination.iter_mut().zip(&source.data[index]) {
            *output += *input * coefficient;
        }
    }
    let downmix = AudioBuffer {
        sample_rate: source.sample_rate,
        channels: 2,
        frames: source.frames,
        data: vec![left, right],
        channel_roles: crate::wav::default_channel_roles(2),
        source_kind: PcmKind::F32,
    };
    Ok(DownmixMeasurement {
        analysis: analyze(&downmix),
        method: "Lo/Ro: L/R + center/surround at -3.01 dB; LFE omitted; WAVE channel order",
    })
}

fn standard_stereo_downmix_layout(source: &AudioBuffer) -> Option<downmix::Layout> {
    let layout = match source.channels {
        6 => downmix::Layout::FiveOne,
        7 => downmix::Layout::SixOne,
        8 => downmix::Layout::SevenOne,
        10 => downmix::Layout::FiveOneFour,
        12 => downmix::Layout::SevenOneFour,
        _ => return None,
    };
    (source.channel_roles == layout.roles()).then_some(layout)
}

pub fn load_adm_presentation_map(path: &Path) -> Result<AdmPresentationMap, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("read ADM presentation map {}: {error}", path.display()))?;
    let map = match path.extension().and_then(|value| value.to_str()) {
        Some("json") => serde_json::from_str(&text)
            .map_err(|error| format!("parse ADM presentation map {}: {error}", path.display()))?,
        Some("toml") => toml::from_str(&text)
            .map_err(|error| format!("parse ADM presentation map {}: {error}", path.display()))?,
        _ => return Err("ADM presentation maps must use a .json or .toml extension".into()),
    };
    validate_adm_presentation_map(&map)?;
    Ok(map)
}

fn validate_adm_presentation_map(map: &AdmPresentationMap) -> Result<(), String> {
    if map.presentations.is_empty() {
        return Err("ADM presentation map contains no presentations".into());
    }
    let mut ids = std::collections::HashSet::new();
    for presentation in &map.presentations {
        if presentation.id.trim().is_empty() || presentation.name.trim().is_empty() {
            return Err("ADM presentation IDs and names cannot be empty".into());
        }
        if !ids.insert(&presentation.id) {
            return Err(format!("duplicate ADM presentation ID {}", presentation.id));
        }
        if presentation.channels.is_empty() || presentation.channels.contains(&0) {
            return Err(format!(
                "ADM presentation {} requires one-based channel numbers",
                presentation.id
            ));
        }
    }
    Ok(())
}

pub fn analyze_adm_presentations(
    path: &Path,
    channel_roles: Option<&[ChannelRole]>,
    map: &AdmPresentationMap,
) -> Result<AdmQcResult, String> {
    validate_adm_presentation_map(map)?;
    let (source, layout_provenance) = decoder::decode_with_layout(path)?;
    let roles = resolve_decoded_channel_roles(
        path,
        source.channels,
        &source.channel_roles,
        layout_provenance,
        channel_roles,
    )?;
    let axml = metadata::read_wave_chunk(path, *b"axml")?;
    let chna_present = metadata::read_wave_chunk(path, *b"chna")?.is_some();
    let axml_text = axml
        .as_deref()
        .map(String::from_utf8_lossy)
        .unwrap_or_default();
    let mut presentations = Vec::with_capacity(map.presentations.len());
    for presentation in &map.presentations {
        if presentation
            .channels
            .iter()
            .any(|channel| *channel > source.channels as usize)
        {
            return Err(format!(
                "ADM presentation {} references a channel beyond {}",
                presentation.id, source.channels
            ));
        }
        let indices = presentation
            .channels
            .iter()
            .map(|channel| channel - 1)
            .collect::<Vec<_>>();
        let rendered = AudioBuffer {
            sample_rate: source.sample_rate,
            channels: indices.len() as u16,
            frames: source.frames,
            data: indices
                .iter()
                .map(|index| source.data[*index].clone())
                .collect(),
            channel_roles: indices.iter().map(|index| roles[*index]).collect(),
            source_kind: source.source_kind,
        };
        let measured = analyze(&rendered);
        presentations.push(AdmPresentationMeasurement {
            id: presentation.id.clone(),
            name: presentation.name.clone(),
            channels: presentation.channels.clone(),
            integrated_lufs: measured.lufs,
            true_peak_dbtp: measured.true_peak_db(),
            render_method: "direct-channel-map (no ADM object renderer)",
            referenced_by_axml: axml_text.contains(&presentation.id),
        });
    }
    let passed = axml.is_some()
        && chna_present
        && presentations
            .iter()
            .all(|presentation| presentation.referenced_by_axml);
    Ok(AdmQcResult {
        axml_present: axml.is_some(),
        chna_present,
        presentations,
        passed,
    })
}

/// Deterministic, auditable multi-feature dialogue-candidate detector.
pub fn detect_dialogue_ranges(
    path: &Path,
    channel_roles: Option<&[ChannelRole]>,
    threshold: f64,
) -> Result<DialogueDetection, String> {
    if !threshold.is_finite() || !(0.0..=1.0).contains(&threshold) {
        return Err("dialogue confidence threshold must be between 0 and 1".into());
    }
    let (mut source, layout_provenance) = decoder::decode_with_layout(path)?;
    source.channel_roles = resolve_decoded_channel_roles(
        path,
        source.channels,
        &source.channel_roles,
        layout_provenance,
        channel_roles,
    )?;
    let window = (source.sample_rate as usize / 4).max(1);
    let mut frames = Vec::new();
    for start in (0..source.frames).step_by(window) {
        let end = (start + window).min(source.frames);
        if end - start < window / 2 {
            continue;
        }
        let (focus_signal, focus_score) = dialogue_focus(&source.data, start, end);
        let energy = mean_square(&focus_signal);
        let rms_dbfs = if energy > 0.0 {
            10.0 * energy.log10()
        } else {
            -120.0
        };
        let crossings = focus_signal
            .windows(2)
            .filter(|pair| pair[0].is_sign_positive() != pair[1].is_sign_positive())
            .count();
        let zcr = crossings as f64 / focus_signal.len() as f64;
        frames.push(DialogueDetectionFrame {
            start_seconds: start as f64 / source.sample_rate as f64,
            duration_seconds: (end - start) as f64 / source.sample_rate as f64,
            rms_dbfs,
            adaptive_noise_floor_dbfs: 0.0,
            signal_to_noise_db: 0.0,
            center_or_mid_focus: focus_score,
            zero_crossing_rate: zcr,
            speech_band_energy_ratio: speech_band_energy_ratio(&focus_signal, source.sample_rate),
            amplitude_modulation_db: amplitude_modulation_db(&focus_signal, source.sample_rate),
            periodicity: speech_periodicity(&focus_signal, source.sample_rate),
            confidence: 0.0,
            selected: false,
        });
    }
    let mut ordered_levels = frames
        .iter()
        .map(|frame| frame.rms_dbfs)
        .collect::<Vec<_>>();
    ordered_levels.sort_by(f64::total_cmp);
    let noise_index = ordered_levels.len().saturating_sub(1) / 5;
    let adaptive_noise_floor = ordered_levels
        .get(noise_index)
        .copied()
        .unwrap_or(-120.0)
        .min(-45.0);
    for frame in &mut frames {
        frame.adaptive_noise_floor_dbfs = adaptive_noise_floor;
        frame.signal_to_noise_db = frame.rms_dbfs - adaptive_noise_floor;
        let energy_score = (((frame.rms_dbfs + 55.0) / 30.0).clamp(0.0, 1.0)
            + ((frame.signal_to_noise_db - 3.0) / 15.0).clamp(0.0, 1.0))
        .min(1.0);
        let zcr_score = (frame.zero_crossing_rate / 0.005).clamp(0.0, 1.0)
            * ((0.30 - frame.zero_crossing_rate) / 0.10).clamp(0.0, 1.0);
        let band_score = ((frame.speech_band_energy_ratio - 0.15) / 0.55).clamp(0.0, 1.0);
        let modulation_score = (frame.amplitude_modulation_db / 6.0).clamp(0.0, 1.0);
        let periodicity_score = ((frame.periodicity - 0.10) / 0.65).clamp(0.0, 1.0);
        frame.confidence = 0.30 * energy_score
            + 0.20 * frame.center_or_mid_focus
            + 0.20 * band_score
            + 0.10 * zcr_score
            + 0.10 * modulation_score
            + 0.10 * periodicity_score;
    }

    let exit_threshold = (threshold - 0.12).max(0.0);
    let mut active = false;
    let mut hangover_used = false;
    for frame in &mut frames {
        if !active {
            active = frame.confidence >= threshold;
            hangover_used = false;
            frame.selected = active;
        } else if frame.confidence >= exit_threshold {
            frame.selected = true;
            hangover_used = false;
        } else if !hangover_used {
            frame.selected = true;
            hangover_used = true;
        } else {
            active = false;
            hangover_used = false;
        }
    }

    let mut ranges: Vec<DetectedDialogueRange> = Vec::new();
    for frame in frames.iter().filter(|frame| frame.selected) {
        let candidate = DetectedDialogueRange {
            start_seconds: frame.start_seconds,
            duration_seconds: frame.duration_seconds,
            confidence: frame.confidence,
        };
        if let Some(previous) = ranges.last_mut() {
            let previous_end = previous.start_seconds + previous.duration_seconds;
            if (previous_end - candidate.start_seconds).abs() < 1e-9 {
                let total = previous.duration_seconds + candidate.duration_seconds;
                previous.confidence = (previous.confidence * previous.duration_seconds
                    + candidate.confidence * candidate.duration_seconds)
                    / total;
                previous.duration_seconds = total;
                continue;
            }
        }
        ranges.push(candidate);
    }
    if ranges.is_empty() {
        return Err(format!(
            "dialogue detector found no candidates at confidence {threshold:.2}"
        ));
    }
    Ok(DialogueDetection {
        detector: "forge-dialogue-deterministic",
        detector_version: concat!("v2/", env!("CARGO_PKG_VERSION")),
        threshold,
        window_seconds: 0.25,
        features: vec![
            "window_rms_dbfs",
            "adaptive_noise_floor_dbfs",
            "signal_to_noise_db",
            "center_or_mid_focus",
            "zero_crossing_rate",
            "speech_band_energy_ratio",
            "amplitude_modulation_db",
            "periodicity",
        ],
        frames,
        ranges,
    })
}

fn speech_band_energy_ratio(samples: &[f32], sample_rate: u32) -> f64 {
    if samples.is_empty() || sample_rate == 0 {
        return 0.0;
    }
    let dt = 1.0 / sample_rate as f64;
    let high_pass_rc = 1.0 / (std::f64::consts::TAU * 80.0);
    let high_pass_alpha = high_pass_rc / (high_pass_rc + dt);
    let low_pass_rc = 1.0 / (std::f64::consts::TAU * 4_000.0);
    let low_pass_alpha = dt / (low_pass_rc + dt);
    let mut previous_input = 0.0;
    let mut high_pass = 0.0;
    let mut speech_band = 0.0;
    let mut band_energy = CompensatedSum::new();
    let mut total_energy = CompensatedSum::new();
    for sample in samples {
        let input = f64::from(*sample);
        high_pass = high_pass_alpha * (high_pass + input - previous_input);
        speech_band += low_pass_alpha * (high_pass - speech_band);
        previous_input = input;
        band_energy.add(speech_band * speech_band);
        total_energy.add(input * input);
    }
    (band_energy.total() / (total_energy.total() + f64::EPSILON)).clamp(0.0, 1.0)
}

fn amplitude_modulation_db(samples: &[f32], sample_rate: u32) -> f64 {
    let subwindow = (sample_rate as usize / 50).max(1);
    let levels = samples
        .chunks(subwindow)
        .filter(|chunk| chunk.len() >= subwindow / 2)
        .map(|chunk| {
            let energy = mean_square(chunk);
            10.0 * energy.max(1e-12).log10()
        })
        .collect::<Vec<_>>();
    if levels.len() < 2 {
        return 0.0;
    }
    let mean = levels.iter().copied().collect::<CompensatedSum>().total() / levels.len() as f64;
    (levels
        .iter()
        .map(|level| (level - mean).powi(2))
        .collect::<CompensatedSum>()
        .total()
        / levels.len() as f64)
        .sqrt()
}

fn speech_periodicity(samples: &[f32], sample_rate: u32) -> f64 {
    const FREQUENCIES: [u32; 7] = [80, 100, 125, 160, 200, 250, 300];
    let energy = samples
        .iter()
        .step_by(4)
        .map(|sample| f64::from(*sample).powi(2))
        .collect::<CompensatedSum>()
        .total();
    if energy <= f64::EPSILON {
        return 0.0;
    }
    FREQUENCIES
        .into_iter()
        .filter_map(|frequency| {
            let lag = (sample_rate / frequency) as usize;
            (lag < samples.len()).then(|| {
                let mut correlation = CompensatedSum::new();
                let mut delayed_energy = CompensatedSum::new();
                for index in (lag..samples.len()).step_by(4) {
                    let current = f64::from(samples[index]);
                    let delayed = f64::from(samples[index - lag]);
                    correlation.add(current * delayed);
                    delayed_energy.add(delayed * delayed);
                }
                (correlation.total() / (energy * delayed_energy.total()).sqrt().max(f64::EPSILON))
                    .max(0.0)
            })
        })
        .fold(0.0, f64::max)
        .clamp(0.0, 1.0)
}

fn dialogue_focus(channels: &[Vec<f32>], start: usize, end: usize) -> (Vec<f32>, f64) {
    if channels.len() >= 3 {
        let center = channels[2][start..end].to_vec();
        let center_energy = mean_square(&center);
        let other_channels = channels
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != 2 && !(channels.len() >= 6 && *index == 3))
            .map(|(_, channel)| mean_square(&channel[start..end]))
            .collect::<Vec<_>>();
        let other_energy = other_channels
            .iter()
            .copied()
            .collect::<CompensatedSum>()
            .total()
            / other_channels.len().max(1) as f64;
        let focus = center_energy / (center_energy + other_energy + f64::EPSILON);
        (center, focus)
    } else if channels.len() == 2 {
        let mid = channels[0][start..end]
            .iter()
            .zip(&channels[1][start..end])
            .map(|(left, right)| 0.5 * (left + right))
            .collect::<Vec<_>>();
        let side = channels[0][start..end]
            .iter()
            .zip(&channels[1][start..end])
            .map(|(left, right)| 0.5 * (left - right))
            .collect::<Vec<_>>();
        let mid_energy = mean_square(&mid);
        let side_energy = mean_square(&side);
        let focus = mid_energy / (mid_energy + side_energy + f64::EPSILON);
        (mid, focus)
    } else {
        let signal = channels[0][start..end].to_vec();
        let focus = if mean_square(&signal) > 0.0 { 1.0 } else { 0.0 };
        (signal, focus)
    }
}

fn mean_square(samples: &[f32]) -> f64 {
    samples
        .iter()
        .map(|sample| *sample as f64 * *sample as f64)
        .collect::<CompensatedSum>()
        .total()
        / samples.len().max(1) as f64
}

/// Linear gain that maps `an` onto the plan's target, after ceiling protection.
pub fn compute_gain(an: &Analysis, plan: &Plan) -> f32 {
    try_compute_gain(an, plan).unwrap_or(1.0)
}

/// Checked gain calculation for callers that need invalid-input diagnostics.
pub fn try_compute_gain(an: &Analysis, plan: &Plan) -> Result<f32, String> {
    plan.validate()?;
    if !an.true_peak.is_finite() || an.true_peak < 0.0 {
        return Err("analysis true peak must be finite and non-negative".into());
    }
    let measured_level = match plan.mode {
        Mode::Lufs => an.lufs,
        Mode::Peak => an.sample_peak_db(),
        Mode::Rms => an.rms_db,
    };
    if measured_level.is_nan() || measured_level == f64::INFINITY {
        return Err("analysis level must be finite or negative infinity".into());
    }
    let gain_db = match plan.mode {
        Mode::Lufs => plan.target_lufs - an.lufs,
        Mode::Peak => plan.target_peak_db - an.sample_peak_db(),
        Mode::Rms => plan.target_rms_db - an.rms_db,
    };
    Ok(clamp_gain(
        10.0_f64.powf(gain_db / 20.0),
        an.true_peak as f64,
        plan,
    ))
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
    // Digital silence has no finite gain that can reach a level target.
    // Preserve it at unity instead of allowing 0.0 * infinity to create NaNs.
    if !lin.is_finite() || lin <= 0.0 {
        lin = 1.0;
    }
    lin as f32
}

/// Apply `gain` to every channel, then a safety brick-wall clip to the ceiling.
pub fn apply_gain_and_protect(buf: &mut AudioBuffer, gain: f32, plan: &Plan) {
    let _ = try_apply_gain_and_protect(buf, gain, plan);
}

/// Checked, transactional gain application.
///
/// Validation completes before the buffer is changed. If limiter processing
/// fails, the original samples remain untouched.
pub fn try_apply_gain_and_protect(
    buf: &mut AudioBuffer,
    gain: f32,
    plan: &Plan,
) -> Result<(), String> {
    plan.validate()?;
    validate_audio_buffer_geometry(buf)?;
    validate_audio_buffer_samples(buf)?;
    validate_supported_output_sample_rate(buf.sample_rate)?;
    if buf.channels == 0 {
        return Err("audio buffer must contain at least one channel".into());
    }
    if !gain.is_finite() || gain < 0.0 {
        return Err("gain must be finite and non-negative".into());
    }
    if let Some(config) = plan.limiter {
        let mut data = buf.data.clone();
        apply_gain(&mut data, gain);
        let mut limiter =
            TruePeakLimiter::new_finite(buf.sample_rate, buf.channels, plan.ceiling_db, config)
                .map_err(|error| format!("create true-peak limiter: {error}"))?;
        let mut output = limiter.process(&data)?;
        let tail = limiter.finish();
        for (channel, tail) in output.iter_mut().zip(tail) {
            channel.extend(tail);
        }
        buf.data = output;
        return Ok(());
    }
    let ceil_lin = 10.0_f64.powf(plan.ceiling_db / 20.0) as f32;
    for ch in buf.data.iter_mut() {
        simd::apply_gain_and_hard_clip(ch, gain, ceil_lin);
    }
    Ok(())
}

pub fn load<P: AsRef<Path>>(path: P) -> Result<AudioBuffer, String> {
    decoder::decode(path.as_ref())
}

pub(crate) fn capture_stable_input(path: &Path) -> Result<StableInput, String> {
    let options = StableInputOptions::new(u64::MAX).map_err(|error| error.to_string())?;
    StableInput::from_path(path, &options).map_err(|error| error.to_string())
}

fn verify_stable_inputs(inputs: &[StableInput], context: &str) -> Result<(), String> {
    for input in inputs {
        input.verify_source().map_err(|error| {
            let source = input.source_path().map_or_else(
                || "in-memory input".to_owned(),
                |path| path.display().to_string(),
            );
            format!("{context}: {source}: {error}")
        })?;
    }
    Ok(())
}

fn validate_output_aliases(inputs: &[StableInput], outputs: &[PathBuf]) -> Result<(), String> {
    let mut keys = Vec::with_capacity(outputs.len());
    for output in outputs {
        for input in inputs {
            if input
                .aliases_source_path(output)
                .map_err(|error| error.to_string())?
            {
                return Err(format!(
                    "output aliases a protected input: {}",
                    output.display()
                ));
            }
        }
        for previous in &outputs[..keys.len()] {
            if paths_alias_if_existing(previous, output).map_err(|error| error.to_string())? {
                return Err(format!(
                    "multiple outputs alias the same file: {} and {}",
                    previous.display(),
                    output.display()
                ));
            }
        }
        let key = lexical_absolute_path(output)?;
        if keys.contains(&key) {
            return Err(format!(
                "multiple outputs resolve to the same path: {}",
                output.display()
            ));
        }
        keys.push(key);
    }
    Ok(())
}

fn lexical_absolute_path(path: &Path) -> Result<PathBuf, String> {
    use std::path::Component;

    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("read current directory: {error}"))?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(format!(
                        "output path escapes its filesystem root: {}",
                        path.display()
                    ));
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

fn validate_audio_buffer_geometry(buf: &AudioBuffer) -> Result<(), String> {
    if buf.data.len() != usize::from(buf.channels) {
        return Err(format!(
            "audio buffer has {} sample planes but declares {} channels",
            buf.data.len(),
            buf.channels
        ));
    }
    if let Some((channel, samples)) = buf
        .data
        .iter()
        .enumerate()
        .find(|(_, samples)| samples.len() != buf.frames)
    {
        return Err(format!(
            "audio buffer channel {channel} has {} frames but declares {}",
            samples.len(),
            buf.frames
        ));
    }
    Ok(())
}

fn validate_audio_buffer_samples(buf: &AudioBuffer) -> Result<(), String> {
    for (channel, samples) in buf.data.iter().enumerate() {
        if let Some((frame, _)) = samples
            .iter()
            .enumerate()
            .find(|(_, sample)| !sample.is_finite())
        {
            return Err(format!(
                "audio buffer contains a non-finite sample at frame {frame}, channel {channel}"
            ));
        }
    }
    Ok(())
}

pub fn write<P: AsRef<Path>>(
    buf: &AudioBuffer,
    path: P,
    plan: &Plan,
    format: OutputFormat,
) -> Result<(), String> {
    let p = path.as_ref();
    // Validate format-independent caller data before consulting optional
    // encoder availability. This keeps malformed in-memory PCM a deterministic
    // error even in builds that intentionally omit a codec feature.
    plan.validate()?;
    validate_audio_buffer_geometry(buf)?;
    validate_audio_buffer_samples(buf)?;
    validate_plan_for_signal(
        plan,
        format,
        buf.sample_rate,
        buf.channels,
        &buf.channel_roles,
        buf.source_kind,
        None,
        LayoutAliasPolicy::ExplicitLegacy,
    )?;
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
        OutputFormat::M4a | OutputFormat::Alac | OutputFormat::Vorbis => {
            #[cfg(feature = "ffmpeg-encoding")]
            {
                // FFmpeg may reject codec-specific constraints only after it
                // has consumed PCM. Keep the public buffer writer
                // transactional so such a late failure cannot clobber an
                // existing destination.
                let staged = AtomicOutput::new(p)?;
                let codec = match format {
                    OutputFormat::M4a => crate::aac::FfmpegCodec::Aac,
                    OutputFormat::Alac => crate::aac::FfmpegCodec::Alac,
                    OutputFormat::Vorbis => crate::aac::FfmpegCodec::Vorbis,
                    _ => unreachable!(),
                };
                let mut writer = crate::aac::AacStreamWriter::create_codec(
                    staged.path(),
                    buf.sample_rate,
                    buf.channels,
                    plan.mp3_bitrate,
                    codec,
                )?;
                writer.write_chunk(&buf.data)?;
                writer.finish()?;
                staged.commit()
            }
            #[cfg(not(feature = "ffmpeg-encoding"))]
            {
                let _ = (buf, plan);
                Err(
                    "AAC/ALAC/Vorbis output is unavailable; rebuild with `--features ffmpeg-encoding`"
                        .into(),
                )
            }
        }
    }
}

/// Analyze a file on disk (buffer is dropped after measurement).
pub fn analyze_file<P: AsRef<Path>>(path: P) -> Result<Analysis, String> {
    analyze_file_with_roles(path, None)
}

/// Analyze a complete file with an explicitly selected measurement engine.
pub fn analyze_file_with_engine<P: AsRef<Path>>(
    path: P,
    engine: AnalysisEngine,
) -> Result<Analysis, String> {
    analyze_file_with_roles_and_engine(path, None, engine)
}

/// Analyze a file with an optional explicit channel layout.
pub fn analyze_file_with_roles<P: AsRef<Path>>(
    path: P,
    channel_roles: Option<&[ChannelRole]>,
) -> Result<Analysis, String> {
    Ok(analyze_file_range_with_roles(path, channel_roles, 0.0, None, None)?.analysis)
}

/// Analyze a complete file with channel roles and an explicit engine.
pub fn analyze_file_with_roles_and_engine<P: AsRef<Path>>(
    path: P,
    channel_roles: Option<&[ChannelRole]>,
    engine: AnalysisEngine,
) -> Result<Analysis, String> {
    Ok(
        analyze_file_range_with_roles_and_engine(path, channel_roles, 0.0, None, None, engine)?
            .analysis,
    )
}

/// Analyze the exact output-domain signal when a plan requests sample-rate
/// conversion. This keeps gain and true-peak decisions after the anti-aliasing
/// filter rather than predicting them from the source rate.
pub fn analyze_file_for_plan<P: AsRef<Path>>(
    path: P,
    channel_roles: Option<&[ChannelRole]>,
    plan: &Plan,
) -> Result<Analysis, String> {
    plan.validate()?;
    let input = capture_stable_input(path.as_ref())?;
    let analysis = analyze_stable_input_for_plan_unbound(&input, channel_roles, plan)?;
    verify_stable_inputs(
        std::slice::from_ref(&input),
        "input changed during analysis",
    )?;
    Ok(analysis)
}

/// Analyze a captured input and bind the result to its exact bytes and
/// output-domain measurement request.
pub fn analyze_stable_input_for_plan(
    input: &StableInput,
    channel_roles: Option<&[ChannelRole]>,
    plan: &Plan,
) -> Result<BoundAnalysis, BoundAnalysisError> {
    plan.validate()
        .map_err(BoundAnalysisError::invalid_request)?;
    let analysis = analyze_stable_input_for_plan_unbound(input, channel_roles, plan)
        .map_err(BoundAnalysisError::analysis_failed)?;
    input
        .verify_source()
        .map_err(|error| BoundAnalysisError::analysis_failed(error.to_string()))?;
    BoundAnalysis::for_output_domain(input, analysis, channel_roles, plan)
}

/// Analyze and bind the exact track, source range, and layout selected by an
/// input descriptor in the normalization plan's output sample-rate domain.
pub fn analyze_input_descriptor_for_plan(
    descriptor: &InputDescriptor,
    plan: &Plan,
) -> Result<BoundAnalysis, BoundAnalysisError> {
    plan.validate()
        .map_err(BoundAnalysisError::invalid_request)?;
    let analysis = analyze_input_descriptor_for_plan_unbound(descriptor, plan)
        .map_err(BoundAnalysisError::analysis_failed)?;
    descriptor
        .stable_input()
        .verify_source()
        .map_err(|error| BoundAnalysisError::analysis_failed(error.to_string()))?;
    BoundAnalysis::for_descriptor(descriptor, analysis, plan)
}

pub(crate) fn analyze_stable_input_for_plan_unbound(
    input: &StableInput,
    channel_roles: Option<&[ChannelRole]>,
    plan: &Plan,
) -> Result<Analysis, String> {
    Ok(prepare_file_for_plan(input.stable_path(), channel_roles, plan, false)?.analysis)
}

pub(crate) fn analyze_input_descriptor_for_plan_unbound(
    descriptor: &InputDescriptor,
    plan: &Plan,
) -> Result<Analysis, String> {
    plan.validate()?;
    if plan
        .output_sample_rate
        .is_none_or(|sample_rate| sample_rate == descriptor.stream_info().sample_rate)
    {
        return analyze_input_descriptor_range_with_engine(descriptor, None, AnalysisEngine::Fast)
            .map(|timed| timed.analysis);
    }
    Ok(prepare_descriptor_for_plan(descriptor, plan, false)?.analysis)
}

struct PreparedAnalysis {
    analysis: Analysis,
    spool: Option<PcmSpool>,
}

const ANALYSIS_PIPELINE_DEPTH: usize = 2;
// Rubato intentionally keeps 1,024-input-frame FFT blocks for its established
// response. Coalesce only the downstream handoff so synchronization is
// amortized without changing resampling arithmetic or crossing the common
// 16,384-frame nested True Peak threshold.
const TARGET_RESAMPLED_ANALYSIS_CHUNK_FRAMES: usize = 12 * 1024;
// A converter batch of this size already amortizes the analysis-channel handoff.
// Transfer it directly instead of copying it through the coalescing buffer.
const MIN_DIRECT_RESAMPLED_ANALYSIS_CHUNK_FRAMES: usize = 8 * 1024;

enum AnalysisPipelineMessage {
    Start {
        analyzer: Box<lufs::StreamingAnalyzer>,
        spool: Option<PcmSpool>,
    },
    Chunk(Vec<Vec<f32>>),
    Finish,
    Abort,
}

enum AnalysisPipelineOutcome {
    Finished {
        analyzer: Option<Box<lufs::StreamingAnalyzer>>,
        spool: Option<PcmSpool>,
    },
    Aborted,
}

fn prepare_file_for_plan(
    path: &Path,
    channel_roles: Option<&[ChannelRole]>,
    plan: &Plan,
    capture_spool: bool,
) -> Result<PreparedAnalysis, String> {
    plan.validate()?;
    if analysis_pipeline_enabled(plan) {
        return prepare_source_for_plan_pipelined(path, None, channel_roles, plan, capture_spool);
    }
    prepare_file_for_plan_sequential(path, channel_roles, plan, capture_spool)
}

fn prepare_file_for_plan_sequential(
    path: &Path,
    channel_roles: Option<&[ChannelRole]>,
    plan: &Plan,
    capture_spool: bool,
) -> Result<PreparedAnalysis, String> {
    let mut analyzer: Option<lufs::StreamingAnalyzer> = None;
    let mut converter: Option<SampleRateConverter> = None;
    let mut spool: Option<PcmSpool> = None;
    let mut resolved_roles = None;
    let info = decoder::decode_stream_with_layout_and_declared_frames(
        path,
        |info, layout_provenance, declared_frames, chunk| {
            if analyzer.is_none() {
                let roles = resolve_stream_roles(path, info, layout_provenance, channel_roles)?;
                let output_rate = plan.output_sample_rate.unwrap_or(info.sample_rate);
                analyzer = Some(lufs::StreamingAnalyzer::new(output_rate, roles.clone()));
                resolved_roles = Some(roles);
                if output_rate != info.sample_rate {
                    converter = Some(SampleRateConverter::new_streaming(
                        info.sample_rate,
                        output_rate,
                        info.channels as usize,
                        plan.resample_quality,
                    )?);
                }
                if capture_spool && should_capture_pcm(path, converter.is_some()) {
                    // The spool is a performance optimization. If its bounded RAM
                    // or temporary-file storage cannot be created, retain the
                    // established two-decode path rather than failing an otherwise
                    // valid normalization.
                    let expected_bytes =
                        expected_pcm_spool_bytes(path, info, output_rate, declared_frames);
                    spool = PcmSpool::new(info.channels as usize, expected_bytes).ok();
                }
            }
            if let Some(converter) = converter.as_mut() {
                converter.process(chunk, |output| {
                    analyze_and_capture(output, analyzer.as_mut().unwrap(), &mut spool)
                })
            } else {
                analyze_and_capture(chunk, analyzer.as_mut().unwrap(), &mut spool)
            }
        },
    )?;
    if let Some(converter) = converter.as_mut() {
        converter
            .finish(|output| analyze_and_capture(output, analyzer.as_mut().unwrap(), &mut spool))?;
    }
    let analyzer = analyzer.ok_or_else(|| format!("{}: no audio decoded", path.display()))?;
    let roles = resolved_roles.expect("analyzer creation resolves channel roles");
    finish_prepared_analysis(info, roles, plan, analyzer, spool)
}

fn prepare_descriptor_for_plan_sequential(
    descriptor: &InputDescriptor,
    plan: &Plan,
    capture_spool: bool,
) -> Result<PreparedAnalysis, String> {
    let path = descriptor.stable_input().stable_path();
    let mut analyzer: Option<lufs::StreamingAnalyzer> = None;
    let mut converter: Option<SampleRateConverter> = None;
    let mut spool: Option<PcmSpool> = None;
    let mut resolved_roles = None;
    let info = decoder::decode_descriptor_stream_with_layout_and_declared_frames(
        descriptor,
        |info, layout_provenance, declared_frames, chunk| {
            if analyzer.is_none() {
                let roles = resolve_stream_roles(path, info, layout_provenance, None)?;
                let output_rate = plan.output_sample_rate.unwrap_or(info.sample_rate);
                analyzer = Some(lufs::StreamingAnalyzer::new(output_rate, roles.clone()));
                resolved_roles = Some(roles);
                if output_rate != info.sample_rate {
                    converter = Some(SampleRateConverter::new_streaming(
                        info.sample_rate,
                        output_rate,
                        usize::from(info.channels),
                        plan.resample_quality,
                    )?);
                }
                if capture_spool && should_capture_descriptor_pcm(descriptor, converter.is_some()) {
                    let expected_bytes =
                        expected_pcm_spool_bytes(path, info, output_rate, declared_frames);
                    spool = PcmSpool::new(info.channels as usize, expected_bytes).ok();
                }
            }
            if let Some(converter) = converter.as_mut() {
                converter.process(chunk, |output| {
                    analyze_and_capture(
                        output,
                        analyzer
                            .as_mut()
                            .expect("descriptor analyzer was initialized"),
                        &mut spool,
                    )
                })
            } else {
                analyze_and_capture(
                    chunk,
                    analyzer
                        .as_mut()
                        .expect("descriptor analyzer was initialized"),
                    &mut spool,
                )
            }
        },
    )?;
    if let Some(converter) = converter.as_mut() {
        converter.finish(|output| {
            analyze_and_capture(
                output,
                analyzer
                    .as_mut()
                    .expect("descriptor analyzer was initialized"),
                &mut spool,
            )
        })?;
    }
    let analyzer = analyzer.ok_or_else(|| format!("{}: no audio decoded", path.display()))?;
    let roles = resolved_roles.expect("analyzer creation resolves channel roles");
    finish_prepared_analysis(info, roles, plan, analyzer, spool)
}

fn prepare_descriptor_for_plan(
    descriptor: &InputDescriptor,
    plan: &Plan,
    capture_spool: bool,
) -> Result<PreparedAnalysis, String> {
    plan.validate()?;
    if analysis_pipeline_enabled(plan) {
        return prepare_source_for_plan_pipelined(
            descriptor.stable_input().stable_path(),
            Some(descriptor),
            None,
            plan,
            capture_spool,
        );
    }
    prepare_descriptor_for_plan_sequential(descriptor, plan, capture_spool)
}

fn analysis_pipeline_enabled(plan: &Plan) -> bool {
    // Nested batch/album jobs already use the shared worker budget across
    // files. An explicit output-rate request supplies enough independent
    // producer work to amortize the bounded handoff; default-rate decoding
    // retains its lower-CPU path.
    plan.output_sample_rate.is_some()
        && rayon::current_num_threads() > 1
        && rayon::current_thread_index().is_none()
}

fn run_analysis_pipeline(
    input: Receiver<AnalysisPipelineMessage>,
    recycled: SyncSender<Vec<Vec<f32>>>,
    failed: &AtomicBool,
) -> Result<AnalysisPipelineOutcome, String> {
    let mut analyzer = None;
    let mut spool = None;
    let mut first_error = None;
    while let Ok(message) = input.recv() {
        match message {
            AnalysisPipelineMessage::Start {
                analyzer: next_analyzer,
                spool: next_spool,
            } => {
                if analyzer.is_some() {
                    failed.store(true, Ordering::Release);
                    first_error.get_or_insert_with(|| {
                        "analysis pipeline received duplicate initialization".to_string()
                    });
                } else {
                    analyzer = Some(next_analyzer);
                    spool = next_spool;
                }
            }
            AnalysisPipelineMessage::Chunk(mut chunk) => {
                if first_error.is_none() {
                    let result = analyzer.as_mut().map_or_else(
                        || Err("analysis pipeline received PCM before initialization".into()),
                        |analyzer| analyzer.process(&chunk),
                    );
                    if let Err(error) = result {
                        failed.store(true, Ordering::Release);
                        first_error = Some(error);
                    } else if let Some(captured) = spool.as_mut() {
                        match captured.write_owned_chunk(chunk) {
                            Ok(recycled) => chunk = recycled,
                            Err(returned) => {
                                chunk = returned;
                                spool = None;
                            }
                        }
                    }
                }
                // The producer owns exactly ANALYSIS_PIPELINE_DEPTH slots, so
                // this bounded return channel cannot exceed its capacity.
                let _ = recycled.send(chunk);
            }
            AnalysisPipelineMessage::Finish => {
                if let Some(error) = first_error {
                    return Err(error);
                }
                return Ok(AnalysisPipelineOutcome::Finished { analyzer, spool });
            }
            AnalysisPipelineMessage::Abort => {
                return first_error.map_or(Ok(AnalysisPipelineOutcome::Aborted), Err);
            }
        }
    }
    first_error.map_or(Ok(AnalysisPipelineOutcome::Aborted), Err)
}

fn send_analysis_pipeline_chunk(
    chunk: Vec<Vec<f32>>,
    input: &SyncSender<AnalysisPipelineMessage>,
    recycled: &Receiver<Vec<Vec<f32>>>,
    available: &mut Vec<Vec<Vec<f32>>>,
    failed: &AtomicBool,
) -> Result<Vec<Vec<f32>>, String> {
    if failed.load(Ordering::Acquire) {
        return Err("analysis pipeline stopped after an analysis error".into());
    }
    loop {
        match recycled.try_recv() {
            Ok(chunk) => available.push(chunk),
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                return Err("analysis pipeline stopped unexpectedly".into());
            }
        }
    }
    input
        .send(AnalysisPipelineMessage::Chunk(chunk))
        .map_err(|_| "analysis pipeline stopped unexpectedly")?;
    if failed.load(Ordering::Acquire) {
        return Err("analysis pipeline stopped after an analysis error".into());
    }
    let recycled = if let Some(chunk) = available.pop() {
        chunk
    } else {
        recycled
            .recv()
            .map_err(|_| "analysis pipeline stopped unexpectedly")?
    };
    if failed.load(Ordering::Acquire) {
        return Err("analysis pipeline stopped after an analysis error".into());
    }
    Ok(recycled)
}

fn append_resampled_analysis_pipeline_chunk(
    mut output: Vec<Vec<f32>>,
    pending: &mut Vec<Vec<f32>>,
    input: &SyncSender<AnalysisPipelineMessage>,
    recycled: &Receiver<Vec<Vec<f32>>>,
    available: &mut Vec<Vec<Vec<f32>>>,
    failed: &AtomicBool,
) -> Result<Vec<Vec<f32>>, String> {
    if failed.load(Ordering::Acquire) {
        return Err("analysis pipeline stopped after an analysis error".into());
    }
    if pending.len() != output.len() {
        return Err("resampled analysis pipeline channel count changed".into());
    }
    let frames = output.first().map_or(0, Vec::len);
    if output.iter().any(|channel| channel.len() != frames) {
        return Err("resampled analysis pipeline received unequal channel lengths".into());
    }
    if frames >= MIN_DIRECT_RESAMPLED_ANALYSIS_CHUNK_FRAMES
        && pending.first().is_some_and(Vec::is_empty)
    {
        let mut next = send_analysis_pipeline_chunk(output, input, recycled, available, failed)?;
        for channel in &mut next {
            channel.clear();
        }
        return Ok(next);
    }
    for (destination, source) in pending.iter_mut().zip(&output) {
        destination.extend_from_slice(source);
    }
    if pending
        .first()
        .is_some_and(|channel| channel.len() >= TARGET_RESAMPLED_ANALYSIS_CHUNK_FRAMES)
    {
        let chunk = std::mem::take(pending);
        let mut next = send_analysis_pipeline_chunk(chunk, input, recycled, available, failed)?;
        for channel in &mut next {
            channel.clear();
        }
        *pending = next;
    }
    for channel in &mut output {
        channel.clear();
    }
    Ok(output)
}

fn flush_resampled_analysis_pipeline_chunk(
    pending: &mut Vec<Vec<f32>>,
    input: &SyncSender<AnalysisPipelineMessage>,
    recycled: &Receiver<Vec<Vec<f32>>>,
    available: &mut Vec<Vec<Vec<f32>>>,
    failed: &AtomicBool,
) -> Result<(), String> {
    if pending.first().is_none_or(Vec::is_empty) {
        return Ok(());
    }
    let chunk = std::mem::take(pending);
    let mut next = send_analysis_pipeline_chunk(chunk, input, recycled, available, failed)?;
    for channel in &mut next {
        channel.clear();
    }
    *pending = next;
    Ok(())
}

fn decode_analysis_source_owned<F>(
    path: &Path,
    descriptor: Option<&InputDescriptor>,
    consume: F,
) -> Result<decoder::StreamInfo, String>
where
    F: FnMut(
        &decoder::StreamInfo,
        decoder::ChannelLayoutProvenance,
        Option<u64>,
        Vec<Vec<f32>>,
    ) -> Result<Vec<Vec<f32>>, String>,
{
    if let Some(descriptor) = descriptor {
        decoder::decode_descriptor_stream_owned_with_layout_and_declared_frames(descriptor, consume)
    } else {
        decoder::decode_stream_owned_with_layout_and_declared_frames(path, consume)
    }
}

fn prepare_source_for_plan_pipelined(
    path: &Path,
    descriptor: Option<&InputDescriptor>,
    channel_roles: Option<&[ChannelRole]>,
    plan: &Plan,
    capture_spool: bool,
) -> Result<PreparedAnalysis, String> {
    let (input_sender, input_receiver) = sync_channel(ANALYSIS_PIPELINE_DEPTH);
    let (recycle_sender, recycle_receiver) = sync_channel(ANALYSIS_PIPELINE_DEPTH);
    let (result_sender, result_receiver) = sync_channel(1);
    let failed = Arc::new(AtomicBool::new(false));
    let producer_failed = Arc::clone(&failed);
    let analyzer_failed = Arc::clone(&failed);
    let mut converter: Option<SampleRateConverter> = None;
    let mut resolved_roles = None;
    let mut available = Vec::new();
    let mut resampled_pending = Vec::new();
    let mut started = false;

    let (decoding, resolved_roles) = rayon::scope(move |scope| {
        scope.spawn(move |_| {
            let result =
                run_analysis_pipeline(input_receiver, recycle_sender, analyzer_failed.as_ref());
            let _ = result_sender.send(result);
        });

        let decoded = decode_analysis_source_owned(
            path,
            descriptor,
            |info, layout_provenance, declared_frames, planar| {
                if !started {
                    let roles = resolve_stream_roles(path, info, layout_provenance, channel_roles)?;
                    let output_rate = plan.output_sample_rate.unwrap_or(info.sample_rate);
                    let next_converter = if output_rate != info.sample_rate {
                        Some(SampleRateConverter::new_streaming(
                            info.sample_rate,
                            output_rate,
                            info.channels as usize,
                            plan.resample_quality,
                        )?)
                    } else {
                        None
                    };
                    let should_capture = descriptor.map_or_else(
                        || should_capture_pcm(path, next_converter.is_some()),
                        |descriptor| {
                            should_capture_descriptor_pcm(descriptor, next_converter.is_some())
                        },
                    );
                    let spool = if capture_spool && should_capture {
                        let expected_bytes =
                            expected_pcm_spool_bytes(path, info, output_rate, declared_frames);
                        PcmSpool::new_for_top_level_pipeline(info.channels as usize, expected_bytes)
                            .ok()
                    } else {
                        None
                    };
                    input_sender
                        .send(AnalysisPipelineMessage::Start {
                            analyzer: Box::new(lufs::StreamingAnalyzer::new(
                                output_rate,
                                roles.clone(),
                            )),
                            spool,
                        })
                        .map_err(|_| "analysis pipeline stopped during setup")?;
                    available = (1..ANALYSIS_PIPELINE_DEPTH)
                        .map(|_| {
                            (0..usize::from(info.channels))
                                .map(|_| Vec::new())
                                .collect::<Vec<_>>()
                        })
                        .collect();
                    converter = next_converter;
                    if converter.is_some() {
                        resampled_pending = (0..usize::from(info.channels))
                            .map(|_| Vec::with_capacity(TARGET_RESAMPLED_ANALYSIS_CHUNK_FRAMES))
                            .collect();
                    }
                    resolved_roles = Some(roles);
                    started = true;
                }

                if let Some(converter) = converter.as_mut() {
                    converter.process_owned(&planar, |chunk| {
                        append_resampled_analysis_pipeline_chunk(
                            chunk,
                            &mut resampled_pending,
                            &input_sender,
                            &recycle_receiver,
                            &mut available,
                            producer_failed.as_ref(),
                        )
                    })?;
                    Ok(planar)
                } else {
                    send_analysis_pipeline_chunk(
                        planar,
                        &input_sender,
                        &recycle_receiver,
                        &mut available,
                        producer_failed.as_ref(),
                    )
                }
            },
        );
        let processed = decoded.and_then(|info| {
            if let Some(converter) = converter.as_mut() {
                converter.finish_owned(|chunk| {
                    append_resampled_analysis_pipeline_chunk(
                        chunk,
                        &mut resampled_pending,
                        &input_sender,
                        &recycle_receiver,
                        &mut available,
                        producer_failed.as_ref(),
                    )
                })?;
                flush_resampled_analysis_pipeline_chunk(
                    &mut resampled_pending,
                    &input_sender,
                    &recycle_receiver,
                    &mut available,
                    producer_failed.as_ref(),
                )?;
            }
            Ok(info)
        });
        let terminal = if processed.is_ok() {
            AnalysisPipelineMessage::Finish
        } else {
            AnalysisPipelineMessage::Abort
        };
        let processed = if input_sender.send(terminal).is_err() && processed.is_ok() {
            Err("analysis pipeline stopped unexpectedly".into())
        } else {
            processed
        };
        (processed, resolved_roles)
    });
    let analyzed = result_receiver
        .recv()
        .map_err(|_| "analysis pipeline stopped without a result")?;

    match analyzed {
        // Analysis of an earlier chunk precedes a later decode/resample failure.
        Err(error) => Err(error),
        Ok(AnalysisPipelineOutcome::Finished { analyzer, spool }) => {
            let info = decoding?;
            let analyzer =
                analyzer.ok_or_else(|| format!("{}: no audio decoded", path.display()))?;
            let roles = resolved_roles.expect("analyzer creation resolves channel roles");
            finish_prepared_analysis(info, roles, plan, *analyzer, spool)
        }
        Ok(AnalysisPipelineOutcome::Aborted) => match decoding {
            Err(error) => Err(error),
            Ok(_) => Err("analysis pipeline aborted unexpectedly".into()),
        },
    }
}

fn finish_prepared_analysis(
    info: decoder::StreamInfo,
    roles: Vec<ChannelRole>,
    plan: &Plan,
    analyzer: lufs::StreamingAnalyzer,
    mut spool: Option<PcmSpool>,
) -> Result<PreparedAnalysis, String> {
    let measured = analyzer.finish();
    let discard_spool = spool.as_mut().is_some_and(|captured| {
        captured.frames() != measured.frames || captured.finish_writing().is_err()
    });
    if discard_spool {
        // A spool is only a performance optimization. Preserve the established
        // re-decode fallback when a buffered tail cannot reach temporary
        // storage instead of failing an otherwise valid normalization.
        spool = None;
    }
    Ok(PreparedAnalysis {
        analysis: Analysis {
            sample_rate: plan.output_sample_rate.unwrap_or(info.sample_rate),
            channels: info.channels,
            channel_roles: roles,
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
        spool,
    })
}

fn analyze_and_capture(
    planar: &mut [Vec<f32>],
    analyzer: &mut lufs::StreamingAnalyzer,
    spool: &mut Option<PcmSpool>,
) -> Result<(), String> {
    analyzer.process(planar)?;
    let capture_failed = spool
        .as_mut()
        .is_some_and(|captured| captured.write_chunk(planar).is_err());
    if capture_failed {
        *spool = None;
    }
    Ok(())
}

fn resolve_stream_roles(
    path: &Path,
    info: &decoder::StreamInfo,
    layout_provenance: decoder::ChannelLayoutProvenance,
    channel_roles: Option<&[ChannelRole]>,
) -> Result<Vec<ChannelRole>, String> {
    resolve_decoded_channel_roles(
        path,
        info.channels,
        &info.channel_roles,
        layout_provenance,
        channel_roles,
    )
}

pub(crate) fn resolve_decoded_channel_roles(
    path: &Path,
    channels: u16,
    decoded_roles: &[ChannelRole],
    layout_provenance: decoder::ChannelLayoutProvenance,
    channel_roles: Option<&[ChannelRole]>,
) -> Result<Vec<ChannelRole>, String> {
    if let Some(roles) = channel_roles {
        if roles.len() != channels as usize {
            return Err(format!(
                "channel layout has {} channels but input has {}",
                roles.len(),
                channels
            ));
        }
        return Ok(roles.to_vec());
    }
    match layout_provenance {
        decoder::ChannelLayoutProvenance::KnownSpeakers => Ok(decoded_roles.to_vec()),
        decoder::ChannelLayoutProvenance::Unknown => Err(format!(
            "{}: ambiguous {}-channel layout; provide an explicit speaker layout",
            path.display(),
            channels
        )),
        decoder::ChannelLayoutProvenance::SceneBased => Err(format!(
            "{}: scene-based {}-channel audio requires a speaker renderer or an explicit speaker layout",
            path.display(),
            channels
        )),
    }
}

fn should_capture_pcm(path: &Path, resampling: bool) -> bool {
    resampling
        || path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                matches!(
                    extension.to_ascii_lowercase().as_str(),
                    "flac" | "dsf" | "dff"
                )
            })
}

fn should_capture_descriptor_pcm(descriptor: &InputDescriptor, resampling: bool) -> bool {
    resampling
        || matches!(
            descriptor.codec(),
            decoder::AudioCodec::Flac | decoder::AudioCodec::Dsd
        )
}

fn expected_pcm_spool_bytes(
    path: &Path,
    info: &decoder::StreamInfo,
    output_rate: u32,
    declared_frames: Option<u64>,
) -> Option<usize> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if !matches!(
        extension.as_str(),
        "wav" | "wave" | "bwf" | "bw64" | "rf64" | "flac" | "dsf" | "dff"
    ) {
        return None;
    }
    let source_frames = u128::from(declared_frames?);
    let output_frames = if output_rate == info.sample_rate {
        source_frames
    } else {
        (source_frames * u128::from(output_rate) + u128::from(info.sample_rate) / 2)
            / u128::from(info.sample_rate)
    };
    usize::try_from(output_frames)
        .ok()?
        .checked_mul(info.channels as usize)?
        .checked_mul(std::mem::size_of::<f32>())
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
    analyze_file_range_with_roles_and_engine(
        path,
        channel_roles,
        start_seconds,
        duration_seconds,
        timeline_interval_ms,
        AnalysisEngine::Fast,
    )
}

/// Analyze a source-time range with an explicitly selected measurement engine.
pub fn analyze_file_range_with_roles_and_engine<P: AsRef<Path>>(
    path: P,
    channel_roles: Option<&[ChannelRole]>,
    start_seconds: f64,
    duration_seconds: Option<f64>,
    timeline_interval_ms: Option<f64>,
    engine: AnalysisEngine,
) -> Result<TimedAnalysis, String> {
    validate_analysis_range(start_seconds, duration_seconds, timeline_interval_ms)?;
    let input = capture_stable_input(path.as_ref())?;
    let result = analyze_stable_input_range_with_engine(
        &input,
        channel_roles,
        start_seconds,
        duration_seconds,
        timeline_interval_ms,
        engine,
    )?;
    verify_stable_inputs(
        std::slice::from_ref(&input),
        "input changed during analysis",
    )?;
    Ok(result)
}

#[cfg(test)]
pub(crate) fn analyze_stable_input_range(
    input: &StableInput,
    channel_roles: Option<&[ChannelRole]>,
    start_seconds: f64,
    duration_seconds: Option<f64>,
    timeline_interval_ms: Option<f64>,
) -> Result<TimedAnalysis, String> {
    analyze_stable_input_range_with_engine(
        input,
        channel_roles,
        start_seconds,
        duration_seconds,
        timeline_interval_ms,
        AnalysisEngine::Fast,
    )
}

pub(crate) fn analyze_stable_input_range_with_engine(
    input: &StableInput,
    channel_roles: Option<&[ChannelRole]>,
    start_seconds: f64,
    duration_seconds: Option<f64>,
    timeline_interval_ms: Option<f64>,
    engine: AnalysisEngine,
) -> Result<TimedAnalysis, String> {
    validate_analysis_range(start_seconds, duration_seconds, timeline_interval_ms)?;
    let mut options =
        InputDescriptorOptions::default().with_time_range(start_seconds, duration_seconds);
    if let Some(roles) = channel_roles {
        options = options.with_channel_roles(roles.to_vec());
    }
    let descriptor = InputDescriptor::probe(input.clone(), options)?;
    analyze_input_descriptor_range_with_engine(&descriptor, timeline_interval_ms, engine)
}

/// Analyze exactly the programme and source-frame range bound by a descriptor.
pub fn analyze_input_descriptor_range_with_engine(
    descriptor: &InputDescriptor,
    timeline_interval_ms: Option<f64>,
    engine: AnalysisEngine,
) -> Result<TimedAnalysis, String> {
    if timeline_interval_ms.is_some_and(|value| !value.is_finite() || value <= 0.0) {
        return Err("timeline interval must be a finite positive number".into());
    }
    analyze_descriptor_range(descriptor, timeline_interval_ms, engine)
}

fn validate_analysis_range(
    start_seconds: f64,
    duration_seconds: Option<f64>,
    timeline_interval_ms: Option<f64>,
) -> Result<(), String> {
    if !start_seconds.is_finite() || start_seconds < 0.0 {
        return Err("analysis start must be a finite non-negative number".into());
    }
    if duration_seconds.is_some_and(|value| !value.is_finite() || value <= 0.0) {
        return Err("analysis duration must be a finite positive number".into());
    }
    if timeline_interval_ms.is_some_and(|value| !value.is_finite() || value <= 0.0) {
        return Err("timeline interval must be a finite positive number".into());
    }
    Ok(())
}

fn analyze_descriptor_range(
    descriptor: &InputDescriptor,
    timeline_interval_ms: Option<f64>,
    engine: AnalysisEngine,
) -> Result<TimedAnalysis, String> {
    enum Analyzer {
        Fast(lufs::StreamingAnalyzer),
        Reference(lufs::ReferenceStreamingAnalyzer),
    }
    impl Analyzer {
        fn process(&mut self, chunk: decoder::AnalysisPcmChunk<'_>) -> Result<(), String> {
            match (self, chunk) {
                (Self::Fast(analyzer), decoder::AnalysisPcmChunk::F32(planar)) => {
                    analyzer.process(planar)
                }
                (Self::Fast(analyzer), decoder::AnalysisPcmChunk::S32(planar)) => {
                    analyzer.process_i32(planar)
                }
                (Self::Fast(analyzer), decoder::AnalysisPcmChunk::F64(planar)) => {
                    analyzer.process_f64(planar)
                }
                (Self::Reference(analyzer), decoder::AnalysisPcmChunk::F32(planar)) => {
                    analyzer.process(planar)
                }
                (Self::Reference(analyzer), decoder::AnalysisPcmChunk::S32(planar)) => {
                    analyzer.process_i32(planar)
                }
                (Self::Reference(analyzer), decoder::AnalysisPcmChunk::F64(planar)) => {
                    analyzer.process_f64(planar)
                }
            }
        }

        fn finish(self) -> lufs::StreamingMeasurements {
            match self {
                Self::Fast(analyzer) => analyzer.finish(),
                Self::Reference(analyzer) => analyzer.finish(),
            }
        }
    }
    let mut analyzer: Option<Analyzer> = None;
    let info = decoder::decode_descriptor_analysis_stream(
        descriptor,
        |info, layout_provenance, chunk| {
            let roles = resolve_stream_roles(
                descriptor.stable_input().stable_path(),
                info,
                layout_provenance,
                None,
            )?;
            let interval_frames = timeline_interval_ms.map(|milliseconds| {
                ((f64::from(info.sample_rate) * milliseconds / 1_000.0).round() as usize).max(1)
            });
            if analyzer.is_none() {
                analyzer = Some(match engine {
                    AnalysisEngine::Fast => {
                        Analyzer::Fast(lufs::StreamingAnalyzer::with_timeline_interval(
                            info.sample_rate,
                            roles,
                            interval_frames,
                        ))
                    }
                    AnalysisEngine::Reference => Analyzer::Reference(
                        lufs::ReferenceStreamingAnalyzer::with_timeline_interval(
                            info.sample_rate,
                            roles,
                            interval_frames,
                        )?,
                    ),
                });
            }
            analyzer
                .as_mut()
                .expect("descriptor analyzer was initialized")
                .process(chunk)
        },
    )?;
    let measured = analyzer
        .ok_or_else(|| "input descriptor produced no analysis frames".to_string())?
        .finish();
    let mut timeline = measured.timeline;
    let actual_start_seconds =
        descriptor.source_range().start() as f64 / f64::from(info.sample_rate);
    for point in &mut timeline {
        point.start_seconds += actual_start_seconds;
        point.end_seconds += actual_start_seconds;
    }
    Ok(TimedAnalysis {
        analysis: Analysis {
            sample_rate: info.sample_rate,
            channels: info.channels,
            channel_roles: info.channel_roles,
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
    let info =
        decoder::decode_stream_with_layout(path.as_ref(), |info, layout_provenance, chunk| {
            let roles =
                resolve_stream_roles(path.as_ref(), info, layout_provenance, channel_roles)?;
            if source == DialogueSource::Center && info.channels < 3 {
                return Err(
                    "center dialogue source requires an input with a centre channel".into(),
                );
            }
            let chunk_start = source_frames;
            let chunk_end = source_frames + chunk.first().map_or(0, Vec::len);
            source_frames = chunk_end;
            for (range, analyzer) in ranges.iter().zip(&mut analyzers) {
                let range_start = (range.start_seconds * info.sample_rate as f64).round() as usize;
                let range_end = range_start.saturating_add(
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
                        roles.clone()
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
    let mut weighted_energy = CompensatedSum::new();
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
            .finish_without_lra_tail();
        weighted_energy.add(measured.weighted_mean_square * measured.frames as f64);
        gating_blocks.extend(measured.ebu.gating_blocks);
        frames += measured.frames;
    }
    if frames == 0 {
        return Err("dialogue ranges contain no audio".into());
    }
    let (loudness, standard_name, method) = match standard {
        DialogueStandard::AtscA85 => (
            lufs::ungated_lufs(weighted_energy.total() / frames as f64),
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
    plan.validate()?;
    if !tolerance.is_finite() || tolerance < 0.0 {
        return Err("verification tolerance must be a finite non-negative number".into());
    }
    if !gain.is_finite() || gain <= 0.0 {
        return Err("verification gain must be a finite positive number".into());
    }
    let output = analyze_file(output)?;
    try_verify_analysis(&output, source, gain, plan, tolerance)
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
    plan.validate()?;
    if !tolerance.is_finite() || tolerance < 0.0 {
        return Err("verification tolerance must be a finite non-negative number".into());
    }
    if !expected_level.is_finite() {
        return Err("verification expected level must be finite".into());
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
    try_verify_analysis(output, source, gain, plan, tolerance).unwrap_or_else(|_| Verification {
        output: output.clone(),
        expected_level: f64::NAN,
        actual_level: f64::NAN,
        deviation: f64::INFINITY,
        level_ok: false,
        true_peak_ok: false,
    })
}

/// Checked verification for already measured source and output signals.
pub fn try_verify_analysis(
    output: &Analysis,
    source: &Analysis,
    gain: f32,
    plan: &Plan,
    tolerance: f64,
) -> Result<Verification, String> {
    plan.validate()?;
    if !tolerance.is_finite() || tolerance < 0.0 {
        return Err("verification tolerance must be a finite non-negative number".into());
    }
    if !gain.is_finite() || gain <= 0.0 {
        return Err("verification gain must be a finite positive number".into());
    }
    if !output.true_peak.is_finite() || output.true_peak < 0.0 {
        return Err("output analysis true peak must be finite and non-negative".into());
    }
    let gain_db = 20.0 * (gain as f64).log10();
    let source_level = analysis_level(source, plan.mode);
    let output_level = analysis_level(output, plan.mode);
    if source_level.is_nan()
        || source_level == f64::INFINITY
        || output_level.is_nan()
        || output_level == f64::INFINITY
    {
        return Err("verification levels must be finite or negative infinity".into());
    }
    let expected_level = source_level + gain_db;
    Ok(verify_analysis_at_level(
        output,
        expected_level,
        plan,
        tolerance,
    ))
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
        true_peak_ok: true_peak_within_ceiling(output.true_peak_db(), plan.ceiling_db),
    }
}

/// True Peak ceilings are maxima, not target values. Loudness verification
/// tolerance must therefore never permit a decoded output above the ceiling.
pub(crate) fn true_peak_within_ceiling(true_peak_db: f64, ceiling_db: f64) -> bool {
    true_peak_db <= ceiling_db
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

#[derive(Clone, Copy)]
enum AnalysisReuse<'a> {
    Measure,
    Legacy(&'a Analysis),
    Bound(&'a BoundAnalysis),
}

fn analyses_identical(left: &Analysis, right: &Analysis) -> bool {
    left.sample_rate == right.sample_rate
        && left.channels == right.channels
        && left.channel_roles == right.channel_roles
        && left.frames == right.frames
        && left.kind == right.kind
        && left.lufs.to_bits() == right.lufs.to_bits()
        && left.max_momentary_lufs.to_bits() == right.max_momentary_lufs.to_bits()
        && left.max_short_term_lufs.to_bits() == right.max_short_term_lufs.to_bits()
        && left.loudness_range_lu.to_bits() == right.loudness_range_lu.to_bits()
        && left.rms_db.to_bits() == right.rms_db.to_bits()
        && left.sample_peak.to_bits() == right.sample_peak.to_bits()
        && left.true_peak.to_bits() == right.true_peak.to_bits()
        && left.loudness_blocks.len() == right.loudness_blocks.len()
        && left
            .loudness_blocks
            .iter()
            .zip(&right.loudness_blocks)
            .all(|(left, right)| left.to_bits() == right.to_bits())
}

fn prepare_analysis_for_render(
    input: &StableInput,
    channel_roles: Option<&[ChannelRole]>,
    plan: &Plan,
    reuse: AnalysisReuse<'_>,
) -> Result<(Analysis, Option<PcmSpool>, LayoutAliasPolicy), String> {
    match reuse {
        AnalysisReuse::Measure => {
            let prepared = prepare_file_for_plan(input.stable_path(), channel_roles, plan, true)?;
            let policy =
                LayoutAliasPolicy::for_override(channel_roles, &prepared.analysis.channel_roles)?;
            Ok((prepared.analysis, prepared.spool, policy))
        }
        AnalysisReuse::Legacy(expected) => {
            // A bare Analysis has no content or request identity. Remeasure the
            // stable bytes and require exact agreement before rendering.
            let prepared = prepare_file_for_plan(input.stable_path(), channel_roles, plan, true)?;
            if !analyses_identical(&prepared.analysis, expected) {
                return Err(
                    "unbound precomputed analysis does not match the captured input and plan"
                        .into(),
                );
            }
            let policy =
                LayoutAliasPolicy::for_override(channel_roles, &prepared.analysis.channel_roles)?;
            Ok((prepared.analysis, prepared.spool, policy))
        }
        AnalysisReuse::Bound(bound) => {
            bound
                .validate_for_plan(input, plan)
                .map_err(|error| error.to_string())?;
            let policy = if bound.used_explicit_roles() {
                LayoutAliasPolicy::ExplicitLegacy
            } else {
                LayoutAliasPolicy::ExactOnly
            };
            Ok((bound.analysis().clone(), None, policy))
        }
    }
}

/// Normalize a single file with exact analysis followed by gain application.
///
/// Expensive decode/resample paths may spool output-domain PCM to temporary
/// storage between the two stages; the complete signal is never retained in
/// memory.
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
    normalize_one_with_roles_and_policy(
        input,
        output,
        plan,
        format,
        channel_roles,
        OutputConflictPolicy::ReplaceUnchanged,
    )
}

/// Normalize one file with an explicit commit-time conflict policy.
pub fn normalize_one_with_roles_and_policy<P: AsRef<Path>>(
    input: P,
    output: P,
    plan: &Plan,
    format: OutputFormat,
    channel_roles: Option<&[ChannelRole]>,
    output_policy: OutputConflictPolicy,
) -> Result<(Analysis, f32), String> {
    let (analysis, gain, _) = normalize_one_with_roles_impl(
        input,
        output,
        plan,
        format,
        channel_roles,
        None,
        false,
        output_policy,
    )?;
    Ok((analysis, gain))
}

/// Render and finalize one output without replacing its destination.
///
/// The caller must invoke [`StagedNormalization::commit`] to publish the
/// result. Dropping the returned value preserves any existing destination.
pub fn normalize_one_staged_with_roles<P: AsRef<Path>>(
    input: P,
    output: P,
    plan: &Plan,
    format: OutputFormat,
    channel_roles: Option<&[ChannelRole]>,
) -> Result<StagedNormalization, String> {
    normalize_one_staged_with_roles_and_policy(
        input,
        output,
        plan,
        format,
        channel_roles,
        OutputConflictPolicy::ReplaceUnchanged,
    )
}

/// Stage one output with an explicit commit-time conflict policy.
pub fn normalize_one_staged_with_roles_and_policy<P: AsRef<Path>>(
    input: P,
    output: P,
    plan: &Plan,
    format: OutputFormat,
    channel_roles: Option<&[ChannelRole]>,
    output_policy: OutputConflictPolicy,
) -> Result<StagedNormalization, String> {
    normalize_one_staged_with_roles_impl(
        input.as_ref(),
        output.as_ref(),
        plan,
        format,
        channel_roles,
        None,
        false,
        output_policy,
    )
}

/// Compatibility entry point accepting an unbound precomputed analysis.
///
/// The input is measured again from a private snapshot and must match exactly;
/// use [`normalize_one_bound`] for efficient, content-bound cache reuse.
pub fn normalize_one_preanalyzed_with_roles<P: AsRef<Path>>(
    input: P,
    output: P,
    plan: &Plan,
    format: OutputFormat,
    channel_roles: Option<&[ChannelRole]>,
    analysis: &Analysis,
) -> Result<(Analysis, f32), String> {
    let (analysis, gain, _) = normalize_one_with_roles_impl(
        input,
        output,
        plan,
        format,
        channel_roles,
        Some(analysis),
        false,
        OutputConflictPolicy::ReplaceUnchanged,
    )?;
    Ok((analysis, gain))
}

/// Stage one output using an unbound compatibility analysis.
///
/// The supplied value is never trusted by itself and is checked against a new
/// measurement of the captured input before rendering.
///
/// The analysis must describe the same input, channel roles, output sample
/// rate, and resampling quality as the supplied plan.
pub fn normalize_one_preanalyzed_staged_with_roles<P: AsRef<Path>>(
    input: P,
    output: P,
    plan: &Plan,
    format: OutputFormat,
    channel_roles: Option<&[ChannelRole]>,
    analysis: &Analysis,
) -> Result<StagedNormalization, String> {
    normalize_one_staged_with_roles_impl(
        input.as_ref(),
        output.as_ref(),
        plan,
        format,
        channel_roles,
        Some(analysis),
        false,
        OutputConflictPolicy::ReplaceUnchanged,
    )
}

pub fn normalize_one_audited_with_roles<P: AsRef<Path>>(
    input: P,
    output: P,
    plan: &Plan,
    format: OutputFormat,
    channel_roles: Option<&[ChannelRole]>,
) -> Result<(Analysis, f32, RenderStatistics), String> {
    normalize_one_audited_with_roles_and_policy(
        input,
        output,
        plan,
        format,
        channel_roles,
        OutputConflictPolicy::ReplaceUnchanged,
    )
}

/// Audited normalization with an explicit commit-time conflict policy.
pub fn normalize_one_audited_with_roles_and_policy<P: AsRef<Path>>(
    input: P,
    output: P,
    plan: &Plan,
    format: OutputFormat,
    channel_roles: Option<&[ChannelRole]>,
    output_policy: OutputConflictPolicy,
) -> Result<(Analysis, f32, RenderStatistics), String> {
    let (analysis, gain, render) = normalize_one_with_roles_impl(
        input,
        output,
        plan,
        format,
        channel_roles,
        None,
        true,
        output_policy,
    )?;
    Ok((
        analysis,
        gain,
        render.expect("audited normalization captures render statistics"),
    ))
}

/// Audited normalization using an unbound compatibility analysis.
pub fn normalize_one_preanalyzed_audited_with_roles<P: AsRef<Path>>(
    input: P,
    output: P,
    plan: &Plan,
    format: OutputFormat,
    channel_roles: Option<&[ChannelRole]>,
    analysis: &Analysis,
) -> Result<(Analysis, f32, RenderStatistics), String> {
    let (analysis, gain, render) = normalize_one_with_roles_impl(
        input,
        output,
        plan,
        format,
        channel_roles,
        Some(analysis),
        true,
        OutputConflictPolicy::ReplaceUnchanged,
    )?;
    Ok((
        analysis,
        gain,
        render.expect("audited normalization captures render statistics"),
    ))
}

/// Normalize an immutable input using a content- and request-bound analysis.
///
/// Unlike the legacy `*_preanalyzed_*` entry points, this API can reuse a
/// cached measurement without decoding again because the binding is checked
/// before any output is created.
pub fn normalize_one_bound(
    input: &StableInput,
    output: &Path,
    plan: &Plan,
    format: OutputFormat,
    analysis: &BoundAnalysis,
) -> Result<(Analysis, f32), BoundAnalysisError> {
    normalize_one_bound_with_policy(
        input,
        output,
        plan,
        format,
        analysis,
        OutputConflictPolicy::ReplaceUnchanged,
    )
}

/// Normalize a bound input with an explicit commit-time conflict policy.
pub fn normalize_one_bound_with_policy(
    input: &StableInput,
    output: &Path,
    plan: &Plan,
    format: OutputFormat,
    analysis: &BoundAnalysis,
    output_policy: OutputConflictPolicy,
) -> Result<(Analysis, f32), BoundAnalysisError> {
    let staged = normalize_one_bound_staged_with_policy(
        input,
        output,
        plan,
        format,
        analysis,
        output_policy,
    )?;
    let outcome = staged.commit().map_err(BoundAnalysisError::render_failed)?;
    Ok((outcome.source, outcome.gain))
}

/// Render a bound input without publishing its destination.
pub fn normalize_one_bound_staged(
    input: &StableInput,
    output: &Path,
    plan: &Plan,
    format: OutputFormat,
    analysis: &BoundAnalysis,
) -> Result<StagedNormalization, BoundAnalysisError> {
    normalize_one_bound_staged_with_policy(
        input,
        output,
        plan,
        format,
        analysis,
        OutputConflictPolicy::ReplaceUnchanged,
    )
}

/// Stage a bound input with an explicit commit-time conflict policy.
pub fn normalize_one_bound_staged_with_policy(
    input: &StableInput,
    output: &Path,
    plan: &Plan,
    format: OutputFormat,
    analysis: &BoundAnalysis,
    output_policy: OutputConflictPolicy,
) -> Result<StagedNormalization, BoundAnalysisError> {
    plan.validate_for_format(format)
        .map_err(BoundAnalysisError::invalid_request)?;
    analysis.validate_for_plan(input, plan)?;
    let channel_roles = analysis.explicit_roles();
    normalize_one_staged_stable_impl(
        input,
        output,
        plan,
        format,
        channel_roles,
        AnalysisReuse::Bound(analysis),
        false,
        output_policy,
    )
    .map_err(BoundAnalysisError::render_failed)
}

/// Audited bound normalization with pre-codec render statistics.
pub fn normalize_one_bound_audited(
    input: &StableInput,
    output: &Path,
    plan: &Plan,
    format: OutputFormat,
    analysis: &BoundAnalysis,
) -> Result<(Analysis, f32, RenderStatistics), BoundAnalysisError> {
    normalize_one_bound_audited_with_policy(
        input,
        output,
        plan,
        format,
        analysis,
        OutputConflictPolicy::ReplaceUnchanged,
    )
}

/// Audited bound normalization with an explicit commit-time conflict policy.
pub fn normalize_one_bound_audited_with_policy(
    input: &StableInput,
    output: &Path,
    plan: &Plan,
    format: OutputFormat,
    analysis: &BoundAnalysis,
    output_policy: OutputConflictPolicy,
) -> Result<(Analysis, f32, RenderStatistics), BoundAnalysisError> {
    plan.validate_for_format(format)
        .map_err(BoundAnalysisError::invalid_request)?;
    analysis.validate_for_plan(input, plan)?;
    let channel_roles = analysis.explicit_roles();
    let outcome = normalize_one_staged_stable_impl(
        input,
        output,
        plan,
        format,
        channel_roles,
        AnalysisReuse::Bound(analysis),
        true,
        output_policy,
    )
    .map_err(BoundAnalysisError::render_failed)?
    .commit()
    .map_err(BoundAnalysisError::render_failed)?;
    Ok((
        outcome.source,
        outcome.gain,
        outcome
            .render
            .expect("audited bound normalization captures render statistics"),
    ))
}

/// Prepare the descriptor's output-domain analysis and retain decoded PCM
/// whenever replaying it is cheaper than decoding the immutable snapshot again.
fn prepare_descriptor_analysis_for_render(
    descriptor: &InputDescriptor,
    plan: &Plan,
) -> Result<PreparedAnalysis, String> {
    let resampling = plan
        .output_sample_rate
        .is_some_and(|sample_rate| sample_rate != descriptor.stream_info().sample_rate);
    if should_capture_descriptor_pcm(descriptor, resampling) {
        prepare_descriptor_for_plan(descriptor, plan, true)
    } else {
        Ok(PreparedAnalysis {
            analysis: analyze_input_descriptor_for_plan_unbound(descriptor, plan)?,
            spool: None,
        })
    }
}

/// Analyze, normalize, and publish one descriptor-bound programme as a single
/// transaction.
pub fn normalize_one_descriptor_with_policy(
    descriptor: &InputDescriptor,
    output: &Path,
    plan: &Plan,
    format: OutputFormat,
    output_policy: OutputConflictPolicy,
) -> Result<(Analysis, f32), BoundAnalysisError> {
    let staged = normalize_one_descriptor_staged_with_policy(
        descriptor,
        output,
        plan,
        format,
        output_policy,
    )?;
    let outcome = staged.commit().map_err(BoundAnalysisError::render_failed)?;
    Ok((outcome.source, outcome.gain))
}

/// Analyze and stage one descriptor-bound programme as a single transaction.
///
/// The immutable snapshot is measured and rendered without rehashing the live
/// source between those two phases. [`StagedNormalization::commit`] still
/// verifies the live source immediately before publication.
pub fn normalize_one_descriptor_staged_with_policy(
    descriptor: &InputDescriptor,
    output: &Path,
    plan: &Plan,
    format: OutputFormat,
    output_policy: OutputConflictPolicy,
) -> Result<StagedNormalization, BoundAnalysisError> {
    plan.validate_for_format(format)
        .map_err(BoundAnalysisError::invalid_request)?;
    let prepared = prepare_descriptor_analysis_for_render(descriptor, plan)
        .map_err(BoundAnalysisError::analysis_failed)?;
    let analysis = BoundAnalysis::for_descriptor(descriptor, prepared.analysis, plan)?;
    normalize_one_descriptor_bound_staged_impl(
        descriptor,
        output,
        plan,
        format,
        &analysis,
        prepared.spool,
        false,
        output_policy,
    )
}

/// Stage a normalization render from the exact descriptor used to produce a
/// bound analysis. This is the track-aware counterpart of
/// [`normalize_one_bound_staged_with_policy`].
pub fn normalize_one_descriptor_bound_staged_with_policy(
    descriptor: &InputDescriptor,
    output: &Path,
    plan: &Plan,
    format: OutputFormat,
    analysis: &BoundAnalysis,
    output_policy: OutputConflictPolicy,
) -> Result<StagedNormalization, BoundAnalysisError> {
    normalize_one_descriptor_bound_staged_impl(
        descriptor,
        output,
        plan,
        format,
        analysis,
        None,
        false,
        output_policy,
    )
}

/// Normalize and publish one descriptor-bound programme.
pub fn normalize_one_descriptor_bound_with_policy(
    descriptor: &InputDescriptor,
    output: &Path,
    plan: &Plan,
    format: OutputFormat,
    analysis: &BoundAnalysis,
    output_policy: OutputConflictPolicy,
) -> Result<(Analysis, f32), BoundAnalysisError> {
    let staged = normalize_one_descriptor_bound_staged_impl(
        descriptor,
        output,
        plan,
        format,
        analysis,
        None,
        false,
        output_policy,
    )?;
    let outcome = staged.commit().map_err(BoundAnalysisError::render_failed)?;
    Ok((outcome.source, outcome.gain))
}

/// Normalize one descriptor-bound programme and return render statistics.
pub fn normalize_one_descriptor_bound_audited_with_policy(
    descriptor: &InputDescriptor,
    output: &Path,
    plan: &Plan,
    format: OutputFormat,
    analysis: &BoundAnalysis,
    output_policy: OutputConflictPolicy,
) -> Result<(Analysis, f32, RenderStatistics), BoundAnalysisError> {
    let outcome = normalize_one_descriptor_bound_staged_impl(
        descriptor,
        output,
        plan,
        format,
        analysis,
        None,
        true,
        output_policy,
    )?
    .commit()
    .map_err(BoundAnalysisError::render_failed)?;
    Ok((
        outcome.source,
        outcome.gain,
        outcome
            .render
            .expect("audited descriptor render captures statistics"),
    ))
}

#[allow(clippy::too_many_arguments)]
fn normalize_one_descriptor_bound_staged_impl(
    descriptor: &InputDescriptor,
    output: &Path,
    plan: &Plan,
    format: OutputFormat,
    analysis: &BoundAnalysis,
    mut source_spool: Option<PcmSpool>,
    capture_statistics: bool,
    output_policy: OutputConflictPolicy,
) -> Result<StagedNormalization, BoundAnalysisError> {
    plan.validate_for_format(format)
        .map_err(BoundAnalysisError::invalid_request)?;
    analysis.validate_descriptor_for_plan(descriptor, plan)?;
    let input = descriptor.stable_input();
    validate_output_aliases(
        std::slice::from_ref(input),
        std::slice::from_ref(&output.to_owned()),
    )
    .map_err(BoundAnalysisError::invalid_request)?;
    let source = analysis.analysis().clone();
    let layout_alias_policy = if analysis.used_explicit_roles() {
        LayoutAliasPolicy::ExplicitLegacy
    } else {
        LayoutAliasPolicy::ExactOnly
    };
    validate_plan_for_signal(
        plan,
        format,
        source.sample_rate,
        source.channels,
        &source.channel_roles,
        source.kind,
        Some(descriptor.channel_layout()),
        layout_alias_policy,
    )
    .map_err(BoundAnalysisError::invalid_request)?;
    let gain = compute_gain(&source, plan);
    let mut staged = AtomicOutput::new_with_overwrite(output, output_policy.allows_overwrite())
        .map_err(BoundAnalysisError::render_failed)?;
    let replaying_spool = source_spool.is_some();
    let rendered = normalize_stream(
        StreamSource {
            path: input.stable_path(),
            descriptor: (!replaying_spool).then_some(descriptor),
            spool: source_spool.as_mut(),
        },
        staged.path(),
        &source,
        gain,
        plan,
        format,
        StreamRenderOptions {
            opus_album_lufs: None,
            capture_statistics,
            capture_lossless_verification: false,
            verification_channel_roles: None,
            channel_layout: Some(descriptor.channel_layout()),
            layout_alias_policy,
        },
    )
    .map_err(BoundAnalysisError::render_failed)?;
    finalize_metadata(
        input.stable_path(),
        &mut staged,
        format,
        None,
        source.lufs + gain_db(gain),
        None,
        plan,
    )
    .map_err(BoundAnalysisError::render_failed)?;
    Ok(StagedNormalization {
        output: staged,
        outcome: NormalizationOutcome {
            source,
            gain,
            render: rendered.statistics,
        },
        protected_inputs: vec![input.clone()],
    })
}

#[allow(clippy::too_many_arguments)]
fn normalize_one_with_roles_impl<P: AsRef<Path>>(
    input: P,
    output: P,
    plan: &Plan,
    format: OutputFormat,
    channel_roles: Option<&[ChannelRole]>,
    preanalyzed: Option<&Analysis>,
    capture_statistics: bool,
    output_policy: OutputConflictPolicy,
) -> Result<(Analysis, f32, Option<RenderStatistics>), String> {
    let outcome = normalize_one_staged_with_roles_impl(
        input.as_ref(),
        output.as_ref(),
        plan,
        format,
        channel_roles,
        preanalyzed,
        capture_statistics,
        output_policy,
    )?
    .commit()?;
    Ok((outcome.source, outcome.gain, outcome.render))
}

#[allow(clippy::too_many_arguments)]
fn normalize_one_staged_with_roles_impl(
    input: &Path,
    output: &Path,
    plan: &Plan,
    format: OutputFormat,
    channel_roles: Option<&[ChannelRole]>,
    preanalyzed: Option<&Analysis>,
    capture_statistics: bool,
    output_policy: OutputConflictPolicy,
) -> Result<StagedNormalization, String> {
    plan.validate()?;
    // A legacy pre-analysis is remeasured below before it can affect a render,
    // but it can still prove that the requested container cannot represent the
    // caller's declared speaker layout. Preserve that fail-fast contract
    // before an optional encoder feature or input path is consulted.
    if let Some(analysis) = preanalyzed {
        let alias_policy = LayoutAliasPolicy::for_override(channel_roles, &analysis.channel_roles)?;
        validate_output_channel_layout(
            format,
            analysis.channels,
            &analysis.channel_roles,
            alias_policy,
        )?;
    }
    plan.validate_for_format(format)?;
    let stable = capture_stable_input(input)?;
    normalize_one_staged_stable_impl(
        &stable,
        output,
        plan,
        format,
        channel_roles,
        preanalyzed.map_or(AnalysisReuse::Measure, AnalysisReuse::Legacy),
        capture_statistics,
        output_policy,
    )
}

#[allow(clippy::too_many_arguments)]
fn normalize_one_staged_stable_impl(
    input: &StableInput,
    output: &Path,
    plan: &Plan,
    format: OutputFormat,
    channel_roles: Option<&[ChannelRole]>,
    reuse: AnalysisReuse<'_>,
    capture_statistics: bool,
    output_policy: OutputConflictPolicy,
) -> Result<StagedNormalization, String> {
    plan.validate_for_format(format)?;
    validate_output_aliases(
        std::slice::from_ref(input),
        std::slice::from_ref(&output.to_owned()),
    )?;
    let channel_layout = match reuse {
        AnalysisReuse::Bound(analysis) => Some(analysis.channel_layout()),
        AnalysisReuse::Measure | AnalysisReuse::Legacy(_) => None,
    };
    let (an, mut source_spool, layout_alias_policy) =
        prepare_analysis_for_render(input, channel_roles, plan, reuse)?;
    validate_plan_for_signal(
        plan,
        format,
        an.sample_rate,
        an.channels,
        &an.channel_roles,
        an.kind,
        channel_layout,
        layout_alias_policy,
    )?;
    let gain = compute_gain(&an, plan);
    let mut staged = AtomicOutput::new_with_overwrite(output, output_policy.allows_overwrite())?;
    let rendered = normalize_stream(
        StreamSource {
            path: input.stable_path(),
            descriptor: None,
            spool: source_spool.as_mut(),
        },
        staged.path(),
        &an,
        gain,
        plan,
        format,
        StreamRenderOptions {
            opus_album_lufs: None,
            capture_statistics,
            capture_lossless_verification: false,
            verification_channel_roles: None,
            channel_layout,
            layout_alias_policy,
        },
    )?;
    finalize_metadata(
        input.stable_path(),
        &mut staged,
        format,
        None,
        an.lufs + gain_db(gain),
        None,
        plan,
    )?;
    Ok(StagedNormalization {
        output: staged,
        outcome: NormalizationOutcome {
            source: an,
            gain,
            render: rendered.statistics,
        },
        protected_inputs: vec![input.clone()],
    })
}

/// Normalize, verify the exact encoded signal, and automatically compensate
/// for post-encode level drift or a true-peak overshoot. Native WAVE/FLAC are
/// measured inside their lossless encoder pass; codec-dependent formats are
/// re-decoded. Every correction is rendered again from the original input, so
/// lossy artifacts are never compounded across retries.
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
    normalize_one_corrected_with_roles_and_policy(
        input,
        output,
        plan,
        format,
        tolerance,
        max_retries,
        channel_roles,
        OutputConflictPolicy::ReplaceUnchanged,
    )
}

/// Correct and verify one output with an explicit conflict policy.
#[allow(clippy::too_many_arguments)]
pub fn normalize_one_corrected_with_roles_and_policy<P: AsRef<Path>>(
    input: P,
    output: P,
    plan: &Plan,
    format: OutputFormat,
    tolerance: f64,
    max_retries: usize,
    channel_roles: Option<&[ChannelRole]>,
    output_policy: OutputConflictPolicy,
) -> Result<CorrectedNormalization, String> {
    normalize_one_corrected_staged_with_roles_and_policy(
        input,
        output,
        plan,
        format,
        tolerance,
        max_retries,
        channel_roles,
        output_policy,
    )?
    .commit()
}

/// Produce a verified corrected render without publishing its destination.
#[allow(clippy::too_many_arguments)]
pub fn normalize_one_corrected_staged_with_roles_and_policy<P: AsRef<Path>>(
    input: P,
    output: P,
    plan: &Plan,
    format: OutputFormat,
    tolerance: f64,
    max_retries: usize,
    channel_roles: Option<&[ChannelRole]>,
    output_policy: OutputConflictPolicy,
) -> Result<StagedCorrectedNormalization, String> {
    normalize_one_corrected_staged_with_optional_analysis(
        input.as_ref(),
        output.as_ref(),
        plan,
        format,
        tolerance,
        max_retries,
        channel_roles,
        None,
        output_policy,
    )
}

/// Corrected normalization using an unbound compatibility analysis.
///
/// The captured input is remeasured and must match exactly before encoding.
#[allow(clippy::too_many_arguments)]
pub fn normalize_one_preanalyzed_corrected_with_roles<P: AsRef<Path>>(
    input: P,
    output: P,
    plan: &Plan,
    format: OutputFormat,
    tolerance: f64,
    max_retries: usize,
    channel_roles: Option<&[ChannelRole]>,
    analysis: &Analysis,
) -> Result<CorrectedNormalization, String> {
    normalize_one_corrected_with_optional_analysis(
        input.as_ref(),
        output.as_ref(),
        plan,
        format,
        tolerance,
        max_retries,
        channel_roles,
        Some(analysis),
        OutputConflictPolicy::ReplaceUnchanged,
    )
}

/// Corrected normalization from a content- and request-bound analysis.
pub fn normalize_one_bound_corrected(
    input: &StableInput,
    output: &Path,
    plan: &Plan,
    format: OutputFormat,
    tolerance: f64,
    max_retries: usize,
    analysis: &BoundAnalysis,
) -> Result<CorrectedNormalization, BoundAnalysisError> {
    normalize_one_bound_corrected_with_policy(
        input,
        output,
        plan,
        format,
        tolerance,
        max_retries,
        analysis,
        OutputConflictPolicy::ReplaceUnchanged,
    )
}

/// Correct and verify a bound input with an explicit conflict policy.
#[allow(clippy::too_many_arguments)]
pub fn normalize_one_bound_corrected_with_policy(
    input: &StableInput,
    output: &Path,
    plan: &Plan,
    format: OutputFormat,
    tolerance: f64,
    max_retries: usize,
    analysis: &BoundAnalysis,
    output_policy: OutputConflictPolicy,
) -> Result<CorrectedNormalization, BoundAnalysisError> {
    normalize_one_bound_corrected_staged_with_policy(
        input,
        output,
        plan,
        format,
        tolerance,
        max_retries,
        analysis,
        output_policy,
    )?
    .commit()
    .map_err(BoundAnalysisError::render_failed)
}

/// Produce a verified corrected bound render without publishing it.
#[allow(clippy::too_many_arguments)]
pub fn normalize_one_bound_corrected_staged_with_policy(
    input: &StableInput,
    output: &Path,
    plan: &Plan,
    format: OutputFormat,
    tolerance: f64,
    max_retries: usize,
    analysis: &BoundAnalysis,
    output_policy: OutputConflictPolicy,
) -> Result<StagedCorrectedNormalization, BoundAnalysisError> {
    plan.validate_for_format(format)
        .map_err(BoundAnalysisError::invalid_request)?;
    analysis.validate_for_plan(input, plan)?;
    let channel_roles = analysis.explicit_roles();
    normalize_one_corrected_stable_impl(
        input,
        output,
        plan,
        format,
        tolerance,
        max_retries,
        channel_roles,
        AnalysisReuse::Bound(analysis),
        output_policy,
    )
    .map_err(BoundAnalysisError::render_failed)
}

/// Produce a verified corrected render from the descriptor that was measured.
#[allow(clippy::too_many_arguments)]
pub fn normalize_one_descriptor_bound_corrected_staged_with_policy(
    descriptor: &InputDescriptor,
    output: &Path,
    plan: &Plan,
    format: OutputFormat,
    tolerance: f64,
    max_retries: usize,
    analysis: &BoundAnalysis,
    output_policy: OutputConflictPolicy,
) -> Result<StagedCorrectedNormalization, BoundAnalysisError> {
    plan.validate_for_format(format)
        .map_err(BoundAnalysisError::invalid_request)?;
    if !tolerance.is_finite() || tolerance < 0.0 {
        return Err(BoundAnalysisError::invalid_request(
            "verification tolerance must be a finite non-negative number",
        ));
    }
    analysis.validate_descriptor_for_plan(descriptor, plan)?;
    let input = descriptor.stable_input();
    validate_output_aliases(
        std::slice::from_ref(input),
        std::slice::from_ref(&output.to_owned()),
    )
    .map_err(BoundAnalysisError::invalid_request)?;
    let source = analysis.analysis().clone();
    let channel_roles = analysis.explicit_roles();
    let layout_alias_policy = if analysis.used_explicit_roles() {
        LayoutAliasPolicy::ExplicitLegacy
    } else {
        LayoutAliasPolicy::ExactOnly
    };
    validate_plan_for_signal(
        plan,
        format,
        source.sample_rate,
        source.channels,
        &source.channel_roles,
        source.kind,
        Some(descriptor.channel_layout()),
        layout_alias_policy,
    )
    .map_err(BoundAnalysisError::invalid_request)?;
    let mut gain = compute_gain(&source, plan);
    let mut intended_level = None;
    let mut staged = AtomicOutput::new_with_overwrite(output, output_policy.allows_overwrite())
        .map_err(BoundAnalysisError::render_failed)?;

    for attempt in 0..=max_retries {
        let rendered = normalize_stream(
            StreamSource {
                path: input.stable_path(),
                descriptor: Some(descriptor),
                spool: None,
            },
            staged.path(),
            &source,
            gain,
            plan,
            format,
            StreamRenderOptions {
                opus_album_lufs: None,
                capture_statistics: true,
                capture_lossless_verification: true,
                verification_channel_roles: channel_roles,
                channel_layout: Some(descriptor.channel_layout()),
                layout_alias_policy,
            },
        )
        .map_err(BoundAnalysisError::render_failed)?;
        let render = rendered
            .statistics
            .expect("corrected descriptor normalization captures render statistics");
        let expected_level =
            *intended_level.get_or_insert_with(|| analysis_level(&render.intended, plan.mode));
        let verification = if let Some(output) = rendered.lossless_output {
            verify_analysis_at_level(&output, expected_level, plan, tolerance)
        } else {
            verify_file_at_level_with_roles(
                staged.path(),
                expected_level,
                plan,
                tolerance,
                channel_roles,
            )
            .map_err(BoundAnalysisError::render_failed)?
        };
        if verification.passed() {
            finalize_metadata(
                input.stable_path(),
                &mut staged,
                format,
                channel_roles.is_none().then_some(&verification.output),
                verification.output.lufs,
                None,
                plan,
            )
            .map_err(BoundAnalysisError::render_failed)?;
            return Ok(StagedCorrectedNormalization {
                output: staged,
                outcome: CorrectedNormalization {
                    source,
                    gain,
                    verification,
                    render,
                    attempts: attempt + 1,
                },
                protected_inputs: vec![input.clone()],
            });
        }
        if attempt == max_retries {
            return Err(BoundAnalysisError::render_failed(format!(
                "post-encode verification failed after {} encoding pass(es)",
                attempt + 1
            )));
        }
        gain =
            corrected_gain(gain, &verification, plan).map_err(BoundAnalysisError::render_failed)?;
    }
    unreachable!("the inclusive retry loop always returns")
}

#[allow(clippy::too_many_arguments)]
fn normalize_one_corrected_with_optional_analysis(
    input: &Path,
    output: &Path,
    plan: &Plan,
    format: OutputFormat,
    tolerance: f64,
    max_retries: usize,
    channel_roles: Option<&[ChannelRole]>,
    preanalyzed: Option<&Analysis>,
    output_policy: OutputConflictPolicy,
) -> Result<CorrectedNormalization, String> {
    normalize_one_corrected_staged_with_optional_analysis(
        input,
        output,
        plan,
        format,
        tolerance,
        max_retries,
        channel_roles,
        preanalyzed,
        output_policy,
    )?
    .commit()
}

#[allow(clippy::too_many_arguments)]
fn normalize_one_corrected_staged_with_optional_analysis(
    input: &Path,
    output: &Path,
    plan: &Plan,
    format: OutputFormat,
    tolerance: f64,
    max_retries: usize,
    channel_roles: Option<&[ChannelRole]>,
    preanalyzed: Option<&Analysis>,
    output_policy: OutputConflictPolicy,
) -> Result<StagedCorrectedNormalization, String> {
    plan.validate_for_format(format)?;
    if !tolerance.is_finite() || tolerance < 0.0 {
        return Err("verification tolerance must be a finite non-negative number".into());
    }
    let stable = capture_stable_input(input)?;
    normalize_one_corrected_stable_impl(
        &stable,
        output,
        plan,
        format,
        tolerance,
        max_retries,
        channel_roles,
        preanalyzed.map_or(AnalysisReuse::Measure, AnalysisReuse::Legacy),
        output_policy,
    )
}

#[allow(clippy::too_many_arguments)]
fn normalize_one_corrected_stable_impl(
    input: &StableInput,
    output: &Path,
    plan: &Plan,
    format: OutputFormat,
    tolerance: f64,
    max_retries: usize,
    channel_roles: Option<&[ChannelRole]>,
    reuse: AnalysisReuse<'_>,
    output_policy: OutputConflictPolicy,
) -> Result<StagedCorrectedNormalization, String> {
    plan.validate_for_format(format)?;
    if !tolerance.is_finite() || tolerance < 0.0 {
        return Err("verification tolerance must be a finite non-negative number".into());
    }
    validate_output_aliases(
        std::slice::from_ref(input),
        std::slice::from_ref(&output.to_owned()),
    )?;
    let channel_layout = match reuse {
        AnalysisReuse::Bound(analysis) => Some(analysis.channel_layout()),
        AnalysisReuse::Measure | AnalysisReuse::Legacy(_) => None,
    };
    let (source, mut source_spool, layout_alias_policy) =
        prepare_analysis_for_render(input, channel_roles, plan, reuse)?;
    validate_plan_for_signal(
        plan,
        format,
        source.sample_rate,
        source.channels,
        &source.channel_roles,
        source.kind,
        channel_layout,
        layout_alias_policy,
    )?;
    let mut gain = compute_gain(&source, plan);
    let mut intended_level = None;
    let mut staged = AtomicOutput::new_with_overwrite(output, output_policy.allows_overwrite())?;

    for attempt in 0..=max_retries {
        let rendered = normalize_stream(
            StreamSource {
                path: input.stable_path(),
                descriptor: None,
                spool: source_spool.as_mut(),
            },
            staged.path(),
            &source,
            gain,
            plan,
            format,
            StreamRenderOptions {
                opus_album_lufs: None,
                capture_statistics: true,
                capture_lossless_verification: true,
                verification_channel_roles: channel_roles,
                channel_layout,
                layout_alias_policy,
            },
        )?;
        let render = rendered
            .statistics
            .expect("corrected normalization captures render statistics");
        let expected_level =
            *intended_level.get_or_insert_with(|| analysis_level(&render.intended, plan.mode));
        let verification = if let Some(output) = rendered.lossless_output {
            verify_analysis_at_level(&output, expected_level, plan, tolerance)
        } else {
            verify_file_at_level_with_roles(
                staged.path(),
                expected_level,
                plan,
                tolerance,
                channel_roles,
            )?
        };
        if verification.passed() {
            finalize_metadata(
                input.stable_path(),
                &mut staged,
                format,
                channel_roles.is_none().then_some(&verification.output),
                verification.output.lufs,
                None,
                plan,
            )?;
            return Ok(StagedCorrectedNormalization {
                output: staged,
                outcome: CorrectedNormalization {
                    source,
                    gain,
                    verification,
                    render,
                    attempts: attempt + 1,
                },
                protected_inputs: vec![input.clone()],
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

/// Render one source to several containers with one shared gain, then verify
/// every output. Corrections are accepted only when a single gain remains
/// feasible for every output's level tolerance and the common true-peak
/// ceiling. Metadata-finalized outputs are still re-decoded before commit.
#[allow(clippy::too_many_arguments)]
pub(crate) fn normalize_multi_delivery_corrected_with_roles(
    input: &StableInput,
    outputs: &[PathBuf],
    plan: &Plan,
    formats: &[OutputFormat],
    tolerance: f64,
    max_retries: usize,
    channel_roles: Option<&[ChannelRole]>,
    output_policy: OutputConflictPolicy,
) -> Result<CorrectedMultiDeliveryNormalization, String> {
    if outputs.is_empty() {
        return Err("multi-delivery requires at least one output".into());
    }
    if outputs.len() != formats.len() {
        return Err("multi-delivery output/format count mismatch".into());
    }
    if !tolerance.is_finite() || tolerance < 0.0 {
        return Err("verification tolerance must be a finite non-negative number".into());
    }
    for format in formats {
        plan.validate_for_format(*format)?;
    }
    validate_output_aliases(std::slice::from_ref(input), outputs)?;
    let prepared = prepare_file_for_plan(input.stable_path(), channel_roles, plan, true)?;
    let source = prepared.analysis;
    let mut source_spool = prepared.spool;
    if plan.mode == Mode::Lufs && !source.lufs.is_finite() {
        return Err("multi-delivery requires finite integrated source loudness".into());
    }
    let layout_alias_policy =
        LayoutAliasPolicy::for_override(channel_roles, &source.channel_roles)?;
    for format in formats {
        validate_plan_for_signal(
            plan,
            *format,
            source.sample_rate,
            source.channels,
            &source.channel_roles,
            source.kind,
            None,
            layout_alias_policy,
        )?;
    }
    let mut gain = compute_gain(&source, plan);
    let mut expected_level = None;
    let mut staged: Vec<AtomicOutput> = outputs
        .iter()
        .map(|output| AtomicOutput::new_with_overwrite(output, output_policy.allows_overwrite()))
        .collect::<Result<_, _>>()?;
    let staged_paths: Vec<PathBuf> = staged
        .iter()
        .map(|output| output.path().to_owned())
        .collect();

    for attempt in 0..=max_retries {
        let rendered = normalize_streams(
            StreamSource {
                path: input.stable_path(),
                descriptor: None,
                spool: source_spool.as_mut(),
            },
            &staged_paths,
            &source,
            gain,
            plan,
            formats,
            StreamRenderOptions {
                opus_album_lufs: None,
                capture_statistics: true,
                capture_lossless_verification: true,
                verification_channel_roles: channel_roles,
                channel_layout: None,
                layout_alias_policy,
            },
        )?;
        let render = rendered
            .statistics
            .expect("corrected multi-delivery captures render statistics");
        let renders = vec![render; formats.len()];
        let mut lossless_outputs = rendered.lossless_outputs;
        let current_intended = analysis_level(&renders[0].intended, plan.mode);
        let expected = *expected_level.get_or_insert(current_intended);
        if renders.iter().any(|render| {
            level_deviation(
                current_intended,
                analysis_level(&render.intended, plan.mode),
            ) > 1.0e-9
        }) {
            return Err("multi-delivery pre-codec renders do not share one intended level".into());
        }
        let decoded = staged_paths
            .iter()
            .zip(&mut lossless_outputs)
            .map(|(path, lossless)| {
                lossless
                    .take()
                    .map(Ok)
                    .unwrap_or_else(|| analyze_file_with_roles(path, channel_roles))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut verifications = decoded
            .iter()
            .map(|output| verify_analysis_at_level(output, expected, plan, tolerance))
            .collect::<Vec<_>>();
        if verifications.iter().all(Verification::passed) {
            for ((output, format), decoded) in staged.iter_mut().zip(formats).zip(&decoded) {
                finalize_metadata(
                    input.stable_path(),
                    output,
                    *format,
                    channel_roles.is_none().then_some(decoded),
                    decoded.lufs,
                    None,
                    plan,
                )?;
            }
            // Metadata writers may rewrite a container. Verify the exact
            // staged bytes that will become visible, not only the encodes
            // before their final metadata was attached.
            verifications = staged_paths
                .iter()
                .map(|path| {
                    analyze_file_with_roles(path, channel_roles)
                        .map(|output| verify_analysis_at_level(&output, expected, plan, tolerance))
                })
                .collect::<Result<Vec<_>, _>>()?;
            if !verifications.iter().all(Verification::passed) {
                return Err(
                    "multi-delivery verification failed after final metadata was written".into(),
                );
            }
            verify_stable_inputs(
                std::slice::from_ref(input),
                "input changed before multi-delivery publication",
            )?;
            for output in staged {
                output.commit()?;
            }
            return Ok(CorrectedMultiDeliveryNormalization {
                source,
                gain,
                verifications,
                renders,
                expected_level: expected,
                attempts: attempt + 1,
            });
        }
        if attempt == max_retries {
            return Err(format!(
                "multi-delivery verification failed after {} complete encoding pass(es)",
                attempt + 1
            ));
        }
        gain = shared_corrected_gain(gain, &verifications, plan, tolerance)?;
    }
    unreachable!("the inclusive retry loop always returns")
}

/// Album loudness from the combined population of all complete gating blocks.
pub fn album_lufs(analyses: &[Analysis]) -> f64 {
    lufs::gated_lufs_iter(
        analyses
            .iter()
            .flat_map(|analysis| analysis.loudness_blocks.iter().copied()),
    )
}

/// Album-mode gain: a single shared gain from the album loudness, constrained
/// by the worst (largest) true peak across all files so nothing exceeds the ceiling.
pub fn album_gain(analyses: &[Analysis], plan: &Plan) -> f32 {
    try_album_gain(analyses, plan).unwrap_or(1.0)
}

/// Checked album gain calculation.
pub fn try_album_gain(analyses: &[Analysis], plan: &Plan) -> Result<f32, String> {
    plan.validate()?;
    if analyses.is_empty() {
        return Err("cannot calculate gain for an empty album".into());
    }
    if analyses
        .iter()
        .any(|analysis| !analysis.true_peak.is_finite() || analysis.true_peak < 0.0)
    {
        return Err("album analyses contain an invalid true peak".into());
    }
    let album_l = album_lufs(analyses);
    if album_l.is_nan() || album_l == f64::INFINITY {
        return Err("album loudness must be finite or negative infinity".into());
    }
    let gain_db = plan.target_lufs - album_l;
    let worst_tp = analyses.iter().map(|a| a.true_peak).fold(0.0f32, f32::max);
    Ok(clamp_gain(
        10.0_f64.powf(gain_db / 20.0),
        worst_tp as f64,
        plan,
    ))
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
    normalize_album_with_roles_and_policy(
        inputs,
        outputs,
        plan,
        formats,
        channel_roles,
        OutputConflictPolicy::ReplaceUnchanged,
    )
}

/// Normalize an album with an explicit conflict policy for every output.
pub fn normalize_album_with_roles_and_policy(
    inputs: &[PathBuf],
    outputs: &[PathBuf],
    plan: &Plan,
    formats: &[OutputFormat],
    channel_roles: Option<&[ChannelRole]>,
    output_policy: OutputConflictPolicy,
) -> Result<Vec<(Analysis, f32)>, String> {
    Ok(normalize_album_with_roles_impl(
        inputs,
        outputs,
        plan,
        formats,
        channel_roles,
        None,
        false,
        output_policy,
    )?
    .into_iter()
    .map(|(analysis, gain, _)| (analysis, gain))
    .collect())
}

/// Album normalization using unbound compatibility analyses.
///
/// Every track is remeasured from its captured input before rendering.
pub fn normalize_album_preanalyzed_with_roles(
    inputs: &[PathBuf],
    outputs: &[PathBuf],
    plan: &Plan,
    formats: &[OutputFormat],
    channel_roles: Option<&[ChannelRole]>,
    analyses: &[Analysis],
) -> Result<Vec<(Analysis, f32)>, String> {
    Ok(normalize_album_with_roles_impl(
        inputs,
        outputs,
        plan,
        formats,
        channel_roles,
        Some(analyses),
        false,
        OutputConflictPolicy::ReplaceUnchanged,
    )?
    .into_iter()
    .map(|(analysis, gain, _)| (analysis, gain))
    .collect())
}

pub fn normalize_album_audited_with_roles(
    inputs: &[PathBuf],
    outputs: &[PathBuf],
    plan: &Plan,
    formats: &[OutputFormat],
    channel_roles: Option<&[ChannelRole]>,
) -> Result<Vec<(Analysis, f32, RenderStatistics)>, String> {
    normalize_album_audited_with_roles_and_policy(
        inputs,
        outputs,
        plan,
        formats,
        channel_roles,
        OutputConflictPolicy::ReplaceUnchanged,
    )
}

/// Audited album normalization with an explicit output conflict policy.
pub fn normalize_album_audited_with_roles_and_policy(
    inputs: &[PathBuf],
    outputs: &[PathBuf],
    plan: &Plan,
    formats: &[OutputFormat],
    channel_roles: Option<&[ChannelRole]>,
    output_policy: OutputConflictPolicy,
) -> Result<Vec<(Analysis, f32, RenderStatistics)>, String> {
    Ok(normalize_album_with_roles_impl(
        inputs,
        outputs,
        plan,
        formats,
        channel_roles,
        None,
        true,
        output_policy,
    )?
    .into_iter()
    .map(|(analysis, gain, render)| {
        (
            analysis,
            gain,
            render.expect("audited album normalization captures render statistics"),
        )
    })
    .collect())
}

/// Audited album normalization using unbound compatibility analyses.
pub fn normalize_album_preanalyzed_audited_with_roles(
    inputs: &[PathBuf],
    outputs: &[PathBuf],
    plan: &Plan,
    formats: &[OutputFormat],
    channel_roles: Option<&[ChannelRole]>,
    analyses: &[Analysis],
) -> Result<Vec<(Analysis, f32, RenderStatistics)>, String> {
    Ok(normalize_album_with_roles_impl(
        inputs,
        outputs,
        plan,
        formats,
        channel_roles,
        Some(analyses),
        true,
        OutputConflictPolicy::ReplaceUnchanged,
    )?
    .into_iter()
    .map(|(analysis, gain, render)| {
        (
            analysis,
            gain,
            render.expect("audited album normalization captures render statistics"),
        )
    })
    .collect())
}

/// Normalize an album from immutable inputs and content-bound analyses.
///
/// Every analysis must correspond to the input at the same index and to the
/// supplied output-domain plan. All live sources are checked again before any
/// destination is published.
pub fn normalize_album_bound(
    inputs: &[StableInput],
    outputs: &[PathBuf],
    plan: &Plan,
    formats: &[OutputFormat],
    analyses: &[BoundAnalysis],
) -> Result<Vec<(Analysis, f32)>, BoundAnalysisError> {
    normalize_album_bound_with_policy(
        inputs,
        outputs,
        plan,
        formats,
        analyses,
        OutputConflictPolicy::ReplaceUnchanged,
    )
}

/// Normalize a bound album with an explicit output conflict policy.
pub fn normalize_album_bound_with_policy(
    inputs: &[StableInput],
    outputs: &[PathBuf],
    plan: &Plan,
    formats: &[OutputFormat],
    analyses: &[BoundAnalysis],
    output_policy: OutputConflictPolicy,
) -> Result<Vec<(Analysis, f32)>, BoundAnalysisError> {
    let channel_roles = validate_bound_album_request(inputs, outputs, plan, formats, analyses)?;
    normalize_album_stable_impl(
        inputs,
        outputs,
        plan,
        formats,
        channel_roles.as_deref(),
        None,
        Some(analyses),
        false,
        output_policy,
    )
    .map(|results| {
        results
            .into_iter()
            .map(|(analysis, gain, _)| (analysis, gain))
            .collect()
    })
    .map_err(BoundAnalysisError::render_failed)
}

/// Audited album normalization from immutable inputs and bound analyses.
pub fn normalize_album_bound_audited(
    inputs: &[StableInput],
    outputs: &[PathBuf],
    plan: &Plan,
    formats: &[OutputFormat],
    analyses: &[BoundAnalysis],
) -> Result<Vec<(Analysis, f32, RenderStatistics)>, BoundAnalysisError> {
    normalize_album_bound_audited_with_policy(
        inputs,
        outputs,
        plan,
        formats,
        analyses,
        OutputConflictPolicy::ReplaceUnchanged,
    )
}

/// Audited bound album normalization with an explicit conflict policy.
pub fn normalize_album_bound_audited_with_policy(
    inputs: &[StableInput],
    outputs: &[PathBuf],
    plan: &Plan,
    formats: &[OutputFormat],
    analyses: &[BoundAnalysis],
    output_policy: OutputConflictPolicy,
) -> Result<Vec<(Analysis, f32, RenderStatistics)>, BoundAnalysisError> {
    let channel_roles = validate_bound_album_request(inputs, outputs, plan, formats, analyses)?;
    normalize_album_stable_impl(
        inputs,
        outputs,
        plan,
        formats,
        channel_roles.as_deref(),
        None,
        Some(analyses),
        true,
        output_policy,
    )
    .map(|results| {
        results
            .into_iter()
            .map(|(analysis, gain, render)| {
                (
                    analysis,
                    gain,
                    render.expect("audited bound album normalization captures statistics"),
                )
            })
            .collect()
    })
    .map_err(BoundAnalysisError::render_failed)
}

fn validate_bound_album_request(
    inputs: &[StableInput],
    outputs: &[PathBuf],
    plan: &Plan,
    formats: &[OutputFormat],
    analyses: &[BoundAnalysis],
) -> Result<Option<Vec<ChannelRole>>, BoundAnalysisError> {
    if inputs.is_empty() {
        return Err(BoundAnalysisError::invalid_request(
            "cannot normalize an empty album",
        ));
    }
    if inputs.len() != outputs.len()
        || inputs.len() != formats.len()
        || inputs.len() != analyses.len()
    {
        return Err(BoundAnalysisError::invalid_request(
            "bound album input/output/format/analysis count mismatch",
        ));
    }
    for format in formats {
        plan.validate_for_format(*format)
            .map_err(BoundAnalysisError::invalid_request)?;
    }
    for (input, analysis) in inputs.iter().zip(analyses) {
        analysis.validate_for_plan(input, plan)?;
    }
    validate_output_aliases(inputs, outputs).map_err(BoundAnalysisError::invalid_request)?;
    let channel_roles = analyses
        .first()
        .and_then(BoundAnalysis::explicit_roles)
        .map(<[ChannelRole]>::to_vec);
    if analyses
        .iter()
        .any(|analysis| analysis.explicit_roles() != channel_roles.as_deref())
    {
        return Err(BoundAnalysisError::invalid_request(
            "bound album analyses must use one common explicit channel layout",
        ));
    }
    Ok(channel_roles)
}

#[allow(clippy::too_many_arguments)]
fn normalize_album_with_roles_impl(
    inputs: &[PathBuf],
    outputs: &[PathBuf],
    plan: &Plan,
    formats: &[OutputFormat],
    channel_roles: Option<&[ChannelRole]>,
    preanalyzed: Option<&[Analysis]>,
    capture_statistics: bool,
    output_policy: OutputConflictPolicy,
) -> Result<Vec<(Analysis, f32, Option<RenderStatistics>)>, String> {
    if inputs.is_empty() {
        return Err("cannot normalize an empty album".into());
    }
    if inputs.len() != outputs.len() {
        return Err("album input/output count mismatch".into());
    }
    if inputs.len() != formats.len() {
        return Err("album input/format count mismatch".into());
    }
    if preanalyzed.is_some_and(|analyses| analyses.len() != inputs.len()) {
        return Err("album precomputed analysis/input count mismatch".into());
    }
    for format in formats {
        plan.validate_for_format(*format)?;
    }
    let captured = inputs
        .par_iter()
        .map(|path| capture_stable_input(path))
        .collect::<Vec<_>>()
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    normalize_album_stable_impl(
        &captured,
        outputs,
        plan,
        formats,
        channel_roles,
        preanalyzed,
        None,
        capture_statistics,
        output_policy,
    )
}

#[allow(clippy::too_many_arguments)]
fn normalize_album_stable_impl(
    captured: &[StableInput],
    outputs: &[PathBuf],
    plan: &Plan,
    formats: &[OutputFormat],
    channel_roles: Option<&[ChannelRole]>,
    preanalyzed: Option<&[Analysis]>,
    bound_analyses: Option<&[BoundAnalysis]>,
    capture_statistics: bool,
    output_policy: OutputConflictPolicy,
) -> Result<Vec<(Analysis, f32, Option<RenderStatistics>)>, String> {
    if captured.is_empty() {
        return Err("cannot normalize an empty album".into());
    }
    if captured.len() != outputs.len() {
        return Err("album input/output count mismatch".into());
    }
    if captured.len() != formats.len() {
        return Err("album input/format count mismatch".into());
    }
    if preanalyzed.is_some() && bound_analyses.is_some() {
        return Err("album analysis reuse modes are mutually exclusive".into());
    }
    if preanalyzed.is_some_and(|analyses| analyses.len() != captured.len())
        || bound_analyses.is_some_and(|analyses| analyses.len() != captured.len())
    {
        return Err("album precomputed analysis/input count mismatch".into());
    }
    for format in formats {
        plan.validate_for_format(*format)?;
    }
    validate_output_aliases(captured, outputs)?;
    let analyses = if let Some(bound) = bound_analyses {
        captured
            .iter()
            .zip(bound)
            .map(|(input, analysis)| {
                analysis
                    .validate_for_plan(input, plan)
                    .map(|_| analysis.analysis().clone())
                    .map_err(|error| error.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        let measured = captured
            .par_iter()
            .map(|input| analyze_stable_input_for_plan_unbound(input, channel_roles, plan))
            .collect::<Vec<_>>()
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(expected) = preanalyzed {
            if measured
                .iter()
                .zip(expected)
                .any(|(measured, expected)| !analyses_identical(measured, expected))
            {
                return Err(
                    "one or more unbound album analyses do not match the captured inputs and plan"
                        .into(),
                );
            }
        }
        measured
    };
    let layout_alias_policies = if let Some(bound) = bound_analyses {
        bound
            .iter()
            .map(|analysis| {
                if analysis.used_explicit_roles() {
                    LayoutAliasPolicy::ExplicitLegacy
                } else {
                    LayoutAliasPolicy::ExactOnly
                }
            })
            .collect()
    } else {
        analyses
            .iter()
            .map(|analysis| LayoutAliasPolicy::for_override(channel_roles, &analysis.channel_roles))
            .collect::<Result<Vec<_>, _>>()?
    };
    for (index, analysis) in analyses.iter().enumerate() {
        validate_plan_for_signal(
            plan,
            formats[index],
            analysis.sample_rate,
            analysis.channels,
            &analysis.channel_roles,
            analysis.kind,
            bound_analyses.map(|bound| bound[index].channel_layout()),
            layout_alias_policies[index],
        )?;
    }
    let gain = album_gain(&analyses, plan);
    let album_output_lufs = album_lufs(&analyses) + gain_db(gain);
    let write_album_tags = formats.iter().copied().any(writes_album_loudness_tags);
    let mut staged: Vec<AtomicOutput> = outputs
        .iter()
        .map(|output| AtomicOutput::new_with_overwrite(output, output_policy.allows_overwrite()))
        .collect::<Result<_, _>>()?;
    let staged_paths = staged
        .iter()
        .map(|output| output.path().to_owned())
        .collect::<Vec<_>>();
    let rendered = captured
        .par_iter()
        .zip(staged_paths.par_iter())
        .enumerate()
        .map(|(i, (input, output))| {
            let fmt = formats[i];
            normalize_stream(
                StreamSource {
                    path: input.stable_path(),
                    descriptor: None,
                    spool: None,
                },
                output,
                &analyses[i],
                gain,
                plan,
                fmt,
                StreamRenderOptions {
                    opus_album_lufs: Some(album_output_lufs),
                    capture_statistics,
                    capture_lossless_verification: write_album_tags,
                    verification_channel_roles: None,
                    channel_layout: bound_analyses.map(|bound| bound[i].channel_layout()),
                    layout_alias_policy: layout_alias_policies[i],
                },
            )
        })
        .collect::<Vec<Result<_, String>>>();
    // Indexed collection preserves caller order. Resolve errors serially so
    // simultaneous failures still report the first input deterministically.
    let rendered = rendered.into_iter().collect::<Result<Vec<_>, _>>()?;
    let (statistics, lossless_outputs): (Vec<_>, Vec<_>) = rendered
        .into_iter()
        .map(|result| (result.statistics, result.lossless_output))
        .unzip();
    let metadata_outputs = write_album_tags
        .then(|| {
            staged_paths
                .par_iter()
                .zip(lossless_outputs.into_par_iter())
                .map(|(path, lossless)| lossless.map(Ok).unwrap_or_else(|| analyze_file(path)))
                .collect::<Vec<_>>()
                .into_iter()
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;
    let album_metadata = metadata_outputs.as_deref().map(album_loudness_metadata);
    let finalized = captured
        .par_iter()
        .zip(staged.par_iter_mut())
        .enumerate()
        .map(|(index, (input, output))| {
            let format = formats[index];
            let measured = metadata_outputs.as_ref().map(|analyses| &analyses[index]);
            finalize_metadata(
                input.stable_path(),
                output,
                format,
                measured,
                measured.map_or(analyses[index].lufs + gain_db(gain), |value| value.lufs),
                album_metadata,
                plan,
            )
        })
        .collect::<Vec<_>>();
    finalized.into_iter().collect::<Result<Vec<_>, _>>()?;
    let results = analyses
        .into_iter()
        .zip(statistics)
        .map(|(analysis, statistics)| (analysis, gain, statistics))
        .collect();
    verify_stable_inputs(captured, "album input changed before output publication")?;
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
    normalize_album_corrected_with_roles_and_policy(
        inputs,
        outputs,
        plan,
        formats,
        tolerance,
        max_retries,
        channel_roles,
        OutputConflictPolicy::ReplaceUnchanged,
    )
}

/// Correct and verify an album with an explicit output conflict policy.
#[allow(clippy::too_many_arguments)]
pub fn normalize_album_corrected_with_roles_and_policy(
    inputs: &[PathBuf],
    outputs: &[PathBuf],
    plan: &Plan,
    formats: &[OutputFormat],
    tolerance: f64,
    max_retries: usize,
    channel_roles: Option<&[ChannelRole]>,
    output_policy: OutputConflictPolicy,
) -> Result<CorrectedAlbumNormalization, String> {
    normalize_album_corrected_with_optional_analyses(
        inputs,
        outputs,
        plan,
        formats,
        tolerance,
        max_retries,
        channel_roles,
        None,
        output_policy,
    )
}

/// Corrected album normalization using unbound compatibility analyses.
#[allow(clippy::too_many_arguments)]
pub fn normalize_album_preanalyzed_corrected_with_roles(
    inputs: &[PathBuf],
    outputs: &[PathBuf],
    plan: &Plan,
    formats: &[OutputFormat],
    tolerance: f64,
    max_retries: usize,
    channel_roles: Option<&[ChannelRole]>,
    analyses: &[Analysis],
) -> Result<CorrectedAlbumNormalization, String> {
    normalize_album_corrected_with_optional_analyses(
        inputs,
        outputs,
        plan,
        formats,
        tolerance,
        max_retries,
        channel_roles,
        Some(analyses),
        OutputConflictPolicy::ReplaceUnchanged,
    )
}

/// Corrected album normalization from immutable inputs and bound analyses.
#[allow(clippy::too_many_arguments)]
pub fn normalize_album_bound_corrected(
    inputs: &[StableInput],
    outputs: &[PathBuf],
    plan: &Plan,
    formats: &[OutputFormat],
    tolerance: f64,
    max_retries: usize,
    analyses: &[BoundAnalysis],
) -> Result<CorrectedAlbumNormalization, BoundAnalysisError> {
    normalize_album_bound_corrected_with_policy(
        inputs,
        outputs,
        plan,
        formats,
        tolerance,
        max_retries,
        analyses,
        OutputConflictPolicy::ReplaceUnchanged,
    )
}

/// Correct and verify a bound album with an explicit conflict policy.
#[allow(clippy::too_many_arguments)]
pub fn normalize_album_bound_corrected_with_policy(
    inputs: &[StableInput],
    outputs: &[PathBuf],
    plan: &Plan,
    formats: &[OutputFormat],
    tolerance: f64,
    max_retries: usize,
    analyses: &[BoundAnalysis],
    output_policy: OutputConflictPolicy,
) -> Result<CorrectedAlbumNormalization, BoundAnalysisError> {
    let channel_roles = validate_bound_album_request(inputs, outputs, plan, formats, analyses)?;
    if !tolerance.is_finite() || tolerance < 0.0 {
        return Err(BoundAnalysisError::invalid_request(
            "verification tolerance must be a finite non-negative number",
        ));
    }
    normalize_album_corrected_stable_impl(
        inputs,
        outputs,
        plan,
        formats,
        tolerance,
        max_retries,
        channel_roles.as_deref(),
        None,
        Some(analyses),
        output_policy,
    )
    .map_err(BoundAnalysisError::render_failed)
}

#[allow(clippy::too_many_arguments)]
fn normalize_album_corrected_with_optional_analyses(
    inputs: &[PathBuf],
    outputs: &[PathBuf],
    plan: &Plan,
    formats: &[OutputFormat],
    tolerance: f64,
    max_retries: usize,
    channel_roles: Option<&[ChannelRole]>,
    preanalyzed: Option<&[Analysis]>,
    output_policy: OutputConflictPolicy,
) -> Result<CorrectedAlbumNormalization, String> {
    if inputs.is_empty() {
        return Err("cannot correct an empty album".into());
    }
    if inputs.len() != outputs.len() {
        return Err("album input/output count mismatch".into());
    }
    if inputs.len() != formats.len() {
        return Err("album input/format count mismatch".into());
    }
    if !tolerance.is_finite() || tolerance < 0.0 {
        return Err("verification tolerance must be a finite non-negative number".into());
    }
    if preanalyzed.is_some_and(|analyses| analyses.len() != inputs.len()) {
        return Err("album precomputed analysis/input count mismatch".into());
    }
    for format in formats {
        plan.validate_for_format(*format)?;
    }
    let captured = inputs
        .par_iter()
        .map(|path| capture_stable_input(path))
        .collect::<Vec<_>>()
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    normalize_album_corrected_stable_impl(
        &captured,
        outputs,
        plan,
        formats,
        tolerance,
        max_retries,
        channel_roles,
        preanalyzed,
        None,
        output_policy,
    )
}

#[allow(clippy::too_many_arguments)]
fn normalize_album_corrected_stable_impl(
    captured: &[StableInput],
    outputs: &[PathBuf],
    plan: &Plan,
    formats: &[OutputFormat],
    tolerance: f64,
    max_retries: usize,
    channel_roles: Option<&[ChannelRole]>,
    preanalyzed: Option<&[Analysis]>,
    bound_analyses: Option<&[BoundAnalysis]>,
    output_policy: OutputConflictPolicy,
) -> Result<CorrectedAlbumNormalization, String> {
    if captured.is_empty() {
        return Err("cannot correct an empty album".into());
    }
    if captured.len() != outputs.len() {
        return Err("album input/output count mismatch".into());
    }
    if captured.len() != formats.len() {
        return Err("album input/format count mismatch".into());
    }
    if !tolerance.is_finite() || tolerance < 0.0 {
        return Err("verification tolerance must be a finite non-negative number".into());
    }
    if preanalyzed.is_some() && bound_analyses.is_some() {
        return Err("album analysis reuse modes are mutually exclusive".into());
    }
    if preanalyzed.is_some_and(|analyses| analyses.len() != captured.len())
        || bound_analyses.is_some_and(|analyses| analyses.len() != captured.len())
    {
        return Err("album precomputed analysis/input count mismatch".into());
    }
    for format in formats {
        plan.validate_for_format(*format)?;
    }
    validate_output_aliases(captured, outputs)?;
    let sources = if let Some(bound) = bound_analyses {
        captured
            .iter()
            .zip(bound)
            .map(|(input, analysis)| {
                analysis
                    .validate_for_plan(input, plan)
                    .map(|_| analysis.analysis().clone())
                    .map_err(|error| error.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        let measured = captured
            .par_iter()
            .map(|input| analyze_stable_input_for_plan_unbound(input, channel_roles, plan))
            .collect::<Vec<_>>()
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(expected) = preanalyzed {
            if measured
                .iter()
                .zip(expected)
                .any(|(measured, expected)| !analyses_identical(measured, expected))
            {
                return Err(
                    "one or more unbound album analyses do not match the captured inputs and plan"
                        .into(),
                );
            }
        }
        measured
    };
    let layout_alias_policies = if let Some(bound) = bound_analyses {
        bound
            .iter()
            .map(|analysis| {
                if analysis.used_explicit_roles() {
                    LayoutAliasPolicy::ExplicitLegacy
                } else {
                    LayoutAliasPolicy::ExactOnly
                }
            })
            .collect()
    } else {
        sources
            .iter()
            .map(|source| LayoutAliasPolicy::for_override(channel_roles, &source.channel_roles))
            .collect::<Result<Vec<_>, _>>()?
    };
    for (index, source) in sources.iter().enumerate() {
        validate_plan_for_signal(
            plan,
            formats[index],
            source.sample_rate,
            source.channels,
            &source.channel_roles,
            source.kind,
            bound_analyses.map(|bound| bound[index].channel_layout()),
            layout_alias_policies[index],
        )?;
    }
    let mut gain = album_gain(&sources, plan);
    let mut intended_album_lufs = None;
    let mut intended_track_levels = None;
    let mut staged: Vec<AtomicOutput> = outputs
        .iter()
        .map(|output| AtomicOutput::new_with_overwrite(output, output_policy.allows_overwrite()))
        .collect::<Result<_, _>>()?;
    let staged_paths: Vec<PathBuf> = staged
        .iter()
        .map(|output| output.path().to_owned())
        .collect();

    for attempt in 0..=max_retries {
        let album_output_lufs = album_lufs(&sources) + gain_db(gain);
        let rendered = captured
            .par_iter()
            .zip(staged_paths.par_iter())
            .enumerate()
            .map(|(index, (input, output))| {
                let format = formats[index];
                normalize_stream(
                    StreamSource {
                        path: input.stable_path(),
                        descriptor: None,
                        spool: None,
                    },
                    output,
                    &sources[index],
                    gain,
                    plan,
                    format,
                    StreamRenderOptions {
                        opus_album_lufs: Some(album_output_lufs),
                        capture_statistics: true,
                        capture_lossless_verification: true,
                        verification_channel_roles: channel_roles,
                        channel_layout: bound_analyses.map(|bound| bound[index].channel_layout()),
                        layout_alias_policy: layout_alias_policies[index],
                    },
                )
            })
            .collect::<Vec<Result<_, String>>>();
        let rendered = rendered.into_iter().collect::<Result<Vec<_>, _>>()?;
        let mut renders = Vec::with_capacity(rendered.len());
        let mut lossless_outputs = Vec::with_capacity(rendered.len());
        for result in rendered {
            renders.push(
                result
                    .statistics
                    .expect("corrected album normalization captures render statistics"),
            );
            lossless_outputs.push(result.lossless_output);
        }
        let expected_album_lufs = *intended_album_lufs.get_or_insert_with(|| {
            album_lufs(
                &renders
                    .iter()
                    .map(|render| render.intended.clone())
                    .collect::<Vec<_>>(),
            )
        });
        let expected_track_levels = intended_track_levels.get_or_insert_with(|| {
            renders
                .iter()
                .map(|render| analysis_level(&render.intended, plan.mode))
                .collect::<Vec<_>>()
        });
        let measured = staged_paths
            .par_iter()
            .zip(lossless_outputs.into_par_iter())
            .map(|(path, lossless)| {
                lossless
                    .map(Ok)
                    .unwrap_or_else(|| analyze_file_with_roles(path, channel_roles))
            })
            .collect::<Vec<_>>();
        let decoded: Vec<Analysis> = measured.into_iter().collect::<Result<_, _>>()?;
        let actual_album_lufs = album_lufs(&decoded);
        let verifications: Vec<Verification> = decoded
            .iter()
            .zip(expected_track_levels.iter())
            .map(|(output, expected)| verify_analysis_at_level(output, *expected, plan, tolerance))
            .collect();
        let album_deviation = level_deviation(expected_album_lufs, actual_album_lufs);
        let worst_true_peak = decoded
            .iter()
            .map(Analysis::true_peak_db)
            .fold(f64::NEG_INFINITY, f64::max);
        let album_passed =
            album_measurements_pass(album_deviation, worst_true_peak, plan.ceiling_db, tolerance)
                && verifications.iter().all(Verification::passed);
        if album_passed {
            let write_album_tags = formats.iter().copied().any(writes_album_loudness_tags);
            let metadata_outputs = if write_album_tags {
                if channel_roles.is_none() {
                    Some(decoded.clone())
                } else {
                    let measured = staged_paths
                        .par_iter()
                        .map(analyze_file)
                        .collect::<Vec<_>>();
                    Some(measured.into_iter().collect::<Result<Vec<_>, _>>()?)
                }
            } else {
                None
            };
            let album_metadata = metadata_outputs.as_deref().map(album_loudness_metadata);
            for (index, (input, output)) in captured.iter().zip(&mut staged).enumerate() {
                let format = formats[index];
                let measured = metadata_outputs
                    .as_ref()
                    .map(|analyses| &analyses[index])
                    .or_else(|| channel_roles.is_none().then_some(&decoded[index]));
                finalize_metadata(
                    input.stable_path(),
                    output,
                    format,
                    measured,
                    measured.map_or(decoded[index].lufs, |value| value.lufs),
                    album_metadata,
                    plan,
                )?;
            }
            verify_stable_inputs(
                captured,
                "album input changed before corrected output publication",
            )?;
            for output in staged {
                output.commit()?;
            }
            return Ok(CorrectedAlbumNormalization {
                sources,
                gain,
                verifications,
                renders,
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
            true_peak_ok: true_peak_within_ceiling(worst_true_peak, plan.ceiling_db),
        };
        gain = corrected_gain(gain, &album_verification, plan)?;
    }
    unreachable!("the inclusive retry loop always returns")
}

fn album_measurements_pass(
    loudness_deviation: f64,
    worst_true_peak_db: f64,
    ceiling_db: f64,
    loudness_tolerance: f64,
) -> bool {
    loudness_deviation <= loudness_tolerance
        && true_peak_within_ceiling(worst_true_peak_db, ceiling_db)
}

fn gain_db(gain: f32) -> f64 {
    20.0 * (gain as f64).log10()
}

fn writes_album_loudness_tags(format: OutputFormat) -> bool {
    matches!(
        format,
        OutputFormat::Opus | OutputFormat::M4a | OutputFormat::Alac | OutputFormat::Vorbis
    )
}

#[derive(Clone, Copy)]
struct AlbumLoudnessMetadata {
    lufs: f64,
    sample_peak: f32,
    true_peak: f32,
}

fn album_loudness_metadata(analyses: &[Analysis]) -> AlbumLoudnessMetadata {
    AlbumLoudnessMetadata {
        lufs: album_lufs(analyses),
        sample_peak: analyses
            .iter()
            .map(|analysis| analysis.sample_peak)
            .fold(0.0_f32, f32::max),
        true_peak: analyses
            .iter()
            .map(|analysis| analysis.true_peak)
            .fold(0.0_f32, f32::max),
    }
}

fn finalize_metadata(
    input: &Path,
    output: &mut AtomicOutput,
    format: OutputFormat,
    measured_output: Option<&Analysis>,
    _track_lufs: f64,
    album: Option<AlbumLoudnessMetadata>,
    plan: &Plan,
) -> Result<(), String> {
    let output_path = output.path().to_owned();
    metadata::copy_metadata(input, &output_path)?;
    output.adopt_path_writer_output()?;
    if format == OutputFormat::Wav && plan.bwf {
        let measured = known_or_analyze_output(&output_path, measured_output)?;
        metadata::update_bwf_loudness(&output_path, &measured)?;
        output.adopt_path_writer_output()?;
    }
    if format == OutputFormat::Opus {
        #[cfg(feature = "opus-encoding")]
        {
            let track_lufs = measured_output.map_or(_track_lufs, |measured| measured.lufs);
            crate::opus::rewrite_r128_tags(
                &output_path,
                track_lufs,
                album.map(|album| album.lufs),
            )?;
            output.adopt_path_writer_output()?;
        }
    }
    if matches!(
        format,
        OutputFormat::M4a | OutputFormat::Alac | OutputFormat::Vorbis
    ) {
        let measured = known_or_analyze_output(&output_path, measured_output)?;
        metadata::write_replaygain(
            &output_path,
            measured.lufs,
            measured.true_peak,
            album.map(|album| (album.lufs, album.true_peak)),
        )?;
        output.adopt_path_writer_output()?;
        if matches!(format, OutputFormat::M4a | OutputFormat::Alac) {
            let replaced = metadata::write_isobmff_loudness_metadata(
                &output_path,
                &measured,
                album.map(|album| (album.lufs, album.sample_peak, album.true_peak)),
            )?;
            if replaced {
                output.adopt_path_writer_output()?;
            }
        }
    }
    Ok(())
}

/// Reuse a PCM-derived measurement when its channel-role interpretation is
/// known to match the container defaults; otherwise measure the final file.
fn known_or_analyze_output<'a>(
    output: &Path,
    measured_output: Option<&'a Analysis>,
) -> Result<Cow<'a, Analysis>, String> {
    measured_output.map_or_else(
        || analyze_file(output).map(Cow::Owned),
        |analysis| Ok(Cow::Borrowed(analysis)),
    )
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

fn shared_corrected_gain(
    current_gain: f32,
    verifications: &[Verification],
    plan: &Plan,
    tolerance: f64,
) -> Result<f32, String> {
    let mut minimum_adjustment = f64::NEG_INFINITY;
    let mut maximum_adjustment = f64::INFINITY;
    for verification in verifications {
        if !verification.expected_level.is_finite() || !verification.actual_level.is_finite() {
            return Err("cannot jointly correct a non-finite output level".into());
        }
        minimum_adjustment = minimum_adjustment
            .max(verification.expected_level - tolerance - verification.actual_level);
        maximum_adjustment = maximum_adjustment
            .min(verification.expected_level + tolerance - verification.actual_level);
        if verification.output.true_peak > 0.0 {
            maximum_adjustment =
                maximum_adjustment.min(plan.ceiling_db - verification.output.true_peak_db());
        }
    }
    if let Some(max_gain_db) = plan.max_gain_db {
        maximum_adjustment = maximum_adjustment.min(max_gain_db - gain_db(current_gain));
    }
    if !minimum_adjustment.is_finite()
        || !maximum_adjustment.is_finite()
        || minimum_adjustment > maximum_adjustment
    {
        return Err(
            "no shared gain can satisfy every delivery's level and true-peak constraints".into(),
        );
    }
    // The quietest feasible point preserves the most headroom while keeping
    // every decoded output within the requested level tolerance.
    let corrected = f64::from(current_gain) * 10.0_f64.powf(minimum_adjustment / 20.0);
    if !corrected.is_finite() || corrected <= 0.0 {
        return Err("shared gain correction produced an invalid gain".into());
    }
    Ok(corrected as f32)
}

#[derive(Debug, Clone, Copy)]
struct StreamRenderOptions<'a> {
    opus_album_lufs: Option<f64>,
    capture_statistics: bool,
    capture_lossless_verification: bool,
    verification_channel_roles: Option<&'a [ChannelRole]>,
    channel_layout: Option<&'a ChannelLayoutDescriptor>,
    layout_alias_policy: LayoutAliasPolicy,
}

struct StreamSource<'a> {
    path: &'a Path,
    descriptor: Option<&'a InputDescriptor>,
    spool: Option<&'a mut PcmSpool>,
}

struct StreamRenderResult {
    statistics: Option<RenderStatistics>,
    lossless_output: Option<Analysis>,
}

struct MultiStreamRenderResult {
    statistics: Option<RenderStatistics>,
    lossless_outputs: Vec<Option<Analysis>>,
}

const STREAM_WRITER_PIPELINE_DEPTH: usize = 2;

enum StreamWriterMessage {
    Chunk(Vec<Vec<f32>>),
    Finish,
    Abort,
}

enum StreamWriterOutcome {
    Finished(Vec<Option<Analysis>>),
    Aborted,
}

// Multi-delivery bounds this enum to 32 heap-resident Vec entries. Keeping the
// concrete writers inline avoids another allocation and indirection on every
// streamed chunk; even 32 largest variants occupy less than 28 KiB.
#[allow(clippy::large_enum_variant)]
enum NormalizedStreamWriter {
    Wav {
        output: PathBuf,
        kind: PcmKind,
        writer: WavStreamWriter,
        lossless: Option<LosslessAnalysisBuilder>,
    },
    Flac {
        writer: FlacStreamWriter,
        lossless: Option<LosslessAnalysisBuilder>,
    },
    #[cfg(feature = "mp3-encoding")]
    Mp3(mp3enc::Mp3StreamWriter),
    #[cfg(feature = "opus-encoding")]
    Opus(crate::opus::OpusStreamWriter),
    #[cfg(feature = "ffmpeg-encoding")]
    Ffmpeg(crate::aac::AacStreamWriter),
}

impl NormalizedStreamWriter {
    fn create(
        input: &Path,
        output: &Path,
        analysis: &Analysis,
        gain: f32,
        plan: &Plan,
        format: OutputFormat,
        options: StreamRenderOptions<'_>,
    ) -> Result<Self, String> {
        validate_output_channel_layout_with_descriptor(
            format,
            analysis.channels,
            &analysis.channel_roles,
            options.channel_layout,
            options.layout_alias_policy,
        )?;
        #[cfg(not(feature = "opus-encoding"))]
        let _ = (gain, options.opus_album_lufs);
        match format {
            OutputFormat::Wav => {
                let kind = plan.output_kind.unwrap_or(analysis.kind);
                let metadata_chunks = if plan.bwf {
                    metadata::prepare_broadcast_chunks(input)?
                } else {
                    Vec::new()
                };
                let writer = if let Some(layout) = options.channel_layout {
                    WavStreamWriter::create_with_channel_layout_and_metadata(
                        output,
                        analysis.sample_rate,
                        analysis.frames,
                        kind,
                        plan.dither,
                        plan.wav_container,
                        layout,
                        &metadata_chunks,
                    )
                } else {
                    WavStreamWriter::create_with_metadata(
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
                }
                .map_err(|error| format!("write {}: {error}", output.display()))?;
                let lossless = if options.capture_lossless_verification {
                    let roles = if let Some(roles) = options.verification_channel_roles {
                        roles.to_vec()
                    } else if let Some(layout) = options.channel_layout {
                        layout.channel_roles()
                    } else {
                        crate::wav::writer::persisted_channel_roles(&analysis.channel_roles)
                            .map_err(|error| format!("write {}: {error}", output.display()))?
                    };
                    Some(LosslessAnalysisBuilder::new(
                        analysis.sample_rate,
                        analysis.channels,
                        roles,
                        kind,
                    ))
                } else {
                    None
                };
                Ok(Self::Wav {
                    output: output.to_owned(),
                    kind,
                    writer,
                    lossless,
                })
            }
            OutputFormat::Flac => {
                let bits = flac_bits(plan.output_kind.unwrap_or(analysis.kind))?;
                let writer = if let Some(layout) = options.channel_layout {
                    FlacStreamWriter::create_with_channel_layout(
                        output,
                        analysis.sample_rate,
                        bits,
                        plan.dither,
                        layout,
                    )
                } else {
                    FlacStreamWriter::create(
                        output,
                        analysis.sample_rate,
                        analysis.channels,
                        bits,
                        plan.dither,
                    )
                }?;
                let lossless = if options.capture_lossless_verification {
                    let roles = options.verification_channel_roles.map_or_else(
                        || {
                            options.channel_layout.map_or_else(
                                || flac_persisted_channel_roles(analysis.channels),
                                ChannelLayoutDescriptor::channel_roles,
                            )
                        },
                        <[ChannelRole]>::to_vec,
                    );
                    Some(LosslessAnalysisBuilder::new(
                        analysis.sample_rate,
                        analysis.channels,
                        roles,
                        PcmKind::F32,
                    ))
                } else {
                    None
                };
                Ok(Self::Flac { writer, lossless })
            }
            OutputFormat::Mp3 => {
                #[cfg(feature = "mp3-encoding")]
                {
                    mp3enc::Mp3StreamWriter::create(
                        output,
                        analysis.sample_rate,
                        analysis.channels,
                        plan.mp3_bitrate,
                        plan.mp3_quality,
                    )
                    .map(Self::Mp3)
                }
                #[cfg(not(feature = "mp3-encoding"))]
                {
                    Err("MP3 output is unavailable; rebuild with `--features mp3-encoding`".into())
                }
            }
            OutputFormat::Opus => {
                #[cfg(feature = "opus-encoding")]
                {
                    let output_lufs = analysis.lufs + gain_db(gain);
                    crate::opus::OpusStreamWriter::create(
                        output,
                        analysis.sample_rate,
                        analysis.frames,
                        analysis.channels,
                        &analysis.channel_roles,
                        plan.mp3_bitrate,
                        output_lufs,
                        options.opus_album_lufs,
                    )
                    .map(Self::Opus)
                }
                #[cfg(not(feature = "opus-encoding"))]
                {
                    Err(
                        "Ogg Opus output is unavailable; rebuild with `--features opus-encoding`"
                            .into(),
                    )
                }
            }
            OutputFormat::M4a | OutputFormat::Alac | OutputFormat::Vorbis => {
                #[cfg(feature = "ffmpeg-encoding")]
                {
                    let codec = match format {
                        OutputFormat::M4a => crate::aac::FfmpegCodec::Aac,
                        OutputFormat::Alac => crate::aac::FfmpegCodec::Alac,
                        OutputFormat::Vorbis => crate::aac::FfmpegCodec::Vorbis,
                        _ => unreachable!(),
                    };
                    crate::aac::AacStreamWriter::create_codec(
                        output,
                        analysis.sample_rate,
                        analysis.channels,
                        plan.mp3_bitrate,
                        codec,
                    )
                    .map(Self::Ffmpeg)
                }
                #[cfg(not(feature = "ffmpeg-encoding"))]
                {
                    Err(
                        "AAC/ALAC/Vorbis output is unavailable; rebuild with `--features ffmpeg-encoding`"
                            .into(),
                    )
                }
            }
        }
    }

    fn write_chunk(&mut self, planar: &[Vec<f32>]) -> Result<(), String> {
        match self {
            Self::Wav {
                output,
                kind,
                writer,
                lossless,
            } => {
                writer
                    .write_chunk(planar)
                    .map_err(|error| format!("write {}: {error}", output.display()))?;
                if let Some(lossless) = lossless.as_mut() {
                    lossless.observe_wave(writer.last_encoded_chunk(), *kind)?;
                }
                Ok(())
            }
            Self::Flac { writer, lossless } => {
                if let Some(lossless) = lossless.as_mut() {
                    writer.write_chunk_observed(planar, |interleaved, bits| {
                        lossless.observe_integer(interleaved, bits)
                    })
                } else {
                    writer.write_chunk(planar)
                }
            }
            #[cfg(feature = "mp3-encoding")]
            Self::Mp3(writer) => writer.write_chunk(planar),
            #[cfg(feature = "opus-encoding")]
            Self::Opus(writer) => writer.write_chunk(planar),
            #[cfg(feature = "ffmpeg-encoding")]
            Self::Ffmpeg(writer) => writer.write_chunk(planar),
        }
    }

    fn supports_borrowed_planar(&self) -> bool {
        matches!(
            self,
            Self::Wav {
                writer,
                lossless: None,
                ..
            } if writer.supports_borrowed_planar()
        )
    }

    fn write_normalized_borrowed_chunk(
        &mut self,
        planar: &[&[f32]],
        gain: f32,
        ceiling: f32,
    ) -> Result<(), String> {
        match self {
            Self::Wav {
                output,
                writer,
                lossless: None,
                ..
            } if writer.supports_borrowed_planar() => writer
                .write_normalized_borrowed_chunk(planar, gain, ceiling)
                .map_err(|error| format!("write {}: {error}", output.display())),
            _ => Err("stream writer does not support borrowed planar PCM".into()),
        }
    }

    fn finish(self) -> Result<Option<Analysis>, String> {
        match self {
            Self::Wav {
                output,
                writer,
                lossless,
                ..
            } => {
                writer
                    .finish()
                    .map_err(|error| format!("write {}: {error}", output.display()))?;
                Ok(lossless.map(LosslessAnalysisBuilder::finish))
            }
            Self::Flac { writer, lossless } => {
                writer.finish()?;
                Ok(lossless.map(LosslessAnalysisBuilder::finish))
            }
            #[cfg(feature = "mp3-encoding")]
            Self::Mp3(writer) => {
                writer.finish()?;
                Ok(None)
            }
            #[cfg(feature = "opus-encoding")]
            Self::Opus(writer) => {
                writer.finish()?;
                Ok(None)
            }
            #[cfg(feature = "ffmpeg-encoding")]
            Self::Ffmpeg(writer) => {
                writer.finish()?;
                Ok(None)
            }
        }
    }
}

fn stream_writer_work_can_overlap(
    formats: &[OutputFormat],
    options: StreamRenderOptions<'_>,
) -> bool {
    !formats.is_empty()
        && (formats
            .iter()
            .any(|format| matches!(format, OutputFormat::Mp3 | OutputFormat::Opus))
            || (options.capture_statistics
                && options.capture_lossless_verification
                && formats
                    .iter()
                    .all(|format| matches!(format, OutputFormat::Wav))))
}

fn stream_writer_pipeline_enabled(
    formats: &[OutputFormat],
    options: StreamRenderOptions<'_>,
) -> bool {
    // Nested file/album work already owns the shared worker budget. FLAC also
    // keeps its existing frame-parallel writer: putting that writer behind
    // this outer handoff reduces its measured parallelism rather than helping.
    rayon::current_num_threads() > 1
        && rayon::current_thread_index().is_none()
        && stream_writer_work_can_overlap(formats, options)
}

fn copy_pipeline_chunk(destination: &mut Vec<Vec<f32>>, source: &[Vec<f32>]) {
    destination.resize_with(source.len(), Vec::new);
    destination.truncate(source.len());
    for (output, input) in destination.iter_mut().zip(source) {
        output.resize(input.len(), 0.0);
        output.copy_from_slice(input);
    }
}

fn run_stream_writer_pipeline(
    mut writers: Vec<NormalizedStreamWriter>,
    input: Receiver<StreamWriterMessage>,
    recycled: SyncSender<Vec<Vec<f32>>>,
    failed: &AtomicBool,
) -> Result<StreamWriterOutcome, String> {
    let mut first_error = None;
    while let Ok(message) = input.recv() {
        match message {
            StreamWriterMessage::Chunk(chunk) => {
                if first_error.is_none() {
                    for writer in &mut writers {
                        if let Err(error) = writer.write_chunk(&chunk) {
                            failed.store(true, Ordering::Release);
                            first_error = Some(error);
                            break;
                        }
                    }
                }
                // The producer owns exactly STREAM_WRITER_PIPELINE_DEPTH slots,
                // so this bounded return channel cannot fill beyond capacity.
                let _ = recycled.send(chunk);
            }
            StreamWriterMessage::Finish => {
                if let Some(error) = first_error {
                    return Err(error);
                }
                let outputs = writers
                    .into_iter()
                    .map(NormalizedStreamWriter::finish)
                    .collect::<Result<Vec<_>, _>>()?;
                return Ok(StreamWriterOutcome::Finished(outputs));
            }
            StreamWriterMessage::Abort => {
                return first_error.map_or(Ok(StreamWriterOutcome::Aborted), Err);
            }
        }
    }
    first_error.map_or(Ok(StreamWriterOutcome::Aborted), Err)
}

#[allow(clippy::too_many_arguments)]
fn process_normalized_stream_pipelined(
    source: StreamSource<'_>,
    analysis: &Analysis,
    gain: f32,
    ceiling: f32,
    plan: &Plan,
    capture_statistics: bool,
    make_writers: impl FnOnce() -> Result<Vec<NormalizedStreamWriter>, String> + Send,
) -> Result<(Option<RenderStatistics>, Vec<Option<Analysis>>), String> {
    let (input_sender, input_receiver) = sync_channel(STREAM_WRITER_PIPELINE_DEPTH);
    let (recycle_sender, recycle_receiver) = sync_channel(STREAM_WRITER_PIPELINE_DEPTH);
    let (ready_sender, ready_receiver) = sync_channel(1);
    let (result_sender, result_receiver) = sync_channel(1);
    let failed = Arc::new(AtomicBool::new(false));
    let producer_failed = Arc::clone(&failed);
    let writer_failed = Arc::clone(&failed);
    let channels = usize::from(analysis.channels);
    let transfer_owned_chunks = source.descriptor.is_none()
        && plan.limiter.is_none()
        && (source.spool.is_some() || plan.output_sample_rate.is_none());

    let processing = rayon::scope(move |scope| {
        scope.spawn(move |_| {
            let writers = match make_writers() {
                Ok(writers) => {
                    if ready_sender.send(Ok(())).is_err() {
                        let _ = result_sender.send(Ok(StreamWriterOutcome::Aborted));
                        return;
                    }
                    writers
                }
                Err(error) => {
                    let _ = ready_sender.send(Err(error.clone()));
                    let _ = result_sender.send(Err(error));
                    return;
                }
            };
            let result = run_stream_writer_pipeline(
                writers,
                input_receiver,
                recycle_sender,
                writer_failed.as_ref(),
            );
            let _ = result_sender.send(result);
        });

        match ready_receiver.recv() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err("stream writer pipeline failed during setup".into()),
        }
        let processed = if transfer_owned_chunks {
            // The decoder or spool owns the first slot. One spare allocation
            // lets it continue immediately while the writer consumes that slot.
            let mut available = (1..STREAM_WRITER_PIPELINE_DEPTH)
                .map(|_| (0..channels).map(|_| Vec::new()).collect::<Vec<_>>())
                .collect::<Vec<_>>();
            process_normalized_stream_owned(
                source,
                analysis,
                gain,
                ceiling,
                capture_statistics,
                |chunk| {
                    if producer_failed.load(Ordering::Acquire) {
                        return Err("stream writer pipeline stopped after an encoder error".into());
                    }
                    loop {
                        match recycle_receiver.try_recv() {
                            Ok(chunk) => available.push(chunk),
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => {
                                return Err("stream writer pipeline stopped unexpectedly".into());
                            }
                        }
                    }
                    input_sender
                        .send(StreamWriterMessage::Chunk(chunk))
                        .map_err(|_| "stream writer pipeline stopped unexpectedly")?;
                    if producer_failed.load(Ordering::Acquire) {
                        return Err("stream writer pipeline stopped after an encoder error".into());
                    }
                    let recycled = if let Some(chunk) = available.pop() {
                        chunk
                    } else {
                        recycle_receiver
                            .recv()
                            .map_err(|_| "stream writer pipeline stopped unexpectedly")?
                    };
                    if producer_failed.load(Ordering::Acquire) {
                        return Err("stream writer pipeline stopped after an encoder error".into());
                    }
                    Ok(recycled)
                },
            )
        } else {
            let mut available = (0..STREAM_WRITER_PIPELINE_DEPTH)
                .map(|_| (0..channels).map(|_| Vec::new()).collect::<Vec<_>>())
                .collect::<Vec<_>>();
            process_normalized_stream(
                source,
                analysis,
                gain,
                ceiling,
                plan,
                capture_statistics,
                |planar| {
                    if producer_failed.load(Ordering::Acquire) {
                        return Err("stream writer pipeline stopped after an encoder error".into());
                    }
                    loop {
                        match recycle_receiver.try_recv() {
                            Ok(chunk) => available.push(chunk),
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => {
                                return Err("stream writer pipeline stopped unexpectedly".into());
                            }
                        }
                    }
                    let mut chunk = if let Some(chunk) = available.pop() {
                        chunk
                    } else {
                        recycle_receiver
                            .recv()
                            .map_err(|_| "stream writer pipeline stopped unexpectedly")?
                    };
                    copy_pipeline_chunk(&mut chunk, planar);
                    input_sender
                        .send(StreamWriterMessage::Chunk(chunk))
                        .map_err(|_| "stream writer pipeline stopped unexpectedly")?;
                    if producer_failed.load(Ordering::Acquire) {
                        return Err("stream writer pipeline stopped after an encoder error".into());
                    }
                    Ok(())
                },
            )
        };
        let terminal = if processed.is_ok() {
            StreamWriterMessage::Finish
        } else {
            StreamWriterMessage::Abort
        };
        if input_sender.send(terminal).is_err() && processed.is_ok() {
            return Err("stream writer pipeline stopped unexpectedly".into());
        }
        processed
    });
    let writing = result_receiver
        .recv()
        .map_err(|_| "stream writer pipeline stopped without a result")?;

    match writing {
        // A writer handling an earlier chunk precedes any processing failure
        // observed while later chunks were in flight.
        Err(error) => Err(error),
        Ok(StreamWriterOutcome::Finished(outputs)) => Ok((processing?, outputs)),
        Ok(StreamWriterOutcome::Aborted) => match processing {
            Err(error) => Err(error),
            Ok(_) => Err("stream writer pipeline aborted unexpectedly".into()),
        },
    }
}

fn process_normalized_stream_owned(
    source: StreamSource<'_>,
    analysis: &Analysis,
    gain: f32,
    ceiling: f32,
    capture_statistics: bool,
    mut write: impl FnMut(Vec<Vec<f32>>) -> Result<Vec<Vec<f32>>, String>,
) -> Result<Option<RenderStatistics>, String> {
    let StreamSource {
        path: input,
        descriptor,
        spool: source_spool,
    } = source;
    if descriptor.is_some() {
        return Err("owned render pipeline does not accept descriptor-bound input".into());
    }
    let mut statistics = capture_statistics.then(|| RenderStatisticsBuilder::new(analysis));
    let mut process = |mut planar: Vec<Vec<f32>>| {
        if statistics.is_none() {
            for channel in &mut planar {
                simd::apply_gain_and_hard_clip(channel, gain, ceiling);
            }
            return write(planar);
        }
        let observed = statistics.as_mut().unwrap();
        observed.observe_input(&planar);
        apply_gain(&mut planar, gain);
        observed.observe_post_gain(&planar, ceiling);
        for channel in &mut planar {
            simd::hard_clip(channel, ceiling);
        }
        observed.observe_protected(&planar)?;
        write(planar)
    };
    if let Some(spool) = source_spool {
        spool.replay_owned(&mut process)?;
    } else {
        decoder::decode_stream_owned_with_declared_frames(input, |info, _, planar| {
            if info.sample_rate != analysis.sample_rate {
                return Err(format!(
                    "owned stream pipeline expected {} Hz input, got {} Hz",
                    analysis.sample_rate, info.sample_rate
                ));
            }
            process(planar)
        })?;
    }
    Ok(statistics.map(|statistics| statistics.finish(None)))
}

fn normalize_stream(
    source: StreamSource<'_>,
    output: &Path,
    analysis: &Analysis,
    gain: f32,
    plan: &Plan,
    format: OutputFormat,
    options: StreamRenderOptions<'_>,
) -> Result<StreamRenderResult, String> {
    let ceiling = 10.0_f64.powf(plan.ceiling_db / 20.0) as f32;
    if stream_writer_pipeline_enabled(&[format], options) {
        let input = source.path;
        let (statistics, mut lossless_outputs) = process_normalized_stream_pipelined(
            source,
            analysis,
            gain,
            ceiling,
            plan,
            options.capture_statistics,
            move || {
                NormalizedStreamWriter::create(input, output, analysis, gain, plan, format, options)
                    .map(|writer| vec![writer])
            },
        )?;
        return Ok(StreamRenderResult {
            statistics,
            lossless_output: lossless_outputs
                .pop()
                .ok_or_else(|| "stream writer pipeline omitted its output".to_string())?,
        });
    }
    let mut writer =
        NormalizedStreamWriter::create(source.path, output, analysis, gain, plan, format, options)?;
    if plan.limiter.is_none()
        && !options.capture_statistics
        && writer.supports_borrowed_planar()
        && source
            .spool
            .as_deref()
            .is_some_and(PcmSpool::can_replay_borrowed)
    {
        source
            .spool
            .as_deref()
            .expect("borrowed spool eligibility checked above")
            .replay_borrowed(|planar| {
                writer.write_normalized_borrowed_chunk(planar, gain, ceiling)
            })?;
        return Ok(StreamRenderResult {
            statistics: None,
            lossless_output: writer.finish()?,
        });
    }
    let statistics = process_normalized_stream(
        source,
        analysis,
        gain,
        ceiling,
        plan,
        options.capture_statistics,
        |planar| writer.write_chunk(planar),
    )?;
    Ok(StreamRenderResult {
        statistics,
        lossless_output: writer.finish()?,
    })
}

fn normalize_streams(
    source: StreamSource<'_>,
    outputs: &[PathBuf],
    analysis: &Analysis,
    gain: f32,
    plan: &Plan,
    formats: &[OutputFormat],
    options: StreamRenderOptions<'_>,
) -> Result<MultiStreamRenderResult, String> {
    if outputs.len() != formats.len() {
        return Err("stream output/format count mismatch".into());
    }
    let ceiling = 10.0_f64.powf(plan.ceiling_db / 20.0) as f32;
    if stream_writer_pipeline_enabled(formats, options) {
        let input = source.path;
        let (statistics, lossless_outputs) = process_normalized_stream_pipelined(
            source,
            analysis,
            gain,
            ceiling,
            plan,
            options.capture_statistics,
            move || {
                outputs
                    .iter()
                    .zip(formats)
                    .map(|(output, format)| {
                        NormalizedStreamWriter::create(
                            input, output, analysis, gain, plan, *format, options,
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()
            },
        )?;
        return Ok(MultiStreamRenderResult {
            statistics,
            lossless_outputs,
        });
    }
    let mut writers = outputs
        .iter()
        .zip(formats)
        .map(|(output, format)| {
            NormalizedStreamWriter::create(
                source.path,
                output,
                analysis,
                gain,
                plan,
                *format,
                options,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let statistics = process_normalized_stream(
        source,
        analysis,
        gain,
        ceiling,
        plan,
        options.capture_statistics,
        |planar| {
            for writer in &mut writers {
                writer.write_chunk(planar)?;
            }
            Ok(())
        },
    )?;
    let lossless_outputs = writers
        .into_iter()
        .map(NormalizedStreamWriter::finish)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(MultiStreamRenderResult {
        statistics,
        lossless_outputs,
    })
}

fn process_normalized_stream(
    source: StreamSource<'_>,
    analysis: &Analysis,
    gain: f32,
    ceiling: f32,
    plan: &Plan,
    capture_statistics: bool,
    mut write: impl FnMut(&[Vec<f32>]) -> Result<(), String>,
) -> Result<Option<RenderStatistics>, String> {
    let StreamSource {
        path: input,
        descriptor,
        spool: source_spool,
    } = source;
    let limiter_proven_idle = limiter_is_proven_idle(analysis, gain, ceiling, plan);
    let mut limiter = plan
        .limiter
        .map(|config| {
            TruePeakLimiter::new_finite(
                analysis.sample_rate,
                analysis.channels,
                plan.ceiling_db,
                config,
            )
        })
        .transpose()?;
    if limiter_proven_idle {
        limiter = None;
    }
    let statistics_interval = capture_statistics.then(|| limiter_statistics_interval(analysis));
    if let Some(statistics_interval) = statistics_interval {
        if let Some(limiter) = limiter.as_mut() {
            limiter.set_statistics_interval_frames(statistics_interval);
        }
    }
    let mut limiter_output = if limiter.is_some() {
        (0..analysis.channels)
            .map(|_| Vec::new())
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let mut statistics = capture_statistics.then(|| RenderStatisticsBuilder::new(analysis));
    if let Some(spool) = source_spool {
        spool.replay(|planar| {
            process_normalized_chunk(
                planar,
                gain,
                ceiling,
                &mut limiter,
                &mut limiter_output,
                &mut statistics,
                &mut write,
            )
        })?;
    } else {
        let mut converter: Option<SampleRateConverter> = None;
        let mut consume = |info: &decoder::StreamInfo, planar: &mut [Vec<f32>]| {
            if info.sample_rate == analysis.sample_rate {
                return process_normalized_chunk(
                    planar,
                    gain,
                    ceiling,
                    &mut limiter,
                    &mut limiter_output,
                    &mut statistics,
                    &mut write,
                );
            }
            if converter.is_none() {
                converter = Some(SampleRateConverter::new_with_expected_output(
                    info.sample_rate,
                    analysis.sample_rate,
                    analysis.frames,
                    analysis.channels as usize,
                    plan.resample_quality,
                )?);
            }
            converter.as_mut().unwrap().process(planar, |output| {
                process_normalized_chunk(
                    output,
                    gain,
                    ceiling,
                    &mut limiter,
                    &mut limiter_output,
                    &mut statistics,
                    &mut write,
                )
            })
        };
        if let Some(descriptor) = descriptor {
            decoder::decode_descriptor_stream_coalesced(descriptor, &mut consume)?;
        } else {
            decoder::decode_stream_coalesced(input, &mut consume)?;
        }
        if let Some(converter) = converter.as_mut() {
            converter.finish(|output| {
                process_normalized_chunk(
                    output,
                    gain,
                    ceiling,
                    &mut limiter,
                    &mut limiter_output,
                    &mut statistics,
                    &mut write,
                )
            })?;
        }
    }
    let limiter_statistics = if let Some(limiter) = limiter {
        let limiter_statistics = if capture_statistics {
            Some(limiter.finish_with_statistics_into(&mut limiter_output)?)
        } else {
            limiter.finish_into(&mut limiter_output)?;
            None
        };
        if limiter_output
            .first()
            .is_some_and(|channel| !channel.is_empty())
        {
            if let Some(statistics) = statistics.as_mut() {
                statistics.observe_protected(&limiter_output)?;
            }
            write(&limiter_output)?;
        }
        limiter_statistics
    } else if limiter_proven_idle {
        statistics_interval
            .map(|interval| LimiterStatistics::proven_idle(analysis.frames, interval))
    } else {
        None
    };
    Ok(statistics.map(|statistics| statistics.finish(limiter_statistics)))
}

fn limiter_statistics_interval(analysis: &Analysis) -> usize {
    const MAX_ENVELOPE_POINTS: usize = 10_000;
    let minimum_interval = (analysis.sample_rate as usize / 10).max(1);
    let bounded_interval = analysis.frames.div_ceil(MAX_ENVELOPE_POINTS).max(1);
    minimum_interval.max(bounded_interval)
}

/// Prove from a discrete-sample bound that the linked True Peak limiter cannot
/// leave unity gain. The sample peak and gain are multiplied as `f32`, exactly
/// matching the render pass; correctly rounded multiplication is monotonic for
/// the finite non-negative magnitudes accepted here. The True Peak helper then
/// expands that rounded maximum by the FIR phase L1 bound.
fn limiter_is_proven_idle(analysis: &Analysis, gain: f32, ceiling: f32, plan: &Plan) -> bool {
    if plan.limiter.is_none()
        || !analysis.sample_peak.is_finite()
        || analysis.sample_peak < 0.0
        || !gain.is_finite()
        || gain < 0.0
        || !ceiling.is_finite()
        || ceiling < 0.0
    {
        return false;
    }
    let post_gain_sample_peak = analysis.sample_peak * gain;
    crate::dsp::truepeak::upper_bound_from_sample_peak(analysis.sample_rate, post_gain_sample_peak)
        <= f64::from(ceiling)
}

fn process_normalized_chunk(
    planar: &mut [Vec<f32>],
    gain: f32,
    ceiling: f32,
    limiter: &mut Option<TruePeakLimiter>,
    limiter_output: &mut [Vec<f32>],
    statistics: &mut Option<RenderStatisticsBuilder>,
    write: &mut impl FnMut(&[Vec<f32>]) -> Result<(), String>,
) -> Result<(), String> {
    if limiter.is_none() && statistics.is_none() {
        for channel in planar.iter_mut() {
            simd::apply_gain_and_hard_clip(channel, gain, ceiling);
        }
        return write(planar);
    }
    if let Some(statistics) = statistics.as_mut() {
        statistics.observe_input(planar);
    }
    apply_gain(planar, gain);
    if let Some(statistics) = statistics.as_mut() {
        statistics.observe_post_gain(planar, ceiling);
    }
    if let Some(limiter) = limiter.as_mut() {
        limiter.process_into(planar, limiter_output)?;
        if limiter_output
            .first()
            .is_some_and(|channel| !channel.is_empty())
        {
            if let Some(statistics) = statistics.as_mut() {
                statistics.observe_protected(limiter_output)?;
            }
            write(limiter_output)?;
        }
    } else {
        for channel in planar.iter_mut() {
            simd::hard_clip(channel, ceiling);
        }
        if let Some(statistics) = statistics.as_mut() {
            statistics.observe_protected(planar)?;
        }
        write(planar)?;
    }
    Ok(())
}

/// Streaming measurement of the exact PCM representation accepted by a
/// lossless writer. The scratch channels are retained across chunks; only the
/// compact BS.1770 gating-block history grows with programme duration.
struct LosslessAnalysisBuilder {
    analyzer: lufs::StreamingAnalyzer,
    sample_rate: u32,
    channels: u16,
    channel_roles: Vec<ChannelRole>,
    kind: PcmKind,
    scratch: Vec<Vec<f32>>,
    integer_scratch: Vec<Vec<i32>>,
    f64_scratch: Vec<Vec<f64>>,
}

impl LosslessAnalysisBuilder {
    fn new(
        sample_rate: u32,
        channels: u16,
        channel_roles: Vec<ChannelRole>,
        kind: PcmKind,
    ) -> Self {
        Self {
            analyzer: lufs::StreamingAnalyzer::new(sample_rate, channel_roles.clone()),
            sample_rate,
            channels,
            channel_roles,
            kind,
            scratch: vec![Vec::new(); channels as usize],
            integer_scratch: vec![Vec::new(); channels as usize],
            f64_scratch: vec![Vec::new(); channels as usize],
        }
    }

    fn observe_wave(&mut self, interleaved: &[u8], kind: PcmKind) -> Result<(), String> {
        debug_assert_eq!(kind, self.kind);
        match kind {
            PcmKind::S32 => {
                convert::decode_s32_planar_into(
                    interleaved,
                    self.channels as usize,
                    &mut self.integer_scratch,
                );
                self.analyzer.process_i32(&self.integer_scratch)
            }
            PcmKind::F64 => {
                convert::decode_f64_planar_into(
                    interleaved,
                    self.channels as usize,
                    &mut self.f64_scratch,
                );
                self.analyzer.process_f64(&self.f64_scratch)
            }
            // Every signed 24-bit code is exactly representable as f32, and
            // division by 2^23 is an exact exponent adjustment. Keep generated
            // S24 output on the paired K-weighting/True Peak fast path without
            // losing source-sample information; descriptor-based redecoding
            // uses the same representation and remains bit-identical.
            PcmKind::U8 | PcmKind::S16 | PcmKind::S24 | PcmKind::F32 => {
                convert::decode_planar_into(
                    interleaved,
                    kind,
                    self.channels as usize,
                    &mut self.scratch,
                );
                self.analyzer.process(&self.scratch)
            }
        }
    }

    fn observe_integer(&mut self, interleaved: &[i32], bits: usize) -> Result<(), String> {
        let channels = self.channels as usize;
        if !interleaved.len().is_multiple_of(channels) {
            return Err("lossless encoder produced a partial PCM frame".into());
        }
        let frames = interleaved.len() / channels;
        let scale = (1_u32 << (bits - 1)) as f32;
        for (channel_index, channel) in self.scratch.iter_mut().enumerate() {
            channel.clear();
            channel.reserve(frames);
            channel.extend(
                interleaved[channel_index..]
                    .iter()
                    .step_by(channels)
                    .map(|sample| *sample as f32 / scale),
            );
        }
        self.analyzer.process(&self.scratch)
    }

    fn finish(self) -> Analysis {
        let measured = self.analyzer.finish();
        Analysis {
            sample_rate: self.sample_rate,
            channels: self.channels,
            channel_roles: self.channel_roles,
            frames: measured.frames,
            kind: self.kind,
            lufs: measured.ebu.integrated_lufs,
            max_momentary_lufs: measured.ebu.max_momentary_lufs,
            max_short_term_lufs: measured.ebu.max_short_term_lufs,
            loudness_range_lu: measured.ebu.loudness_range_lu,
            rms_db: measured.rms_db,
            sample_peak: measured.sample_peak,
            true_peak: measured.true_peak,
            loudness_blocks: measured.ebu.gating_blocks,
        }
    }
}

struct RenderStatisticsBuilder {
    analyzer: lufs::StreamingAnalyzer,
    sample_rate: u32,
    kind: PcmKind,
    channels: u16,
    channel_roles: Vec<ChannelRole>,
    input_full_scale_exceeding_samples: u64,
    post_gain_full_scale_exceeding_samples: u64,
    post_gain_ceiling_exceeding_samples: u64,
    protected_full_scale_exceeding_samples: u64,
}

impl RenderStatisticsBuilder {
    fn new(analysis: &Analysis) -> Self {
        Self {
            analyzer: lufs::StreamingAnalyzer::new(
                analysis.sample_rate,
                analysis.channel_roles.clone(),
            ),
            sample_rate: analysis.sample_rate,
            kind: analysis.kind,
            channels: analysis.channels,
            channel_roles: analysis.channel_roles.clone(),
            input_full_scale_exceeding_samples: 0,
            post_gain_full_scale_exceeding_samples: 0,
            post_gain_ceiling_exceeding_samples: 0,
            protected_full_scale_exceeding_samples: 0,
        }
    }

    fn observe_input(&mut self, planar: &[Vec<f32>]) {
        self.input_full_scale_exceeding_samples += count_exceeding(planar, 1.0);
    }

    fn observe_post_gain(&mut self, planar: &[Vec<f32>], ceiling: f32) {
        self.post_gain_full_scale_exceeding_samples += count_exceeding(planar, 1.0);
        self.post_gain_ceiling_exceeding_samples += count_exceeding(planar, ceiling);
    }

    fn observe_protected(&mut self, planar: &[Vec<f32>]) -> Result<(), String> {
        self.protected_full_scale_exceeding_samples += count_exceeding(planar, 1.0);
        self.analyzer.process(planar)
    }

    fn finish(self, limiter: Option<LimiterStatistics>) -> RenderStatistics {
        let measured = self.analyzer.finish();
        RenderStatistics {
            intended: Analysis {
                sample_rate: self.sample_rate,
                channels: self.channels,
                channel_roles: self.channel_roles,
                frames: measured.frames,
                kind: self.kind,
                lufs: measured.ebu.integrated_lufs,
                max_momentary_lufs: measured.ebu.max_momentary_lufs,
                max_short_term_lufs: measured.ebu.max_short_term_lufs,
                loudness_range_lu: measured.ebu.loudness_range_lu,
                rms_db: measured.rms_db,
                sample_peak: measured.sample_peak,
                true_peak: measured.true_peak,
                loudness_blocks: measured.ebu.gating_blocks,
            },
            input_full_scale_exceeding_samples: self.input_full_scale_exceeding_samples,
            post_gain_full_scale_exceeding_samples: self.post_gain_full_scale_exceeding_samples,
            post_gain_ceiling_exceeding_samples: self.post_gain_ceiling_exceeding_samples,
            protected_full_scale_exceeding_samples: self.protected_full_scale_exceeding_samples,
            limiter,
        }
    }
}

fn count_exceeding(planar: &[Vec<f32>], threshold: f32) -> u64 {
    planar
        .iter()
        .flat_map(|channel| channel.iter())
        .filter(|sample| sample.abs() > threshold)
        .count() as u64
}

fn flac_bits(kind: PcmKind) -> Result<u16, String> {
    match kind {
        PcmKind::U8 | PcmKind::S16 => Ok(16),
        PcmKind::S24 | PcmKind::S32 | PcmKind::F32 | PcmKind::F64 => Ok(24),
    }
}

fn flac_persisted_channel_roles(channels: u16) -> Vec<ChannelRole> {
    crate::channel_layout::default_flac_channel_mask(channels)
        .map(|mask| crate::wav::reader::roles_from_wave_mask(mask, channels))
        .unwrap_or_default()
}

fn legacy_flac_channel_roles(channels: u16) -> Option<Vec<ChannelRole>> {
    Some(match channels {
        1..=6 => crate::wav::default_channel_roles(channels),
        7 => crate::wav::named_channel_layout("6.1")?,
        8 => crate::wav::named_channel_layout("7.1")?,
        _ => return None,
    })
}

fn legacy_wave_channel_roles(channels: u16) -> Option<Vec<ChannelRole>> {
    Some(match channels {
        1 | 2 => crate::wav::default_channel_roles(channels),
        6 => crate::wav::named_channel_layout("5.1")?,
        7 => crate::wav::named_channel_layout("6.1")?,
        8 => crate::wav::named_channel_layout("7.1")?,
        10 => crate::wav::named_channel_layout("5.1.4")?,
        12 => crate::wav::named_channel_layout("7.1.4")?,
        _ => return None,
    })
}

fn wave_persisted_channel_roles(channels: u16) -> Option<Vec<ChannelRole>> {
    let accepted = legacy_wave_channel_roles(channels)?;
    crate::wav::writer::persisted_channel_roles(&accepted).ok()
}

fn ffmpeg_persisted_channel_roles(channels: u16) -> Option<Vec<ChannelRole>> {
    Some(match channels {
        1 | 2 => crate::wav::default_channel_roles(channels),
        6 => crate::wav::reader::roles_from_wave_mask(0x0000_003f, 6),
        8 => crate::wav::reader::roles_from_wave_mask(0x0000_063f, 8),
        _ => return None,
    })
}

fn legacy_ffmpeg_channel_roles(channels: u16) -> Option<Vec<ChannelRole>> {
    Some(match channels {
        1 | 2 => crate::wav::default_channel_roles(channels),
        6 => crate::wav::named_channel_layout("5.1")?,
        8 => crate::wav::named_channel_layout("7.1")?,
        _ => return None,
    })
}

#[cfg(feature = "opus-encoding")]
fn legacy_opus_channel_roles(channels: u16) -> Option<Vec<ChannelRole>> {
    Some(match channels {
        1..=6 => crate::wav::default_channel_roles(channels),
        7 => crate::wav::named_channel_layout("6.1")?,
        8 => crate::wav::named_channel_layout("7.1")?,
        _ => return None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayoutAliasPolicy {
    ExactOnly,
    ExplicitLegacy,
}

impl LayoutAliasPolicy {
    fn for_override(
        channel_roles: Option<&[ChannelRole]>,
        analyzed_roles: &[ChannelRole],
    ) -> Result<Self, String> {
        match channel_roles {
            Some(roles) if roles != analyzed_roles => Err(
                "explicit channel roles do not match the roles bound to the supplied analysis"
                    .into(),
            ),
            Some(_) => Ok(Self::ExplicitLegacy),
            None => Ok(Self::ExactOnly),
        }
    }

    fn allows_legacy(self) -> bool {
        self == Self::ExplicitLegacy
    }
}

fn validate_representable_linear_db(name: &str, value: f64) -> Result<(), String> {
    let linear = 10.0_f64.powf(value / 20.0) as f32;
    if linear.is_finite() && linear > 0.0 {
        Ok(())
    } else {
        Err(format!(
            "normalization plan {name} is outside the representable linear f32 range"
        ))
    }
}

fn validate_mp3_sample_rate(sample_rate: u32) -> Result<(), String> {
    if matches!(
        sample_rate,
        8_000 | 11_025 | 12_000 | 16_000 | 22_050 | 24_000 | 32_000 | 44_100 | 48_000
    ) {
        Ok(())
    } else {
        Err(format!(
            "MP3 output sample rate {sample_rate} Hz is unsupported; use 8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, or 48000 Hz"
        ))
    }
}

fn validate_mp3_bitrate(bitrate_kbps: i32, sample_rate: Option<u32>) -> Result<(), String> {
    const MPEG1: &[i32] = &[
        32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
    ];
    const MPEG2: &[i32] = &[8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    let allowed = match sample_rate {
        Some(32_000 | 44_100 | 48_000) => MPEG1,
        Some(8_000 | 11_025 | 12_000 | 16_000 | 22_050 | 24_000) => MPEG2,
        Some(sample_rate) => {
            validate_mp3_sample_rate(sample_rate)?;
            unreachable!("validated MP3 sample rate belongs to an MPEG family")
        }
        None => {
            if MPEG1.contains(&bitrate_kbps) || MPEG2.contains(&bitrate_kbps) {
                return Ok(());
            }
            return Err("MP3 bitrate is not a supported MPEG CBR value".into());
        }
    };
    if allowed.contains(&bitrate_kbps) {
        Ok(())
    } else {
        Err(format!(
            "MP3 bitrate {bitrate_kbps} kbps is unsupported at {} Hz",
            sample_rate.expect("sample rate selected an MPEG family")
        ))
    }
}

fn validate_aac_sample_rate(sample_rate: u32) -> Result<(), String> {
    const AAC_SAMPLE_RATES: [u32; 12] = [
        8_000, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000, 64_000, 88_200,
        96_000,
    ];
    if AAC_SAMPLE_RATES.contains(&sample_rate) {
        Ok(())
    } else {
        Err(format!(
            "AAC encoder cannot preserve unsupported sample rate {sample_rate} Hz"
        ))
    }
}

fn aac_bitrate_range(sample_rate: u32, channels: u16) -> (i32, i32) {
    let maximum = (u64::from(sample_rate) * 6 * u64::from(channels) / 1_000).min(1_024) as i32;
    (8, maximum)
}

fn vorbis_bitrate_range(sample_rate: u32, channels: u16) -> Option<(i32, i32)> {
    let (minimum_per_channel_bps, maximum_per_channel_bps) = match (sample_rate, channels) {
        (8_000..=8_999, 2) => (6_000_i64, 32_000_i64),
        (8_000..=8_999, 1 | 6 | 8) => (8_000, 42_000),
        (9_000..=14_999, 2) => (8_000, 44_000),
        (9_000..=14_999, 1 | 6 | 8) => (12_000, 50_000),
        (15_000..=18_999, 2) => (12_000, 86_000),
        (15_000..=18_999, 1 | 6 | 8) => (16_000, 100_000),
        (19_000..=25_999, 2) => (15_000, 86_000),
        (19_000..=25_999, 1 | 6 | 8) => (16_000, 90_000),
        (26_000..=39_999, 2) => (18_000, 190_000),
        (26_000..=39_999, 1 | 6 | 8) => (30_000, 190_000),
        (40_000..=50_000, 2) => (22_500, 250_001),
        (40_000..=70_000, 6) => (14_000, 240_001),
        (40_000..=50_000, 1 | 8) => (32_000, 240_001),
        _ => return None,
    };
    let channels = i64::from(channels);
    let exact_minimum_kbps = (minimum_per_channel_bps * channels + 999).checked_div(1_000)?;
    let minimum_kbps = (exact_minimum_kbps + 7).checked_div(8)?.checked_mul(8)?;
    let maximum_kbps = (maximum_per_channel_bps * channels)
        .checked_div(1_000)?
        .min(1_024);
    Some((
        i32::try_from(minimum_kbps).ok()?,
        i32::try_from(maximum_kbps).ok()?,
    ))
}

fn validate_plan_format_settings(
    plan: &Plan,
    format: OutputFormat,
    sample_rate: Option<u32>,
    channels: Option<u16>,
) -> Result<(), String> {
    match format {
        OutputFormat::Wav | OutputFormat::Flac | OutputFormat::Alac => Ok(()),
        OutputFormat::Mp3 => {
            if let Some(sample_rate) = sample_rate {
                validate_mp3_sample_rate(sample_rate)?;
            }
            validate_mp3_bitrate(plan.mp3_bitrate, sample_rate)?;
            if !(0..=9).contains(&plan.mp3_quality) {
                return Err("MP3 encoder quality must be between 0 and 9".into());
            }
            Ok(())
        }
        OutputFormat::Opus => {
            let bitrate_bps = plan
                .mp3_bitrate
                .checked_mul(1_000)
                .ok_or_else(|| "Opus bitrate exceeds the supported range".to_string())?;
            if !(500..=512_000).contains(&bitrate_bps) {
                return Err(
                    "Opus bitrate must be between 1 and 512 kbps at the integer-kbps API".into(),
                );
            }
            Ok(())
        }
        OutputFormat::M4a => {
            if let Some(sample_rate) = sample_rate {
                validate_aac_sample_rate(sample_rate)?;
            }
            let (minimum, maximum) = match (sample_rate, channels) {
                (Some(sample_rate), Some(channels)) => aac_bitrate_range(sample_rate, channels),
                _ => (8, 1_024),
            };
            if !(minimum..=maximum).contains(&plan.mp3_bitrate) {
                return Err(format!(
                    "AAC bitrate must be between {minimum} and {maximum} kbps for the selected signal"
                ));
            }
            Ok(())
        }
        OutputFormat::Vorbis => {
            if let Some(sample_rate) = sample_rate {
                if !(8_000..=70_000).contains(&sample_rate) {
                    return Err(format!(
                        "Vorbis managed-bitrate encoding does not support {sample_rate} Hz"
                    ));
                }
            }
            if let (Some(sample_rate), Some(channels)) = (sample_rate, channels) {
                let (minimum, maximum) =
                    vorbis_bitrate_range(sample_rate, channels).ok_or_else(|| {
                        format!(
                            "Vorbis {channels}-channel managed-bitrate encoding does not support {sample_rate} Hz"
                        )
                    })?;
                if !(minimum..=maximum).contains(&plan.mp3_bitrate) {
                    return Err(format!(
                        "Vorbis {channels}-channel bitrate at {sample_rate} Hz must be between {minimum} and {maximum} kbps"
                    ));
                }
            } else if !(8..=1_024).contains(&plan.mp3_bitrate) {
                return Err("Vorbis bitrate must be between 8 and 1024 kbps".into());
            }
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_plan_for_signal(
    plan: &Plan,
    format: OutputFormat,
    sample_rate: u32,
    channels: u16,
    roles: &[ChannelRole],
    source_kind: PcmKind,
    channel_layout: Option<&ChannelLayoutDescriptor>,
    alias_policy: LayoutAliasPolicy,
) -> Result<(), String> {
    plan.validate()?;
    validate_supported_output_sample_rate(sample_rate)?;
    if plan
        .output_sample_rate
        .is_some_and(|requested| requested != sample_rate)
    {
        return Err(format!(
            "plan output sample rate does not match the {sample_rate} Hz output-domain signal"
        ));
    }
    if channels == 0 {
        return Err("output signal must contain at least one channel".into());
    }
    validate_output_channel_layout_with_descriptor(
        format,
        channels,
        roles,
        channel_layout,
        alias_policy,
    )?;
    validate_plan_format_settings(plan, format, Some(sample_rate), Some(channels))?;
    validate_output_encoder_available(format)?;
    if format == OutputFormat::Wav
        && plan.dither
        && plan.output_kind.unwrap_or(source_kind).is_float()
    {
        return Err("WAVE dither requires an integer PCM output kind".into());
    }
    Ok(())
}

pub(crate) fn validate_output_encoder_available(format: OutputFormat) -> Result<(), String> {
    match format {
        #[cfg(not(feature = "mp3-encoding"))]
        OutputFormat::Mp3 => {
            Err("MP3 output is unavailable; rebuild with `--features mp3-encoding`".into())
        }
        #[cfg(not(feature = "opus-encoding"))]
        OutputFormat::Opus => {
            Err("Ogg Opus output is unavailable; rebuild with `--features opus-encoding`".into())
        }
        #[cfg(not(feature = "ffmpeg-encoding"))]
        OutputFormat::M4a | OutputFormat::Alac | OutputFormat::Vorbis => Err(
            "AAC/ALAC/Vorbis output is unavailable; rebuild with `--features ffmpeg-encoding`"
                .into(),
        ),
        #[cfg(feature = "ffmpeg-encoding")]
        OutputFormat::M4a => crate::aac::preflight_ffmpeg(crate::aac::FfmpegCodec::Aac),
        #[cfg(feature = "ffmpeg-encoding")]
        OutputFormat::Alac => crate::aac::preflight_ffmpeg(crate::aac::FfmpegCodec::Alac),
        #[cfg(feature = "ffmpeg-encoding")]
        OutputFormat::Vorbis => crate::aac::preflight_ffmpeg(crate::aac::FfmpegCodec::Vorbis),
        _ => Ok(()),
    }
}

fn validate_supported_output_sample_rate(sample_rate: u32) -> Result<(), String> {
    if (MIN_DECODE_SAMPLE_RATE_HZ..=MAX_DECODE_SAMPLE_RATE_HZ).contains(&sample_rate) {
        Ok(())
    } else {
        Err(format!(
            "output sample rate {sample_rate} Hz is outside Forge's supported {MIN_DECODE_SAMPLE_RATE_HZ}..={MAX_DECODE_SAMPLE_RATE_HZ} Hz range"
        ))
    }
}

pub(crate) fn validate_plan_output_sample_rate(plan: &Plan) -> Result<(), String> {
    if let Some(sample_rate) = plan.output_sample_rate {
        validate_supported_output_sample_rate(sample_rate)?;
    }
    Ok(())
}

/// Reject an output before creating its encoder unless the current public
/// channel-role model proves that the muxer will retain the same speaker order.
/// Generic aliases are accepted only at APIs where the caller explicitly
/// supplied the layout. Decoder-derived and unqualified precomputed layouts
/// must carry exact speaker positions so lossy metadata cannot pass preflight.
fn validate_output_channel_layout_with_descriptor(
    format: OutputFormat,
    channels: u16,
    roles: &[ChannelRole],
    channel_layout: Option<&ChannelLayoutDescriptor>,
    alias_policy: LayoutAliasPolicy,
) -> Result<(), String> {
    if let Some(layout) =
        channel_layout.filter(|_| matches!(format, OutputFormat::Wav | OutputFormat::Flac))
    {
        if layout.channel_count() != usize::from(channels) || layout.channel_roles() != roles {
            return Err("exact channel layout does not match the measured signal".into());
        }
        if !layout.is_measurement_ready() {
            return Err("exact channel layout is not a complete physical-speaker mapping".into());
        }
        crate::wav::writer::channel_mask_from_descriptor(layout).map_err(|error| {
            format!("{format:?} output cannot preserve the exact channel layout: {error}")
        })?;
        return Ok(());
    }
    validate_output_channel_layout(format, channels, roles, alias_policy)
}

fn validate_output_channel_layout(
    format: OutputFormat,
    channels: u16,
    roles: &[ChannelRole],
    alias_policy: LayoutAliasPolicy,
) -> Result<(), String> {
    if roles.len() != usize::from(channels) {
        return Err(format!(
            "output channel-role count {} does not match the {channels}-channel signal",
            roles.len()
        ));
    }
    let exact = match format {
        OutputFormat::Wav => wave_persisted_channel_roles(channels).as_deref() == Some(roles),
        OutputFormat::Flac => {
            (1..=8).contains(&channels) && flac_persisted_channel_roles(channels) == roles
        }
        OutputFormat::Opus => {
            #[cfg(feature = "opus-encoding")]
            {
                crate::opus::persisted_channel_roles(channels).as_deref() == Some(roles)
            }
            #[cfg(not(feature = "opus-encoding"))]
            {
                false
            }
        }
        OutputFormat::Mp3 => {
            (1..=2).contains(&channels) && crate::wav::default_channel_roles(channels) == roles
        }
        OutputFormat::M4a | OutputFormat::Alac if channels > 2 => false,
        OutputFormat::M4a | OutputFormat::Alac | OutputFormat::Vorbis => {
            ffmpeg_persisted_channel_roles(channels).as_deref() == Some(roles)
        }
    };
    let legacy = alias_policy.allows_legacy()
        && match format {
            OutputFormat::Wav => legacy_wave_channel_roles(channels).as_deref() == Some(roles),
            OutputFormat::Flac => legacy_flac_channel_roles(channels).as_deref() == Some(roles),
            OutputFormat::Opus => {
                #[cfg(feature = "opus-encoding")]
                {
                    legacy_opus_channel_roles(channels).as_deref() == Some(roles)
                }
                #[cfg(not(feature = "opus-encoding"))]
                {
                    false
                }
            }
            OutputFormat::Mp3 => {
                (1..=2).contains(&channels) && crate::wav::default_channel_roles(channels) == roles
            }
            OutputFormat::M4a | OutputFormat::Alac if channels > 2 => false,
            OutputFormat::M4a | OutputFormat::Alac | OutputFormat::Vorbis => {
                legacy_ffmpeg_channel_roles(channels).as_deref() == Some(roles)
            }
        };
    let preserved = exact || legacy;
    if preserved {
        Ok(())
    } else {
        Err(format!(
            "{format:?} output cannot preserve the measured {channels}-channel speaker roles/order; provide an explicit canonical channel layout supported by the output format"
        ))
    }
}

fn apply_gain(planar: &mut [Vec<f32>], gain: f32) {
    for channel in planar {
        simd::apply_gain(channel, gain);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::{default_channel_roles, named_channel_layout};

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

    #[test]
    fn output_layout_preflight_allows_only_roles_the_writer_can_prove() {
        let stereo = default_channel_roles(2);
        let stereo_formats = [
            OutputFormat::Wav,
            OutputFormat::Flac,
            OutputFormat::Mp3,
            OutputFormat::M4a,
            OutputFormat::Alac,
            OutputFormat::Vorbis,
        ];
        for format in stereo_formats {
            assert!(validate_output_channel_layout(
                format,
                2,
                &stereo,
                LayoutAliasPolicy::ExactOnly,
            )
            .is_ok());
        }
        #[cfg(feature = "opus-encoding")]
        assert!(validate_output_channel_layout(
            OutputFormat::Opus,
            2,
            &stereo,
            LayoutAliasPolicy::ExactOnly,
        )
        .is_ok());

        let front_left_and_center =
            crate::wav::reader::roles_from_wave_mask((1 << 0) | (1 << 2), 2);
        for format in [
            OutputFormat::Wav,
            OutputFormat::Flac,
            OutputFormat::Mp3,
            OutputFormat::Opus,
            OutputFormat::M4a,
            OutputFormat::Alac,
            OutputFormat::Vorbis,
        ] {
            let error = validate_output_channel_layout(
                format,
                2,
                &front_left_and_center,
                LayoutAliasPolicy::ExactOnly,
            )
            .unwrap_err();
            assert!(error.contains("cannot preserve"), "{format:?}: {error}");
        }

        let mono_lfe = [ChannelRole::Lfe];
        for format in [OutputFormat::Wav, OutputFormat::Flac, OutputFormat::Opus] {
            let error =
                validate_output_channel_layout(format, 1, &mono_lfe, LayoutAliasPolicy::ExactOnly)
                    .unwrap_err();
            assert!(error.contains("cannot preserve"), "{error}");
        }

        let quad = default_channel_roles(4);
        assert!(validate_output_channel_layout(
            OutputFormat::Flac,
            4,
            &quad,
            LayoutAliasPolicy::ExactOnly,
        )
        .is_err());
        assert!(validate_output_channel_layout(
            OutputFormat::Wav,
            4,
            &quad,
            LayoutAliasPolicy::ExactOnly,
        )
        .is_err());

        let five_one_four = named_channel_layout("5.1.4").unwrap();
        assert!(validate_output_channel_layout(
            OutputFormat::Wav,
            10,
            &five_one_four,
            LayoutAliasPolicy::ExactOnly,
        )
        .is_err());
        assert!(validate_output_channel_layout(
            OutputFormat::Flac,
            10,
            &five_one_four,
            LayoutAliasPolicy::ExplicitLegacy,
        )
        .is_err());
        let exact_five_one_four = crate::wav::reader::roles_from_wave_mask(0x0002_d03f, 10);
        assert!(validate_output_channel_layout(
            OutputFormat::Wav,
            10,
            &exact_five_one_four,
            LayoutAliasPolicy::ExactOnly,
        )
        .is_ok());
        assert!(validate_output_channel_layout(
            OutputFormat::Wav,
            6,
            &stereo,
            LayoutAliasPolicy::ExplicitLegacy,
        )
        .is_err());

        let wave_five_one = crate::wav::reader::roles_from_wave_mask(0x0000_003f, 6);
        let flac_five_one = flac_persisted_channel_roles(6);
        assert_eq!(wave_five_one, flac_five_one);
        for format in [OutputFormat::Wav, OutputFormat::Flac] {
            assert!(validate_output_channel_layout(
                format,
                6,
                &wave_five_one,
                LayoutAliasPolicy::ExactOnly,
            )
            .is_ok());
        }
        assert!(validate_output_channel_layout(
            OutputFormat::M4a,
            6,
            &wave_five_one,
            LayoutAliasPolicy::ExplicitLegacy,
        )
        .is_err());
        assert!(validate_output_channel_layout(
            OutputFormat::Alac,
            6,
            &wave_five_one,
            LayoutAliasPolicy::ExplicitLegacy,
        )
        .is_err());
        assert!(validate_output_channel_layout(
            OutputFormat::Vorbis,
            6,
            &wave_five_one,
            LayoutAliasPolicy::ExactOnly,
        )
        .is_ok());

        let explicit_side_five_one = crate::wav::reader::roles_from_wave_mask(0x0000_060f, 6);
        assert_ne!(explicit_side_five_one, flac_five_one);
        assert!(validate_output_channel_layout(
            OutputFormat::Flac,
            6,
            &explicit_side_five_one,
            LayoutAliasPolicy::ExplicitLegacy,
        )
        .is_err());
        assert!(validate_output_channel_layout(
            OutputFormat::Wav,
            6,
            &explicit_side_five_one,
            LayoutAliasPolicy::ExplicitLegacy,
        )
        .is_err());

        // Decoder-derived generic metadata cannot prove positions. Explicit
        // public API input retains the historical canonical alias contract.
        let legacy_five_one = named_channel_layout("5.1").unwrap();
        assert!(LayoutAliasPolicy::for_override(Some(&legacy_five_one), &wave_five_one).is_err());
        assert_eq!(
            LayoutAliasPolicy::for_override(Some(&legacy_five_one), &legacy_five_one).unwrap(),
            LayoutAliasPolicy::ExplicitLegacy
        );
        for format in [OutputFormat::Wav, OutputFormat::Flac] {
            assert!(validate_output_channel_layout(
                format,
                6,
                &legacy_five_one,
                LayoutAliasPolicy::ExactOnly,
            )
            .is_err());
            assert!(validate_output_channel_layout(
                format,
                6,
                &legacy_five_one,
                LayoutAliasPolicy::ExplicitLegacy,
            )
            .is_ok());
        }

        let seven_one = ffmpeg_persisted_channel_roles(8).unwrap();
        assert!(validate_output_channel_layout(
            OutputFormat::M4a,
            8,
            &seven_one,
            LayoutAliasPolicy::ExactOnly,
        )
        .is_err());
        assert!(validate_output_channel_layout(
            OutputFormat::Vorbis,
            8,
            &seven_one,
            LayoutAliasPolicy::ExactOnly,
        )
        .is_ok());
        assert!(validate_output_channel_layout(
            OutputFormat::Alac,
            8,
            &seven_one,
            LayoutAliasPolicy::ExplicitLegacy,
        )
        .is_err());

        #[cfg(feature = "opus-encoding")]
        {
            let exact_opus = crate::opus::persisted_channel_roles(6).unwrap();
            assert!(validate_output_channel_layout(
                OutputFormat::Opus,
                6,
                &exact_opus,
                LayoutAliasPolicy::ExactOnly,
            )
            .is_ok());
            assert!(validate_output_channel_layout(
                OutputFormat::Opus,
                6,
                &legacy_five_one,
                LayoutAliasPolicy::ExactOnly,
            )
            .is_err());
            assert!(validate_output_channel_layout(
                OutputFormat::Opus,
                6,
                &legacy_five_one,
                LayoutAliasPolicy::ExplicitLegacy,
            )
            .is_ok());
        }
    }

    #[test]
    fn unsupported_multichannel_aac_fails_before_output_staging() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("missing-input.wav");
        let output = directory.path().join("must-not-exist.m4a");
        let mut cached = analysis(-23.0, -6.0);
        cached.channels = 6;
        cached.channel_roles = crate::wav::reader::roles_from_wave_mask(0x003f, 6);

        let error = normalize_one_preanalyzed_with_roles(
            &input,
            &output,
            &plan(),
            OutputFormat::M4a,
            None,
            &cached,
        )
        .unwrap_err();
        assert!(error.contains("cannot preserve"), "{error}");
        assert!(!output.exists());
    }

    #[test]
    fn unsupported_output_sample_rates_fail_before_input_or_destination_access() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("missing-input.wav");
        let output = directory.path().join("existing.wav");
        for sample_rate in [7_999, 384_001] {
            std::fs::write(&output, b"existing destination").unwrap();
            let mut render_plan = plan();
            render_plan.output_sample_rate = Some(sample_rate);
            let error = normalize_one(&input, &output, &render_plan, OutputFormat::Wav)
                .expect_err("unsupported output sample rate must fail before decoding");
            assert!(
                error.contains(&format!("sample rate {sample_rate} Hz")),
                "{error}"
            );
            assert_eq!(std::fs::read(&output).unwrap(), b"existing destination");
        }
    }

    #[test]
    fn public_write_rejects_invalid_buffer_geometry_before_touching_any_destination() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("existing.output");
        let valid = AudioBuffer {
            sample_rate: 48_000,
            channels: 2,
            frames: 32,
            data: vec![vec![0.0; 32], vec![0.0; 32]],
            channel_roles: default_channel_roles(2),
            source_kind: PcmKind::F32,
        };
        let mut missing_plane = valid.clone();
        missing_plane.data.pop();
        let mut short_plane = valid;
        short_plane.data[1].pop();

        for (name, buffer) in [
            ("missing-plane", missing_plane),
            ("short-plane", short_plane),
        ] {
            for format in [
                OutputFormat::Wav,
                OutputFormat::Flac,
                OutputFormat::Mp3,
                OutputFormat::Opus,
                OutputFormat::M4a,
                OutputFormat::Alac,
                OutputFormat::Vorbis,
            ] {
                std::fs::write(&destination, b"existing destination").unwrap();
                let error = write(&buffer, &destination, &plan(), format).unwrap_err();
                assert!(
                    error.contains("audio buffer"),
                    "{name}, {format:?}: {error}"
                );
                assert_eq!(
                    std::fs::read(&destination).unwrap(),
                    b"existing destination",
                    "{name}, {format:?}"
                );
            }
        }
    }

    #[cfg(feature = "ffmpeg-encoding")]
    #[test]
    fn public_ffmpeg_write_failure_preserves_existing_destination() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("existing.ogg");
        std::fs::write(&destination, b"existing destination").unwrap();
        let buffer = AudioBuffer {
            sample_rate: 48_000,
            channels: 2,
            frames: 32,
            data: vec![vec![0.0; 32], vec![0.0; 32]],
            channel_roles: default_channel_roles(2),
            source_kind: PcmKind::F32,
        };
        let mut render_plan = plan();
        render_plan.mp3_bitrate = 1;

        assert!(write(&buffer, &destination, &render_plan, OutputFormat::Vorbis,).is_err());
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"existing destination"
        );
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
            output_sample_rate: None,
            resample_quality: ResampleQuality::Balanced,
        }
    }

    fn write_mono_tone(path: &Path, amplitude: f32) {
        let frames = 48_000;
        let samples = (0..frames)
            .map(|frame| {
                amplitude * (std::f32::consts::TAU * 440.0 * frame as f32 / 48_000.0).sin()
            })
            .collect::<Vec<_>>();
        WavWriter::write(
            path,
            &AudioBuffer {
                sample_rate: 48_000,
                channels: 1,
                frames,
                data: vec![samples],
                channel_roles: default_channel_roles(1),
                source_kind: PcmKind::F32,
            },
            PcmKind::S16,
            false,
        )
        .unwrap();
    }

    #[test]
    fn every_non_finite_plan_value_fails_before_input_or_output_access() {
        let directory = tempfile::tempdir().unwrap();
        let missing_input = directory.path().join("missing.wav");
        let output = directory.path().join("existing.wav");
        let mut cases = Vec::new();
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut value = plan();
            value.target_lufs = invalid;
            cases.push(("target_lufs", value));
            let mut value = plan();
            value.target_peak_db = invalid;
            cases.push(("target_peak_db", value));
            let mut value = plan();
            value.target_rms_db = invalid;
            cases.push(("target_rms_db", value));
            let mut value = plan();
            value.ceiling_db = invalid;
            cases.push(("ceiling_db", value));
            let mut value = plan();
            value.max_gain_db = Some(invalid);
            cases.push(("max_gain_db", value));
            let mut value = plan();
            value.limiter = Some(LimiterConfig {
                lookahead_ms: invalid,
                release_ms: 100.0,
            });
            cases.push(("limiter look-ahead", value));
            let mut value = plan();
            value.limiter = Some(LimiterConfig {
                lookahead_ms: 5.0,
                release_ms: invalid,
            });
            cases.push(("limiter release", value));
        }

        for (field, invalid_plan) in cases {
            std::fs::write(&output, b"existing destination").unwrap();
            let error = normalize_one(&missing_input, &output, &invalid_plan, OutputFormat::Wav)
                .unwrap_err();
            assert!(error.contains(field), "{field}: {error}");
            assert_eq!(std::fs::read(&output).unwrap(), b"existing destination");
        }
    }

    #[test]
    fn format_numeric_settings_fail_before_input_or_output_access() {
        let directory = tempfile::tempdir().unwrap();
        let missing_input = directory.path().join("missing.wav");
        let output = directory.path().join("existing.bin");
        let cases = [
            (OutputFormat::Mp3, 7, 2, "MP3 bitrate"),
            (OutputFormat::Mp3, 192, 10, "quality"),
            (OutputFormat::Opus, 513, 2, "Opus bitrate"),
            (OutputFormat::M4a, 1_025, 2, "AAC bitrate"),
            (OutputFormat::Vorbis, 1_025, 2, "Vorbis bitrate"),
        ];
        for (format, bitrate, quality, diagnostic) in cases {
            let mut invalid_plan = plan();
            invalid_plan.mp3_bitrate = bitrate;
            invalid_plan.mp3_quality = quality;
            std::fs::write(&output, b"existing destination").unwrap();
            let error = normalize_one(&missing_input, &output, &invalid_plan, format).unwrap_err();
            assert!(error.contains(diagnostic), "{format:?}: {error}");
            assert_eq!(std::fs::read(&output).unwrap(), b"existing destination");
        }
    }

    #[test]
    fn non_finite_pcm_is_rejected_without_mutation_or_publication() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("existing.wav");
        for kind in [
            PcmKind::U8,
            PcmKind::S16,
            PcmKind::S24,
            PcmKind::S32,
            PcmKind::F32,
            PcmKind::F64,
        ] {
            for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
                let mut buffer = AudioBuffer {
                    sample_rate: 48_000,
                    channels: 1,
                    frames: 3,
                    data: vec![vec![0.25, invalid, -0.25]],
                    channel_roles: default_channel_roles(1),
                    source_kind: kind,
                };
                let original = buffer.data.clone();
                std::fs::write(&output, b"existing destination").unwrap();
                let error = write(&buffer, &output, &plan(), OutputFormat::Wav).unwrap_err();
                assert!(error.contains("non-finite sample"), "{kind:?}: {error}");
                assert_eq!(std::fs::read(&output).unwrap(), b"existing destination");

                assert!(try_apply_gain_and_protect(&mut buffer, 1.0, &plan()).is_err());
                for (actual, expected) in buffer.data[0].iter().zip(&original[0]) {
                    assert_eq!(actual.to_bits(), expected.to_bits());
                }
            }
        }
    }

    #[test]
    fn staged_commit_rejects_same_length_source_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("source.wav");
        let replacement = directory.path().join("replacement.wav");
        let output = directory.path().join("output.wav");
        write_mono_tone(&input, 0.1);
        write_mono_tone(&replacement, 0.2);
        assert_eq!(
            std::fs::metadata(&input).unwrap().len(),
            std::fs::metadata(&replacement).unwrap().len()
        );
        std::fs::write(&output, b"existing destination").unwrap();

        let staged =
            normalize_one_staged_with_roles(&input, &output, &plan(), OutputFormat::Wav, None)
                .unwrap();
        std::fs::copy(&replacement, &input).unwrap();
        let error = staged.commit().unwrap_err();
        assert!(error.contains("input changed"), "{error}");
        assert_eq!(std::fs::read(&output).unwrap(), b"existing destination");
    }

    #[test]
    fn create_new_policy_rejects_a_destination_created_after_rendering() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("source.wav");
        let output = directory.path().join("output.wav");
        write_mono_tone(&input, 0.1);

        let staged = normalize_one_staged_with_roles_and_policy(
            &input,
            &output,
            &plan(),
            OutputFormat::Wav,
            None,
            OutputConflictPolicy::CreateNew,
        )
        .unwrap();
        std::fs::write(&output, b"competitor").unwrap();

        let error = staged.commit().unwrap_err();
        assert!(error.contains("without overwrite"), "{error}");
        assert_eq!(std::fs::read(&output).unwrap(), b"competitor");
    }

    #[test]
    fn corrected_render_can_be_checkpointed_before_publication() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("source.wav");
        let output = directory.path().join("output.wav");
        write_mono_tone(&input, 0.1);

        let staged = normalize_one_corrected_staged_with_roles_and_policy(
            &input,
            &output,
            &plan(),
            OutputFormat::Wav,
            0.01,
            2,
            None,
            OutputConflictPolicy::CreateNew,
        )
        .unwrap();
        assert!(staged.staged_path().is_file());
        assert!(!output.exists());
        assert!(staged.outcome().verification.passed());

        let outcome = staged.commit().unwrap();
        assert!(output.is_file());
        assert!(outcome.verification.passed());
    }

    #[test]
    fn bound_album_rejects_a_changed_second_track_before_any_publication() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.wav");
        let second = directory.path().join("second.wav");
        let replacement = directory.path().join("replacement.wav");
        let first_output = directory.path().join("first-output.wav");
        let second_output = directory.path().join("second-output.wav");
        write_mono_tone(&first, 0.1);
        write_mono_tone(&second, 0.15);
        write_mono_tone(&replacement, 0.2);
        let options = StableInputOptions::new(u64::MAX).unwrap();
        let inputs = vec![
            StableInput::from_path(&first, &options).unwrap(),
            StableInput::from_path(&second, &options).unwrap(),
        ];
        let analyses = inputs
            .iter()
            .map(|input| analyze_stable_input_for_plan(input, None, &plan()).unwrap())
            .collect::<Vec<_>>();
        std::fs::write(&first_output, b"first destination").unwrap();
        std::fs::write(&second_output, b"second destination").unwrap();
        std::fs::copy(&replacement, &second).unwrap();

        let error = normalize_album_bound(
            &inputs,
            &[first_output.clone(), second_output.clone()],
            &plan(),
            &[OutputFormat::Wav, OutputFormat::Wav],
            &analyses,
        )
        .unwrap_err();
        assert_eq!(
            error.kind(),
            crate::bound_analysis::BoundAnalysisErrorKind::AnalysisRequestMismatch
        );
        assert!(error.to_string().contains("changed"), "{error}");
        assert_eq!(std::fs::read(&first_output).unwrap(), b"first destination");
        assert_eq!(
            std::fs::read(&second_output).unwrap(),
            b"second destination"
        );
    }

    #[test]
    fn descriptor_bound_render_uses_only_the_selected_frame_range() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("programme.wav");
        let output = directory.path().join("selected.wav");
        let frames = 96_000;
        let samples = (0..frames)
            .map(|frame| {
                let amplitude = if frame < 48_000 { 0.02 } else { 0.2 };
                amplitude * (std::f32::consts::TAU * 997.0 * frame as f32 / 48_000.0).sin()
            })
            .collect::<Vec<_>>();
        WavWriter::write(
            &input,
            &AudioBuffer {
                sample_rate: 48_000,
                channels: 1,
                channel_roles: default_channel_roles(1),
                frames,
                data: vec![samples],
                source_kind: PcmKind::F32,
            },
            PcmKind::F32,
            false,
        )
        .unwrap();
        let stable_options = StableInputOptions::new(u64::MAX).unwrap();
        let descriptor = InputDescriptor::from_path(
            &input,
            &stable_options,
            InputDescriptorOptions::default().with_time_range(1.0, Some(1.0)),
        )
        .unwrap();
        let render_plan = plan();
        let bound = analyze_input_descriptor_for_plan(&descriptor, &render_plan).unwrap();
        let (source, _) = normalize_one_descriptor_bound_with_policy(
            &descriptor,
            &output,
            &render_plan,
            OutputFormat::Wav,
            &bound,
            OutputConflictPolicy::CreateNew,
        )
        .unwrap();
        assert_eq!(source.frames, 48_000);
        let decoded = decoder::decode(&output).unwrap();
        assert_eq!(decoded.frames, 48_000);
        assert_eq!(decoded.sample_rate, 48_000);
    }

    #[test]
    fn descriptor_transaction_rejects_a_changed_live_source_at_commit() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("source.wav");
        let output = directory.path().join("output.wav");
        write_mono_tone(&input, 0.1);
        let stable_options = StableInputOptions::new(u64::MAX).unwrap();
        let descriptor =
            InputDescriptor::from_path(&input, &stable_options, InputDescriptorOptions::default())
                .unwrap();
        let render_plan = plan();
        let staged = normalize_one_descriptor_staged_with_policy(
            &descriptor,
            &output,
            &render_plan,
            OutputFormat::Wav,
            OutputConflictPolicy::CreateNew,
        )
        .unwrap();

        std::fs::write(&input, b"changed after the immutable render").unwrap();
        let error = staged.commit().unwrap_err();
        assert!(error.contains("input changed before output publication"));
        assert!(!output.exists());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn album_rejects_input_output_and_output_output_hardlink_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.wav");
        let second = directory.path().join("second.wav");
        let output_a = directory.path().join("a.wav");
        let output_b = directory.path().join("b.wav");
        write_mono_tone(&first, 0.1);
        write_mono_tone(&second, 0.2);
        std::fs::hard_link(&second, &output_a).unwrap();
        std::fs::write(&output_b, b"second destination").unwrap();
        let second_before = std::fs::read(&second).unwrap();

        let error = normalize_album(
            &[first.clone(), second.clone()],
            &[output_a.clone(), output_b.clone()],
            &plan(),
            &[OutputFormat::Wav, OutputFormat::Wav],
        )
        .unwrap_err();
        assert!(error.contains("aliases a protected input"), "{error}");
        assert_eq!(std::fs::read(&second).unwrap(), second_before);
        assert_eq!(std::fs::read(&output_b).unwrap(), b"second destination");

        std::fs::remove_file(&output_a).unwrap();
        std::fs::write(&output_a, b"shared destination").unwrap();
        std::fs::remove_file(&output_b).unwrap();
        std::fs::hard_link(&output_a, &output_b).unwrap();
        let error = normalize_album(
            &[first, second],
            &[output_a.clone(), output_b.clone()],
            &plan(),
            &[OutputFormat::Wav, OutputFormat::Wav],
        )
        .unwrap_err();
        assert!(error.contains("multiple outputs alias"), "{error}");
        assert_eq!(std::fs::read(&output_a).unwrap(), b"shared destination");
        assert_eq!(std::fs::read(&output_b).unwrap(), b"shared destination");
    }

    #[test]
    fn six_one_stereo_downmix_keeps_bc_when_sr_cancels_only_the_right_output() {
        let frames = 32;
        let mut data = vec![vec![0.0; frames]; 7];
        data[4].fill(0.25); // BC feeds both outputs.
        data[6].fill(-0.25); // SR cancels BC on the right only.
        let source = AudioBuffer {
            sample_rate: 48_000,
            channels: 7,
            frames,
            data,
            channel_roles: named_channel_layout("6.1").unwrap(),
            source_kind: PcmKind::F32,
        };

        let layout = standard_stereo_downmix_layout(&source).unwrap();
        assert_eq!(layout, downmix::Layout::SixOne);
        let rendered = downmix::render(&source, layout, downmix::Profile::Stereo).unwrap();
        let expected_left = 0.25 * std::f32::consts::FRAC_1_SQRT_2;
        assert!(rendered.buffer.data[0]
            .iter()
            .all(|sample| (*sample - expected_left).abs() <= f32::EPSILON));
        assert!(rendered.buffer.data[1]
            .iter()
            .all(|sample| sample.abs() <= f32::EPSILON));
    }

    #[test]
    fn decoded_layout_provenance_fails_closed_without_an_override() {
        let path = Path::new("ambiguous.wav");
        let decoded = default_channel_roles(2);
        let unknown = resolve_decoded_channel_roles(
            path,
            2,
            &decoded,
            decoder::ChannelLayoutProvenance::Unknown,
            None,
        )
        .unwrap_err();
        assert!(unknown.contains("ambiguous 2-channel layout"));

        let scene_based = resolve_decoded_channel_roles(
            path,
            4,
            &default_channel_roles(4),
            decoder::ChannelLayoutProvenance::SceneBased,
            None,
        )
        .unwrap_err();
        assert!(scene_based.contains("speaker renderer"));

        let explicit = named_channel_layout("stereo").unwrap();
        assert_eq!(
            resolve_decoded_channel_roles(
                path,
                2,
                &decoded,
                decoder::ChannelLayoutProvenance::Unknown,
                Some(&explicit),
            )
            .unwrap(),
            explicit
        );
    }

    #[test]
    fn limiter_idle_proof_is_conservative_at_the_ceiling() {
        let mut render_plan = plan();
        render_plan.limiter = Some(LimiterConfig::default());
        let mut measured = analysis(-20.0, -6.020_599_913_279_624);
        measured.sample_rate = 192_000;
        measured.sample_peak = 0.5;

        assert!(limiter_is_proven_idle(&measured, 1.0, 0.5, &render_plan));
        assert!(!limiter_is_proven_idle(
            &measured,
            0.5_f32.next_up() / 0.5,
            0.5,
            &render_plan
        ));

        measured.sample_rate = 48_000;
        assert!(!limiter_is_proven_idle(&measured, 1.0, 0.5, &render_plan));
        assert!(limiter_is_proven_idle(&measured, 0.25, 0.5, &render_plan));

        render_plan.limiter = None;
        assert!(!limiter_is_proven_idle(&measured, 0.25, 0.5, &render_plan));
    }

    #[test]
    fn proven_idle_stream_matches_the_full_limiter_and_its_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("source.wav");
        let bypass_output = directory.path().join("bypass.wav");
        let full_output = directory.path().join("full.wav");
        let frames = 48_000 * 4 + 137;
        let data = (0..2)
            .map(|channel| {
                let frequency = 701.0 + channel as f32 * 421.0;
                (0..frames)
                    .map(|frame| {
                        0.03 * (std::f32::consts::TAU * frequency * frame as f32 / 48_000.0).sin()
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        WavWriter::write(
            &input,
            &AudioBuffer {
                sample_rate: 48_000,
                channels: 2,
                frames,
                data,
                channel_roles: default_channel_roles(2),
                source_kind: PcmKind::F32,
            },
            PcmKind::F32,
            false,
        )
        .unwrap();
        let source = analyze_file(&input).unwrap();
        let mut forced_full = source.clone();
        forced_full.sample_peak = 1.0;
        let mut render_plan = plan();
        render_plan.limiter = Some(LimiterConfig::default());
        render_plan.output_kind = Some(PcmKind::F32);
        let ceiling = 10.0_f64.powf(render_plan.ceiling_db / 20.0) as f32;
        assert!(limiter_is_proven_idle(&source, 1.0, ceiling, &render_plan));
        assert!(!limiter_is_proven_idle(
            &forced_full,
            1.0,
            ceiling,
            &render_plan
        ));
        let options = StreamRenderOptions {
            opus_album_lufs: None,
            capture_statistics: true,
            capture_lossless_verification: false,
            verification_channel_roles: None,
            channel_layout: None,
            layout_alias_policy: LayoutAliasPolicy::ExactOnly,
        };
        let bypass = normalize_stream(
            StreamSource {
                path: &input,
                descriptor: None,
                spool: None,
            },
            &bypass_output,
            &source,
            1.0,
            &render_plan,
            OutputFormat::Wav,
            options,
        )
        .unwrap()
        .statistics
        .unwrap();
        let full = normalize_stream(
            StreamSource {
                path: &input,
                descriptor: None,
                spool: None,
            },
            &full_output,
            &forced_full,
            1.0,
            &render_plan,
            OutputFormat::Wav,
            options,
        )
        .unwrap()
        .statistics
        .unwrap();

        assert_eq!(
            std::fs::read(&bypass_output).unwrap(),
            std::fs::read(&full_output).unwrap()
        );
        assert_analysis_identical(&bypass.intended, &full.intended, "idle limiter bypass");
        assert_eq!(
            bypass.input_full_scale_exceeding_samples,
            full.input_full_scale_exceeding_samples
        );
        assert_eq!(
            bypass.post_gain_full_scale_exceeding_samples,
            full.post_gain_full_scale_exceeding_samples
        );
        assert_eq!(
            bypass.post_gain_ceiling_exceeding_samples,
            full.post_gain_ceiling_exceeding_samples
        );
        assert_eq!(
            bypass.protected_full_scale_exceeding_samples,
            full.protected_full_scale_exceeding_samples
        );
        assert_eq!(
            serde_json::to_value(bypass.limiter.unwrap()).unwrap(),
            serde_json::to_value(full.limiter.unwrap()).unwrap()
        );
    }

    #[test]
    fn lossless_writer_overlap_is_limited_to_expensive_wave_verification() {
        let verified = StreamRenderOptions {
            opus_album_lufs: None,
            capture_statistics: true,
            capture_lossless_verification: true,
            verification_channel_roles: None,
            channel_layout: None,
            layout_alias_policy: LayoutAliasPolicy::ExactOnly,
        };
        assert!(stream_writer_work_can_overlap(
            &[OutputFormat::Wav],
            verified
        ));
        assert!(stream_writer_work_can_overlap(
            &[OutputFormat::Wav, OutputFormat::Wav],
            verified,
        ));
        assert!(!stream_writer_work_can_overlap(&[], verified));
        assert!(!stream_writer_work_can_overlap(
            &[OutputFormat::Flac],
            verified
        ));
        assert!(!stream_writer_work_can_overlap(
            &[OutputFormat::Wav, OutputFormat::Flac],
            verified,
        ));

        let without_statistics = StreamRenderOptions {
            capture_statistics: false,
            ..verified
        };
        let without_verification = StreamRenderOptions {
            capture_lossless_verification: false,
            ..verified
        };
        assert!(!stream_writer_work_can_overlap(
            &[OutputFormat::Wav],
            without_statistics,
        ));
        assert!(!stream_writer_work_can_overlap(
            &[OutputFormat::Wav],
            without_verification,
        ));

        // Lossy codecs retain the v0.150.0 encoder-overlap eligibility.
        assert!(stream_writer_work_can_overlap(
            &[OutputFormat::Mp3],
            verified
        ));
        assert!(stream_writer_work_can_overlap(
            &[OutputFormat::Opus],
            verified
        ));
    }

    #[test]
    fn known_output_analysis_avoids_reopening_the_completed_file() {
        let directory = tempfile::tempdir().unwrap();
        let missing_output = directory.path().join("not-written.wav");
        let known = analysis(-16.0, -1.0);

        let measured = known_or_analyze_output(&missing_output, Some(&known)).unwrap();

        assert!(matches!(measured, Cow::Borrowed(_)));
        assert!(std::ptr::eq(measured.as_ref(), &known));
        assert!(known_or_analyze_output(&missing_output, None).is_err());
    }

    #[test]
    fn album_loudness_uses_the_same_strict_gate_without_collecting_tracks() {
        let absolute_gate = 10.0_f64.powf((-70.0 + 0.691) / 10.0);
        let relative_gate = 2.0_f64.powi(-20);
        let loud = 19.0 * relative_gate;
        let mut first = analysis(-16.0, -3.0);
        first.loudness_blocks = vec![absolute_gate, relative_gate];
        let mut second = analysis(-16.0, -3.0);
        second.loudness_blocks = vec![loud];

        let expected = lufs::gated_lufs(&[absolute_gate, relative_gate, loud]);
        assert_eq!(album_lufs(&[first, second]), expected);
        assert_eq!(expected, -0.691 + 10.0 * loud.log10());
    }

    #[test]
    fn single_verification_never_applies_loudness_tolerance_to_true_peak() {
        let output = analysis(-16.4, -0.75);
        let verification = verify_analysis_at_level(&output, -16.0, &plan(), 0.5);

        assert!(verification.level_ok);
        assert!(!verification.true_peak_ok);
        assert!(!verification.passed());
        assert!(true_peak_within_ceiling(-1.0, -1.0));
    }

    #[test]
    fn album_verification_never_applies_loudness_tolerance_to_true_peak() {
        assert!(album_measurements_pass(0.4, -1.0, -1.0, 0.5));
        assert!(!album_measurements_pass(0.4, -0.75, -1.0, 0.5));
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
    fn shared_corrected_gain_uses_quietest_common_feasible_point() {
        let first = verify_analysis_at_level(&analysis(-16.8, -3.0), -16.0, &plan(), 0.5);
        let second = verify_analysis_at_level(&analysis(-16.2, -3.0), -16.0, &plan(), 0.5);

        let corrected = shared_corrected_gain(1.0, &[first, second], &plan(), 0.5).unwrap();

        // The intervals are +0.3..+1.3 dB and -0.3..+0.7 dB. Choosing
        // +0.3 dB is the lowest common point and preserves the most headroom.
        assert!((gain_db(corrected) - 0.3).abs() < 1e-5);
    }

    #[test]
    fn shared_corrected_gain_rejects_disjoint_codec_constraints() {
        let quiet = verify_analysis_at_level(&analysis(-16.8, -3.0), -16.0, &plan(), 0.1);
        let loud = verify_analysis_at_level(&analysis(-15.6, -3.0), -16.0, &plan(), 0.1);

        let error = shared_corrected_gain(1.0, &[quiet, loud], &plan(), 0.1).unwrap_err();

        assert!(error.contains("no shared gain"));
    }

    #[test]
    fn shared_corrected_gain_obeys_common_true_peak_ceiling() {
        let quiet = verify_analysis_at_level(&analysis(-16.8, -0.5), -16.0, &plan(), 0.1);

        let error = shared_corrected_gain(1.0, &[quiet], &plan(), 0.1).unwrap_err();

        assert!(error.contains("no shared gain"));
    }

    #[test]
    fn digital_silence_uses_finite_unity_gain() {
        let silence = Analysis {
            lufs: f64::NEG_INFINITY,
            rms_db: f64::NEG_INFINITY,
            sample_peak: 0.0,
            true_peak: 0.0,
            ..analysis(-16.0, -120.0)
        };
        for mode in [Mode::Lufs, Mode::Peak, Mode::Rms] {
            let mut plan = plan();
            plan.mode = mode;
            assert_eq!(compute_gain(&silence, &plan), 1.0);
        }
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
    fn many_short_dialogue_ranges_preserve_the_selected_energy() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("many-short-ranges.wav");
        let sample_rate = 8_192;
        let frames_per_range = 8;
        let range_count = 1_024;
        let frames = frames_per_range * range_count;
        let samples = vec![0.125; frames];
        WavWriter::write(
            &input,
            &AudioBuffer {
                sample_rate,
                channels: 1,
                frames,
                data: vec![samples],
                channel_roles: default_channel_roles(1),
                source_kind: PcmKind::F32,
            },
            PcmKind::F32,
            false,
        )
        .unwrap();
        let ranges = (0..range_count)
            .map(|index| DialogueRange {
                start_seconds: (index * frames_per_range) as f64 / sample_rate as f64,
                duration_seconds: frames_per_range as f64 / sample_rate as f64,
            })
            .collect::<Vec<_>>();

        let measured = analyze_dialogue_ranges_with_roles(&input, None, &ranges).unwrap();
        let mut reference = lufs::StreamingAnalyzer::new(sample_rate, vec![ChannelRole::Main]);
        reference.process(&[vec![0.125; frames_per_range]]).unwrap();
        let expected = lufs::ungated_lufs(reference.finish_without_lra_tail().weighted_mean_square);

        assert_eq!(measured.range_count, range_count);
        assert_eq!(measured.duration_seconds, 1.0);
        assert!((measured.lufs - expected).abs() < 1.0e-12);
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

    #[test]
    fn pcm_spool_estimate_uses_reliable_output_domain_lengths() {
        let info = decoder::StreamInfo {
            sample_rate: 44_100,
            channels: 2,
            channel_roles: default_channel_roles(2),
            source_kind: PcmKind::F32,
        };
        assert_eq!(
            expected_pcm_spool_bytes(Path::new("track.flac"), &info, 48_000, Some(44_100 * 300),),
            Some(48_000 * 300 * 2 * std::mem::size_of::<f32>())
        );
        assert_eq!(
            expected_pcm_spool_bytes(Path::new("track.mp3"), &info, 48_000, Some(44_100 * 300),),
            None,
            "lossy container duration metadata is not an exact allocation bound"
        );
        assert_eq!(
            expected_pcm_spool_bytes(Path::new("track.flac"), &info, 48_000, None),
            None
        );
    }

    #[test]
    fn output_domain_spool_matches_the_redecode_resample_path() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input.wav");
        let spooled_output = directory.path().join("spooled.wav");
        let redecode_output = directory.path().join("redecoded.wav");
        let sample_rate = 44_100;
        let frames = sample_rate as usize * 2 + 137;
        let samples = (0..frames)
            .map(|frame| {
                0.15 * (2.0 * std::f32::consts::PI * 997.0 * frame as f32 / sample_rate as f32)
                    .sin()
            })
            .collect::<Vec<_>>();
        let buffer = AudioBuffer {
            sample_rate,
            channels: 2,
            frames,
            data: vec![samples.clone(), samples],
            channel_roles: default_channel_roles(2),
            source_kind: PcmKind::F32,
        };
        WavWriter::write(&input, &buffer, PcmKind::F32, false).unwrap();

        let mut resample_plan = plan();
        resample_plan.output_sample_rate = Some(48_000);
        let prepared =
            prepare_file_for_plan(&input, None, &resample_plan, true).expect("prepare input");
        assert_eq!(prepared.analysis.sample_rate, 48_000);
        assert_eq!(
            prepared.spool.as_ref().map(PcmSpool::frames),
            Some(prepared.analysis.frames)
        );
        let preanalyzed = prepared.analysis;

        let (spooled_analysis, spooled_gain) =
            normalize_one(&input, &spooled_output, &resample_plan, OutputFormat::Wav).unwrap();
        let (redecoded_analysis, redecoded_gain) = normalize_one_preanalyzed_with_roles(
            &input,
            &redecode_output,
            &resample_plan,
            OutputFormat::Wav,
            None,
            &preanalyzed,
        )
        .unwrap();

        assert_eq!(spooled_analysis.frames, redecoded_analysis.frames);
        assert_eq!(spooled_gain.to_bits(), redecoded_gain.to_bits());
        assert_eq!(
            std::fs::read(spooled_output).unwrap(),
            std::fs::read(redecode_output).unwrap()
        );

        let same_rate = prepare_file_for_plan(&input, None, &plan(), true).unwrap();
        assert!(
            same_rate.spool.is_none(),
            "same-rate WAVE should skip spooling"
        );
    }

    #[test]
    fn analysis_pipeline_matches_sequential_measurements_and_pcm() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("pipeline-input.wav");
        let sample_rate = 44_100;
        let frames = sample_rate as usize * 2 + 509;
        let left = (0..frames)
            .map(|frame| {
                0.19 * (std::f32::consts::TAU * 997.0 * frame as f32 / sample_rate as f32).sin()
            })
            .collect::<Vec<_>>();
        let right = (0..frames)
            .map(|frame| {
                0.11 * (std::f32::consts::TAU * 613.0 * frame as f32 / sample_rate as f32).cos()
            })
            .collect::<Vec<_>>();
        WavWriter::write(
            &input,
            &AudioBuffer {
                sample_rate,
                channels: 2,
                frames,
                data: vec![left, right],
                channel_roles: default_channel_roles(2),
                source_kind: PcmKind::F32,
            },
            PcmKind::F32,
            false,
        )
        .unwrap();
        let mut resample_plan = plan();
        resample_plan.output_sample_rate = Some(48_000);

        let sequential =
            prepare_file_for_plan_sequential(&input, None, &resample_plan, true).unwrap();
        let pipelined =
            prepare_source_for_plan_pipelined(&input, None, None, &resample_plan, true).unwrap();
        let left = &sequential.analysis;
        let right = &pipelined.analysis;
        assert_eq!(left.sample_rate, right.sample_rate);
        assert_eq!(left.channels, right.channels);
        assert_eq!(left.channel_roles, right.channel_roles);
        assert_eq!(left.frames, right.frames);
        assert_eq!(left.kind, right.kind);
        assert_eq!(left.lufs.to_bits(), right.lufs.to_bits());
        assert_eq!(
            left.max_momentary_lufs.to_bits(),
            right.max_momentary_lufs.to_bits()
        );
        assert_eq!(
            left.max_short_term_lufs.to_bits(),
            right.max_short_term_lufs.to_bits()
        );
        assert_eq!(
            left.loudness_range_lu.to_bits(),
            right.loudness_range_lu.to_bits()
        );
        assert_eq!(left.rms_db.to_bits(), right.rms_db.to_bits());
        assert_eq!(left.sample_peak.to_bits(), right.sample_peak.to_bits());
        assert_eq!(left.true_peak.to_bits(), right.true_peak.to_bits());
        assert_eq!(
            left.loudness_blocks
                .iter()
                .map(|value| value.to_bits())
                .collect::<Vec<_>>(),
            right
                .loudness_blocks
                .iter()
                .map(|value| value.to_bits())
                .collect::<Vec<_>>()
        );

        let collect_spool = |mut spool: PcmSpool| {
            let mut output = vec![Vec::new(), Vec::new()];
            spool
                .replay(|chunk| {
                    for (destination, source) in output.iter_mut().zip(chunk) {
                        destination.extend_from_slice(source);
                    }
                    Ok(())
                })
                .unwrap();
            output
        };
        let sequential_pcm = collect_spool(sequential.spool.unwrap());
        let pipelined_pcm = collect_spool(pipelined.spool.unwrap());
        assert_eq!(sequential_pcm, pipelined_pcm);

        let stable_options = StableInputOptions::new(u64::MAX).unwrap();
        let descriptor = InputDescriptor::from_path(
            &input,
            &stable_options,
            InputDescriptorOptions::default()
                .with_time_range(509.0 / f64::from(sample_rate), Some(1.25)),
        )
        .unwrap();
        let descriptor_sequential =
            prepare_descriptor_for_plan_sequential(&descriptor, &resample_plan, true).unwrap();
        let descriptor_pipelined = prepare_source_for_plan_pipelined(
            descriptor.stable_input().stable_path(),
            Some(&descriptor),
            None,
            &resample_plan,
            true,
        )
        .unwrap();
        assert_analysis_identical(
            &descriptor_sequential.analysis,
            &descriptor_pipelined.analysis,
            "descriptor analysis pipeline",
        );
        assert_eq!(
            collect_spool(descriptor_sequential.spool.unwrap()),
            collect_spool(descriptor_pipelined.spool.unwrap())
        );
    }

    #[test]
    fn compressed_input_spool_matches_the_redecode_path() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input.flac");
        let spooled_output = directory.path().join("spooled.wav");
        let redecode_output = directory.path().join("redecoded.wav");
        let frames = 48_000 * 2 + 73;
        let samples = (0..frames)
            .map(|frame| {
                0.1 * (2.0 * std::f32::consts::PI * 1_003.0 * frame as f32 / 48_000.0).sin()
            })
            .collect::<Vec<_>>();
        let mut writer = FlacStreamWriter::create(&input, 48_000, 2, 24, false).unwrap();
        writer.write_chunk(&[samples.clone(), samples]).unwrap();
        writer.finish().unwrap();

        let prepared = prepare_file_for_plan(&input, None, &plan(), true).unwrap();
        assert_eq!(
            prepared.spool.as_ref().map(PcmSpool::frames),
            Some(prepared.analysis.frames)
        );
        let preanalyzed = prepared.analysis;

        let (_, spooled_gain) =
            normalize_one(&input, &spooled_output, &plan(), OutputFormat::Wav).unwrap();
        let (_, redecoded_gain) = normalize_one_preanalyzed_with_roles(
            &input,
            &redecode_output,
            &plan(),
            OutputFormat::Wav,
            None,
            &preanalyzed,
        )
        .unwrap();
        assert_eq!(spooled_gain.to_bits(), redecoded_gain.to_bits());
        assert_eq!(
            std::fs::read(spooled_output).unwrap(),
            std::fs::read(redecode_output).unwrap()
        );

        let misleading = directory.path().join("lossless-audio.wav");
        std::fs::copy(&input, &misleading).unwrap();
        let descriptor = InputDescriptor::from_path(
            &misleading,
            &StableInputOptions::new(u64::MAX).unwrap(),
            InputDescriptorOptions::default(),
        )
        .unwrap();
        assert_eq!(descriptor.codec(), decoder::AudioCodec::Flac);
        let descriptor_prepared =
            prepare_descriptor_analysis_for_render(&descriptor, &plan()).unwrap();
        assert_eq!(
            descriptor_prepared.spool.as_ref().map(PcmSpool::frames),
            Some(descriptor_prepared.analysis.frames),
            "descriptor capture must follow the probed codec, not the suffix"
        );
    }

    #[test]
    fn descriptor_normalization_round_trips_nondefault_layouts() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("source.wav");
        let wave_output = directory.path().join("normalized.wav");
        let flac_output = directory.path().join("normalized.flac");
        let frames = 48_000;
        let channels = 4_u16;
        let layout =
            crate::channel_layout::ChannelLayoutDescriptor::wave(channels, true, Some(0x5003));
        let signal = (0..frames)
            .map(|frame| 0.05 * (std::f32::consts::TAU * 997.0 * frame as f32 / 48_000.0).sin())
            .collect::<Vec<_>>();
        let buffer = AudioBuffer {
            sample_rate: 48_000,
            channels,
            frames,
            data: vec![signal; usize::from(channels)],
            channel_roles: layout.channel_roles(),
            source_kind: PcmKind::S24,
        };
        WavWriter::write_with_channel_layout(
            &input,
            &buffer,
            PcmKind::S24,
            false,
            WavContainer::Riff,
            &layout,
        )
        .unwrap();
        let descriptor = InputDescriptor::from_path(
            &input,
            &StableInputOptions::new(u64::MAX).unwrap(),
            InputDescriptorOptions::default(),
        )
        .unwrap();
        assert_eq!(
            descriptor.channel_layout().wave_channel_mask(),
            Some(0x5003)
        );

        for (output, format) in [
            (&wave_output, OutputFormat::Wav),
            (&flac_output, OutputFormat::Flac),
        ] {
            normalize_one_descriptor_with_policy(
                &descriptor,
                output,
                &plan(),
                format,
                OutputConflictPolicy::CreateNew,
            )
            .unwrap();
            let (_, actual) =
                decoder::decode_limited_with_channel_layout(output, u64::MAX).unwrap();
            assert_eq!(actual.channel_count(), usize::from(channels));
            assert_eq!(
                actual.wave_channel_mask().or(actual.flac_channel_mask()),
                Some(0x5003),
                "{format:?}"
            );
            assert_eq!(actual.channel_roles(), layout.channel_roles());
        }
    }

    #[test]
    fn pcm_capture_policy_avoids_raw_io_for_fast_lossy_decoders() {
        assert!(should_capture_pcm(Path::new("album.flac"), false));
        assert!(should_capture_pcm(Path::new("archive.dsf"), false));
        assert!(!should_capture_pcm(Path::new("track.mp3"), false));
        assert!(!should_capture_pcm(Path::new("track.m4a"), false));
        assert!(!should_capture_pcm(Path::new("track.ogg"), false));
        assert!(should_capture_pcm(Path::new("track.mp3"), true));
        assert!(should_capture_pcm(Path::new("track.wav"), true));
    }

    #[test]
    fn lossless_encoder_tee_matches_completed_wave_and_flac_decodes() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("source.wav");
        let frames = 48_000 * 4 + 137;
        let left = (0..frames)
            .map(|frame| {
                0.217 * (2.0 * std::f32::consts::PI * 997.0 * frame as f32 / 48_000.0).sin()
            })
            .collect::<Vec<_>>();
        let right = (0..frames)
            .map(|frame| {
                0.133 * (2.0 * std::f32::consts::PI * 1_499.0 * frame as f32 / 48_000.0).sin()
            })
            .collect::<Vec<_>>();
        let input_buffer = AudioBuffer {
            sample_rate: 48_000,
            channels: 2,
            frames,
            data: vec![left, right],
            channel_roles: default_channel_roles(2),
            source_kind: PcmKind::F32,
        };
        WavWriter::write(&input, &input_buffer, PcmKind::F32, false).unwrap();
        let source = analyze_file(&input).unwrap();

        let cases = [
            (OutputFormat::Wav, PcmKind::U8, true, "u8.wav"),
            (OutputFormat::Wav, PcmKind::S16, false, "s16.wav"),
            (OutputFormat::Wav, PcmKind::S16, true, "s16-dither.wav"),
            (OutputFormat::Wav, PcmKind::S24, true, "s24.wav"),
            (OutputFormat::Wav, PcmKind::S32, true, "s32.wav"),
            (OutputFormat::Wav, PcmKind::F32, false, "f32.wav"),
            (OutputFormat::Wav, PcmKind::F64, false, "f64.wav"),
            (OutputFormat::Flac, PcmKind::S16, false, "s16.flac"),
            (OutputFormat::Flac, PcmKind::S16, true, "s16-dither.flac"),
            (OutputFormat::Flac, PcmKind::S24, true, "s24.flac"),
        ];
        for (format, kind, dither, name) in cases {
            let output = directory.path().join(name);
            let mut render_plan = plan();
            render_plan.output_kind = Some(kind);
            render_plan.dither = dither;
            let rendered = normalize_stream(
                StreamSource {
                    path: &input,
                    descriptor: None,
                    spool: None,
                },
                &output,
                &source,
                compute_gain(&source, &render_plan),
                &render_plan,
                format,
                StreamRenderOptions {
                    opus_album_lufs: None,
                    capture_statistics: false,
                    capture_lossless_verification: true,
                    verification_channel_roles: None,
                    channel_layout: None,
                    layout_alias_policy: LayoutAliasPolicy::ExactOnly,
                },
            )
            .unwrap();
            let tee = rendered.lossless_output.expect("native lossless tee");
            let decoded = analyze_file(&output).unwrap();
            assert_analysis_identical(&tee, &decoded, name);
        }
    }

    #[test]
    fn multi_stream_fanout_and_bounded_pipeline_match_separate_lossless_encodes() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("source.wav");
        let frames = 48_000 * 4 + 137;
        let left = (0..frames)
            .map(|frame| {
                0.217 * (2.0 * std::f32::consts::PI * 997.0 * frame as f32 / 48_000.0).sin()
            })
            .collect::<Vec<_>>();
        let right = (0..frames)
            .map(|frame| {
                0.133 * (2.0 * std::f32::consts::PI * 1_499.0 * frame as f32 / 48_000.0).sin()
            })
            .collect::<Vec<_>>();
        WavWriter::write(
            &input,
            &AudioBuffer {
                sample_rate: 48_000,
                channels: 2,
                frames,
                data: vec![left, right],
                channel_roles: default_channel_roles(2),
                source_kind: PcmKind::F32,
            },
            PcmKind::F32,
            false,
        )
        .unwrap();
        let source = analyze_file(&input).unwrap();
        let gain = compute_gain(&source, &plan());
        let options = StreamRenderOptions {
            opus_album_lufs: None,
            capture_statistics: true,
            capture_lossless_verification: true,
            verification_channel_roles: None,
            channel_layout: None,
            layout_alias_policy: LayoutAliasPolicy::ExactOnly,
        };
        let separate_paths = [
            directory.path().join("separate.wav"),
            directory.path().join("separate.flac"),
        ];
        let formats = [OutputFormat::Wav, OutputFormat::Flac];
        let separate = separate_paths
            .iter()
            .zip(formats)
            .map(|(output, format)| {
                normalize_stream(
                    StreamSource {
                        path: &input,
                        descriptor: None,
                        spool: None,
                    },
                    output,
                    &source,
                    gain,
                    &plan(),
                    format,
                    options,
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        let fanout_paths = [
            directory.path().join("fanout.wav"),
            directory.path().join("fanout.flac"),
        ];
        let fanout = normalize_streams(
            StreamSource {
                path: &input,
                descriptor: None,
                spool: None,
            },
            &fanout_paths,
            &source,
            gain,
            &plan(),
            &formats,
            options,
        )
        .unwrap();
        let pipeline_paths = [
            directory.path().join("pipeline.wav"),
            directory.path().join("pipeline.flac"),
        ];
        let ceiling = 10.0_f64.powf(plan().ceiling_db / 20.0) as f32;
        let (pipeline_statistics, pipeline_lossless_outputs) = process_normalized_stream_pipelined(
            StreamSource {
                path: &input,
                descriptor: None,
                spool: None,
            },
            &source,
            gain,
            ceiling,
            &plan(),
            options.capture_statistics,
            || {
                pipeline_paths
                    .iter()
                    .zip(formats)
                    .map(|(output, format)| {
                        NormalizedStreamWriter::create(
                            &input,
                            output,
                            &source,
                            gain,
                            &plan(),
                            format,
                            options,
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()
            },
        )
        .unwrap();

        for ((separate_path, fanout_path), format) in
            separate_paths.iter().zip(&fanout_paths).zip(formats)
        {
            assert_eq!(
                std::fs::read(separate_path).unwrap(),
                std::fs::read(fanout_path).unwrap(),
                "{format:?} fan-out output changed"
            );
        }
        for ((separate_path, pipeline_path), format) in
            separate_paths.iter().zip(&pipeline_paths).zip(formats)
        {
            assert_eq!(
                std::fs::read(separate_path).unwrap(),
                std::fs::read(pipeline_path).unwrap(),
                "{format:?} pipelined output changed"
            );
        }
        let fanout_statistics = fanout.statistics.unwrap();
        let pipeline_statistics = pipeline_statistics.unwrap();
        assert_analysis_identical(
            &fanout_statistics.intended,
            &pipeline_statistics.intended,
            "pipelined render statistics",
        );
        assert_eq!(
            fanout_statistics.input_full_scale_exceeding_samples,
            pipeline_statistics.input_full_scale_exceeding_samples
        );
        assert_eq!(
            fanout_statistics.post_gain_full_scale_exceeding_samples,
            pipeline_statistics.post_gain_full_scale_exceeding_samples
        );
        assert_eq!(
            fanout_statistics.post_gain_ceiling_exceeding_samples,
            pipeline_statistics.post_gain_ceiling_exceeding_samples
        );
        assert_eq!(
            fanout_statistics.protected_full_scale_exceeding_samples,
            pipeline_statistics.protected_full_scale_exceeding_samples
        );
        for result in &separate {
            let statistics = result.statistics.as_ref().unwrap();
            assert_analysis_identical(
                &statistics.intended,
                &fanout_statistics.intended,
                "fan-out render statistics",
            );
            assert_eq!(
                statistics.input_full_scale_exceeding_samples,
                fanout_statistics.input_full_scale_exceeding_samples
            );
            assert_eq!(
                statistics.post_gain_full_scale_exceeding_samples,
                fanout_statistics.post_gain_full_scale_exceeding_samples
            );
            assert_eq!(
                statistics.post_gain_ceiling_exceeding_samples,
                fanout_statistics.post_gain_ceiling_exceeding_samples
            );
            assert_eq!(
                statistics.protected_full_scale_exceeding_samples,
                fanout_statistics.protected_full_scale_exceeding_samples
            );
        }
        for (index, measured) in fanout.lossless_outputs.into_iter().enumerate() {
            assert_analysis_identical(
                separate[index].lossless_output.as_ref().unwrap(),
                measured.as_ref().unwrap(),
                "fan-out lossless verification",
            );
        }
        for (index, measured) in pipeline_lossless_outputs.into_iter().enumerate() {
            assert_analysis_identical(
                separate[index].lossless_output.as_ref().unwrap(),
                measured.as_ref().unwrap(),
                "pipelined lossless verification",
            );
        }
    }

    #[cfg(feature = "mp3-encoding")]
    #[test]
    fn bounded_pipeline_preserves_mp3_bytes_and_writer_chunk_boundaries() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("source.wav");
        let synchronous_output = directory.path().join("synchronous.mp3");
        let pipelined_output = directory.path().join("pipelined.mp3");
        let frames = 48_000 * 4 + 137;
        let data = (0..2)
            .map(|channel| {
                let frequency = 997.0 + channel as f32 * 502.0;
                (0..frames)
                    .map(|frame| {
                        0.17 * (std::f32::consts::TAU * frequency * frame as f32 / 48_000.0).sin()
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        WavWriter::write(
            &input,
            &AudioBuffer {
                sample_rate: 48_000,
                channels: 2,
                frames,
                data,
                channel_roles: default_channel_roles(2),
                source_kind: PcmKind::F32,
            },
            PcmKind::F32,
            false,
        )
        .unwrap();
        let source = analyze_file(&input).unwrap();
        let render_plan = plan();
        let gain = compute_gain(&source, &render_plan);
        let ceiling = 10.0_f64.powf(render_plan.ceiling_db / 20.0) as f32;
        let options = StreamRenderOptions {
            opus_album_lufs: None,
            capture_statistics: false,
            capture_lossless_verification: false,
            verification_channel_roles: None,
            channel_layout: None,
            layout_alias_policy: LayoutAliasPolicy::ExactOnly,
        };

        let mut writer = NormalizedStreamWriter::create(
            &input,
            &synchronous_output,
            &source,
            gain,
            &render_plan,
            OutputFormat::Mp3,
            options,
        )
        .unwrap();
        process_normalized_stream(
            StreamSource {
                path: &input,
                descriptor: None,
                spool: None,
            },
            &source,
            gain,
            ceiling,
            &render_plan,
            false,
            |planar| writer.write_chunk(planar),
        )
        .unwrap();
        writer.finish().unwrap();

        let (_, outputs) = process_normalized_stream_pipelined(
            StreamSource {
                path: &input,
                descriptor: None,
                spool: None,
            },
            &source,
            gain,
            ceiling,
            &render_plan,
            false,
            || {
                NormalizedStreamWriter::create(
                    &input,
                    &pipelined_output,
                    &source,
                    gain,
                    &render_plan,
                    OutputFormat::Mp3,
                    options,
                )
                .map(|writer| vec![writer])
            },
        )
        .unwrap();
        assert_eq!(outputs.len(), 1);
        assert!(outputs[0].is_none());
        assert_eq!(
            std::fs::read(synchronous_output).unwrap(),
            std::fs::read(pipelined_output).unwrap()
        );
    }

    #[test]
    fn flac_encoder_tee_matches_multichannel_decoder_layouts() {
        let directory = tempfile::tempdir().unwrap();
        for (channels, roles, name) in [
            (6_u16, default_channel_roles(6), "surround-5.1"),
            (8_u16, named_channel_layout("7.1").unwrap(), "surround-7.1"),
        ] {
            let frames = 48_000 * 4 + 31;
            let data = (0..channels)
                .map(|channel| {
                    let frequency = 701.0 + f32::from(channel) * 113.0;
                    let amplitude = 0.04 + f32::from(channel) * 0.01;
                    (0..frames)
                        .map(|frame| {
                            amplitude
                                * (2.0 * std::f32::consts::PI * frequency * frame as f32 / 48_000.0)
                                    .sin()
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            let input = directory.path().join(format!("{name}-source.wav"));
            let output = directory.path().join(format!("{name}.flac"));
            WavWriter::write(
                &input,
                &AudioBuffer {
                    sample_rate: 48_000,
                    channels,
                    frames,
                    data,
                    channel_roles: roles,
                    source_kind: PcmKind::F32,
                },
                PcmKind::F32,
                false,
            )
            .unwrap();
            let source = analyze_file(&input).unwrap();
            let mut render_plan = plan();
            render_plan.output_kind = Some(PcmKind::S24);
            let rendered = normalize_stream(
                StreamSource {
                    path: &input,
                    descriptor: None,
                    spool: None,
                },
                &output,
                &source,
                compute_gain(&source, &render_plan),
                &render_plan,
                OutputFormat::Flac,
                StreamRenderOptions {
                    opus_album_lufs: None,
                    capture_statistics: false,
                    capture_lossless_verification: true,
                    verification_channel_roles: None,
                    channel_layout: None,
                    layout_alias_policy: LayoutAliasPolicy::ExactOnly,
                },
            )
            .unwrap();
            let tee = rendered.lossless_output.expect("FLAC lossless tee");
            let decoded = analyze_file(&output).unwrap();
            assert_analysis_identical(&tee, &decoded, name);
        }
    }

    fn assert_analysis_identical(left: &Analysis, right: &Analysis, context: &str) {
        assert_eq!(left.sample_rate, right.sample_rate, "{context}");
        assert_eq!(left.channels, right.channels, "{context}");
        assert_eq!(left.channel_roles, right.channel_roles, "{context}");
        assert_eq!(left.frames, right.frames, "{context}");
        assert_eq!(left.kind, right.kind, "{context}");
        assert_eq!(left.lufs.to_bits(), right.lufs.to_bits(), "{context}");
        assert_eq!(
            left.max_momentary_lufs.to_bits(),
            right.max_momentary_lufs.to_bits(),
            "{context}"
        );
        assert_eq!(
            left.max_short_term_lufs.to_bits(),
            right.max_short_term_lufs.to_bits(),
            "{context}"
        );
        assert_eq!(
            left.loudness_range_lu.to_bits(),
            right.loudness_range_lu.to_bits(),
            "{context}"
        );
        assert_eq!(left.rms_db.to_bits(), right.rms_db.to_bits(), "{context}");
        assert_eq!(
            left.sample_peak.to_bits(),
            right.sample_peak.to_bits(),
            "{context}"
        );
        assert_eq!(
            left.true_peak.to_bits(),
            right.true_peak.to_bits(),
            "{context}"
        );
        assert_eq!(
            left.loudness_blocks
                .iter()
                .map(|value| value.to_bits())
                .collect::<Vec<_>>(),
            right
                .loudness_blocks
                .iter()
                .map(|value| value.to_bits())
                .collect::<Vec<_>>(),
            "{context}"
        );
    }
}
