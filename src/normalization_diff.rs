//! Non-normative evidence describing a normalization render and codec drift.

use crate::atomic::AtomicOutput;
use crate::decoder;
use crate::normalize::{Analysis, OutputFormat, Plan, RenderStatistics};
use crate::wav::PcmKind;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;

pub const RESULT_SCHEMA: &str =
    "https://penguin425.github.io/audio-normalizer/schema/normalization-difference-v1";
pub const METHOD_ID: &str = "forge-normalization-difference-v1";
const MAX_GAIN_ENVELOPE_POINTS: usize = 10_000;

#[derive(Debug, Clone, Serialize)]
pub struct NormalizationDifferenceReport {
    pub schema: &'static str,
    pub generator: &'static str,
    pub method: MethodEvidence,
    pub assets: Vec<NormalizationDifferenceAsset>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MethodEvidence {
    pub id: &'static str,
    pub classification: &'static str,
    pub intended_signal: &'static str,
    pub gain_envelope: &'static str,
    pub clipping: &'static str,
    pub codec_drift: &'static str,
    pub maximum_gain_envelope_points_per_asset: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct NormalizationDifferenceAsset {
    pub input: FileEvidence,
    pub output: FileEvidence,
    pub output_format: &'static str,
    pub source: MeasurementEvidence,
    pub intended_pre_codec: MeasurementEvidence,
    pub decoded_output: MeasurementEvidence,
    pub static_gain_db: f64,
    pub gain_envelope: Vec<GainEnvelopePoint>,
    pub protection: ProtectionEvidence,
    pub clipping: ClippingEvidence,
    pub codec_drift: CodecDriftEvidence,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileEvidence {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MeasurementEvidence {
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub frames: usize,
    pub duration_seconds: f64,
    pub integrated_lufs: Option<f64>,
    pub loudness_range_lu: Option<f64>,
    pub rms_dbfs: Option<f64>,
    pub sample_peak_dbfs: Option<f64>,
    pub true_peak_dbtp: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GainEnvelopePoint {
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub mean_limiter_reduction_db: f64,
    pub maximum_limiter_reduction_db: f64,
    pub mean_effective_gain_db: f64,
    pub minimum_effective_gain_db: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProtectionEvidence {
    pub mode: &'static str,
    pub ceiling_dbtp: f64,
    pub limiter_lookahead_ms: Option<f64>,
    pub limiter_release_ms: Option<f64>,
    pub limited_frames: usize,
    pub mean_limiter_reduction_db: f64,
    pub maximum_limiter_reduction_db: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClippingEvidence {
    /// Samples already outside digital full scale in the output-rate source.
    pub input_full_scale_exceeding_samples: u64,
    /// Samples outside digital full scale after static gain and before protection.
    pub post_gain_full_scale_exceeding_samples: u64,
    /// Samples above the configured ceiling after static gain and before protection.
    pub pre_protection_ceiling_exceeding_samples: u64,
    /// Samples actually hard-clipped to the configured ceiling.
    pub hard_clipped_samples: u64,
    /// Samples outside digital full scale in the float signal sent to the encoder.
    pub protected_full_scale_exceeding_samples: u64,
    /// Re-decoded samples at a representable full-scale endpoint.
    pub decoded_output_full_scale_samples: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CodecDriftEvidence {
    pub integrated_loudness_lu: Option<f64>,
    pub loudness_range_lu: Option<f64>,
    pub rms_db: Option<f64>,
    pub sample_peak_db: Option<f64>,
    pub true_peak_db: Option<f64>,
    pub duration_seconds: f64,
    pub frames: i64,
}

pub struct AssetMeasurements<'a> {
    pub source: &'a Analysis,
    pub output: &'a Analysis,
    pub gain: f32,
    pub render: &'a RenderStatistics,
}

impl NormalizationDifferenceReport {
    pub fn new(assets: Vec<NormalizationDifferenceAsset>) -> Self {
        Self {
            schema: RESULT_SCHEMA,
            generator: concat!("forge-normalizer/", env!("CARGO_PKG_VERSION")),
            method: MethodEvidence {
                id: METHOD_ID,
                classification:
                    "non-normative deterministic engineering evidence; not a perceptual quality score",
                intended_signal:
                    "streaming measurement of the protected planar-f32 signal passed to the encoder",
                gain_envelope:
                    "static gain minus linked true-peak-limiter reduction at emitted-frame intervals",
                clipping:
                    "strict float full-scale exceedance before encoding; decoded endpoint count uses the source PCM quantizer LSB where known",
                codec_drift:
                    "decoded output measurement minus intended pre-codec measurement; no implicit time alignment",
                maximum_gain_envelope_points_per_asset: MAX_GAIN_ENVELOPE_POINTS,
            },
            assets,
        }
    }
}

pub fn write_report(path: &Path, report: &NormalizationDifferenceReport) -> Result<(), String> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let staged = AtomicOutput::new(path)?;
    let mut file = File::create(staged.path())
        .map_err(|error| format!("create {}: {error}", staged.path().display()))?;
    serde_json::to_writer_pretty(&mut file, report)
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    file.write_all(b"\n")
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    drop(file);
    staged.commit()
}

pub fn build_asset(
    input: &FileEvidence,
    output: &Path,
    format: OutputFormat,
    plan: &Plan,
    measurements: AssetMeasurements<'_>,
) -> Result<NormalizationDifferenceAsset, String> {
    let AssetMeasurements {
        source,
        output: output_analysis,
        gain,
        render,
    } = measurements;
    let static_gain_db = linear_to_db(f64::from(gain));
    let gain_envelope = gain_envelope(static_gain_db, render);
    if gain_envelope.len() > MAX_GAIN_ENVELOPE_POINTS {
        return Err("normalization gain envelope exceeded its evidence bound".into());
    }
    let limiter = render.limiter.as_ref();
    let decoded_output_full_scale_samples =
        count_decoded_full_scale_samples(output, output_analysis.kind)?;
    Ok(NormalizationDifferenceAsset {
        input: input.clone(),
        output: inspect_file(output)?,
        output_format: format_name(format),
        source: MeasurementEvidence::from(source),
        intended_pre_codec: MeasurementEvidence::from(&render.intended),
        decoded_output: MeasurementEvidence::from(output_analysis),
        static_gain_db,
        gain_envelope,
        protection: ProtectionEvidence {
            mode: if limiter.is_some() {
                "linked-lookahead-true-peak-limiter"
            } else {
                "hard-ceiling-safety-clip"
            },
            ceiling_dbtp: plan.ceiling_db,
            limiter_lookahead_ms: plan.limiter.map(|value| value.lookahead_ms),
            limiter_release_ms: plan.limiter.map(|value| value.release_ms),
            limited_frames: limiter.map_or(0, |value| value.limited_frames),
            mean_limiter_reduction_db: limiter.map_or(0.0, |value| value.mean_reduction_db),
            maximum_limiter_reduction_db: limiter.map_or(0.0, |value| value.maximum_reduction_db),
        },
        clipping: ClippingEvidence {
            input_full_scale_exceeding_samples: render.input_full_scale_exceeding_samples,
            post_gain_full_scale_exceeding_samples: render.post_gain_full_scale_exceeding_samples,
            pre_protection_ceiling_exceeding_samples: render.post_gain_ceiling_exceeding_samples,
            hard_clipped_samples: if render.limiter.is_none() {
                render.post_gain_ceiling_exceeding_samples
            } else {
                0
            },
            protected_full_scale_exceeding_samples: render.protected_full_scale_exceeding_samples,
            decoded_output_full_scale_samples,
        },
        codec_drift: CodecDriftEvidence {
            integrated_loudness_lu: finite_difference(output_analysis.lufs, render.intended.lufs),
            loudness_range_lu: finite_difference(
                output_analysis.loudness_range_lu,
                render.intended.loudness_range_lu,
            ),
            rms_db: finite_difference(output_analysis.rms_db, render.intended.rms_db),
            sample_peak_db: finite_difference(
                output_analysis.sample_peak_db(),
                render.intended.sample_peak_db(),
            ),
            true_peak_db: finite_difference(
                output_analysis.true_peak_db(),
                render.intended.true_peak_db(),
            ),
            duration_seconds: output_analysis.duration_secs() - render.intended.duration_secs(),
            frames: signed_frame_difference(output_analysis.frames, render.intended.frames),
        },
    })
}

impl From<&Analysis> for MeasurementEvidence {
    fn from(value: &Analysis) -> Self {
        Self {
            sample_rate_hz: value.sample_rate,
            channels: value.channels,
            frames: value.frames,
            duration_seconds: value.duration_secs(),
            integrated_lufs: finite(value.lufs),
            loudness_range_lu: finite(value.loudness_range_lu),
            rms_dbfs: finite(value.rms_db),
            sample_peak_dbfs: finite(value.sample_peak_db()),
            true_peak_dbtp: finite(value.true_peak_db()),
        }
    }
}

fn gain_envelope(static_gain_db: f64, render: &RenderStatistics) -> Vec<GainEnvelopePoint> {
    let sample_rate = f64::from(render.intended.sample_rate);
    if let Some(limiter) = &render.limiter {
        return limiter
            .envelope
            .iter()
            .map(|point| GainEnvelopePoint {
                start_seconds: point.start_frame as f64 / sample_rate,
                end_seconds: point.end_frame as f64 / sample_rate,
                mean_limiter_reduction_db: point.mean_reduction_db,
                maximum_limiter_reduction_db: point.maximum_reduction_db,
                mean_effective_gain_db: static_gain_db - point.mean_reduction_db,
                minimum_effective_gain_db: static_gain_db - point.maximum_reduction_db,
            })
            .collect();
    }
    if render.intended.frames == 0 {
        Vec::new()
    } else {
        vec![GainEnvelopePoint {
            start_seconds: 0.0,
            end_seconds: render.intended.duration_secs(),
            mean_limiter_reduction_db: 0.0,
            maximum_limiter_reduction_db: 0.0,
            mean_effective_gain_db: static_gain_db,
            minimum_effective_gain_db: static_gain_db,
        }]
    }
}

pub fn inspect_file(path: &Path) -> Result<FileEvidence, String> {
    let bytes = fs::metadata(path)
        .map_err(|error| format!("inspect {}: {error}", path.display()))?
        .len();
    let mut file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("hash {}: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let digest = hasher.finalize();
    let mut sha256 = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut sha256, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(FileEvidence {
        path: path.to_string_lossy().into_owned(),
        bytes,
        sha256,
    })
}

fn count_decoded_full_scale_samples(path: &Path, kind: PcmKind) -> Result<u64, String> {
    let positive_threshold = match kind {
        PcmKind::U8 => 127.0 / 128.0,
        PcmKind::S16 => 32_767.0 / 32_768.0,
        PcmKind::S24 => 8_388_607.0 / 8_388_608.0,
        PcmKind::S32 => 2_147_483_647.0 / 2_147_483_648.0,
        PcmKind::F32 | PcmKind::F64 => 1.0,
    };
    let mut count = 0u64;
    decoder::decode_stream(path, |_, planar| {
        count = count.saturating_add(
            planar
                .iter()
                .flat_map(|channel| channel.iter())
                .filter(|sample| f64::from(**sample) >= positive_threshold || **sample <= -1.0)
                .count() as u64,
        );
        Ok(())
    })?;
    Ok(count)
}

fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

fn finite_difference(actual: f64, intended: f64) -> Option<f64> {
    finite(actual).zip(finite(intended)).map(|(a, b)| a - b)
}

fn linear_to_db(value: f64) -> f64 {
    20.0 * value.log10()
}

fn signed_frame_difference(actual: usize, intended: usize) -> i64 {
    i128::try_from(actual)
        .unwrap_or(i128::MAX)
        .saturating_sub(i128::try_from(intended).unwrap_or(i128::MAX))
        .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

fn format_name(format: OutputFormat) -> &'static str {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::{AudioBuffer, WavWriter};

    #[test]
    fn decoded_full_scale_count_recognizes_integer_endpoints() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("endpoints.wav");
        let audio = AudioBuffer {
            sample_rate: 48_000,
            channels: 1,
            frames: 3,
            data: vec![vec![1.0, -1.0, 0.5]],
            channel_roles: crate::wav::default_channel_roles(1),
            source_kind: PcmKind::F32,
        };
        WavWriter::write(&path, &audio, PcmKind::S16, false).unwrap();

        assert_eq!(
            count_decoded_full_scale_samples(&path, PcmKind::S16).unwrap(),
            2
        );
    }
}
