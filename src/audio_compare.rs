//! Deterministic decoded-audio comparison against a reference signal.
//!
//! This is an engineering QC method, not a conforming implementation of
//! ITU-R BS.1387 (PEAQ). BS.1387 motivates time alignment of reference and
//! test signals but deliberately leaves synchronization to implementations.

use crate::decoder;
use crate::wav::AudioBuffer;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::f64::consts::TAU;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

pub const RESULT_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/audio-comparison-v1";
pub const METHOD_SOURCE: &str = "https://www.itu.int/rec/R-REC-BS.1387-2-202305-I/en";

const MAX_CHANNELS: usize = 32;
const MAX_ALIGNMENT_MILLISECONDS: u64 = 10_000;
const ALIGNMENT_BLOCK_FRAMES: usize = 32;
const ALIGNMENT_PROBE_SECONDS: usize = 10;
const ALIGNMENT_MATRIX_POINTS: usize = 25_000;
const ALIGNMENT_FINE_POINTS: usize = 100_000;
const MAPPING_POINTS: usize = 200_000;
const SPECTRAL_WINDOWS: usize = 8;
const SPECTRAL_WINDOW_FRAMES: usize = 4_096;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioCompareOptions {
    pub max_input_bytes: u64,
    pub max_decoded_samples: u64,
    pub alignment_search_ms: u64,
    pub max_offset_samples: u64,
    pub duration_tolerance_samples: u64,
    pub min_alignment_correlation: f64,
    pub min_channel_correlation: f64,
    pub min_null_depth_db: f64,
    pub max_residual_peak_dbfs: f64,
    pub max_spectral_error_db: f64,
    pub allow_channel_permutation: bool,
    pub allow_polarity_inversion: bool,
}

impl Default for AudioCompareOptions {
    fn default() -> Self {
        Self {
            max_input_bytes: 4 * 1024 * 1024 * 1024,
            max_decoded_samples: 400_000_000,
            alignment_search_ms: 1_000,
            max_offset_samples: 0,
            duration_tolerance_samples: 0,
            min_alignment_correlation: 0.9,
            min_channel_correlation: 0.999,
            min_null_depth_db: 60.0,
            max_residual_peak_dbfs: -60.0,
            max_spectral_error_db: 0.1,
            allow_channel_permutation: false,
            allow_polarity_inversion: false,
        }
    }
}

impl AudioCompareOptions {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_input_bytes == 0 {
            return Err("max_input_bytes must be greater than zero".into());
        }
        if self.max_decoded_samples == 0 {
            return Err("max_decoded_samples must be greater than zero".into());
        }
        if self.alignment_search_ms > MAX_ALIGNMENT_MILLISECONDS {
            return Err(format!(
                "alignment_search_ms must not exceed {MAX_ALIGNMENT_MILLISECONDS}"
            ));
        }
        for (name, value) in [
            ("min_alignment_correlation", self.min_alignment_correlation),
            ("min_channel_correlation", self.min_channel_correlation),
        ] {
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return Err(format!("{name} must be finite and between zero and one"));
            }
        }
        for (name, value) in [
            ("min_null_depth_db", self.min_null_depth_db),
            ("max_residual_peak_dbfs", self.max_residual_peak_dbfs),
            ("max_spectral_error_db", self.max_spectral_error_db),
        ] {
            if !value.is_finite() {
                return Err(format!("{name} must be finite"));
            }
        }
        if self.min_null_depth_db < 0.0 {
            return Err("min_null_depth_db must be non-negative".into());
        }
        if self.max_spectral_error_db < 0.0 {
            return Err("max_spectral_error_db must be non-negative".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct InputEvidence {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub frames: usize,
    pub duration_seconds: f64,
    pub non_finite_samples: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MethodEvidence {
    pub classification: &'static str,
    pub source: &'static str,
    pub synchronization: &'static str,
    pub channel_assignment: &'static str,
    pub residual: &'static str,
    pub spectral_error: &'static str,
    pub alignment_block_frames: usize,
    pub alignment_probe_seconds: usize,
    pub mapping_point_limit: usize,
    pub spectral_windows: usize,
    pub spectral_window_frames: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct AlignmentEvidence {
    /// Candidate sample index minus reference sample index.
    pub offset_samples: i64,
    pub offset_seconds: f64,
    pub correlation: f64,
    pub search_limit_samples: u64,
    pub minimum_overlap_frames: usize,
    pub reference_start_frame: usize,
    pub candidate_start_frame: usize,
    pub compared_frames: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChannelEvidence {
    pub reference_channel: u16,
    pub candidate_channel: u16,
    pub correlation: f64,
    pub polarity_inverted: bool,
    pub reference_rms_dbfs: f64,
    pub residual_rms_dbfs: f64,
    pub corrected_residual_rms_dbfs: f64,
    pub residual_peak_dbfs: f64,
    pub corrected_residual_peak_dbfs: f64,
    pub null_depth_db: f64,
    pub corrected_null_depth_db: f64,
    pub spectral_error_db: f64,
    pub exact_sample_match_ratio: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AggregateEvidence {
    pub aligned_identity_null_depth_db: f64,
    pub mapped_null_depth_db: f64,
    pub polarity_corrected_null_depth_db: f64,
    pub mapped_residual_peak_dbfs: f64,
    pub polarity_corrected_residual_peak_dbfs: f64,
    pub maximum_spectral_error_db: f64,
    pub minimum_absolute_channel_correlation: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FindingLevel {
    Error,
    Note,
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub level: FindingLevel,
    pub rule_id: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AudioComparison {
    pub schema: &'static str,
    pub generator: &'static str,
    pub passed: bool,
    pub reference: InputEvidence,
    pub candidate: InputEvidence,
    pub options: AudioCompareOptions,
    pub method: MethodEvidence,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alignment: Option<AlignmentEvidence>,
    pub channels: Vec<ChannelEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregate: Option<AggregateEvidence>,
    pub error_count: usize,
    pub findings: Vec<Finding>,
}

pub fn compare_paths(
    reference_path: &Path,
    candidate_path: &Path,
    options: &AudioCompareOptions,
) -> Result<AudioComparison, String> {
    options.validate()?;
    let reference_file = inspect_file(reference_path, options.max_input_bytes)?;
    let candidate_file = inspect_file(candidate_path, options.max_input_bytes)?;
    // The comparison is deliberately permutation-invariant and derives its
    // channel assignment from decoded PCM. Preserve the decoder's fail-closed
    // layout contract for measurement paths, but do not require speaker
    // metadata that this operation neither consumes nor trusts.
    let reference =
        decoder::decode_limited_with_layout(reference_path, options.max_decoded_samples)
            .map_err(|error| format!("decode reference: {error}"))?
            .0;
    let candidate =
        decoder::decode_limited_with_layout(candidate_path, options.max_decoded_samples)
            .map_err(|error| format!("decode candidate: {error}"))?
            .0;
    if reference.channels as usize > MAX_CHANNELS || candidate.channels as usize > MAX_CHANNELS {
        return Err(format!(
            "audio comparison supports at most {MAX_CHANNELS} channels per input"
        ));
    }

    let reference_non_finite = count_non_finite(&reference);
    let candidate_non_finite = count_non_finite(&candidate);
    let reference_evidence = input_evidence(
        reference_path,
        reference_file,
        &reference,
        reference_non_finite,
    );
    let candidate_evidence = input_evidence(
        candidate_path,
        candidate_file,
        &candidate,
        candidate_non_finite,
    );
    let method = MethodEvidence {
        classification: "non-normative deterministic engineering QC; not PEAQ conformance",
        source: METHOD_SOURCE,
        synchronization: "permutation-invariant block-energy coarse search followed by bounded sample-lag correlation refinement",
        channel_assignment: "maximum-weight one-to-one assignment by absolute aligned Pearson correlation",
        residual: "full aligned decoded-f32 subtraction after channel assignment, with separate polarity-corrected evidence",
        spectral_error: "RMS magnitude error at active one-third-octave probe frequencies over eight Hann-windowed excerpts",
        alignment_block_frames: ALIGNMENT_BLOCK_FRAMES,
        alignment_probe_seconds: ALIGNMENT_PROBE_SECONDS,
        mapping_point_limit: MAPPING_POINTS,
        spectral_windows: SPECTRAL_WINDOWS,
        spectral_window_frames: SPECTRAL_WINDOW_FRAMES,
    };
    let mut findings = Vec::new();

    if reference_non_finite > 0 || candidate_non_finite > 0 {
        error(
            &mut findings,
            "FORGE-AUDIO-COMPARE-NON-FINITE",
            "decoded audio contains non-finite samples",
            json!({
                "reference_non_finite_samples": reference_non_finite,
                "candidate_non_finite_samples": candidate_non_finite
            }),
        );
    }
    if reference.sample_rate != candidate.sample_rate {
        error(
            &mut findings,
            "FORGE-AUDIO-COMPARE-SAMPLE-RATE",
            "reference and candidate sample rates differ; resampling is never implicit",
            json!({
                "reference_hz": reference.sample_rate,
                "candidate_hz": candidate.sample_rate
            }),
        );
    }
    if reference.channels != candidate.channels {
        error(
            &mut findings,
            "FORGE-AUDIO-COMPARE-CHANNEL-COUNT",
            "reference and candidate channel counts differ",
            json!({
                "reference_channels": reference.channels,
                "candidate_channels": candidate.channels
            }),
        );
    }

    if reference_non_finite > 0
        || candidate_non_finite > 0
        || reference.sample_rate != candidate.sample_rate
        || reference.channels != candidate.channels
    {
        return Ok(finish(
            reference_evidence,
            candidate_evidence,
            options,
            method,
            AnalysisEvidence::default(),
            findings,
        ));
    }

    let max_lag = ((u64::from(reference.sample_rate) * options.alignment_search_ms) / 1_000)
        .min(i64::MAX as u64) as i64;
    let (effective_max_lag, minimum_overlap_frames) =
        alignment_limits(&reference, &candidate, max_lag);
    let lag = estimate_offset(&reference, &candidate, max_lag);
    let (reference_start, candidate_start, compared_frames) =
        aligned_geometry(reference.frames, candidate.frames, lag);
    let alignment_score = best_pair_correlation(&reference, &candidate, lag, ALIGNMENT_FINE_POINTS);
    let alignment = AlignmentEvidence {
        offset_samples: lag,
        offset_seconds: lag as f64 / reference.sample_rate as f64,
        correlation: alignment_score,
        search_limit_samples: effective_max_lag as u64,
        minimum_overlap_frames,
        reference_start_frame: reference_start,
        candidate_start_frame: candidate_start,
        compared_frames,
    };

    if lag.unsigned_abs() > options.max_offset_samples {
        error(
            &mut findings,
            "FORGE-AUDIO-COMPARE-OFFSET",
            "detected sample offset exceeds the configured tolerance",
            json!({
                "offset_samples": lag,
                "tolerance_samples": options.max_offset_samples
            }),
        );
    }
    if alignment_score < options.min_alignment_correlation {
        error(
            &mut findings,
            "FORGE-AUDIO-COMPARE-ALIGNMENT-CONFIDENCE",
            "alignment correlation is below the configured minimum",
            json!({
                "correlation": alignment_score,
                "minimum": options.min_alignment_correlation
            }),
        );
    }
    let duration_delta = reference.frames.abs_diff(candidate.frames) as u64;
    if duration_delta > options.duration_tolerance_samples {
        error(
            &mut findings,
            "FORGE-AUDIO-COMPARE-DURATION",
            "decoded duration difference exceeds the configured tolerance",
            json!({
                "difference_samples": duration_delta,
                "tolerance_samples": options.duration_tolerance_samples
            }),
        );
    }
    if compared_frames == 0 {
        error(
            &mut findings,
            "FORGE-AUDIO-COMPARE-OVERLAP",
            "the detected alignment leaves no overlapping decoded samples",
            json!({"offset_samples": lag}),
        );
        return Ok(finish(
            reference_evidence,
            candidate_evidence,
            options,
            method,
            AnalysisEvidence {
                alignment: Some(alignment),
                ..AnalysisEvidence::default()
            },
            findings,
        ));
    }

    let correlations = channel_correlation_matrix(&reference, &candidate, lag);
    let assignment = maximum_weight_assignment(&correlations);
    let permutation = assignment
        .iter()
        .enumerate()
        .any(|(reference_channel, &candidate_channel)| reference_channel != candidate_channel);
    if permutation {
        let mapping = assignment
            .iter()
            .enumerate()
            .map(|(reference_channel, candidate_channel)| {
                json!({
                    "reference_channel": reference_channel + 1,
                    "candidate_channel": candidate_channel + 1
                })
            })
            .collect::<Vec<_>>();
        let level = if options.allow_channel_permutation {
            FindingLevel::Note
        } else {
            FindingLevel::Error
        };
        findings.push(Finding {
            level,
            rule_id: "FORGE-AUDIO-COMPARE-CHANNEL-PERMUTATION".into(),
            message: "maximum-correlation channel assignment is not the identity mapping".into(),
            evidence: Some(json!({"mapping": mapping})),
        });
    }

    let identity = aggregate_residual(
        &reference,
        &candidate,
        lag,
        &(0..assignment.len()).collect::<Vec<_>>(),
        false,
    );
    let mapped = aggregate_residual(&reference, &candidate, lag, &assignment, false);
    let corrected = aggregate_residual(&reference, &candidate, lag, &assignment, true);
    let mut channels = Vec::with_capacity(assignment.len());
    for (reference_channel, &candidate_channel) in assignment.iter().enumerate() {
        let correlation = correlations[reference_channel][candidate_channel];
        let inverted = correlation < 0.0;
        let metrics = channel_metrics(
            &reference,
            &candidate,
            reference_channel,
            candidate_channel,
            lag,
            inverted,
        );
        if inverted {
            findings.push(Finding {
                level: if options.allow_polarity_inversion {
                    FindingLevel::Note
                } else {
                    FindingLevel::Error
                },
                rule_id: "FORGE-AUDIO-COMPARE-POLARITY".into(),
                message: format!(
                    "candidate channel {} has inverted polarity relative to reference channel {}",
                    candidate_channel + 1,
                    reference_channel + 1
                ),
                evidence: Some(json!({"correlation": correlation})),
            });
        }
        if correlation.abs() < options.min_channel_correlation {
            error(
                &mut findings,
                "FORGE-AUDIO-COMPARE-CORRELATION",
                format!(
                    "reference channel {} correlation is below the configured minimum",
                    reference_channel + 1
                ),
                json!({
                    "candidate_channel": candidate_channel + 1,
                    "absolute_correlation": correlation.abs(),
                    "minimum": options.min_channel_correlation
                }),
            );
        }
        if metrics.spectral_error_db > options.max_spectral_error_db {
            error(
                &mut findings,
                "FORGE-AUDIO-COMPARE-SPECTRAL-ERROR",
                format!(
                    "reference channel {} spectral error exceeds the configured maximum",
                    reference_channel + 1
                ),
                json!({
                    "candidate_channel": candidate_channel + 1,
                    "spectral_error_db": metrics.spectral_error_db,
                    "maximum_db": options.max_spectral_error_db
                }),
            );
        }
        channels.push(ChannelEvidence {
            reference_channel: reference_channel as u16 + 1,
            candidate_channel: candidate_channel as u16 + 1,
            correlation,
            polarity_inverted: inverted,
            reference_rms_dbfs: metrics.reference_rms_dbfs,
            residual_rms_dbfs: metrics.residual_rms_dbfs,
            corrected_residual_rms_dbfs: metrics.corrected_residual_rms_dbfs,
            residual_peak_dbfs: metrics.residual_peak_dbfs,
            corrected_residual_peak_dbfs: metrics.corrected_residual_peak_dbfs,
            null_depth_db: metrics.null_depth_db,
            corrected_null_depth_db: metrics.corrected_null_depth_db,
            spectral_error_db: metrics.spectral_error_db,
            exact_sample_match_ratio: metrics.exact_sample_match_ratio,
        });
    }

    let effective_null_depth = if options.allow_polarity_inversion {
        corrected.null_depth_db
    } else {
        mapped.null_depth_db
    };
    let effective_peak = if options.allow_polarity_inversion {
        corrected.peak_dbfs
    } else {
        mapped.peak_dbfs
    };
    if effective_null_depth < options.min_null_depth_db {
        error(
            &mut findings,
            "FORGE-AUDIO-COMPARE-NULL-DEPTH",
            "aggregate null depth is below the configured minimum",
            json!({
                "null_depth_db": effective_null_depth,
                "minimum_db": options.min_null_depth_db,
                "polarity_correction_applied": options.allow_polarity_inversion
            }),
        );
    }
    if effective_peak > options.max_residual_peak_dbfs {
        error(
            &mut findings,
            "FORGE-AUDIO-COMPARE-RESIDUAL-PEAK",
            "aggregate residual peak exceeds the configured maximum",
            json!({
                "residual_peak_dbfs": effective_peak,
                "maximum_dbfs": options.max_residual_peak_dbfs,
                "polarity_correction_applied": options.allow_polarity_inversion
            }),
        );
    }
    let aggregate = AggregateEvidence {
        aligned_identity_null_depth_db: identity.null_depth_db,
        mapped_null_depth_db: mapped.null_depth_db,
        polarity_corrected_null_depth_db: corrected.null_depth_db,
        mapped_residual_peak_dbfs: mapped.peak_dbfs,
        polarity_corrected_residual_peak_dbfs: corrected.peak_dbfs,
        maximum_spectral_error_db: channels
            .iter()
            .map(|channel| channel.spectral_error_db)
            .fold(0.0_f64, f64::max),
        minimum_absolute_channel_correlation: channels
            .iter()
            .map(|channel| channel.correlation.abs())
            .fold(1.0_f64, f64::min),
    };
    findings.sort_by(|left, right| {
        left.rule_id
            .cmp(&right.rule_id)
            .then_with(|| left.message.cmp(&right.message))
    });
    Ok(finish(
        reference_evidence,
        candidate_evidence,
        options,
        method,
        AnalysisEvidence {
            alignment: Some(alignment),
            channels,
            aggregate: Some(aggregate),
        },
        findings,
    ))
}

#[derive(Default)]
struct AnalysisEvidence {
    alignment: Option<AlignmentEvidence>,
    channels: Vec<ChannelEvidence>,
    aggregate: Option<AggregateEvidence>,
}

fn finish(
    reference: InputEvidence,
    candidate: InputEvidence,
    options: &AudioCompareOptions,
    method: MethodEvidence,
    analysis: AnalysisEvidence,
    findings: Vec<Finding>,
) -> AudioComparison {
    let error_count = findings
        .iter()
        .filter(|finding| finding.level == FindingLevel::Error)
        .count();
    AudioComparison {
        schema: RESULT_SCHEMA,
        generator: concat!("forge-normalizer/", env!("CARGO_PKG_VERSION")),
        passed: error_count == 0,
        reference,
        candidate,
        options: options.clone(),
        method,
        alignment: analysis.alignment,
        channels: analysis.channels,
        aggregate: analysis.aggregate,
        error_count,
        findings,
    }
}

fn inspect_file(path: &Path, max_bytes: u64) -> Result<(u64, String), String> {
    let bytes = fs::metadata(path)
        .map_err(|error| format!("inspect {}: {error}", path.display()))?
        .len();
    if bytes > max_bytes {
        return Err(format!(
            "{}: input size {bytes} exceeds safety limit {max_bytes}",
            path.display()
        ));
    }
    let mut file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut read_bytes = 0u64;
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("hash {}: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        read_bytes = read_bytes
            .checked_add(count as u64)
            .ok_or_else(|| format!("{}: input byte count overflow", path.display()))?;
        if read_bytes > max_bytes {
            return Err(format!(
                "{}: input exceeds safety limit {max_bytes} while hashing",
                path.display()
            ));
        }
        hasher.update(&buffer[..count]);
    }
    let digest = hasher.finalize();
    let mut sha256 = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut sha256, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok((bytes, sha256))
}

fn input_evidence(
    path: &Path,
    file: (u64, String),
    audio: &AudioBuffer,
    non_finite_samples: u64,
) -> InputEvidence {
    InputEvidence {
        path: path.to_string_lossy().into_owned(),
        bytes: file.0,
        sha256: file.1,
        sample_rate_hz: audio.sample_rate,
        channels: audio.channels,
        frames: audio.frames,
        duration_seconds: audio.frames as f64 / audio.sample_rate as f64,
        non_finite_samples,
    }
}

fn count_non_finite(audio: &AudioBuffer) -> u64 {
    audio
        .data
        .iter()
        .flat_map(|channel| channel.iter())
        .filter(|sample| !sample.is_finite())
        .count() as u64
}

fn estimate_offset(reference: &AudioBuffer, candidate: &AudioBuffer, max_lag: i64) -> i64 {
    let (max_lag, _) = alignment_limits(reference, candidate, max_lag);
    if max_lag == 0 {
        return 0;
    }
    let probe_frames = reference
        .sample_rate
        .saturating_mul(ALIGNMENT_PROBE_SECONDS as u32) as usize;
    let probe_frames = probe_frames
        .min(reference.frames)
        .min(candidate.frames)
        .saturating_add(max_lag as usize)
        .min(reference.frames.max(candidate.frames));
    let reference_energy = energy_envelope(reference, probe_frames);
    let candidate_energy = energy_envelope(candidate, probe_frames);
    let max_blocks = (max_lag as usize).div_ceil(ALIGNMENT_BLOCK_FRAMES) as i64;
    let mut best_block_lag = 0i64;
    let mut best_score = f64::NEG_INFINITY;
    for block_lag in -max_blocks..=max_blocks {
        let score = overlap_weighted_correlation(
            &reference_energy,
            &candidate_energy,
            block_lag,
            usize::MAX,
        );
        if better_lag(score, block_lag, best_score, best_block_lag) {
            best_score = score;
            best_block_lag = block_lag;
        }
    }
    let coarse_lag = best_block_lag * ALIGNMENT_BLOCK_FRAMES as i64;
    let (reference_channel, candidate_channel) =
        best_channel_pair(reference, candidate, coarse_lag);
    // Block-energy correlation can have nearby aliases for sustained tones.
    // Inspect a wider sample-domain neighbourhood so an exact transient/onset
    // alignment is not excluded by a periodic coarse maximum.
    let fine_radius = (ALIGNMENT_BLOCK_FRAMES * 16) as i64;
    let mut best_lag = coarse_lag.clamp(-max_lag, max_lag);
    let minimum = (best_lag - fine_radius).max(-max_lag);
    let maximum = (best_lag + fine_radius).min(max_lag);
    let mut best_fine_score = correlation_at(
        &reference.data[reference_channel],
        &candidate.data[candidate_channel],
        best_lag,
        ALIGNMENT_FINE_POINTS,
    )
    .abs();
    for lag in minimum..=maximum {
        let score = correlation_at(
            &reference.data[reference_channel],
            &candidate.data[candidate_channel],
            lag,
            ALIGNMENT_FINE_POINTS,
        )
        .abs();
        if better_lag(score, lag, best_fine_score, best_lag) {
            best_fine_score = score;
            best_lag = lag;
        }
    }
    best_lag
}

fn alignment_limits(
    reference: &AudioBuffer,
    candidate: &AudioBuffer,
    requested_max_lag: i64,
) -> (i64, usize) {
    let minimum_frames = reference.frames.min(candidate.frames);
    let minimum_overlap = if minimum_frames >= reference.sample_rate as usize {
        (minimum_frames / 2).max(reference.sample_rate as usize / 2)
    } else {
        (minimum_frames / 2).max(2)
    };
    let max_lag = requested_max_lag.min(
        minimum_frames
            .saturating_sub(minimum_overlap)
            .min(i64::MAX as usize) as i64,
    );
    (max_lag.max(0), minimum_overlap)
}

fn energy_envelope(audio: &AudioBuffer, maximum_frames: usize) -> Vec<f32> {
    let frames = maximum_frames.min(audio.frames);
    let blocks = frames / ALIGNMENT_BLOCK_FRAMES;
    let mut result = Vec::with_capacity(blocks);
    for block in 0..blocks {
        let start = block * ALIGNMENT_BLOCK_FRAMES;
        let end = start + ALIGNMENT_BLOCK_FRAMES;
        let mut energy = 0.0;
        for channel in &audio.data {
            for &sample in &channel[start..end] {
                energy += (sample as f64).powi(2);
            }
        }
        result.push(
            (energy / (ALIGNMENT_BLOCK_FRAMES * audio.data.len()).max(1) as f64).sqrt() as f32,
        );
    }
    result
}

fn better_lag(score: f64, lag: i64, best_score: f64, best_lag: i64) -> bool {
    score > best_score + 1e-12
        || ((score - best_score).abs() <= 1e-12
            && (lag.unsigned_abs(), lag) < (best_lag.unsigned_abs(), best_lag))
}

fn best_channel_pair(reference: &AudioBuffer, candidate: &AudioBuffer, lag: i64) -> (usize, usize) {
    let mut best = (0usize, 0usize);
    let mut best_score = f64::NEG_INFINITY;
    for reference_channel in 0..reference.data.len() {
        for candidate_channel in 0..candidate.data.len() {
            let score = correlation_at(
                &reference.data[reference_channel],
                &candidate.data[candidate_channel],
                lag,
                ALIGNMENT_MATRIX_POINTS,
            )
            .abs();
            if score > best_score {
                best_score = score;
                best = (reference_channel, candidate_channel);
            }
        }
    }
    best
}

fn best_pair_correlation(
    reference: &AudioBuffer,
    candidate: &AudioBuffer,
    lag: i64,
    point_limit: usize,
) -> f64 {
    let mut best = 0.0_f64;
    for reference_channel in 0..reference.data.len() {
        for candidate_channel in 0..candidate.data.len() {
            best = best.max(
                correlation_at(
                    &reference.data[reference_channel],
                    &candidate.data[candidate_channel],
                    lag,
                    point_limit,
                )
                .abs(),
            );
        }
    }
    best
}

fn channel_correlation_matrix(
    reference: &AudioBuffer,
    candidate: &AudioBuffer,
    lag: i64,
) -> Vec<Vec<f64>> {
    reference
        .data
        .iter()
        .map(|reference_channel| {
            candidate
                .data
                .iter()
                .map(|candidate_channel| {
                    correlation_at(reference_channel, candidate_channel, lag, MAPPING_POINTS)
                })
                .collect()
        })
        .collect()
}

fn correlation_at(reference: &[f32], candidate: &[f32], lag: i64, limit: usize) -> f64 {
    let (reference_start, candidate_start, frames) =
        aligned_geometry(reference.len(), candidate.len(), lag);
    if frames < 2 {
        return 0.0;
    }
    let stride = frames.div_ceil(limit.max(1)).max(1);
    let mut count = 0.0;
    let mut sum_reference = 0.0;
    let mut sum_candidate = 0.0;
    let mut square_reference = 0.0;
    let mut square_candidate = 0.0;
    let mut product = 0.0;
    for frame in (0..frames).step_by(stride) {
        let reference_sample = reference[reference_start + frame] as f64;
        let candidate_sample = candidate[candidate_start + frame] as f64;
        count += 1.0;
        sum_reference += reference_sample;
        sum_candidate += candidate_sample;
        square_reference += reference_sample * reference_sample;
        square_candidate += candidate_sample * candidate_sample;
        product += reference_sample * candidate_sample;
    }
    let covariance = count * product - sum_reference * sum_candidate;
    let reference_variance = count * square_reference - sum_reference * sum_reference;
    let candidate_variance = count * square_candidate - sum_candidate * sum_candidate;
    let denominator = (reference_variance.max(0.0) * candidate_variance.max(0.0)).sqrt();
    if denominator <= 1e-30 {
        let reference_mean = sum_reference / count;
        let candidate_mean = sum_candidate / count;
        if reference_mean.abs() <= 1e-15 && candidate_mean.abs() <= 1e-15 {
            1.0
        } else if reference_mean.abs() <= 1e-15 || candidate_mean.abs() <= 1e-15 {
            0.0
        } else {
            reference_mean.signum() * candidate_mean.signum()
        }
    } else {
        (covariance / denominator).clamp(-1.0, 1.0)
    }
}

fn overlap_weighted_correlation(
    reference: &[f32],
    candidate: &[f32],
    lag: i64,
    limit: usize,
) -> f64 {
    let (_, _, frames) = aligned_geometry(reference.len(), candidate.len(), lag);
    let available = reference.len().min(candidate.len()).max(1);
    correlation_at(reference, candidate, lag, limit).abs() * frames as f64 / available as f64
}

fn aligned_geometry(
    reference_frames: usize,
    candidate_frames: usize,
    lag: i64,
) -> (usize, usize, usize) {
    let reference_start = if lag < 0 {
        lag.unsigned_abs().min(usize::MAX as u64) as usize
    } else {
        0
    };
    let candidate_start = if lag > 0 {
        lag.unsigned_abs().min(usize::MAX as u64) as usize
    } else {
        0
    };
    let frames = reference_frames
        .saturating_sub(reference_start)
        .min(candidate_frames.saturating_sub(candidate_start));
    (reference_start, candidate_start, frames)
}

fn maximum_weight_assignment(correlations: &[Vec<f64>]) -> Vec<usize> {
    let size = correlations.len();
    if size == 0 {
        return Vec::new();
    }
    // Hungarian algorithm for a square minimum-cost matrix. Negating the
    // absolute correlations converts maximum similarity into minimum cost.
    let mut u = vec![0.0; size + 1];
    let mut v = vec![0.0; size + 1];
    let mut p = vec![0usize; size + 1];
    let mut way = vec![0usize; size + 1];
    for row in 1..=size {
        p[0] = row;
        let mut column0 = 0usize;
        let mut minimum = vec![f64::INFINITY; size + 1];
        let mut used = vec![false; size + 1];
        loop {
            used[column0] = true;
            let row0 = p[column0];
            let mut delta = f64::INFINITY;
            let mut column1 = 0usize;
            for column in 1..=size {
                if used[column] {
                    continue;
                }
                let cost = -correlations[row0 - 1][column - 1].abs();
                let current = cost - u[row0] - v[column];
                if current < minimum[column] {
                    minimum[column] = current;
                    way[column] = column0;
                }
                if minimum[column] < delta {
                    delta = minimum[column];
                    column1 = column;
                }
            }
            for column in 0..=size {
                if used[column] {
                    u[p[column]] += delta;
                    v[column] -= delta;
                } else {
                    minimum[column] -= delta;
                }
            }
            column0 = column1;
            if p[column0] == 0 {
                break;
            }
        }
        loop {
            let column1 = way[column0];
            p[column0] = p[column1];
            column0 = column1;
            if column0 == 0 {
                break;
            }
        }
    }
    let mut assignment = vec![0usize; size];
    for column in 1..=size {
        assignment[p[column] - 1] = column - 1;
    }
    assignment
}

struct ResidualMetrics {
    null_depth_db: f64,
    peak_dbfs: f64,
}

fn aggregate_residual(
    reference: &AudioBuffer,
    candidate: &AudioBuffer,
    lag: i64,
    assignment: &[usize],
    correct_polarity: bool,
) -> ResidualMetrics {
    let (reference_start, candidate_start, frames) =
        aligned_geometry(reference.frames, candidate.frames, lag);
    let mut reference_energy = 0.0;
    let mut residual_energy = 0.0;
    let mut peak = 0.0_f64;
    for (reference_channel, &candidate_channel) in assignment.iter().enumerate() {
        let correlation = correlation_at(
            &reference.data[reference_channel],
            &candidate.data[candidate_channel],
            lag,
            MAPPING_POINTS,
        );
        let polarity = if correct_polarity && correlation < 0.0 {
            -1.0
        } else {
            1.0
        };
        for frame in 0..frames {
            let reference_sample =
                reference.data[reference_channel][reference_start + frame] as f64;
            let candidate_sample =
                candidate.data[candidate_channel][candidate_start + frame] as f64 * polarity;
            let residual = reference_sample - candidate_sample;
            reference_energy += reference_sample * reference_sample;
            residual_energy += residual * residual;
            peak = peak.max(residual.abs());
        }
    }
    ResidualMetrics {
        null_depth_db: null_depth(reference_energy, residual_energy),
        peak_dbfs: amplitude_db(peak),
    }
}

struct ChannelMetrics {
    reference_rms_dbfs: f64,
    residual_rms_dbfs: f64,
    corrected_residual_rms_dbfs: f64,
    residual_peak_dbfs: f64,
    corrected_residual_peak_dbfs: f64,
    null_depth_db: f64,
    corrected_null_depth_db: f64,
    spectral_error_db: f64,
    exact_sample_match_ratio: f64,
}

fn channel_metrics(
    reference: &AudioBuffer,
    candidate: &AudioBuffer,
    reference_channel: usize,
    candidate_channel: usize,
    lag: i64,
    inverted: bool,
) -> ChannelMetrics {
    let (reference_start, candidate_start, frames) =
        aligned_geometry(reference.frames, candidate.frames, lag);
    let reference_samples =
        &reference.data[reference_channel][reference_start..reference_start + frames];
    let candidate_samples =
        &candidate.data[candidate_channel][candidate_start..candidate_start + frames];
    let mut reference_energy = 0.0;
    let mut residual_energy = 0.0;
    let mut corrected_energy = 0.0;
    let mut residual_peak = 0.0_f64;
    let mut corrected_peak = 0.0_f64;
    let mut exact = 0usize;
    let polarity = if inverted { -1.0 } else { 1.0 };
    for (&reference_sample, &candidate_sample) in reference_samples.iter().zip(candidate_samples) {
        let reference_sample = reference_sample as f64;
        let candidate_sample = candidate_sample as f64;
        let residual = reference_sample - candidate_sample;
        let corrected_residual = reference_sample - polarity * candidate_sample;
        reference_energy += reference_sample * reference_sample;
        residual_energy += residual * residual;
        corrected_energy += corrected_residual * corrected_residual;
        residual_peak = residual_peak.max(residual.abs());
        corrected_peak = corrected_peak.max(corrected_residual.abs());
        exact += usize::from(reference_sample.to_bits() == candidate_sample.to_bits());
    }
    let denominator = frames.max(1) as f64;
    ChannelMetrics {
        reference_rms_dbfs: power_db(reference_energy / denominator),
        residual_rms_dbfs: power_db(residual_energy / denominator),
        corrected_residual_rms_dbfs: power_db(corrected_energy / denominator),
        residual_peak_dbfs: amplitude_db(residual_peak),
        corrected_residual_peak_dbfs: amplitude_db(corrected_peak),
        null_depth_db: null_depth(reference_energy, residual_energy),
        corrected_null_depth_db: null_depth(reference_energy, corrected_energy),
        spectral_error_db: spectral_error(
            reference_samples,
            candidate_samples,
            reference.sample_rate,
        ),
        exact_sample_match_ratio: exact as f64 / denominator,
    }
}

fn spectral_error(reference: &[f32], candidate: &[f32], sample_rate: u32) -> f64 {
    let frames = reference.len().min(candidate.len());
    if frames < 32 {
        return 0.0;
    }
    let window = SPECTRAL_WINDOW_FRAMES.min(frames);
    let window_count = SPECTRAL_WINDOWS.min(frames.div_ceil(window)).max(1);
    let maximum_start = frames - window;
    let mut square_error = 0.0;
    let mut active_bins = 0usize;
    for window_index in 0..window_count {
        let start = if window_count == 1 {
            maximum_start / 2
        } else {
            maximum_start * window_index / (window_count - 1)
        };
        let reference_window = &reference[start..start + window];
        let candidate_window = &candidate[start..start + window];
        let mut frequency = 31.5_f64;
        while frequency < sample_rate as f64 * 0.49 {
            let reference_amplitude = hann_goertzel(reference_window, sample_rate, frequency);
            let candidate_amplitude = hann_goertzel(candidate_window, sample_rate, frequency);
            if reference_amplitude.max(candidate_amplitude) >= 1e-8 {
                let difference =
                    amplitude_db(candidate_amplitude) - amplitude_db(reference_amplitude);
                square_error += difference * difference;
                active_bins += 1;
            }
            frequency *= 2.0_f64.powf(1.0 / 3.0);
        }
    }
    if active_bins == 0 {
        0.0
    } else {
        (square_error / active_bins as f64).sqrt().min(300.0)
    }
}

fn hann_goertzel(samples: &[f32], sample_rate: u32, frequency: f64) -> f64 {
    let omega = TAU * frequency / sample_rate as f64;
    let coefficient = 2.0 * omega.cos();
    let mut previous = 0.0;
    let mut previous_two = 0.0;
    let denominator = samples.len().saturating_sub(1).max(1) as f64;
    let mut window_sum = 0.0;
    for (index, &sample) in samples.iter().enumerate() {
        let window = 0.5 - 0.5 * (TAU * index as f64 / denominator).cos();
        let current = sample as f64 * window + coefficient * previous - previous_two;
        previous_two = previous;
        previous = current;
        window_sum += window;
    }
    let power =
        previous * previous + previous_two * previous_two - coefficient * previous * previous_two;
    power.max(0.0).sqrt() / window_sum.max(1e-30)
}

fn null_depth(reference_energy: f64, residual_energy: f64) -> f64 {
    if residual_energy <= 1e-30 {
        300.0
    } else if reference_energy <= 1e-30 {
        0.0
    } else {
        (10.0 * (reference_energy / residual_energy).log10()).clamp(-300.0, 300.0)
    }
}

fn amplitude_db(amplitude: f64) -> f64 {
    (20.0 * amplitude.max(1e-15).log10()).clamp(-300.0, 300.0)
}

fn power_db(power: f64) -> f64 {
    (10.0 * power.max(1e-30).log10()).clamp(-300.0, 300.0)
}

fn error(
    findings: &mut Vec<Finding>,
    rule_id: impl Into<String>,
    message: impl Into<String>,
    evidence: Value,
) {
    findings.push(Finding {
        level: FindingLevel::Error,
        rule_id: rule_id.into(),
        message: message.into(),
        evidence: Some(evidence),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::{ChannelRole, PcmKind, WavWriter};

    fn audio(channels: Vec<Vec<f32>>) -> AudioBuffer {
        AudioBuffer {
            sample_rate: 48_000,
            channels: channels.len() as u16,
            frames: channels[0].len(),
            channel_roles: vec![ChannelRole::Main; channels.len()],
            source_kind: PcmKind::F32,
            data: channels,
        }
    }

    fn programme(frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|frame| {
                let time = frame as f64 / 48_000.0;
                (0.2 * (TAU * 997.0 * time).sin()
                    + 0.03 * (TAU * 211.0 * time).sin()
                    + (frame % 997) as f64 * 1e-6) as f32
            })
            .collect()
    }

    #[test]
    fn offset_search_uses_candidate_minus_reference_sign() {
        let source = programme(24_000);
        let mut delayed = vec![0.0; 137];
        delayed.extend_from_slice(&source);
        let reference = audio(vec![source]);
        let candidate = audio(vec![delayed]);
        assert_eq!(estimate_offset(&reference, &candidate, 500), 137);
    }

    #[test]
    fn assignment_detects_swap_and_polarity() {
        let left = programme(8_000);
        let right = (0..8_000)
            .map(|frame| ((frame as f64 * 0.017).cos() * 0.1) as f32)
            .collect::<Vec<_>>();
        let reference = audio(vec![left.clone(), right.clone()]);
        let candidate = audio(vec![
            right.iter().map(|sample| -*sample).collect(),
            left.clone(),
        ]);
        let matrix = channel_correlation_matrix(&reference, &candidate, 0);
        let assignment = maximum_weight_assignment(&matrix);
        assert_eq!(assignment, vec![1, 0]);
        assert!(matrix[1][0] < -0.999);
    }

    #[test]
    fn identical_channel_has_deep_null_and_no_spectral_error() {
        let samples = programme(12_000);
        let source = audio(vec![samples.clone()]);
        let metrics = channel_metrics(&source, &source, 0, 0, 0, false);
        assert_eq!(metrics.null_depth_db, 300.0);
        assert_eq!(metrics.residual_peak_dbfs, -300.0);
        assert_eq!(metrics.spectral_error_db, 0.0);
        assert_eq!(metrics.exact_sample_match_ratio, 1.0);
    }

    #[test]
    fn spectral_probe_reports_a_uniform_gain_change() {
        let samples = programme(24_000);
        let attenuated = samples.iter().map(|sample| *sample * 0.5).collect();
        let reference = audio(vec![samples]);
        let candidate = audio(vec![attenuated]);
        let metrics = channel_metrics(&reference, &candidate, 0, 0, 0, false);
        assert!((metrics.spectral_error_db - 6.0206).abs() < 0.01);
        assert!((metrics.null_depth_db - 6.0206).abs() < 0.01);
    }

    #[test]
    fn option_validation_rejects_unbounded_alignment() {
        let options = AudioCompareOptions {
            alignment_search_ms: MAX_ALIGNMENT_MILLISECONDS + 1,
            ..AudioCompareOptions::default()
        };
        assert!(options.validate().is_err());
    }

    #[test]
    fn identical_silence_has_defined_alignment_and_null() {
        let silence = audio(vec![vec![0.0; 8_000]]);
        assert_eq!(estimate_offset(&silence, &silence, 1_000), 0);
        assert_eq!(best_pair_correlation(&silence, &silence, 0, 1_000), 1.0);
        let metrics = channel_metrics(&silence, &silence, 0, 0, 0, false);
        assert_eq!(metrics.null_depth_db, 300.0);
    }

    #[test]
    fn alignment_recovers_positive_and_negative_noise_offsets() {
        let mut state = 0x1234_5678_u32;
        let source = (0..32_000)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((state >> 8) as f32 / 16_777_216.0 - 0.5) * 0.4
            })
            .collect::<Vec<_>>();
        let reference = audio(vec![source.clone()]);
        for offset in [-997_i64, -137, 0, 137, 997] {
            let candidate_samples = if offset >= 0 {
                let offset = offset as usize;
                let mut samples = vec![0.0; offset];
                samples.extend_from_slice(&source[..source.len() - offset]);
                samples
            } else {
                let advance = offset.unsigned_abs() as usize;
                let mut samples = source[advance..].to_vec();
                samples.resize(source.len(), 0.0);
                samples
            };
            let candidate = audio(vec![candidate_samples]);
            assert_eq!(
                estimate_offset(&reference, &candidate, 2_000),
                offset,
                "offset {offset}"
            );
        }
    }

    #[test]
    fn path_comparison_accepts_maskless_multichannel_wave() {
        let work = tempfile::tempdir().unwrap();
        let path = work.path().join("maskless.wav");
        let channels = (0..6)
            .map(|channel| {
                programme(8_000)
                    .into_iter()
                    .map(|sample| sample * (channel + 1) as f32 / 6.0)
                    .collect()
            })
            .collect();
        let source = audio(channels);
        WavWriter::write(&path, &source, PcmKind::F32, false).unwrap();

        assert!(decoder::decode_limited(&path, 100_000).is_err());
        let comparison = compare_paths(&path, &path, &AudioCompareOptions::default()).unwrap();
        assert!(comparison.passed, "{:#?}", comparison.findings);
        assert_eq!(comparison.channels.len(), 6);
    }
}
